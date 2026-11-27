//! Interactive, ephemeral music dashboard using Gloamwire message buttons.

use std::error::Error;

use gloam_commands::prelude::*;
use gloamwire::{
    RestClient,
    http::{CreateInteractionResponseQuery, EditInteractionMessageQuery, EditWebhookMessage},
    model::{
        Component, Embed, GuildId, Interaction, InteractionCallbackData, InteractionCallbackType,
        InteractionMessageData, InteractionResponse, InteractionType, InteractiveButtonStyle,
        MessageFlags, Modal,
    },
};
use sonoryn::player::{LoopMode, PlayerSnapshot};

use super::{play, playback, queue, ui};
use crate::{
    gateway_control::{PlaybackAction, PlaybackControlResult},
    state::AppState,
};

type PanelResult<T = ()> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const PREFIX: &str = "sonoryn:panel:v1:";
const ADD_MODAL: &str = "sonoryn:panel:v1:submit";
const INPUT_ID: &str = "track-query";
const PAGE_SIZE: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum View {
    Player,
    Queue(usize),
    History,
    Mixer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    View(View),
    Add,
    Transport(PlaybackAction),
    Shuffle,
    Repeat,
    Volume(u8),
    Restart,
}

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    commands![player]
}

#[command(description = "Open Sonoryn's interactive music player", guild_only)]
pub(crate) async fn player(ctx: Context<AppState>) -> Result<()> {
    reply_view(&ctx, View::Player).await
}

pub(super) async fn reply_view(ctx: &Context<AppState>, view: View) -> Result<()> {
    let Some(guild_id) = ctx.interaction().guild_id else {
        return ctx
            .reply_ephemeral_embed(ui::notice("Sonoryn", "Open the player in a server."))
            .await;
    };
    let (embed, components) = render(ctx.data(), guild_id, view, None).await;
    reply_card(ctx, embed, components).await
}

pub(super) async fn reply_card(
    ctx: &Context<AppState>,
    embed: Embed,
    components: Vec<Component>,
) -> Result<()> {
    // Keep the framework's response state synchronized. Its current reply
    // helpers do not accept components, so attach them after the reply.
    ctx.reply_ephemeral_embed(embed).await?;
    let interaction = ctx.interaction();
    ctx.rest()
        .edit_original_interaction_response(
            interaction.application_id,
            interaction.token.expose(),
            &EditWebhookMessage {
                components: Some(Some(components)),
                ..Default::default()
            },
            &EditInteractionMessageQuery::default(),
        )
        .await?;
    Ok(())
}

pub(super) fn launch_buttons() -> Vec<Component> {
    vec![Component::action_row([
        button("player", "Open player", InteractiveButtonStyle::Primary, false),
        button("add", "Add track", InteractiveButtonStyle::Success, false),
        button("queue:0", "Queue", InteractiveButtonStyle::Secondary, false),
    ])]
}

fn button(action: &str, label: &str, style: InteractiveButtonStyle, disabled: bool) -> Component {
    Component::button(style, format!("{PREFIX}{action}"))
        .with_label(label)
        .with_disabled(disabled)
}

fn parse_action(id: &str) -> Option<Action> {
    let action = id.strip_prefix(PREFIX)?;
    Some(match action {
        "player" => Action::View(View::Player),
        "history" => Action::View(View::History),
        "mixer" => Action::View(View::Mixer),
        "add" => Action::Add,
        "previous" => Action::Transport(PlaybackAction::Previous),
        "pause" => Action::Transport(PlaybackAction::Pause),
        "resume" => Action::Transport(PlaybackAction::Resume),
        "skip" => Action::Transport(PlaybackAction::Skip),
        "shuffle" => Action::Shuffle,
        "repeat" => Action::Repeat,
        "restart" => Action::Restart,
        _ => {
            if let Some(page) = ["queue:", "queue-prev:", "queue-next:", "queue-refresh:"]
                .iter()
                .find_map(|prefix| action.strip_prefix(*prefix))
            {
                Action::View(View::Queue(page.parse().ok()?))
            } else if let Some(volume) = action.strip_prefix("volume:") {
                let volume = volume.parse().ok()?;
                if !matches!(volume, 0 | 25 | 50 | 75 | 100) {
                    return None;
                }
                Action::Volume(volume)
            } else {
                return None;
            }
        }
    })
}

