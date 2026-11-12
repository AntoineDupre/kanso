//! Manual, read-only GitLab imports. Credentials never enter provider metadata.
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

use super::{Candidate, Filter, Provider, ProviderError, ProviderInfo, Source};
use serde::{de::DeserializeOwned, Deserialize};

pub struct GitLab;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
const MAX_PAGES: usize = 3;
const MAX_CREDENTIALS: u64 = 1024 * 1024;

/// Only HTTPS origins are supported. Canonicalize aliases before identity lookup.
pub(crate) fn normalize_instance(value: &str) -> Option<String> {
    if value.len() > 255
        || value.bytes().any(|b| b.is_ascii_whitespace())
        || value.contains(['@', '#', '%', '\\', '|'])
    {
        return None;
    }
    let uri: ureq::http::Uri = value.parse().ok()?;
    if uri.scheme_str()? != "https" || uri.query().is_some() || !matches!(uri.path(), "" | "/") {
        return None;
    }
    let authority = uri.authority()?;
    let host = authority.host().to_ascii_lowercase();
    if host.is_empty()
        || host.ends_with('.')
        || host
            .bytes()
            .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'.' | b'-' | b'[' | b']' | b':'))
    {
        return None;
    }
    let port = authority.port_u16();
    // Reject unparseable and zero ports, rather than accepting an authority alias.
    let expected = match port {
        Some(0) => return None,
        Some(p) => format!("{host}:{p}"),
        None => host.clone(),
    };
    if !authority.as_str().eq_ignore_ascii_case(&expected) {
        return None;
    }
    Some(match port {
        Some(p) if p != 443 => format!("https://{host}:{p}"),
        _ => format!("https://{host}"),
    })
}

pub(crate) fn valid_repository(value: &str) -> bool {
    value.len() <= 512
        && value.split('/').count() >= 2
        && value.split('/').all(|part| {
            !part.is_empty()
                && part.len() <= 255
                && !matches!(part, "." | "..")
                && !part.starts_with(['-', '.'])
                && !part.ends_with('.')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        })
}

fn username(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with(['-', '.'])
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn encode(value: &str) -> String {
    let mut result = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            result.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(result, "%{b:02X}");
        }
    }
    result
}

fn token_from_file(path: &Path, instance: &str) -> Result<String, ProviderError> {
    let file = std::fs::File::open(path).map_err(|_| ProviderError)?;
    if !file.metadata().map_err(|_| ProviderError)?.is_file() {
        return Err(ProviderError);
    }
    let mut bytes = Vec::new();
    file.take(MAX_CREDENTIALS + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ProviderError)?;
    if bytes.len() as u64 > MAX_CREDENTIALS {
        return Err(ProviderError);
    }
    let tokens: HashMap<String, String> =
        serde_json::from_slice(&bytes).map_err(|_| ProviderError)?;
    if tokens.len() > 100
        || tokens
            .keys()
            .any(|k| normalize_instance(k).as_ref() != Some(k))
    {
        return Err(ProviderError);
    }
    tokens
        .get(instance)
        .filter(|t| !t.is_empty() && t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_graphic()))
        .cloned()
        .ok_or(ProviderError)
}

struct Response {
    body: Vec<u8>,
    next: Option<String>,
}

fn request(
    instance: &str,
    token: &str,
    endpoint: &str,
    deadline: Instant,
) -> Result<Response, ProviderError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(ProviderError)?;
    request_with_agent(&http_agent(remaining, true), instance, token, endpoint)
}

fn http_agent(timeout: Duration, https_only: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(https_only)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_per_read(Some(Duration::from_secs(5)))
        .build()
        .into()
}

fn contains_secret(value: &serde_json::Value, token: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value.contains(token),
        serde_json::Value::Array(values) => values.iter().any(|v| contains_secret(v, token)),
        serde_json::Value::Object(values) => values
            .iter()
            .any(|(k, v)| k.contains(token) || contains_secret(v, token)),
        _ => false,
    }
}

