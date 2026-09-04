use std::{collections::HashMap, sync::Arc, time::Duration};

use gloamwire::{
    gateway::{DispatchEvent, GatewayConnection, UpdateVoiceState},
    model::{ChannelId, GuildId, UserId},
    voice::{
        DaveVoiceSession, DaveyProvider, VoiceConnectionInfo, VoiceError, VoiceFramePacer,
        VoiceGatewayEvent, VoiceRecoveryOutcome, VoiceRendezvous, VoiceRendezvousStatus,
        VoiceResult, VoiceSpeakingFlags,
    },
};
use sonoryn::{
    media::{EncodedOpusFrame, FfmpegDecodeOptions, FfmpegOpusDecoder, TrackResolver},
    metrics::Metrics,
    player::{LoopMode, PlayerManager},
};
use tokio::{
    sync::{RwLock, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep, sleep_until},
};
use tracing::{error, info, warn};

use crate::{
    gateway_control::{
        GatewayControl, PlaybackAction, PlaybackControlResult, VoiceJoinResult, VoiceLeaveResult,
    },
    history::HistoryManager,
};

const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const RESTART_WINDOW: Duration = Duration::from_secs(60);
const MAX_VOICE_RESTARTS: u8 = 3;
const MAX_GUILD_VOICE_WORKERS: usize = 64;
const VOICE_COMMAND_CAPACITY: usize = 16;
const VOICE_EVENT_CAPACITY: usize = 64;
const DECODER_FRAME_CAPACITY: usize = 2;
const OPUS_FRAME_DURATION: Duration = Duration::from_millis(20);

struct PendingJoin {
    request_id: u64,
    channel_id: ChannelId,
    rendezvous: VoiceRendezvous,
    response: oneshot::Sender<VoiceJoinResult>,
}

struct VoiceWorkerHandle {
    generation: u64,
    channel_id: ChannelId,
    commands: mpsc::Sender<VoiceWorkerCommand>,
}

struct VoiceWorkerServices {
    worker_events: mpsc::Sender<VoiceWorkerEvent>,
    players: Arc<RwLock<PlayerManager>>,
    history: Arc<RwLock<HistoryManager>>,
    resolver: Arc<dyn TrackResolver>,
    decoder: FfmpegOpusDecoder,
    metrics: Arc<Metrics>,
}

struct RestartBudget {
    window_started: Instant,
    attempts: u8,
}

enum VoiceWorkerCommand {
    Shutdown,
    Playback {
        action: PlaybackAction,
        response: oneshot::Sender<PlaybackControlResult>,
    },
}

#[derive(Debug)]
pub(crate) enum VoiceWorkerStopReason {
    Requested,
    IdleTimedOut,
    ConnectFailed(String),
    RestartRequired(String),
    VoiceFailed(String),
    TerminalVoiceFailure(String),
    Crashed(String),
}

impl VoiceWorkerStopReason {
    fn is_restartable(&self) -> bool {
        matches!(
            self,
            Self::ConnectFailed(_)
                | Self::RestartRequired(_)
                | Self::VoiceFailed(_)
                | Self::Crashed(_)
        )
    }
}

#[derive(Debug)]
pub(crate) enum VoiceWorkerEvent {
    JoinTimedOut {
        guild_id: GuildId,
        request_id: u64,
    },
    Stopped {
        guild_id: GuildId,
        generation: u64,
        reason: VoiceWorkerStopReason,
    },
}

pub(crate) struct VoiceManager {
    pending: HashMap<GuildId, PendingJoin>,
    workers: HashMap<GuildId, VoiceWorkerHandle>,
    restart_budgets: HashMap<GuildId, RestartBudget>,
    worker_events: mpsc::Sender<VoiceWorkerEvent>,
    tasks: JoinSet<()>,
    players: Arc<RwLock<PlayerManager>>,
    history: Arc<RwLock<HistoryManager>>,
    resolver: Arc<dyn TrackResolver>,
    decoder: FfmpegOpusDecoder,
    metrics: Arc<Metrics>,
    next_id: u64,
}

impl VoiceManager {
    pub(crate) fn new(
        players: Arc<RwLock<PlayerManager>>,
        history: Arc<RwLock<HistoryManager>>,
        resolver: Arc<dyn TrackResolver>,
        metrics: Arc<Metrics>,
    ) -> (Self, mpsc::Receiver<VoiceWorkerEvent>) {
        let (worker_events, receiver) = mpsc::channel(VOICE_EVENT_CAPACITY);
        (
            Self {
                pending: HashMap::new(),
                workers: HashMap::new(),
                restart_budgets: HashMap::new(),
                worker_events,
                tasks: JoinSet::new(),
                players,
                history,
                resolver,
                decoder: FfmpegOpusDecoder::new(),
                metrics,
                next_id: 0,
            },
            receiver,
        )
    }

