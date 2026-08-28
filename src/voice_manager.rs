use std::{collections::HashMap, sync::Arc, time::Duration};

use gloamwire::{
    gateway::{DispatchEvent, GatewayConnection, UpdateVoiceState},
    model::{ChannelId, GuildId, UserId},
    voice::{VoiceConnectionInfo, VoiceRendezvous, VoiceRendezvousStatus},
};
use sonoryn::media::{Track, TrackResolver};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::sleep,
};
use tracing::{error, info, warn};

use crate::{
    gateway_control::{
        GatewayControl, SkipTrackResult, TrackEnqueueResult, VoiceJoinResult, VoiceLeaveResult,
    },
    player::PlayerDirectory,
    voice_worker::{
        VoiceWorkerCommand, VoiceWorkerEvent, VoiceWorkerStopReason, run_voice_worker,
    },
};

const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
const VOICE_COMMAND_CAPACITY: usize = 16;
const VOICE_EVENT_CAPACITY: usize = 64;

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

pub(crate) struct VoiceManager {
    pending: HashMap<GuildId, PendingJoin>,
    workers: HashMap<GuildId, VoiceWorkerHandle>,
    worker_events: mpsc::Sender<VoiceWorkerEvent>,
    tasks: JoinSet<()>,
    players: PlayerDirectory,
    resolver: Arc<dyn TrackResolver>,
    next_id: u64,
}

impl VoiceManager {
    pub(crate) fn new(
        players: PlayerDirectory,
        resolver: Arc<dyn TrackResolver>,
    ) -> (Self, mpsc::Receiver<VoiceWorkerEvent>) {
        let (worker_events, receiver) = mpsc::channel(VOICE_EVENT_CAPACITY);
        (
            Self {
                pending: HashMap::new(),
                workers: HashMap::new(),
                worker_events,
                tasks: JoinSet::new(),
                players,
                resolver,
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
            GatewayControl::LeaveVoice { guild_id, response } => {
                self.leave_voice(guild_id, response, gateway).await;
            }
            GatewayControl::EnqueueTrack {
                guild_id,
                track,
                response,
            } => {
                self.enqueue_track(guild_id, track, response).await;
            }
            GatewayControl::SkipTrack { guild_id, response } => {
                self.skip_track(guild_id, response).await;
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
                RendezvousOutcome::Ready(info) => self.start_worker(guild_id, pending, info),
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

                self.workers.remove(&guild_id);
                match reason {
                    VoiceWorkerStopReason::Requested => {
                        info!(guild_id = guild_id.get(), "voice worker stopped");
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
            GatewayControl::EnqueueTrack { response, .. } => {
                let _ = response.send(TrackEnqueueResult::Failed(
                    "Sonoryn is shutting down.".to_owned(),
                ));
            }
            GatewayControl::SkipTrack { response, .. } => {
                let _ = response.send(SkipTrackResult::Failed(
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
                error!(error = %error, "voice worker task failed during shutdown");
            }
        }
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
        response: oneshot::Sender<VoiceLeaveResult>,
        gateway: &mut GatewayConnection,
    ) {
        if let Some(pending) = self.pending.remove(&guild_id) {
            let channel_id = pending.channel_id;
            let _ = pending.response.send(VoiceJoinResult::Cancelled);
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

        let Some(worker) = self.workers.remove(&guild_id) else {
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
                let _ = response.send(VoiceLeaveResult::Failed(format!(
                    "Stopped the voice worker, but failed to leave voice: {error}"
                )));
            }
        }
    }

    async fn enqueue_track(
        &mut self,
        guild_id: GuildId,
        track: Track,
        response: oneshot::Sender<TrackEnqueueResult>,
    ) {
        let Some(worker) = self.workers.get(&guild_id) else {
            let _ = response.send(TrackEnqueueResult::NotConnected);
            return;
        };
        let commands = worker.commands.clone();
        let command = VoiceWorkerCommand::Enqueue { track, response };
        if let Err(error) = commands.send(command).await
            && let VoiceWorkerCommand::Enqueue { response, .. } = error.0
        {
            let _ = response.send(TrackEnqueueResult::Failed(
                "The guild voice worker is unavailable.".to_owned(),
            ));
        }
    }

    async fn skip_track(
        &mut self,
        guild_id: GuildId,
        response: oneshot::Sender<SkipTrackResult>,
    ) {
        let Some(worker) = self.workers.get(&guild_id) else {
            let _ = response.send(SkipTrackResult::NotConnected);
            return;
        };
        let commands = worker.commands.clone();
        let command = VoiceWorkerCommand::Skip { response };
        if let Err(error) = commands.send(command).await
            && let VoiceWorkerCommand::Skip { response } = error.0
        {
            let _ = response.send(SkipTrackResult::Failed(
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

        let worker_events = self.worker_events.clone();
        self.tasks.spawn(run_voice_worker(
            guild_id,
            generation,
            pending.channel_id,
            info,
            receiver,
            pending.response,
            worker_events,
            self.players.clone(),
            Arc::clone(&self.resolver),
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
