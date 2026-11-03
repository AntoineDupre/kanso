use std::io::Read as _;

use serde_json::{json, Map, Value};

use super::client::{ApiResponse, Client, Method};
use super::config::{self, ConfigFile};
use super::output;
use super::{
    CliError, ConfigCommand, MoveTarget, ProjectCommand, SessionCommand, TaskCommand, TokenCommand,
};
use crate::skill;

macro_rules! outln {
    () => {
        output::line(format_args!(""))
    };
    ($($arg:tt)*) => {
        output::line(format_args!($($arg)*))
    };
}

macro_rules! out {
    () => {
        output::text(format_args!(""))
    };
    ($($arg:tt)*) => {
        output::text(format_args!($($arg)*))
    };
}

/// Print a JSON response unchanged for `--json`, or hand it to the caller for
/// plain-text rendering.
fn finish(response: ApiResponse, json_output: bool) -> Result<Option<Value>, CliError> {
    let body = response.success()?;
    if json_output {
        output::print_json(&body);
        return Ok(None);
    }
    if body.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&body)
        .map(Some)
        .map_err(|error| CliError::Io(format!("invalid JSON from server: {error}")))
}

fn fetch(client: &Client, path: &str) -> Result<Value, CliError> {
    let body = client.get(path)?.success()?;
    serde_json::from_str(&body)
        .map_err(|error| CliError::Io(format!("invalid JSON from server: {error}")))
}

fn get_task(client: &Client, reference: &str) -> Result<Value, CliError> {
    fetch(client, &format!("/api/tasks/{reference}"))
}

fn string(value: &Value, key: &str) -> String {
    output::string(value, key)
}

fn optional(value: Option<&str>) -> String {
    output::optional(value)
}

fn print_task(value: &Value) {
    outln!("{}  {}", string(value, "reference"), string(value, "title"));
    outln!(
        "status: {}  priority: {}  due: {}  claimed: {}",
        string(value, "status"),
        string(value, "priority"),
        string(value, "due_date"),
        string(value, "claimed_by")
    );
    if let Some(reason) = value.get("blocked_reason").and_then(Value::as_str) {
        outln!("blocked: {}", output::sanitize(reason));
    }
    if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
        let mut session = format!("session: {}", output::sanitize(session_id));
        if let Some(directory) = value.get("session_directory").and_then(Value::as_str) {
            session.push_str("  ");
            session.push_str(&output::sanitize(directory));
        }
        if let Some(status) = value.get("session_status").and_then(Value::as_str) {
            session.push_str(&format!("  ({})", output::sanitize(status)));
        }
        outln!("{session}");
    }
    outln!("labels: {}", output::labels(&value["labels"]));
    if let Some(description) = value.get("description").and_then(Value::as_str) {
        if !description.trim().is_empty() {
            outln!();
            outln!("{}", output::sanitize(description));
        }
    }

    if let Some(checklist) = value.get("checklist").and_then(Value::as_array) {
        if !checklist.is_empty() {
            let done = checklist
                .iter()
                .filter(|item| item["done"].as_bool().unwrap_or(false))
                .count();
            outln!();
            outln!("checklist ({done}/{}):", checklist.len());
            for (index, item) in checklist.iter().enumerate() {
                let mark = if item["done"].as_bool().unwrap_or(false) {
                    "x"
                } else {
                    " "
                };
                outln!("  [{mark}] {}. {}", index + 1, string(item, "text"));
            }
        }
    }

    if let Some(dependencies) = value.get("dependencies") {
        let blocked_by = references(&dependencies["blocked_by"]);
        let blocks = references(&dependencies["blocks"]);
        if !blocked_by.is_empty() || !blocks.is_empty() {
            outln!();
            outln!("blocked by: {}", join_or_dash(&blocked_by));
            outln!("blocks: {}", join_or_dash(&blocks));
        }
    }

    if let Some(comments) = value.get("comments").and_then(Value::as_array) {
        if !comments.is_empty() {
            outln!();
            outln!("comments:");
            for comment in comments {
                outln!(
                    "  {} {}: {}",
                    string(comment, "author"),
                    output::timestamp(comment, "created_at"),
                    string(comment, "body")
                );
            }
        }
    }

    if let Some(activity) = value.get("activity").and_then(Value::as_array) {
        if !activity.is_empty() {
            outln!();
            outln!("activity:");
            for entry in activity {
                outln!(
                    "  {} {} {}",
                    output::timestamp(entry, "created_at"),
                    string(entry, "actor"),
                    string(entry, "kind")
                );
            }
        }
    }
}

