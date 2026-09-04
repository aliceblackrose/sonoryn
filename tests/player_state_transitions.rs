use std::time::Duration;

use gloamwire::model::{GuildId, UserId};
use sonoryn::{
    media::{RequestedBy, ResolvedTrack, Track, TrackId, TrackMetadata, TrackRequest, TrackSource},
    player::{LoopMode, PlayerManager},
};

fn track(id: u64, title: &str) -> Track {
    Track::from_resolved(
        TrackId::new(id),
        TrackRequest::new(title).expect("request"),
        RequestedBy::new(UserId::new(7)),
        ResolvedTrack {
            source: TrackSource::YouTube,
            metadata: TrackMetadata {
                title: title.to_owned(),
                artist: Some("Fixture Artist".to_owned()),
                duration: Some(Duration::from_secs(60)),
                artwork_url: None,
                webpage_url: format!("https://example.test/{id}"),
            },
            locator: format!("https://example.test/{id}"),
        },
    )
}

#[test]
fn fifo_transition_moves_from_idle_to_playing_to_next_track() {
    let guild = GuildId::new(42);
    let mut players = PlayerManager::new();
    players.enqueue(guild, track(1, "first"));
    players.enqueue(guild, track(2, "second"));

    let first = players.start_next(guild).expect("first track");
    assert_eq!(first.id, TrackId::new(1));
    assert_eq!(players.snapshot(guild).queued_len(), 1);

    assert!(players.complete_current(guild, first.id));
    let second = players.start_next(guild).expect("second track");
    assert_eq!(second.id, TrackId::new(2));
    assert!(players.snapshot(guild).queue.is_empty());
}

#[test]
fn stale_completion_cannot_clear_a_newer_track() {
    let guild = GuildId::new(42);
    let mut players = PlayerManager::new();
    players.enqueue(guild, track(1, "first"));
    players.enqueue(guild, track(2, "second"));

    let first = players.start_next(guild).expect("first track");
    assert!(players.finish_current(guild, first.id));
    let second = players.start_next(guild).expect("second track");

    assert!(!players.finish_current(guild, first.id));
    assert_eq!(
        players.snapshot(guild).now_playing.map(|track| track.id),
        Some(second.id)
    );
}

#[test]
fn queue_loop_requeues_completed_track_after_remaining_items() {
    let guild = GuildId::new(42);
    let mut players = PlayerManager::new();
    players.enqueue(guild, track(1, "first"));
    players.enqueue(guild, track(2, "second"));
    players.set_loop_mode(guild, LoopMode::Queue);

    let first = players.start_next(guild).expect("first track");
    assert!(players.complete_current(guild, first.id));

    let snapshot = players.snapshot(guild);
    assert_eq!(
        snapshot.queue.iter().map(|track| track.id).collect::<Vec<_>>(),
        [TrackId::new(2), TrackId::new(1)]
    );
}

#[test]
fn clear_isolated_to_one_guild_and_returns_removed_state() {
    let left = GuildId::new(1);
    let right = GuildId::new(2);
    let mut players = PlayerManager::new();
    players.enqueue(left, track(1, "left"));
    players.enqueue(right, track(2, "right"));
    players.start_next(left).expect("left track");

    let removed = players.clear(left);
    assert_eq!(removed.now_playing.map(|track| track.id), Some(TrackId::new(1)));
    assert!(players.snapshot(left).is_idle());
    assert_eq!(players.snapshot(right).queued_len(), 1);
}
