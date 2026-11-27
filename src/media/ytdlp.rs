use std::{
    path::PathBuf,
    process::{Output, Stdio},
    time::Duration,
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, BufReader},
    process::Command,
    time::{Instant, timeout, timeout_at},
};

use super::{
    MAX_PLAYLIST_ITEMS, PlayableMedia, ResolveError, ResolveFuture, ResolvedTrack, Track,
    TrackMetadata, TrackRequest, TrackResolver, TrackSource, youtube_search,
};

const DEFAULT_RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
const STDERR_LIMIT: usize = 2_048;

/// `yt-dlp` backed resolver for URLs and free-text search.
///
/// Metadata resolution and playable-media resolution are deliberately separate:
/// the first returns a stable public locator suitable for queue state, while
/// the second refreshes the direct media URL immediately before decoding.
#[derive(Debug, Clone)]
pub struct YtDlpResolver {
    binary: PathBuf,
    timeout: Duration,
    search_client: reqwest::Client,
}

impl Default for YtDlpResolver {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("yt-dlp"),
            timeout: DEFAULT_RESOLVE_TIMEOUT,
            search_client: reqwest::Client::new(),
        }
    }
}

impl YtDlpResolver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn metadata(&self, request: &TrackRequest) -> Result<ResolvedTrack, ResolveError> {
        let target = resolution_target(request.input());
        let json = self
            .run_json([
                "--dump-single-json",
                "--no-warnings",
                "--skip-download",
                "--no-playlist",
                "--",
                target.as_str(),
            ])
            .await?;
        parse_resolved_track(json, request)
    }

    async fn playlist(
        &self,
        request: &TrackRequest,
        limit: usize,
    ) -> Result<Vec<ResolvedTrack>, ResolveError> {
        let limit = limit.clamp(1, MAX_PLAYLIST_ITEMS);
        let limit_arg = limit.to_string();
        let target = request.input().trim();
        let json = if is_search_target(target) {
            self.run_json([
                "--dump-single-json",
                "--no-warnings",
                "--skip-download",
                "--flat-playlist",
                "--yes-playlist",
                "--playlist-end",
                limit_arg.as_str(),
                "--",
                target,
            ])
            .await?
        } else {
            self.run_json([
                "--dump-single-json",
                "--no-warnings",
                "--skip-download",
                "--yes-playlist",
                "--playlist-end",
                limit_arg.as_str(),
                "--",
                target,
            ])
            .await?
        };
        parse_playlist_tracks(json, request, limit)
    }

    async fn search_suggestions(
        &self,
        request: &TrackRequest,
        limit: usize,
        budget: Duration,
    ) -> Result<Vec<ResolvedTrack>, ResolveError> {
        let deadline = Instant::now() + budget.min(self.timeout);
        let limit = limit.clamp(1, MAX_PLAYLIST_ITEMS);
        let limit_arg = limit.to_string();
        let backend = self.binary.display().to_string();
        let mut child = Command::new(&self.binary)
            .args([
                "--dump-json",
                "--no-warnings",
                "--skip-download",
                "--flat-playlist",
                "--lazy-playlist",
                "--yes-playlist",
                "--ignore-errors",
                "--playlist-end",
                limit_arg.as_str(),
                "--",
                request.input(),
            ])
            .env("PYTHONUNBUFFERED", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| ResolveError::Spawn {
                backend: backend.clone(),
                source,
            })?;
        let stdout = child.stdout.take().expect("stdout was piped");
        // Dropping the child stops unfinished searches after the deadline.
        collect_search_tracks(BufReader::new(stdout), limit, deadline, &backend).await
    }

    async fn media(&self, track: &Track) -> Result<PlayableMedia, ResolveError> {
        let json = self
            .run_json([
                "--dump-single-json",
                "--no-warnings",
                "--skip-download",
                "--no-playlist",
                "--format",
                "bestaudio/best",
                "--",
                track.locator.as_str(),
            ])
            .await?;
        parse_playable_media(json)
    }

    async fn run_json<'a>(
        &self,
        args: impl IntoIterator<Item = &'a str>,
    ) -> Result<Value, ResolveError> {
        let backend = self.binary.display().to_string();
        let mut command = Command::new(&self.binary);
        command.args(args).kill_on_drop(true);

        let output = timeout(self.timeout, command.output())
            .await
            .map_err(|_| ResolveError::TimedOut {
                backend: backend.clone(),
                timeout: self.timeout,
            })?
            .map_err(|source| ResolveError::Spawn {
                backend: backend.clone(),
                source,
            })?;

        validate_output(&backend, output)
    }
}