fn request_with_agent(
    agent: &ureq::Agent,
    instance: &str,
    token: &str,
    endpoint: &str,
) -> Result<Response, ProviderError> {
    let mut credential = ureq::http::HeaderValue::from_str(token).map_err(|_| ProviderError)?;
    credential.set_sensitive(true);
    let mut response = agent
        .get(format!("{instance}{endpoint}"))
        .header("PRIVATE-TOKEN", credential)
        .header("Accept", "application/json")
        .call()
        .map_err(|_| ProviderError)?;
    if !response.status().is_success() {
        return Err(ProviderError);
    }
    let next = response
        .headers()
        .get("x-next-page")
        .map(|h| h.to_str().map(str::to_owned))
        .transpose()
        .map_err(|_| ProviderError)?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_OUTPUT)
        .read_to_vec()
        .map_err(|_| ProviderError)?;
    // Even a malicious/incorrect upstream must not reflect the credential into a card.
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| ProviderError)?;
    if contains_secret(&value, token) {
        return Err(ProviderError);
    }
    Ok(Response { body, next })
}

fn decode<T: DeserializeOwned>(response: Response) -> Result<T, ProviderError> {
    if response.body.len() as u64 > MAX_OUTPUT {
        return Err(ProviderError);
    }
    serde_json::from_slice(&response.body).map_err(|_| ProviderError)
}

#[derive(Deserialize, Clone)]
struct User {
    id: u64,
    username: String,
}
#[derive(Deserialize)]
struct Project {
    id: u64,
    path_with_namespace: String,
}
#[derive(Deserialize)]
struct Item {
    iid: u64,
    project_id: u64,
    title: String,
    description: Option<String>,
    state: String,
    author: User,
    assignees: Option<Vec<User>>,
    draft: Option<bool>,
}

fn candidate(
    instance: &str,
    project: &Project,
    iid: u64,
    title: String,
    body: Option<String>,
    author: &str,
    category: &str,
) -> Result<Candidate, ProviderError> {
    if iid == 0 || !username(author) {
        return Err(ProviderError);
    }
    let (entity, kind) = match category {
        "issues" => ("issues", "Issue"),
        "review" => ("merge_requests", "MR to review"),
        "follow" => ("merge_requests", "MR to follow"),
        _ => return Err(ProviderError),
    };
    let url = format!(
        "{instance}/{}/-/{entity}/{iid}",
        project.path_with_namespace
    );
    let header = format!(
        "GitLab {kind} · {} #{} · @{author}\n\n{url}\n\n",
        project.path_with_namespace, iid
    );
    let body = body.unwrap_or_default();
    let oversized = header.len().saturating_add(body.len()) > 64 * 1024 || title.len() > 4096;
    let invalid_title = crate::api::tasks::validate_title(&title).is_err();
    Ok(Candidate {
        identity: format!(
            "{instance}|project:{}|{}:{iid}",
            project.id,
            if category == "issues" { "issue" } else { "mr" }
        ),
        kind: kind.into(),
        title: if title.len() > 4096 {
            format!("GitLab item #{iid} (title too large)")
        } else {
            title
        },
        description: if oversized {
            "GitLab content exceeds import limits and was not copied.".into()
        } else {
            format!("{header}{body}")
        },
        source_url: url,
        invalid_reason: if oversized {
            Some("GitLab content exceeds import limits; nothing was truncated or imported.".into())
        } else if invalid_title {
            Some("GitLab title is empty or exceeds the 200-character title limit.".into())
        } else {
            None
        },
    })
}

