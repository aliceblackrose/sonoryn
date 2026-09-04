use gloamwire::model::{ChannelId, GuildId};
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
    Playback {
        guild_id: GuildId,
        channel_id: ChannelId,
        action: PlaybackAction,
        response: oneshot::Sender<PlaybackControlResult>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaybackAction {
    CheckContext,
    Wake,
    Previous,
    Skip,
    Seek { position_millis: u64 },
    Volume { percent: u8 },
    Pause,
    Resume,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlaybackControlResult {
    Accepted,
    NotConnected,
    WrongVoiceChannel { channel_id: ChannelId },
    NothingPlaying,
    AlreadyPaused,
    AlreadyPlaying,
    Failed(String),
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