impl TrackResolver for YtDlpResolver {
    fn resolve<'a>(&'a self, request: &'a TrackRequest) -> ResolveFuture<'a, ResolvedTrack> {
        Box::pin(async move { self.metadata(request).await })
    }

    fn resolve_playlist<'a>(
        &'a self,
        request: &'a TrackRequest,
        limit: usize,
    ) -> ResolveFuture<'a, Vec<ResolvedTrack>> {
        Box::pin(async move { self.playlist(request, limit).await })
    }

    fn suggest<'a>(
        &'a self,
        request: &'a TrackRequest,
        limit: usize,
        budget: Duration,
    ) -> ResolveFuture<'a, Vec<ResolvedTrack>> {
        Box::pin(async move {
            let Some((_, query)) = request.input().split_once(':') else {
                return self.search_suggestions(request, limit, budget).await;
            };
            if !is_search_target(request.input()) {
                return self.search_suggestions(request, limit, budget).await;
            }

            // The HTTP path avoids Python startup and per-video extraction.
            // Keep yt-dlp available if YouTube changes its public search page.
            let direct = youtube_search::search(&self.search_client, query, limit, budget);
            let fallback = self.search_suggestions(request, limit, budget);
            tokio::pin!(direct, fallback);
            tokio::select! {
                result = &mut direct => match result {
                    Ok(tracks) if !tracks.is_empty() => Ok(tracks),
                    _ => fallback.await,
                },
                result = &mut fallback => match result {
                    Ok(tracks) if !tracks.is_empty() => Ok(tracks),
                    _ => direct.await,
                },
            }
        })
    }

    fn resolve_media<'a>(&'a self, track: &'a Track) -> ResolveFuture<'a, PlayableMedia> {
        Box::pin(async move { self.media(track).await })
    }
}

async fn collect_search_tracks(
    reader: impl AsyncBufRead + Unpin,
    limit: usize,
    deadline: Instant,
    backend: &str,
) -> Result<Vec<ResolvedTrack>, ResolveError> {
    let mut lines = reader.lines();
    let mut tracks = Vec::with_capacity(limit);
    while tracks.len() < limit {
        match timeout_at(deadline, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && let Ok(track) = parse_resolved_value(&value, None)
                {
                    tracks.push(track);
                }
            }
            Ok(Err(error)) if tracks.is_empty() => {
                return Err(ResolveError::BackendFailed {
                    backend: backend.to_owned(),
                    message: format!("could not read search results: {error}"),
                });
            }
            _ => break,
        }
    }
    Ok(tracks)
}