    pub(crate) async fn handle_control(
        &mut self,
        control: GatewayControl,
        gateway: &mut GatewayConnection,
        bot_user_id: Option<UserId>,
    ) {
        match control {
            GatewayControl::JoinVoice {
                guild_id,
                channel_id,
                response,
            } => {
                self.request_voice_join(
                    guild_id,
                    channel_id,
                    response,
                    gateway,
                    bot_user_id,
                    true,
                )
                .await;
            }
            GatewayControl::LeaveVoice { guild_id, response } => {
                self.leave_voice(guild_id, response, gateway).await;
            }
            GatewayControl::Playback {
                guild_id,
                channel_id,
                action,
                response,
            } => {
                self.route_playback(guild_id, channel_id, action, response)
                    .await;
            }
        }
        self.reap_tasks();
    }

    pub(crate) async fn handle_dispatch(
        &mut self,
        dispatch: &DispatchEvent,
        gateway: &mut GatewayConnection,
    ) {
        let mut completed = Vec::new();

        for (&guild_id, pending) in &mut self.pending {
            match pending.rendezvous.update_dispatch(dispatch) {
                Ok(VoiceRendezvousStatus::Pending) => {}
                Ok(VoiceRendezvousStatus::Ready(info)) => {
                    completed.push((guild_id, RendezvousOutcome::Ready(info)));
                }
                Ok(VoiceRendezvousStatus::ServerUnavailable) => {
                    completed.push((guild_id, RendezvousOutcome::ServerUnavailable));
                }
                Err(error) => {
                    self.metrics.increment_failures();
                    warn!(
                        guild_id = guild_id.get(),
                        error = %error,
                        "failed to parse voice rendezvous dispatch"
                    );
                }
            }
        }

        for (guild_id, outcome) in completed {
            let Some(pending) = self.pending.remove(&guild_id) else {
                continue;
            };

            match outcome {
                RendezvousOutcome::Ready(info) => self.start_worker(guild_id, pending, info),
                RendezvousOutcome::ServerUnavailable => {
                    self.metrics.increment_failures();
                    let _ = pending.response.send(VoiceJoinResult::Failed(
                        "Discord has not allocated a voice server for this guild yet.".to_owned(),
                    ));
                    disconnect_gateway_voice(gateway, guild_id).await;
                }
            }
        }

        self.reap_tasks();
    }

