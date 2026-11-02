//! Password sessions, cookie signatures and API-token authentication.
//!
//! The HMAC key is a random 32-byte secret persisted as `session.key` in the
//! data directory (mode 0600). Keeping it independent from `KANSO_PASSWORD`
//! lets sessions survive a restart without tying cookie validity to the
//! password; deleting the file revokes every UI session.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{header, HeaderMap, Method};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::ids;

pub const SESSION_COOKIE: &str = "kanso_session";
pub const SESSION_MAX_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;
pub const SESSION_MAX_AGE_MILLIS: i64 = SESSION_MAX_AGE_SECONDS * 1000;
/// Refresh `ui_sessions.last_seen` at most this often.
pub const SESSION_LAST_SEEN_INTERVAL_MILLIS: i64 = 60 * 60 * 1000;
/// Refresh `tokens.last_used_at` at most this often.
pub const TOKEN_LAST_USED_INTERVAL_MILLIS: i64 = 60 * 1000;
pub const LOGIN_MAX_FAILURES: u32 = 5;
pub const LOGIN_WINDOW: Duration = Duration::from_secs(60);

/// Authorization scope carried by a UI session or an API token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Human,
    Agent,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "human" => Some(Self::Human),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityKind {
    Ui,
    Token,
}

impl IdentityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Token => "token",
        }
    }
}

/// The authenticated caller behind one request.
#[derive(Clone, Debug)]
pub struct Identity {
    pub kind: IdentityKind,
    pub name: String,
    pub scope: Scope,
    /// Set for cookie authentication so logout can delete the row.
    pub session_id: Option<String>,
}

impl Identity {
    pub fn is_human(&self) -> bool {
        self.scope == Scope::Human
    }

    /// Attribution stored in `created_by`, `comments.author` and `activity.actor`.
    pub fn actor(&self) -> &str {
        &self.name
    }
}

/// Runtime authentication configuration.
pub struct AuthConfig {
    password_digest: [u8; 32],
    hmac_key: [u8; 32],
    public_url: Option<String>,
    cookie_secure: bool,
}

impl AuthConfig {
    /// Load the persistent HMAC key from `data_dir`, creating it if needed.
    pub fn load(password: &str, public_url: Option<&str>, data_dir: &Path) -> Result<Self> {
        let hmac_key = load_or_create_key(data_dir)?;
        let cookie_secure = public_url
            .map(|url| url.trim().to_ascii_lowercase().starts_with("https://"))
            .unwrap_or(false);
        Ok(Self {
            password_digest: Sha256::digest(password.as_bytes()).into(),
            hmac_key,
            public_url: public_url.map(str::to_owned),
            cookie_secure,
        })
    }

    /// Compare a submitted password in constant time.
    pub fn password_matches(&self, submitted: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(submitted.as_bytes()).into();
        constant_time_eq(&self.password_digest, &digest)
    }

    pub fn public_url(&self) -> Option<&str> {
        self.public_url.as_deref()
    }

    pub fn cookie_secure(&self) -> bool {
        self.cookie_secure
    }

    /// Sign a session id into the cookie value: `id.signature`.
    pub fn signed_session(&self, session_id: &str) -> String {
        let signature = session_signature(&self.hmac_key, session_id);
        format!("{session_id}.{}", ids::base64_url(&signature))
    }

    /// Verify the signature and return the session id, or `None` when tampered.
    pub fn verify_session(&self, value: &str) -> Option<String> {
        let (session_id, encoded) = value.rsplit_once('.')?;
        if session_id.is_empty() {
            return None;
        }
        let signature = ids::decode_base64_url(encoded)?;
        let expected = session_signature(&self.hmac_key, session_id);
        constant_time_eq(&signature, &expected).then(|| session_id.to_owned())
    }

    pub fn session_cookie(&self, session_id: &str) -> String {
        cookie_header(
            &self.signed_session(session_id),
            SESSION_MAX_AGE_SECONDS,
            self.cookie_secure,
        )
    }

    pub fn clear_cookie(&self) -> String {
        cookie_header("", 0, self.cookie_secure)
    }
}

