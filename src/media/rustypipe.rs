//! Native YouTube resolution. The queue stores public watch URLs, not CDN URLs.
use std::{future::Future, net::IpAddr, time::Duration};

use rustypipe::{
    client::RustyPipe,
    error::{Error as RustyPipeError, ExtractionError},
    model::{Thumbnail, VideoItem},
};
use tokio::time::timeout;

use super::{
    MAX_PLAYLIST_ITEMS, PlayableMedia, ResolveError, ResolveFuture, ResolvedTrack, Track,
    TrackMetadata, TrackRequest, TrackResolver, TrackSource,
};

const BACKEND: &str = "rustypipe";
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);

/// Native YouTube resolver.
#[derive(Default)]
pub struct RustyPipeResolver {
    client: RustyPipe,
    ipv4_only: bool,
}

impl RustyPipeResolver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict the resolver HTTP client to the local IPv4 address family.
    pub fn ipv4_only() -> Result<Self, ResolveError> {
        let address = IpAddr::from([0, 0, 0, 0]);
        let builder = reqwest12::Client::builder().local_address(address);
        let client = RustyPipe::builder()
            .build_with_client(builder)
            .map_err(upstream_error)?;
        Ok(Self {
            client,
            ipv4_only: true,
        })
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<ResolvedTrack>, ResolveError> {
        let result = self
            .client
            .query()
            .search::<VideoItem, _>(query)
            .await
            .map_err(upstream_error)?;
        collect_items(result.items.items, limit)
    }

    async fn playlist(&self, id: &str, limit: usize) -> Result<Vec<ResolvedTrack>, ResolveError> {
        let query = self.client.query();
        let mut playlist = query.playlist(id).await.map_err(upstream_error)?;
        if playlist.videos.items.len() < limit {
            // Keep the first page even if a continuation request fails.
            let _ = playlist.videos.extend_limit(query, limit).await;
        }
        collect_items(playlist.videos.items, limit)
    }

    async fn video(&self, id: &str) -> Result<ResolvedTrack, ResolveError> {
        let player = self
            .client
            .query()
            .player(id)
            .await
            .map_err(upstream_error)?;
        let details = player.details;
        let title = details
            .name
            .filter(|title| !title.trim().is_empty())
            .ok_or(ResolveError::MissingField { field: "title" })?;
        Ok(resolved_track(
            &details.id,
            title,
            details.channel_name,
            (details.duration > 0).then_some(details.duration),
            artwork(&details.thumbnail),
        ))
    }

    async fn resolve_many(
        &self,
        request: &TrackRequest,
        limit: usize,
        prefer_playlist: bool,
    ) -> Result<Vec<ResolvedTrack>, ResolveError> {
        let limit = limit.clamp(1, MAX_PLAYLIST_ITEMS);
        match target(request.input(), prefer_playlist)? {
            Target::Search(query) => self.search(&query, limit).await,
            Target::Playlist(id) => self.playlist(&id, limit).await,
            Target::Video(id) => self.video(&id).await.map(|track| vec![track]),
        }
    }

    async fn media(&self, track: &Track) -> Result<PlayableMedia, ResolveError> {
        let Target::Video(id) = target(&track.locator, false)? else {
            return Err(ResolveError::InvalidMediaUrl);
        };
        let query = self.client.query();
        let player = query.player(id).await.map_err(upstream_error)?;
        // Do not handle encrypted or DRM-protected media.
        if player.drm.is_some() {
            return Err(ResolveError::UnsupportedSource);
        }
        let audio_url = player
            .audio_streams
            .iter()
            .filter(|stream| stream.drm_track_type.is_none() && stream.drm_systems.is_empty())
            .filter(|stream| valid_media_url(&stream.url))
            .max_by_key(|stream| stream.bitrate)
            .map(|stream| stream.url.as_str());
        let url = audio_url
            .or_else(|| {
                player
                    .hls_manifest_url
                    .as_deref()
                    .filter(|url| valid_media_url(url))
            })
            .ok_or(ResolveError::InvalidMediaUrl)?;
        // Preserve the request context used by RustyPipe's own downloader.
        let media = PlayableMedia::new(url).with_http_headers([
            (
                "User-Agent".to_owned(),
                query.user_agent(player.client_type).into_owned(),
            ),
            ("Origin".to_owned(), "https://www.youtube.com".to_owned()),
            ("Referer".to_owned(), "https://www.youtube.com/".to_owned()),
        ]);
        if audio_url.is_some() {
            self.check_media_access(&media).await?;
        }
        Ok(media)
    }

    // A single small range request checks the direct stream without buffering it.
    // Do not follow redirects here or retry explicit access refusals.
    async fn check_media_access(&self, media: &PlayableMedia) -> Result<(), ResolveError> {
        let mut builder = reqwest12::Client::builder()
            .redirect(reqwest12::redirect::Policy::none())
            .timeout(Duration::from_secs(5));
        if self.ipv4_only {
            builder = builder.local_address(IpAddr::from([0, 0, 0, 0]));
        }
        let client = builder.build().map_err(|_| ResolveError::BackendFailed {
            backend: BACKEND.to_owned(),
            message: "could not initialize media HTTP client".to_owned(),
        })?;
        let mut request = client.get(&media.url).header("Range", "bytes=0-0");
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
        if status.is_client_error() || status.is_server_error() {
            tracing::warn!(status = status.as_u16(), "media access check rejected");
            return Err(ResolveError::MediaHttpStatus(status.as_u16()));
        }
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

impl TrackResolver for RustyPipeResolver {
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

// Avoid including signed URLs, request bodies or upstream credentials in
// user-visible errors or logs.
fn upstream_error(error: RustyPipeError) -> ResolveError {
    let message = match error {
        RustyPipeError::Extraction(ExtractionError::InvalidData(message))
            if message.contains("visitor data") =>
        {
            "YouTube did not provide visitor session data; playback is unavailable"
        }
        RustyPipeError::Extraction(ExtractionError::Unavailable { .. }) => {
            "YouTube reports that this content is unavailable"
        }
        RustyPipeError::HttpStatus(429, _) => "YouTube is rate limiting requests",
        RustyPipeError::HttpStatus(403, _) => "YouTube refused the request",
        _ => "YouTube request failed",
    };
    tracing::warn!(backend = BACKEND, reason = message, "media resolution failed");
    ResolveError::BackendFailed {
        backend: BACKEND.to_owned(),
        message: message.to_owned(),
    }
}

fn artwork(thumbnails: &[Thumbnail]) -> Option<String> {
    thumbnails
        .iter()
        .max_by_key(|thumbnail| u64::from(thumbnail.width) * u64::from(thumbnail.height))
        .map(|thumbnail| thumbnail.url.clone())
}

fn resolved_track(
    id: &str,
    title: String,
    artist: Option<String>,
    duration: Option<u32>,
    artwork_url: Option<String>,
) -> ResolvedTrack {
    let locator = format!("https://www.youtube.com/watch?v={id}");
    ResolvedTrack {
        source: TrackSource::YouTube,
        metadata: TrackMetadata {
            title,
            artist,
            duration: duration.map(|seconds| Duration::from_secs(u64::from(seconds))),
            artwork_url,
            webpage_url: locator.clone(),
        },
        locator,
    }
}

fn collect_items(items: Vec<VideoItem>, limit: usize) -> Result<Vec<ResolvedTrack>, ResolveError> {
    let tracks: Vec<_> = items
        .into_iter()
        .filter(|video| {
            valid_video_id(&video.id) && !video.name.trim().is_empty() && !video.is_upcoming
        })
        .take(limit)
        .map(|video| {
            resolved_track(
                &video.id,
                video.name,
                video.channel.map(|channel| channel.name),
                video.duration,
                artwork(&video.thumbnail),
            )
        })
        .collect();
    if tracks.is_empty() {
        Err(ResolveError::NoResults)
    } else {
        Ok(tracks)
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
    async fn ipv4_resolver_can_be_constructed() {
        assert!(RustyPipeResolver::ipv4_only().is_ok());
    }

    #[test]
    fn visitor_data_error_is_recoverable() {
        let error = upstream_error(RustyPipeError::Extraction(ExtractionError::InvalidData(
            "Could not find visitor data on html page".into(),
        )));
        assert!(matches!(
            error,
            ResolveError::BackendFailed { message, .. }
                if message.contains("visitor session data")
        ));
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
