use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use super::output;
use super::CliError;
use crate::fileutil;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConfigFile {
    pub url: String,
    pub token: String,
}

impl ConfigFile {
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
        }
    }
}

/// `KANSO_CONFIG`, or `~/.config/kanso/config.toml`.
pub fn path() -> Result<PathBuf, CliError> {
    if let Some(value) = env_value("KANSO_CONFIG") {
        return Ok(PathBuf::from(value));
    }
    let home = env_value("HOME")
        .or_else(|| env_value("USERPROFILE"))
        .ok_or_else(|| {
            CliError::usage("HOME is not set; set KANSO_CONFIG to the config file path")
        })?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("kanso")
        .join("config.toml"))
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn validate_url(raw: &str) -> Result<String, CliError> {
    let url = raw.trim().trim_end_matches('/').to_owned();
    let (scheme, authority) = if let Some(rest) = url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(CliError::usage(
            "url must start with http:// or https:// and include a host",
        ));
    };
    if authority.is_empty() || authority.starts_with('/') {
        return Err(CliError::usage(
            "url must start with http:// or https:// and include a host",
        ));
    }
    if authority.contains('@') {
        return Err(CliError::usage(
            "url must not embed credentials; pass the token with --token",
        ));
    }
    if authority.contains(['/', '?', '#']) {
        return Err(CliError::usage(
            "url must contain only the scheme and host, without a path or query",
        ));
    }

    // Loopback HTTP is common for local development and tests; a token sent to
    // any other host in cleartext would leak the credential.
    if scheme == "http" && !is_loopback_host(authority) {
        return Err(CliError::usage(
            "http:// is only accepted for a loopback host; use https:// for a remote server",
        ));
    }
    Ok(url)
}

fn is_loopback_host(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host == "127.0.0.1"
        || host == "0:0:0:0:0:0:0:1"
}

pub fn parse(text: &str) -> Result<ConfigFile, CliError> {
    toml::from_str::<ConfigFile>(text)
        .map_err(|error| CliError::usage(format!("invalid config file: {error}")))
}

#[cfg(unix)]
fn mode_is_private(mode: u32) -> bool {
    mode & 0o077 == 0
}

/// The config holds a bearer token. Repair an existing too-open file rather
/// than using it silently, and fail closed when the repair is impossible.
fn repair_permissions(path: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = fs::metadata(path)
            .map_err(|error| {
                CliError::Io(format!("failed to inspect {}: {error}", path.display()))
            })?
            .permissions()
            .mode()
            & 0o777;
        if mode_is_private(mode) {
            return Ok(());
        }

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            CliError::Io(format!(
                "failed to restrict permissions on {}: {error}",
                path.display()
            ))
        })?;
        output::error_line(format_args!(
            "kanso: config {} was readable by other users; set to 0600",
            path.display()
        ));
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn read(path: &Path) -> Result<Option<ConfigFile>, CliError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            repair_permissions(path)?;
            Ok(Some(parse(&text)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliError::Io(format!(
            "failed to read {}: {error}",
            path.display()
        ))),
    }
}

pub fn write(path: &Path, config: &ConfigFile, force: bool) -> Result<(), CliError> {
    if path.exists() && !force {
        return Err(CliError::usage(format!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        )));
    }
    let mut text = toml::to_string(config)
        .map_err(|error| CliError::Io(format!("failed to encode config: {error}")))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fileutil::write_atomic(path, text.as_bytes(), 0o600)
        .map_err(|error| CliError::Io(format!("failed to write {}: {error}", path.display())))
}

/// Resolve the effective URL and token: environment first, then the file.
pub fn resolve() -> Result<(String, String), CliError> {
    let url = env_value("KANSO_URL");
    let token = env_value("KANSO_TOKEN");
    // Both values in the environment need no config file at all: this keeps
    // env-only use working without HOME and ignores an unreadable file.
    if url.is_some() && token.is_some() {
        return merge(
            url.as_deref(),
            token.as_deref(),
            None,
            Path::new("(environment)"),
        );
    }

    let path = path()?;
    let file = read(&path)?;
    merge(url.as_deref(), token.as_deref(), file.as_ref(), &path)
}

