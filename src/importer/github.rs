//! Read-only GitHub adapter. All subprocess diagnostics are discarded.
use std::{
    collections::HashMap,
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use rusqlite::params;
use serde::Deserialize;

use super::{Candidate, Filter, Provider, ProviderError, ProviderInfo, Source};

pub struct GitHub;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
const MAX_PAGES: usize = 3;

#[derive(Deserialize)]
struct User {
    login: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct Item {
    node_id: String,
    number: u64,
    title: String,
    body: Option<String>,
    state: String,
    user: Option<User>,
    assignees: Option<Vec<User>>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
    draft: Option<bool>,
    merged_at: Option<String>,
    requested_reviewers: Option<Vec<User>>,
}

fn username(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 39
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

pub(crate) fn valid_repository(value: &str) -> bool {
    let Some((owner, repo)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && !owner.ends_with('-')
        && owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && !repo.is_empty()
        && repo.len() <= 100
        && repo != "."
        && repo != ".."
        && repo
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

/// Read a bounded pipe concurrently so a noisy child cannot deadlock the timeout.
fn gh_program(
    program: &std::ffi::OsStr,
    endpoint: &str,
    deadline: Instant,
) -> Result<Vec<u8>, ProviderError> {
    let mut child = Command::new(program)
        .args([
            "api",
            "--hostname",
            "github.com",
            "--method",
            "GET",
            endpoint,
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| ProviderError)?;
    let stdout = child.stdout.take().ok_or(ProviderError)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let result = stdout
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut data)
            .map(|_| data);
        let _ = sender.send(result);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ProviderError);
            }
        }
    };
    let output = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| ProviderError)?
        .map_err(|_| ProviderError)?;
    if !status.success() || output.len() as u64 > MAX_OUTPUT {
        return Err(ProviderError);
    }
    Ok(output)
}

fn gh(endpoint: &str, deadline: Instant) -> Result<Vec<u8>, ProviderError> {
    gh_program(std::ffi::OsStr::new("gh"), endpoint, deadline)
}

fn decode<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T, ProviderError> {
    serde_json::from_slice(data).map_err(|_| ProviderError)
}

fn candidate(
    item: Item,
    repository: &str,
    category: &str,
    me: &str,
    selected: &str,
) -> Result<Option<Candidate>, ProviderError> {
    if item.node_id.is_empty() || item.number == 0 {
        return Err(ProviderError);
    }
    if item.state != "open" {
        return Ok(None);
    }
    let author = item
        .user
        .as_ref()
        .map(|u| u.login.as_str())
        .unwrap_or("ghost");
    if author.is_empty()
        || author.len() > 100
        || !author
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'[' | b']'))
    {
        return Err(ProviderError);
    }
    let eligible = match category {
        "issues" => {
            item.pull_request.is_none()
                && (selected.is_empty()
                    || item
                        .assignees
                        .as_ref()
                        .ok_or(ProviderError)?
                        .iter()
                        .any(|u| u.kind == "User" && u.login.eq_ignore_ascii_case(selected)))
        }
        "review" => {
            !item.draft.ok_or(ProviderError)?
                && item.merged_at.is_none()
                && !author.eq_ignore_ascii_case(me)
                && item
                    .requested_reviewers
                    .ok_or(ProviderError)?
                    .iter()
                    .any(|u| u.kind == "User" && u.login.eq_ignore_ascii_case(selected))
        }
        "follow" => {
            let _ = item.draft.ok_or(ProviderError)?;
            item.merged_at.is_none() && author.eq_ignore_ascii_case(selected)
        }
        _ => return Err(ProviderError),
    };
    if !eligible {
        return Ok(None);
    }
    let kind = match category {
        "issues" => "Issue",
        "review" => "PR to review",
        _ => "PR to follow",
    };
    let url = format!(
        "https://github.com/{repository}/{}/{number}",
        if category == "issues" {
            "issues"
        } else {
            "pull"
        },
        number = item.number
    );
    let header = format!(
        "GitHub {kind} · {repository} #{} · @{author}\n\n{url}\n\n",
        item.number
    );
    let body = item.body.unwrap_or_default();
    let title_invalid = crate::api::tasks::validate_title(&item.title).is_err();
    let oversized = header.len().saturating_add(body.len()) > 64 * 1024 || item.title.len() > 4096;
    let title = if item.title.len() > 4096 {
        format!("GitHub item #{} (title too large)", item.number)
    } else {
        item.title
    };
    let description = if oversized {
        "GitHub content exceeds import limits and was not copied.".to_owned()
    } else {
        format!("{header}{body}")
    };
    Ok(Some(Candidate {
        identity: item.node_id,
        kind: kind.into(),
        title,
        description,
        source_url: url,
        invalid_reason: if oversized {
            Some("GitHub content exceeds import limits; nothing was truncated or imported.".into())
        } else if title_invalid {
            Some("GitHub title is empty or exceeds the 200-character title limit.".into())
        } else {
            None
        },
    }))
}

