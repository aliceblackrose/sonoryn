//! Native YouTube resolution using Strawcore's synchronous extraction API.
use std::{
    future::Future,
    net::IpAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use strawcore_core::{
    NewPipe,
    downloader::ReqwestDownloader,
    image::Image,
    youtube::{linkhandler::search::SearchFilter, search_extractor, stream_extractor},
};
use tokio::{sync::Semaphore, time::timeout};

use super::{
    MAX_PLAYLIST_ITEMS, PlayableMedia, ResolveError, ResolveFuture, ResolvedTrack, Track,
    TrackMetadata, TrackRequest, TrackResolver, TrackSource,
};

const BACKEND: &str = "strawcore";
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

// The library's downloader is process-global. Build it on a blocking thread
// exactly once; the service's address-family setting is fixed at startup.
static INITIALIZED: OnceLock<Result<(), ()>> = OnceLock::new();
static EXTRACTION_SLOTS: Semaphore = Semaphore::const_new(4);

pub struct StrawcoreResolver {
    ipv4_only: bool,
}

impl Default for StrawcoreResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl StrawcoreResolver {
    #[must_use]
    pub fn new() -> Self {
        Self {
            ipv4_only: std::env::var("SONORYN_RESOLVER_IPV4_ONLY").as_deref() == Ok("1"),
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> Result<T, ResolveError> + Send + 'static,
    ) -> Result<T, ResolveError> {
        let permit = EXTRACTION_SLOTS
            .acquire()
            .await
            .map_err(|_| backend_failed("extractor worker unavailable"))?;
        let ipv4_only = self.ipv4_only;
        tokio::task::spawn_blocking(move || {
            // The permit stays with the worker even if the caller times out.
            let _permit = permit;
            INITIALIZED
                .get_or_init(|| {
                    let mut builder = reqwest12::blocking::Client::builder()
                        .timeout(HTTP_TIMEOUT)
                        .redirect(reqwest12::redirect::Policy::limited(5))
                        .gzip(true);
                    if ipv4_only {
                        builder = builder.local_address(IpAddr::from([0, 0, 0, 0]));
                    }
                    let client = builder.build().map_err(|_| ())?;
                    NewPipe::init(Arc::new(ReqwestDownloader::with_client(client)));
                    Ok(())
                })
                .as_ref()
                .map_err(|_| backend_failed("could not initialize extractor HTTP client"))?;
            operation()
        })
        .await
        .map_err(|_| backend_failed("extractor worker failed"))?
    }

    async fn search(
        &self,
        query: String,
        limit: usize,
    ) -> Result<Vec<ResolvedTrack>, ResolveError> {
        self.run(move || {
            let result = search_extractor::search(&query, SearchFilter::Videos)
                .map_err(|_| backend_failed("YouTube search failed"))?;
            let tracks: Vec<_> = result
                .videos
                .into_iter()
                .filter_map(|item| {
                    let Target::Video(id) = target(&item.url, false).ok()? else {
                        return None;
                    };
                    if item.name.trim().is_empty() {
                        return None;
                    }
                    Some(resolved_track(
                        &id,
                        item.name,
                        nonempty(item.uploader_name),
                        duration(item.duration_seconds),
                        artwork(&item.thumbnails),
                    ))
                })
                .take(limit)
                .collect();
            if tracks.is_empty() {
                return Err(ResolveError::NoResults);
            }
            Ok(tracks)
        })
        .await
    }

    async fn video(&self, id: String) -> Result<ResolvedTrack, ResolveError> {
        self.run(move || {
            let info = stream_extractor::stream_metadata(&id)
                .map_err(|_| backend_failed("YouTube metadata request failed"))?;
            if info.video_id != id || info.name.trim().is_empty() {
                return Err(ResolveError::MissingField { field: "title" });
            }
            Ok(resolved_track(
                &id,
                info.name,
                nonempty(info.uploader_name),
                duration(info.duration_seconds),
                artwork(&info.thumbnails),
            ))
        })
        .await
    }

    async fn resolve_many(
        &self,
        request: &TrackRequest,
        limit: usize,
        prefer_playlist: bool,
    ) -> Result<Vec<ResolvedTrack>, ResolveError> {
        let limit = limit.clamp(1, MAX_PLAYLIST_ITEMS);
        match target(request.input(), prefer_playlist)? {
            Target::Search(query) => self.search(query, limit).await,
            Target::Video(id) => self.video(id).await.map(|track| vec![track]),
            Target::Playlist(_) => Err(backend_failed(
                "YouTube playlist expansion is not supported by this extractor",
            )),
        }
    }

    async fn media(&self, track: &Track) -> Result<PlayableMedia, ResolveError> {
        let Target::Video(id) = target(&track.locator, false)? else {
            return Err(ResolveError::InvalidMediaUrl);
        };
        let (media, direct) = self
            .run(move || {
                let info = stream_extractor::stream_info(&id)
                    .map_err(|_| backend_failed("YouTube stream extraction failed"))?;
                if info.video_id != id {
                    return Err(ResolveError::InvalidMediaUrl);
                }
                let audio = info
                    .audio_streams
                    .iter()
                    .filter(|stream| !stream.is_descriptive && valid_media_url(&stream.url))
                    .max_by_key(|stream| stream.average_bitrate_kbps.unwrap_or_default())
                    .map(|stream| stream.url.as_str());
                // A progressive muxed stream can carry audio when audio-only
                // formats are absent; FFmpeg drops the video track.
                let muxed = info
                    .video_streams
                    .iter()
                    .filter(|stream| !stream.video_only && valid_media_url(&stream.url))
                    .min_by_key(|stream| stream.bandwidth.unwrap_or(u32::MAX))
                    .map(|stream| stream.url.as_str());
                let direct = audio.or(muxed);
                let url = direct
                    .or_else(|| {
                        info.hls_manifest_url
                            .as_deref()
                            .filter(|url| valid_media_url(url))
                    })
                    .ok_or(ResolveError::InvalidMediaUrl)?;
                Ok((PlayableMedia::new(url), direct.is_some()))
            })
            .await?;
        self.check_media_access(&media, direct).await?;
        Ok(media)
    }

    // Check the direct stream or manifest without buffering the response.
    // Follow only bounded HTTPS redirects to the same trusted CDN domain.
    async fn check_media_access(
        &self,
        media: &PlayableMedia,
        direct: bool,
    ) -> Result<(), ResolveError> {
        let mut builder = reqwest12::Client::builder()
            .redirect(reqwest12::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 || !valid_media_url(attempt.url().as_str()) {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .timeout(Duration::from_secs(5));
        if self.ipv4_only {
            builder = builder.local_address(IpAddr::from([0, 0, 0, 0]));
        }
        let client = builder.build().map_err(|_| ResolveError::BackendFailed {
            backend: BACKEND.to_owned(),
            message: "could not initialize media HTTP client".to_owned(),
        })?;
        let mut request = client.get(&media.url);
        if direct {
            request = request.header("Range", "bytes=0-0");
        }
        for (name, value) in &media.http_headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .send()
            .await
            .map_err(|_| ResolveError::BackendFailed {
                backend: BACKEND.to_owned(),
                message: "media access check failed".to_owned(),
            })?;
        let status = response.status();
        if !status.is_success() {
            tracing::warn!(status = status.as_u16(), "media access check rejected");
            return Err(ResolveError::MediaHttpStatus(status.as_u16()));
        }
        tracing::info!(
            status = status.as_u16(),
            direct,
            "media access check succeeded"
        );
        Ok(())
    }

    async fn bounded<T>(
        &self,
        budget: Duration,
        operation: impl Future<Output = Result<T, ResolveError>>,
    ) -> Result<T, ResolveError> {
        timeout(budget, operation)
            .await
            .map_err(|_| ResolveError::TimedOut {
                backend: BACKEND.to_owned(),
                timeout: budget,
            })?
    }
}

impl TrackResolver for StrawcoreResolver {
    fn resolve<'a>(&'a self, request: &'a TrackRequest) -> ResolveFuture<'a, ResolvedTrack> {
        Box::pin(async move {
            self.bounded(RESOLVE_TIMEOUT, self.resolve_many(request, 1, false))
                .await
                .and_then(|tracks| tracks.into_iter().next().ok_or(ResolveError::NoResults))
        })
    }

    fn resolve_playlist<'a>(
        &'a self,
        request: &'a TrackRequest,
        limit: usize,
    ) -> ResolveFuture<'a, Vec<ResolvedTrack>> {
        Box::pin(async move {
            self.bounded(RESOLVE_TIMEOUT, self.resolve_many(request, limit, true))
                .await
        })
    }

    fn suggest<'a>(
        &'a self,
        request: &'a TrackRequest,
        limit: usize,
        budget: Duration,
    ) -> ResolveFuture<'a, Vec<ResolvedTrack>> {
        Box::pin(async move {
            self.bounded(
                budget.min(RESOLVE_TIMEOUT),
                self.resolve_many(request, limit, false),
            )
            .await
        })
    }

    fn resolve_media<'a>(&'a self, track: &'a Track) -> ResolveFuture<'a, PlayableMedia> {
        Box::pin(async move { self.bounded(RESOLVE_TIMEOUT, self.media(track)).await })
    }
}

