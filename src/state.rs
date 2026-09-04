use std::sync::Arc;

use gloamwire::{Cache, CacheConfig};
use sonoryn::player::PlayerManager;
use tokio::sync::{RwLock, mpsc};

use crate::gateway_control::GatewayControl;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) cache: Arc<RwLock<Cache>>,
    pub(crate) gateway_control: mpsc::Sender<GatewayControl>,
    pub(crate) player_manager: Arc<RwLock<PlayerManager>>,
}

impl AppState {
    #[must_use]
    pub(crate) fn new(gateway_control: mpsc::Sender<GatewayControl>) -> Self {
        Self {
            cache: Arc::new(RwLock::new(Cache::new(CacheConfig::new()))),
            gateway_control,
            player_manager: Arc::new(RwLock::new(PlayerManager::new())),
        }
    }
}
