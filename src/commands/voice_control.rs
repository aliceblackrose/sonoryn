use gloam_commands::prelude::*;
use gloamwire::model::UserId;
use tokio::sync::oneshot;

use crate::{
    gateway_control::{GatewayControl, VoiceJoinResult, VoiceLeaveResult},
    state::AppState,
};

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![join, leave]
}

#[command(description = "Join your current voice channel", guild_only)]
pub(crate) async fn join(ctx: Context<AppState>) -> Result<()> {
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

    let channel_id = {
        let cache = ctx.data().cache.read().await;
        cache
            .voice_state(guild_id, user_id)
            .and_then(|state| state.channel_id)
    };
    let Some(channel_id) = channel_id else {
        ctx.reply_ephemeral("Join a voice channel first, then run `/join`.")
            .await?;
        return Ok(());
    };

    ctx.defer_ephemeral().await?;
    let (response, result) = oneshot::channel();
    if ctx
        .data()
        .gateway_control
        .send(GatewayControl::JoinVoice {
            guild_id,
            channel_id,
            response,
        })
        .await
        .is_err()
    {
        ctx.reply_ephemeral("The Gateway control loop is unavailable.")
            .await?;
        return Ok(());
    }

    let message = match result.await {
        Ok(VoiceJoinResult::Joined { channel_id }) => {
            format!(
                "Connected to <#{}> with DAVE/E2EE enabled.",
                channel_id.get()
            )
        }
        Ok(VoiceJoinResult::AlreadyConnected { channel_id }) => {
            format!("I am already connected to <#{}>.", channel_id.get())
        }
        Ok(VoiceJoinResult::InProgress { channel_id }) => {
            format!(
                "A voice join for <#{}> is already in progress.",
                channel_id.get()
            )
        }
        Ok(VoiceJoinResult::Cancelled) => "The voice join was cancelled.".to_owned(),
        Ok(VoiceJoinResult::Failed(error)) => error,
        Err(_) => "The voice join task ended before returning a result.".to_owned(),
    };
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

#[command(description = "Disconnect Sonoryn from voice", guild_only)]
pub(crate) async fn leave(ctx: Context<AppState>) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        ctx.reply_ephemeral("This command can only be used in a server.")
            .await?;
        return Ok(());
    };

    ctx.defer_ephemeral().await?;
    let (response, result) = oneshot::channel();
    if ctx
        .data()
        .gateway_control
        .send(GatewayControl::LeaveVoice { guild_id, response })
        .await
        .is_err()
    {
        ctx.reply_ephemeral("The Gateway control loop is unavailable.")
            .await?;
        return Ok(());
    }

    let message = match result.await {
        Ok(VoiceLeaveResult::Left { channel_id }) => {
            format!("Disconnected from <#{}>.", channel_id.get())
        }
        Ok(VoiceLeaveResult::CancelledJoin { channel_id }) => {
            format!("Cancelled the pending join for <#{}>.", channel_id.get())
        }
        Ok(VoiceLeaveResult::NotConnected) => "I am not connected to voice here.".to_owned(),
        Ok(VoiceLeaveResult::Failed(error)) => error,
        Err(_) => "The voice leave task ended before returning a result.".to_owned(),
    };
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

fn invoking_user_id(interaction: &gloamwire::model::Interaction) -> Option<UserId> {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.user.as_ref())
        .map(|user| user.id)
        .or_else(|| interaction.user.as_ref().map(|user| user.id))
}
