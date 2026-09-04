use std::time::Duration;

use gloam_commands::prelude::*;
use gloamwire::model::{
    ApplicationCommandInteractionValue, ChannelId, GuildId, Interaction, UserId,
};
use sonoryn::media::{RequestedBy, ResolvedTrack, Track, TrackRequest};
use tokio::{sync::oneshot, time::timeout};

use crate::{
    gateway_control::{GatewayControl, PlaybackAction, PlaybackControlResult, VoiceJoinResult},
    state::AppState,
};

const AUTOCOMPLETE_LIMIT: usize = 10;
const AUTOCOMPLETE_TIMEOUT: Duration = Duration::from_millis(2_200);
const AUTOCOMPLETE_CHOICE_NAME_LIMIT: usize = 100;
const TRACK_TITLE_LIMIT: usize = 96;
const ERROR_MESSAGE_LIMIT: usize = 240;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![play]
}

#[autocomplete]
pub(crate) async fn play_query_autocomplete(
    ctx: AutocompleteContext<AppState>,
) -> Result<Vec<AutocompleteChoice>> {
    let Some(ApplicationCommandInteractionValue::String(input)) = ctx.focused_value() else {
        return Ok(Vec::new());
    };
    let input = input.trim();
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let request = autocomplete_request(input);
    let tracks = match timeout(
        AUTOCOMPLETE_TIMEOUT,
        ctx.data()
            .resolver
            .resolve_playlist(&request, AUTOCOMPLETE_LIMIT),
    )
    .await
    {
        Ok(Ok(tracks)) => tracks,
        Ok(Err(_)) | Err(_) => return Ok(Vec::new()),
    };

    Ok(tracks
        .into_iter()
        .filter_map(autocomplete_choice)
        .take(AUTOCOMPLETE_LIMIT)
        .collect())
}

#[command(description = "Play a song or add it to the queue", guild_only)]
pub(crate) async fn play(
    ctx: Context<AppState>,
    #[description = "Song URL or search query"]
    #[autocomplete = play_query_autocomplete]
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

fn autocomplete_request(input: &str) -> TrackRequest {
    let target = if is_http_url(input) {
        input.to_owned()
    } else {
        format!("ytsearch{AUTOCOMPLETE_LIMIT}:{input}")
    };
    TrackRequest::new(target).expect("autocomplete input was checked as non-empty")
}

fn autocomplete_choice(track: ResolvedTrack) -> Option<AutocompleteChoice> {
    if track.metadata.webpage_url.chars().count() > 100 {
        return None;
    }

    let label = match track.metadata.artist.as_deref() {
        Some(artist) if !artist.trim().is_empty() => {
            format!("{} — {}", track.metadata.title, artist)
        }
        _ => track.metadata.title.clone(),
    };
    let label = truncate_chars(&label, AUTOCOMPLETE_CHOICE_NAME_LIMIT);
    Some(AutocompleteChoice::string(
        label,
        track.metadata.webpage_url,
    ))
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

fn invoking_user_id(interaction: &Interaction) -> Option<UserId> {
    interaction
        .member
        .as_ref()
        .and_then(|member| member.user.as_ref())
        .map(|user| user.id)
        .or_else(|| interaction.user.as_ref().map(|user| user.id))
}

fn format_track(track: &Track) -> String {
    let title = escape_markdown(&truncate_chars(&track.metadata.title, TRACK_TITLE_LIMIT));
    let title = format!("**{title}**");
    match track.metadata.artist.as_deref() {
        Some(artist) if !artist.trim().is_empty() => {
            let artist = escape_markdown(&truncate_chars(artist, TRACK_TITLE_LIMIT));
            format!("{title} — {artist}")
        }
        _ => title,
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    if limit == 0 {
        return String::new();
    }

    let mut truncated = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '*' | '_' | '~' | '`' | '|' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sonoryn::media::{ResolvedTrack, TrackMetadata, TrackSource};

    use super::{AUTOCOMPLETE_LIMIT, autocomplete_choice, autocomplete_request};

    #[test]
    fn autocomplete_turns_free_text_into_multi_result_search() {
        let request = autocomplete_request("artist song");
        assert_eq!(
            request.input(),
            format!("ytsearch{AUTOCOMPLETE_LIMIT}:artist song")
        );
    }

    #[test]
    fn autocomplete_keeps_urls_exact() {
        let request = autocomplete_request("https://example.test/watch/1");
        assert_eq!(request.input(), "https://example.test/watch/1");
    }

    #[test]
    fn autocomplete_uses_public_track_url_as_submitted_value() {
        let track = ResolvedTrack {
            source: TrackSource::YouTube,
            metadata: TrackMetadata {
                title: "A song".to_owned(),
                artist: Some("An artist".to_owned()),
                duration: Some(Duration::from_secs(123)),
                artwork_url: None,
                webpage_url: "https://example.test/watch/1".to_owned(),
            },
            locator: "https://example.test/watch/1".to_owned(),
        };

        let choice = autocomplete_choice(track).expect("choice");
        assert_eq!(choice.name, "A song — An artist");
        assert_eq!(
            choice.value,
            gloam_commands::AutocompleteChoiceValue::String(
                "https://example.test/watch/1".to_owned()
            )
        );
    }
}
