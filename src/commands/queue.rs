use gloam_commands::prelude::*;
use gloamwire::model::{GuildId, UserId};
use tokio::sync::oneshot;

use crate::{
    gateway_control::{GatewayControl, PlaybackAction, PlaybackControlResult},
    state::AppState,
};

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![shuffle]
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