fn merge(
    env_url: Option<&str>,
    env_token: Option<&str>,
    file: Option<&ConfigFile>,
    path: &Path,
) -> Result<(String, String), CliError> {
    let file_url = file.map(|config| config.url.trim());
    let file_token = file.map(|config| config.token.trim());

    let url = env_url.or(file_url).filter(|value| !value.is_empty());
    let token = env_token.or(file_token).filter(|value| !value.is_empty());

    let url = url.ok_or_else(|| {
        CliError::usage(format!(
            "no server URL configured; run `kanso config init` or set KANSO_URL (looked at {})",
            path.display()
        ))
    })?;
    let token = token.ok_or_else(|| {
        CliError::usage(format!(
            "no token configured; run `kanso config init` or set KANSO_TOKEN (looked at {})",
            path.display()
        ))
    })?;

    Ok((validate_url(url)?, token.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn parses_url_and_token() {
        let config = parse("url = \"https://kanso.example\"\ntoken = \"kanso_abc\"\n")
            .expect("parse config");
        assert_eq!(config.url, "https://kanso.example");
        assert_eq!(config.token, "kanso_abc");
    }

    #[test]
    fn rejects_missing_fields_and_malformed_toml() {
        assert!(parse("url = \"https://kanso.example\"\n").is_err());
        assert!(parse("url = [").is_err());
    }

    #[test]
    fn environment_wins_over_the_file() {
        let config = ConfigFile::new("https://file.example", "kanso_file");
        let (url, token) = merge(
            Some("https://env.example"),
            Some("kanso_env"),
            Some(&config),
            Path::new("/tmp/config.toml"),
        )
        .expect("merge");
        assert_eq!(url, "https://env.example");
        assert_eq!(token, "kanso_env");
    }

    #[test]
    fn file_fills_in_missing_environment_values() {
        let config = ConfigFile::new("https://file.example", "kanso_file");
        let (url, token) = merge(
            Some("https://env.example"),
            None,
            Some(&config),
            Path::new("/tmp/config.toml"),
        )
        .expect("merge");
        assert_eq!(url, "https://env.example");
        assert_eq!(token, "kanso_file");
    }

    #[test]
    fn missing_values_are_usage_errors() {
        let error = merge(None, None, None, Path::new("/tmp/config.toml")).unwrap_err();
        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("no server URL"), "{error}");
    }

    #[test]
    fn urls_must_be_http_or_https_with_a_host() {
        assert_eq!(
            validate_url(" https://kanso.example/ ").expect("url"),
            "https://kanso.example"
        );
        assert_eq!(
            validate_url("http://127.0.0.1:8087").expect("url"),
            "http://127.0.0.1:8087"
        );
        assert_eq!(
            validate_url("http://localhost:8087").expect("url"),
            "http://localhost:8087"
        );
        assert_eq!(
            validate_url("http://[::1]:8087").expect("url"),
            "http://[::1]:8087"
        );
        assert!(validate_url("http://kanso.example").is_err());
        assert!(validate_url("http://127.0.0.1:8087@evil.example").is_err());
        for invalid in [
            "kanso.example",
            "ftp://kanso.example",
            "https://",
            "http:///x",
            "https://kanso.example/path",
        ] {
            assert!(validate_url(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn writes_a_private_file_and_refuses_to_clobber_it() {
        let dir = TempDir::new();
        let path = dir.path().join("kanso/config.toml");
        let config = ConfigFile::new("https://kanso.example", "kanso_abc");

        write(&path, &config, false).expect("write config");
        let text = fs::read_to_string(&path).expect("read config");
        assert!(text.contains("url = \"https://kanso.example\""));
        assert!(text.contains("token = \"kanso_abc\""));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let error = write(&path, &config, false).unwrap_err();
        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("--force"), "{error}");

        write(
            &path,
            &ConfigFile::new("https://other.example", "kanso_def"),
            true,
        )
        .expect("forced write");
        let text = fs::read_to_string(&path).expect("read config");
        assert!(text.contains("https://other.example"));
    }

    #[cfg(unix)]
    #[test]
    fn mode_check_accepts_only_owner_access() {
        assert!(mode_is_private(0o600));
        assert!(mode_is_private(0o400));
        assert!(!mode_is_private(0o640));
        assert!(!mode_is_private(0o604));
        assert!(!mode_is_private(0o644));
        assert!(!mode_is_private(0o666));
    }

    #[cfg(unix)]
    #[test]
    fn reading_repairs_a_world_readable_config_to_0600() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new();
        let path = dir.path().join("kanso/config.toml");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(
            &path,
            "url = \"https://kanso.example\"\ntoken = \"kanso_abc\"\n",
        )
        .expect("write config");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod 644");

        let config = read(&path).expect("read config").expect("config");
        assert_eq!(config.token, "kanso_abc");
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
