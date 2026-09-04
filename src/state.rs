use std::sync::Arc;

use gloamwire::{Cache, CacheConfig};
use sonoryn::{
    media::{MAX_RESOLUTION_CONCURRENCY, ObservedResolver, TrackResolver, YtDlpResolver},
    metrics::Metrics,
    player::PlayerManager,
};
use tokio::sync::{RwLock, mpsc};

use crate::{gateway_control::GatewayControl, history::HistoryManager};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) cache: Arc<RwLock<Cache>>,
    pub(crate) gateway_control: mpsc::Sender<GatewayControl>,
    pub(crate) player_manager: Arc<RwLock<PlayerManager>>,
    pub(crate) history_manager: Arc<RwLock<HistoryManager>>,
    pub(crate) resolver: Arc<dyn TrackResolver>,
    pub(crate) metrics: Arc<Metrics>,
}

impl AppState {
    #[must_use]
    pub(crate) fn new(gateway_control: mpsc::Sender<GatewayControl>) -> Self {
        let metrics = Arc::new(Metrics::new());
        let resolver: Arc<dyn TrackResolver> = Arc::new(ObservedResolver::new(
            Arc::new(YtDlpResolver::new()),
            metrics.clone(),
            MAX_RESOLUTION_CONCURRENCY,
        ));
        Self {
            cache: Arc::new(RwLock::new(Cache::new(CacheConfig::new()))),
            gateway_control,
            player_manager: Arc::new(RwLock::new(PlayerManager::new())),
            history_manager: Arc::new(RwLock::new(HistoryManager::new())),
            resolver,
            metrics,
        }
    }
}