fn backend_failed(message: &str) -> ResolveError {
    tracing::warn!(
        backend = BACKEND,
        reason = message,
        "media resolution failed"
    );
    ResolveError::BackendFailed {
        backend: BACKEND.to_owned(),
        message: message.to_owned(),
    }
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn duration(seconds: i64) -> Option<u64> {
    u64::try_from(seconds).ok().filter(|seconds| *seconds > 0)
}

fn artwork(images: &[Image]) -> Option<String> {
    images
        .iter()
        .max_by_key(|image| i64::from(image.width().max(0)) * i64::from(image.height().max(0)))
        .map(|image| image.url().to_owned())
}

fn resolved_track(
    id: &str,
    title: String,
    artist: Option<String>,
    duration: Option<u64>,
    artwork_url: Option<String>,
) -> ResolvedTrack {
    let locator = format!("https://www.youtube.com/watch?v={id}");
    ResolvedTrack {
        source: TrackSource::YouTube,
        metadata: TrackMetadata {
            title,
            artist,
            duration: duration.map(Duration::from_secs),
            artwork_url,
            webpage_url: locator.clone(),
        },
        locator,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Target {
    Search(String),
    Video(String),
    Playlist(String),
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_video_id(id: &str) -> bool {
    id.len() == 11 && valid_id(id)
}

fn target(input: &str, prefer_playlist: bool) -> Result<Target, ResolveError> {
    let input = input.trim();
    let Ok(url) = reqwest::Url::parse(input) else {
        return if input.is_empty() {
            Err(ResolveError::NoResults)
        } else {
            Ok(Target::Search(input.to_owned()))
        };
    };
    if !matches!(url.scheme(), "http" | "https") && !input.contains("://") {
        return Ok(Target::Search(input.to_owned()));
    }
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ResolveError::UnsupportedSource);
    }
    let host = url.host_str().ok_or(ResolveError::UnsupportedSource)?;
    if !matches!(
        host,
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"
    ) {
        return Err(ResolveError::UnsupportedSource);
    }
    let list = url
        .query_pairs()
        .find(|(key, value)| key == "list" && valid_id(value))
        .map(|(_, value)| value.into_owned());
    if prefer_playlist && let Some(id) = &list {
        return Ok(Target::Playlist(id.clone()));
    }
    let mut segments = url.path_segments().into_iter().flatten();
    let first = segments.next().unwrap_or_default();
    let video = if host == "youtu.be" {
        Some(first.to_owned())
    } else if matches!(first, "shorts" | "live" | "embed") {
        segments.next().map(str::to_owned)
    } else if first == "watch" {
        url.query_pairs()
            .find(|(key, _)| key == "v")
            .map(|(_, value)| value.into_owned())
    } else {
        None
    };
    if let Some(id) = video.filter(|id| valid_video_id(id)) {
        return Ok(Target::Video(id));
    }
    list.map(Target::Playlist)
        .ok_or(ResolveError::UnsupportedSource)
}

fn valid_media_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url
                .host_str()
                .is_some_and(|host| host == "googlevideo.com" || host.ends_with(".googlevideo.com"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn playlist_is_explicitly_unsupported() {
        let resolver = StrawcoreResolver::new();
        let request = TrackRequest::new("https://www.youtube.com/playlist?list=PL_example")
            .expect("valid request");
        let error = resolver.resolve_playlist(&request, 10).await.unwrap_err();
        assert!(error.to_string().contains("playlist expansion"));
    }

    #[test]
    fn parses_search_and_video_targets() {
        assert_eq!(
            target("artist song", false).unwrap(),
            Target::Search("artist song".to_owned())
        );
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
        ] {
            assert_eq!(
                target(url, false).unwrap(),
                Target::Video("dQw4w9WgXcQ".to_owned())
            );
        }
    }

    #[test]
    fn playlist_intent_takes_priority_only_when_requested() {
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL_example";
        assert_eq!(
            target(url, true).unwrap(),
            Target::Playlist("PL_example".to_owned())
        );
        assert_eq!(
            target(url, false).unwrap(),
            Target::Video("dQw4w9WgXcQ".to_owned())
        );
    }

    #[test]
    fn rejects_untrusted_hosts_and_invalid_media_urls() {
        assert!(
            target(
                "https://youtube.com.example.test/watch?v=dQw4w9WgXcQ",
                false
            )
            .is_err()
        );
        assert!(target("https://user@youtube.com/watch?v=dQw4w9WgXcQ", false).is_err());
        assert!(valid_media_url(
            "https://example.googlevideo.com/videoplayback"
        ));
        assert!(!valid_media_url(
            "https://googlevideo.com.example.test/videoplayback"
        ));
        assert!(!valid_media_url(
            "http://example.googlevideo.com/videoplayback"
        ));
    }

    #[test]
    fn durable_metadata_never_includes_stream_url() {
        let track = resolved_track("dQw4w9WgXcQ", "Example".to_owned(), None, Some(42), None);
        assert_eq!(track.locator, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(track.metadata.webpage_url, track.locator);
        assert_eq!(track.metadata.duration, Some(Duration::from_secs(42)));
    }
}