impl Provider for GitHub {
    fn failure_message(&self) -> &'static str {
        "GitHub preview failed. Check server gh installation, authentication, repository access, and query limits."
    }
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "github".into(),
            name: "GitHub".into(),
            sources: vec![],
            filters: vec![Filter {
                id: "user".into(),
                label: "GitHub username".into(),
            }],
        }
    }

    fn project_info(
        &self,
        db: &rusqlite::Connection,
        project: &str,
    ) -> crate::error::Result<ProviderInfo> {
        let mut info = self.info();
        let mut statement = db.prepare("SELECT repository, issues, review, follow FROM github_repositories WHERE project_id=?1 ORDER BY repository COLLATE NOCASE")?;
        let rows = statement.query_map(params![project], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        for row in rows {
            let (repository, issues, review, follow) = row?;
            for (category, enabled, label) in [
                ("issues", issues, "Issues"),
                ("review", review, "PRs to review"),
                ("follow", follow, "PRs to follow"),
            ] {
                if enabled {
                    info.sources.push(Source {
                        id: format!("{repository}|{category}"),
                        name: format!("{repository} — {label}"),
                    });
                }
            }
        }
        Ok(info)
    }

    fn preview(
        &self,
        source: &str,
        filters: &HashMap<String, String>,
    ) -> Result<Vec<Candidate>, ProviderError> {
        fetch(source, filters, gh)
    }
}

