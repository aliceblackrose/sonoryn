use std::{collections::VecDeque, sync::Arc};

use gloamwire::{
    model::{ChannelId, GuildId},
    voice::{
        DaveVoiceSession, DaveyProvider, OPUS_SILENCE_FLUSH_FRAMES, VoiceConnectionInfo,
        VoiceFramePacer, VoiceOpusFrame, VoiceSpeakingFlags,
    },
};
use sonoryn::media::{
    EncodedOpusFrame, FfmpegOpusDecoder, FfmpegOpusStream, Track, TrackResolver,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{info, warn};

use crate::{
    gateway_control::{SkipTrackResult, TrackEnqueueResult, VoiceJoinResult},
    player::{PlaybackState, PlayerDirectory, PlayerSnapshot},
};

const DECODE_FRAME_BUFFER: usize = 4;

#[derive(Debug)]
pub(crate) enum VoiceWorkerCommand {
    Enqueue {
        track: Track,
        response: oneshot::Sender<TrackEnqueueResult>,
    },
    Skip {
        response: oneshot::Sender<SkipTrackResult>,
    },
    Shutdown,
}

#[derive(Debug)]
pub(crate) enum VoiceWorkerStopReason {
    Requested,
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

#[derive(Debug, Clone, Copy)]
enum PrepareFailure {
    MediaResolution,
    DecoderStart,
}

#[derive(Debug)]
enum DecoderEvent {
    Frame(EncodedOpusFrame),
    Finished,
    Failed,
}

struct ActivePlayback {
    frames: mpsc::Receiver<DecoderEvent>,
    task: JoinHandle<()>,
    pacer: VoiceFramePacer,
}

impl ActivePlayback {
    fn spawn(mut stream: FfmpegOpusStream) -> Self {
        let (frames, receiver) = mpsc::channel(DECODE_FRAME_BUFFER);
        let task = tokio::spawn(async move {
            loop {
                let (event, terminal) = match stream.next_frame().await {
                    Ok(Some(frame)) => (DecoderEvent::Frame(frame), false),
                    Ok(None) => (DecoderEvent::Finished, true),
                    Err(_) => (DecoderEvent::Failed, true),
                };

                if frames.send(event).await.is_err() || terminal {
                    break;
                }
            }
        });

        Self {
            frames: receiver,
            task,
            pacer: VoiceFramePacer::default(),
        }
    }

    async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

struct GuildPlayback {
    guild_id: GuildId,
    channel_id: ChannelId,
    queue: VecDeque<Track>,
    current: Option<Track>,
    preparing: Option<JoinHandle<Result<FfmpegOpusStream, PrepareFailure>>>,
    active: Option<ActivePlayback>,
    players: PlayerDirectory,
    resolver: Arc<dyn TrackResolver>,
    decoder: FfmpegOpusDecoder,
    speaking: bool,
}

impl GuildPlayback {
    fn new(
        guild_id: GuildId,
        channel_id: ChannelId,
        players: PlayerDirectory,
        resolver: Arc<dyn TrackResolver>,
    ) -> Self {
        Self {
            guild_id,
            channel_id,
            queue: VecDeque::new(),
            current: None,
            preparing: None,
            active: None,
            players,
            resolver,
            decoder: FfmpegOpusDecoder::new(),
            speaking: false,
        }
    }

    async fn enqueue(&mut self, track: Track) -> usize {
        let position = if self.current.is_none() && self.queue.is_empty() {
            0
        } else {
            self.queue.len() + 1
        };
        self.queue.push_back(track);
        self.publish().await;
        position
    }

    async fn start_next_if_idle(&mut self) {
        if self.current.is_some() || self.preparing.is_some() || self.active.is_some() {
            return;
        }

        let Some(track) = self.queue.pop_front() else {
            return;
        };
        self.current = Some(track.clone());

        let resolver = Arc::clone(&self.resolver);
        let decoder = self.decoder.clone();
        self.preparing = Some(tokio::spawn(async move {
            let media = resolver
                .resolve_media(&track)
                .await
                .map_err(|_| PrepareFailure::MediaResolution)?;
            decoder
                .open(&media)
                .map_err(|_| PrepareFailure::DecoderStart)
        }));
        self.publish().await;
    }

    async fn complete_prepare(
        &mut self,
        result: Result<Result<FfmpegOpusStream, PrepareFailure>, tokio::task::JoinError>,
        session: &mut DaveVoiceSession<DaveyProvider>,
    ) -> gloamwire::voice::VoiceResult<()> {
        let _ = self.preparing.take();
        match result {
            Ok(Ok(stream)) => {
                session.set_speaking(VoiceSpeakingFlags::MICROPHONE).await?;
                self.speaking = true;
                self.active = Some(ActivePlayback::spawn(stream));
                self.publish().await;
            }
            Ok(Err(stage)) => {
                self.log_prepare_failure(stage);
                self.current = None;
                self.publish().await;
            }
            Err(error) if error.is_cancelled() => {
                self.current = None;
                self.publish().await;
            }
            Err(_) => {
                warn!(
                    guild_id = self.guild_id.get(),
                    track_id = self.current.as_ref().map(|track| track.id.get()),
                    "track preparation task panicked"
                );
                self.current = None;
                self.publish().await;
            }
        }
        Ok(())
    }

    async fn handle_decoder_event(
        &mut self,
        event: Option<DecoderEvent>,
        session: &mut DaveVoiceSession<DaveyProvider>,
    ) -> gloamwire::voice::VoiceResult<()> {
        match event {
            Some(DecoderEvent::Frame(frame)) => {
                let active = self
                    .active
                    .as_mut()
                    .expect("decoder event requires active playback");
                active.pacer.wait_for_next_frame().await;
                session.send_opus_frame(frame.as_voice_frame()?).await?;
            }
            Some(DecoderEvent::Finished) => {
                self.finish_current(session, false).await?;
            }
            Some(DecoderEvent::Failed) | None => {
                warn!(
                    guild_id = self.guild_id.get(),
                    track_id = self.current.as_ref().map(|track| track.id.get()),
                    "FFmpeg playback stream failed"
                );
                self.finish_current(session, true).await?;
            }
        }
        Ok(())
    }

    async fn skip(
        &mut self,
        session: &mut DaveVoiceSession<DaveyProvider>,
    ) -> gloamwire::voice::VoiceResult<Option<String>> {
        let title = self
            .current
            .as_ref()
            .map(|track| track.metadata.title.clone());
        if title.is_none() {
            return Ok(None);
        }

        if let Some(mut prepare) = self.preparing.take() {
            prepare.abort();
            let _ = (&mut prepare).await;
        }
        if let Some(active) = self.active.take() {
            active.stop().await;
        }
        self.current = None;
        self.stop_speaking(session).await?;
        self.publish().await;
        Ok(title)
    }

    async fn shutdown(
        &mut self,
        session: &mut DaveVoiceSession<DaveyProvider>,
    ) -> gloamwire::voice::VoiceResult<()> {
        self.queue.clear();
        if let Some(mut prepare) = self.preparing.take() {
            prepare.abort();
            let _ = (&mut prepare).await;
        }
        if let Some(active) = self.active.take() {
            active.stop().await;
        }
        self.current = None;
        self.stop_speaking(session).await?;
        self.publish().await;
        Ok(())
    }

    async fn finish_current(
        &mut self,
        session: &mut DaveVoiceSession<DaveyProvider>,
        abort_decoder: bool,
    ) -> gloamwire::voice::VoiceResult<()> {
        if let Some(active) = self.active.take() {
            if abort_decoder {
                active.stop().await;
            } else {
                let _ = active.task.await;
            }
        }
        self.current = None;
        self.stop_speaking(session).await?;
        self.publish().await;
        Ok(())
    }

    async fn stop_speaking(
        &mut self,
        session: &mut DaveVoiceSession<DaveyProvider>,
    ) -> gloamwire::voice::VoiceResult<()> {
        if !self.speaking {
            return Ok(());
        }

        let mut pacer = VoiceFramePacer::default();
        for _ in 0..OPUS_SILENCE_FLUSH_FRAMES {
            pacer.wait_for_next_frame().await;
            session.send_opus_frame(VoiceOpusFrame::silence()).await?;
        }
        session.set_speaking(VoiceSpeakingFlags(0)).await?;
        self.speaking = false;
        Ok(())
    }

    async fn publish(&self) {
        let state = if self.active.is_some() {
            PlaybackState::Playing
        } else if self.preparing.is_some() {
            PlaybackState::Loading
        } else {
            PlaybackState::Idle
        };
        self.players
            .publish(
                self.guild_id,
                PlayerSnapshot {
                    channel_id: Some(self.channel_id),
                    state,
                    current: self.current.clone(),
                    queue: self.queue.iter().cloned().collect(),
                },
            )
            .await;
    }

    fn log_prepare_failure(&self, stage: PrepareFailure) {
        warn!(
            guild_id = self.guild_id.get(),
            track_id = self.current.as_ref().map(|track| track.id.get()),
            stage = ?stage,
            "track preparation failed"
        );
    }
}

pub(crate) async fn run_voice_worker(
    guild_id: GuildId,
    generation: u64,
    channel_id: ChannelId,
    info: VoiceConnectionInfo,
    mut commands: mpsc::Receiver<VoiceWorkerCommand>,
    response: oneshot::Sender<VoiceJoinResult>,
    worker_events: mpsc::Sender<VoiceWorkerEvent>,
    players: PlayerDirectory,
    resolver: Arc<dyn TrackResolver>,
) {
    let connect = DaveVoiceSession::<DaveyProvider>::connect_davey(info, channel_id);
    tokio::pin!(connect);
    let mut buffered = VecDeque::new();

    let mut session = loop {
        tokio::select! {
            result = &mut connect => {
                match result {
                    Ok(session) => break session,
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
            command = commands.recv() => {
                match command {
                    Some(VoiceWorkerCommand::Shutdown) | None => {
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
                    Some(command) => buffered.push_back(command),
                }
            }
        }
    };

    let mut playback = GuildPlayback::new(guild_id, channel_id, players.clone(), resolver);
    playback.publish().await;
    let _ = response.send(VoiceJoinResult::Joined { channel_id });
    info!(
        guild_id = guild_id.get(),
        channel_id = channel_id.get(),
        "DAVE voice session connected"
    );

    let mut requested_stop = false;
    while let Some(command) = buffered.pop_front() {
        match apply_command(&mut playback, &mut session, command).await {
            Ok(true) => {
                requested_stop = true;
                break;
            }
            Ok(false) => {}
            Err(error) => {
                finish_worker(
                    &mut playback,
                    &mut session,
                    &players,
                    &worker_events,
                    guild_id,
                    generation,
                    VoiceWorkerStopReason::VoiceFailed(error.to_string()),
                )
                .await;
                return;
            }
        }
    }

    let reason = if requested_stop {
        VoiceWorkerStopReason::Requested
    } else {
        loop {
            playback.start_next_if_idle().await;

            if playback.preparing.is_some() {
                tokio::select! {
                    command = commands.recv() => {
                        match command {
                            Some(command) => match apply_command(&mut playback, &mut session, command).await {
                                Ok(true) => break VoiceWorkerStopReason::Requested,
                                Ok(false) => {}
                                Err(error) => break VoiceWorkerStopReason::VoiceFailed(error.to_string()),
                            },
                            None => break VoiceWorkerStopReason::Requested,
                        }
                    }
                    event = session.next_event() => {
                        if let Err(error) = event {
                            break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                        }
                    }
                    prepared = async {
                        playback
                            .preparing
                            .as_mut()
                            .expect("preparing branch requires a task")
                            .await
                    } => {
                        if let Err(error) = playback.complete_prepare(prepared, &mut session).await {
                            break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                        }
                    }
                }
            } else if playback.active.is_some() {
                tokio::select! {
                    command = commands.recv() => {
                        match command {
                            Some(command) => match apply_command(&mut playback, &mut session, command).await {
                                Ok(true) => break VoiceWorkerStopReason::Requested,
                                Ok(false) => {}
                                Err(error) => break VoiceWorkerStopReason::VoiceFailed(error.to_string()),
                            },
                            None => break VoiceWorkerStopReason::Requested,
                        }
                    }
                    event = session.next_event() => {
                        if let Err(error) = event {
                            break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                        }
                    }
                    decoder_event = async {
                        playback
                            .active
                            .as_mut()
                            .expect("active branch requires playback")
                            .frames
                            .recv()
                            .await
                    } => {
                        if let Err(error) = playback.handle_decoder_event(decoder_event, &mut session).await {
                            break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                        }
                    }
                }
            } else {
                tokio::select! {
                    command = commands.recv() => {
                        match command {
                            Some(command) => match apply_command(&mut playback, &mut session, command).await {
                                Ok(true) => break VoiceWorkerStopReason::Requested,
                                Ok(false) => {}
                                Err(error) => break VoiceWorkerStopReason::VoiceFailed(error.to_string()),
                            },
                            None => break VoiceWorkerStopReason::Requested,
                        }
                    }
                    event = session.next_event() => {
                        if let Err(error) = event {
                            break VoiceWorkerStopReason::VoiceFailed(error.to_string());
                        }
                    }
                }
            }
        }
    };

    finish_worker(
        &mut playback,
        &mut session,
        &players,
        &worker_events,
        guild_id,
        generation,
        reason,
    )
    .await;
}

async fn apply_command(
    playback: &mut GuildPlayback,
    session: &mut DaveVoiceSession<DaveyProvider>,
    command: VoiceWorkerCommand,
) -> gloamwire::voice::VoiceResult<bool> {
    match command {
        VoiceWorkerCommand::Enqueue { track, response } => {
            let position = playback.enqueue(track).await;
            let _ = response.send(TrackEnqueueResult::Accepted { position });
            Ok(false)
        }
        VoiceWorkerCommand::Skip { response } => {
            match playback.skip(session).await {
                Ok(Some(title)) => {
                    let _ = response.send(SkipTrackResult::Skipped { title });
                    Ok(false)
                }
                Ok(None) => {
                    let _ = response.send(SkipTrackResult::NothingPlaying);
                    Ok(false)
                }
                Err(error) => {
                    let _ = response.send(SkipTrackResult::Failed(error.to_string()));
                    Err(error)
                }
            }
        }
        VoiceWorkerCommand::Shutdown => Ok(true),
    }
}

async fn finish_worker(
    playback: &mut GuildPlayback,
    session: &mut DaveVoiceSession<DaveyProvider>,
    players: &PlayerDirectory,
    worker_events: &mpsc::Sender<VoiceWorkerEvent>,
    guild_id: GuildId,
    generation: u64,
    mut reason: VoiceWorkerStopReason,
) {
    if let Err(error) = playback.shutdown(session).await {
        reason = VoiceWorkerStopReason::VoiceFailed(error.to_string());
    }
    if let Err(error) = session.shutdown().await {
        warn!(
            guild_id = guild_id.get(),
            error = %error,
            "failed to gracefully close Discord voice session"
        );
    }
    players.remove(guild_id).await;
    send_stopped(worker_events, guild_id, generation, reason).await;
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
