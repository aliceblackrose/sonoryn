use std::{future::Future, io, pin::Pin, time::Duration};

use thiserror::Error;

use super::{PlayableMedia, ResolvedTrack, Track, TrackRequest};

pub const MAX_PLAYLIST_ITEMS: usize = 25;

/// Boxed resolver future used to keep [`TrackResolver`] object-safe without an
/// async-trait runtime dependency.
pub type ResolveFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ResolveError>> + Send + 'a>>;

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

impl ResolveError {
    /// Whether repeating the source operation can reasonably recover without
    /// changing configuration or user input.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::TimedOut { .. } | Self::BackendFailed { .. } | Self::InvalidMediaUrl
        )
    }
}

/// Resolves user submissions into durable queue metadata and refreshes
/// short-lived direct media information immediately before playback.
pub trait TrackResolver: Send + Sync {
    fn resolve<'a>(&'a self, request: &'a TrackRequest) -> ResolveFuture<'a, ResolvedTrack>;

    /// Expands a bounded playlist submission into durable queue metadata.
    ///
    /// Backends that do not have playlist-specific support safely fall back to
    /// resolving one track, keeping the trait object-safe and source-agnostic.
    fn resolve_playlist<'a>(
        &'a self,
        request: &'a TrackRequest,
        _limit: usize,
    ) -> ResolveFuture<'a, Vec<ResolvedTrack>> {
        Box::pin(async move { self.resolve(request).await.map(|track| vec![track]) })
    }

    fn resolve_media<'a>(&'a self, track: &'a Track) -> ResolveFuture<'a, PlayableMedia>;
}

#[cfg(test)]
mod tests {
    use std::{io, time::Duration};

    use super::ResolveError;

    #[test]
    fn retry_classification_separates_transient_from_structural_failures() {
        assert!(ResolveError::TimedOut {
            backend: "fixture".to_owned(),
            timeout: Duration::from_secs(1),
        }
        .is_retryable());
        assert!(ResolveError::BackendFailed {
            backend: "fixture".to_owned(),
            message: "temporary network failure".to_owned(),
        }
        .is_retryable());
        assert!(ResolveError::InvalidMediaUrl.is_retryable());

        assert!(!ResolveError::Spawn {
            backend: "fixture".to_owned(),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        }
        .is_retryable());
        assert!(!ResolveError::NoResults.is_retryable());
        assert!(!ResolveError::MissingField { field: "title" }.is_retryable());
    }
}