    pub(crate) async fn handle_worker_event(
        &mut self,
        event: VoiceWorkerEvent,
        gateway: &mut GatewayConnection,
        bot_user_id: Option<UserId>,
    ) {
        match event {
            VoiceWorkerEvent::JoinTimedOut {
                guild_id,
                request_id,
            } => {
                let should_cancel = self
                    .pending
                    .get(&guild_id)
                    .is_some_and(|pending| pending.request_id == request_id);
                if should_cancel {
                    let pending = self
                        .pending
                        .remove(&guild_id)
                        .expect("pending join was checked above");
                    self.metrics.increment_failures();
                    let _ = pending.response.send(VoiceJoinResult::Failed(
                        "Discord did not complete the voice rendezvous in time.".to_owned(),
                    ));
                    warn!(guild_id = guild_id.get(), "voice join rendezvous timed out");
                    disconnect_gateway_voice(gateway, guild_id).await;
                }
            }
            VoiceWorkerEvent::Stopped {
                guild_id,
                generation,
                reason,
            } => {
                let Some(worker) = self.workers.get(&guild_id) else {
                    return;
                };
                if worker.generation != generation {
                    return;
                }
                let channel_id = worker.channel_id;
                self.workers.remove(&guild_id);

                let restartable = reason.is_restartable();
                match &reason {
                    VoiceWorkerStopReason::Requested => {
                        self.restart_budgets.remove(&guild_id);
                        info!(guild_id = guild_id.get(), "voice worker stopped");
                    }
                    VoiceWorkerStopReason::IdleTimedOut => {
                        self.restart_budgets.remove(&guild_id);
                        info!(
                            guild_id = guild_id.get(),
                            idle_seconds = IDLE_TIMEOUT.as_secs(),
                            "voice worker disconnected after idle timeout"
                        );
                        disconnect_gateway_voice(gateway, guild_id).await;
                    }
                    VoiceWorkerStopReason::ConnectFailed(error) => {
                        self.metrics.increment_failures();
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "voice worker failed to connect"
                        );
                    }
                    VoiceWorkerStopReason::RestartRequired(error) => {
                        self.metrics.increment_failures();
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "voice credentials became invalid; restarting main-Gateway rendezvous"
                        );
                    }
                    VoiceWorkerStopReason::VoiceFailed(error) => {
                        self.metrics.increment_failures();
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "voice worker stopped after voice transport failure"
                        );
                    }
                    VoiceWorkerStopReason::TerminalVoiceFailure(error) => {
                        self.metrics.increment_failures();
                        self.restart_budgets.remove(&guild_id);
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "voice worker stopped after terminal Discord voice close"
                        );
                        self.players.write().await.clear(guild_id);
                        disconnect_gateway_voice(gateway, guild_id).await;
                    }
                    VoiceWorkerStopReason::Crashed(error) => {
                        self.metrics.increment_failures();
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "voice worker panicked; supervising restart"
                        );
                    }
                }

                if restartable {
                    self.requeue_current_for_restart(guild_id).await;
                    disconnect_gateway_voice(gateway, guild_id).await;
                    self.restart_voice(guild_id, channel_id, gateway, bot_user_id)
                        .await;
                }
            }
        }

        self.reap_tasks();
    }

    pub(crate) fn reject_control(control: GatewayControl) {
        match control {
            GatewayControl::JoinVoice { response, .. } => {
                let _ = response.send(VoiceJoinResult::Failed(
                    "Sonoryn is shutting down.".to_owned(),
                ));
            }
            GatewayControl::LeaveVoice { response, .. } => {
                let _ = response.send(VoiceLeaveResult::Failed(
                    "Sonoryn is shutting down.".to_owned(),
                ));
            }
            GatewayControl::Playback { response, .. } => {
                let _ = response.send(PlaybackControlResult::Failed(
                    "Sonoryn is shutting down.".to_owned(),
                ));
            }
        }
    }

    pub(crate) async fn shutdown(&mut self, gateway: &mut GatewayConnection) {
        let pending = self.pending.drain().collect::<Vec<_>>();
        for (guild_id, pending) in pending {
            let _ = pending.response.send(VoiceJoinResult::Cancelled);
            disconnect_gateway_voice(gateway, guild_id).await;
        }

        let workers = self.workers.drain().collect::<Vec<_>>();
        for (guild_id, worker) in workers {
            disconnect_gateway_voice(gateway, guild_id).await;
            let _ = worker.commands.send(VoiceWorkerCommand::Shutdown).await;
        }

        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result {
                error!(error = %error, "voice supervisor task failed during shutdown");
            }
        }
    }

    async fn request_voice_join(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        response: oneshot::Sender<VoiceJoinResult>,
        gateway: &mut GatewayConnection,
        bot_user_id: Option<UserId>,
        reset_restart_budget: bool,
    ) {
        if let Some(worker) = self.workers.get(&guild_id) {
            let _ = response.send(VoiceJoinResult::AlreadyConnected {
                channel_id: worker.channel_id,
            });
            return;
        }
        if let Some(pending) = self.pending.get(&guild_id) {
            let _ = response.send(VoiceJoinResult::InProgress {
                channel_id: pending.channel_id,
            });
            return;
        }
        if self.workers.len().saturating_add(self.pending.len()) >= MAX_GUILD_VOICE_WORKERS {
            let _ = response.send(VoiceJoinResult::Failed(format!(
                "Sonoryn is at its process limit of {MAX_GUILD_VOICE_WORKERS} active voice guilds."
            )));
            return;
        }

        let Some(bot_user_id) = bot_user_id else {
            let _ = response.send(VoiceJoinResult::Failed(
                "The Discord Gateway is not ready yet.".to_owned(),
            ));
            return;
        };
        if reset_restart_budget {
            self.restart_budgets.remove(&guild_id);
        }

        let update = UpdateVoiceState::new(guild_id, Some(channel_id)).with_self_deaf(true);
        if let Err(error) = gateway.update_voice_state(&update).await {
            self.metrics.increment_failures();
            let _ = response.send(VoiceJoinResult::Failed(format!(
                "Failed to request the voice join: {error}"
            )));
            return;
        }

        let request_id = self.next_id();
        self.pending.insert(
            guild_id,
            PendingJoin {
                request_id,
                channel_id,
                rendezvous: VoiceRendezvous::new(guild_id, bot_user_id),
                response,
            },
        );

        let worker_events = self.worker_events.clone();
        tokio::spawn(async move {
            sleep(JOIN_TIMEOUT).await;
            let _ = worker_events
                .send(VoiceWorkerEvent::JoinTimedOut {
                    guild_id,
                    request_id,
                })
                .await;
        });

        info!(
            guild_id = guild_id.get(),
            channel_id = channel_id.get(),
            "requested Discord voice join"
        );
    }

    async fn restart_voice(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        gateway: &mut GatewayConnection,
        bot_user_id: Option<UserId>,
    ) {
        let Some(bot_user_id) = bot_user_id else {
            warn!(
                guild_id = guild_id.get(),
                "cannot restart voice worker before the Discord Gateway is ready"
            );
            return;
        };
        if !self.take_restart_slot(guild_id) {
            warn!(
                guild_id = guild_id.get(),
                attempts = MAX_VOICE_RESTARTS,
                window_seconds = RESTART_WINDOW.as_secs(),
                "voice restart budget exhausted"
            );
            return;
        }

        self.metrics.increment_reconnects();
        let (response, _result) = oneshot::channel();
        self.request_voice_join(
            guild_id,
            channel_id,
            response,
            gateway,
            Some(bot_user_id),
            false,
        )
        .await;
    }

    fn take_restart_slot(&mut self, guild_id: GuildId) -> bool {
        let now = Instant::now();
        let budget = self.restart_budgets.entry(guild_id).or_insert(RestartBudget {
            window_started: now,
            attempts: 0,
        });
        if now.duration_since(budget.window_started) >= RESTART_WINDOW {
            budget.window_started = now;
            budget.attempts = 0;
        }
        if budget.attempts >= MAX_VOICE_RESTARTS {
            return false;
        }
        budget.attempts += 1;
        true
    }

    async fn requeue_current_for_restart(&self, guild_id: GuildId) {
        let mut players = self.players.write().await;
        let current = players.snapshot(guild_id).now_playing;
        let Some(current) = current else {
            return;
        };
        if players.finish_current(guild_id, current.id) {
            let position = players.enqueue(guild_id, current);
            let _ = players.move_queued(guild_id, position.saturating_sub(1), 0);
        }
    }

    async fn leave_voice(
        &mut self,
        guild_id: GuildId,
        response: oneshot::Sender<VoiceLeaveResult>,
        gateway: &mut GatewayConnection,
    ) {
        self.restart_budgets.remove(&guild_id);
        if let Some(pending) = self.pending.remove(&guild_id) {
            let channel_id = pending.channel_id;
            let _ = pending.response.send(VoiceJoinResult::Cancelled);
            self.players.write().await.clear(guild_id);
            match gateway
                .update_voice_state(&UpdateVoiceState::new(guild_id, None))
                .await
            {
                Ok(()) => {
                    let _ = response.send(VoiceLeaveResult::CancelledJoin { channel_id });
                }
                Err(error) => {
                    self.metrics.increment_failures();
                    let _ = response.send(VoiceLeaveResult::Failed(format!(
                        "Cancelled the pending join, but failed to leave voice: {error}"
                    )));
                }
            }
            return;
        }

        let Some(worker) = self.workers.remove(&guild_id) else {
            self.players.write().await.clear(guild_id);
            let _ = response.send(VoiceLeaveResult::NotConnected);
            return;
        };

        let _ = worker.commands.send(VoiceWorkerCommand::Shutdown).await;
        match gateway
            .update_voice_state(&UpdateVoiceState::new(guild_id, None))
            .await
        {
            Ok(()) => {
                let _ = response.send(VoiceLeaveResult::Left {
                    channel_id: worker.channel_id,
                });
            }
            Err(error) => {
                self.metrics.increment_failures();
                let _ = response.send(VoiceLeaveResult::Failed(format!(
                    "Stopped the voice worker, but failed to leave voice: {error}"
                )));
            }
        }
    }

    async fn route_playback(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        action: PlaybackAction,
        response: oneshot::Sender<PlaybackControlResult>,
    ) {
        let Some(worker) = self.workers.get(&guild_id) else {
            let _ = response.send(PlaybackControlResult::NotConnected);
            return;
        };
        if worker.channel_id != channel_id {
            let _ = response.send(PlaybackControlResult::WrongVoiceChannel {
                channel_id: worker.channel_id,
            });
            return;
        }
        let commands = worker.commands.clone();

        if let Err(error) = commands
            .send(VoiceWorkerCommand::Playback { action, response })
            .await
            && let VoiceWorkerCommand::Playback { response, .. } = error.0
        {
            self.metrics.increment_failures();
            let _ = response.send(PlaybackControlResult::Failed(
                "The guild voice worker is unavailable.".to_owned(),
            ));
        }
    }

    fn start_worker(&mut self, guild_id: GuildId, pending: PendingJoin, info: VoiceConnectionInfo) {
        let generation = self.next_id();
        let (commands, receiver) = mpsc::channel(VOICE_COMMAND_CAPACITY);
        self.workers.insert(
            guild_id,
            VoiceWorkerHandle {
                generation,
                channel_id: pending.channel_id,
                commands,
            },
        );

        let services = VoiceWorkerServices {
            worker_events: self.worker_events.clone(),
            players: self.players.clone(),
            history: self.history.clone(),
            resolver: self.resolver.clone(),
            decoder: self.decoder.clone(),
            metrics: self.metrics.clone(),
        };
        let crash_events = self.worker_events.clone();
        let channel_id = pending.channel_id;
        let response = pending.response;
        self.tasks.spawn(async move {
            let worker = tokio::spawn(run_voice_worker(
                guild_id,
                generation,
                channel_id,
                info,
                receiver,
                response,
                services,
            ));
            if let Err(error) = worker.await {
                send_stopped(
                    &crash_events,
                    guild_id,
                    generation,
                    VoiceWorkerStopReason::Crashed(error.to_string()),
                )
                .await;
            }
        });
    }

    fn next_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    fn reap_tasks(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            if let Err(error) = result {
                error!(error = %error, "voice supervisor task panicked");
            }
        }
    }
}

