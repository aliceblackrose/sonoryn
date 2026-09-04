use std::fmt::Write;

use gloam_commands::prelude::*;
use gloamwire::model::{ChannelId, GuildId, UserId};
use sonoryn::{
    media::Track,
    player::{LoopMode, PlayerSnapshot},
};
use tokio::sync::oneshot;

use crate::{
    gateway_control::{GatewayControl, PlaybackAction, PlaybackControlResult},
    state::AppState,
};

const QUEUE_PAGE_SIZE: usize = 10;
const HISTORY_PREVIEW_LIMIT: usize = 10;
const TRACK_TITLE_LIMIT: usize = 96;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![shuffle, loop_mode, queue_page, previous, history]
}

#[command(description = "Shuffle the queued tracks", guild_only)]
pub(crate) async fn shuffle(ctx: Context<AppState>) -> Result<()> {
    let Some(guild_id) = require_queue_control_context(&ctx).await? else {
        return Ok(());
    };

    let shuffled = {
        let mut players = ctx.data().player_manager.write().await;
        players.shuffle_queued(guild_id)
    };

    let message = match shuffled {
        0 => "The queue is empty.".to_owned(),
        1 => "There is only one queued track, so there is nothing to shuffle.".to_owned(),
        count => format!("Shuffled {count} queued tracks."),
    };
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

#[command(
    name = "loop",
    description = "Show or change the playback loop mode",
    guild_only
)]
pub(crate) async fn loop_mode(
    ctx: Context<AppState>,
    #[description = "Loop mode"]
    #[choice(name = "Off", value = "off")]
    #[choice(name = "Track", value = "track")]
    #[choice(name = "Queue", value = "queue")]
    mode: Option<String>,
) -> Result<()> {
    let Some(guild_id) = require_queue_control_context(&ctx).await? else {
        return Ok(());
    };

    let current = if let Some(mode) = mode {
        let mode = match mode.as_str() {
            "off" => LoopMode::Off,
            "track" => LoopMode::Track,
            "queue" => LoopMode::Queue,
            _ => {
                ctx.reply_ephemeral("That loop mode is not supported.")
                    .await?;
                return Ok(());
            }
        };
        let mut players = ctx.data().player_manager.write().await;
        players.set_loop_mode(guild_id, mode);
        mode
    } else {
        let players = ctx.data().player_manager.read().await;
        players.loop_mode(guild_id)
    };

    ctx.reply_ephemeral(format!("Loop mode: **{}**.", loop_mode_name(current)))
        .await?;
    Ok(())
}

#[command(
    name = "queue-page",
    description = "Show a page of the current music queue",
    guild_only
)]
pub(crate) async fn queue_page(
    ctx: Context<AppState>,
    #[description = "One-based queue page"]
    #[min = 1]
    page: Option<i64>,
) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };
    let page = page.unwrap_or(1);
    let Some(page_index) = page_index(page) else {
        ctx.reply_ephemeral("That queue page is invalid.").await?;
        return Ok(());
    };

    let snapshot = {
        let players = ctx.data().player_manager.read().await;
        players.snapshot(guild_id)
    };
    ctx.reply_ephemeral(render_queue_page(&snapshot, page_index))
        .await?;
    Ok(())
}

#[command(description = "Play the previous track from this guild's history", guild_only)]
pub(crate) async fn previous(ctx: Context<AppState>) -> Result<()> {
    let Some((guild_id, channel_id)) = require_voice_channel(&ctx, "control playback").await? else {
        return Ok(());
    };

    let message = match playback_action(
        ctx.data(),
        guild_id,
        channel_id,
        PlaybackAction::Previous,
    )
    .await
    {
        PlaybackControlResult::Accepted => "Playing the previous track.".to_owned(),
        PlaybackControlResult::NothingPlaying => "There is no previous track in history.".to_owned(),
        PlaybackControlResult::NotConnected => "I am not connected to voice here.".to_owned(),
        PlaybackControlResult::WrongVoiceChannel { channel_id } => format!(
            "I am connected to <#{}>. Join that channel to control playback.",
            channel_id.get()
        ),
        PlaybackControlResult::AlreadyPaused | PlaybackControlResult::AlreadyPlaying => {
            "The voice worker rejected the previous-track request.".to_owned()
        }
        PlaybackControlResult::Failed(error) => error,
    };
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

#[command(description = "Show recent playback history", guild_only)]
pub(crate) async fn history(ctx: Context<AppState>) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };

    let history = {
        let history = ctx.data().history_manager.read().await;
        history.snapshot(guild_id)
    };
    ctx.reply_ephemeral(render_history(&history)).await?;
    Ok(())
}

