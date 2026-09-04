use std::process::Output;

use gloam_commands::prelude::*;
use gloamwire::model::UserId;
use tokio::process::Command;

use crate::state::AppState;

const TOOL_VERSION_LIMIT: usize = 120;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![ping, health, voice]
}

#[command(description = "Check whether Sonoryn is online")]
pub(crate) async fn ping(ctx: Context<AppState>) -> Result<()> {
    ctx.reply("Sonoryn is online.").await?;
    Ok(())
}

#[command(description = "Check Sonoryn's runtime dependencies")]
pub(crate) async fn health(ctx: Context<AppState>) -> Result<()> {
    ctx.defer_ephemeral().await?;

    let (ytdlp, ffmpeg) = tokio::join!(
        tool_health("yt-dlp", &["--version"]),
        tool_health("ffmpeg", &["-version"]),
    );

    ctx.reply_ephemeral(format!(
        "**Sonoryn health**\nGateway: online\nyt-dlp: {ytdlp}\nFFmpeg: {ffmpeg}"
    ))
    .await?;
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

async fn tool_health(binary: &str, args: &[&str]) -> String {
    let output = match Command::new(binary).args(args).output().await {
        Ok(output) => output,
        Err(error) => return format!("unavailable ({})", compact(&error.to_string())),
    };

    if !output.status.success() {
        return format!("failed ({})", output.status);
    }

    first_output_line(&output).map_or_else(
        || "available".to_owned(),
        |version| format!("available — `{}`", escape_inline_code(&version)),
    )
}

fn first_output_line(output: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(compact)
}

fn compact(value: &str) -> String {
    let mut chars = value.chars();
    let mut compacted: String = chars.by_ref().take(TOOL_VERSION_LIMIT).collect();
    if chars.next().is_some() {
        compacted.push('…');
    }
    compacted
}

fn escape_inline_code(value: &str) -> String {
    value.replace('`', "ˋ").replace('@', "＠")
}

fn invoking_user_id(interaction: &gloamwire::model::Interaction) -> Option<UserId> {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.user.as_ref())
        .map(|user| user.id)
        .or_else(|| interaction.user.as_ref().map(|user| user.id))
}