fn references(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().map(|item| string(item, "reference")).collect())
        .unwrap_or_default()
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        values.join(", ")
    }
}

pub fn config(command: ConfigCommand, _json_output: bool) -> Result<(), CliError> {
    match command {
        ConfigCommand::Init { url, token, force } => {
            let client = Client::new(&url, &token)?;
            let me = fetch(&client, "/api/me")?;
            let path = config::path()?;
            config::write(&path, &ConfigFile::new(&url, &token), force)?;
            outln!(
                "authenticated as {} ({})",
                string(&me, "name"),
                string(&me, "scope")
            );
            outln!("wrote {} (0600)", path.display());
            Ok(())
        }
    }
}

pub fn project(command: ProjectCommand, json_output: bool) -> Result<(), CliError> {
    let client = Client::from_config()?;
    match command {
        ProjectCommand::List => {
            let response = client.get("/api/projects")?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            let rows: Vec<Vec<String>> = value
                .as_array()
                .map(|projects| {
                    projects
                        .iter()
                        .map(|project| {
                            vec![
                                string(project, "key"),
                                string(project, "name"),
                                optional(project["repo_path"].as_str()),
                                output::timestamp(project, "archived_at"),
                            ]
                        })
                        .collect()
                })
                .unwrap_or_default();
            out!(
                "{}",
                output::table(&["KEY", "NAME", "REPO", "ARCHIVED"], &rows)
            );
            Ok(())
        }
        ProjectCommand::Add {
            key,
            name,
            description,
            repo_path,
            color,
        } => {
            let mut body = Map::new();
            body.insert("key".to_owned(), json!(key));
            body.insert("name".to_owned(), json!(name));
            if let Some(description) = description {
                body.insert("description".to_owned(), json!(description));
            }
            if let Some(repo_path) = repo_path {
                body.insert("repo_path".to_owned(), json!(repo_path));
            }
            if let Some(color) = color {
                body.insert("color".to_owned(), json!(color));
            }
            let response = client.request(
                Method::Post,
                "/api/projects",
                &[],
                Some(&Value::Object(body)),
                None,
            )?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            outln!("created project {}", string(&value, "key"));
            Ok(())
        }
        ProjectCommand::Show { reference } => {
            let response = client.get(&format!("/api/projects/{reference}"))?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            outln!("{}  {}", string(&value, "key"), string(&value, "name"));
            outln!("id: {}", string(&value, "id"));
            outln!("repo: {}", optional(value["repo_path"].as_str()));
            outln!("color: {}", string(&value, "color"));
            outln!("archived: {}", output::timestamp(&value, "archived_at"));
            Ok(())
        }
        ProjectCommand::Edit {
            reference,
            key,
            name,
            description,
            repo_path,
            color,
        } => {
            let mut body = Map::new();
            if let Some(key) = key {
                body.insert("key".to_owned(), json!(key));
            }
            if let Some(name) = name {
                body.insert("name".to_owned(), json!(name));
            }
            if let Some(description) = description {
                body.insert("description".to_owned(), json!(description));
            }
            if let Some(repo_path) = repo_path {
                body.insert("repo_path".to_owned(), json!(repo_path));
            }
            if let Some(color) = color {
                body.insert("color".to_owned(), json!(color));
            }
            if body.is_empty() {
                return Err(CliError::usage("no editable fields provided"));
            }
            let response = client.request(
                Method::Patch,
                &format!("/api/projects/{reference}"),
                &[],
                Some(&Value::Object(body)),
                None,
            )?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            outln!("updated project {}", string(&value, "key"));
            Ok(())
        }
    }
}

