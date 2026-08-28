use std::sync::Arc;

use gloamwire::{Cache, CacheConfig};
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub cache: Arc<RwLock<Cache>>,
}

impl AppState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(Cache::new(CacheConfig::new()))),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
