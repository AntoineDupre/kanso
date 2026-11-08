//! Provider-neutral external task previews.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

pub mod github;

/// All metadata is public and must never contain credentials.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub sources: Vec<Source>,
    pub filters: Vec<Filter>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Source {
    pub id: String,
    pub name: String,
}

/// Text controls only; providers validate their values before querying.
#[derive(Clone, Debug, Serialize)]
pub struct Filter {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    /// Stable logical identity across sources, including any category distinctions.
    pub identity: String,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub source_url: String,
    /// Explicitly rejected source content; never eligible for import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_reason: Option<String>,
}

#[derive(Debug)]
pub struct ProviderError;

/// Implementations must keep credentials out of metadata, candidates and errors.
/// `info` must be fast and nonblocking. `preview` runs on a blocking worker
/// without a database lock; the adapter must bound its own I/O and response sizes.
/// Provider errors are never exposed to clients.
pub trait Provider: Send + Sync {
    fn info(&self) -> ProviderInfo;
    fn failure_message(&self) -> &'static str {
        "Provider preview failed"
    }
    /// Resolve public sources for a project while holding the database lock.
    /// Must not perform network or subprocess I/O.
    fn project_info(
        &self,
        _db: &rusqlite::Connection,
        _project: &str,
    ) -> crate::error::Result<ProviderInfo> {
        Ok(self.info())
    }
    fn preview(
        &self,
        source: &str,
        filters: &HashMap<String, String>,
    ) -> Result<Vec<Candidate>, ProviderError>;
}

#[derive(Clone)]
pub(crate) struct Preview {
    pub created: Instant,
    pub owner: String,
    pub project: String,
    pub provider: String,
    pub source: String,
    pub candidates: Vec<Candidate>,
}

#[derive(Default)]
pub struct Registry {
    pub(crate) providers: Vec<Arc<dyn Provider>>,
    pub(crate) previews: Mutex<HashMap<String, Preview>>,
}

impl Registry {
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self {
            providers,
            previews: Mutex::new(HashMap::new()),
        }
    }
}
