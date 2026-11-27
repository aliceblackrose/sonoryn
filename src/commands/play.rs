use std::{collections::HashSet, time::Duration};

use gloam_commands::prelude::*;
use gloamwire::model::{
    ApplicationCommandInteractionValue, ChannelId, Embed, GuildId, Interaction, UserId,
};
use sonoryn::{
    media::{RequestedBy, ResolvedTrack, TrackRequest},
    player::QUEUE_CAPACITY_MESSAGE,
};
use tokio::sync::oneshot;

use super::{panel, ui};

use crate::{
    gateway_control::{GatewayControl, PlaybackAction, PlaybackControlResult, VoiceJoinResult},
    state::AppState,
};

const AUTOCOMPLETE_LIMIT: usize = 10;
const AUTOCOMPLETE_TIMEOUT: Duration = Duration::from_millis(2_200);
const AUTOCOMPLETE_CHOICE_NAME_LIMIT: usize = 100;
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

    // URLs can still be submitted directly. Do not invent a search result.
    if is_http_url(input) {
        return Ok(Vec::new());
    }

    let request = autocomplete_request(input);
    let tracks = match ctx
        .data()
        .resolver
        .suggest(&request, AUTOCOMPLETE_LIMIT, AUTOCOMPLETE_TIMEOUT)
        .await
    {
        Ok(tracks) => tracks,
        Err(error) => {
            tracing::debug!(%error, "autocomplete search failed");
            Vec::new()
        }
    };

    Ok(autocomplete_choices(tracks))
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
    ctx.defer_ephemeral().await?;
    let embed = enqueue_card(ctx.data(), ctx.interaction(), query).await;
    panel::reply_card(&ctx, embed, panel::launch_buttons()).await
}

/// Shared by slash commands and the track-entry modal.
pub(super) async fn enqueue_card(
    state: &AppState,
    interaction: &Interaction,
    query: String,
) -> Embed {
    let notice = |message: String| ui::notice("Add track", message);
    let Some(guild_id) = interaction.guild_id else {
        return notice("This action can only be used in a server.".to_owned());
    };
    let Some(user_id) = invoking_user_id(interaction) else {
        return notice("I could not determine the invoking user.".to_owned());
    };
    if current_voice_channel(state, guild_id, user_id)
        .await
        .is_none()
    {
        return notice("Join a voice channel first, then add a track.".to_owned());
    }
    let request = match TrackRequest::new(query) {
        Ok(request) => request,
        Err(message) => return notice(message.to_owned()),
    };
    let resolved = match state.resolver.resolve(&request).await {
        Ok(resolved) => resolved,
        Err(error) => {
            let message = escape_markdown(&truncate_chars(&error.to_string(), ERROR_MESSAGE_LIMIT));
            return notice(format!("I could not resolve that track: {message}"));
        }
    };
    let Some(channel_id) = current_voice_channel(state, guild_id, user_id).await else {
        return notice("You left voice while I was resolving that track.".to_owned());
    };
    match ensure_voice(state, guild_id, channel_id).await {
        VoiceJoinResult::Joined { .. } => {}
        VoiceJoinResult::AlreadyConnected {
            channel_id: connected,
        } if connected == channel_id => {}
        VoiceJoinResult::AlreadyConnected {
            channel_id: connected,
        } => {
            return notice(format!(
                "I am already connected to <#{}>. Join that channel to control playback.",
                connected.get()
            ));
        }
        VoiceJoinResult::InProgress { channel_id } => {
            return notice(format!(
                "A voice join for <#{}> is already in progress. Try again once it connects.",
                channel_id.get()
            ));
        }
        VoiceJoinResult::Cancelled => return notice("The voice join was cancelled.".to_owned()),
        VoiceJoinResult::Failed(error) => return notice(error),
    }
    let (track, queue_position) = {
        let mut players = state.player_manager.write().await;
        if !players.can_enqueue(guild_id, 1) {
            return notice(QUEUE_CAPACITY_MESSAGE.to_owned());
        }
        players.enqueue_resolved(guild_id, request, RequestedBy::new(user_id), resolved)
    };
    match playback_control(state, guild_id, channel_id, PlaybackAction::Wake).await {
        PlaybackControlResult::Accepted => {
            let snapshot = state.player_manager.read().await.snapshot(guild_id);
            if snapshot
                .now_playing
                .as_ref()
                .is_some_and(|current| current.id == track.id)
            {
                ui::track_card("Now playing", &track, None)
            } else {
                ui::track_card("Added to queue", &track, Some(queue_position))
            }
        }
        PlaybackControlResult::NotConnected => {
            notice("The voice worker disconnected before playback could start.".to_owned())
        }
        PlaybackControlResult::WrongVoiceChannel { channel_id } => notice(format!(
            "I am connected to <#{}>. Join that channel to control playback.",
            channel_id.get()
        )),
        PlaybackControlResult::NothingPlaying => {
            notice("The track was queued, but the player did not start it.".to_owned())
        }
        PlaybackControlResult::AlreadyPaused | PlaybackControlResult::AlreadyPlaying => {
            ui::track_card("Added to queue", &track, Some(queue_position))
        }
        PlaybackControlResult::Failed(error) => notice(error),
    }
}

fn autocomplete_request(input: &str) -> TrackRequest {
    TrackRequest::new(input).expect("autocomplete input was checked as non-empty")
}

fn autocomplete_choices(tracks: Vec<ResolvedTrack>) -> Vec<AutocompleteChoice> {
    let mut seen = HashSet::new();
    tracks
        .into_iter()
        .filter_map(autocomplete_choice)
        .filter(|choice| {
            if let gloam_commands::AutocompleteChoiceValue::String(value) = &choice.value {
                seen.insert(value.clone())
            } else {
                false
            }
        })
        .take(AUTOCOMPLETE_LIMIT)
        .collect()
}

fn autocomplete_choice(track: ResolvedTrack) -> Option<AutocompleteChoice> {
    if track.metadata.title.trim().is_empty()
        || !is_http_url(&track.metadata.webpage_url)
        || track.metadata.webpage_url.chars().count() > 100
    {
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

    use super::{autocomplete_choice, autocomplete_choices, autocomplete_request};

    #[test]
    fn autocomplete_never_echoes_the_query_as_a_fake_result() {
        assert!(autocomplete_choices(Vec::new()).is_empty());
    }

    #[test]
    fn autocomplete_keeps_free_text_exact() {
        let request = autocomplete_request("artist song");
        assert_eq!(request.input(), "artist song");
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

        let choices = autocomplete_choices(vec![track.clone(), track.clone()]);
        assert_eq!(choices.len(), 1);

        let mut invalid = track.clone();
        invalid.metadata.title = " ".to_owned();
        assert!(autocomplete_choice(invalid).is_none());

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
