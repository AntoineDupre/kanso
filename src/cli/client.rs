use std::time::Duration;

use serde_json::Value;

use super::config;
use super::CliError;

#[derive(Clone, Copy, Debug)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: String,
}

pub struct ApiResponse {
    pub status: u16,
    pub body: String,
}

impl ApiResponse {
    /// Return the body of a 2xx response, otherwise map the API envelope to a
    /// CLI error. A 204 has an empty body.
    pub fn success(self) -> Result<String, CliError> {
        if (200..300).contains(&self.status) {
            Ok(self.body)
        } else {
            Err(self.error())
        }
    }

    pub fn error(self) -> CliError {
        let parsed: Value = serde_json::from_str(&self.body).unwrap_or(Value::Null);
        let code = parsed
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("http")
            .to_owned();
        let message = parsed
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| {
                if self.body.trim().is_empty() {
                    format!("server returned HTTP {}", self.status)
                } else {
                    self.body.trim().to_owned()
                }
            });
        CliError::Api {
            status: self.status,
            code,
            message,
        }
    }
}

impl Client {
    pub fn new(url: &str, token: &str) -> Result<Self, CliError> {
        Self::configured(url, token, false)
    }

    fn configured(url: &str, token: &str, secret: bool) -> Result<Self, CliError> {
        let base = config::validate_url(url)?;
        let token = token.trim();
        if token.is_empty() {
            return Err(CliError::usage("token must not be empty"));
        }
        let mut builder = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)));
        if secret {
            builder = builder.max_redirects(0);
        }
        let config = builder.build();
        Ok(Self {
            agent: config.into(),
            base,
            token: token.to_owned(),
        })
    }

    pub fn from_config() -> Result<Self, CliError> {
        let (url, token) = config::resolve()?;
        Self::new(&url, &token)
    }

    /// Secret requests never follow a redirect carrying a capability or value.
    pub fn secret_from_config() -> Result<Self, CliError> {
        let (url, token) = config::resolve()?;
        Self::configured(&url, &token, true)
    }

    pub fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        agent: Option<&str>,
    ) -> Result<ApiResponse, CliError> {
        let url = format!("{}{}", self.base, path);
        let authorization = format!("Bearer {}", self.token);

        let result = match method {
            Method::Get => {
                let mut builder = self.agent.get(&url);
                for (key, value) in query {
                    builder = builder.query(*key, value.as_str());
                }
                builder = builder.header("Authorization", &authorization);
                if let Some(agent) = agent {
                    builder = builder.header(crate::api::AGENT_HEADER, agent);
                }
                builder.call()
            }
            Method::Delete => {
                let mut builder = self.agent.delete(&url);
                for (key, value) in query {
                    builder = builder.query(*key, value.as_str());
                }
                builder = builder.header("Authorization", &authorization);
                if let Some(agent) = agent {
                    builder = builder.header(crate::api::AGENT_HEADER, agent);
                }
                builder.call()
            }
            Method::Post => {
                let mut builder = self.agent.post(&url);
                for (key, value) in query {
                    builder = builder.query(*key, value.as_str());
                }
                builder = builder.header("Authorization", &authorization);
                if let Some(agent) = agent {
                    builder = builder.header(crate::api::AGENT_HEADER, agent);
                }
                match body {
                    Some(body) => builder
                        .header("Content-Type", "application/json")
                        .send(body.to_string()),
                    None => builder.send_empty(),
                }
            }
            Method::Patch => {
                let mut builder = self.agent.patch(&url);
                for (key, value) in query {
                    builder = builder.query(*key, value.as_str());
                }
                builder = builder.header("Authorization", &authorization);
                if let Some(agent) = agent {
                    builder = builder.header(crate::api::AGENT_HEADER, agent);
                }
                match body {
                    Some(body) => builder
                        .header("Content-Type", "application/json")
                        .send(body.to_string()),
                    None => builder.send_empty(),
                }
            }
        };

        Self::read_response(result)
    }

    /// POST with a longer deadline, used by `backup`: `VACUUM INTO` can take
    /// minutes on a large database while the normal 30 s deadline is for
    /// ordinary API calls.
    pub fn post_with_timeout(
        &self,
        path: &str,
        body: &Value,
        timeout: std::time::Duration,
    ) -> Result<ApiResponse, CliError> {
        let url = format!("{}{}", self.base, path);
        let authorization = format!("Bearer {}", self.token);
        let builder = self
            .agent
            .post(&url)
            .header("Authorization", &authorization)
            .header("Content-Type", "application/json")
            .config()
            .timeout_global(Some(timeout))
            .build();
        Self::read_response(builder.send(body.to_string()))
    }

    fn read_response(
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<ApiResponse, CliError> {
        match result {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let body = response.body_mut().read_to_string().map_err(|error| {
                    CliError::Network(format!("failed to read response: {error}"))
                })?;
                Ok(ApiResponse { status, body })
            }
            Err(error) => Err(CliError::Network(error.to_string())),
        }
    }

    pub fn get(&self, path: &str) -> Result<ApiResponse, CliError> {
        self.request(Method::Get, path, &[], None, None)
    }
}
