use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use axum::{extract::State, Json};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{now_millis, require_human, tasks, ApiError, ApiJson, Auth};
use crate::importer::{Candidate, Preview, ProviderInfo};
use crate::{ids, rank, server::AppState};

const TTL: Duration = Duration::from_secs(600);
const MAX_ITEMS: usize = 200;
const MAX_PREVIEWS: usize = 50;
const MAX_PREVIEW_BYTES: usize = 2 * 1024 * 1024;

fn owner(auth: &Auth) -> String {
    format!(
        "{}:{}",
        auth.0.kind.as_str(),
        auth.0
            .session_id
            .as_deref()
            .or(auth.0.token_id.as_deref())
            .unwrap_or("")
    )
}

fn valid_metadata(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn safe_url(value: &str) -> bool {
    if value.len() > 2048
        || value
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return false;
    }
    let Ok(uri) = value.parse::<axum::http::Uri>() else {
        return false;
    };
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return false;
    }
    let Some(authority) = uri.authority() else {
        return false;
    };
    let host = authority.host();
    if authority.as_str().contains('@') || host.is_empty() {
        return false;
    }
    // HTTP URI parsing accepts arbitrary port text and bracketed non-IP hosts.
    // Browsers cannot reliably open those as web URLs, so reject them here.
    if host.starts_with('[')
        && host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .and_then(|host| host.parse::<std::net::Ipv6Addr>().ok())
            .is_none()
    {
        return false;
    }
    match authority.as_str().strip_prefix(host) {
        Some("") => true,
        Some(port) => port.strip_prefix(':').is_some_and(|port| {
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok()
        }),
        None => false,
    }
}

fn valid_candidate(candidate: &Candidate) -> bool {
    valid_metadata(&candidate.identity, 512)
        && valid_metadata(&candidate.kind, 128)
        && safe_url(&candidate.source_url)
        && tasks::validate_title(&candidate.title).is_ok()
        && tasks::validate_description(&candidate.description).is_ok()
}

fn usable_info(info: &ProviderInfo) -> bool {
    valid_metadata(&info.id, 128)
        && valid_metadata(&info.name, 200)
        && info.sources.len() <= 100
        && info.filters.len() <= 50
        && info
            .sources
            .iter()
            .all(|s| valid_metadata(&s.id, 256) && valid_metadata(&s.name, 200))
        && info
            .filters
            .iter()
            .all(|f| valid_metadata(&f.id, 128) && valid_metadata(&f.label, 200))
}

pub async fn providers(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<ProviderInfo>>, ApiError> {
    require_human(&auth.0)?;
    let infos: Vec<_> = state.imports.providers.iter().map(|p| p.info()).collect();
    if infos.iter().any(|info| !usable_info(info)) {
        return Err(ApiError::service_unavailable(
            "provider_error",
            "Provider metadata is invalid",
        ));
    }
    Ok(Json(infos))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    project_id: String,
    provider: String,
    source: String,
    filters: HashMap<String, String>,
}

#[derive(Serialize)]
struct PreviewItem {
    index: usize,
    candidate: Candidate,
    valid: bool,
    existing: Option<String>,
}

#[derive(Serialize)]
pub struct PreviewResponse {
    preview_id: String,
    candidates: Vec<PreviewItem>,
}

async fn active_project(state: &AppState, project: String) -> Result<bool, ApiError> {
    Ok(state
        .db
        .call(move |db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1 AND archived_at IS NULL)",
                [project],
                |row| row.get(0),
            )?)
        })
        .await?)
}

/// Reserve before any async work, so even a later invalid request invalidates
/// the caller's old preview and an older request cannot finish over a newer one.
fn reserve_preview(state: &AppState, auth: &Auth) -> Result<String, ApiError> {
    let preview_id = ids::base64_url(&ids::random_bytes::<32>()?);
    let owner = owner(auth);
    let mut cache = state
        .imports
        .previews
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.retain(|_, p| p.created.elapsed() < TTL && p.owner != owner);
    if cache.len() >= MAX_PREVIEWS {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, p)| p.created)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        preview_id.clone(),
        Preview {
            created: Instant::now(),
            owner,
            project: String::new(),
            provider: String::new(),
            source: String::new(),
            candidates: Vec::new(),
        },
    );
    Ok(preview_id)
}

