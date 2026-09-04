use std::collections::{HashMap, VecDeque};

use gloamwire::model::GuildId;
use sonoryn::media::Track;

pub(crate) const HISTORY_LIMIT: usize = 20;

#[derive(Debug, Default)]
pub(crate) struct HistoryManager {
    histories: HashMap<GuildId, VecDeque<Track>>,
}

impl HistoryManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, guild_id: GuildId, track: Track) {
        let history = self.histories.entry(guild_id).or_default();
        if history.len() == HISTORY_LIMIT {
            history.pop_front();
        }
        history.push_back(track);
    }

    pub(crate) fn pop_latest(&mut self, guild_id: GuildId) -> Option<Track> {
        let history = self.histories.get_mut(&guild_id)?;
        let track = history.pop_back();
        if history.is_empty() {
            self.histories.remove(&guild_id);
        }
        track
    }

    pub(crate) fn snapshot(&self, guild_id: GuildId) -> Vec<Track> {
        self.histories
            .get(&guild_id)
            .map(|history| history.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gloamwire::model::{GuildId, UserId};
    use sonoryn::media::{
        RequestedBy, ResolvedTrack, Track, TrackId, TrackMetadata, TrackRequest, TrackSource,
    };

    use super::{HISTORY_LIMIT, HistoryManager};

    fn track(id: u64) -> Track {
        Track::from_resolved(
            TrackId::new(id),
            TrackRequest::new(format!("track {id}")).expect("request"),
            RequestedBy::new(UserId::new(7)),
            ResolvedTrack {
                source: TrackSource::YouTube,
                metadata: TrackMetadata {
                    title: format!("track {id}"),
                    artist: None,
                    duration: Some(Duration::from_secs(id)),
                    artwork_url: None,
                    webpage_url: format!("https://example.test/{id}"),
                },
                locator: format!("https://example.test/{id}"),
            },
        )
    }

    #[test]
    fn history_is_bounded_and_keeps_newest_tracks() {
        let guild_id = GuildId::new(42);
        let mut history = HistoryManager::new();
        for id in 1..=(HISTORY_LIMIT as u64 + 3) {
            history.push(guild_id, track(id));
        }

        let snapshot = history.snapshot(guild_id);
        assert_eq!(snapshot.len(), HISTORY_LIMIT);
        assert_eq!(snapshot.first().map(|track| track.id.get()), Some(4));
        assert_eq!(
            snapshot.last().map(|track| track.id.get()),
            Some(HISTORY_LIMIT as u64 + 3)
        );
    }

    #[test]
    fn pop_latest_is_lifo() {
        let guild_id = GuildId::new(42);
        let mut history = HistoryManager::new();
        history.push(guild_id, track(1));
        history.push(guild_id, track(2));

        assert_eq!(history.pop_latest(guild_id).map(|track| track.id.get()), Some(2));
        assert_eq!(history.pop_latest(guild_id).map(|track| track.id.get()), Some(1));
        assert!(history.pop_latest(guild_id).is_none());
    }
}
