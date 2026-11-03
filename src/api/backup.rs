use std::path::{Component, Path, PathBuf};

use axum::body::Bytes;
use axum::extract::State;
use axum::Json;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{now_millis, require_human, ApiError, Auth};
use crate::ids;
use crate::server::AppState;
use crate::time;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupRequest {
    /// Optional directory. Relative paths resolve under the data directory;
    /// absolute paths must stay inside it.
    dir: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Backup {
    pub path: String,
    pub size: u64,
    pub created_at: i64,
}

/// Lexically normalise a path, resolving `.` and `..` without touching disk.
fn normalise(path: &Path) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => result.push(prefix.as_os_str()),
            Component::RootDir => result.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            Component::Normal(part) => result.push(part),
        }
    }
    Some(result)
}

/// Resolve an absolute or relative request against both the configured data
/// directory and its canonical form. `data_dir` may itself be a symlink, so an
/// absolute path using the configured spelling is still inside the data dir.
fn resolve_for_data_dir(data_dir: &Path, canonical: &Path, requested: &Path) -> Option<PathBuf> {
    if requested.is_absolute() {
        let candidate = normalise(requested)?;
        if candidate.starts_with(canonical) {
            return Some(candidate);
        }
        let configured = normalise(data_dir)?;
        if candidate.starts_with(&configured) {
            let relative = candidate.strip_prefix(&configured).ok()?;
            return Some(canonical.join(relative));
        }
        return None;
    }
    normalise(&canonical.join(requested)).filter(|candidate| candidate.starts_with(canonical))
}

/// The deepest ancestor that exists, used to check symlinks before creating
/// directories below it.
fn nearest_existing(path: &Path) -> Option<&Path> {
    let mut current = path;
    loop {
        if current.exists() {
            return Some(current);
        }
        current = current.parent()?;
    }
}

fn validate_dir(raw: &str) -> Result<PathBuf, ApiError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ApiError::validation("dir must not be empty"));
    }
    if raw.len() > 4096 {
        return Err(ApiError::validation("dir must be at most 4096 bytes"));
    }
    Ok(PathBuf::from(raw))
}

fn parse_request(body: &[u8]) -> Result<BackupRequest, ApiError> {
    if body.is_empty() {
        return Ok(BackupRequest::default());
    }
    serde_json::from_slice(body)
        .map_err(|error| ApiError::validation(format!("invalid JSON body: {error}")))
}

pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    body: Bytes,
) -> Result<Json<Backup>, ApiError> {
    require_human(&auth.0)?;
    let payload = parse_request(&body)?;

    let db_path = state.db.path();
    let data_dir = db_path
        .parent()
        .ok_or_else(|| ApiError::internal("database path has no parent directory"))?;
    std::fs::create_dir_all(data_dir)
        .map_err(|error| ApiError::internal(format!("failed to create data directory: {error}")))?;
    let base = std::fs::canonicalize(data_dir).map_err(|error| {
        ApiError::internal(format!("failed to resolve data directory: {error}"))
    })?;

    let directory = match payload.dir.as_deref() {
        Some(raw) => {
            let requested = validate_dir(raw)?;
            resolve_for_data_dir(data_dir, &base, &requested)
                .ok_or_else(|| ApiError::validation("dir must stay inside the data directory"))?
        }
        None => base.join("backups"),
    };

    // Check the deepest existing ancestor before creating anything: a symlink
    // planted inside the data directory must not make `create_dir_all` create
    // directories outside it.
    let ancestor = nearest_existing(&directory)
        .ok_or_else(|| ApiError::validation("dir must stay inside the data directory"))?;
    let ancestor = std::fs::canonicalize(ancestor)
        .map_err(|error| ApiError::internal(format!("failed to resolve backup path: {error}")))?;
    if !ancestor.starts_with(&base) {
        return Err(ApiError::validation(
            "dir must stay inside the data directory",
        ));
    }

    std::fs::create_dir_all(&directory).map_err(|error| {
        ApiError::internal(format!("failed to create backup directory: {error}"))
    })?;
    let resolved_dir = std::fs::canonicalize(&directory).map_err(|error| {
        ApiError::internal(format!("failed to resolve backup directory: {error}"))
    })?;
    if !resolved_dir.starts_with(&base) {
        return Err(ApiError::validation(
            "dir must stay inside the data directory",
        ));
    }

    let created_at = now_millis();
    let stamp = time::file_stamp(created_at);
    // A random suffix makes the name unique across processes as well as within
    // this one, so a failed VACUUM can safely remove only our own partial file.
    let directory_for_query = resolved_dir.clone();
    let path = state
        .db
        .call(move |connection| {
            let name = format!("kanso-{stamp}-{}.db", ids::uuid_v4()?);
            let candidate = directory_for_query.join(name);
            if candidate.symlink_metadata().is_ok() {
                return Err(crate::error::Error::msg(
                    "generated backup path already exists",
                ));
            }
            if let Err(error) = connection.execute(
                "VACUUM INTO ?1",
                params![candidate.to_string_lossy().into_owned()],
            ) {
                // A failed VACUUM can leave a partial file behind.
                let _ = std::fs::remove_file(&candidate);
                return Err(crate::error::Error::from(error));
            }
            // SQLite does not fsync a `VACUUM INTO` output, so make the
            // complete copy durable before reporting it.
            if let Err(error) = std::fs::File::open(&candidate).and_then(|file| file.sync_all()) {
                let _ = std::fs::remove_file(&candidate);
                return Err(crate::error::Error::msg(format!(
                    "failed to sync backup {}: {error}",
                    candidate.display()
                )));
            }
            Ok(candidate)
        })
        .await
        .map_err(|error| ApiError::internal(format!("backup failed: {error}")))?;

    #[cfg(unix)]
    if let Ok(directory) = std::fs::File::open(&resolved_dir) {
        let _ = directory.sync_all();
    }

    let size = std::fs::metadata(&path)
        .map_err(|error| ApiError::internal(format!("failed to stat backup: {error}")))?
        .len();

    Ok(Json(Backup {
        path: path.to_string_lossy().into_owned(),
        size,
        created_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(base: &Path, requested: &Path) -> Option<PathBuf> {
        resolve_for_data_dir(base, base, requested)
    }

    #[test]
    fn resolves_relative_directories_under_the_base() {
        let base = Path::new("/data");
        assert_eq!(
            resolve(base, Path::new("backups")),
            Some(PathBuf::from("/data/backups"))
        );
        assert_eq!(
            resolve(base, Path::new("./backups/./daily")),
            Some(PathBuf::from("/data/backups/daily"))
        );
        assert_eq!(
            resolve(base, Path::new("/data/backups")),
            Some(PathBuf::from("/data/backups"))
        );
    }

    #[test]
    fn rejects_paths_that_escape_the_base() {
        let base = Path::new("/data");
        assert_eq!(resolve(base, Path::new("../etc")), None);
        assert_eq!(resolve(base, Path::new("/etc")), None);
        assert_eq!(resolve(base, Path::new("/data/../etc")), None);
        assert_eq!(resolve(base, Path::new("backups/../../etc")), None);
    }

    #[test]
    fn accepts_the_configured_spelling_of_a_symlinked_data_directory() {
        assert_eq!(
            resolve_for_data_dir(
                Path::new("/data"),
                Path::new("/mnt/data"),
                Path::new("/data/backups")
            ),
            Some(PathBuf::from("/mnt/data/backups"))
        );
    }

    #[test]
    fn accepts_an_empty_body_and_rejects_malformed_json() {
        let empty = parse_request(b"").expect("empty body");
        assert!(empty.dir.is_none());
        let parsed = parse_request(br#"{"dir":"backups"}"#).expect("json body");
        assert_eq!(parsed.dir.as_deref(), Some("backups"));
        assert!(parse_request(b"{").is_err());
        assert!(parse_request(br#"{"other":1}"#).is_err());
    }
}
