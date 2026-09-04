use gloam_commands::prelude::*;
use gloamwire::model::{GuildId, UserId};
use sonoryn::player::LoopMode;
use tokio::sync::oneshot;

use crate::{
    gateway_control::{GatewayControl, PlaybackAction, PlaybackControlResult},
    state::AppState,
};

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![shuffle, loop_mode]
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
                ctx.reply_ephemeral("That loop mode is not supported.").await?;
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

fn loop_mode_name(mode: LoopMode) -> &'static str {
    match mode {
        LoopMode::Off => "off",
        LoopMode::Track => "track",
        LoopMode::Queue => "queue",
    }
}

async fn require_queue_control_context(ctx: &Context<AppState>) -> Result<Option<GuildId>> {
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
        ctx.reply_ephemeral("Join my voice channel first to edit the queue.")
            .await?;
        return Ok(None);
    };

    let (response, result) = oneshot::channel();
    if ctx
        .data()
        .gateway_control
        .send(GatewayControl::Playback {
            guild_id,
            channel_id,
            action: PlaybackAction::CheckContext,
            response,
        })
        .await
        .is_err()
    {
        ctx.reply_ephemeral("The Gateway control loop is unavailable.")
            .await?;
        return Ok(None);
    }

    match result.await.unwrap_or_else(|_| {
        PlaybackControlResult::Failed(
            "The voice worker ended before returning a playback result.".to_owned(),
        )
    }) {
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
    use sonoryn::player::LoopMode;

    use super::loop_mode_name;

    #[test]
    fn loop_mode_names_are_stable() {
        assert_eq!(loop_mode_name(LoopMode::Off), "off");
        assert_eq!(loop_mode_name(LoopMode::Track), "track");
        assert_eq!(loop_mode_name(LoopMode::Queue), "queue");
    }
}