enum RendezvousOutcome {
    Ready(VoiceConnectionInfo),
    ServerUnavailable,
}

enum VoiceWorkerInput {
    Command(Option<VoiceWorkerCommand>),
    Voice(VoiceResult<VoiceGatewayEvent>),
    Decoder(Option<DecoderEvent>),
    IdleTimeout,
}

struct ActivePlayback {
    track: sonoryn::media::Track,
    track_id: sonoryn::media::TrackId,
    position: Duration,
    events: mpsc::Receiver<DecoderEvent>,
    task: JoinHandle<()>,
    pacer: VoiceFramePacer,
    paused: bool,
    speaking: bool,
}

impl ActivePlayback {
    fn spawn(
        track: sonoryn::media::Track,
        resolver: Arc<dyn TrackResolver>,
        decoder: FfmpegOpusDecoder,
        start_at: Duration,
        volume_percent: u8,
        paused: bool,
    ) -> Self {
        let track_id = track.id;
        let (events, receiver) = mpsc::channel(DECODER_FRAME_CAPACITY);
        let task = tokio::spawn(run_decoder(
            track.clone(),
            resolver,
            decoder,
            events,
            start_at,
            volume_percent,
        ));
        Self {
            track,
            track_id,
            position: start_at,
            events: receiver,
            task,
            pacer: VoiceFramePacer::default(),
            paused,
            speaking: false,
        }
    }

