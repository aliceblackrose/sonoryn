use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gloamwire::{
    gateway::{DispatchEvent, GatewayConnection, UpdateVoiceState},
    model::{ChannelId, GuildId, UserId},
    voice::{
        DaveProviderLifecycle, DaveVoiceSession, VoiceCommand, VoiceConnectionInfo, VoiceDriver,
        VoiceError, VoiceFramePacer, VoiceGatewayConfig, VoiceGatewayEvent, VoiceRendezvous,
        VoiceRendezvousStatus, VoiceResult, VoiceSpeakingFlags,
    },
};
use sonoryn::{
    media::{EncodedOpusFrame, FfmpegDecodeOptions, FfmpegOpusDecoder, TrackResolver},
    player::{LoopMode, PlayerManager},
};
use tokio::{
    sync::{RwLock, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep, sleep_until},
};
use tracing::{error, info, warn};

use crate::{
    dave_provider::LibDaveProvider,
    gateway_control::{
        GatewayControl, PlaybackAction, PlaybackControlResult, VoiceJoinResult, VoiceLeaveResult,
    },
    history::HistoryManager,
    persistence::{CheckpointEntry, Checkpoints, VoiceCheckpoint},
};

const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const VOICE_COMMAND_CAPACITY: usize = 16;
const VOICE_EVENT_CAPACITY: usize = 64;
const DECODER_FRAME_CAPACITY: usize = 2;
const OPUS_FRAME_DURATION: Duration = Duration::from_millis(20);
const MAX_DECODER_RECOVERY_ATTEMPTS: u8 = 2;
const DECODER_STALL_TIMEOUT: Duration = Duration::from_secs(30);

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
    ready: Arc<AtomicBool>,
    leaving: bool,
}

struct VoiceWorkerServices {
    worker_events: mpsc::Sender<VoiceWorkerEvent>,
    players: Arc<RwLock<PlayerManager>>,
    history: Arc<RwLock<HistoryManager>>,
    resolver: Arc<dyn TrackResolver>,
    decoder: FfmpegOpusDecoder,
    checkpoints: Checkpoints,
    resume: Option<VoiceCheckpoint>,
    ready: Arc<AtomicBool>,
}

enum VoiceWorkerCommand {
    Shutdown {
        preserve: bool,
    },
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
    VoiceFailed(String),
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
    worker_events: mpsc::Sender<VoiceWorkerEvent>,
    tasks: JoinSet<()>,
    players: Arc<RwLock<PlayerManager>>,
    history: Arc<RwLock<HistoryManager>>,
    resolver: Arc<dyn TrackResolver>,
    decoder: FfmpegOpusDecoder,
    checkpoints: Checkpoints,
    next_id: u64,
}