fn page_count(snapshot: &PlayerSnapshot) -> usize {
    snapshot.queue.len().max(1).div_ceil(PAGE_SIZE)
}

fn clamp_page(snapshot: &PlayerSnapshot, page: usize) -> usize {
    page.min(page_count(snapshot) - 1)
}

fn next_loop(mode: LoopMode) -> LoopMode {
    match mode {
        LoopMode::Off => LoopMode::Track,
        LoopMode::Track => LoopMode::Queue,
        LoopMode::Queue => LoopMode::Off,
    }
}

fn loop_label(mode: LoopMode) -> &'static str {
    match mode {
        LoopMode::Off => "Off",
        LoopMode::Track => "Track",
        LoopMode::Queue => "Queue",
    }
}

fn controls(
    snapshot: &PlayerSnapshot,
    mode: LoopMode,
    view: View,
    has_history: bool,
    source: Option<&str>,
) -> Vec<Component> {
    use InteractiveButtonStyle::{Primary, Secondary, Success};

    let idle = snapshot.now_playing.is_none();
    let navigation = vec![
        button("player", "Player", if view == View::Player { Primary } else { Secondary }, false),
        button("queue:0", "Queue", if matches!(view, View::Queue(_)) { Primary } else { Secondary }, false),
        button("history", "History", if view == View::History { Primary } else { Secondary }, false),
        button("mixer", "Mixer", if view == View::Mixer { Primary } else { Secondary }, false),
        button("add", "Add track", Success, false),
    ];
    let mut rows = vec![Component::action_row(navigation)];
    rows.push(Component::action_row([
        button("previous", "Previous", Secondary, !has_history),
        button("pause", "Pause", Secondary, idle),
        button("resume", "Resume", Success, idle),
        button("skip", "Skip", Primary, idle),
    ]));
    rows.push(Component::action_row([
        button("shuffle", "Shuffle", Secondary, snapshot.queue.len() < 2),
        button(
            "repeat",
            &format!("Repeat: {}", loop_label(mode)),
            if mode == LoopMode::Off { Secondary } else { Success },
            false,
        ),
        button("restart", "Restart track", Secondary, idle),
    ]));
    match view {
        View::Queue(page) => {
            let last = page_count(snapshot) - 1;
            // Disabled boundary buttons have distinct IDs, and the first page
            // uses a dedicated refresh ID to avoid colliding with the Queue tab.
            rows.push(Component::action_row([
                button(
                    &format!("queue-prev:{}", page.saturating_sub(1)),
                    "Earlier",
                    Secondary,
                    page == 0,
                ),
                button(
                    &format!("queue-refresh:{page}"),
                    &format!("Page {} / {} · Refresh", page + 1, last + 1),
                    Primary,
                    false,
                ),
                button(
                    &format!("queue-next:{}", page.saturating_add(1).min(last)),
                    "Later",
                    Secondary,
                    page >= last,
                ),
            ]));
        }
        View::Mixer => {
            rows.push(Component::action_row([0, 25, 50, 75, 100].map(|percent| {
                button(
                    &format!("volume:{percent}"),
                    if percent == 0 { "Mute".to_owned() } else { format!("{percent}%") }.as_str(),
                    Secondary,
                    false,
                )
            })));
        }
        View::Player | View::History => {
            if let Some(url) = source {
                rows.push(Component::action_row([
                    Component::link_button(url).with_label("Open source"),
                ]));
            }
        }
    }
    rows
}

