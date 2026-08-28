use gloamwire::model::{ChannelId, GuildId};
use sonoryn::media::Track;
use tokio::sync::oneshot;

pub(crate) enum GatewayControl {
    JoinVoice {
        guild_id: GuildId,
        channel_id: ChannelId,
        response: oneshot::Sender<VoiceJoinResult>,
    },
    LeaveVoice {
        guild_id: GuildId,
        response: oneshot::Sender<VoiceLeaveResult>,
    },
    EnqueueTrack {
        guild_id: GuildId,
        track: Track,
        response: oneshot::Sender<TrackEnqueueResult>,
    },
    SkipTrack {
        guild_id: GuildId,
        response: oneshot::Sender<SkipTrackResult>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceJoinResult {
    Joined { channel_id: ChannelId },
    AlreadyConnected { channel_id: ChannelId },
    InProgress { channel_id: ChannelId },
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VoiceLeaveResult {
    Left { channel_id: ChannelId },
    CancelledJoin { channel_id: ChannelId },
    NotConnected,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrackEnqueueResult {
    Accepted { position: usize },
    NotConnected,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkipTrackResult {
    Skipped { title: String },
    NothingPlaying,
    NotConnected,
    Failed(String),
}
