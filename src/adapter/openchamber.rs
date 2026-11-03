//! The OpenChamber control client and the pure parts of the adapter.
//!
//! OpenChamber 2.6 exposes one JSON control endpoint
//! (`POST /api/openchamber/control`), a public version probe and a
//! permission-auto-accept endpoint. Everything here is isolated from the rest
//! of the server so a contract change only touches this module.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api::model::{Project, Task};

/// The version probe is cheap, but the settings screen must not hammer it.
pub const VERSION_CACHE_TTL: Duration = Duration::from_secs(5);
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(2);
pub const AUTO_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
pub const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Worktree provisioning can take about two minutes, so dispatch waits above
/// that before giving up.
pub const DISPATCH_TIMEOUT: Duration = Duration::from_secs(150);

const MAX_SLUG_CHARS: usize = 40;
const DEFAULT_AGENT: &str = "build";
const DEFAULT_BASE_BRANCH: &str = "main";

/// The built-in prompt template from PLAN §9.
pub const DEFAULT_PROMPT_TEMPLATE: &str = "You are working on card {{ref}}: {{title}}
Project: {{project}}

{{description}}

Rules:
- Work only inside the current worktree.
- Report progress with the CLI: `kanso comment {{ref}} \"…\"`.
- When finished, run `kanso move {{ref}} in_review` and comment a summary.
- If blocked, run `kanso move {{ref}} blocker --reason \"…\"`.
- Never set the card to done, never delete it, never touch other cards.";

/// One failure from OpenChamber, classified so the API can answer 502 for an
/// upstream rejection and 503 when the service cannot be reached at all.
#[derive(Clone, Debug)]
pub enum OpenChamberError {
    Transport(String),
    /// OpenChamber answered 2xx but the body did not carry the fields Kanso
    /// needs (for example a session id or directory).
    Protocol(String),
    Http {
        status: u16,
        message: String,
    },
}

impl fmt::Display for OpenChamberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "could not reach OpenChamber: {message}"),
            Self::Protocol(message) => {
                write!(
                    f,
                    "OpenChamber answered with an invalid response: {message}"
                )
            }
            Self::Http { status, message } => {
                write!(f, "OpenChamber returned HTTP {status}: {message}")
            }
        }
    }
}

/// The result of the public `/api/version` probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionInfo {
    pub reachable: bool,
    pub version: Option<String>,
}

#[derive(Clone, Debug)]
struct ProbeEntry {
    checked_at: Instant,
    value: VersionInfo,
}

/// A configured OpenChamber endpoint. Cloning shares the version cache.
#[derive(Clone)]
pub struct OpenChamber {
    base: Arc<str>,
    token: Arc<str>,
    probe: Arc<Mutex<Option<ProbeEntry>>>,
    probe_lock: Arc<tokio::sync::Mutex<()>>,
}

