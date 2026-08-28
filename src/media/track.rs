use std::time::Duration;

use gloamwire::model::UserId;

/// Stable identifier assigned by Sonoryn when a resolved track enters a player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(u64);

impl TrackId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The exact user submission that produced a track resolution request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrackRequest {
    input: String,
}

impl TrackRequest {
    pub fn new(input: impl Into<String>) -> Result<Self, &'static str> {
        let input = input.into();
        if input.trim().is_empty() {
            return Err("track request cannot be empty");
        }
        Ok(Self { input })
    }

    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

/// Discord user that requested a queued track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestedBy {
    pub user_id: UserId,
}

impl RequestedBy {
    #[must_use]
    pub const fn new(user_id: UserId) -> Self {
        Self { user_id }
    }
}

/// Origin service reported by the resolver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrackSource {
    YouTube,
    SoundCloud,
    Bandcamp,
    Twitch,
    Vimeo,
    Other(String),
}

impl TrackSource {
    #[must_use]
    pub fn from_extractor(extractor: &str) -> Self {
        let normalized = extractor.to_ascii_lowercase();
        if normalized.contains("youtube") {
            Self::YouTube
        } else if normalized.contains("soundcloud") {
            Self::SoundCloud
        } else if normalized.contains("bandcamp") {
            Self::Bandcamp
        } else if normalized.contains("twitch") {
            Self::Twitch
        } else if normalized.contains("vimeo") {
            Self::Vimeo
        } else {
            Self::Other(extractor.to_owned())
        }
    }
}

/// Metadata safe to retain in queue or persistence state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: Option<String>,
    pub duration: Option<Duration>,
    pub artwork_url: Option<String>,
    pub webpage_url: String,
}

/// Resolver output before the player assigns an application-local track ID.
///
/// `locator` must be a stable/public source locator such as a webpage URL. It
/// must not be a short-lived signed CDN/media URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTrack {
    pub source: TrackSource,
    pub metadata: TrackMetadata,
    pub locator: String,
}

/// Durable queue entry.
///
/// Direct playable media URLs deliberately do not live here. They are resolved
/// on demand into [`PlayableMedia`] immediately before decode/playback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: TrackId,
    pub request: TrackRequest,
    pub source: TrackSource,
    pub requested_by: RequestedBy,
    pub metadata: TrackMetadata,
    pub locator: String,
}

impl Track {
    #[must_use]
    pub fn from_resolved(
        id: TrackId,
        request: TrackRequest,
        requested_by: RequestedBy,
        resolved: ResolvedTrack,
    ) -> Self {
        Self {
            id,
            request,
            source: resolved.source,
            requested_by,
            metadata: resolved.metadata,
            locator: resolved.locator,
        }
    }
}

/// Ephemeral direct-media information used only by the decoder boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayableMedia {
    pub url: String,
    pub http_headers: Vec<(String, String)>,
}

impl PlayableMedia {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            http_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_http_headers(
        mut self,
        headers: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.http_headers = headers.into_iter().collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gloamwire::model::UserId;

    use super::{RequestedBy, ResolvedTrack, Track, TrackId, TrackMetadata, TrackRequest, TrackSource};

    #[test]
    fn creates_durable_track_without_playable_media_state() {
        let request = TrackRequest::new("never gonna give you up").expect("request");
        let resolved = ResolvedTrack {
            source: TrackSource::YouTube,
            metadata: TrackMetadata {
                title: "Example".to_owned(),
                artist: Some("Artist".to_owned()),
                duration: Some(Duration::from_secs(42)),
                artwork_url: Some("https://img.example/cover.jpg".to_owned()),
                webpage_url: "https://example.test/watch/1".to_owned(),
            },
            locator: "https://example.test/watch/1".to_owned(),
        };
        let track = Track::from_resolved(
            TrackId::new(7),
            request,
            RequestedBy::new(UserId::new(9)),
            resolved,
        );

        assert_eq!(track.id.get(), 7);
        assert_eq!(track.request.input(), "never gonna give you up");
        assert_eq!(track.metadata.duration, Some(Duration::from_secs(42)));
        assert_eq!(track.locator, track.metadata.webpage_url);
    }

    #[test]
    fn classifies_common_extractors() {
        assert_eq!(TrackSource::from_extractor("Youtube"), TrackSource::YouTube);
        assert_eq!(
            TrackSource::from_extractor("soundcloud:set"),
            TrackSource::SoundCloud
        );
        assert_eq!(
            TrackSource::from_extractor("custom"),
            TrackSource::Other("custom".to_owned())
        );
    }
}