async fn render(
    state: &AppState,
    guild_id: GuildId,
    view: View,
    status: Option<&str>,
) -> (Embed, Vec<Component>) {
    let (snapshot, mode) = {
        let players = state.player_manager.read().await;
        (players.snapshot(guild_id), players.loop_mode(guild_id))
    };
    let history = state.history_manager.read().await.snapshot(guild_id);
    let view = match view {
        View::Queue(page) => View::Queue(clamp_page(&snapshot, page)),
        other => other,
    };
    let mut embed = match view {
        View::Player => {
            let mut card = snapshot.now_playing.as_ref().map_or_else(
                || ui::notice("Sonoryn player", "Nothing is playing. Use **Add track** to get started."),
                |track| ui::track_card("SONORYN · Current track", track, None),
            );
            card.fields.push(ui::field("Up next", snapshot.queue.len().to_string()));
            card.fields.push(ui::field("Repeat", loop_label(mode)));
            if let Some(next) = snapshot.queue.first() {
                card.fields.push(ui::section("Next track", &next.metadata.title));
            }
            card
        }
        View::Queue(page) => ui::queue_card(
            "Music queue",
            queue::render_queue_page(&snapshot, page),
            &snapshot,
            mode,
        ),
        View::History => {
            let mut card = ui::notice("Recently played", queue::render_history(&history));
            card.fields.push(ui::field("History size", history.len().to_string()));
            card
        }
        View::Mixer => ui::notice(
            "Playback mixer",
            "Set a volume preset or restart the current track. Use the Player tab to see the current track.",
        ),
    };
    if let Some(status) = status {
        embed.fields.push(ui::section("Last action", status));
    }
    let components = controls(
        &snapshot,
        mode,
        view,
        !history.is_empty(),
        embed.url.as_deref(),
    );
    (embed, components)
}