fn validate_output(backend: &str, output: Output) -> Result<Value, ResolveError> {
    if !output.status.success() {
        return Err(ResolveError::BackendFailed {
            backend: backend.to_owned(),
            message: bounded_stderr(&output.stderr),
        });
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn bounded_stderr(stderr: &[u8]) -> String {
    let end = stderr.len().min(STDERR_LIMIT);
    let mut message = String::from_utf8_lossy(&stderr[..end]).trim().to_owned();
    if stderr.len() > STDERR_LIMIT {
        message.push('…');
    }
    if message.is_empty() {
        message = "backend returned a non-zero exit status without stderr".to_owned();
    }
    message
}

fn resolution_target(input: &str) -> String {
    let input = input.trim();
    if is_http_url(input) {
        input.to_owned()
    } else {
        format!("ytsearch1:{input}")
    }
}

fn is_http_url(input: &str) -> bool {
    reqwest::Url::parse(input).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn is_search_target(input: &str) -> bool {
    input
        .split_once(':')
        .is_some_and(|(prefix, query)| prefix.starts_with("ytsearch") && !query.trim().is_empty())
}

fn primary_entry(value: Value) -> Result<Value, ResolveError> {
    let Some(entries) = value.get("entries") else {
        return Ok(value);
    };
    let Some(entries) = entries.as_array() else {
        return Err(ResolveError::NoResults);
    };
    entries
        .iter()
        .find(|entry| !entry.is_null())
        .cloned()
        .ok_or(ResolveError::NoResults)
}

fn parse_resolved_track(
    value: Value,
    request: &TrackRequest,
) -> Result<ResolvedTrack, ResolveError> {
    let value = primary_entry(value)?;
    parse_resolved_value(&value, Some(request.input()))
}

fn parse_playlist_tracks(
    value: Value,
    request: &TrackRequest,
    limit: usize,
) -> Result<Vec<ResolvedTrack>, ResolveError> {
    let Some(entries) = value.get("entries") else {
        return parse_resolved_track(value, request).map(|track| vec![track]);
    };
    let Some(entries) = entries.as_array() else {
        return Err(ResolveError::NoResults);
    };

    let mut tracks = Vec::with_capacity(limit.min(entries.len()));
    let mut first_error = None;
    for entry in entries.iter().filter(|entry| !entry.is_null()).take(limit) {
        match parse_resolved_value(entry, None) {
            Ok(track) => tracks.push(track),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    if tracks.is_empty() {
        Err(first_error.unwrap_or(ResolveError::NoResults))
    } else {
        Ok(tracks)
    }
}

fn parse_resolved_value(
    value: &Value,
    fallback_url: Option<&str>,
) -> Result<ResolvedTrack, ResolveError> {
    let title = string_field(value, "title")?;
    if title.trim().is_empty() {
        return Err(ResolveError::MissingField { field: "title" });
    }
    let extractor = ["extractor_key", "extractor", "ie_key"]
        .into_iter()
        .find_map(|field| value.get(field).and_then(Value::as_str))
        .unwrap_or("unknown");
    let webpage_url = youtube_flat_url(value, extractor)
        .or_else(|| {
            ["webpage_url", "original_url", "url"]
                .into_iter()
                .filter_map(|field| value.get(field).and_then(Value::as_str))
                .find(|url| is_http_url(url))
                .map(str::to_owned)
        })
        .or_else(|| {
            fallback_url
                .filter(|url| is_http_url(url.trim()))
                .map(|url| url.trim().to_owned())
        })
        .ok_or(ResolveError::MissingField {
            field: "webpage_url",
        })?;

    let artist = ["artist", "uploader", "creator", "channel"]
        .into_iter()
        .find_map(|field| value.get(field).and_then(Value::as_str))
        .map(str::to_owned);
    let duration = value
        .get("duration")
        .and_then(Value::as_f64)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
    let artwork_url = value
        .get("thumbnail")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| last_thumbnail_url(value));
    Ok(ResolvedTrack {
        source: TrackSource::from_extractor(extractor),
        metadata: TrackMetadata {
            title,
            artist,
            duration,
            artwork_url,
            webpage_url: webpage_url.clone(),
        },
        locator: webpage_url,
    })
}

// Flat YouTube entries can contain a bare video ID instead of a webpage URL.
fn youtube_flat_url(value: &Value, extractor: &str) -> Option<String> {
    if !extractor.eq_ignore_ascii_case("youtube")
        || value.get("_type").and_then(Value::as_str) != Some("url")
    {
        return None;
    }

    ["id", "url"]
        .into_iter()
        .filter_map(|field| value.get(field).and_then(Value::as_str))
        .find(|id| {
            id.len() == 11
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .map(|id| format!("https://www.youtube.com/watch?v={id}"))
}

fn parse_playable_media(value: Value) -> Result<PlayableMedia, ResolveError> {
    let value = primary_entry(value)?;
    let url = value
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| is_http_url(url))
        .ok_or(ResolveError::InvalidMediaUrl)?;

    let headers = value
        .get("http_headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| value.as_str().map(|value| (name.clone(), value.to_owned())))
        .collect::<Vec<_>>();

    Ok(PlayableMedia::new(url).with_http_headers(headers))
}

fn string_field(value: &Value, field: &'static str) -> Result<String, ResolveError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(ResolveError::MissingField { field })
}

fn last_thumbnail_url(value: &Value) -> Option<String> {
    value
        .get("thumbnails")?
        .as_array()?
        .iter()
        .rev()
        .find_map(|thumbnail| thumbnail.get("url").and_then(Value::as_str))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{
        collect_search_tracks, is_search_target, parse_playable_media, parse_playlist_tracks,
        parse_resolved_track, resolution_target,
    };
    use crate::media::{TrackRequest, TrackSource};

    #[test]
    fn free_text_uses_one_result_search() {
        assert_eq!(
            resolution_target("  artist song  "),
            "ytsearch1:artist song"
        );
        assert_eq!(
            resolution_target("https://example.test/watch/1"),
            "https://example.test/watch/1"
        );
    }

    #[test]
    fn recognizes_bounded_youtube_search_targets() {
        assert!(is_search_target("ytsearch10:artist song"));
        assert!(is_search_target("ytsearch:artist song"));
        assert!(!is_search_target(
            "https://example.test/ytsearch10:artist song"
        ));
        assert!(!is_search_target("ytsearch10:"));
    }

    #[test]
    fn parses_search_entry_into_durable_metadata() {
        let request = TrackRequest::new("artist song").expect("request");
        let resolved = parse_resolved_track(
            json!({
                "entries": [{
                    "title": "Song",
                    "uploader": "Artist",
                    "duration": 61.5,
                    "thumbnail": "https://img.example/song.jpg",
                    "webpage_url": "https://www.youtube.com/watch?v=abc",
                    "extractor_key": "Youtube"
                }]
            }),
            &request,
        )
        .expect("resolved track");

        assert_eq!(resolved.source, TrackSource::YouTube);
        assert_eq!(resolved.metadata.title, "Song");
        assert_eq!(resolved.metadata.artist.as_deref(), Some("Artist"));
        assert_eq!(
            resolved.metadata.duration,
            Some(Duration::from_millis(61_500))
        );
        assert_eq!(resolved.locator, "https://www.youtube.com/watch?v=abc");
    }

    #[test]
    fn invalid_duration_metadata_cannot_panic_resolution() {
        let request = TrackRequest::new("song").expect("request");
        for duration in [1e100, -1.0] {
            let track = parse_resolved_track(
                json!({
                    "title": "Song",
                    "webpage_url": "https://example.test/song",
                    "duration": duration
                }),
                &request,
            )
            .expect("track");
            assert_eq!(track.metadata.duration, None);
        }
    }

    #[test]
    fn blank_title_is_not_a_playable_result() {
        let request = TrackRequest::new("song").expect("request");
        let result = parse_resolved_track(
            json!({"title": "  ", "webpage_url": "https://example.test/song"}),
            &request,
        );
        assert!(result.is_err());
    }

    #[test]
    fn flat_youtube_entries_resolve_bare_video_ids() {
        let request = TrackRequest::new("ytsearch10:song").expect("request");
        let tracks = parse_playlist_tracks(
            json!({
                "entries": [{
                    "_type": "url",
                    "ie_key": "Youtube",
                    "id": "dQw4w9WgXcQ",
                    "url": "dQw4w9WgXcQ",
                    "title": "Song"
                }]
            }),
            &request,
            10,
        )
        .expect("search results");

        assert_eq!(tracks[0].source, TrackSource::YouTube);
        assert_eq!(
            tracks[0].locator,
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }

    #[tokio::test]
    async fn search_retains_partial_results_when_backend_stalls() {
        use tokio::{
            io::{AsyncWriteExt, BufReader, duplex},
            time::Instant,
        };

        let (mut writer, reader) = duplex(1024);
        writer
            .write_all(
                b"invalid json\n{\"title\":\"Song\",\"url\":\"https://example.test/song\"}\n",
            )
            .await
            .expect("write fixture");

        // Keep the writer open so the next line never arrives.
        let tracks = collect_search_tracks(
            BufReader::new(reader),
            10,
            Instant::now() + Duration::from_millis(50),
            "fixture",
        )
        .await
        .expect("partial results");
        drop(writer);

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].metadata.title, "Song");
    }

    #[test]
    fn playlist_expansion_honors_the_item_limit() {
        let request = TrackRequest::new("https://example.test/playlist").expect("request");
        let tracks = parse_playlist_tracks(
            json!({
                "entries": [
                    {
                        "title": "One",
                        "webpage_url": "https://example.test/watch/1",
                        "extractor_key": "Youtube"
                    },
                    {
                        "title": "Two",
                        "webpage_url": "https://example.test/watch/2",
                        "extractor_key": "Youtube"
                    },
                    {
                        "title": "Three",
                        "webpage_url": "https://example.test/watch/3",
                        "extractor_key": "Youtube"
                    }
                ]
            }),
            &request,
            2,
        )
        .expect("playlist");

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].metadata.title, "One");
        assert_eq!(tracks[1].metadata.title, "Two");
    }

    #[test]
    fn playlist_expansion_skips_bad_entries_when_valid_tracks_remain() {
        let request = TrackRequest::new("https://example.test/playlist").expect("request");
        let tracks = parse_playlist_tracks(
            json!({
                "entries": [
                    {"title": "Missing URL"},
                    {
                        "title": "Playable",
                        "webpage_url": "https://example.test/watch/2"
                    }
                ]
            }),
            &request,
            25,
        )
        .expect("playlist");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].metadata.title, "Playable");
    }

    #[test]
    fn playable_media_keeps_signed_url_outside_track_metadata() {
        let media = parse_playable_media(json!({
            "url": "https://cdn.example.test/signed?expires=1",
            "http_headers": {
                "User-Agent": "Sonoryn fixture",
                "Referer": "https://example.test/"
            }
        }))
        .expect("playable media");

        assert_eq!(media.url, "https://cdn.example.test/signed?expires=1");
        assert_eq!(media.http_headers.len(), 2);
    }
}
