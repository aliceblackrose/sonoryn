use std::fmt::Write;

use gloam_commands::prelude::*;
use gloamwire::model::{ChannelId, GuildId, UserId};
use sonoryn::{
    media::{RequestedBy, Track, TrackRequest},
    player::PlayerSnapshot,
};
use tokio::sync::oneshot;

use crate::{
    gateway_control::{GatewayControl, PlaybackAction, PlaybackControlResult, VoiceJoinResult},
    state::AppState,
};

const QUEUE_PREVIEW_LIMIT: usize = 10;
const TRACK_TITLE_LIMIT: usize = 96;
const ERROR_MESSAGE_LIMIT: usize = 240;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![play, skip, pause, resume, stop, queue, nowplaying]
}

#[command(description = "Play a song or add it to the queue", guild_only)]
pub(crate) async fn play(
    ctx: Context<AppState>,
    #[description = "Song URL or search query"]
    #[min_length = 1]
    #[max_length = 200]
    query: String,
) -> Result<()> {
    let interaction = ctx.interaction();
    let Some(guild_id) = interaction.guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };
    let Some(user_id) = invoking_user_id(interaction) else {
        ctx.reply_ephemeral("I could not determine the invoking user.")
            .await?;
        return Ok(());
    };
    if current_voice_channel(ctx.data(), guild_id, user_id)
        .await
        .is_none()
    {
        ctx.reply_ephemeral("Join a voice channel first, then run `/play`.")
            .await?;
        return Ok(());
    }

    let request = match TrackRequest::new(query) {
        Ok(request) => request,
        Err(message) => {
            ctx.reply_ephemeral(message).await?;
            return Ok(());
        }
    };

    ctx.defer_ephemeral().await?;
    let resolved = match ctx.data().resolver.resolve(&request).await {
        Ok(resolved) => resolved,
        Err(error) => {
            let message = escape_markdown(&truncate_chars(&error.to_string(), ERROR_MESSAGE_LIMIT));
            ctx.reply_ephemeral(format!("I could not resolve that track: {message}"))
                .await?;
            return Ok(());
        }
    };

    let Some(channel_id) = current_voice_channel(ctx.data(), guild_id, user_id).await else {
        ctx.reply_ephemeral("You left voice while I was resolving that track.")
            .await?;
        return Ok(());
    };

    match ensure_voice(ctx.data(), guild_id, channel_id).await {
        VoiceJoinResult::Joined { .. } => {}
        VoiceJoinResult::AlreadyConnected {
            channel_id: connected,
        } if connected == channel_id => {}
        VoiceJoinResult::AlreadyConnected {
            channel_id: connected,
        } => {
            ctx.reply_ephemeral(format!(
                "I am already connected to <#{}>. Join that channel to control playback.",
                connected.get()
            ))
            .await?;
            return Ok(());
        }
        VoiceJoinResult::InProgress { channel_id } => {
            ctx.reply_ephemeral(format!(
                "A voice join for <#{}> is already in progress. Try `/play` again once it connects.",
                channel_id.get()
            ))
            .await?;
            return Ok(());
        }
        VoiceJoinResult::Cancelled => {
            ctx.reply_ephemeral("The voice join was cancelled.").await?;
            return Ok(());
        }
        VoiceJoinResult::Failed(error) => {
            ctx.reply_ephemeral(error).await?;
            return Ok(());
        }
    }

    let (track, queue_position) = {
        let mut players = ctx.data().player_manager.write().await;
        players.enqueue_resolved(guild_id, request, RequestedBy::new(user_id), resolved)
    };

    match playback_control(ctx.data(), guild_id, channel_id, PlaybackAction::Wake).await {
        PlaybackControlResult::Accepted => {
            let snapshot = {
                let players = ctx.data().player_manager.read().await;
                players.snapshot(guild_id)
            };
            if snapshot
                .now_playing
                .as_ref()
                .is_some_and(|current| current.id == track.id)
            {
                ctx.reply_ephemeral(format!("Playing {}", format_track(&track)))
                    .await?;
            } else {
                ctx.reply_ephemeral(format!(
                    "Queued {} at position {queue_position}.",
                    format_track(&track)
                ))
                .await?;
            }
        }
        PlaybackControlResult::NotConnected => {
            ctx.reply_ephemeral("The voice worker disconnected before playback could start.")
                .await?;
        }
        PlaybackControlResult::WrongVoiceChannel { channel_id } => {
            ctx.reply_ephemeral(format!(
                "I am connected to <#{}>. Join that channel to control playback.",
                channel_id.get()
            ))
            .await?;
        }
        PlaybackControlResult::NothingPlaying => {
            ctx.reply_ephemeral("The track was queued, but the player did not start it.")
                .await?;
        }
        PlaybackControlResult::AlreadyPaused | PlaybackControlResult::AlreadyPlaying => {
            ctx.reply_ephemeral(format!("Queued {}.", format_track(&track)))
                .await?;
        }
        PlaybackControlResult::Failed(error) => {
            ctx.reply_ephemeral(error).await?;
        }
    }
    Ok(())
}

