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

    /// Returns a detached snapshot so callers do not need to retain a manager lock
    /// while rendering responses or performing I/O.
    #[must_use]
    pub fn snapshot(&self, guild_id: GuildId) -> PlayerSnapshot {
        self.players
            .get(&guild_id)
            .map(GuildPlayer::snapshot)
            .unwrap_or_default()
    }

    /// Converts resolver output into a durable track and appends it to the guild
    /// FIFO. Track IDs are process-local but unique for the lifetime of this
    /// manager, including across guilds.
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

    /// Appends a track to the guild FIFO and returns its one-based queue position.
    pub fn enqueue(&mut self, guild_id: GuildId, track: Track) -> usize {
        let player = self.players.entry(guild_id).or_default();
        player.queue.push_back(track);
        player.queue.len()
    }

    /// Removes one queued track by zero-based index without touching the active track.
    pub fn remove_queued(&mut self, guild_id: GuildId, index: usize) -> Option<Track> {
        self.players.get_mut(&guild_id)?.queue.remove(index)
    }

    /// Moves one queued track between zero-based indices and returns the moved track.
    ///
    /// Invalid source or destination indices leave the queue unchanged.
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

    /// Randomizes only the queued tracks and returns the number of tracks shuffled.
    ///
    /// The active track is intentionally excluded from the shuffle.
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

    /// Promotes the next queued track to `now_playing` when the player is idle.
    ///
    /// A clone is returned for a playback worker while the authoritative track
    /// remains visible through snapshots.
    pub fn start_next(&mut self, guild_id: GuildId) -> Option<Track> {
        let player = self.players.get_mut(&guild_id)?;
        if player.now_playing.is_some() {
            return None;
        }

        let track = player.queue.pop_front()?;
        player.now_playing = Some(track.clone());
        Some(track)
    }

    /// Clears the current track only when the completing worker still owns it.
    ///
    /// Matching by `TrackId` prevents a stale decoder task from clearing a newer
    /// track after a skip or rapid transition.
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

    /// Removes all playback state for a guild and returns what was removed.
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
        assert_eq!(before.queue[0].id, TrackId::new(1));
        assert_eq!(before.queue[1].id, TrackId::new(2));

        let started = players.start_next(guild_id).expect("next track");
        assert_eq!(started.id, TrackId::new(1));

        let after = players.snapshot(guild_id);
        assert_eq!(
            after.now_playing.as_ref().map(|track| track.id),
            Some(TrackId::new(1))
        );
        assert_eq!(after.queue.len(), 1);
        assert_eq!(before.queue.len(), 2);
    }

    #[test]
    fn resolved_tracks_receive_unique_process_local_ids() {
        let mut players = PlayerManager::new();
        let requested_by = RequestedBy::new(UserId::new(7));

        let (first, first_position) = players.enqueue_resolved(
            GuildId::new(1),
            TrackRequest::new("first").expect("request"),
            requested_by,
            resolved(1, "first"),
        );
        let (second, second_position) = players.enqueue_resolved(
            GuildId::new(2),
            TrackRequest::new("second").expect("request"),
            requested_by,
            resolved(2, "second"),
        );

        assert_eq!(first.id, TrackId::new(1));
        assert_eq!(second.id, TrackId::new(2));
        assert_eq!(first_position, 1);
        assert_eq!(second_position, 1);
    }

    #[test]
    fn current_track_can_only_be_finished_by_matching_worker() {
        let guild_id = GuildId::new(42);
        let mut players = PlayerManager::new();
        players.enqueue(guild_id, track(1, "first"));
        players.start_next(guild_id).expect("next track");

        assert!(!players.finish_current(guild_id, TrackId::new(99)));
        assert!(players.snapshot(guild_id).now_playing.is_some());

        assert!(players.finish_current(guild_id, TrackId::new(1)));
        assert!(players.snapshot(guild_id).is_idle());
    }

    #[test]
    fn remove_queued_does_not_touch_current_track() {
        let guild_id = GuildId::new(42);
        let mut players = PlayerManager::new();
        players.enqueue(guild_id, track(1, "current"));
        players.enqueue(guild_id, track(2, "remove"));
        players.enqueue(guild_id, track(3, "keep"));
        players.start_next(guild_id).expect("current track");

        let removed = players.remove_queued(guild_id, 0).expect("queued track");
        assert_eq!(removed.id, TrackId::new(2));

        let snapshot = players.snapshot(guild_id);
        assert_eq!(
            snapshot.now_playing.as_ref().map(|track| track.id),
            Some(TrackId::new(1))
        );
        assert_eq!(snapshot.queue.len(), 1);
        assert_eq!(snapshot.queue[0].id, TrackId::new(3));
    }

    #[test]
    fn move_queued_reorders_without_losing_tracks() {
        let guild_id = GuildId::new(42);
        let mut players = PlayerManager::new();
        players.enqueue(guild_id, track(1, "first"));
        players.enqueue(guild_id, track(2, "second"));
        players.enqueue(guild_id, track(3, "third"));

        let moved = players.move_queued(guild_id, 0, 2).expect("moved track");
        assert_eq!(moved.id, TrackId::new(1));
        assert_eq!(
            players
                .snapshot(guild_id)
                .queue
                .iter()
                .map(|track| track.id)
                .collect::<Vec<_>>(),
            [TrackId::new(2), TrackId::new(3), TrackId::new(1)]
        );
    }

    #[test]
    fn invalid_queue_move_is_non_destructive() {
        let guild_id = GuildId::new(42);
        let mut players = PlayerManager::new();
        players.enqueue(guild_id, track(1, "first"));
        players.enqueue(guild_id, track(2, "second"));
        let before = players.snapshot(guild_id);

        assert!(players.move_queued(guild_id, 0, 9).is_none());
        assert_eq!(players.snapshot(guild_id), before);
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

        let mut ids = snapshot
            .queue
            .iter()
            .map(|track| track.id.get())
            .collect::<Vec<_>>();
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
