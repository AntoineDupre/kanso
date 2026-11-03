use axum::extract::State;
use axum::Json;
use serde::Serialize;

use super::{ApiError, Auth};
use crate::server::AppState;

#[derive(Serialize)]
pub struct Integration {
    pub openchamber: OpenChamberInfo,
}

/// Disabled is exactly `{enabled:false}`; enabled always carries all four keys,
/// including `version:null` when the probe cannot read one.
#[derive(Serialize)]
#[serde(untagged)]
pub enum OpenChamberInfo {
    Disabled {
        enabled: bool,
    },
    Enabled {
        enabled: bool,
        url: String,
        reachable: bool,
        version: Option<String>,
    },
}

/// `GET /api/integration` — never fails when OpenChamber is down, and never
/// returns the configured token.
pub async fn status(
    State(state): State<AppState>,
    _auth: Auth,
) -> Result<Json<Integration>, ApiError> {
    let Some(openchamber) = state.openchamber.as_ref() else {
        return Ok(Json(Integration {
            openchamber: OpenChamberInfo::Disabled { enabled: false },
        }));
    };

    let probe = openchamber.version().await;
    Ok(Json(Integration {
        openchamber: OpenChamberInfo::Enabled {
            enabled: true,
            url: openchamber.url().to_owned(),
            reachable: probe.reachable,
            version: probe.version,
        },
    }))
}