    fn cancel(self) {
        self.task.abort();
    }
}

#[derive(Debug, Clone, Copy)]
enum PlaybackFailure {
    MediaResolution,
    DecoderStart,
    DecoderRead,
    DecoderCrashed,
}

enum DecoderEvent {
    Frame(EncodedOpusFrame),
    Finished,
    Failed(PlaybackFailure),
}

async fn run_voice_worker(
    guild_id: GuildId,
    generation: u64,
    channel_id: ChannelId,
    info: VoiceConnectionInfo,
    mut commands: mpsc::Receiver<VoiceWorkerCommand>,
    response: oneshot::Sender<VoiceJoinResult>,
    services: VoiceWorkerServices,
) {
    let VoiceWorkerServices {
        worker_events,
        players,
        history,
        resolver,
        decoder,
        metrics,
    } = services;
    let started = Instant::now();
    let connect = DaveVoiceSession::<DaveyProvider>::connect_davey(info, channel_id);
    tokio::pin!(connect);

    let mut session = tokio::select! {
        result = &mut connect => {
            metrics.record_startup_latency(started.elapsed());
            match result {
                Ok(session) => session,
                Err(error) => {
                    metrics.increment_failures();
                    let message = error.to_string();
                    let _ = response.send(VoiceJoinResult::Failed(format!(
                        "Failed to establish the Discord voice session: {message}"
                    )));
                    send_stopped(
                        &worker_events,
                        guild_id,
                        generation,
                        VoiceWorkerStopReason::ConnectFailed(message),
                    )
                    .await;
                    return;
                }
            }
        }
        _ = commands.recv() => {
            players.write().await.clear(guild_id);
            let _ = response.send(VoiceJoinResult::Cancelled);
            send_stopped(
                &worker_events,
                guild_id,
                generation,
                VoiceWorkerStopReason::Requested,
            )
            .await;
            return;
        }
    };

    let _ = response.send(VoiceJoinResult::Joined { channel_id });
    info!(
        guild_id = guild_id.get(),
        channel_id = channel_id.get(),
        "DAVE voice session connected"
    );

    let mut active: Option<ActivePlayback> = None;
    let mut volume_percent = 100_u8;
    let mut idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
    let reason = loop {
        let decoder_enabled = active.as_ref().is_some_and(|playback| !playback.paused);
        let idle = active.is_none();
        let deadline = idle_deadline.unwrap_or_else(|| Instant::now() + IDLE_TIMEOUT);
        let input = if decoder_enabled {
            tokio::select! {
                command = commands.recv() => VoiceWorkerInput::Command(command),
                event = session.next_event() => VoiceWorkerInput::Voice(event),
                decoder_event = recv_decoder_event(&mut active) => {
                    VoiceWorkerInput::Decoder(decoder_event)
                }
                _ = sleep_until(deadline), if idle => VoiceWorkerInput::IdleTimeout,
            }
        } else {
            tokio::select! {
                command = commands.recv() => VoiceWorkerInput::Command(command),
                event = session.next_event() => VoiceWorkerInput::Voice(event),
                _ = sleep_until(deadline), if idle => VoiceWorkerInput::IdleTimeout,
            }
        };

        match input {
            VoiceWorkerInput::Command(Some(VoiceWorkerCommand::Shutdown))
            | VoiceWorkerInput::Command(None) => {
                players.write().await.clear(guild_id);
                if let Some(playback) = active.take() {
                    let speaking = playback.speaking;
                    playback.cancel();
                    if speaking && let Err(error) = session.finish_speaking().await {
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "failed to finish speaking during voice shutdown"
                        );
                    }
                }
                break VoiceWorkerStopReason::Requested;
            }
            VoiceWorkerInput::Command(Some(VoiceWorkerCommand::Playback { action, response })) => {
                let result = match action {
                    PlaybackAction::CheckContext => PlaybackControlResult::Accepted,
                    PlaybackAction::Wake => {
                        if active.is_none() {
                            active = start_next_playback(
                                guild_id,
                                &players,
                                &resolver,
                                &decoder,
                                volume_percent,
                            )
                            .await;
                        }
                        if active.is_some() {
                            PlaybackControlResult::Accepted
                        } else {
                            PlaybackControlResult::NothingPlaying
                        }
                    }
                    PlaybackAction::Previous => {
                        let previous = {
                            let history = history.read().await;
                            history.snapshot(guild_id).last().cloned()
                        };
                        let Some(previous) = previous else {
                            let _ = response.send(PlaybackControlResult::NothingPlaying);
                            continue;
                        };

                        let current = {
                            let players = players.read().await;
                            players.snapshot(guild_id).now_playing
                        };
                        if let Some(playback) = active.take() {
                            let speaking = playback.speaking;
                            playback.cancel();
                            if speaking && let Err(error) = session.finish_speaking().await {
                                break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                            }
                        }

                        {
                            let mut players = players.write().await;
                            if let Some(current) = current
                                && players.finish_current(guild_id, current.id)
                            {
                                let position = players.enqueue(guild_id, current);
                                let _ = players.move_queued(guild_id, position - 1, 0);
                            }
                            let position = players.enqueue(guild_id, previous);
                            let _ = players.move_queued(guild_id, position - 1, 0);
                        }
                        let _ = history.write().await.pop_latest(guild_id);
                        active = start_next_playback(
                            guild_id,
                            &players,
                            &resolver,
                            &decoder,
                            volume_percent,
                        )
                        .await;
                        PlaybackControlResult::Accepted
                    }
                    PlaybackAction::Skip => {
                        let Some(playback) = active.take() else {
                            let _ = response.send(PlaybackControlResult::NothingPlaying);
                            continue;
                        };
                        metrics.increment_skips();
                        let track_id = playback.track_id;
                        let current = {
                            let players = players.read().await;
                            players.snapshot(guild_id).now_playing
                        };
                        let speaking = playback.speaking;
                        playback.cancel();
                        if speaking && let Err(error) = session.finish_speaking().await {
                            break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                        }
                        let finished = players.write().await.finish_current(guild_id, track_id);
                        if finished && let Some(track) = current {
                            history.write().await.push(guild_id, track);
                        }
                        active = start_next_playback(
                            guild_id,
                            &players,
                            &resolver,
                            &decoder,
                            volume_percent,
                        )
                        .await;
                        PlaybackControlResult::Accepted
                    }
                    PlaybackAction::Seek { position_millis } => {
                        let Some(playback) = active.take() else {
                            let _ = response.send(PlaybackControlResult::NothingPlaying);
                            continue;
                        };
                        let position = Duration::from_millis(position_millis);
                        if playback
                            .track
                            .metadata
                            .duration
                            .is_some_and(|duration| position >= duration)
                        {
                            active = Some(playback);
                            PlaybackControlResult::Failed(
                                "The seek position is at or beyond the track duration.".to_owned(),
                            )
                        } else {
                            let track = playback.track.clone();
                            let paused = playback.paused;
                            let speaking = playback.speaking;
                            playback.cancel();
                            if speaking && let Err(error) = session.finish_speaking().await {
                                break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                            }
                            active = Some(ActivePlayback::spawn(
                                track,
                                resolver.clone(),
                                decoder.clone(),
                                position,
                                volume_percent,
                                paused,
                            ));
                            PlaybackControlResult::Accepted
                        }
                    }
                    PlaybackAction::Volume { percent } => {
                        volume_percent = percent.min(100);
                        if let Some(playback) = active.take() {
                            let track = playback.track.clone();
                            let position = playback.position;
                            let paused = playback.paused;
                            let speaking = playback.speaking;
                            playback.cancel();
                            if speaking && let Err(error) = session.finish_speaking().await {
                                break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                            }
                            active = Some(ActivePlayback::spawn(
                                track,
                                resolver.clone(),
                                decoder.clone(),
                                position,
                                volume_percent,
                                paused,
                            ));
                            PlaybackControlResult::Accepted
                        } else {
                            PlaybackControlResult::NothingPlaying
                        }
                    }
                    PlaybackAction::Pause => match active.as_mut() {
                        Some(playback) if playback.paused => PlaybackControlResult::AlreadyPaused,
                        Some(playback) => {
                            playback.paused = true;
                            if playback.speaking {
                                if let Err(error) = session.finish_speaking().await {
                                    break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                                }
                                playback.speaking = false;
                            }
                            PlaybackControlResult::Accepted
                        }
                        None => PlaybackControlResult::NothingPlaying,
                    },
                    PlaybackAction::Resume => match active.as_mut() {
                        Some(playback) if !playback.paused => PlaybackControlResult::AlreadyPlaying,
                        Some(playback) => {
                            playback.paused = false;
                            playback.pacer = VoiceFramePacer::default();
                            PlaybackControlResult::Accepted
                        }
                        None => PlaybackControlResult::NothingPlaying,
                    },
                    PlaybackAction::Stop => {
                        let removed = players.write().await.clear(guild_id);
                        if let Some(playback) = active.take() {
                            let speaking = playback.speaking;
                            playback.cancel();
                            if speaking && let Err(error) = session.finish_speaking().await {
                                break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                            }
                        }
                        if removed.is_idle() {
                            PlaybackControlResult::NothingPlaying
                        } else {
                            PlaybackControlResult::Accepted
                        }
                    }
                };
                let _ = response.send(result);
            }
            VoiceWorkerInput::Voice(Ok(_)) => {}
            VoiceWorkerInput::Voice(Err(VoiceError::Closed { code, reason })) => {
                match session.recover_after_close(code).await {
                    Ok(VoiceRecoveryOutcome::Resumed) => {
                        metrics.increment_reconnects();
                        info!(
                            guild_id = guild_id.get(),
                            "resumed Discord voice Gateway session"
                        );
                        continue;
                    }
                    Ok(VoiceRecoveryOutcome::RestartRequired) => {
                        break VoiceWorkerStopReason::RestartRequired(reason);
                    }
                    Ok(VoiceRecoveryOutcome::Stopped) => {
                        break VoiceWorkerStopReason::TerminalVoiceFailure(reason);
                    }
                    Err(error) => {
                        break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                    }
                }
            }
            VoiceWorkerInput::Voice(Err(error)) => {
                break VoiceWorkerStopReason::VoiceFailed(error.to_string());
            }
            VoiceWorkerInput::Decoder(Some(DecoderEvent::Frame(frame))) => {
                let Some(playback) = active.as_mut() else {
                    continue;
                };
                if !playback.speaking {
                    if let Err(error) = session.set_speaking(VoiceSpeakingFlags::MICROPHONE).await {
                        break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                    }
                    playback.speaking = true;
                }
                playback.pacer.wait_for_next_frame().await;
                let frame = match frame.as_voice_frame() {
                    Ok(frame) => frame,
                    Err(error) => {
                        break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                    }
                };
                if let Err(error) = session.send_opus_frame(frame).await {
                    break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                }
                playback.position = playback.position.saturating_add(OPUS_FRAME_DURATION);
            }
            VoiceWorkerInput::Decoder(Some(DecoderEvent::Finished)) => {
                let Some(playback) = active.take() else {
                    continue;
                };
                let track_id = playback.track_id;
                let (current, loop_mode) = {
                    let players = players.read().await;
                    (
                        players.snapshot(guild_id).now_playing,
                        players.loop_mode(guild_id),
                    )
                };
                let speaking = playback.speaking;
                if speaking && let Err(error) = session.finish_speaking().await {
                    break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                }
                let completed = players.write().await.complete_current(guild_id, track_id);
                if completed
                    && loop_mode != LoopMode::Track
                    && let Some(track) = current
                {
                    history.write().await.push(guild_id, track);
                }
                active =
                    start_next_playback(guild_id, &players, &resolver, &decoder, volume_percent)
                        .await;
            }
            VoiceWorkerInput::Decoder(Some(DecoderEvent::Failed(failure))) => {
                let Some(playback) = active.take() else {
                    continue;
                };
                metrics.increment_failures();
                let track_id = playback.track_id;
                let speaking = playback.speaking;
                warn!(
                    guild_id = guild_id.get(),
                    failure = ?failure,
                    "track playback failed; advancing queue"
                );
                if speaking && let Err(error) = session.finish_speaking().await {
                    break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                }
                players.write().await.finish_current(guild_id, track_id);
                active =
                    start_next_playback(guild_id, &players, &resolver, &decoder, volume_percent)
                        .await;
            }
            VoiceWorkerInput::Decoder(None) => {
                let Some(playback) = active.take() else {
                    continue;
                };
                metrics.increment_underruns();
                metrics.increment_failures();
                let track_id = playback.track_id;
                let speaking = playback.speaking;
                let task_error = playback.task.await.err();
                warn!(
                    guild_id = guild_id.get(),
                    error = ?task_error,
                    failure = ?PlaybackFailure::DecoderCrashed,
                    "decoder task ended without a terminal event; advancing queue"
                );
                if speaking && let Err(error) = session.finish_speaking().await {
                    break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                }
                players.write().await.finish_current(guild_id, track_id);
                active =
                    start_next_playback(guild_id, &players, &resolver, &decoder, volume_percent)
                        .await;
            }
            VoiceWorkerInput::IdleTimeout => {
                idle_deadline = None;
                let snapshot = {
                    let players = players.read().await;
                    players.snapshot(guild_id)
                };
                if snapshot.is_idle() {
                    info!(
                        guild_id = guild_id.get(),
                        idle_seconds = IDLE_TIMEOUT.as_secs(),
                        "voice worker idle timeout elapsed"
                    );
                    break VoiceWorkerStopReason::IdleTimedOut;
                }

                active =
                    start_next_playback(guild_id, &players, &resolver, &decoder, volume_percent)
                        .await;
            }
        }

        if active.is_some() {
            idle_deadline = None;
        } else if idle_deadline.is_none() {
            idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
        }
    };

    if matches!(
        &reason,
        VoiceWorkerStopReason::Requested
            | VoiceWorkerStopReason::IdleTimedOut
            | VoiceWorkerStopReason::TerminalVoiceFailure(_)
    ) {
        players.write().await.clear(guild_id);
    }
    if let Some(playback) = active.take() {
        playback.cancel();
    }

    if let Err(error) = session.shutdown().await {
        warn!(
            guild_id = guild_id.get(),
            error = %error,
            "failed to gracefully close Discord voice session"
        );
    }

    send_stopped(&worker_events, guild_id, generation, reason).await;
}

