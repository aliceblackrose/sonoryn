use std::sync::Arc;

use gloamwire::{Cache, CacheConfig};
use sonoryn::media::TrackResolver;
use tokio::sync::{RwLock, mpsc};

use crate::{gateway_control::GatewayControl, player::PlayerDirectory};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) cache: Arc<RwLock<Cache>>,
    pub(crate) gateway_control: mpsc::Sender<GatewayControl>,
    pub(crate) players: PlayerDirectory,
    pub(crate) resolver: Arc<dyn TrackResolver>,
}

impl AppState {
    #[must_use]
    pub(crate) fn new(
        gateway_control: mpsc::Sender<GatewayControl>,
        players: PlayerDirectory,
        resolver: Arc<dyn TrackResolver>,
    ) -> Self {
        Self {
            cache: Arc::new(RwLock::new(Cache::new(CacheConfig::new()))),
            gateway_control,
            players,
            resolver,
        }
    }
}
