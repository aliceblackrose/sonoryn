use std::{future::Future, io, pin::Pin, time::Duration};

use thiserror::Error;

use super::{PlayableMedia, ResolvedTrack, Track, TrackRequest};

/// Boxed resolver future used to keep [`TrackResolver`] object-safe without an
/// async-trait runtime dependency.
pub type ResolveFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ResolveError>> + Send + 'a>>;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("failed to start resolver backend `{backend}`: {source}")]
    Spawn {
        backend: String,
        #[source]
        source: io::Error,
    },

    #[error("resolver backend `{backend}` timed out after {timeout:?}")]
    TimedOut { backend: String, timeout: Duration },

    #[error("resolver backend `{backend}` exited unsuccessfully: {message}")]
    BackendFailed { backend: String, message: String },

    #[error("resolver backend returned no matching track")]
    NoResults,

    #[error("resolver output was missing required field `{field}`")]
    MissingField { field: &'static str },

    #[error("resolver output was invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),

    #[error("resolver returned an invalid media URL")]
    InvalidMediaUrl,
}

/// Resolves user submissions into durable queue metadata and refreshes
/// short-lived direct media information immediately before playback.
pub trait TrackResolver: Send + Sync {
    fn resolve<'a>(&'a self, request: &'a TrackRequest) -> ResolveFuture<'a, ResolvedTrack>;

    fn resolve_media<'a>(&'a self, track: &'a Track) -> ResolveFuture<'a, PlayableMedia>;
}
