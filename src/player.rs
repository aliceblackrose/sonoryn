use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gloamwire::model::{ChannelId, GuildId};
use sonoryn::media::{Track, TrackId};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PlaybackState {
    #[default]
    Idle,
    Loading,
    Playing,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PlayerSnapshot {
    pub(crate) channel_id: Option<ChannelId>,
    pub(crate) state: PlaybackState,
    pub(crate) current: Option<Track>,
    pub(crate) queue: Vec<Track>,
}

#[derive(Default)]
struct PlayerDirectoryInner {
    snapshots: RwLock<HashMap<GuildId, PlayerSnapshot>>,
    next_track_id: AtomicU64,
}

#[derive(Clone, Default)]
pub(crate) struct PlayerDirectory {
    inner: Arc<PlayerDirectoryInner>,
}

impl PlayerDirectory {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub(crate) fn next_track_id(&self) -> TrackId {
        TrackId::new(self.inner.next_track_id.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) async fn snapshot(&self, guild_id: GuildId) -> Option<PlayerSnapshot> {
        self.inner.snapshots.read().await.get(&guild_id).cloned()
    }

    pub(crate) async fn publish(&self, guild_id: GuildId, snapshot: PlayerSnapshot) {
        self.inner
            .snapshots
            .write()
            .await
            .insert(guild_id, snapshot);
    }

    pub(crate) async fn remove(&self, guild_id: GuildId) {
        self.inner.snapshots.write().await.remove(&guild_id);
    }
}

#[cfg(test)]
mod tests {
    use gloamwire::model::{ChannelId, GuildId};

    use super::{PlaybackState, PlayerDirectory, PlayerSnapshot};

    #[tokio::test]
    async fn publishes_and_removes_guild_snapshots() {
        let directory = PlayerDirectory::new();
        let guild_id = GuildId::new(1);
        let snapshot = PlayerSnapshot {
            channel_id: Some(ChannelId::new(2)),
            state: PlaybackState::Loading,
            ..Default::default()
        };

        directory.publish(guild_id, snapshot.clone()).await;
        assert_eq!(directory.snapshot(guild_id).await, Some(snapshot));

        directory.remove(guild_id).await;
        assert_eq!(directory.snapshot(guild_id).await, None);
    }

    #[test]
    fn allocates_monotonic_track_ids() {
        let directory = PlayerDirectory::new();
        assert_eq!(directory.next_track_id().get(), 0);
        assert_eq!(directory.next_track_id().get(), 1);
    }
}
