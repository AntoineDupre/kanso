//! Global, session-owned GitHub device login. No subprocess output is logged.
use crate::{api::now_millis, db::Database};
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::{
    ffi::{OsStr, OsString},
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{mpsc, oneshot, Mutex, Notify},
};

const LIMIT: usize = 8192;
const ATTEMPT: Duration = Duration::from_secs(600);
const PROBE: Duration = Duration::from_secs(8);
const CACHE: Duration = Duration::from_secs(5);

#[derive(Clone, Serialize)]
pub struct Status {
    pub state: &'static str,
    pub account: Option<String>,
    pub operator_managed: bool,
    pub compatible: bool,
}

#[derive(Clone, Serialize)]
pub struct Progress {
    pub state: &'static str,
    pub owner: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds_remaining: Option<u64>,
}

struct Active {
    owner: String,
    code: Option<String>,
    started: Instant,
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct State {
    active: Option<Active>,
    terminal: Option<(String, &'static str)>,
}

pub struct Manager {
    state: Mutex<State>,
    cache: Mutex<Option<(Instant, Status, bool)>>,
    probe_lock: Mutex<()>,
    generation: AtomicU64,
    stopped: Notify,
    program: OsString,
    attempt_limit: Duration,
    startup_limit: Duration,
    #[cfg(test)]
    managed_override: Option<bool>,
}

impl Default for Manager {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            cache: Mutex::new(None),
            probe_lock: Mutex::new(()),
            generation: AtomicU64::new(0),
            stopped: Notify::new(),
            program: "gh".into(),
            attempt_limit: ATTEMPT,
            startup_limit: Duration::from_secs(30),
            #[cfg(test)]
            managed_override: None,
        }
    }
}

fn environment_managed() -> bool {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()))
}

fn command(program: &OsStr, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    // gh does not need Kanso's own password, tokens, or other application
    // configuration. Preserve the rest for parity with importer authentication
    // (including GH_TOKEN, proxy, HOME and GH_CONFIG_DIR).
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KANSO_") {
            cmd.env_remove(key);
        }
    }
    cmd.args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn limited<R: AsyncRead + Unpin>(mut input: R) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = input.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            return Ok(bytes);
        }
        if bytes.len() + n > LIMIT {
            return Err(());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

// Always drain both pipes concurrently; kill and reap on timeout or overflow.
async fn probe(program: &OsStr, args: &[&str]) -> Result<(bool, Vec<u8>), std::io::ErrorKind> {
    let mut child = command(program, args).spawn().map_err(|e| e.kind())?;
    let stdout = child.stdout.take().ok_or(std::io::ErrorKind::Other)?;
    let stderr = child.stderr.take().ok_or(std::io::ErrorKind::Other)?;
    let result = tokio::time::timeout(PROBE, async {
        let (out, err, status) = tokio::join!(limited(stdout), limited(stderr), child.wait());
        match (out, err, status) {
            (Ok(out), Ok(_), Ok(status)) => Ok((status.success(), out)),
            _ => Err(std::io::ErrorKind::Other),
        }
    })
    .await;
    match result {
        Ok(Ok(value)) => Ok(value),
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(std::io::ErrorKind::Other)
        }
    }
}

fn compatible(output: &[u8]) -> bool {
    // Human-oriented device output is tested only for this exact version.
    output.starts_with(b"gh version 2.97.0 ")
}