pub(crate) fn owns(interaction: &Interaction) -> bool {
    matches!(interaction.kind, InteractionType::MESSAGE_COMPONENT | InteractionType::MODAL_SUBMIT)
        && interaction
            .data
            .as_ref()
            .and_then(|data| data.get("custom_id"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| id.starts_with(PREFIX))
}

async fn acknowledge(
    rest: &RestClient,
    interaction: &Interaction,
    response: InteractionResponse,
) -> PanelResult {
    rest.create_interaction_response(
        interaction.id,
        interaction.token.expose(),
        &response,
        &CreateInteractionResponseQuery::default(),
    )
    .await?;
    Ok(())
}

async fn private_notice(
    rest: &RestClient,
    interaction: &Interaction,
    message: &str,
) -> PanelResult {
    acknowledge(
        rest,
        interaction,
        InteractionResponse::message(InteractionMessageData {
            embeds: vec![ui::notice("Sonoryn", message)],
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    )
    .await
}

async fn edit_panel(
    rest: &RestClient,
    interaction: &Interaction,
    embed: Embed,
    components: Vec<Component>,
) -> PanelResult {
    rest.edit_original_interaction_response(
        interaction.application_id,
        interaction.token.expose(),
        &EditWebhookMessage {
            embeds: Some(Some(vec![embed])),
            components: Some(Some(components)),
            ..Default::default()
        },
        &EditInteractionMessageQuery::default(),
    )
    .await?;
    Ok(())
}

fn modal() -> serde_json::Result<Modal> {
    // Discord's Label (type 18) wraps the text input in its singular
    // component field; Gloamwire preserves that field in Component::extra.
    serde_json::from_value(serde_json::json!({
        "custom_id": ADD_MODAL,
        "title": "Add a track",
        "components": [{
            "type": 18,
            "label": "Song URL or search query",
            "component": {
                "type": 4,
                "custom_id": INPUT_ID,
                "style": 1,
                "required": true,
                "min_length": 1,
                "max_length": 200,
                "placeholder": "Artist, track title, or URL"
            }
        }]
    }))
}

fn submitted_query(data: &serde_json::Value) -> Option<&str> {
    let rows = data.get("components")?.as_array()?;
    rows.iter()
        .filter_map(|row| row.get("component").or_else(|| row.get("components")?.as_array()?.first()))
        .find_map(|input| {
            if input.get("custom_id")?.as_str()? != INPUT_ID {
                return None;
            }
            let value = input.get("value")?.as_str()?.trim();
            (!value.is_empty() && value.encode_utf16().count() <= 200).then_some(value)
        })
}

pub(crate) async fn handle(
    rest: &RestClient,
    state: &AppState,
    interaction: &Interaction,
    busy: bool,
) -> PanelResult {
    if busy {
        return private_notice(rest, interaction, "The player is busy. Please try again.").await;
    }
    let Some(guild_id) = interaction.guild_id else {
        return private_notice(rest, interaction, "Open the player in a server.").await;
    };
    if interaction.kind == InteractionType::MODAL_SUBMIT {
        let Some(data) = interaction.modal_submit_data()? else {
            return private_notice(rest, interaction, "Invalid track form.").await;
        };
        if data.custom_id != ADD_MODAL {
            return private_notice(rest, interaction, "That form is no longer supported.").await;
        }
        let Some(query) = interaction.data.as_ref().and_then(submitted_query) else {
            return private_notice(rest, interaction, "Enter a song or URL (1–200 characters).").await;
        };
        acknowledge(
            rest,
            interaction,
            InteractionResponse {
                kind: InteractionCallbackType::DEFERRED_CHANNEL_MESSAGE_WITH_SOURCE,
                data: Some(InteractionCallbackData::Message(Box::new(InteractionMessageData {
                    flags: Some(MessageFlags::EPHEMERAL),
                    ..Default::default()
                }))),
            },
        )
        .await?;
        let embed = play::enqueue_card(state, interaction, query.to_owned()).await;
        return edit_panel(rest, interaction, embed, launch_buttons()).await;
    }
    let Some(button) = interaction.button_data()? else {
        return private_notice(rest, interaction, "This control is not a button.").await;
    };
    let Some(action) = parse_action(&button.custom_id) else {
        return private_notice(rest, interaction, "This control is outdated. Open /player.").await;
    };
    if action == Action::Add {
        return acknowledge(
            rest,
            interaction,
            InteractionResponse {
                kind: InteractionCallbackType::MODAL,
                data: Some(InteractionCallbackData::Modal(modal()?)),
            },
        )
        .await;
    }

    // Acknowledge before waiting for cache locks, voice workers, or rendering.
    acknowledge(
        rest,
        interaction,
        InteractionResponse::new(InteractionCallbackType::DEFERRED_UPDATE_MESSAGE),
    )
    .await?;
    let (view, status) = if let Action::View(view) = action {
        (view, None)
    } else {
        let view = if matches!(action, Action::Volume(_)) { View::Mixer } else { View::Player };
        (view, Some(apply(state, interaction, guild_id, action).await))
    };
    let (embed, components) = render(state, guild_id, view, status.as_deref()).await;
    edit_panel(rest, interaction, embed, components).await
}

async fn apply(
    state: &AppState,
    interaction: &Interaction,
    guild_id: GuildId,
    action: Action,
) -> String {
    let Some(user_id) = playback::invoking_user_id(interaction) else {
        return "I could not identify the invoking user.".to_owned();
    };
    let Some(channel_id) = playback::current_voice_channel(state, guild_id, user_id).await else {
        return "Join my voice channel first to control playback.".to_owned();
    };
    let (transport, accepted, idle) = match action {
        Action::Transport(PlaybackAction::Previous) => (
            PlaybackAction::Previous, "Playing the previous track.", "No previous track in history.",
        ),
        Action::Transport(PlaybackAction::Pause) => (
            PlaybackAction::Pause, "Paused playback.", "Nothing is playing.",
        ),
        Action::Transport(PlaybackAction::Resume) => (
            PlaybackAction::Resume, "Resumed playback.", "Nothing is playing.",
        ),
        Action::Transport(PlaybackAction::Skip) => (
            PlaybackAction::Skip, "Skipped the current track.", "Nothing is playing.",
        ),
        Action::Restart => (
            PlaybackAction::Seek { position_millis: 0 }, "Restarted the track.", "Nothing is playing.",
        ),
        Action::Volume(percent) => (
            PlaybackAction::Volume { percent }, "Volume updated.", "Volume set for the next track.",
        ),
        Action::Shuffle | Action::Repeat => (
            PlaybackAction::CheckContext, "", "",
        ),
        _ => return "Unsupported control.".to_owned(),
    };
    let result = playback::playback_control(state, guild_id, channel_id, transport).await;
    match result {
        PlaybackControlResult::Accepted if action == Action::Shuffle => {
            let count = state.player_manager.write().await.shuffle_queued(guild_id);
            if count < 2 {
                "At least two queued tracks are needed to shuffle.".to_owned()
            } else {
                format!("Shuffled {count} queued tracks.")
            }
        }
        PlaybackControlResult::Accepted if action == Action::Repeat => {
            let mut players = state.player_manager.write().await;
            let mode = next_loop(players.loop_mode(guild_id));
            players.set_loop_mode(guild_id, mode);
            format!("Repeat: {}.", loop_label(mode))
        }
        PlaybackControlResult::Accepted | PlaybackControlResult::NothingPlaying
            if matches!(action, Action::Volume(_)) =>
        {
            let Action::Volume(percent) = action else { unreachable!() };
            format!("Volume set to {percent}%.")
        }
        PlaybackControlResult::Accepted => accepted.to_owned(),
        PlaybackControlResult::NothingPlaying if matches!(action, Action::Shuffle | Action::Repeat) => {
            "The voice worker rejected the queue-control context.".to_owned()
        }
        PlaybackControlResult::NothingPlaying => idle.to_owned(),
        PlaybackControlResult::NotConnected => "I am not connected to voice here.".to_owned(),
        PlaybackControlResult::WrongVoiceChannel { channel_id } => {
            format!("Join <#{}> to control playback.", channel_id.get())
        }
        PlaybackControlResult::AlreadyPaused => "Playback is already paused.".to_owned(),
        PlaybackControlResult::AlreadyPlaying => "Playback is already running.".to_owned(),
        PlaybackControlResult::Failed(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn routes_only_supported_namespaced_controls() {
        assert_eq!(parse_action("sonoryn:panel:v1:queue-next:2"), Some(Action::View(View::Queue(2))));
        assert_eq!(parse_action("sonoryn:panel:v1:volume:50"), Some(Action::Volume(50)));
        assert_eq!(parse_action("sonoryn:panel:v1:volume:999"), None);
        assert_eq!(parse_action("sonoryn:panel:v1:queue:-1"), None);
        assert_eq!(parse_action("other:pause"), None);
        assert_eq!(parse_action("sonoryn:panel:v1:unknown"), None);
    }

    #[test]
    fn queue_navigation_clamps_empty_and_stale_pages() {
        let snapshot = PlayerSnapshot::default();
        assert_eq!(page_count(&snapshot), 1);
        assert_eq!(clamp_page(&snapshot, usize::MAX), 0);
    }

    #[test]
    fn repeat_cycles_without_losing_a_mode() {
        assert_eq!(next_loop(LoopMode::Off), LoopMode::Track);
        assert_eq!(next_loop(LoopMode::Track), LoopMode::Queue);
        assert_eq!(next_loop(LoopMode::Queue), LoopMode::Off);
    }

    #[test]
    fn panels_obey_discord_button_limits_and_have_unique_routable_ids() {
        for view in [View::Player, View::Queue(0), View::History, View::Mixer] {
            let rows = controls(&PlayerSnapshot::default(), LoopMode::Off, view, false, Some("https://example.com"));
            assert!(rows.len() <= 5);
            let mut ids = HashSet::new();
            for row in rows {
                assert!((1..=5).contains(&row.components.len()));
                for component in row.components {
                    if let Some(id) = component.custom_id {
                        assert!(id.len() <= 100);
                        assert!(ids.insert(id.clone()), "duplicate {id}");
                        assert!(parse_action(&id).is_some(), "unroutable {id}");
                    }
                }
            }
        }
    }

    #[test]
    fn modal_uses_modern_label_and_accepts_only_the_expected_input() {
        let form = serde_json::to_value(modal().expect("modal")).expect("serialize");
        assert_eq!(form["components"][0]["component"]["custom_id"], INPUT_ID);
        let submission = serde_json::json!({
            "components": [{"type": 18, "component": {"type": 4, "custom_id": INPUT_ID, "value": "  a song  "}}]
        });
        assert_eq!(submitted_query(&submission), Some("a song"));
        assert_eq!(submitted_query(&serde_json::json!({"components": []})), None);
    }
}