impl VoiceManager {
    pub(crate) fn new(
        players: Arc<RwLock<PlayerManager>>,
        history: Arc<RwLock<HistoryManager>>,
        resolver: Arc<dyn TrackResolver>,
        checkpoints: Checkpoints,
    ) -> (Self, mpsc::Receiver<VoiceWorkerEvent>) {
        let (worker_events, receiver) = mpsc::channel(VOICE_EVENT_CAPACITY);
        (
            Self {
                pending: HashMap::new(),
                workers: HashMap::new(),
                worker_events,
                tasks: JoinSet::new(),
                players,
                history,
                resolver,
                decoder: FfmpegOpusDecoder::new(),
                checkpoints,
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
                self.join_voice(guild_id, channel_id, response, gateway, bot_user_id)
                    .await;
            }
            GatewayControl::LeaveVoice {
                guild_id,
                channel_id,
                response,
            } => {
                self.leave_voice(guild_id, channel_id, response, gateway)
                    .await;
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
                RendezvousOutcome::Ready(info) => self.start_worker(guild_id, pending, info).await,
                RendezvousOutcome::ServerUnavailable => {
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
                let is_current = self
                    .workers
                    .get(&guild_id)
                    .is_some_and(|worker| worker.generation == generation);
                if !is_current {
                    return;
                }

                let leaving = self
                    .workers
                    .remove(&guild_id)
                    .is_some_and(|worker| worker.leaving);
                if leaving {
                    self.players.write().await.clear(guild_id);
                }
                match reason {
                    VoiceWorkerStopReason::Requested => {
                        info!(guild_id = guild_id.get(), "voice worker stopped");
                    }
                    VoiceWorkerStopReason::IdleTimedOut => {
                        info!(
                            guild_id = guild_id.get(),
                            idle_seconds = IDLE_TIMEOUT.as_secs(),
                            "voice worker disconnected after idle timeout"
                        );
                        disconnect_gateway_voice(gateway, guild_id).await;
                    }
                    VoiceWorkerStopReason::ConnectFailed(error) => {
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "voice worker failed to connect"
                        );
                        disconnect_gateway_voice(gateway, guild_id).await;
                    }
                    VoiceWorkerStopReason::VoiceFailed(error) => {
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "voice worker stopped after voice transport failure"
                        );
                        disconnect_gateway_voice(gateway, guild_id).await;
                    }
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
        for (_, worker) in &workers {
            let _ = worker
                .commands
                .send(VoiceWorkerCommand::Shutdown {
                    preserve: !worker.leaving,
                })
                .await;
        }

        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result {
                error!(error = %error, "voice worker task failed during shutdown");
            }
        }
        for (guild_id, worker) in workers {
            if worker.leaving {
                self.players.write().await.clear(guild_id);
            }
            disconnect_gateway_voice(gateway, guild_id).await;
        }
    }

    pub(crate) async fn restore_voice(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        gateway: &mut GatewayConnection,
        bot_id: UserId,
    ) {
        if self.players.read().await.snapshot(guild_id).is_idle() {
            return;
        }
        let (response, _result) = oneshot::channel();
        self.join_voice(guild_id, channel_id, response, gateway, Some(bot_id))
            .await;
    }

    async fn join_voice(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        response: oneshot::Sender<VoiceJoinResult>,
        gateway: &mut GatewayConnection,
        bot_user_id: Option<UserId>,
    ) {
        if let Some(worker) = self.workers.get(&guild_id) {
            let result = if worker.leaving {
                VoiceJoinResult::Failed("Voice is disconnecting; try again shortly.".to_owned())
            } else if worker.ready.load(Ordering::Acquire) {
                VoiceJoinResult::AlreadyConnected {
                    channel_id: worker.channel_id,
                }
            } else {
                VoiceJoinResult::InProgress {
                    channel_id: worker.channel_id,
                }
            };
            let _ = response.send(result);
            return;
        }
        if let Some(pending) = self.pending.get(&guild_id) {
            let _ = response.send(VoiceJoinResult::InProgress {
                channel_id: pending.channel_id,
            });
            return;
        }

        let Some(bot_user_id) = bot_user_id else {
            let _ = response.send(VoiceJoinResult::Failed(
                "The Discord Gateway is not ready yet.".to_owned(),
            ));
            return;
        };

        let update = UpdateVoiceState::new(guild_id, Some(channel_id)).with_self_deaf(true);
        if let Err(error) = gateway.update_voice_state(&update).await {
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

    async fn leave_voice(
        &mut self,
        guild_id: GuildId,
        channel_id: ChannelId,
        response: oneshot::Sender<VoiceLeaveResult>,
        gateway: &mut GatewayConnection,
    ) {
        let expected = self
            .workers
            .get(&guild_id)
            .map(|worker| worker.channel_id)
            .or_else(|| {
                self.pending
                    .get(&guild_id)
                    .map(|pending| pending.channel_id)
            });
        let mut checkpoints = self.checkpoints.write().await;
        let expected = expected.or_else(|| {
            checkpoints
                .get(&guild_id)
                .map(|entry| entry.checkpoint.channel_id)
        });
        if expected.is_some_and(|expected| expected != channel_id) {
            let _ = response.send(VoiceLeaveResult::Failed(
                "Join my voice channel before disconnecting me.".to_owned(),
            ));
            return;
        }
        checkpoints.remove(&guild_id);
        drop(checkpoints);
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
                    let _ = response.send(VoiceLeaveResult::Failed(format!(
                        "Cancelled the pending join, but failed to leave voice: {error}"
                    )));
                }
            }
            return;
        }

        let Some(worker) = self.workers.get_mut(&guild_id) else {
            self.players.write().await.clear(guild_id);
            let _ = response.send(VoiceLeaveResult::NotConnected);
            return;
        };

        worker.leaving = true;
        self.players.write().await.clear(guild_id);
        let _ = worker
            .commands
            .send(VoiceWorkerCommand::Shutdown { preserve: false })
            .await;
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
        if worker.leaving || !worker.ready.load(Ordering::Acquire) {
            let _ = response.send(PlaybackControlResult::NotConnected);
            return;
        }
        if worker.channel_id != channel_id {
            let _ = response.send(PlaybackControlResult::WrongVoiceChannel {
                channel_id: worker.channel_id,
            });
            return;
        }
        let commands = worker.commands.clone();

        if let Err(error) = commands.try_send(VoiceWorkerCommand::Playback { action, response })
            && let VoiceWorkerCommand::Playback { response, .. } = error.into_inner()
        {
            let _ = response.send(PlaybackControlResult::Failed(
                "The guild voice worker is busy or unavailable; try again.".to_owned(),
            ));
        }
    }