async fn session_valid(db: &Database, owner: &str) -> Option<bool> {
    let id = owner.to_owned();
    tokio::time::timeout(
        Duration::from_secs(2),
        db.call(move |c| {
            Ok(c.query_row(
                "SELECT 1 FROM ui_sessions WHERE id=?1 AND expires_at>?2",
                rusqlite::params![id, now_millis()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        }),
    )
    .await
    .ok()
    .and_then(Result::ok)
}

impl Manager {
    #[cfg(test)]
    fn with_program(program: std::path::PathBuf) -> Self {
        Self {
            program: program.into_os_string(),
            managed_override: Some(false),
            ..Self::default()
        }
    }

    pub async fn status(&self, refresh: bool) -> Status {
        let max_age = if refresh {
            Duration::from_secs(2)
        } else {
            CACHE
        };
        self.check_status(Some(max_age), refresh).await
    }

    async fn check_status(&self, max_age: Option<Duration>, refresh: bool) -> Status {
        if let Some(age) = max_age {
            if let Some(value) = self.cached(age, refresh).await {
                return value;
            }
        }
        let _single_flight = self.probe_lock.lock().await;
        if let Some(age) = max_age {
            if let Some(value) = self.cached(age, refresh).await {
                return value;
            }
        }
        let generation = self.generation.load(Ordering::Acquire);
        let operator_managed = {
            #[cfg(test)]
            {
                self.managed_override.unwrap_or_else(environment_managed)
            }
            #[cfg(not(test))]
            {
                environment_managed()
            }
        };
        let mut status = Status {
            state: "unavailable",
            account: None,
            operator_managed,
            compatible: false,
        };
        match probe(&self.program, &["--version"]).await {
            Err(std::io::ErrorKind::NotFound) => status.state = "missing_cli",
            Err(_) => {}
            Ok((true, version)) => {
                status.compatible = compatible(&version);
                match probe(
                    &self.program,
                    &["api", "--hostname", "github.com", "--method", "GET", "user"],
                )
                .await
                {
                    Ok((true, bytes)) => {
                        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                            if let Some(login) = value.get("login").and_then(|v| v.as_str()) {
                                if !login.is_empty()
                                    && login.len() <= 39
                                    && login
                                        .bytes()
                                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                                {
                                    status.state = "connected";
                                    status.account = Some(login.to_owned());
                                }
                            }
                        }
                    }
                    _ if !operator_managed => {
                        if let Ok((true, bytes)) = probe(
                            &self.program,
                            &[
                                "auth",
                                "status",
                                "--active",
                                "--hostname",
                                "github.com",
                                "--json",
                                "hosts",
                            ],
                        )
                        .await
                        {
                            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                                if let Some(hosts) = value.get("hosts").and_then(|v| v.as_object())
                                {
                                    if hosts.is_empty() {
                                        status.state = "not_connected";
                                    } else if hosts
                                        .get("github.com")
                                        .and_then(|v| v.as_array())
                                        .is_some_and(|entries| {
                                            entries.iter().any(|entry| {
                                                entry.get("active").and_then(|v| v.as_bool())
                                                    == Some(true)
                                                    && entry.get("state").and_then(|v| v.as_str())
                                                        == Some("error")
                                            })
                                        })
                                    {
                                        status.state = "credential_error";
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        let mut cache = self.cache.lock().await;
        if self.generation.load(Ordering::Acquire) == generation {
            *cache = Some((Instant::now(), status.clone(), refresh));
        }
        status
    }

    async fn cached(&self, max_age: Duration, refresh: bool) -> Option<Status> {
        self.cache
            .lock()
            .await
            .as_ref()
            .and_then(|(at, value, was_refresh)| {
                (at.elapsed() < max_age && (!refresh || *was_refresh)).then(|| value.clone())
            })
    }

    pub async fn progress(&self, owner: &str) -> Progress {
        let state = self.state.lock().await;
        if let Some(active) = &state.active {
            let owned = active.owner == owner;
            return Progress {
                state: "pending",
                owner: owned,
                code: if owned { active.code.clone() } else { None },
                seconds_remaining: if owned {
                    Some(
                        self.attempt_limit
                            .saturating_sub(active.started.elapsed())
                            .as_secs(),
                    )
                } else {
                    None
                },
            };
        }
        if let Some((id, terminal)) = &state.terminal {
            if id == owner {
                return Progress {
                    state: terminal,
                    owner: true,
                    code: None,
                    seconds_remaining: None,
                };
            }
        }
        Progress {
            state: "idle",
            owner: false,
            code: None,
            seconds_remaining: None,
        }
    }

    pub async fn start(
        self: &Arc<Self>,
        owner: String,
        confirmed: bool,
        db: Database,
    ) -> Result<(), &'static str> {
        // Reserve first: concurrent probes must never create concurrent attempts.
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            if state.active.is_some() {
                return Err("A login is already in progress.");
            }
            state.terminal = None;
            state.active = Some(Active {
                owner: owner.clone(),
                code: None,
                started: Instant::now(),
                cancel: Some(tx),
            });
        }
        let manager = self.clone();
        tokio::spawn(async move {
            manager.run(owner, confirmed, db, rx).await;
        });
        Ok(())
    }

    pub async fn cancel(&self, owner: &str) -> bool {
        let mut state = self.state.lock().await;
        if let Some(active) = state.active.as_mut() {
            if active.owner == owner {
                active.code = None;
                if let Some(tx) = active.cancel.take() {
                    let _ = tx.send(());
                }
                return true;
            }
        }
        false
    }

    pub async fn shutdown(&self) {
        loop {
            let notified = self.stopped.notified();
            let mut state = self.state.lock().await;
            match state.active.as_mut() {
                None => return,
                Some(active) => {
                    active.code = None;
                    if let Some(tx) = active.cancel.take() {
                        let _ = tx.send(());
                    }
                }
            }
            drop(state);
            notified.await;
        }
    }

    async fn run(
        self: Arc<Self>,
        owner: String,
        confirmed: bool,
        db: Database,
        mut cancel: oneshot::Receiver<()>,
    ) {
        let deadline = tokio::time::Instant::now() + self.attempt_limit;
        let outcome = self
            .login(&owner, confirmed, db, &mut cancel, deadline)
            .await;
        self.generation.fetch_add(1, Ordering::AcqRel);
        *self.cache.lock().await = None;
        {
            let mut state = self.state.lock().await;
            state.active = None;
            state.terminal = Some((owner, outcome));
        }
        self.stopped.notify_one();
        // Approval and cancellation can race. Recheck outside the active slot;
        // a slow network must not block cancellation or shutdown.
        let _ = self.check_status(None, false).await;
    }

    async fn login(
        &self,
        owner: &str,
        confirmed: bool,
        db: Database,
        cancel: &mut oneshot::Receiver<()>,
        deadline: tokio::time::Instant,
    ) -> &'static str {
        let status = tokio::select! {
            _ = &mut *cancel => return "cancelled",
            _ = tokio::time::sleep_until(deadline) => return "expired",
            status = self.check_status(None, false) => status,
        };
        if status.operator_managed
            || !status.compatible
            || !matches!(
                status.state,
                "connected" | "not_connected" | "credential_error"
            )
            || (matches!(status.state, "connected" | "credential_error") && !confirmed)
        {
            return "failed";
        }
        let valid = tokio::select! {
            _ = &mut *cancel => return "cancelled",
            _ = tokio::time::sleep_until(deadline) => return "expired",
            valid = session_valid(&db, owner) => valid,
        };
        match valid {
            Some(true) => {}
            Some(false) => return "cancelled",
            None => return "failed",
        }
        let mut cmd = command(
            &self.program,
            &[
                "auth",
                "login",
                "--hostname",
                "github.com",
                "--git-protocol",
                "https",
                "--web",
                "--skip-ssh-key",
            ],
        );
        cmd.env("GH_BROWSER", "/bin/true");
        let Ok(mut child) = cmd.spawn() else {
            return "failed";
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return "failed";
        };
        let (tx, mut rx) = mpsc::channel::<(usize, Vec<u8>)>(8);
        let mut readers = Vec::new();
        for (index, mut input) in [
            (
                0,
                Box::pin(stdout) as std::pin::Pin<Box<dyn AsyncRead + Send>>,
            ),
            (1, Box::pin(stderr)),
        ] {
            let tx = tx.clone();
            readers.push(tokio::spawn(async move {
                let mut buf = [0u8; 512];
                loop {
                    match input.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send((index, buf[..n].to_vec())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }));
        }
        drop(tx);
        let mut buffers = [Vec::new(), Vec::new()];
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        let startup_deadline = tokio::time::Instant::now() + self.startup_limit;
        let mut code_seen = false;
        let mut streams_open = true;
        let outcome = loop {
            tokio::select! {
                _ = &mut *cancel => break "cancelled",
                _ = tokio::time::sleep_until(deadline) => break "expired",
                _ = tokio::time::sleep_until(startup_deadline), if !code_seen => break "failed",
                _ = interval.tick() => {
                    let valid = session_valid(&db, owner).await;
                    match valid { Some(true) => {}, Some(false) => break "cancelled", None => break "failed" }
                },
                output = rx.recv(), if streams_open => {
                    if let Some((index, chunk)) = output {
                        if buffers[index].len() + chunk.len() > LIMIT { break "failed"; }
                        buffers[index].extend_from_slice(&chunk);
                        if !code_seen {
                            if let Some(code) = parse_code(&buffers[index]) {
                                let mut state = self.state.lock().await;
                                if let Some(active) = state.active.as_mut() { if active.owner == owner && active.cancel.is_some() { active.code = Some(code); code_seen = true; } }
                            }
                        }
                    } else { streams_open = false; }
                },
                status = child.wait() => break if status.is_ok_and(|s| s.success()) && code_seen { "completed" } else { "failed" },
            }
        };
        let _ = child.kill().await;
        let _ = child.wait().await;
        for reader in readers {
            reader.abort();
            let _ = reader.await;
        }
        outcome
    }
}

fn parse_code(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let rest = text.split_once("First copy your one-time code: ")?.1;
    let code = rest.get(..9)?;
    let b = code.as_bytes();
    if rest.as_bytes().get(9).is_some_and(u8::is_ascii_whitespace)
        && b[4] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        Some(code.to_owned())
    } else {
        None
    }
}

#[cfg(all(test, unix))]
#[path = "github_auth_tests.rs"]
mod tests;
