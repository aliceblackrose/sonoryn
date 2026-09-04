use std::time::Duration;

use gloam_commands::prelude::*;
use gloamwire::model::{ChannelId, GuildId, UserId};
use tokio::sync::oneshot;

use crate::{
    gateway_control::{GatewayControl, PlaybackAction, PlaybackControlResult},
    state::AppState,
};

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![seek, volume]
}

#[command(
    description = "Seek the current track to a playback position",
    guild_only
)]
pub(crate) async fn seek(
    ctx: Context<AppState>,
    #[description = "Position as seconds, M:SS, or H:MM:SS"]
    #[min_length = 1]
    #[max_length = 20]
    position: String,
) -> Result<()> {
    let Some(position) = parse_seek_position(&position) else {
        ctx.reply_ephemeral("Use a seek position like `90`, `1:30`, or `1:02:03`.")
            .await?;
        return Ok(());
    };
    let Ok(position_millis) = u64::try_from(position.as_millis()) else {
        ctx.reply_ephemeral("That seek position is too large.")
            .await?;
        return Ok(());
    };
    let Some((guild_id, channel_id)) = require_voice_channel(&ctx).await? else {
        return Ok(());
    };

    let message = match playback_action(
        ctx.data(),
        guild_id,
        channel_id,
        PlaybackAction::Seek { position_millis },
    )
    .await
    {
        PlaybackControlResult::Accepted => {
            format!("Seeked to **{}**.", format_position(position))
        }
        PlaybackControlResult::NothingPlaying => "Nothing is playing right now.".to_owned(),
        PlaybackControlResult::NotConnected => "I am not connected to voice here.".to_owned(),
        PlaybackControlResult::WrongVoiceChannel { channel_id } => format!(
            "I am connected to <#{}>. Join that channel to control playback.",
            channel_id.get()
        ),
        PlaybackControlResult::AlreadyPaused | PlaybackControlResult::AlreadyPlaying => {
            "The voice worker rejected the seek request.".to_owned()
        }
        PlaybackControlResult::Failed(error) => error,
    };
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

#[command(description = "Set playback volume from 0 to 100 percent", guild_only)]
pub(crate) async fn volume(
    ctx: Context<AppState>,
    #[description = "Volume percentage"]
    #[min = 0]
    #[max = 100]
    percent: i64,
) -> Result<()> {
    let Ok(percent) = u8::try_from(percent) else {
        ctx.reply_ephemeral("Volume must be between 0 and 100 percent.")
            .await?;
        return Ok(());
    };
    let Some((guild_id, channel_id)) = require_voice_channel(&ctx).await? else {
        return Ok(());
    };

    let message = match playback_action(
        ctx.data(),
        guild_id,
        channel_id,
        PlaybackAction::Volume { percent },
    )
    .await
    {
        PlaybackControlResult::Accepted => format!("Volume set to **{percent}%**."),
        PlaybackControlResult::NothingPlaying => {
            format!("Volume set to **{percent}%** for the next track.")
        }
        PlaybackControlResult::NotConnected => "I am not connected to voice here.".to_owned(),
        PlaybackControlResult::WrongVoiceChannel { channel_id } => format!(
            "I am connected to <#{}>. Join that channel to control playback.",
            channel_id.get()
        ),
        PlaybackControlResult::AlreadyPaused | PlaybackControlResult::AlreadyPlaying => {
            "The voice worker rejected the volume request.".to_owned()
        }
        PlaybackControlResult::Failed(error) => error,
    };
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

async fn require_voice_channel(ctx: &Context<AppState>) -> Result<Option<(GuildId, ChannelId)>> {
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
        ctx.reply_ephemeral("Join my voice channel first to control playback.")
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

fn parse_seek_position(value: &str) -> Option<Duration> {
    let parts = value.trim().split(':').collect::<Vec<_>>();
    let total_seconds = match parts.as_slice() {
        [seconds] => seconds.parse::<u64>().ok()?,
        [minutes, seconds] => {
            let minutes = minutes.parse::<u64>().ok()?;
            let seconds = seconds.parse::<u64>().ok()?;
            if seconds >= 60 {
                return None;
            }
            minutes.checked_mul(60)?.checked_add(seconds)?
        }
        [hours, minutes, seconds] => {
            let hours = hours.parse::<u64>().ok()?;
            let minutes = minutes.parse::<u64>().ok()?;
            let seconds = seconds.parse::<u64>().ok()?;
            if minutes >= 60 || seconds >= 60 {
                return None;
            }
            hours
                .checked_mul(3_600)?
                .checked_add(minutes.checked_mul(60)?)?
                .checked_add(seconds)?
        }
        _ => return None,
    };
    total_seconds.checked_mul(1_000).map(Duration::from_millis)
}

fn format_position(position: Duration) -> String {
    let total_seconds = position.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{format_position, parse_seek_position};

    #[test]
    fn seek_parser_accepts_seconds_and_clock_forms() {
        assert_eq!(parse_seek_position("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_seek_position("1:30"), Some(Duration::from_secs(90)));
        assert_eq!(
            parse_seek_position("1:02:03"),
            Some(Duration::from_secs(3_723))
        );
    }

    #[test]
    fn seek_parser_rejects_invalid_clock_fields() {
        assert_eq!(parse_seek_position("1:75"), None);
        assert_eq!(parse_seek_position("1:60:00"), None);
        assert_eq!(parse_seek_position("1:2:70"), None);
        assert_eq!(parse_seek_position("nope"), None);
    }

    #[test]
    fn seek_positions_render_consistently() {
        assert_eq!(format_position(Duration::from_secs(90)), "1:30");
        assert_eq!(format_position(Duration::from_secs(3_723)), "1:02:03");
    }
}
