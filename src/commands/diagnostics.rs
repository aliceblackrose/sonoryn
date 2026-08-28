use gloam_commands::prelude::*;
use gloamwire::model::UserId;

use crate::state::AppState;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![ping, voice]
}

#[command(description = "Check whether Sonoryn is online")]
pub(crate) async fn ping(ctx: Context<AppState>) -> Result<()> {
    ctx.reply("Sonoryn is online.").await?;
    Ok(())
}

#[command(
    description = "Show the voice channel Sonoryn currently sees you in",
    guild_only
)]
pub(crate) async fn voice(ctx: Context<AppState>) -> Result<()> {
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

    match channel_id {
        Some(channel_id) => {
            ctx.reply_ephemeral(format!(
                "Your cached voice channel is <#{}>.",
                channel_id.get()
            ))
            .await?;
        }
        None => {
            ctx.reply_ephemeral("I do not currently see you in a voice channel.")
                .await?;
        }
    }

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
