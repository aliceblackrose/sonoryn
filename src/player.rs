use std::collections::{HashMap, VecDeque};

use gloamwire::model::GuildId;
use rand::seq::SliceRandom;

use crate::media::{RequestedBy, ResolvedTrack, Track, TrackId, TrackRequest};

/// Read-only copy of one guild's playback state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerSnapshot {
    pub now_playing: Option<Track>,
    pub queue: Vec<Track>,
}

impl PlayerSnapshot {
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.now_playing.is_none() && self.queue.is_empty()
    }

    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }
}

#[derive(Debug, Default)]
struct GuildPlayer {
    now_playing: Option<Track>,
    queue: VecDeque<Track>,
}

impl GuildPlayer {
    fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            now_playing: self.now_playing.clone(),
            queue: self.queue.iter().cloned().collect(),
        }
    }

    fn into_snapshot(self) -> PlayerSnapshot {
        PlayerSnapshot {
            now_playing: self.now_playing,
            queue: self.queue.into_iter().collect(),
        }
    }
}

/// Owns isolated playback state for every guild known to Sonoryn.
///
/// The manager deliberately contains no Discord transport or resolver handles.
/// Player workers can mutate this state without coupling queue semantics to the
/// Gateway task, command framework, or media backend.
#[derive(Debug, Default)]
pub struct PlayerManager {
    players: HashMap<GuildId, GuildPlayer>,
    next_track_id: u64,
}

impl PlayerManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn snapshot(&self, guild_id: GuildId) -> PlayerSnapshot {
        self.players
            .get(&guild_id)
            .map(GuildPlayer::snapshot)
            .unwrap_or_default()
    }

    pub fn enqueue_resolved(
        &mut self,
        guild_id: GuildId,
        request: TrackRequest,
        requested_by: RequestedBy,
        resolved: ResolvedTrack,
    ) -> (Track, usize) {
        let track = Track::from_resolved(self.allocate_track_id(), request, requested_by, resolved);
        let position = self.enqueue(guild_id, track.clone());
        (track, position)
    }

    pub fn enqueue(&mut self, guild_id: GuildId, track: Track) -> usize {
        let player = self.players.entry(guild_id).or_default();
        player.queue.push_back(track);
        player.queue.len()
    }

    pub fn remove_queued(&mut self, guild_id: GuildId, index: usize) -> Option<Track> {
        self.players.get_mut(&guild_id)?.queue.remove(index)
    }

    pub fn move_queued(
        &mut self,
        guild_id: GuildId,
        from_index: usize,
        to_index: usize,
    ) -> Option<Track> {
        let player = self.players.get_mut(&guild_id)?;
        if from_index >= player.queue.len() || to_index >= player.queue.len() {
            return None;
        }
        if from_index == to_index {
            return player.queue.get(from_index).cloned();
        }

        let track = player.queue.remove(from_index)?;
        let moved = track.clone();
        player.queue.insert(to_index, track);
        Some(moved)
    }

    pub fn shuffle_queued(&mut self, guild_id: GuildId) -> usize {
        let Some(player) = self.players.get_mut(&guild_id) else {
            return 0;
        };
        let count = player.queue.len();
        if count < 2 {
            return count;
        }

        player.queue.make_contiguous().shuffle(&mut rand::rng());
        count
    }

    pub fn start_next(&mut self, guild_id: GuildId) -> Option<Track> {
        let player = self.players.get_mut(&guild_id)?;
        if player.now_playing.is_some() {
            return None;
        }

        let track = player.queue.pop_front()?;
        player.now_playing = Some(track.clone());
        Some(track)
    }

    pub fn finish_current(&mut self, guild_id: GuildId, track_id: TrackId) -> bool {
        let Some(player) = self.players.get_mut(&guild_id) else {
            return false;
        };
        let matches = player
            .now_playing
            .as_ref()
            .is_some_and(|track| track.id == track_id);
        if matches {
            player.now_playing = None;
        }
        matches
    }

    pub fn clear(&mut self, guild_id: GuildId) -> PlayerSnapshot {
        self.players
            .remove(&guild_id)
            .map(GuildPlayer::into_snapshot)
            .unwrap_or_default()
    }

    fn allocate_track_id(&mut self) -> TrackId {
        loop {
            self.next_track_id = self.next_track_id.wrapping_add(1);
            if self.next_track_id != 0 {
                return TrackId::new(self.next_track_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gloamwire::model::{GuildId, UserId};

    use super::PlayerManager;
    use crate::media::{
        RequestedBy, ResolvedTrack, Track, TrackId, TrackMetadata, TrackRequest, TrackSource,
    };

    fn resolved(id: u64, title: &str) -> ResolvedTrack {
        ResolvedTrack {
            source: TrackSource::YouTube,
            metadata: TrackMetadata {
                title: title.to_owned(),
                artist: Some("Artist".to_owned()),
                duration: Some(Duration::from_secs(60 + id)),
                artwork_url: None,
                webpage_url: format!("https://example.test/{id}"),
            },
            locator: format!("https://example.test/{id}"),
        }
    }

    fn track(id: u64, title: &str) -> Track {
        Track::from_resolved(
            TrackId::new(id),
            TrackRequest::new(title).expect("request"),
            RequestedBy::new(UserId::new(7)),
            resolved(id, title),
        )
    }

    #[test]
    fn queue_is_fifo_and_snapshots_are_detached() {
        let guild_id = GuildId::new(42);
        let mut players = PlayerManager::new();
        assert_eq!(players.enqueue(guild_id, track(1, "first")), 1);
        assert_eq!(players.enqueue(guild_id, track(2, "second")), 2);
        let before = players.snapshot(guild_id);
        let started = players.start_next(guild_id).expect("next track");
        assert_eq!(started.id, TrackId::new(1));
        let after = players.snapshot(guild_id);
        assert_eq!(after.queue.len(), 1);
        assert_eq!(before.queue.len(), 2);
    }

    #[test]
    fn shuffle_queued_preserves_current_track_and_membership() {
        let guild_id = GuildId::new(42);
        let mut players = PlayerManager::new();
        players.enqueue(guild_id, track(1, "current"));
        for id in 2..=7 {
            players.enqueue(guild_id, track(id, &format!("track {id}")));
        }
        players.start_next(guild_id).expect("current track");

        assert_eq!(players.shuffle_queued(guild_id), 6);
        let snapshot = players.snapshot(guild_id);
        assert_eq!(
            snapshot.now_playing.as_ref().map(|track| track.id),
            Some(TrackId::new(1))
        );
        let mut ids = snapshot.queue.iter().map(|track| track.id.get()).collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, [2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn shuffle_queued_handles_short_queues_without_mutation() {
        let guild_id = GuildId::new(42);
        let mut players = PlayerManager::new();
        assert_eq!(players.shuffle_queued(guild_id), 0);
        players.enqueue(guild_id, track(1, "only"));
        let before = players.snapshot(guild_id);
        assert_eq!(players.shuffle_queued(guild_id), 1);
        assert_eq!(players.snapshot(guild_id), before);
    }

    #[test]
    fn clear_returns_removed_state() {
        let guild_id = GuildId::new(42);
        let mut players = PlayerManager::new();
        players.enqueue(guild_id, track(1, "first"));
        let removed = players.clear(guild_id);
        assert_eq!(removed.queued_len(), 1);
        assert!(players.snapshot(guild_id).is_idle());
    }
}
