use std::time::Duration;

use gloam_commands::prelude::*;
use gloamwire::model::{GuildId, UserId};
use sonoryn::media::{RequestedBy, Track, TrackRequest};
use tokio::sync::oneshot;

use crate::{
    gateway_control::{GatewayControl, SkipTrackResult, TrackEnqueueResult, VoiceJoinResult},
    player::{PlaybackState, PlayerSnapshot},
    state::AppState,
};

const QUEUE_PREVIEW_LIMIT: usize = 10;
const QUEUE_CONTENT_LIMIT: usize = 1_800;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![play, skip, queue, nowplaying]
}

#[command(description = "Play or queue a track", guild_only)]
pub(crate) async fn play(
    ctx: Context<AppState>,
    #[description = "Song name or URL"]
    #[min_length = 1]
    #[max_length = 512]
    query: String,
) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };
    let Some(user_id) = invoking_user_id(ctx.interaction()) else {
        ctx.reply_ephemeral("I could not determine the invoking user.")
            .await?;
        return Ok(());
    };
    let Some(channel_id) = invoking_voice_channel(&ctx, guild_id, user_id).await else {
        ctx.reply_ephemeral("Join a voice channel first, then run `/play`.")
            .await?;
        return Ok(());
    };

    let request = match TrackRequest::new(query) {
        Ok(request) => request,
        Err(message) => {
            ctx.reply_ephemeral(message).await?;
            return Ok(());
        }
    };

    ctx.defer().await?;
    let resolved = match ctx.data().resolver.resolve(&request).await {
        Ok(resolved) => resolved,
        Err(error) => {
            ctx.reply(format!("I couldn't resolve that track: {error}"))
                .await?;
            return Ok(());
        }
    };

    let track = Track::from_resolved(
        ctx.data().players.next_track_id(),
        request,
        RequestedBy::new(user_id),
        resolved,
    );

    if let Err(message) = ensure_voice(&ctx, guild_id, channel_id).await {
        ctx.reply(message).await?;
        return Ok(());
    }

    let title = track.metadata.title.clone();
    let (response, result) = oneshot::channel();
    if ctx
        .data()
        .gateway_control
        .send(GatewayControl::EnqueueTrack {
            guild_id,
            track,
            response,
        })
        .await
        .is_err()
    {
        ctx.reply("The Gateway control loop is unavailable.")
            .await?;
        return Ok(());
    }

    let message = match result.await {
        Ok(TrackEnqueueResult::Accepted { position: 0 }) => {
            format!("▶️ Loading **{title}**.")
        }
        Ok(TrackEnqueueResult::Accepted { position }) => {
            format!("➕ Queued **{title}** at position **{position}**.")
        }
        Ok(TrackEnqueueResult::NotConnected) => {
            "The voice session ended before I could queue the track.".to_owned()
        }
        Ok(TrackEnqueueResult::Failed(error)) => error,
        Err(_) => "The guild player ended before accepting the track.".to_owned(),
    };
    ctx.reply(message).await?;
    Ok(())
}

#[command(description = "Skip the current track", guild_only)]
pub(crate) async fn skip(ctx: Context<AppState>) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };

    ctx.defer().await?;
    let (response, result) = oneshot::channel();
    if ctx
        .data()
        .gateway_control
        .send(GatewayControl::SkipTrack { guild_id, response })
        .await
        .is_err()
    {
        ctx.reply("The Gateway control loop is unavailable.")
            .await?;
        return Ok(());
    }

    let message = match result.await {
        Ok(SkipTrackResult::Skipped { title }) => format!("⏭️ Skipped **{title}**."),
        Ok(SkipTrackResult::NothingPlaying) => "Nothing is playing right now.".to_owned(),
        Ok(SkipTrackResult::NotConnected) => "I'm not connected to voice here.".to_owned(),
        Ok(SkipTrackResult::Failed(error)) => error,
        Err(_) => "The guild player ended before returning a skip result.".to_owned(),
    };
    ctx.reply(message).await?;
    Ok(())
}

#[command(description = "Show the current music queue", guild_only)]
pub(crate) async fn queue(ctx: Context<AppState>) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };

    let Some(snapshot) = ctx.data().players.snapshot(guild_id).await else {
        ctx.reply("There is no active player in this server.")
            .await?;
        return Ok(());
    };

    ctx.reply(render_queue(&snapshot)).await?;
    Ok(())
}