    async fn start_worker(
        &mut self,
        guild_id: GuildId,
        pending: PendingJoin,
        info: VoiceConnectionInfo,
    ) {
        let generation = self.next_id();
        let (commands, receiver) = mpsc::channel(VOICE_COMMAND_CAPACITY);
        let ready = Arc::new(AtomicBool::new(false));
        self.workers.insert(
            guild_id,
            VoiceWorkerHandle {
                generation,
                channel_id: pending.channel_id,
                commands,
                ready: ready.clone(),
                leaving: false,
            },
        );

        let resume = {
            let mut checkpoints = self.checkpoints.write().await;
            let old = checkpoints.get(&guild_id).map(|entry| entry.checkpoint);
            let mut checkpoint = old.unwrap_or_else(|| VoiceCheckpoint::new(pending.channel_id));
            checkpoint.channel_id = pending.channel_id;
            checkpoints.insert(
                guild_id,
                CheckpointEntry {
                    generation,
                    checkpoint,
                },
            );
            old
        };
        let services = VoiceWorkerServices {
            worker_events: self.worker_events.clone(),
            players: self.players.clone(),
            history: self.history.clone(),
            resolver: self.resolver.clone(),
            decoder: self.decoder.clone(),
            checkpoints: self.checkpoints.clone(),
            resume,
            ready,
        };
        self.tasks.spawn(run_voice_worker(
            guild_id,
            generation,
            pending.channel_id,
            info,
            receiver,
            pending.response,
            services,
        ));
    }

    fn next_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    fn reap_tasks(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            if let Err(error) = result {
                error!(error = %error, "voice worker task panicked");
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
    recovery_attempts: u8,
    sent_audio: bool,
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
            recovery_attempts: 0,
            sent_audio: false,
        }
    }

    fn cancel(self) {
        self.task.abort();
    }
}