fn cookie_header(value: &str, max_age_seconds: i64, secure: bool) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_seconds}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Load the 32-byte key, creating it with mode 0600 on first start.
///
/// A new key is written to a temporary file, fsynced and hard-linked into
/// place, so a crash can never leave a truncated key at the final path and a
/// concurrent starter either reads the complete key or wins the link race.
fn load_or_create_key(data_dir: &Path) -> Result<[u8; 32]> {
    fs::create_dir_all(data_dir).map_err(|error| {
        Error::msg(format!(
            "failed to create data directory {}: {error}",
            data_dir.display()
        ))
    })?;
    let path = data_dir.join("session.key");

    for _ in 0..3 {
        match fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() != 32 {
                    return Err(Error::msg(format!(
                        "{} must contain exactly 32 bytes",
                        path.display()
                    )));
                }
                // Fail closed if an existing key cannot be made private.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(
                        |error| {
                            Error::msg(format!(
                                "failed to restrict permissions on {}: {error}",
                                path.display()
                            ))
                        },
                    )?;
                }
                let mut key = [0_u8; 32];
                key.copy_from_slice(&bytes);
                return Ok(key);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = ids::random_bytes::<32>()?;
                let temporary = data_dir.join(format!(
                    ".session.key.{}.{}",
                    std::process::id(),
                    ids::uuid_v4()?
                ));

                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;
                    options.mode(0o600);
                }
                let result = options.open(&temporary).and_then(|mut file| {
                    file.write_all(&key)?;
                    file.sync_all()
                });
                if let Err(error) = result {
                    let _ = fs::remove_file(&temporary);
                    return Err(Error::msg(format!(
                        "failed to create {}: {error}",
                        temporary.display()
                    )));
                }

                match fs::hard_link(&temporary, &path) {
                    Ok(()) => {
                        let _ = fs::remove_file(&temporary);
                        return Ok(key);
                    }
                    // Another process won the race; read its complete key.
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let _ = fs::remove_file(&temporary);
                        continue;
                    }
                    Err(error) => {
                        let _ = fs::remove_file(&temporary);
                        return Err(Error::msg(format!(
                            "failed to install {}: {error}",
                            path.display()
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(Error::msg(format!(
                    "failed to read {}: {error}",
                    path.display()
                )));
            }
        }
    }

    Err(Error::msg(format!(
        "could not read a stable session key from {}",
        path.display()
    )))
}

/// Constant-time byte-slice equality (no early return once lengths match).
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

type HmacSha256 = Hmac<Sha256>;

fn session_signature(key: &[u8], session_id: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA-256 accepts keys of any size");
    mac.update(session_id.as_bytes());
    mac.finalize().into_bytes().into()
}

/// Read `kanso_session` from one or more `Cookie` headers.
pub fn session_cookie_value(headers: &HeaderMap) -> Option<String> {
    for header in headers.get_all(header::COOKIE) {
        let Ok(value) = header.to_str() else {
            continue;
        };
        for pair in value.split(';') {
            let pair = pair.trim();
            if let Some(cookie) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
                return Some(cookie.to_owned());
            }
        }
    }
    None
}

pub fn is_mutating(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PATCH | Method::DELETE | Method::PUT
    )
}

/// CSRF check for cookie-authenticated mutations.
///
/// The request must carry an `Origin` (or, when absent, a `Referer`) whose
/// authority matches the `Host` header or `KANSO_PUBLIC_URL`.
pub fn csrf_origin_allowed(headers: &HeaderMap, public_url: Option<&str>) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get(header::REFERER)
                .and_then(|value| value.to_str().ok())
        })
    else {
        return false;
    };
    let Some(origin_authority) = authority(origin) else {
        return false;
    };

    if let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(normalise_authority)
    {
        if host == origin_authority {
            return true;
        }
    }

    public_url
        .and_then(authority)
        .is_some_and(|public| public == origin_authority)
}

fn authority(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(normalise_authority(authority))
}

fn normalise_authority(value: &str) -> String {
    value.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Client address used as the rate-limit key.
///
/// Direct non-loopback connections use the socket address. Behind the loopback
/// nginx proxy, `X-Real-IP` (which nginx overwrites) is trusted, then the last
/// `X-Forwarded-For` hop that the proxy appended. Spoofed headers therefore
/// cannot move a direct client into an arbitrary bucket.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let Some(peer) = peer else {
        // Fail closed: without a socket address we cannot tell a direct client
        // from one behind the proxy, so ignore spoofable headers.
        return "unknown".to_owned();
    };
    if !peer.ip().is_loopback() {
        return peer.ip().to_string();
    }

    headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.rsplit(',').next())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| peer.ip().to_string())
}

#[derive(Clone, Copy, Debug)]
struct Attempts {
    started: Instant,
    attempts: u32,
}

const LIMITER_MAX_KEYS: usize = 10_000;
const LIMITER_PRUNE_EVERY: u64 = 1024;

#[derive(Debug, Default)]
struct LimiterState {
    attempts: HashMap<String, Attempts>,
    operations: u64,
}