async fn start_next_playback(
    guild_id: GuildId,
    players: &Arc<RwLock<PlayerManager>>,
    resolver: &Arc<dyn TrackResolver>,
    decoder: &FfmpegOpusDecoder,
    volume_percent: u8,
) -> Option<ActivePlayback> {
    let track = players.write().await.start_next(guild_id)?;
    Some(ActivePlayback::spawn(
        track,
        resolver.clone(),
        decoder.clone(),
        Duration::ZERO,
        volume_percent,
        false,
    ))
}

async fn recv_decoder_event(active: &mut Option<ActivePlayback>) -> Option<DecoderEvent> {
    active.as_mut()?.events.recv().await
}

async fn run_decoder(
    track: sonoryn::media::Track,
    resolver: Arc<dyn TrackResolver>,
    decoder: FfmpegOpusDecoder,
    events: mpsc::Sender<DecoderEvent>,
    start_at: Duration,
    volume_percent: u8,
) {
    let media = match resolver.resolve_media(&track).await {
        Ok(media) => media,
        Err(_) => {
            let _ = events
                .send(DecoderEvent::Failed(PlaybackFailure::MediaResolution))
                .await;
            return;
        }
    };

    let options = FfmpegDecodeOptions::new()
        .with_start_at(start_at)
        .with_volume_percent(volume_percent);
    let mut stream = match decoder.open_with_options(&media, options) {
        Ok(stream) => stream,
        Err(_) => {
            let _ = events
                .send(DecoderEvent::Failed(PlaybackFailure::DecoderStart))
                .await;
            return;
        }
    };

    loop {
        match stream.next_frame().await {
            Ok(Some(frame)) => {
                if events.send(DecoderEvent::Frame(frame)).await.is_err() {
                    let _ = stream.shutdown().await;
                    return;
                }
            }
            Ok(None) => {
                let _ = events.send(DecoderEvent::Finished).await;
                return;
            }
            Err(_) => {
                let _ = events
                    .send(DecoderEvent::Failed(PlaybackFailure::DecoderRead))
                    .await;
                return;
            }
        }
    }
}

async fn send_stopped(
    worker_events: &mpsc::Sender<VoiceWorkerEvent>,
    guild_id: GuildId,
    generation: u64,
    reason: VoiceWorkerStopReason,
) {
    let _ = worker_events
        .send(VoiceWorkerEvent::Stopped {
            guild_id,
            generation,
            reason,
        })
        .await;
}

async fn disconnect_gateway_voice(gateway: &mut GatewayConnection, guild_id: GuildId) {
    if let Err(error) = gateway
        .update_voice_state(&UpdateVoiceState::new(guild_id, None))
        .await
    {
        warn!(
            guild_id = guild_id.get(),
            error = %error,
            "failed to clear main-Gateway voice state"
        );
    }
}