fn loop_mode_name(mode: LoopMode) -> &'static str {
    match mode {
        LoopMode::Off => "off",
        LoopMode::Track => "track",
        LoopMode::Queue => "queue",
    }
}

fn page_index(page: i64) -> Option<usize> {
    page.checked_sub(1)
        .and_then(|index| usize::try_from(index).ok())
}

fn render_queue_page(snapshot: &PlayerSnapshot, page_index: usize) -> String {
    if snapshot.is_idle() {
        return "The queue is empty.".to_owned();
    }

    let page_count = snapshot.queue.len().max(1).div_ceil(QUEUE_PAGE_SIZE);
    if page_index >= page_count {
        return format!(
            "Queue page {} does not exist. There {} {} page{}.",
            page_index + 1,
            if page_count == 1 { "is" } else { "are" },
            page_count,
            if page_count == 1 { "" } else { "s" }
        );
    }

    let mut output = String::new();
    if let Some(track) = &snapshot.now_playing {
        let _ = writeln!(output, "Now playing: {}", format_track(track));
        output.push('\n');
    }

    let start = page_index * QUEUE_PAGE_SIZE;
    let end = (start + QUEUE_PAGE_SIZE).min(snapshot.queue.len());
    let _ = writeln!(output, "Queue — page {}/{}:", page_index + 1, page_count);
    for (offset, track) in snapshot.queue[start..end].iter().enumerate() {
        let _ = writeln!(output, "{}. {}", start + offset + 1, format_track(track));
    }

    output.trim_end().to_owned()
}

