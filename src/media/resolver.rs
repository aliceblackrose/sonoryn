use std::{future::Future, io, pin::Pin, time::Duration};

use thiserror::Error;

use super::{PlayableMedia, ResolvedTrack, Track, TrackRequest};

pub const MAX_PLAYLIST_ITEMS: usize = 25;

/// Whether retrying a failed source operation can reasonably succeed without
/// changing the request or local configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// The failure is transient and may succeed on a later attempt.
    Retryable,
    /// Retrying the same operation is not expected to help.
    Permanent,
}

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
    /// Classifies source failures without relying on backend error strings.
    ///
    /// Timeouts and backend process failures can be caused by transient network
    /// or upstream conditions. Spawn/configuration failures and malformed or
    /// semantically empty resolver output require operator or request changes.
    #[must_use]
    pub const fn retry_class(&self) -> RetryClass {
        match self {
            Self::TimedOut { .. } | Self::BackendFailed { .. } => RetryClass::Retryable,
            Self::Spawn { .. }
            | Self::NoResults
            | Self::MissingField { .. }
            | Self::InvalidJson(_)
            | Self::InvalidMediaUrl => RetryClass::Permanent,
        }
    }

    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self.retry_class(), RetryClass::Retryable)
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

    use super::{ResolveError, RetryClass};

    #[test]
    fn transient_source_failures_are_retryable() {
        let timeout = ResolveError::TimedOut {
            backend: "fixture".to_owned(),
            timeout: Duration::from_secs(1),
        };
        let backend = ResolveError::BackendFailed {
            backend: "fixture".to_owned(),
            message: "temporary upstream failure".to_owned(),
        };

        assert_eq!(timeout.retry_class(), RetryClass::Retryable);
        assert!(backend.is_retryable());
    }

    #[test]
    fn malformed_or_local_failures_are_permanent() {
        let spawn = ResolveError::Spawn {
            backend: "fixture".to_owned(),
            source: io::Error::new(io::ErrorKind::NotFound, "missing binary"),
        };

        assert_eq!(spawn.retry_class(), RetryClass::Permanent);
        assert_eq!(ResolveError::NoResults.retry_class(), RetryClass::Permanent);
        assert_eq!(
            ResolveError::InvalidMediaUrl.retry_class(),
            RetryClass::Permanent
        );
    }
}
