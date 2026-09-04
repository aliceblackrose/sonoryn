use std::fmt::Write;

use gloam_commands::prelude::*;
use sonoryn::{media::Track, player::PlayerSnapshot};

use crate::state::AppState;

const QUEUE_PREVIEW_LIMIT: usize = 10;
const TRACK_TITLE_LIMIT: usize = 96;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![queue, nowplaying]
}

#[command(description = "Show the current music queue", guild_only)]
pub(crate) async fn queue(ctx: Context<AppState>) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };

    let snapshot = {
        let players = ctx.data().player_manager.read().await;
        players.snapshot(guild_id)
    };

    ctx.reply_ephemeral(render_queue(&snapshot)).await?;
    Ok(())
}

#[command(description = "Show the track Sonoryn is currently playing", guild_only)]
pub(crate) async fn nowplaying(ctx: Context<AppState>) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };

    let current = {
        let players = ctx.data().player_manager.read().await;
        players.snapshot(guild_id).now_playing
    };

    let message = current.as_ref().map_or_else(
        || "Nothing is playing right now.".to_owned(),
        |track| format!("Now playing: {}", format_track(track)),
    );
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

fn render_queue(snapshot: &PlayerSnapshot) -> String {
    if snapshot.is_idle() {
        return "The queue is empty.".to_owned();
    }

    let mut output = String::new();
    if let Some(track) = &snapshot.now_playing {
        let _ = writeln!(output, "Now playing: {}", format_track(track));
    }

    if !snapshot.queue.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("Queue:\n");
        for (index, track) in snapshot.queue.iter().take(QUEUE_PREVIEW_LIMIT).enumerate() {
            let _ = writeln!(output, "{}. {}", index + 1, format_track(track));
        }

        let hidden = snapshot.queue.len().saturating_sub(QUEUE_PREVIEW_LIMIT);
        if hidden > 0 {
            let _ = write!(output, "… and {hidden} more.");
        }
    }

    output.trim_end().to_owned()
}

fn format_track(track: &Track) -> String {
    let title = escape_markdown(&truncate_chars(&track.metadata.title, TRACK_TITLE_LIMIT));
    let artist = track
        .metadata
        .artist
        .as_deref()
        .map(|artist| escape_markdown(&truncate_chars(artist, TRACK_TITLE_LIMIT)));

    let mut output = format!("**{title}**");
    if let Some(artist) = artist {
        let _ = write!(output, " — {artist}");
    }
    if let Some(duration) = track.metadata.duration {
        let _ = write!(output, " ({})", format_duration(duration.as_secs()));
    }
    let _ = write!(
        output,
        " · requested by user `{}`",
        track.requested_by.user_id.get()
    );
    output
}

fn format_duration(total_seconds: u64) -> String {
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let mut truncated: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        truncated.push('…');
    }
    truncated
}

fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' | '*' | '_' | '~' | '`' | '>' | '|' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '@' => escaped.push_str("@\u{200b}"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gloamwire::model::UserId;
    use sonoryn::{
        media::{
            RequestedBy, ResolvedTrack, Track, TrackId, TrackMetadata, TrackRequest, TrackSource,
        },
        player::PlayerSnapshot,
    };

    use super::{format_duration, render_queue};

    fn track(id: u64, title: &str) -> Track {
        Track::from_resolved(
            TrackId::new(id),
            TrackRequest::new(title).expect("request"),
            RequestedBy::new(UserId::new(7)),
            ResolvedTrack {
                source: TrackSource::YouTube,
                metadata: TrackMetadata {
                    title: title.to_owned(),
                    artist: Some("Artist".to_owned()),
                    duration: Some(Duration::from_secs(65)),
                    artwork_url: None,
                    webpage_url: format!("https://example.test/{id}"),
                },
                locator: format!("https://example.test/{id}"),
            },
        )
    }

    #[test]
    fn empty_queue_has_compact_response() {
        assert_eq!(render_queue(&PlayerSnapshot::default()), "The queue is empty.");
    }

    #[test]
    fn queue_preview_is_bounded_and_neutralizes_mentions() {
        let snapshot = PlayerSnapshot {
            now_playing: Some(track(1, "@everyone *current*")),
            queue: (2..=13)
                .map(|id| track(id, &format!("track {id}")))
                .collect(),
        };

        let rendered = render_queue(&snapshot);
        assert!(rendered.contains("@\u{200b}everyone \\*current\\*"));
        assert!(rendered.contains("1. **track 2**"));
        assert!(rendered.contains("10. **track 11**"));
        assert!(!rendered.contains("11. **track 12**"));
        assert!(rendered.contains("… and 2 more."));
    }

    #[test]
    fn duration_formats_hours_and_minutes() {
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(3_661), "1:01:01");
    }
}