impl Provider for GitLab {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "gitlab".into(),
            name: "GitLab".into(),
            sources: vec![],
            filters: vec![Filter {
                id: "user".into(),
                label: "GitLab username".into(),
            }],
        }
    }
    fn failure_message(&self) -> &'static str {
        "GitLab preview failed. Check server KANSO_GITLAB_TOKENS_FILE, the instance token (read_api), repository/user access, supported GitLab API fields, and query limits."
    }
    fn project_info(
        &self,
        db: &rusqlite::Connection,
        project: &str,
    ) -> crate::error::Result<ProviderInfo> {
        let mut info = self.info();
        let mut statement = db.prepare("SELECT instance,repository,issues,review,follow FROM gitlab_repositories WHERE project_id=?1 ORDER BY instance, repository COLLATE NOCASE")?;
        let rows = statement.query_map([project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?;
        for row in rows {
            let (instance, repository, issues, review, follow) = row?;
            for (category, enabled, label) in [
                ("issues", issues, "Issues"),
                ("review", review, "Merge requests to review"),
                ("follow", follow, "Merge requests to follow"),
            ] {
                if enabled {
                    info.sources.push(Source {
                        id: format!("{instance}|{repository}|{category}"),
                        name: format!("{instance}/{repository} — {label}"),
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
        let instance = source.split('|').next().ok_or(ProviderError)?;
        if normalize_instance(instance).as_deref() != Some(instance) {
            return Err(ProviderError);
        }
        let path = std::env::var_os("KANSO_GITLAB_TOKENS_FILE").ok_or(ProviderError)?;
        let token = token_from_file(Path::new(&path), instance)?;
        fetch(source, filters, |endpoint, deadline| {
            request(instance, &token, endpoint, deadline)
        })
    }
}

fn fetch(
    source: &str,
    filters: &HashMap<String, String>,
    mut request: impl FnMut(&str, Instant) -> Result<Response, ProviderError>,
) -> Result<Vec<Candidate>, ProviderError> {
    let parts: Vec<_> = source.split('|').collect();
    if parts.len() != 3
        || normalize_instance(parts[0]).as_deref() != Some(parts[0])
        || !valid_repository(parts[1])
        || !matches!(parts[2], "issues" | "review" | "follow")
        || filters.keys().any(|k| k != "user")
    {
        return Err(ProviderError);
    }
    let (instance, repository, category) = (parts[0], parts[1], parts[2]);
    let deadline = Instant::now() + TIMEOUT;
    let me: User = decode(request("/api/v4/user", deadline)?)?;
    if me.id == 0 || !username(&me.username) {
        return Err(ProviderError);
    }
    let selected = filters
        .get("user")
        .map(String::as_str)
        .unwrap_or(&me.username);
    if (selected.is_empty() && category != "issues")
        || (!selected.is_empty() && !username(selected))
    {
        return Err(ProviderError);
    }
    let selected_id = if selected.is_empty() {
        None
    } else if selected.eq_ignore_ascii_case(&me.username) {
        Some(me.id)
    } else {
        let users: Vec<User> = decode(request(
            &format!("/api/v4/users?username={}&per_page=100", encode(selected)),
            deadline,
        )?)?;
        if users.len() != 1 || users[0].id == 0 || !users[0].username.eq_ignore_ascii_case(selected)
        {
            return Err(ProviderError);
        }
        Some(users[0].id)
    };
    let project: Project = decode(request(
        &format!("/api/v4/projects/{}", encode(repository)),
        deadline,
    )?)?;
    if project.id == 0 || !valid_repository(&project.path_with_namespace) {
        return Err(ProviderError);
    }
    if category == "review" {
        return review(instance, &project, &me, selected, &mut request, deadline);
    }
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for page in 1..=MAX_PAGES {
        let entity = if category == "issues" {
            "issues"
        } else {
            "merge_requests"
        };
        let filter = match selected_id {
            Some(id) => format!(
                "&{}={id}",
                if category == "issues" {
                    "assignee_id"
                } else {
                    "author_id"
                }
            ),
            None => String::new(),
        };
        let endpoint = format!(
            "/api/v4/projects/{}/{entity}?state=opened&scope=all&per_page=100&page={page}{filter}",
            project.id
        );
        let response = request(&endpoint, deadline)?;
        let next = response.next.clone();
        let items: Vec<Item> = decode(response)?;
        if items.len() > 100 {
            return Err(ProviderError);
        }
        let complete = match next.as_deref() {
            Some("") => true,
            Some(value) if value.parse::<usize>().ok() == Some(page + 1) => false,
            Some(_) => return Err(ProviderError),
            None => items.len() < 100,
        };
        for item in items {
            if item.project_id != project.id
                || item.iid == 0
                || item.author.id == 0
                || !seen.insert(item.iid)
                || !matches!(
                    item.state.as_str(),
                    "opened" | "closed" | "merged" | "locked"
                )
            {
                return Err(ProviderError);
            }
            if item.state != "opened" {
                continue;
            }
            let eligible = if category == "issues" {
                selected_id.is_none()
                    || item
                        .assignees
                        .as_ref()
                        .ok_or(ProviderError)?
                        .iter()
                        .any(|u| Some(u.id) == selected_id)
            } else {
                let _ = item.draft.ok_or(ProviderError)?;
                Some(item.author.id) == selected_id
            };
            if eligible {
                result.push(candidate(
                    instance,
                    &project,
                    item.iid,
                    item.title,
                    item.description,
                    &item.author.username,
                    category,
                )?);
            }
        }
        if result.len() > 200 {
            return Err(ProviderError);
        }
        if complete {
            return Ok(result);
        }
    }
    Err(ProviderError)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    nodes: Vec<T>,
    page_info: PageInfo,
}
#[derive(Deserialize)]
struct GraphUser {
    id: String,
    username: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Interaction {
    review_state: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reviewer {
    username: String,
    merge_request_interaction: Interaction,
}
#[derive(Deserialize)]
struct MergeRequest {
    iid: String,
    title: String,
    description: Option<String>,
    state: String,
    draft: bool,
    author: GraphUser,
    reviewers: Connection<Reviewer>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphProject {
    id: String,
    merge_requests: Connection<MergeRequest>,
}
#[derive(Deserialize)]
struct GraphData {
    project: GraphProject,
}
#[derive(Deserialize)]
struct GraphResponse {
    data: Option<GraphData>,
    errors: Option<Vec<serde_json::Value>>,
}

fn review(
    instance: &str,
    project: &Project,
    me: &User,
    selected: &str,
    request: &mut impl FnMut(&str, Instant) -> Result<Response, ProviderError>,
    deadline: Instant,
) -> Result<Vec<Candidate>, ProviderError> {
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for _ in 0..MAX_PAGES {
        let path =
            serde_json::to_string(&project.path_with_namespace).map_err(|_| ProviderError)?;
        let user = serde_json::to_string(selected).map_err(|_| ProviderError)?;
        let after = serde_json::to_string(&cursor).map_err(|_| ProviderError)?;
        let query = format!("query {{ project(fullPath:{path}) {{ id mergeRequests(state:opened, reviewerUsername:{user}, first:100, after:{after}) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ iid title description state draft author {{ id username }} reviewers(first:100) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ username mergeRequestInteraction {{ reviewState }} }} }} }} }} }} }}");
        let response: GraphResponse = decode(request(
            &format!("/api/graphql?query={}", encode(&query)),
            deadline,
        )?)?;
        if response.errors.is_some_and(|e| !e.is_empty()) {
            return Err(ProviderError);
        }
        let data = response.data.ok_or(ProviderError)?;
        if data.project.id != format!("gid://gitlab/Project/{}", project.id) {
            return Err(ProviderError);
        }
        let connection = data.project.merge_requests;
        if connection.nodes.len() > 100 {
            return Err(ProviderError);
        }
        for item in connection.nodes {
            let iid: u64 = item.iid.parse().map_err(|_| ProviderError)?;
            let author_id = item
                .author
                .id
                .strip_prefix("gid://gitlab/User/")
                .and_then(|id| id.parse::<u64>().ok())
                .filter(|id| *id > 0)
                .ok_or(ProviderError)?;
            if iid == 0
                || !seen.insert(iid)
                || item.reviewers.nodes.len() > 100
                || item.reviewers.page_info.has_next_page
            {
                return Err(ProviderError);
            }
            if !matches!(
                item.state.as_str(),
                "opened" | "closed" | "merged" | "locked"
            ) {
                return Err(ProviderError);
            }
            if item.state != "opened" || item.draft || author_id == me.id {
                continue;
            }
            let selected_reviewer = item
                .reviewers
                .nodes
                .iter()
                .find(|u| u.username.eq_ignore_ascii_case(selected));
            let Some(reviewer) = selected_reviewer else {
                continue;
            };
            let pending = match reviewer.merge_request_interaction.review_state.as_str() {
                "UNREVIEWED" | "REVIEW_STARTED" => true,
                "REVIEWED" | "APPROVED" | "REQUESTED_CHANGES" | "UNAPPROVED" => false,
                _ => return Err(ProviderError),
            };
            if pending {
                result.push(candidate(
                    instance,
                    project,
                    iid,
                    item.title,
                    item.description,
                    &item.author.username,
                    "review",
                )?);
            }
        }
        if result.len() > 200 {
            return Err(ProviderError);
        }
        if !connection.page_info.has_next_page {
            return Ok(result);
        }
        let next = connection.page_info.end_cursor.ok_or(ProviderError)?;
        if next.is_empty() || next.len() > 1024 || !cursors.insert(next.clone()) {
            return Err(ProviderError);
        }
        cursor = Some(next);
    }
    Err(ProviderError)
}

#[cfg(test)]
#[path = "gitlab_tests.rs"]
mod tests;