#[command(description = "Skip the current track", guild_only)]
pub(crate) async fn skip(ctx: Context<AppState>) -> Result<()> {
    run_simple_control(
        &ctx,
        PlaybackAction::Skip,
        "Skipped the current track.",
        "Nothing is playing right now.",
    )
    .await
}

#[command(description = "Pause the current track", guild_only)]
pub(crate) async fn pause(ctx: Context<AppState>) -> Result<()> {
    run_simple_control(
        &ctx,
        PlaybackAction::Pause,
        "Paused playback.",
        "Nothing is playing right now.",
    )
    .await
}

#[command(description = "Resume a paused track", guild_only)]
pub(crate) async fn resume(ctx: Context<AppState>) -> Result<()> {
    run_simple_control(
        &ctx,
        PlaybackAction::Resume,
        "Resumed playback.",
        "Nothing is playing right now.",
    )
    .await
}

#[command(description = "Stop playback and clear the queue", guild_only)]
pub(crate) async fn stop(ctx: Context<AppState>) -> Result<()> {
    run_simple_control(
        &ctx,
        PlaybackAction::Stop,
        "Stopped playback and cleared the queue.",
        "Nothing is playing and the queue is already empty.",
    )
    .await
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

#[command(
    description = "Show the track Sonoryn is currently playing",
    guild_only
)]
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

async fn run_simple_control(
    ctx: &Context<AppState>,
    action: PlaybackAction,
    accepted: &str,
    idle: &str,
) -> Result<()> {
    let interaction = ctx.interaction();
    let Some(guild_id) = interaction.guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };
    let Some(user_id) = invoking_user_id(interaction) else {
        ctx.reply_ephemeral("I could not determine the invoking user.")
            .await?;
        return Ok(());
    };
    let Some(channel_id) = current_voice_channel(ctx.data(), guild_id, user_id).await else {
        ctx.reply_ephemeral("Join my voice channel first to control playback.")
            .await?;
        return Ok(());
    };

    let message = match playback_control(ctx.data(), guild_id, channel_id, action).await {
        PlaybackControlResult::Accepted => accepted.to_owned(),
        PlaybackControlResult::NothingPlaying => idle.to_owned(),
        PlaybackControlResult::NotConnected => "I am not connected to voice here.".to_owned(),
        PlaybackControlResult::WrongVoiceChannel { channel_id } => format!(
            "I am connected to <#{}>. Join that channel to control playback.",
            channel_id.get()
        ),
        PlaybackControlResult::AlreadyPaused => "Playback is already paused.".to_owned(),
        PlaybackControlResult::AlreadyPlaying => "Playback is already running.".to_owned(),
        PlaybackControlResult::Failed(error) => error,
    };
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

async fn current_voice_channel(
    data: &AppState,
    guild_id: GuildId,
    user_id: UserId,
) -> Option<ChannelId> {
    let cache = data.cache.read().await;
    cache
        .voice_state(guild_id, user_id)
        .and_then(|state| state.channel_id)
}

async fn ensure_voice(
    data: &AppState,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> VoiceJoinResult {
    let (response, result) = oneshot::channel();
    if data
        .gateway_control
        .send(GatewayControl::JoinVoice {
            guild_id,
            channel_id,
            response,
        })
        .await
        .is_err()
    {
        return VoiceJoinResult::Failed("The Gateway control loop is unavailable.".to_owned());
    }

    result.await.unwrap_or_else(|_| {
        VoiceJoinResult::Failed("The voice join ended before returning a result.".to_owned())
    })
}

async fn playback_control(
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
        assert_eq!(
            render_queue(&PlayerSnapshot::default()),
            "The queue is empty."
        );
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