pub fn task(command: TaskCommand, json_output: bool) -> Result<(), CliError> {
    let client = Client::from_config()?;
    match command {
        TaskCommand::List {
            project,
            status,
            label,
            mine,
        } => {
            let mut query: Vec<(&str, String)> = Vec::new();
            if let Some(project) = project {
                query.push(("project", project));
            }
            if let Some(status) = status {
                query.push(("status", status));
            }
            if let Some(label) = label {
                query.push(("label", label));
            }
            if mine {
                let me = fetch(&client, "/api/me")?;
                query.push(("claimed_by", string(&me, "name")));
            }
            let response = client.request(Method::Get, "/api/tasks", &query, None, None)?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            let rows: Vec<Vec<String>> = value
                .as_array()
                .map(|tasks| {
                    tasks
                        .iter()
                        .map(|task| {
                            vec![
                                string(task, "reference"),
                                string(task, "status"),
                                string(task, "priority"),
                                optional(task["claimed_by"].as_str()),
                                optional(task["due_date"].as_str()),
                                output::labels(&task["labels"]),
                                string(task, "title"),
                            ]
                        })
                        .collect()
                })
                .unwrap_or_default();
            out!(
                "{}",
                output::table(
                    &["REF", "STATUS", "PRIORITY", "CLAIMED", "DUE", "LABELS", "TITLE"],
                    &rows
                )
            );
            Ok(())
        }
        TaskCommand::Show { reference } => {
            let response = client.get(&format!("/api/tasks/{reference}"))?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            print_task(&value);
            Ok(())
        }
        TaskCommand::Add {
            title,
            project,
            description,
            status,
            priority,
        } => {
            let mut body = Map::new();
            body.insert("project_key".to_owned(), json!(project));
            body.insert("title".to_owned(), json!(title));
            if let Some(description) = description {
                body.insert("description".to_owned(), json!(description));
            }
            if let Some(status) = status {
                body.insert("status".to_owned(), json!(status));
            }
            if let Some(priority) = priority {
                body.insert("priority".to_owned(), json!(priority));
            }
            let response = client.request(
                Method::Post,
                "/api/tasks",
                &[],
                Some(&Value::Object(body)),
                None,
            )?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            outln!(
                "created {}  {}",
                string(&value, "reference"),
                string(&value, "title")
            );
            Ok(())
        }
        TaskCommand::Edit {
            reference,
            title,
            description,
            due,
        } => {
            let mut body = Map::new();
            if let Some(title) = title {
                body.insert("title".to_owned(), json!(title));
            }
            if let Some(description) = description {
                body.insert("description".to_owned(), json!(description));
            }
            if let Some(due) = due {
                body.insert("due_date".to_owned(), json!(due));
            }
            if body.is_empty() {
                return Err(CliError::usage("no editable fields provided"));
            }
            let response = client.request(
                Method::Patch,
                &format!("/api/tasks/{reference}"),
                &[],
                Some(&Value::Object(body)),
                None,
            )?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            outln!("updated {}", string(&value, "reference"));
            Ok(())
        }
    }
}

pub fn next(
    project: Option<String>,
    claim: bool,
    agent: Option<String>,
    json_output: bool,
) -> Result<(), CliError> {
    let client = Client::from_config()?;
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(project) = project {
        query.push(("project", project));
    }
    if claim {
        query.push(("claim", "1".to_owned()));
    }
    let response = client.request(
        Method::Get,
        "/api/agent/next",
        &query,
        None,
        agent.as_deref(),
    )?;
    match finish(response, json_output)? {
        Some(value) => print_task(&value),
        None if !json_output => outln!("no cards ready"),
        None => {}
    }
    Ok(())
}

