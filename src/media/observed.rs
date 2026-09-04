use std::{sync::Arc, time::Instant};

use tokio::sync::Semaphore;

use crate::{metrics::Metrics, redaction::redact_sensitive};

use super::{
    PlayableMedia, ResolveError, ResolveFuture, ResolvedTrack, Track, TrackRequest, TrackResolver,
};

/// Default process-wide cap for concurrent resolver/backend operations.
pub const MAX_RESOLUTION_CONCURRENCY: usize = 8;

/// Decorates a resolver with bounded concurrency, redaction, and latency/failure metrics.
pub struct ObservedResolver {
    inner: Arc<dyn TrackResolver>,
    metrics: Arc<Metrics>,
    permits: Arc<Semaphore>,
}

impl std::fmt::Debug for ObservedResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservedResolver")
            .field("available_permits", &self.permits.available_permits())
            .finish_non_exhaustive()
    }
}

impl ObservedResolver {
    #[must_use]
    pub fn new(
        inner: Arc<dyn TrackResolver>,
        metrics: Arc<Metrics>,
        max_concurrency: usize,
    ) -> Self {
        assert!(max_concurrency > 0, "resolver concurrency must be non-zero");
        Self {
            inner,
            metrics,
            permits: Arc::new(Semaphore::new(max_concurrency)),
        }
    }

    async fn instrument<T>(&self, future: ResolveFuture<'_, T>) -> Result<T, ResolveError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .expect("resolver semaphore is never closed");
        let started = Instant::now();
        let result = future.await.map_err(sanitize_error);
        self.metrics.record_resolve_latency(started.elapsed());
        if result.is_err() {
            self.metrics.increment_failures();
        }
        result
    }
}

impl TrackResolver for ObservedResolver {
    fn resolve<'a>(&'a self, request: &'a TrackRequest) -> ResolveFuture<'a, ResolvedTrack> {
        Box::pin(async move { self.instrument(self.inner.resolve(request)).await })
    }

    fn resolve_playlist<'a>(
        &'a self,
        request: &'a TrackRequest,
        limit: usize,
    ) -> ResolveFuture<'a, Vec<ResolvedTrack>> {
        Box::pin(async move {
            self.instrument(self.inner.resolve_playlist(request, limit))
                .await
        })
    }

    fn resolve_media<'a>(&'a self, track: &'a Track) -> ResolveFuture<'a, PlayableMedia> {
        Box::pin(async move { self.instrument(self.inner.resolve_media(track)).await })
    }
}

fn sanitize_error(error: ResolveError) -> ResolveError {
    match error {
        ResolveError::BackendFailed { backend, message } => ResolveError::BackendFailed {
            backend,
            message: redact_sensitive(&message),
        },
        error => error,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::time::sleep;

    use super::ObservedResolver;
    use crate::{
        media::{
            PlayableMedia, ResolveError, ResolveFuture, ResolvedTrack, Track, TrackRequest,
            TrackResolver,
        },
        metrics::Metrics,
    };

    struct BlockingResolver {
        active: AtomicUsize,
        maximum: AtomicUsize,
    }

    impl BlockingResolver {
        fn new() -> Self {
            Self {
                active: AtomicUsize::new(0),
                maximum: AtomicUsize::new(0),
            }
        }

        async fn enter(&self) -> Result<ResolvedTrack, ResolveError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            sleep(Duration::from_millis(20)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Err(ResolveError::NoResults)
        }
    }

    impl TrackResolver for BlockingResolver {
        fn resolve<'a>(&'a self, _request: &'a TrackRequest) -> ResolveFuture<'a, ResolvedTrack> {
            Box::pin(self.enter())
        }

        fn resolve_media<'a>(&'a self, _track: &'a Track) -> ResolveFuture<'a, PlayableMedia> {
            Box::pin(async { Err(ResolveError::NoResults) })
        }
    }

    struct LeakyResolver;

    impl TrackResolver for LeakyResolver {
        fn resolve<'a>(&'a self, _request: &'a TrackRequest) -> ResolveFuture<'a, ResolvedTrack> {
            Box::pin(async {
                Err(ResolveError::BackendFailed {
                    backend: "fixture".to_owned(),
                    message: "failed https://cdn.example.test/media?sig=super-secret".to_owned(),
                })
            })
        }

        fn resolve_media<'a>(&'a self, _track: &'a Track) -> ResolveFuture<'a, PlayableMedia> {
            Box::pin(async { Err(ResolveError::NoResults) })
        }
    }

    #[tokio::test]
    async fn bounds_concurrent_resolution_and_records_failures() {
        let inner = Arc::new(BlockingResolver::new());
        let metrics = Arc::new(Metrics::new());
        let resolver = Arc::new(ObservedResolver::new(inner.clone(), metrics.clone(), 2));
        let request = TrackRequest::new("fixture").expect("request");

        let mut tasks = Vec::new();
        for _ in 0..6 {
            let resolver = resolver.clone();
            let request = request.clone();
            tasks.push(tokio::spawn(async move {
                let _ = resolver.resolve(&request).await;
            }));
        }
        for task in tasks {
            task.await.expect("resolver task");
        }

        assert_eq!(inner.maximum.load(Ordering::SeqCst), 2);
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.resolves, 6);
        assert_eq!(snapshot.failures, 6);
    }

    #[tokio::test]
    async fn sanitizes_backend_errors_before_returning_them() {
        let resolver = ObservedResolver::new(
            Arc::new(LeakyResolver),
            Arc::new(Metrics::new()),
            1,
        );
        let request = TrackRequest::new("fixture").expect("request");

        let error = resolver.resolve(&request).await.expect_err("resolver error");
        let message = error.to_string();
        assert!(message.contains("[REDACTED_URL]"));
        assert!(!message.contains("super-secret"));
        assert!(!message.contains("cdn.example.test"));
    }
}
