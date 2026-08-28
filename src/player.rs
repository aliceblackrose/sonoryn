use std::{collections::HashMap, sync::Arc};

use gloamwire::model::{ChannelId, GuildId};
use sonoryn::media::Track;
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

#[derive(Clone, Default)]
pub(crate) struct PlayerDirectory {
    inner: Arc<RwLock<HashMap<GuildId, PlayerSnapshot>>>,
}

impl PlayerDirectory {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn snapshot(&self, guild_id: GuildId) -> Option<PlayerSnapshot> {
        self.inner.read().await.get(&guild_id).cloned()
    }

    pub(crate) async fn publish(&self, guild_id: GuildId, snapshot: PlayerSnapshot) {
        self.inner.write().await.insert(guild_id, snapshot);
    }

    pub(crate) async fn remove(&self, guild_id: GuildId) {
        self.inner.write().await.remove(&guild_id);
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
}