#[command(description = "Show the current track", guild_only)]
pub(crate) async fn nowplaying(ctx: Context<AppState>) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };

    let Some(snapshot) = ctx.data().players.snapshot(guild_id).await else {
        ctx.reply("Nothing is playing right now.").await?;
        return Ok(());
    };
    let Some(track) = snapshot.current else {
        ctx.reply("Nothing is playing right now.").await?;
        return Ok(());
    };

    let state = playback_label(snapshot.state);
    let artist = track
        .metadata
        .artist
        .as_deref()
        .map(|artist| format!(" — {artist}"))
        .unwrap_or_default();
    let duration = track
        .metadata
        .duration
        .map(format_duration)
        .unwrap_or_else(|| "unknown duration".to_owned());

    ctx.reply(format!(
        "🎵 **{}**{}\nState: **{}** · Duration: **{}** · Requested by <@{}>\n{}",
        track.metadata.title,
        artist,
        state,
        duration,
        track.requested_by.user_id.get(),
        track.metadata.webpage_url,
    ))
    .await?;
    Ok(())
}

async fn ensure_voice(
    ctx: &Context<AppState>,
    guild_id: GuildId,
    channel_id: gloamwire::model::ChannelId,
) -> std::result::Result<(), String> {
    let (response, result) = oneshot::channel();
    ctx.data()
        .gateway_control
        .send(GatewayControl::JoinVoice {
            guild_id,
            channel_id,
            response,
        })
        .await
        .map_err(|_| "The Gateway control loop is unavailable.".to_owned())?;

    match result.await {
        Ok(VoiceJoinResult::Joined {
            channel_id: connected,
        }) => joined_channel_result(connected, channel_id),
        Ok(VoiceJoinResult::AlreadyConnected {
            channel_id: connected,
        }) => joined_channel_result(connected, channel_id),
        Ok(VoiceJoinResult::InProgress { channel_id }) => Err(format!(
            "A voice connection to <#{}> is already starting. Try `/play` again once it connects.",
            channel_id.get()
        )),
        Ok(VoiceJoinResult::Cancelled) => Err("The voice join was cancelled.".to_owned()),
        Ok(VoiceJoinResult::Failed(error)) => Err(error),
        Err(_) => Err("The voice join task ended before returning a result.".to_owned()),
    }
}

fn joined_channel_result(
    connected: gloamwire::model::ChannelId,
    requested: gloamwire::model::ChannelId,
) -> std::result::Result<(), String> {
    if connected == requested {
        Ok(())
    } else {
        Err(format!(
            "I'm already active in <#{}>. Join that channel to control playback.",
            connected.get()
        ))
    }
}

async fn invoking_voice_channel(
    ctx: &Context<AppState>,
    guild_id: GuildId,
    user_id: UserId,
) -> Option<gloamwire::model::ChannelId> {
    let cache = ctx.data().cache.read().await;
    cache
        .voice_state(guild_id, user_id)
        .and_then(|state| state.channel_id)
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
    let mut output = String::new();
    if let Some(current) = &snapshot.current {
        output.push_str(&format!(
            "**Now {}:** {}\n",
            playback_label(snapshot.state),
            current.metadata.title
        ));
    } else {
        output.push_str("**Now:** idle\n");
    }

    if snapshot.queue.is_empty() {
        output.push_str("\nThe queue is empty.");
        return output;
    }

    output.push_str("\n**Up next:**\n");
    for (index, track) in snapshot.queue.iter().take(QUEUE_PREVIEW_LIMIT).enumerate() {
        let line = format!("{}. {}\n", index + 1, track.metadata.title);
        if output.len() + line.len() > QUEUE_CONTENT_LIMIT {
            output.push('…');
            break;
        }
        output.push_str(&line);
    }

    if snapshot.queue.len() > QUEUE_PREVIEW_LIMIT {
        output.push_str(&format!(
            "…and {} more.",
            snapshot.queue.len() - QUEUE_PREVIEW_LIMIT
        ));
    }
    output
}

fn playback_label(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Idle => "idle",
        PlaybackState::Loading => "loading",
        PlaybackState::Playing => "playing",
    }
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gloamwire::model::ChannelId;

    use super::{format_duration, joined_channel_result};

    #[test]
    fn formats_track_durations() {
        assert_eq!(format_duration(Duration::from_secs(65)), "1:05");
        assert_eq!(format_duration(Duration::from_secs(3_661)), "1:01:01");
    }

    #[test]
    fn join_result_rejects_a_different_connected_channel() {
        assert!(joined_channel_result(ChannelId::new(2), ChannelId::new(2)).is_ok());
        assert!(joined_channel_result(ChannelId::new(2), ChannelId::new(3)).is_err());
    }
}