impl OpenChamber {
    pub fn new(url: &str, token: &str) -> Self {
        Self {
            base: Arc::from(url.trim().trim_end_matches('/')),
            token: Arc::from(token.trim()),
            probe: Arc::new(Mutex::new(None)),
            probe_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    fn cached_probe(&self, now: Instant) -> Option<VersionInfo> {
        let cache = self.probe.lock().unwrap_or_else(|error| error.into_inner());
        cache.as_ref().and_then(|entry| {
            (now.duration_since(entry.checked_at) < VERSION_CACHE_TTL).then(|| entry.value.clone())
        })
    }

    fn store_probe(&self, value: VersionInfo) {
        let mut cache = self.probe.lock().unwrap_or_else(|error| error.into_inner());
        *cache = Some(ProbeEntry {
            checked_at: Instant::now(),
            value,
        });
    }

    /// `GET /api/version`, cached for a few seconds and never failing the API.
    pub async fn version(&self) -> VersionInfo {
        if let Some(value) = self.cached_probe(Instant::now()) {
            return value;
        }

        let _guard = self.probe_lock.lock().await;
        if let Some(value) = self.cached_probe(Instant::now()) {
            return value;
        }

        let base = self.base.to_string();
        let value = tokio::task::spawn_blocking(move || probe_version(&base))
            .await
            .unwrap_or(VersionInfo {
                reachable: false,
                version: None,
            });
        self.store_probe(value.clone());
        value
    }

    /// `session.create`, given the full control payload from
    /// [`build_session_create`]. Returns the session id and the directory
    /// OpenChamber actually used (the worktree path, not the repo path).
    pub async fn create_session(
        &self,
        payload: Value,
        timeout: Duration,
    ) -> Result<CreatedSession, OpenChamberError> {
        let base = self.base.to_string();
        let token = self.token.to_string();
        tokio::task::spawn_blocking(move || {
            let value = post_control(&base, &token, &payload, timeout)?;
            let session_id =
                string_field(&value, &["sessionId", "session_id", "id"]).ok_or_else(|| {
                    OpenChamberError::Protocol("session.create returned no id".into())
                })?;
            let directory =
                string_field(&value, &["directory", "worktree", "path"]).ok_or_else(|| {
                    OpenChamberError::Protocol("session.create returned no directory".into())
                })?;
            Ok(CreatedSession {
                session_id,
                directory,
            })
        })
        .await
        .map_err(|error| OpenChamberError::Transport(error.to_string()))?
    }

    /// Enable auto-accept for one session so it never waits for a permission
    /// prompt that has no UI to answer it.
    pub async fn auto_accept(
        &self,
        session_id: &str,
        directory: &str,
    ) -> Result<(), OpenChamberError> {
        let base = self.base.to_string();
        let token = self.token.to_string();
        let session_id = session_id.to_owned();
        let directory = directory.to_owned();
        tokio::task::spawn_blocking(move || {
            let url = format!(
                "{}/api/permission-auto-accept/sessions/{}",
                base,
                percent_encode_path(&session_id)
            );
            let payload = json!({ "mode": "auto", "directory": directory });
            let agent = agent(AUTO_ACCEPT_TIMEOUT);
            let response = agent
                .put(&url)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .send(payload.to_string());
            read_response(response).map(|_| ())
        })
        .await
        .map_err(|error| OpenChamberError::Transport(error.to_string()))?
    }

    /// `session.list` for one directory. One call covers every card whose
    /// session lives in that directory.
    pub async fn list_sessions(
        &self,
        directory: &str,
    ) -> Result<Vec<SessionSummary>, OpenChamberError> {
        let base = self.base.to_string();
        let token = self.token.to_string();
        let directory = directory.to_owned();
        tokio::task::spawn_blocking(move || {
            let payload = json!({
                "action": "session.list",
                "input": { "withStatus": true, "directory": directory },
            });
            let value = post_control(&base, &token, &payload, LIST_TIMEOUT)?;
            Ok(parse_session_list(&value))
        })
        .await
        .map_err(|error| OpenChamberError::Transport(error.to_string()))?
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedSession {
    pub session_id: String,
    pub directory: String,
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn probe_version(base: &str) -> VersionInfo {
    let url = format!("{}/api/version", base.trim_end_matches('/'));
    let response = agent(VERSION_TIMEOUT)
        .get(&url)
        .header("Accept", "application/json")
        .call();
    match read_response(response) {
        Ok(value) => VersionInfo {
            reachable: true,
            version: string_field(&value, &["openchamberVersion", "version"]),
        },
        Err(_) => VersionInfo {
            reachable: false,
            version: None,
        },
    }
}

fn post_control(
    base: &str,
    token: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<Value, OpenChamberError> {
    let url = format!("{}/api/openchamber/control", base.trim_end_matches('/'));
    let response = agent(timeout)
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(payload.to_string());
    read_response(response)
}

fn read_response(
    result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Value, OpenChamberError> {
    match result {
        Ok(mut response) => {
            let status = response.status().as_u16();
            let body = response
                .body_mut()
                .read_to_string()
                .map_err(|error| OpenChamberError::Transport(error.to_string()))?;
            if !(200..300).contains(&status) {
                return Err(OpenChamberError::Http {
                    status,
                    message: error_message(&body),
                });
            }
            if body.trim().is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(&body)
                .map_err(|error| OpenChamberError::Protocol(format!("invalid JSON: {error}")))
        }
        Err(error) => Err(OpenChamberError::Transport(error.to_string())),
    }
}

fn error_message(body: &str) -> String {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if let Some(message) = parsed.get("error").and_then(Value::as_str) {
        if !message.is_empty() {
            return message.to_owned();
        }
    }
    if let Some(message) = parsed
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        if !message.is_empty() {
            return message.to_owned();
        }
    }
    if let Some(message) = parsed.get("message").and_then(Value::as_str) {
        if !message.is_empty() {
            return message.to_owned();
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty error response".to_owned()
    } else {
        trimmed.chars().take(500).collect()
    }
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn percent_encode_path(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

// ---------------------------------------------------------------------------
// Session status mapping

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Busy,
    Idle,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub state: SessionState,
    pub outcome: Option<String>,
    /// OpenChamber exposes `time.busy` when the session has actually run, which
    /// distinguishes a real idle session from one that never started.
    pub busy_at: Option<i64>,
    pub idle_at: Option<i64>,
    pub has_assistant_message: bool,
}

/// What Kanso does with one observed session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappedStatus {
    /// Session is running; the card stays where it is.
    Running,
    /// Session finished successfully.
    Idle,
    /// Session finished unsuccessfully or never really ran; the card stays and
    /// asks for attention.
    Failed(&'static str),
    /// No usable status in this OpenChamber version.
    Unknown,
}

#[derive(Clone, Debug)]
pub struct SessionObservation {
    pub state: SessionState,
    pub outcome: Option<String>,
    /// Whether this session was ever observed busy (poller memory) or reports
    /// `time.busy` (durable across restarts).
    pub ever_busy: bool,
    pub has_assistant_message: bool,
}

const SUCCESS_OUTCOMES: [&str; 4] = ["succeeded", "success", "completed", "done"];

pub fn outcome_is_success(outcome: Option<&str>) -> bool {
    match outcome.map(str::trim).filter(|value| !value.is_empty()) {
        None => true,
        Some(value) => SUCCESS_OUTCOMES
            .iter()
            .any(|success| value.eq_ignore_ascii_case(success)),
    }
}

/// The status table from PLAN §9, adapted to the data OpenChamber 2.6 exposes.
pub fn map_observation(observation: &SessionObservation) -> MappedStatus {
    match observation.state {
        SessionState::Busy => MappedStatus::Running,
        SessionState::Unknown => MappedStatus::Unknown,
        SessionState::Idle => {
            if !outcome_is_success(observation.outcome.as_deref()) {
                MappedStatus::Failed("outcome")
            } else if !observation.ever_busy && !observation.has_assistant_message {
                MappedStatus::Failed("suspicious_idle")
            } else {
                MappedStatus::Idle
            }
        }
    }
}

pub fn session_observation(summary: &SessionSummary, seen_busy: bool) -> SessionObservation {
    SessionObservation {
        state: summary.state,
        outcome: summary.outcome.clone(),
        ever_busy: seen_busy || summary.busy_at.is_some(),
        has_assistant_message: summary.has_assistant_message,
    }
}

fn parse_session_list(value: &Value) -> Vec<SessionSummary> {
    let entries = if let Some(array) = value.as_array() {
        Some(array)
    } else {
        ["sessions", "items", "data"]
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_array))
    };
    entries
        .map(|entries| entries.iter().filter_map(parse_session).collect())
        .unwrap_or_default()
}

fn parse_session(value: &Value) -> Option<SessionSummary> {
    let id = string_field(value, &["id", "sessionId", "session_id"])?;
    let status = value.get("status");
    let kind = status.and_then(Value::as_str).or_else(|| {
        status
            .and_then(|status| status.get("type"))
            .and_then(Value::as_str)
    });
    let state = match kind {
        Some("busy") | Some("running") => SessionState::Busy,
        Some("idle") => SessionState::Idle,
        _ => SessionState::Unknown,
    };
    let time = value.get("time");
    let busy_at = number_field(time, "busy");
    let idle_at = number_field(time, "idle");
    Some(SessionSummary {
        id,
        state,
        outcome: string_field(value, &["outcome"]),
        busy_at,
        idle_at,
        has_assistant_message: has_assistant_message(value.get("metadata")),
    })
}

fn number_field(value: Option<&Value>, name: &str) -> Option<i64> {
    let value = value?.get(name)?;
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
}

/// Best-effort transcript check: OpenChamber 2.6's `session.list` may include
/// assistant counters in `metadata`, but the contract does not promise them.
fn has_assistant_message(metadata: Option<&Value>) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    for key in [
        "assistant_messages",
        "assistantMessages",
        "assistant_message_count",
    ] {
        match metadata.get(key) {
            Some(Value::Bool(value)) => {
                if *value {
                    return true;
                }
            }
            Some(Value::Number(number)) => {
                if number.as_i64().unwrap_or(0) > 0 {
                    return true;
                }
            }
            Some(Value::Array(items)) if !items.is_empty() => {
                return true;
            }
            _ => {}
        }
    }
    if let Some(Value::Array(messages)) = metadata.get("messages") {
        return messages.iter().any(|message| {
            message
                .get("role")
                .and_then(Value::as_str)
                .map(|role| role == "assistant")
                .unwrap_or(false)
        });
    }
    false
}

// ---------------------------------------------------------------------------
// Dispatch payload

/// Lowercase ASCII slug used for the worktree branch: `[a-z0-9-]`, runs
/// collapsed, trimmed and capped. Deterministic for the same card title.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() {
            separator = true;
        }
    }
    if slug.len() > MAX_SLUG_CHARS {
        slug.truncate(MAX_SLUG_CHARS);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "task".to_owned()
    } else {
        slug
    }
}

pub struct PromptValues<'a> {
    pub reference: &'a str,
    pub title: &'a str,
    pub project: &'a str,
    pub description: &'a str,
}

impl PromptValues<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        match name {
            "ref" => Some(self.reference),
            "title" => Some(self.title),
            "project" => Some(self.project),
            "description" => Some(self.description),
            _ => None,
        }
    }
}

