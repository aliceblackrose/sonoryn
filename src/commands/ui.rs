//! Shared presentation helpers for Discord command responses.

use std::future::Future;

use gloam_commands::{Context, Result};
use gloamwire::model::{Embed, EmbedAuthor, EmbedField, EmbedFooter, EmbedMedia};
use sonoryn::{
    media::{Track, TrackSource},
    player::{LoopMode, PlayerSnapshot},
};

use crate::state::AppState;

const ACCENT: u32 = 0x8B5CF6;
const DESCRIPTION_LIMIT: usize = 4096;

pub(super) trait ReplyUi {
    fn reply_status(
        &self,
        message: impl Into<String> + Send,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl ReplyUi for Context<AppState> {
    async fn reply_status(&self, message: impl Into<String> + Send) -> Result<()> {
        self.reply_ephemeral_embed(notice(format!("/{}", self.command_name()), message))
            .await
    }
}

pub(super) fn notice(title: impl Into<String>, description: impl Into<String>) -> Embed {
    Embed {
        title: Some(compact(&title.into(), 256)),
        description: Some(compact(&description.into(), DESCRIPTION_LIMIT)),
        color: Some(ACCENT),
        footer: Some(EmbedFooter {
            text: "SONORYN  •  Music player".to_owned(),
            icon_url: None,
            proxy_icon_url: None,
        }),
        ..Default::default()
    }
}

pub(super) fn field(name: impl Into<String>, value: impl Into<String>) -> EmbedField {
    EmbedField {
        name: compact(&name.into(), 256),
        value: compact(&value.into(), 1024),
        inline: Some(true),
    }
}

pub(super) fn section(name: impl Into<String>, value: impl Into<String>) -> EmbedField {
    let mut field = field(name, value);
    field.inline = Some(false);
    field
}

pub(super) fn track_card(status: &str, track: &Track, position: Option<usize>) -> Embed {
    let metadata = &track.metadata;
    let artist = metadata.artist.as_deref().unwrap_or("Unknown artist");
    let mut embed = notice(&metadata.title, artist);
    embed.author = Some(EmbedAuthor {
        name: status.to_owned(),
        url: None,
        icon_url: None,
        proxy_icon_url: None,
    });
    embed.url = public_url(&metadata.webpage_url);
    embed.thumbnail = artwork(track);
    embed.fields = vec![
        field(
            "Duration",
            metadata.duration.map_or_else(
                || "Unknown".to_owned(),
                |duration| clock(duration.as_secs()),
            ),
        ),
        field("Source", source_name(&track.source)),
        field(
            "Requested by",
            format!("<@{}>", track.requested_by.user_id.get()),
        ),
    ];
    if let Some(position) = position {
        embed
            .fields
            .push(field("Queue position", position.to_string()));
    }
    embed
}

pub(super) fn queue_card(
    title: &str,
    description: String,
    snapshot: &PlayerSnapshot,
    mode: LoopMode,
) -> Embed {
    let mut embed = notice(title, description);
    let (seconds, unknown) =
        snapshot
            .queue
            .iter()
            .fold((0u64, 0usize), |(total, unknown), track| {
                match track.metadata.duration {
                    Some(duration) => (total.saturating_add(duration.as_secs()), unknown),
                    None => (total, unknown + 1),
                }
            });
    let duration = if unknown == 0 {
        clock(seconds)
    } else {
        format!("{} + {unknown} unknown", clock(seconds))
    };
    let mode = match mode {
        LoopMode::Off => "Off",
        LoopMode::Track => "Track",
        LoopMode::Queue => "Queue",
    };
    embed.fields = vec![
        field("Up next", snapshot.queue.len().to_string()),
        field("Queued duration", duration),
        field("Repeat", mode),
    ];
    embed.thumbnail = snapshot.now_playing.as_ref().and_then(artwork);
    embed
}

fn artwork(track: &Track) -> Option<EmbedMedia> {
    let url = public_url(track.metadata.artwork_url.as_deref()?)?;
    Some(EmbedMedia {
        url: Some(url),
        ..Default::default()
    })
}

fn public_url(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some() && value.len() <= 2048)
        .then(|| value.to_owned())
}

fn source_name(source: &TrackSource) -> &str {
    match source {
        TrackSource::YouTube => "YouTube",
        TrackSource::SoundCloud => "SoundCloud",
        TrackSource::Bandcamp => "Bandcamp",
        TrackSource::Twitch => "Twitch",
        TrackSource::Vimeo => "Vimeo",
        _ => "Other",
    }
}

fn clock(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn compact(value: &str, limit: usize) -> String {
    if value.trim().is_empty() {
        return "—".to_owned();
    }
    if value.encode_utf16().count() <= limit {
        return value.to_owned();
    }
    let mut remaining = limit.saturating_sub(1);
    let mut output = String::new();
    for character in value.chars() {
        if character.len_utf16() > remaining {
            break;
        }
        remaining -= character.len_utf16();
        output.push(character);
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gloamwire::model::UserId;
    use sonoryn::media::{RequestedBy, TrackId, TrackMetadata, TrackRequest};

    use super::*;

    fn track() -> Track {
        Track {
            id: TrackId::new(1),
            request: TrackRequest::new("song").expect("request"),
            source: TrackSource::YouTube,
            requested_by: RequestedBy::new(UserId::new(7)),
            metadata: TrackMetadata {
                title: "Song".to_owned(),
                artist: Some("Artist".to_owned()),
                duration: Some(Duration::from_secs(65)),
                artwork_url: Some("https://example.test/art.jpg".to_owned()),
                webpage_url: "https://example.test/song".to_owned(),
            },
            locator: "https://example.test/song".to_owned(),
        }
    }

    #[test]
    fn track_cards_show_actual_metadata_and_queue_position() {
        let embed = track_card("Added to queue", &track(), Some(2));
        assert_eq!(embed.title.as_deref(), Some("Song"));
        assert_eq!(embed.description.as_deref(), Some("Artist"));
        assert_eq!(embed.url.as_deref(), Some("https://example.test/song"));
        assert_eq!(embed.fields[0].value, "1:05");
        assert_eq!(embed.fields[3].value, "2");
        assert!(embed.thumbnail.is_some());
    }

    #[test]
    fn queue_summary_counts_unknown_durations_separately() {
        let known = track();
        let mut unknown = track();
        unknown.metadata.duration = None;
        let snapshot = PlayerSnapshot {
            now_playing: None,
            queue: vec![known, unknown],
        };
        let embed = queue_card("Queue", "Next up".to_owned(), &snapshot, LoopMode::Queue);
        assert_eq!(embed.fields[0].value, "2");
        assert_eq!(embed.fields[1].value, "1:05 + 1 unknown");
        assert_eq!(embed.fields[2].value, "Queue");
    }

    #[test]
    fn limits_use_utf16_and_reject_non_http_links() {
        let embed = notice("🎵".repeat(300), "x".repeat(5000));
        assert!(embed.title.expect("title").encode_utf16().count() <= 256);
        assert_eq!(
            embed
                .description
                .expect("description")
                .encode_utf16()
                .count(),
            4096
        );
        assert!(public_url("file:///tmp/secret").is_none());
    }
}