const EXISTING_SQL: &str = "SELECT p.key || '-' || t.number
     FROM task_provenance x
     JOIN tasks t ON t.id = x.task_id
     JOIN projects p ON p.id = t.project_id
     WHERE x.project_id = ?1 AND x.provider = ?2 AND x.identity = ?3";

pub async fn preview(
    State(state): State<AppState>,
    auth: Auth,
    ApiJson(request): ApiJson<PreviewRequest>,
) -> Result<Json<PreviewResponse>, ApiError> {
    require_human(&auth.0)?;
    let preview_id = reserve_preview(&state, &auth)?;
    if !active_project(&state, request.project_id.clone()).await? {
        return Err(ApiError::validation(
            "Destination project is missing or archived",
        ));
    }
    let provider = state
        .imports
        .providers
        .iter()
        .find(|p| p.info().id == request.provider)
        .cloned()
        .ok_or_else(|| ApiError::validation("Unknown provider"))?;
    let info = provider.info();
    if !usable_info(&info)
        || !info.sources.iter().any(|s| s.id == request.source)
        || request.filters.len() > info.filters.len()
        || request.filters.iter().any(|(k, v)| {
            !info.filters.iter().any(|f| &f.id == k)
                || v.len() > 1024
                || v.chars().any(char::is_control)
        })
    {
        return Err(ApiError::validation("Invalid provider source or filters"));
    }
    let source = request.source.clone();
    let filters = request.filters;
    let candidates = tokio::task::spawn_blocking(move || provider.preview(&source, &filters))
        .await
        .map_err(|_| ApiError::service_unavailable("provider_error", "Provider preview failed"))?
        .map_err(|_| ApiError::service_unavailable("provider_error", "Provider preview failed"))?;
    if candidates.len() > MAX_ITEMS
        || candidates
            .iter()
            .map(|c| {
                c.identity.len()
                    + c.kind.len()
                    + c.title.len()
                    + c.description.len()
                    + c.source_url.len()
            })
            .sum::<usize>()
            > MAX_PREVIEW_BYTES
    {
        return Err(ApiError::service_unavailable(
            "provider_error",
            "Provider preview exceeds limits",
        ));
    }
    let project = request.project_id.clone();
    let provider_id = request.provider.clone();
    let identities: Vec<_> = candidates.iter().map(|c| c.identity.clone()).collect();
    let existing = state
        .db
        .call(move |db| {
            let mut statement = db.prepare(EXISTING_SQL)?;
            identities
                .iter()
                .map(|id| {
                    statement
                        .query_row(params![project, provider_id, id], |row| row.get(0))
                        .optional()
                        .map_err(Into::into)
                })
                .collect::<crate::error::Result<Vec<Option<String>>>>()
        })
        .await?;
    let response = candidates
        .iter()
        .cloned()
        .zip(existing)
        .enumerate()
        .map(|(index, (candidate, existing))| PreviewItem {
            index,
            valid: valid_candidate(&candidate),
            candidate,
            existing,
        })
        .collect();
    let mut cache = state
        .imports
        .previews
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let snapshot = cache
        .get_mut(&preview_id)
        .filter(|p| p.created.elapsed() < TTL)
        .ok_or_else(|| ApiError::conflict("Preview expired or changed; request a new preview"))?;
    snapshot.project = request.project_id;
    snapshot.provider = request.provider;
    snapshot.source = request.source;
    snapshot.candidates = candidates;
    Ok(Json(PreviewResponse {
        preview_id,
        candidates: response,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    preview_id: String,
    project_id: String,
    provider: String,
    source: String,
    selected: Vec<usize>,
}

#[derive(Serialize)]
pub struct Outcome {
    index: usize,
    status: &'static str,
    reference: Option<String>,
    message: Option<&'static str>,
}

enum ImportResult {
    Created(Box<super::model::Task>),
    Existing(String),
    MissingProject,
}

fn import_candidate(
    db: &mut rusqlite::Connection,
    project: &str,
    provider: &str,
    actor: &str,
    candidate: &Candidate,
) -> crate::error::Result<ImportResult> {
    let tx = db.transaction()?;
    let project_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1 AND archived_at IS NULL)",
        [project],
        |row| row.get(0),
    )?;
    if !project_exists {
        return Ok(ImportResult::MissingProject);
    }

    let existing: Option<String> = tx
        .query_row(
            EXISTING_SQL,
            params![project, provider, candidate.identity],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(reference) = existing {
        return Ok(ImportResult::Existing(reference));
    }

    let number: i64 = tx.query_row(
        "SELECT COALESCE(MAX(number), 0) + 1 FROM tasks WHERE project_id = ?1",
        [project],
        |row| row.get(0),
    )?;
    let max_rank: Option<f64> = tx.query_row(
        "SELECT MAX(rank) FROM tasks WHERE project_id = ?1 AND status = 'backlog'",
        [project],
        |row| row.get(0),
    )?;
    let id = ids::uuid_v4()?;
    let now = now_millis();
    let title = tasks::validate_title(&candidate.title)
        .map_err(|_| crate::error::Error::msg("invalid title"))?;
    tx.execute(
        "INSERT INTO tasks
         (id, project_id, number, title, description, status, state, rank,
          priority, source, created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'backlog', 'active', ?6,
                 'normal', 'human', ?7, ?8, ?8)",
        params![
            id,
            project,
            number,
            title,
            candidate.description,
            rank::append(max_rank),
            actor,
            now
        ],
    )?;
    tx.execute(
        "INSERT INTO task_provenance
         (task_id, project_id, provider, identity, kind, source_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            project,
            provider,
            candidate.identity,
            candidate.kind,
            candidate.source_url
        ],
    )?;
    tasks::insert_activity(
        &tx,
        &id,
        actor,
        "created",
        &json!({"number": number, "status": "backlog"}),
    )?;
    let task = tasks::task_by_ref(&tx, &id)?
        .ok_or_else(|| crate::error::Error::msg("created task missing"))?;
    tx.commit()?;
    Ok(ImportResult::Created(Box::new(task)))
}

pub async fn confirm(
    State(state): State<AppState>,
    auth: Auth,
    ApiJson(request): ApiJson<ConfirmRequest>,
) -> Result<Json<Vec<Outcome>>, ApiError> {
    require_human(&auth.0)?;
    let preview = {
        let mut cache = state
            .imports
            .previews
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, p| p.created.elapsed() < TTL);
        cache.get(&request.preview_id).cloned()
    }
    .ok_or_else(|| ApiError::conflict("Preview expired or changed; request a new preview"))?;
    if preview.owner != owner(&auth)
        || preview.project != request.project_id
        || preview.provider != request.provider
        || preview.source != request.source
    {
        return Err(ApiError::conflict("Preview does not match this import"));
    }
    if request.selected.is_empty()
        || request.selected.len() > MAX_ITEMS
        || request
            .selected
            .iter()
            .any(|i| *i >= preview.candidates.len())
        || request.selected.iter().collect::<HashSet<_>>().len() != request.selected.len()
    {
        return Err(ApiError::validation(
            "Select distinct candidates from the preview",
        ));
    }
    if !active_project(&state, preview.project.clone()).await? {
        return Err(ApiError::validation(
            "Destination project is missing or archived",
        ));
    }
    let mut outcomes = Vec::new();
    for index in request.selected {
        let candidate = preview.candidates[index].clone();
        if !valid_candidate(&candidate) {
            outcomes.push(Outcome {
                index,
                status: "failed",
                reference: None,
                message: Some("Invalid candidate metadata or content"),
            });
            continue;
        }
        let project = preview.project.clone();
        let provider = preview.provider.clone();
        let actor = auth.0.actor().to_owned();
        let result = state
            .db
            .call(move |db| import_candidate(db, &project, &provider, &actor, &candidate))
            .await;
        match result {
            Ok(ImportResult::Created(task)) => {
                let reference = task.reference.clone();
                state.publish("task.created", json!({"task": task}));
                outcomes.push(Outcome {
                    index,
                    status: "created",
                    reference: Some(reference),
                    message: None,
                });
            }
            Ok(ImportResult::Existing(reference)) => outcomes.push(Outcome {
                index,
                status: "skipped",
                reference: Some(reference),
                message: None,
            }),
            _ => outcomes.push(Outcome {
                index,
                status: "failed",
                reference: None,
                message: Some("Could not import candidate"),
            }),
        }
    }
    Ok(Json(outcomes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthConfig, Identity, IdentityKind, Scope};
    use crate::db::Database;
    use crate::test_support::TempDir;

    #[test]
    fn source_urls_only_allow_safe_web_authorities() {
        assert!(safe_url("https://example.com/item?q=1"));
        assert!(safe_url("https://[::1]:443/item"));
        for value in [
            "javascript:alert(1)",
            "//example.com",
            "https://user:pass@example.com/",
            "https://example.com\\@evil.test",
            "https://example.com/\n",
            "https://",
            "https://example.com:notaport/",
            "https://example.com:65536/",
            "https://[not-an-ip]/",
        ] {
            assert!(!safe_url(value), "{value:?}");
        }
    }

    #[test]
    fn preview_cache_evicts_the_oldest_when_full() {
        let dir = TempDir::new();
        let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
        let auth_config = AuthConfig::load("password", None, dir.path()).unwrap();
        let state = AppState::new(db, dir.path().to_path_buf(), auth_config);
        let mut auth = Auth(Identity {
            kind: IdentityKind::Ui,
            name: "ui".into(),
            scope: Scope::Human,
            display_name: None,
            session_id: None,
            token_id: None,
        });
        let mut first_id = String::new();
        for index in 0..=MAX_PREVIEWS {
            auth.0.session_id = Some(format!("session-{index}"));
            let id = reserve_preview(&state, &auth).unwrap();
            if index == 0 {
                first_id = id.clone();
                state
                    .imports
                    .previews
                    .lock()
                    .unwrap()
                    .get_mut(&id)
                    .unwrap()
                    .created = Instant::now() - Duration::from_secs(1);
            }
        }
        let cache = state.imports.previews.lock().unwrap();
        assert_eq!(cache.len(), MAX_PREVIEWS);
        assert!(!cache.contains_key(&first_id));
    }

    #[tokio::test]
    async fn expired_snapshots_are_rejected() {
        let dir = TempDir::new();
        let db = Database::open(&dir.path().join("db.sqlite")).unwrap();
        let auth_config = AuthConfig::load("password", None, dir.path()).unwrap();
        let state = AppState::new(db, dir.path().to_path_buf(), auth_config);
        let auth = Auth(Identity {
            kind: IdentityKind::Ui,
            name: "ui".into(),
            scope: Scope::Human,
            display_name: None,
            session_id: Some("session".into()),
            token_id: None,
        });
        state.imports.previews.lock().unwrap().insert(
            "expired".into(),
            Preview {
                created: Instant::now() - TTL - Duration::from_secs(1),
                owner: owner(&auth),
                project: "p".into(),
                provider: "fixture".into(),
                source: "s".into(),
                candidates: Vec::new(),
            },
        );
        let result = confirm(
            State(state),
            auth,
            ApiJson(ConfirmRequest {
                preview_id: "expired".into(),
                project_id: "p".into(),
                provider: "fixture".into(),
                source: "s".into(),
                selected: vec![0],
            }),
        )
        .await;
        assert_eq!(
            result.err().unwrap().status(),
            axum::http::StatusCode::CONFLICT
        );
    }
}