/// Substitute the four known placeholders in one pass. Unknown placeholders
/// stay untouched, and substituted text is never scanned again.
pub fn render_prompt(template: &str, values: &PromptValues<'_>) -> String {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            output.push_str(&rest[start..]);
            return output;
        };
        let name = &after[..end];
        match values.get(name) {
            Some(value) => output.push_str(value),
            None => output.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    output.push_str(rest);
    output
}

pub fn worktree_branch(task: &Task) -> String {
    format!("kanso/{}-{}", task.reference, slugify(&task.title))
}

/// Build the exact `session.create` control payload for one card.
pub fn build_session_create(project: &Project, task: &Task) -> Value {
    let template = project
        .prompt_template
        .as_deref()
        .filter(|template| !template.trim().is_empty())
        .unwrap_or(DEFAULT_PROMPT_TEMPLATE);
    let prompt = render_prompt(
        template,
        &PromptValues {
            reference: &task.reference,
            title: &task.title,
            project: &project.name,
            description: &task.description,
        },
    );
    let branch = worktree_branch(task);

    let mut input = serde_json::Map::new();
    input.insert(
        "directory".to_owned(),
        json!(project.repo_path.clone().unwrap_or_default()),
    );
    input.insert(
        "title".to_owned(),
        json!(format!("{} — {}", task.reference, task.title)),
    );
    input.insert("prompt".to_owned(), json!(prompt));
    input.insert(
        "agent".to_owned(),
        json!(project
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|agent| !agent.is_empty())
            .unwrap_or(DEFAULT_AGENT)),
    );
    if let Some(model) = project
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        input.insert("model".to_owned(), json!(model));
    }
    input.insert(
        "worktree".to_owned(),
        if project.worktree {
            json!(branch)
        } else {
            json!(false)
        },
    );
    input.insert("branch".to_owned(), json!(branch));
    input.insert(
        "startRef".to_owned(),
        json!(project
            .base_branch
            .as_deref()
            .map(str::trim)
            .filter(|branch| !branch.is_empty())
            .unwrap_or(DEFAULT_BASE_BRANCH)),
    );
    input.insert("wait".to_owned(), json!(false));

    json!({ "action": "session.create", "input": Value::Object(input) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        Project {
            id: "p1".to_owned(),
            key: "ALP".to_owned(),
            name: "Alpha".to_owned(),
            description: String::new(),
            repo_path: Some("/home/me/code/alpha".to_owned()),
            color: "#64748b".to_owned(),
            position: 1.0,
            openchamber_project_id: None,
            agent: None,
            model: None,
            base_branch: None,
            worktree: true,
            prompt_template: None,
            archived_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn task(title: &str) -> Task {
        Task {
            id: "t1".to_owned(),
            project_id: "p1".to_owned(),
            project_key: "ALP".to_owned(),
            number: 42,
            reference: "ALP-42".to_owned(),
            title: title.to_owned(),
            description: "Some details.".to_owned(),
            status: "todo".to_owned(),
            state: "active".to_owned(),
            blocked_from: None,
            blocked_reason: None,
            rank: 1.0,
            priority: "normal".to_owned(),
            due_date: None,
            source: "human".to_owned(),
            created_by: Some("ui".to_owned()),
            claimed_by: None,
            session_id: None,
            session_directory: None,
            session_status: None,
            attention: false,
            done_at: None,
            archived_at: None,
            created_at: 1,
            updated_at: 1,
            labels: Vec::new(),
        }
    }

    #[test]
    fn slugs_are_ascii_lowercase_and_deterministic() {
        assert_eq!(slugify("Fix the Flux Capacitor"), "fix-the-flux-capacitor");
        assert_eq!(slugify("Fix   the -- Flux"), "fix-the-flux");
        assert_eq!(slugify("  leading and trailing  "), "leading-and-trailing");
        assert_eq!(slugify("ALP-42: déjà vu!"), "alp-42-d-j-vu");
        assert_eq!(slugify("---"), "task");
        assert_eq!(slugify("日本語"), "task");
        assert_eq!(
            slugify("A very long title that keeps going and going"),
            "a-very-long-title-that-keeps-going-and-g"
        );
        assert_eq!(
            slugify("A very long title that keeps going and going").len(),
            40
        );
        assert_eq!(
            slugify("Fix the Flux Capacitor"),
            slugify("Fix the Flux Capacitor")
        );
    }

    #[test]
    fn prompt_rendering_replaces_only_known_placeholders_once() {
        let values = PromptValues {
            reference: "ALP-42",
            title: "Fix {{ref}}",
            project: "Alpha",
            description: "Body with {{unknown}} and {{title}}.",
        };
        let rendered = render_prompt(
            "{{ref}} {{title}} {{project}}\n{{description}}\n{{unknown}} {{ref}}",
            &values,
        );
        assert_eq!(
            rendered,
            "ALP-42 Fix {{ref}} Alpha\nBody with {{unknown}} and {{title}}.\n{{unknown}} ALP-42"
        );
    }

    #[test]
    fn an_unclosed_placeholder_is_left_alone() {
        let values = PromptValues {
            reference: "ALP-42",
            title: "Title",
            project: "Alpha",
            description: "",
        };
        assert_eq!(render_prompt("a {{ref", &values), "a {{ref");
        assert_eq!(render_prompt("a {{ref}} b {{", &values), "a ALP-42 b {{");
    }

    #[test]
    fn payload_uses_plan_defaults() {
        let payload = build_session_create(&project(), &task("Fix the Flux Capacitor"));
        assert_eq!(payload["action"], "session.create");
        let input = &payload["input"];
        assert_eq!(input["directory"], "/home/me/code/alpha");
        assert_eq!(input["title"], "ALP-42 — Fix the Flux Capacitor");
        assert_eq!(input["agent"], "build");
        assert_eq!(input["worktree"], "kanso/ALP-42-fix-the-flux-capacitor");
        assert_eq!(input["branch"], "kanso/ALP-42-fix-the-flux-capacitor");
        assert_eq!(input["startRef"], "main");
        assert_eq!(input["wait"], false);
        assert!(input.get("model").is_none());
        assert!(input["prompt"]
            .as_str()
            .expect("prompt")
            .contains("You are working on card ALP-42: Fix the Flux Capacitor"));
        assert!(input["prompt"]
            .as_str()
            .expect("prompt")
            .contains("Some details."));
    }

    #[test]
    fn payload_uses_project_overrides_and_custom_template() {
        let mut project = project();
        project.agent = Some("plan".to_owned());
        project.model = Some("provider/model".to_owned());
        project.base_branch = Some("develop".to_owned());
        project.worktree = false;
        project.prompt_template =
            Some("Card {{ref}} — {{title}}\n{{project}}\n{{description}}".into());
        let payload = build_session_create(&project, &task("Fix it"));
        let input = &payload["input"];
        assert_eq!(input["agent"], "plan");
        assert_eq!(input["model"], "provider/model");
        assert_eq!(input["worktree"], false);
        assert_eq!(input["branch"], "kanso/ALP-42-fix-it");
        assert_eq!(input["startRef"], "develop");
        assert_eq!(
            input["prompt"],
            "Card ALP-42 — Fix it\nAlpha\nSome details."
        );
    }

    #[test]
    fn empty_project_settings_fall_back_to_defaults() {
        let mut project = project();
        project.agent = Some("   ".to_owned());
        project.model = Some(String::new());
        project.base_branch = Some(String::new());
        project.prompt_template = Some("   ".to_owned());
        let payload = build_session_create(&project, &task("Fix it"));
        let input = &payload["input"];
        assert_eq!(input["agent"], "build");
        assert_eq!(input["startRef"], "main");
        assert!(input.get("model").is_none());
        assert!(input["prompt"].as_str().expect("prompt").contains("Rules:"));
    }

    #[test]
    fn status_mapping_matches_the_m5_table() {
        let busy = SessionObservation {
            state: SessionState::Busy,
            outcome: None,
            ever_busy: false,
            has_assistant_message: false,
        };
        assert_eq!(map_observation(&busy), MappedStatus::Running);

        let idle = SessionObservation {
            state: SessionState::Idle,
            outcome: Some("succeeded".to_owned()),
            ever_busy: true,
            has_assistant_message: false,
        };
        assert_eq!(map_observation(&idle), MappedStatus::Idle);

        let failed = SessionObservation {
            state: SessionState::Idle,
            outcome: Some("error".to_owned()),
            ever_busy: true,
            has_assistant_message: true,
        };
        assert_eq!(map_observation(&failed), MappedStatus::Failed("outcome"));

        let suspicious = SessionObservation {
            state: SessionState::Idle,
            outcome: None,
            ever_busy: false,
            has_assistant_message: false,
        };
        assert_eq!(
            map_observation(&suspicious),
            MappedStatus::Failed("suspicious_idle")
        );

        let assisted = SessionObservation {
            has_assistant_message: true,
            ..suspicious
        };
        assert_eq!(map_observation(&assisted), MappedStatus::Idle);

        let unknown = SessionObservation {
            state: SessionState::Unknown,
            outcome: None,
            ever_busy: false,
            has_assistant_message: false,
        };
        assert_eq!(map_observation(&unknown), MappedStatus::Unknown);
    }

    #[test]
    fn success_outcomes_are_case_insensitive() {
        for outcome in ["succeeded", "Success", "completed", "done", ""] {
            assert!(outcome_is_success(Some(outcome)), "{outcome}");
        }
        for outcome in ["failed", "error", "cancelled"] {
            assert!(!outcome_is_success(Some(outcome)), "{outcome}");
        }
        assert!(outcome_is_success(None));
    }

    #[test]
    fn session_list_accepts_both_envelopes_and_status_shapes() {
        let value = json!({
            "sessions": [
                {"id": "ses_1", "status": {"type": "busy"}, "time": {"busy": 10}},
                {"sessionId": "ses_2", "status": "idle", "outcome": "failed"}
            ]
        });
        let sessions = parse_session_list(&value);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "ses_1");
        assert_eq!(sessions[0].state, SessionState::Busy);
        assert_eq!(sessions[0].busy_at, Some(10));
        assert_eq!(sessions[1].id, "ses_2");
        assert_eq!(sessions[1].state, SessionState::Idle);
        assert_eq!(sessions[1].outcome.as_deref(), Some("failed"));

        let array = json!([{"id": "ses_3", "status": {"type": "idle"}}]);
        assert_eq!(parse_session_list(&array).len(), 1);
        assert!(parse_session_list(&json!({})).is_empty());
    }

    #[test]
    fn session_ids_are_percent_encoded_in_paths() {
        assert_eq!(
            percent_encode_path("ses_a/b?c%d e"),
            "ses_a%2Fb%3Fc%25d%20e"
        );
        assert_eq!(percent_encode_path("ses_abc-1.2~"), "ses_abc-1.2~");
        assert_eq!(percent_encode_path("é"), "%C3%A9");
    }

    #[test]
    fn session_metadata_can_prove_an_assistant_message() {
        let value = json!({
            "sessions": [{
                "id": "ses_1",
                "status": {"type": "idle"},
                "metadata": {"messages": [{"role": "assistant", "text": "done"}]}
            }]
        });
        assert!(parse_session_list(&value)[0].has_assistant_message);

        let value = json!({
            "sessions": [{
                "id": "ses_2",
                "status": {"type": "idle"},
                "metadata": {"messages": [{"role": "user", "text": "go"}]}
            }]
        });
        assert!(!parse_session_list(&value)[0].has_assistant_message);
    }
}
