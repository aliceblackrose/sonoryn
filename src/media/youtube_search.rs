//! Lightweight YouTube search metadata, independent of media extraction.

use std::{collections::HashSet, time::Duration};

use serde_json::Value;

use super::{MAX_PLAYLIST_ITEMS, ResolveError, ResolvedTrack, TrackMetadata, TrackSource};

pub(super) async fn search(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
    budget: Duration,
) -> Result<Vec<ResolvedTrack>, ResolveError> {
    let body = tokio::time::timeout(budget, async {
        client
            .get("https://www.youtube.com/results")
            .query(&[("search_query", query)])
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await?
            .error_for_status()?
            .text()
            .await
    })
    .await
    .map_err(|_| ResolveError::TimedOut {
        backend: "YouTube search".to_owned(),
        timeout: budget,
    })?
    .map_err(|error| ResolveError::BackendFailed {
        backend: "YouTube search".to_owned(),
        message: error.without_url().to_string(),
    })?;

    parse_search_page(&body, limit.clamp(1, MAX_PLAYLIST_ITEMS))
}

fn parse_search_page(body: &str, limit: usize) -> Result<Vec<ResolvedTrack>, ResolveError> {
    let json = ["var ytInitialData =", "window[\"ytInitialData\"] ="]
        .into_iter()
        .find_map(|marker| body.split_once(marker).map(|(_, json)| json.trim_start()))
        .ok_or(ResolveError::NoResults)?;
    // Deserialize the JSON object only; the surrounding script continues afterward.
    let data = serde_json::Deserializer::from_str(json)
        .into_iter::<Value>()
        .next()
        .ok_or(ResolveError::NoResults)??;
    let sections = data
        .pointer(
            "/contents/twoColumnSearchResultsRenderer/primaryContents/sectionListRenderer/contents",
        )
        .and_then(Value::as_array)
        .ok_or(ResolveError::NoResults)?;

    let mut seen = HashSet::new();
    Ok(sections
        .iter()
        .filter_map(|section| section.pointer("/itemSectionRenderer/contents"))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|item| item.get("videoRenderer"))
        .filter_map(parse_video)
        .filter(|track| seen.insert(track.locator.clone()))
        .take(limit)
        .collect())
}

fn parse_video(video: &Value) -> Option<ResolvedTrack> {
    let id = video.get("videoId")?.as_str()?;
    if id.len() != 11
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    let title = renderer_text(video.get("title")?)?;
    let artist = ["ownerText", "longBylineText", "shortBylineText"]
        .into_iter()
        .filter_map(|field| video.get(field))
        .find_map(renderer_text);
    let webpage_url = format!("https://www.youtube.com/watch?v={id}");

    Some(ResolvedTrack {
        source: TrackSource::YouTube,
        metadata: TrackMetadata {
            title,
            artist,
            duration: None,
            artwork_url: None,
            webpage_url: webpage_url.clone(),
        },
        locator: webpage_url,
    })
}

fn renderer_text(value: &Value) -> Option<String> {
    let text = if let Some(text) = value.get("simpleText").and_then(Value::as_str) {
        text.to_owned()
    } else {
        value
            .get("runs")?
            .as_array()?
            .iter()
            .filter_map(|run| run.get("text").and_then(Value::as_str))
            .collect::<String>()
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_search_page;

    #[test]
    fn real_video_titles_and_urls_are_returned_in_search_order() {
        let first = json!({
            "videoRenderer": {
                "videoId": "dQw4w9WgXcQ",
                "title": {"runs": [{"text": "Never Gonna "}, {"text": "Give You Up"}]},
                "ownerText": {"runs": [{"text": "Rick Astley"}]}
            }
        });
        let second = json!({
            "videoRenderer": {
                "videoId": "abcdefghijk",
                "title": {"simpleText": "Another song"}
            }
        });
        let page = json!({
            "contents": {
                "twoColumnSearchResultsRenderer": {
                    "primaryContents": {
                        "sectionListRenderer": {
                            "contents": [{"itemSectionRenderer": {
                                "contents": [
                                    {"channelRenderer": {"title": {"simpleText": "Not a song"}}},
                                    first.clone(),
                                    first,
                                    {"videoRenderer": {"videoId": "invalid", "title": {"simpleText": "Bad ID"}}},
                                    second
                                ]
                            }}]
                        }
                    }
                }
            }
        });
        let html = format!("<script>var ytInitialData = {page};</script>");
        let tracks = parse_search_page(&html, 10).expect("search results");
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].metadata.title, "Never Gonna Give You Up");
        assert_eq!(tracks[0].metadata.artist.as_deref(), Some("Rick Astley"));
        assert_eq!(
            tracks[0].metadata.webpage_url,
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert_eq!(tracks[1].metadata.title, "Another song");
        assert_eq!(
            parse_search_page(&html, 1).expect("limited results").len(),
            1
        );
    }

    #[test]
    fn missing_search_data_is_not_a_track() {
        assert!(parse_search_page("<html>Consent required</html>", 10).is_err());
    }
}
