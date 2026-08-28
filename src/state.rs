use std::sync::Arc;

use gloamwire::{Cache, CacheConfig};
use tokio::sync::{RwLock, mpsc};

use crate::gateway_control::GatewayControl;

#[derive(Clone)]
pub struct AppState {
    pub cache: Arc<RwLock<Cache>>,
    pub gateway_control: mpsc::Sender<GatewayControl>,
}

impl AppState {
    #[must_use]
    pub fn new(gateway_control: mpsc::Sender<GatewayControl>) -> Self {
        Self {
            cache: Arc::new(RwLock::new(Cache::new(CacheConfig::new()))),
            gateway_control,
        }
    }
}