fn fetch(
    source: &str,
    filters: &HashMap<String, String>,
    mut request: impl FnMut(&str, Instant) -> Result<Vec<u8>, ProviderError>,
) -> Result<Vec<Candidate>, ProviderError> {
    let (repository, category) = source.split_once('|').ok_or(ProviderError)?;
    if !valid_repository(repository)
        || !matches!(category, "issues" | "review" | "follow")
        || filters.keys().any(|k| k != "user")
    {
        return Err(ProviderError);
    }
    let deadline = Instant::now() + TIMEOUT;
    let me: User = decode(&request("user", deadline)?)?;
    if me.kind != "User" || !username(&me.login) {
        return Err(ProviderError);
    }
    let selected = filters.get("user").map(String::as_str).unwrap_or(&me.login);
    if (selected.is_empty() && category != "issues")
        || (!selected.is_empty() && !username(selected))
    {
        return Err(ProviderError);
    }
    let mut result = Vec::new();
    for page in 1..=MAX_PAGES {
        let endpoint = format!(
            "repos/{repository}/{}?state=open&per_page=100&page={page}",
            if category == "issues" {
                "issues"
            } else {
                "pulls"
            }
        );
        let items: Vec<Item> = decode(&request(&endpoint, deadline)?)?;
        if items.len() > 100 {
            return Err(ProviderError);
        }
        let done = items.len() < 100;
        for item in items {
            if let Some(item) = candidate(item, repository, category, &me.login, selected)? {
                result.push(item);
            }
        }
        if result.len() > 200 {
            return Err(ProviderError);
        }
        if done {
            return Ok(result);
        }
    }
    Err(ProviderError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn user(name: &str) -> Value {
        json!({"login":name,"type":"User"})
    }
    fn item(id: &str, author: &str) -> Value {
        json!({"node_id":id,"number":42,"title":"<img onerror=alert(1)>","body":"**body**", "state":"open", "user":user(author), "assignees":[user("me")], "draft":false, "merged_at":null, "requested_reviewers":[user("me")]})
    }
    fn preview(
        category: &str,
        filters: HashMap<String, String>,
        items: Vec<Value>,
    ) -> Result<Vec<Candidate>, ProviderError> {
        fetch(&format!("octo/repo|{category}"), &filters, |endpoint, _| {
            Ok(if endpoint == "user" {
                serde_json::to_vec(&user("me")).unwrap()
            } else {
                serde_json::to_vec(&items).unwrap()
            })
        })
    }
    #[test]
    fn issue_defaults_empty_and_pr_exclusion() {
        let mut other = item("other", "someone");
        other["assignees"] = json!([user("other")]);
        let mut pr = item("pr", "someone");
        pr["pull_request"] = json!({"url":"https://example.com"});
        let items = vec![item("one", "someone"), other, pr];
        assert_eq!(
            preview("issues", HashMap::new(), items.clone())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            preview(
                "issues",
                HashMap::from([("user".into(), "".into())]),
                items.clone()
            )
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            preview(
                "issues",
                HashMap::from([("user".into(), "other".into())]),
                items
            )
            .unwrap()[0]
                .identity,
            "other"
        );
    }
    #[test]
    fn review_direct_current_non_draft_and_no_own_author() {
        let mut draft = item("draft", "other");
        draft["draft"] = json!(true);
        let mut team = item("team", "other");
        team["requested_reviewers"] = json!([]);
        team["requested_teams"] = json!([{"name":"team"}]);
        let mut closed = item("closed", "other");
        closed["state"] = json!("closed");
        let mut merged = item("merged", "other");
        merged["merged_at"] = json!("2026-01-01");
        let mut someone = item("someone", "other");
        someone["requested_reviewers"] = json!([user("alice")]);
        let items = vec![
            item("yes", "other"),
            item("own", "me"),
            draft,
            team,
            closed,
            merged,
            someone,
        ];
        let result = preview("review", HashMap::new(), items.clone()).unwrap();
        assert_eq!(
            result
                .iter()
                .map(|i| i.identity.as_str())
                .collect::<Vec<_>>(),
            ["yes"]
        );
        assert_eq!(
            preview(
                "review",
                HashMap::from([("user".into(), "alice".into())]),
                items
            )
            .unwrap()[0]
                .identity,
            "someone"
        );
    }
    #[test]
    fn follow_includes_drafts_and_stable_identity_crosses_categories() {
        let mut own = item("node-123", "me");
        own["draft"] = json!(true);
        let result = preview("follow", HashMap::new(), vec![own.clone()]).unwrap();
        assert_eq!(result[0].identity, "node-123");
        assert!(result[0].description.contains("octo/repo #42"));
        let mut other = item("node-123", "alice");
        other["requested_reviewers"] = json!([user("me")]);
        assert_eq!(
            preview("review", HashMap::new(), vec![other]).unwrap()[0].identity,
            result[0].identity
        );
        assert_eq!(
            preview(
                "follow",
                HashMap::from([("user".into(), "alice".into())]),
                vec![item("alice", "alice"), own]
            )
            .unwrap()
            .len(),
            1
        );
    }
    #[test]
    fn failures_and_page_limits_are_not_empty_successes() {
        assert!(fetch("octo/repo|issues", &HashMap::new(), |_, _| Err(
            ProviderError
        ))
        .is_err());
        assert!(fetch("octo/repo|issues", &HashMap::new(), |_, _| Ok(
            b"not json".to_vec()
        ))
        .is_err());
        let items = vec![item("same", "other"); 100];
        assert!(fetch("octo/repo|issues", &HashMap::new(), |endpoint, _| Ok(
            if endpoint == "user" {
                serde_json::to_vec(&user("me")).unwrap()
            } else {
                serde_json::to_vec(&items).unwrap()
            }
        ))
        .is_err());
        assert!(preview(
            "follow",
            HashMap::from([("user".into(), "".into())]),
            vec![]
        )
        .is_err());
        assert!(preview(
            "issues",
            HashMap::from([("user".into(), "--bad".into())]),
            vec![]
        )
        .is_err());
    }
    #[test]
    fn oversized_items_are_rejected_individually_and_missing_assignees_fail_closed() {
        let mut long_body = item("body", "other");
        long_body["body"] = json!("x".repeat(70_000));
        let mut long_title = item("title", "other");
        long_title["title"] = json!("x".repeat(201));
        let result = preview(
            "issues",
            HashMap::new(),
            vec![long_body, long_title, item("good", "other")],
        )
        .unwrap();
        assert!(result[0]
            .invalid_reason
            .as_ref()
            .unwrap()
            .contains("exceeds"));
        assert!(result[1]
            .invalid_reason
            .as_ref()
            .unwrap()
            .contains("200-character"));
        assert!(result[2].invalid_reason.is_none());
        let mut missing = item("missing", "other");
        missing.as_object_mut().unwrap().remove("assignees");
        assert!(preview("issues", HashMap::new(), vec![missing]).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn subprocess_failures_and_timeout_are_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::TempDir::new();
        let program = dir.path().join("fake-gh");
        assert!(gh_program(
            program.as_os_str(),
            "user",
            Instant::now() + Duration::from_secs(1)
        )
        .is_err());
        std::fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(gh_program(
            program.as_os_str(),
            "user",
            Instant::now() + Duration::from_secs(1)
        )
        .is_err());
        std::fs::write(&program, "#!/bin/sh\nexec sleep 5\n").unwrap();
        let start = Instant::now();
        assert!(gh_program(
            program.as_os_str(),
            "user",
            start + Duration::from_millis(50)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