pub fn claim(reference: &str, agent: Option<String>, json_output: bool) -> Result<(), CliError> {
    let client = Client::from_config()?;
    let response = client.request(
        Method::Post,
        &format!("/api/tasks/{reference}/claim"),
        &[],
        None,
        agent.as_deref(),
    )?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    outln!(
        "claimed {} ({})",
        string(&value, "reference"),
        string(&value, "status")
    );
    Ok(())
}

pub fn release(reference: &str, json_output: bool) -> Result<(), CliError> {
    let client = Client::from_config()?;
    let response = client.request(
        Method::Post,
        &format!("/api/tasks/{reference}/release"),
        &[],
        None,
        None,
    )?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    outln!(
        "released {} ({})",
        string(&value, "reference"),
        string(&value, "status")
    );
    Ok(())
}

pub fn move_(
    reference: String,
    target: MoveTarget,
    reason: Option<String>,
    agent: Option<String>,
    json_output: bool,
) -> Result<(), CliError> {
    if reason.is_some() && !matches!(target, MoveTarget::Blocker) {
        return Err(CliError::usage(
            "--reason is only valid when moving to blocker",
        ));
    }
    let client = Client::from_config()?;
    let mut body = Map::new();
    match target.status() {
        Some(status) => {
            body.insert("status".to_owned(), json!(status));
        }
        None => {
            body.insert("restore".to_owned(), json!(true));
        }
    }
    if let Some(reason) = reason {
        body.insert("reason".to_owned(), json!(reason));
    }
    let response = client.request(
        Method::Post,
        &format!("/api/tasks/{reference}/move"),
        &[],
        Some(&Value::Object(body)),
        agent.as_deref(),
    )?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    outln!(
        "moved {} to {}",
        string(&value, "reference"),
        string(&value, "status")
    );
    if let Some(reason) = value.get("blocked_reason").and_then(Value::as_str) {
        outln!("reason: {}", output::sanitize(reason));
    }
    Ok(())
}

pub fn comment(
    reference: String,
    text: String,
    agent: Option<String>,
    json_output: bool,
) -> Result<(), CliError> {
    let body = if text == "-" {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|error| CliError::Io(format!("failed to read stdin: {error}")))?;
        buffer.trim_end_matches(['\r', '\n']).to_owned()
    } else {
        text
    };
    if body.trim().is_empty() {
        return Err(CliError::usage("comment text must not be empty"));
    }
    let client = Client::from_config()?;
    let response = client.request(
        Method::Post,
        &format!("/api/tasks/{reference}/comment"),
        &[],
        Some(&json!({ "body": body })),
        agent.as_deref(),
    )?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    outln!("commented on {reference} as {}", string(&value, "author"));
    Ok(())
}

