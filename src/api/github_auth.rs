use super::{ApiError, ApiJson, Auth};
use crate::{
    github_auth::{Progress, Status},
    server::AppState,
};
use axum::{extract::State, Json};
use serde::Deserialize;

fn session(auth: Auth) -> Result<String, ApiError> {
    super::require_human(&auth.0)?;
    auth.0
        .session_id
        .ok_or_else(|| ApiError::forbidden("GitHub connection settings require a browser session"))
}

pub async fn status(State(state): State<AppState>, auth: Auth) -> Result<Json<Status>, ApiError> {
    session(auth)?;
    Ok(Json(state.github_auth.status(false).await))
}

pub async fn refresh(State(state): State<AppState>, auth: Auth) -> Result<Json<Status>, ApiError> {
    session(auth)?;
    Ok(Json(state.github_auth.status(true).await))
}

pub async fn progress(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Progress>, ApiError> {
    let owner = session(auth)?;
    Ok(Json(state.github_auth.progress(&owner).await))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    confirmed: bool,
}

pub async fn start(
    State(state): State<AppState>,
    auth: Auth,
    ApiJson(body): ApiJson<Start>,
) -> Result<Json<Progress>, ApiError> {
    let owner = session(auth)?;
    state
        .github_auth
        .start(owner.clone(), body.confirmed, state.db.clone())
        .await
        .map_err(ApiError::conflict)?;
    Ok(Json(state.github_auth.progress(&owner).await))
}

pub async fn cancel(State(state): State<AppState>, auth: Auth) -> Result<Json<Progress>, ApiError> {
    let owner = session(auth)?;
    if !state.github_auth.cancel(&owner).await {
        return Err(ApiError::conflict(
            "No login owned by this session is in progress.",
        ));
    }
    Ok(Json(state.github_auth.progress(&owner).await))
}
