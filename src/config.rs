use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use crate::error::{Error, Result};

pub const DEFAULT_DATA_DIR: &str = "/data";
pub const DEFAULT_BIND: &str = "0.0.0.0:8087";
pub const DEFAULT_LOG: &str = "info";

/// OpenChamber connection settings; present only when both variables are set.
#[derive(Clone)]
pub struct OpenChamberConfig {
    url: String,
    token: String,
}

impl OpenChamberConfig {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for OpenChamberConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenChamberConfig")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Runtime configuration, read from the environment.
#[derive(Clone)]
pub struct Config {
    password: String,
    pub data_dir: PathBuf,
    pub public_url: Option<String>,
    pub bind: SocketAddr,
    pub log: String,
    /// `Some` only when both `KANSO_OPENCHAMBER_URL` and
    /// `KANSO_OPENCHAMBER_TOKEN` are non-empty.
    pub openchamber: Option<OpenChamberConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mut vars = Vec::new();
        for (key, value) in std::env::vars_os() {
            let key = key.to_string_lossy();
            if !key.starts_with("KANSO_") {
                continue;
            }
            let key = key.into_owned();
            let value = value.into_string().map_err(|_| {
                Error::msg(format!("environment variable {key} must be valid UTF-8"))
            })?;
            vars.push((key, value));
        }
        Self::from_vars(vars)
    }

    /// Build a configuration from arbitrary key/value pairs, for testability.
    pub fn from_vars<I, K, V>(vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let vars: HashMap<String, String> = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        Self::from_map(&vars)
    }

    fn from_map(vars: &HashMap<String, String>) -> Result<Self> {
        let password = vars
            .get("KANSO_PASSWORD")
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| Error::msg("KANSO_PASSWORD is required; set it to the UI password"))?;

        let data_dir = vars
            .get("KANSO_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));

        let public_url = vars
            .get("KANSO_PUBLIC_URL")
            .filter(|value| !value.is_empty())
            .cloned();

        let bind = vars
            .get("KANSO_BIND")
            .filter(|value| !value.is_empty())
            .map(String::as_str)
            .unwrap_or(DEFAULT_BIND);
        let bind = SocketAddr::from_str(bind)
            .map_err(|error| Error::msg(format!("KANSO_BIND must be a socket address: {error}")))?;

        let log = vars
            .get("KANSO_LOG")
            .filter(|value| !value.is_empty())
            .cloned()
            .unwrap_or_else(|| DEFAULT_LOG.to_owned());

        // Both variables are required: setting only one is treated as disabled
        // rather than failing startup, so the board keeps working without the
        // optional integration.
        let openchamber_url = vars
            .get("KANSO_OPENCHAMBER_URL")
            .map(|value| value.trim().trim_end_matches('/'))
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let openchamber_token = vars
            .get("KANSO_OPENCHAMBER_TOKEN")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let openchamber = match (openchamber_url, openchamber_token) {
            (Some(url), Some(token)) => Some(OpenChamberConfig { url, token }),
            _ => None,
        };

        Ok(Self {
            password,
            data_dir,
            public_url,
            bind,
            log,
            openchamber,
        })
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("kanso.db")
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("password", &"<redacted>")
            .field("data_dir", &self.data_dir)
            .field("public_url", &self.public_url)
            .field("bind", &self.bind)
            .field("log", &self.log)
            .field("openchamber", &self.openchamber)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config> {
        Config::from_vars(vars.iter().copied())
    }

    #[test]
    fn password_is_required() {
        let error = config(&[]).unwrap_err().to_string();
        assert!(error.contains("KANSO_PASSWORD"), "{error}");
    }

    #[test]
    fn empty_password_is_rejected() {
        let error = config(&[("KANSO_PASSWORD", "")]).unwrap_err().to_string();
        assert!(error.contains("KANSO_PASSWORD"), "{error}");
    }

    #[test]
    fn defaults_are_applied() {
        let config = config(&[("KANSO_PASSWORD", "secret")]).unwrap();
        assert_eq!(config.password(), "secret");
        assert_eq!(config.data_dir, PathBuf::from("/data"));
        assert_eq!(config.bind, "0.0.0.0:8087".parse().unwrap());
        assert_eq!(config.log, "info");
        assert_eq!(config.public_url, None);
        assert!(config.openchamber.is_none());
        assert_eq!(config.db_path(), PathBuf::from("/data/kanso.db"));
    }

    #[test]
    fn environment_overrides_are_applied() {
        let config = config(&[
            ("KANSO_PASSWORD", "secret"),
            ("KANSO_DATA_DIR", "/tmp/kanso"),
            ("KANSO_PUBLIC_URL", "https://kanso.example"),
            ("KANSO_BIND", "127.0.0.1:9000"),
            ("KANSO_LOG", "info,kanso=debug"),
        ])
        .unwrap();
        assert_eq!(config.data_dir, PathBuf::from("/tmp/kanso"));
        assert_eq!(config.public_url.as_deref(), Some("https://kanso.example"));
        assert_eq!(config.bind, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(config.log, "info,kanso=debug");
        assert_eq!(config.db_path(), PathBuf::from("/tmp/kanso/kanso.db"));
    }

    #[test]
    fn invalid_bind_is_rejected() {
        let error = config(&[("KANSO_PASSWORD", "secret"), ("KANSO_BIND", "nope")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("KANSO_BIND"), "{error}");
    }

    #[test]
    fn debug_redacts_the_password() {
        let config = config(&[("KANSO_PASSWORD", "secret")]).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("secret"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn openchamber_requires_both_variables() {
        let enabled = config(&[
            ("KANSO_PASSWORD", "secret"),
            ("KANSO_OPENCHAMBER_URL", "http://127.0.0.1:3000/"),
            ("KANSO_OPENCHAMBER_TOKEN", "oc_client_test"),
        ])
        .unwrap()
        .openchamber
        .expect("integration");
        assert_eq!(enabled.url(), "http://127.0.0.1:3000");
        assert_eq!(enabled.token(), "oc_client_test");

        for vars in [
            vec![("KANSO_PASSWORD", "secret")],
            vec![
                ("KANSO_PASSWORD", "secret"),
                ("KANSO_OPENCHAMBER_URL", "http://127.0.0.1:3000"),
            ],
            vec![
                ("KANSO_PASSWORD", "secret"),
                ("KANSO_OPENCHAMBER_TOKEN", "oc_client_test"),
            ],
            vec![
                ("KANSO_PASSWORD", "secret"),
                ("KANSO_OPENCHAMBER_URL", "  "),
                ("KANSO_OPENCHAMBER_TOKEN", "oc_client_test"),
            ],
        ] {
            assert!(
                config(&vars).unwrap().openchamber.is_none(),
                "{vars:?} should be disabled"
            );
        }
    }

    #[test]
    fn debug_redacts_the_openchamber_token() {
        let config = config(&[
            ("KANSO_PASSWORD", "secret"),
            ("KANSO_OPENCHAMBER_URL", "http://127.0.0.1:3000"),
            ("KANSO_OPENCHAMBER_TOKEN", "oc_client_supersecret"),
        ])
        .unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("oc_client_supersecret"), "{debug}");
        assert!(debug.contains("127.0.0.1:3000"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }
}