pub fn check(
    reference: String,
    add: Option<String>,
    done: Option<usize>,
    list: bool,
    agent: Option<String>,
    json_output: bool,
) -> Result<(), CliError> {
    let selected = usize::from(add.is_some()) + usize::from(done.is_some()) + usize::from(list);
    if selected != 1 {
        return Err(CliError::usage(
            "choose exactly one of --add, --done or --list",
        ));
    }
    if done == Some(0) {
        return Err(CliError::usage("--done expects a 1-based item number"));
    }
    let client = Client::from_config()?;

    if let Some(text) = add {
        let response = client.request(
            Method::Post,
            &format!("/api/tasks/{reference}/checklist"),
            &[],
            Some(&json!({ "text": text })),
            agent.as_deref(),
        )?;
        let Some(value) = finish(response, json_output)? else {
            return Ok(());
        };
        outln!("added checklist item: {}", string(&value, "text"));
        return Ok(());
    }

    if list {
        let response = client.get(&format!("/api/tasks/{reference}"))?;
        let Some(value) = finish(response, json_output)? else {
            return Ok(());
        };
        let rows: Vec<Vec<String>> = value["checklist"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        vec![
                            (index + 1).to_string(),
                            if item["done"].as_bool().unwrap_or(false) {
                                "x".to_owned()
                            } else {
                                " ".to_owned()
                            },
                            string(item, "text"),
                        ]
                    })
                    .collect()
            })
            .unwrap_or_default();
        out!("{}", output::table(&["#", "DONE", "TEXT"], &rows));
        return Ok(());
    }

    let index = done.expect("validated above");
    let task = get_task(&client, &reference)?;
    let item = task["checklist"]
        .as_array()
        .and_then(|items| items.get(index.saturating_sub(1)))
        .ok_or_else(|| CliError::Api {
            status: 404,
            code: "not_found".to_owned(),
            message: format!("checklist item {index} does not exist on {reference}"),
        })?;
    let item_id = string(item, "id");
    let response = client.request(
        Method::Patch,
        &format!("/api/tasks/{reference}/checklist/{item_id}"),
        &[],
        Some(&json!({ "done": true })),
        agent.as_deref(),
    )?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    outln!("checked item {}: {}", index, string(&value, "text"));
    Ok(())
}

pub fn depend(
    reference: String,
    on: String,
    dependency: String,
    json_output: bool,
) -> Result<(), CliError> {
    if !on.eq_ignore_ascii_case("on") {
        return Err(CliError::usage("expected `kanso depend REF on REF`"));
    }
    let client = Client::from_config()?;
    let task = get_task(&client, &reference)?;
    let mut dependencies = references(&task["dependencies"]["blocked_by"]);
    if !dependencies.iter().any(|item| item == &dependency) {
        dependencies.push(dependency.clone());
    }
    let response = client.request(
        Method::Post,
        &format!("/api/tasks/{reference}/dependencies"),
        &[],
        Some(&json!({ "blocked_by": dependencies })),
        None,
    )?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    outln!(
        "{} is blocked by {}",
        string(&value, "reference"),
        join_or_dash(&references(&value["dependencies"]["blocked_by"]))
    );
    Ok(())
}

pub fn propose(
    title: String,
    project: String,
    description: Option<String>,
    agent: Option<String>,
    json_output: bool,
) -> Result<(), CliError> {
    let client = Client::from_config()?;
    let mut body = Map::new();
    body.insert("project_key".to_owned(), json!(project));
    body.insert("title".to_owned(), json!(title));
    if let Some(description) = description {
        body.insert("description".to_owned(), json!(description));
    }
    let response = client.request(
        Method::Post,
        "/api/tasks",
        &[],
        Some(&Value::Object(body)),
        agent.as_deref(),
    )?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    let verb = if value["state"] == "proposed" {
        "proposed"
    } else {
        "created"
    };
    outln!(
        "{verb} {}  {}",
        string(&value, "reference"),
        string(&value, "title")
    );
    Ok(())
}

pub fn inbox(json_output: bool) -> Result<(), CliError> {
    let client = Client::from_config()?;
    let response = client.get("/api/inbox")?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    let rows: Vec<Vec<String>> = value
        .as_array()
        .map(|tasks| {
            tasks
                .iter()
                .map(|task| {
                    vec![
                        string(task, "reference"),
                        string(task, "project_key"),
                        string(task, "priority"),
                        string(task, "title"),
                    ]
                })
                .collect()
        })
        .unwrap_or_default();
    out!(
        "{}",
        output::table(&["REF", "PROJECT", "PRIORITY", "TITLE"], &rows)
    );
    Ok(())
}

