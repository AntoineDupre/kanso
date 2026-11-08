//! Provider-neutral external task previews. Production registers no providers.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

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
}

#[derive(Debug)]
pub struct ProviderError;

/// Implementations must keep credentials out of metadata, candidates and errors.
/// `info` must be fast and nonblocking. `preview` runs on a blocking worker
/// without a database lock; the adapter must bound its own I/O and response sizes.
/// Provider errors are never exposed to clients.
pub trait Provider: Send + Sync {
    fn info(&self) -> ProviderInfo;
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
