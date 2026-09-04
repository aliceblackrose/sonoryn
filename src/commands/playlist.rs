use gloam_commands::prelude::*;
use gloamwire::model::{ChannelId, GuildId, UserId};
use sonoryn::media::{MAX_PLAYLIST_ITEMS, RequestedBy, TrackRequest};
use tokio::sync::oneshot;

use crate::{
    gateway_control::{GatewayControl, PlaybackAction, PlaybackControlResult, VoiceJoinResult},
    state::AppState,
};

const DEFAULT_PLAYLIST_ITEMS: usize = 10;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![playlist]
}

#[command(description = "Add a bounded playlist to the music queue", guild_only)]
pub(crate) async fn playlist(
    ctx: Context<AppState>,
    #[description = "Playlist URL"]
    #[min_length = 1]
    #[max_length = 500]
    url: String,
    #[description = "Maximum tracks to add"]
    #[min = 1]
    #[max = 25]
    limit: Option<i64>,
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
    let Some(_) = current_voice_channel(ctx.data(), guild_id, user_id).await else {
        ctx.reply_ephemeral("Join a voice channel first, then run `/playlist`.")
            .await?;
        return Ok(());
    };
    if !is_http_url(&url) {
        ctx.reply_ephemeral("`/playlist` requires an HTTP or HTTPS playlist URL.")
            .await?;
        return Ok(());
    }
    let Some(limit) = playlist_limit(limit) else {
        ctx.reply_ephemeral("Playlist limit must be between 1 and 25 tracks.")
            .await?;
        return Ok(());
    };
    let request = match TrackRequest::new(url) {
        Ok(request) => request,
        Err(message) => {
            ctx.reply_ephemeral(message).await?;
            return Ok(());
        }
    };

    ctx.defer_ephemeral().await?;
    let resolved = match ctx.data().resolver.resolve_playlist(&request, limit).await {
        Ok(resolved) => resolved,
        Err(error) => {
            ctx.reply_ephemeral(format!("I could not resolve that playlist: {error}"))
                .await?;
            return Ok(());
        }
    };
    if resolved.is_empty() {
        ctx.reply_ephemeral("That playlist did not contain any playable tracks.")
            .await?;
        return Ok(());
    }

    let Some(channel_id) = current_voice_channel(ctx.data(), guild_id, user_id).await else {
        ctx.reply_ephemeral("You left voice while I was resolving that playlist.")
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
                "I am already connected to <#{}>. Join that channel to add a playlist.",
                connected.get()
            ))
            .await?;
            return Ok(());
        }
        VoiceJoinResult::InProgress { channel_id } => {
            ctx.reply_ephemeral(format!(
                "A voice join for <#{}> is already in progress. Try again once it connects.",
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

    let added = resolved.len();
    let first_position = {
        let mut players = ctx.data().player_manager.write().await;
        let mut first_position = None;
        for track in resolved {
            let item_request = TrackRequest::new(track.metadata.webpage_url.clone())
                .unwrap_or_else(|_| request.clone());
            let (_, position) =
                players.enqueue_resolved(guild_id, item_request, RequestedBy::new(user_id), track);
            first_position.get_or_insert(position);
        }
        first_position.unwrap_or(1)
    };

    let message = match playback_control(ctx.data(), guild_id, channel_id, PlaybackAction::Wake)
        .await
    {
        PlaybackControlResult::Accepted => format!(
            "Added **{added}** track{} from the playlist; first queue position was **{first_position}**.",
            if added == 1 { "" } else { "s" }
        ),
        PlaybackControlResult::NotConnected => {
            "The playlist was queued, but the voice worker disconnected before playback started."
                .to_owned()
        }
        PlaybackControlResult::WrongVoiceChannel { channel_id } => format!(
            "The playlist was queued, but I am connected to <#{}>.",
            channel_id.get()
        ),
        PlaybackControlResult::NothingPlaying => {
            "The playlist was queued, but the player did not start it.".to_owned()
        }
        PlaybackControlResult::AlreadyPaused | PlaybackControlResult::AlreadyPlaying => {
            format!("Added **{added}** playlist tracks to the queue.")
        }
        PlaybackControlResult::Failed(error) => error,
    };
    ctx.reply_ephemeral(message).await?;
    Ok(())
}

fn playlist_limit(limit: Option<i64>) -> Option<usize> {
    let limit = limit.unwrap_or(DEFAULT_PLAYLIST_ITEMS as i64);
    let limit = usize::try_from(limit).ok()?;
    (1..=MAX_PLAYLIST_ITEMS).contains(&limit).then_some(limit)
}

fn is_http_url(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("https://") || value.starts_with("http://")
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

#[cfg(test)]
mod tests {
    use super::{is_http_url, playlist_limit};

    #[test]
    fn playlist_limits_are_bounded() {
        assert_eq!(playlist_limit(None), Some(10));
        assert_eq!(playlist_limit(Some(1)), Some(1));
        assert_eq!(playlist_limit(Some(25)), Some(25));
        assert_eq!(playlist_limit(Some(0)), None);
        assert_eq!(playlist_limit(Some(26)), None);
    }

    #[test]
    fn playlist_command_requires_http_urls() {
        assert!(is_http_url("https://example.test/list"));
        assert!(is_http_url(" http://example.test/list "));
        assert!(!is_http_url("artist playlist"));
    }
}