/// In-memory login attempt limiter, keyed by client IP.
///
/// `try_begin` reserves one of the five attempts in the same critical section
/// that checks the window, so a concurrent burst cannot race past the limit. A
/// successful login calls `reset`.
#[derive(Debug, Default)]
pub struct LoginLimiter {
    state: Mutex<LimiterState>,
}

impl LoginLimiter {
    pub fn try_begin(&self, key: &str) -> bool {
        self.try_begin_at(key, Instant::now())
    }

    pub fn reset(&self, key: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.attempts.remove(key);
    }

    fn try_begin_at(&self, key: &str, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.operations = state.operations.wrapping_add(1);
        if state.operations.is_multiple_of(LIMITER_PRUNE_EVERY) {
            state
                .attempts
                .retain(|_, entry| now.duration_since(entry.started) < LOGIN_WINDOW);
        }

        match state.attempts.get_mut(key) {
            Some(entry) if now.duration_since(entry.started) < LOGIN_WINDOW => {
                if entry.attempts >= LOGIN_MAX_FAILURES {
                    return false;
                }
                entry.attempts += 1;
                true
            }
            Some(entry) => {
                *entry = Attempts {
                    started: now,
                    attempts: 1,
                };
                true
            }
            None => {
                if state.attempts.len() >= LIMITER_MAX_KEYS {
                    if let Some(evicted) = state.attempts.keys().next().cloned() {
                        state.attempts.remove(&evicted);
                    }
                }
                state.attempts.insert(
                    key.to_owned(),
                    Attempts {
                        started: now,
                        attempts: 1,
                    },
                );
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn config(dir: &TempDir) -> AuthConfig {
        AuthConfig::load("secret", Some("https://kanso.example"), dir.path()).expect("auth config")
    }

    #[test]
    fn password_comparison_is_constant_time_and_correct() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn hmac_matches_rfc_4231_case_two() {
        let digest = session_signature(b"Jefe", "what do ya want for nothing?");
        // Hex is easier to compare against the RFC.
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_hashes_keys_longer_than_the_block() {
        let key = [0xaa_u8; 131];
        let digest = session_signature(
            &key,
            "Test Using Larger Than Block-Size Key - Hash Key First",
        );
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn cookies_from_the_previous_implementation_are_still_valid() {
        // Fixed key [7; 32] and session id, signed by the old hand-written
        // implementation: the signature must remain byte-for-byte identical.
        let config = AuthConfig {
            password_digest: [0_u8; 32],
            hmac_key: [7_u8; 32],
            public_url: None,
            cookie_secure: false,
        };
        let legacy = "legacy-session-id.n87qyKYN1NzumSF6PgoWPGcFCbczPQKV8C49CI9wmtM";
        assert_eq!(
            config.verify_session(legacy).as_deref(),
            Some("legacy-session-id")
        );
        assert_eq!(config.signed_session("legacy-session-id"), legacy);
    }

    #[test]
    fn password_matches_only_the_configured_secret() {
        let dir = TempDir::new();
        let config = config(&dir);
        assert!(config.password_matches("secret"));
        assert!(!config.password_matches("Secret"));
        assert!(!config.password_matches(""));
        assert!(!config.password_matches("secret "));
    }

    #[test]
    fn session_cookie_round_trips_and_rejects_tampering() {
        let dir = TempDir::new();
        let config = config(&dir);
        let cookie = config.signed_session("session-id");
        assert_eq!(
            config.verify_session(&cookie).as_deref(),
            Some("session-id")
        );

        let mut tampered = cookie.clone();
        tampered.push('x');
        assert!(config.verify_session(&tampered).is_none());
        assert!(config.verify_session("session-id").is_none());
        assert!(config.verify_session("session-id.").is_none());
        assert!(config.verify_session(".abc").is_none());

        let mut parts = cookie.split('.');
        let id = parts.next().expect("id");
        let signature = parts.next().expect("signature");
        let other = config.signed_session("other-id");
        let other_signature = other.split('.').nth(1).expect("signature");
        assert!(config
            .verify_session(&format!("{id}.{other_signature}"))
            .is_none());
        assert!(config
            .verify_session(&format!("other-id.{signature}"))
            .is_none());
    }

    #[test]
    fn cookie_attributes_follow_public_url() {
        let dir = TempDir::new();
        let secure = config(&dir);
        let cookie = secure.session_cookie("id");
        assert!(cookie.starts_with("kanso_session=id."));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age=2592000"));
        assert!(cookie.contains("; Secure"));
        assert!(secure.clear_cookie().contains("Max-Age=0"));

        let plain = AuthConfig::load("secret", None, dir.path()).expect("config");
        assert!(!plain.session_cookie("id").contains("Secure"));
    }

    #[test]
    fn hmac_key_is_persistent_and_private() {
        let dir = TempDir::new();
        let first = config(&dir);
        let second = config(&dir);
        assert_eq!(first.hmac_key, second.hmac_key);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(dir.path().join("session.key"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn csrf_accepts_matching_host_origin_or_referer() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "kanso.example".parse().unwrap());

        headers.insert(header::ORIGIN, "https://kanso.example".parse().unwrap());
        assert!(csrf_origin_allowed(&headers, None));

        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!csrf_origin_allowed(&headers, None));
        assert!(!csrf_origin_allowed(
            &headers,
            Some("https://kanso.example")
        ));

        headers.remove(header::ORIGIN);
        headers.insert(
            header::REFERER,
            "https://kanso.example/p/ALP".parse().unwrap(),
        );
        assert!(csrf_origin_allowed(&headers, None));

        headers.insert(header::ORIGIN, "null".parse().unwrap());
        assert!(!csrf_origin_allowed(
            &headers,
            Some("https://kanso.example")
        ));

        headers.remove(header::ORIGIN);
        headers.remove(header::REFERER);
        assert!(!csrf_origin_allowed(&headers, None));
    }

    #[test]
    fn csrf_compares_ports_and_public_url_authority() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "kanso.example:8087".parse().unwrap());
        headers.insert(header::ORIGIN, "http://kanso.example:8087".parse().unwrap());
        assert!(csrf_origin_allowed(&headers, None));

        headers.insert(header::ORIGIN, "http://kanso.example".parse().unwrap());
        assert!(!csrf_origin_allowed(&headers, None));

        headers.insert(header::HOST, "127.0.0.1:8087".parse().unwrap());
        assert!(csrf_origin_allowed(&headers, Some("https://kanso.example")));
        headers.insert(header::ORIGIN, "https://kanso.example".parse().unwrap());
        assert!(csrf_origin_allowed(
            &headers,
            Some("https://kanso.example/app")
        ));
    }

    #[test]
    fn client_ip_uses_the_socket_or_the_proxy_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "10.0.0.2".parse().unwrap());
        assert_eq!(client_ip(&headers, None), "unknown");

        let direct: SocketAddr = "203.0.113.9:1234".parse().unwrap();
        assert_eq!(client_ip(&headers, Some(direct)), "203.0.113.9");
        let loopback: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        assert_eq!(client_ip(&headers, Some(loopback)), "10.0.0.2");

        headers.remove("x-real-ip");
        headers.insert("x-forwarded-for", "10.0.0.1, 10.0.0.2".parse().unwrap());
        assert_eq!(client_ip(&headers, Some(loopback)), "10.0.0.2");
    }

    #[test]
    fn limiter_allows_five_attempts_then_blocks_the_window() {
        let limiter = LoginLimiter::default();
        let now = Instant::now();
        for attempt in 0..LOGIN_MAX_FAILURES {
            assert!(limiter.try_begin_at("1.2.3.4", now), "attempt {attempt}");
        }
        assert!(!limiter.try_begin_at("1.2.3.4", now));
        assert!(limiter.try_begin_at("5.6.7.8", now));
        assert!(limiter.try_begin_at("1.2.3.4", now + LOGIN_WINDOW));

        limiter.reset("1.2.3.4");
        assert!(limiter.try_begin_at("1.2.3.4", now + LOGIN_WINDOW));
    }

    #[test]
    fn limiter_is_atomic_under_concurrency() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let limiter = Arc::new(LoginLimiter::default());
        let allowed = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..20 {
            let limiter = Arc::clone(&limiter);
            let allowed = Arc::clone(&allowed);
            threads.push(std::thread::spawn(move || {
                if limiter.try_begin("9.9.9.9") {
                    allowed.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for thread in threads {
            thread.join().expect("thread");
        }
        assert_eq!(allowed.load(Ordering::Relaxed), LOGIN_MAX_FAILURES as usize);
    }

    #[test]
    fn limiter_bounds_its_key_set() {
        let limiter = LoginLimiter::default();
        let now = Instant::now();
        for index in 0..(LIMITER_MAX_KEYS + 50) {
            assert!(limiter.try_begin_at(&format!("10.0.{}.{}", index / 256, index % 256), now));
        }
        let state = limiter.state.lock().expect("lock");
        assert!(state.attempts.len() <= LIMITER_MAX_KEYS);
    }

    #[test]
    fn mutating_methods_are_recognised() {
        assert!(is_mutating(&Method::POST));
        assert!(is_mutating(&Method::PATCH));
        assert!(is_mutating(&Method::DELETE));
        assert!(!is_mutating(&Method::GET));
        assert!(!is_mutating(&Method::HEAD));
    }
}
