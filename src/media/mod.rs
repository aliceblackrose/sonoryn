mod decoder;
mod resolver;
mod rustypipe;
mod track;
#[cfg(feature = "yt-dlp-fallback")]
mod youtube_search;
#[cfg(feature = "yt-dlp-fallback")]
mod ytdlp;

pub use decoder::{
    DecodeError, EncodedOpusFrame, FfmpegDecodeOptions, FfmpegOpusDecoder, FfmpegOpusStream,
};
pub use resolver::{MAX_PLAYLIST_ITEMS, ResolveError, ResolveFuture, TrackResolver};
pub use track::{
    PlayableMedia, RequestedBy, ResolvedTrack, Track, TrackId, TrackMetadata, TrackRequest,
    TrackSource,
};
pub use rustypipe::RustyPipeResolver;
#[cfg(feature = "yt-dlp-fallback")]
pub use ytdlp::YtDlpResolver;