fn render_history(history: &[Track]) -> String {
    if history.is_empty() {
        return "Playback history is empty.".to_owned();
    }

    let mut output = String::from("Recent history:\n");
    for (index, track) in history.iter().rev().take(HISTORY_PREVIEW_LIMIT).enumerate() {
        let _ = writeln!(output, "{}. {}", index + 1, format_track(track));
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

async fn require_queue_control_context(ctx: &Context<AppState>) -> Result<Option<GuildId>> {
    let Some((guild_id, channel_id)) = require_voice_channel(ctx, "edit the queue").await? else {
        return Ok(None);
    };

    match playback_action(
        ctx.data(),
        guild_id,
        channel_id,
        PlaybackAction::CheckContext,
    )
    .await
    {
        PlaybackControlResult::Accepted => Ok(Some(guild_id)),
        PlaybackControlResult::NotConnected => {
            ctx.reply_ephemeral("I am not connected to voice here.")
                .await?;
            Ok(None)
        }
        PlaybackControlResult::WrongVoiceChannel { channel_id } => {
            ctx.reply_ephemeral(format!(
                "I am connected to <#{}>. Join that channel to edit the queue.",
                channel_id.get()
            ))
            .await?;
            Ok(None)
        }
        PlaybackControlResult::Failed(error) => {
            ctx.reply_ephemeral(error).await?;
            Ok(None)
        }
        PlaybackControlResult::NothingPlaying
        | PlaybackControlResult::AlreadyPaused
        | PlaybackControlResult::AlreadyPlaying => {
            ctx.reply_ephemeral("The voice worker rejected the queue-control context.")
                .await?;
            Ok(None)
        }
    }
}

async fn require_voice_channel(
    ctx: &Context<AppState>,
    purpose: &str,
) -> Result<Option<(GuildId, ChannelId)>> {
    let interaction = ctx.interaction();
    let Some(guild_id) = interaction.guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(None);
    };
    let Some(user_id) = invoking_user_id(interaction) else {
        ctx.reply_ephemeral("I could not determine the invoking user.")
            .await?;
        return Ok(None);
    };
    let channel_id = {
        let cache = ctx.data().cache.read().await;
        cache
            .voice_state(guild_id, user_id)
            .and_then(|state| state.channel_id)
    };
    let Some(channel_id) = channel_id else {
        ctx.reply_ephemeral(format!("Join my voice channel first to {purpose}."))
            .await?;
        return Ok(None);
    };
    Ok(Some((guild_id, channel_id)))
}

async fn playback_action(
    data: &AppState,
    guild_id: GuildId,
    channel_id: ChannelId,
    action: PlaybackAction,
) -> PlaybackControlResult {
    let (response, result) = oneshot::channel();
    if data
        .gateway_control
        .send(GatewayControl::Playback {
            guild_id,
            channel_id,
            action,
            response,
        })
        .await
        .is_err()
    {
        return PlaybackControlResult::Failed(
            "The Gateway control loop is unavailable.".to_owned(),
        );
    }
    result.await.unwrap_or_else(|_| {
        PlaybackControlResult::Failed(
            "The voice worker ended before returning a playback result.".to_owned(),
        )
    })
}

fn invoking_user_id(interaction: &gloamwire::model::Interaction) -> Option<UserId> {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.user.as_ref())
        .map(|user| user.id)
        .or_else(|| interaction.user.as_ref().map(|user| user.id))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gloamwire::model::UserId;
    use sonoryn::{
        media::{
            RequestedBy, ResolvedTrack, Track, TrackId, TrackMetadata, TrackRequest, TrackSource,
        },
        player::{LoopMode, PlayerSnapshot},
    };

    use super::{loop_mode_name, page_index, render_history, render_queue_page};

    fn track(id: u64) -> Track {
        Track::from_resolved(
            TrackId::new(id),
            TrackRequest::new(format!("track {id}")).expect("request"),
            RequestedBy::new(UserId::new(7)),
            ResolvedTrack {
                source: TrackSource::YouTube,
                metadata: TrackMetadata {
                    title: format!("track {id}"),
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
    fn loop_mode_names_are_stable() {
        assert_eq!(loop_mode_name(LoopMode::Off), "off");
        assert_eq!(loop_mode_name(LoopMode::Track), "track");
        assert_eq!(loop_mode_name(LoopMode::Queue), "queue");
    }

    #[test]
    fn queue_page_uses_global_positions() {
        let snapshot = PlayerSnapshot {
            now_playing: Some(track(1)),
            queue: (2..=23).map(track).collect(),
        };
        let rendered = render_queue_page(&snapshot, 1);
        assert!(rendered.contains("Queue — page 2/3:"));
        assert!(rendered.contains("11. **track 12**"));
        assert!(rendered.contains("20. **track 21**"));
        assert!(!rendered.contains("21. **track 22**"));
    }

    #[test]
    fn queue_page_rejects_out_of_range_pages() {
        let snapshot = PlayerSnapshot {
            now_playing: None,
            queue: (1..=3).map(track).collect(),
        };
        assert_eq!(
            render_queue_page(&snapshot, 2),
            "Queue page 3 does not exist. There is 1 page."
        );
    }

    #[test]
    fn history_is_rendered_newest_first() {
        let rendered = render_history(&[track(1), track(2), track(3)]);
        let third = rendered.find("**track 3**").expect("third track");
        let first = rendered.find("**track 1**").expect("first track");
        assert!(third < first);
    }

    #[test]
    fn page_values_convert_to_zero_based_indices() {
        assert_eq!(page_index(1), Some(0));
        assert_eq!(page_index(2), Some(1));
        assert_eq!(page_index(0), None);
    }
}