impl Drop for ActivePlayback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug, Clone, Copy)]
enum PlaybackFailure {
    MediaResolution,
    DecoderStart,
    DecoderRead,
    DecoderStalled,
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
        checkpoints,
        resume,
        ready,
    } = services;
    let connect = async {
        let provider = LibDaveProvider::new(info.user_id, channel_id)
            .map_err(|error| VoiceError::Protocol(error.to_string()))?;
        let ssrc_handle = provider.ssrc_handle();
        let config = VoiceGatewayConfig::new(info).with_min_dave_protocol_version(1);
        let mut session = DaveVoiceSession::connect(config, provider).await?;
        let ssrc = session.session().gateway().ready().ssrc;
        ssrc_handle
            .set(ssrc)
            .map_err(|_| VoiceError::Protocol("DAVE Opus SSRC already configured".into()))?;

        // Transport negotiation alone does not establish an MLS media ratchet.
        // Do not advertise readiness or start decoding until DAVE is usable.
        while !session.provider().is_ready() {
            session.next_event().await?;
        }
        Ok::<_, VoiceError>(VoiceDriver::spawn(session))
    };
    tokio::pin!(connect);

    let mut session = tokio::select! {
        result = &mut connect => {
            match result {
                Ok(session) => session,
                Err(error) => {
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
        _ = sleep(JOIN_TIMEOUT) => {
            let message = "Discord voice connection timed out.".to_owned();
            let _ = response.send(VoiceJoinResult::Failed(message.clone()));
            send_stopped(
                &worker_events,
                guild_id,
                generation,
                VoiceWorkerStopReason::ConnectFailed(message),
            )
            .await;
            return;
        }
        command = commands.recv() => {
            if matches!(command, Some(VoiceWorkerCommand::Shutdown { preserve: false })) {
                players.write().await.clear(guild_id);
            }
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

    // Sonoryn is send-only. Drop the best-effort inbound audio queue.
    drop(session.take_audio_receiver());
    let voice_commands = session.commands();
    ready.store(true, Ordering::Release);
    let _ = response.send(VoiceJoinResult::Joined { channel_id });
    info!(
        guild_id = guild_id.get(),
        channel_id = channel_id.get(),
        "DAVE voice session connected"
    );

    let mut volume_percent = resume.map_or(100, |state| state.volume_percent.min(100));
    let current = players.read().await.snapshot(guild_id).now_playing;
    let mut active = if let Some(track) = current {
        let restored = resume.filter(|state| state.track_id == Some(track.id));
        let position = restored.map_or(Duration::ZERO, |state| {
            let position = Duration::from_millis(state.position_millis);
            if track
                .metadata
                .duration
                .is_some_and(|duration| position >= duration)
            {
                Duration::ZERO
            } else {
                position
            }
        });
        Some(ActivePlayback::spawn(
            track,
            resolver.clone(),
            decoder.clone(),
            position,
            volume_percent,
            restored.is_some_and(|state| state.paused),
        ))
    } else {
        start_next_playback(guild_id, &players, &resolver, &decoder, volume_percent).await
    };
    let mut last_checkpoint = Instant::now();
    let mut idle_deadline = active.is_none().then(|| Instant::now() + IDLE_TIMEOUT);
    let reason = loop {
        let decoder_enabled = active.as_ref().is_some_and(|playback| !playback.paused);
        let idle = active.is_none();
        let deadline = idle_deadline.unwrap_or_else(|| Instant::now() + IDLE_TIMEOUT);
        let input = if decoder_enabled {
            tokio::select! {
                command = commands.recv() => VoiceWorkerInput::Command(command),
                event = session.next_event() => VoiceWorkerInput::Voice(event.unwrap_or_else(|| {
                    Err(VoiceError::Protocol("voice driver stopped".into()))
                })),
                decoder_event = recv_decoder_event(&mut active) => {
                    VoiceWorkerInput::Decoder(decoder_event)
                }
                _ = sleep_until(deadline), if idle => VoiceWorkerInput::IdleTimeout,
            }
        } else {
            tokio::select! {
                command = commands.recv() => VoiceWorkerInput::Command(command),
                event = session.next_event() => VoiceWorkerInput::Voice(event.unwrap_or_else(|| {
                    Err(VoiceError::Protocol("voice driver stopped".into()))
                })),
                _ = sleep_until(deadline), if idle => VoiceWorkerInput::IdleTimeout,
            }
        };

        let is_frame = matches!(
            &input,
            VoiceWorkerInput::Decoder(Some(DecoderEvent::Frame(_)))
        );
        match input {
            VoiceWorkerInput::Command(Some(VoiceWorkerCommand::Shutdown { preserve })) => {
                if preserve {
                    update_checkpoint(
                        &checkpoints,
                        guild_id,
                        generation,
                        active.as_ref(),
                        volume_percent,
                    )
                    .await;
                } else {
                    players.write().await.clear(guild_id);
                }
                if let Some(playback) = active.take() {
                    let speaking = playback.speaking;
                    playback.cancel();
                    if speaking
                        && let Err(error) = voice_commands.send(VoiceCommand::FinishSpeaking).await
                    {
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "failed to finish speaking during voice shutdown"
                        );
                    }
                }
                break VoiceWorkerStopReason::Requested;
            }
            VoiceWorkerInput::Command(None) => {
                update_checkpoint(
                    &checkpoints,
                    guild_id,
                    generation,
                    active.as_ref(),
                    volume_percent,
                )
                .await;
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
                            if speaking
                                && let Err(error) =
                                    voice_commands.send(VoiceCommand::FinishSpeaking).await
                            {
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
                            let position = players.enqueue_replay(guild_id, previous);
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
                        let track_id = playback.track_id;
                        let current = {
                            let players = players.read().await;
                            players.snapshot(guild_id).now_playing
                        };
                        let speaking = playback.speaking;
                        playback.cancel();
                        if speaking
                            && let Err(error) =
                                voice_commands.send(VoiceCommand::FinishSpeaking).await
                        {
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
                            if speaking
                                && let Err(error) =
                                    voice_commands.send(VoiceCommand::FinishSpeaking).await
                            {
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
                            if speaking
                                && let Err(error) =
                                    voice_commands.send(VoiceCommand::FinishSpeaking).await
                            {
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
                                if let Err(error) =
                                    voice_commands.send(VoiceCommand::FinishSpeaking).await
                                {
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
                            if speaking
                                && let Err(error) =
                                    voice_commands.send(VoiceCommand::FinishSpeaking).await
                            {
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
                update_checkpoint(
                    &checkpoints,
                    guild_id,
                    generation,
                    active.as_ref(),
                    volume_percent,
                )
                .await;
                let _ = response.send(result);
            }
            VoiceWorkerInput::Voice(Ok(_)) => {}
            VoiceWorkerInput::Voice(Err(error)) => {
                break VoiceWorkerStopReason::VoiceFailed(error.to_string());
            }
            VoiceWorkerInput::Decoder(Some(DecoderEvent::Frame(frame))) => {
                let Some(playback) = active.as_mut() else {
                    continue;
                };
                if !playback.speaking {
                    if let Err(error) = voice_commands
                        .send(VoiceCommand::SetSpeaking(VoiceSpeakingFlags::MICROPHONE))
                        .await
                    {
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
                if let Err(error) = voice_commands
                    .send(VoiceCommand::SendOpus {
                        payload: frame.payload().to_vec(),
                        duration: frame.duration(),
                    })
                    .await
                {
                    break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                }
                playback.position = playback.position.saturating_add(OPUS_FRAME_DURATION);
                playback.sent_audio = true;
            }
            VoiceWorkerInput::Decoder(terminal) => {
                let Some(playback) = active.take() else {
                    continue;
                };
                let track_id = playback.track_id;
                let duration = playback.track.metadata.duration;
                let position = playback.position;
                let completed = matches!(&terminal, Some(DecoderEvent::Finished))
                    && playback.sent_audio
                    && !ended_early(position, duration);

                if playback.speaking
                    && let Err(error) = voice_commands.send(VoiceCommand::FinishSpeaking).await
                {
                    // Retain the last transmitted offset on transport failure.
                    active = Some(playback);
                    break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                }

                if !completed {
                    let failure = match terminal {
                        Some(DecoderEvent::Finished) => "premature end of stream",
                        Some(DecoderEvent::Failed(PlaybackFailure::MediaResolution)) => {
                            "media resolution failed"
                        }
                        Some(DecoderEvent::Failed(PlaybackFailure::DecoderStart)) => {
                            "decoder start failed"
                        }
                        Some(DecoderEvent::Failed(PlaybackFailure::DecoderRead)) => {
                            "decoder read failed"
                        }
                        Some(DecoderEvent::Failed(PlaybackFailure::DecoderStalled)) => {
                            "decoder stalled"
                        }
                        Some(DecoderEvent::Frame(_)) => unreachable!("frame handled above"),
                        None => "decoder exited without a terminal event",
                    };
                    if should_recover(position, duration, playback.recovery_attempts) {
                        let attempt = playback.recovery_attempts + 1;
                        warn!(
                            guild_id = guild_id.get(),
                            track_id = track_id.get(),
                            position_ms = position.as_millis() as u64,
                            expected_ms = ?duration.map(|value| value.as_millis()),
                            attempt,
                            failure,
                            "recovering interrupted playback from last transmitted position"
                        );
                        let mut resumed = ActivePlayback::spawn(
                            playback.track.clone(),
                            resolver.clone(),
                            decoder.clone(),
                            position,
                            volume_percent,
                            false,
                        );
                        resumed.recovery_attempts = attempt;
                        active = Some(resumed);
                        continue;
                    }

                    warn!(
                        guild_id = guild_id.get(),
                        track_id = track_id.get(),
                        position_ms = position.as_millis() as u64,
                        expected_ms = ?duration.map(|value| value.as_millis()),
                        failure,
                        "incomplete track; advancing without applying repeat mode"
                    );
                    players.write().await.finish_current(guild_id, track_id);
                } else {
                    let mut players = players.write().await;
                    let loop_mode = players.loop_mode(guild_id);
                    if players.complete_current(guild_id, track_id) && loop_mode != LoopMode::Track
                    {
                        history.write().await.push(guild_id, playback.track.clone());
                    }
                }
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

        if !is_frame || last_checkpoint.elapsed() >= Duration::from_secs(1) {
            update_checkpoint(
                &checkpoints,
                guild_id,
                generation,
                active.as_ref(),
                volume_percent,
            )
            .await;
            last_checkpoint = Instant::now();
        }
        if active.is_some() {
            idle_deadline = None;
        } else if idle_deadline.is_none() {
            idle_deadline = Some(Instant::now() + IDLE_TIMEOUT);
        }
    };

    if matches!(&reason, VoiceWorkerStopReason::IdleTimedOut) {
        let mut checkpoints = checkpoints.write().await;
        if checkpoints
            .get(&guild_id)
            .is_some_and(|entry| entry.generation == generation)
        {
            checkpoints.remove(&guild_id);
        }
    } else if active.is_some() {
        update_checkpoint(
            &checkpoints,
            guild_id,
            generation,
            active.as_ref(),
            volume_percent,
        )
        .await;
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

// Metadata may differ slightly from decoded audio (encoder padding, etc.).
// A significant shortfall is not a successful track completion.
fn ended_early(position: Duration, expected: Option<Duration>) -> bool {
    if position.is_zero() {
        return true;
    }
    expected.is_some_and(|duration| {
        let tolerance = (duration / 20)
            .max(Duration::from_secs(1))
            .min(Duration::from_secs(5));
        duration.saturating_sub(position) > tolerance
    })
}

fn should_recover(position: Duration, expected: Option<Duration>, attempts: u8) -> bool {
    attempts < MAX_DECODER_RECOVERY_ATTEMPTS
        && expected.is_some_and(|duration| position < duration)
        && ended_early(position, expected)
}

async fn update_checkpoint(
    checkpoints: &Checkpoints,
    guild_id: GuildId,
    generation: u64,
    active: Option<&ActivePlayback>,
    volume_percent: u8,
) {
    let mut checkpoints = checkpoints.write().await;
    if let Some(entry) = checkpoints.get_mut(&guild_id)
        && entry.generation == generation
    {
        entry.checkpoint.track_id = active.map(|playback| playback.track_id);
        entry.checkpoint.position_millis = active.map_or(0, |playback| {
            u64::try_from(playback.position.as_millis()).unwrap_or(u64::MAX)
        });
        entry.checkpoint.paused = active.is_some_and(|playback| playback.paused);
        entry.checkpoint.volume_percent = volume_percent;
    }
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
        Err(error) => {
            warn!(%error, "failed to start audio decoder");
            let _ = events
                .send(DecoderEvent::Failed(PlaybackFailure::DecoderStart))
                .await;
            return;
        }
    };

    loop {
        match tokio::time::timeout(DECODER_STALL_TIMEOUT, stream.next_frame()).await {
            Ok(Ok(Some(frame))) => {
                if events.send(DecoderEvent::Frame(frame)).await.is_err() {
                    let _ = stream.shutdown().await;
                    return;
                }
            }
            Ok(Ok(None)) => {
                let _ = events.send(DecoderEvent::Finished).await;
                return;
            }
            Ok(Err(error)) => {
                warn!(%error, "audio decoder read failed");
                let _ = events
                    .send(DecoderEvent::Failed(PlaybackFailure::DecoderRead))
                    .await;
                return;
            }
            Err(_) => {
                let _ = stream.shutdown().await;
                let _ = events
                    .send(DecoderEvent::Failed(PlaybackFailure::DecoderStalled))
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{MAX_DECODER_RECOVERY_ATTEMPTS, ended_early, should_recover};

    #[test]
    fn half_track_is_not_a_successful_completion() {
        let length = Some(Duration::from_secs(240));
        let halfway = Duration::from_secs(120);
        assert!(ended_early(halfway, length));
        assert!(should_recover(halfway, length, 0));
        assert!(!should_recover(
            halfway,
            length,
            MAX_DECODER_RECOVERY_ATTEMPTS
        ));
    }

    #[test]
    fn near_end_and_unknown_duration_are_not_premature_eof() {
        assert!(!ended_early(
            Duration::from_secs(238),
            Some(Duration::from_secs(240))
        ));
        assert!(!ended_early(Duration::from_secs(30), None));
        assert!(ended_early(Duration::ZERO, None));
        assert!(!should_recover(Duration::from_secs(30), None, 0));
    }
}