pub fn session(command: SessionCommand, json_output: bool) -> Result<(), CliError> {
    let SessionCommand::Link {
        reference,
        session_id,
        directory,
        status,
    } = command;
    let client = Client::from_config()?;
    let mut body = Map::new();
    body.insert("session_id".to_owned(), json!(session_id));
    body.insert("directory".to_owned(), json!(directory));
    if let Some(status) = status {
        body.insert("status".to_owned(), json!(status));
    }
    let response = client.request(
        Method::Post,
        &format!("/api/tasks/{reference}/session"),
        &[],
        Some(&Value::Object(body)),
        None,
    )?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    outln!(
        "linked {} to session {} ({})",
        string(&value, "reference"),
        string(&value, "session_id"),
        string(&value, "session_status")
    );
    Ok(())
}

pub fn token(command: TokenCommand, json_output: bool) -> Result<(), CliError> {
    let client = Client::from_config()?;
    match command {
        TokenCommand::Create { name, scope } => {
            let response = client.request(
                Method::Post,
                "/api/tokens",
                &[],
                Some(&json!({ "name": name, "scope": scope })),
                None,
            )?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            outln!(
                "created token \"{}\" ({})",
                string(&value, "name"),
                string(&value, "scope")
            );
            outln!("{}", string(&value, "token"));
            Ok(())
        }
        TokenCommand::List => {
            let response = client.get("/api/tokens")?;
            let Some(value) = finish(response, json_output)? else {
                return Ok(());
            };
            let rows: Vec<Vec<String>> = value
                .as_array()
                .map(|tokens| {
                    tokens
                        .iter()
                        .map(|token| {
                            vec![
                                string(token, "name"),
                                string(token, "scope"),
                                output::timestamp(token, "created_at"),
                                output::timestamp(token, "last_used_at"),
                                output::timestamp(token, "revoked_at"),
                            ]
                        })
                        .collect()
                })
                .unwrap_or_default();
            out!(
                "{}",
                output::table(&["NAME", "SCOPE", "CREATED", "LAST USED", "REVOKED"], &rows)
            );
            Ok(())
        }
        TokenCommand::Revoke { name } => {
            let tokens = fetch(&client, "/api/tokens")?;
            let id = tokens
                .as_array()
                .and_then(|tokens| {
                    tokens
                        .iter()
                        .find(|token| {
                            token["name"]
                                .as_str()
                                .is_some_and(|value| value.eq_ignore_ascii_case(&name))
                        })
                        .and_then(|token| token["id"].as_str())
                })
                .ok_or_else(|| CliError::Api {
                    status: 404,
                    code: "not_found".to_owned(),
                    message: format!("token {name:?} not found"),
                })?
                .to_owned();
            let response = client.request(
                Method::Delete,
                &format!("/api/tokens/{id}"),
                &[],
                None,
                None,
            )?;
            finish(response, json_output)?;
            if !json_output {
                outln!("revoked token \"{name}\"");
            }
            Ok(())
        }
    }
}

pub fn skill(
    print: bool,
    install: bool,
    dir: Option<std::path::PathBuf>,
    _json_output: bool,
) -> Result<(), CliError> {
    if dir.is_some() && !install {
        return Err(CliError::usage("--dir requires --install"));
    }
    if install {
        let base = match dir {
            Some(dir) => dir,
            None => skill::default_dir()?,
        };
        let (path, outcome) = skill::install(&base)?;
        outln!("{}: {}", outcome.as_str(), path.display());
        return Ok(());
    }
    let _ = print;
    out!("{}", skill::SKILL);
    Ok(())
}

pub fn backup(out: Option<String>, json_output: bool) -> Result<(), CliError> {
    let client = Client::from_config()?;
    let body = match out {
        Some(dir) => json!({ "dir": dir }),
        None => json!({}),
    };
    let response =
        client.post_with_timeout("/api/backup", &body, std::time::Duration::from_secs(600))?;
    let Some(value) = finish(response, json_output)? else {
        return Ok(());
    };
    outln!("backup: {}", string(&value, "path"));
    outln!("size: {} bytes", value["size"].as_u64().unwrap_or_default());
    outln!("created: {}", output::timestamp(&value, "created_at"));
    Ok(())
}
