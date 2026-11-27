use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use gloamwire::voice::{VoiceOpusFrame, VoiceOpusFrameDuration, VoiceResult};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, ChildStdout, Command},
};

use super::PlayableMedia;

const MAX_OGG_PACKET_BYTES: usize = 64 * 1024;
const MAX_OGG_PAGE_BYTES: usize = 255 * 255;

/// Complete owned Opus packet produced by the decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedOpusFrame {
    payload: Vec<u8>,
    duration: VoiceOpusFrameDuration,
}

impl EncodedOpusFrame {
    #[must_use]
    pub fn new(payload: Vec<u8>, duration: VoiceOpusFrameDuration) -> Self {
        Self { payload, duration }
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    #[must_use]
    pub const fn duration(&self) -> VoiceOpusFrameDuration {
        self.duration
    }

    /// Borrows this owned packet as Gloamwire's voice-send boundary.
    pub fn as_voice_frame(&self) -> VoiceResult<VoiceOpusFrame<'_>> {
        VoiceOpusFrame::new(&self.payload, self.duration)
    }
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("failed to start FFmpeg backend `{binary}`: {source}")]
    Spawn {
        binary: String,
        #[source]
        source: io::Error,
    },

    #[error("FFmpeg child did not expose its Opus output pipe")]
    MissingStdout,

    #[error("decoder I/O failed: {0}")]
    Io(#[from] io::Error),

    #[error("invalid FFmpeg Ogg/Opus stream: {0}")]
    InvalidOgg(String),

    #[error("FFmpeg exited unsuccessfully with status {status}")]
    ProcessFailed { status: ExitStatus },

    #[error("media server returned HTTP {0}")]
    MediaHttpStatus(u16),
}

/// Per-process FFmpeg controls applied when a decoder stream starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfmpegDecodeOptions {
    start_at: Duration,
    volume_percent: u8,
}

impl Default for FfmpegDecodeOptions {
    fn default() -> Self {
        Self {
            start_at: Duration::ZERO,
            volume_percent: 100,
        }
    }
}

impl FfmpegDecodeOptions {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn with_start_at(mut self, start_at: Duration) -> Self {
        self.start_at = start_at;
        self
    }

    /// Sets attenuation in the inclusive `0..=100` range.
    ///
    /// Values above 100 are clamped so this option never amplifies the source
    /// and therefore cannot introduce clipping through gain above unity.
    #[must_use]
    pub const fn with_volume_percent(mut self, volume_percent: u8) -> Self {
        self.volume_percent = if volume_percent > 100 {
            100
        } else {
            volume_percent
        };
        self
    }

    #[must_use]
    pub const fn start_at(self) -> Duration {
        self.start_at
    }

    #[must_use]
    pub const fn volume_percent(self) -> u8 {
        self.volume_percent
    }
}

/// FFmpeg-backed transcoder that normalizes arbitrary supported audio inputs to
/// Discord-compatible 48 kHz stereo Opus with fixed 20 ms packets.
#[derive(Debug, Clone)]
pub struct FfmpegOpusDecoder {
    binary: PathBuf,
    ipv4_only: bool,
}

impl Default for FfmpegOpusDecoder {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("ffmpeg"),
            ipv4_only: false,
        }
    }
}

impl FfmpegOpusDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_binary(mut self, binary: impl Into<PathBuf>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Restrict HTTP input connections to IPv4 without changing host networking.
    #[must_use]
    pub const fn with_ipv4_only(mut self, ipv4_only: bool) -> Self {
        self.ipv4_only = ipv4_only;
        self
    }

    /// Starts FFmpeg with default playback controls.
    pub fn open(&self, media: &PlayableMedia) -> Result<FfmpegOpusStream, DecodeError> {
        self.open_with_options(media, FfmpegDecodeOptions::default())
    }

    /// Starts FFmpeg and returns a stream of complete encoded Opus packets.
    ///
    /// The child is configured with `kill_on_drop`, so dropping the returned
    /// stream cancels the decoder instead of leaving a subprocess behind.
    pub fn open_with_options(
        &self,
        media: &PlayableMedia,
        options: FfmpegDecodeOptions,
    ) -> Result<FfmpegOpusStream, DecodeError> {
        let binary = self.binary.display().to_string();
        let mut command = Command::new(&self.binary);
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .arg("-nostdin")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error");

        // Reconnect transient HTTP failures, but never reconnect on a clean EOF.
        // The voice worker refreshes signed URLs and seeks on premature EOF.
        if media.url.starts_with("https://") || media.url.starts_with("http://") {
            if self.ipv4_only {
                command.arg("-local_addr").arg("0.0.0.0");
            }
            command
                .arg("-protocol_whitelist")
                .arg("http,https,tcp,tls,crypto,httpproxy")
                .arg("-rw_timeout")
                .arg("15000000")
                .arg("-reconnect")
                .arg("1")
                .arg("-reconnect_on_network_error")
                .arg("1")
                .arg("-reconnect_on_http_error")
                .arg("5xx")
                .arg("-reconnect_delay_max")
                .arg("5");
        }
        if let Some(headers) = render_http_headers(&media.http_headers) {
            command.arg("-headers").arg(headers);
        }
        if !options.start_at.is_zero() {
            command.arg("-ss").arg(format_seek_offset(options.start_at));
        }

        command
            .arg("-i")
            .arg(&media.url)
            .arg("-vn")
            .arg("-sn")
            .arg("-dn")
            .arg("-map")
            .arg("0:a:0")
            .arg("-ac")
            .arg("2")
            .arg("-ar")
            .arg("48000");

        if options.volume_percent < 100 {
            let ratio = f64::from(options.volume_percent) / 100.0;
            command.arg("-filter:a").arg(format!("volume={ratio:.2}"));
        }

        command
            .arg("-c:a")
            .arg("libopus")
            .arg("-application")
            .arg("audio")
            .arg("-frame_duration")
            .arg("20")
            .arg("-f")
            .arg("ogg")
            .arg("pipe:1");

        let mut child = command.spawn().map_err(|source| DecodeError::Spawn {
            binary: binary.clone(),
            source,
        })?;
        let stdout = child.stdout.take().ok_or(DecodeError::MissingStdout)?;
        let diagnostics = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(drain_decoder_stderr(stderr)));

        Ok(FfmpegOpusStream {
            child,
            packets: OggPacketReader::new(stdout),
            diagnostics,
            finished: false,
        })
    }
}

/// Live FFmpeg process and Ogg packet reader.
///
/// Reading is pull-based: at most the current Ogg page plus completed packets
/// from that page are buffered, so a slow Discord sender cannot create an
/// unbounded in-memory decode queue.
pub struct FfmpegOpusStream {
    child: Child,
    packets: OggPacketReader<ChildStdout>,
    diagnostics: Option<tokio::task::JoinHandle<Option<u16>>>,
    finished: bool,
}

impl std::fmt::Debug for FfmpegOpusStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FfmpegOpusStream")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl FfmpegOpusStream {
    /// Reads the next 20 ms encoded Opus packet.
    pub async fn next_frame(&mut self) -> Result<Option<EncodedOpusFrame>, DecodeError> {
        if self.finished {
            return Ok(None);
        }

        loop {
            let packet = match self.packets.next_packet().await {
                Ok(packet) => packet,
                Err(error) => {
                    if let Some(status) = self.access_failure().await {
                        return Err(DecodeError::MediaHttpStatus(status));
                    }
                    return Err(error);
                }
            };
            match packet {
                Some(packet) if packet.starts_with(b"OpusHead") => continue,
                Some(packet) if packet.starts_with(b"OpusTags") => continue,
                Some(packet) if packet.is_empty() => {
                    return Err(DecodeError::InvalidOgg(
                        "received an empty Opus packet".to_owned(),
                    ));
                }
                Some(packet) => {
                    return Ok(Some(EncodedOpusFrame::new(
                        packet,
                        VoiceOpusFrameDuration::TwentyMs,
                    )));
                }
                None => {
                    let status = self.child.wait().await?;
                    self.finished = true;
                    if status.success() {
                        return Ok(None);
                    }
                    if let Some(code) = self.access_failure().await {
                        return Err(DecodeError::MediaHttpStatus(code));
                    }
                    return Err(DecodeError::ProcessFailed { status });
                }
            }
        }
    }

    async fn access_failure(&mut self) -> Option<u16> {
        let task = self.diagnostics.as_mut()?;
        let result = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .ok()?;
        self.diagnostics = None;
        result.ok().flatten()
    }

    /// Explicitly stops FFmpeg. Dropping the stream provides the same
    /// cancellation guarantee through Tokio's `kill_on_drop` behavior.
    pub async fn shutdown(&mut self) -> Result<(), DecodeError> {
        if self.finished {
            return Ok(());
        }

        if self.child.try_wait()?.is_none() {
            self.child.kill().await?;
        }
        self.finished = true;
        Ok(())
    }
}

// Drain stderr concurrently so FFmpeg never blocks on a full diagnostic pipe.
// Only fixed categories are logged: FFmpeg can print signed media URLs.
async fn drain_decoder_stderr(mut stderr: impl AsyncRead + Unpin) -> Option<u16> {
    let mut buffer = [0_u8; 4096];
    let mut window = Vec::with_capacity(4352);
    let mut saw_output = false;
    let mut reported = false;
    let mut access_status = None;
    while let Ok(count) = stderr.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        saw_output = true;
        window.extend_from_slice(&buffer[..count]);
        let output = String::from_utf8_lossy(&window);
        access_status = access_status.or_else(|| decoder_access_status(&output));
        if !reported && let Some(reason) = decoder_stderr_reason(&output) {
            tracing::warn!(reason, "FFmpeg reported an error");
            reported = true;
        }
        if window.len() > 256 {
            window.drain(..window.len() - 256);
        }
    }
    if saw_output && !reported {
        tracing::warn!("FFmpeg reported an unclassified error (raw output suppressed)");
    }
    access_status
}

fn decoder_access_status(output: &str) -> Option<u16> {
    let output = output.to_ascii_lowercase();
    if output.contains("401 unauthorized") || output.contains("http error 401") {
        Some(401)
    } else if output.contains("403 forbidden") || output.contains("http error 403") {
        Some(403)
    } else if output.contains("429 too many requests") || output.contains("http error 429") {
        Some(429)
    } else {
        None
    }
}

fn decoder_stderr_reason(output: &str) -> Option<&'static str> {
    let output = output.to_ascii_lowercase();
    if output.contains("403 forbidden") || output.contains("http error 403") {
        Some("HTTP 403: media request refused")
    } else if output.contains("429") && output.contains("too many requests") {
        Some("HTTP 429: media request rate limited")
    } else if output.contains("404 not found") || output.contains("http error 404") {
        Some("HTTP 404: media unavailable")
    } else if output.contains("unknown encoder") || output.contains("encoder not found") {
        Some("audio encoder unavailable")
    } else if output.contains("protocol not found") || output.contains("not on whitelist") {
        Some("input protocol unavailable or disallowed")
    } else if output.contains("network is unreachable") || output.contains("connection refused") {
        Some("media network connection failed")
    } else if output.contains("timed out") || output.contains("timeout") {
        Some("media network or I/O timeout")
    } else if output.contains("invalid data found") {
        Some("invalid input media")
    } else if output.contains("matches no streams") {
        Some("input has no selected audio stream")
    } else if output.contains("certificate") || output.contains("tls handshake") {
        Some("TLS connection failed")
    } else {
        None
    }
}

fn format_seek_offset(offset: Duration) -> String {
    format!("{}.{:03}", offset.as_secs(), offset.subsec_millis())
}

fn render_http_headers(headers: &[(String, String)]) -> Option<String> {
    let rendered = headers
        .iter()
        .filter(|(name, value)| !contains_newline(name) && !contains_newline(value))
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    (!rendered.is_empty()).then_some(rendered)
}

fn contains_newline(value: &str) -> bool {
    value.contains('\r') || value.contains('\n')
}

struct OggPacketReader<R> {
    reader: R,
    pending: VecDeque<Vec<u8>>,
    partial: Vec<u8>,
    saw_eos: bool,
}

impl<R> OggPacketReader<R>
where
    R: AsyncRead + Unpin,
{
    fn new(reader: R) -> Self {
        Self {
            reader,
            pending: VecDeque::new(),
            partial: Vec::new(),
            saw_eos: false,
        }
    }

    async fn next_packet(&mut self) -> Result<Option<Vec<u8>>, DecodeError> {
        if let Some(packet) = self.pending.pop_front() {
            return Ok(Some(packet));
        }

        loop {
            if !self.read_page().await? {
                if !self.partial.is_empty() {
                    return Err(DecodeError::InvalidOgg(
                        "stream ended in the middle of an Ogg packet".to_owned(),
                    ));
                }
                if !self.saw_eos {
                    return Err(DecodeError::InvalidOgg(
                        "Ogg stream ended without an end-of-stream page".to_owned(),
                    ));
                }
                return Ok(None);
            }
            if let Some(packet) = self.pending.pop_front() {
                return Ok(Some(packet));
            }
        }
    }

    async fn read_page(&mut self) -> Result<bool, DecodeError> {
        let mut capture = [0_u8; 4];
        let first = self.reader.read(&mut capture[..1]).await?;
        if first == 0 {
            return Ok(false);
        }
        self.reader.read_exact(&mut capture[1..]).await?;
        if &capture != b"OggS" {
            return Err(DecodeError::InvalidOgg(
                "page did not start with the OggS capture pattern".to_owned(),
            ));
        }

        let mut fixed = [0_u8; 23];
        self.reader.read_exact(&mut fixed).await?;
        if fixed[0] != 0 {
            return Err(DecodeError::InvalidOgg(format!(
                "unsupported Ogg stream version {}",
                fixed[0]
            )));
        }

        let header_type = fixed[1];
        let segment_count = usize::from(fixed[22]);
        let mut lacing = vec![0_u8; segment_count];
        self.reader.read_exact(&mut lacing).await?;

        let payload_len = lacing
            .iter()
            .map(|length| usize::from(*length))
            .sum::<usize>();
        if payload_len > MAX_OGG_PAGE_BYTES {
            return Err(DecodeError::InvalidOgg(
                "Ogg page exceeded the protocol payload limit".to_owned(),
            ));
        }
        let mut payload = vec![0_u8; payload_len];
        self.reader.read_exact(&mut payload).await?;
        self.process_page(header_type, &lacing, &payload)?;
        self.saw_eos = header_type & 0x04 != 0;
        Ok(true)
    }

    fn process_page(
        &mut self,
        header_type: u8,
        lacing: &[u8],
        payload: &[u8],
    ) -> Result<(), DecodeError> {
        let continued = header_type & 0x01 != 0;
        if !continued && !self.partial.is_empty() {
            return Err(DecodeError::InvalidOgg(
                "packet continuation was lost between Ogg pages".to_owned(),
            ));
        }

        let mut discard_unknown_continuation = continued && self.partial.is_empty();
        let mut offset = 0_usize;
        for &length in lacing {
            let length = usize::from(length);
            let end = offset.checked_add(length).ok_or_else(|| {
                DecodeError::InvalidOgg("Ogg segment length overflowed".to_owned())
            })?;
            let segment = payload.get(offset..end).ok_or_else(|| {
                DecodeError::InvalidOgg("Ogg lacing exceeded page payload".to_owned())
            })?;
            offset = end;

            if discard_unknown_continuation {
                if length < 255 {
                    discard_unknown_continuation = false;
                }
                continue;
            }

            self.partial.extend_from_slice(segment);
            if self.partial.len() > MAX_OGG_PACKET_BYTES {
                return Err(DecodeError::InvalidOgg(format!(
                    "Ogg packet exceeded {MAX_OGG_PACKET_BYTES} bytes"
                )));
            }

            if length < 255 {
                self.pending.push_back(std::mem::take(&mut self.partial));
            }
        }

        if offset != payload.len() {
            return Err(DecodeError::InvalidOgg(
                "Ogg page contained unreferenced payload bytes".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, time::Duration};

    use gloamwire::voice::VoiceOpusFrameDuration;

    use super::{
        EncodedOpusFrame, FfmpegDecodeOptions, OggPacketReader, decoder_access_status, decoder_stderr_reason,
        format_seek_offset, render_http_headers,
    };

    #[test]
    fn decoder_diagnostics_do_not_expose_signed_urls() {
        assert_eq!(decoder_access_status("HTTP error 403 Forbidden"), Some(403));
        assert_eq!(decoder_access_status("invalid media"), None);
        assert_eq!(
            decoder_stderr_reason("https://example.com/?sig=secret: HTTP error 403 Forbidden"),
            Some("HTTP 403: media request refused")
        );
        assert_eq!(
            decoder_stderr_reason("Unknown encoder 'libopus'"),
            Some("audio encoder unavailable")
        );
        assert_eq!(decoder_stderr_reason("https://example.com/?sig=secret"), None);
    }

    #[tokio::test]
    async fn reads_multiple_packets_from_one_ogg_page() {
        let payload = [b"OpusHead".as_slice(), b"OpusTags".as_slice(), &[1, 2, 3]].concat();
        let bytes = ogg_page(0x06, &[8, 8, 3], &payload);
        let mut reader = OggPacketReader::new(Cursor::new(bytes));

        assert_eq!(
            reader.next_packet().await.expect("packet"),
            Some(b"OpusHead".to_vec())
        );
        assert_eq!(
            reader.next_packet().await.expect("packet"),
            Some(b"OpusTags".to_vec())
        );
        assert_eq!(
            reader.next_packet().await.expect("packet"),
            Some(vec![1, 2, 3])
        );
        assert_eq!(reader.next_packet().await.expect("eof"), None);
    }

    #[tokio::test]
    async fn reconstructs_packet_continued_across_pages() {
        let packet = (0..300)
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>();
        let mut bytes = ogg_page(0x02, &[255], &packet[..255]);
        bytes.extend(ogg_page(0x05, &[45], &packet[255..]));
        let mut reader = OggPacketReader::new(Cursor::new(bytes));

        assert_eq!(reader.next_packet().await.expect("packet"), Some(packet));
        assert_eq!(reader.next_packet().await.expect("eof"), None);
    }

    #[tokio::test]
    async fn rejects_truncated_ogg_stream_without_eos() {
        let bytes = ogg_page(0x02, &[3], &[1, 2, 3]);
        let mut reader = OggPacketReader::new(Cursor::new(bytes));
        assert_eq!(
            reader.next_packet().await.expect("packet"),
            Some(vec![1, 2, 3])
        );
        assert!(reader.next_packet().await.is_err());
    }

    #[test]
    fn frame_borrows_into_gloamwire_boundary() {
        let frame = EncodedOpusFrame::new(vec![1, 2, 3], VoiceOpusFrameDuration::TwentyMs);
        let voice = frame.as_voice_frame().expect("voice frame");
        assert_eq!(voice.payload(), &[1, 2, 3]);
        assert_eq!(voice.duration(), VoiceOpusFrameDuration::TwentyMs);
    }

    #[test]
    fn decoder_options_bound_volume_and_preserve_seek_offset() {
        let options = FfmpegDecodeOptions::new()
            .with_start_at(Duration::from_millis(90_123))
            .with_volume_percent(150);
        assert_eq!(options.start_at(), Duration::from_millis(90_123));
        assert_eq!(options.volume_percent(), 100);
        assert_eq!(format_seek_offset(options.start_at()), "90.123");
    }

    #[test]
    fn rejects_newlines_from_forwarded_http_headers() {
        let headers = vec![
            ("User-Agent".to_owned(), "Sonoryn".to_owned()),
            ("Unsafe".to_owned(), "value\r\nInjected: yes".to_owned()),
        ];
        assert_eq!(
            render_http_headers(&headers).as_deref(),
            Some("User-Agent: Sonoryn\r\n")
        );
    }

    fn ogg_page(header_type: u8, lacing: &[u8], payload: &[u8]) -> Vec<u8> {
        assert!(lacing.len() <= u8::MAX as usize);
        assert_eq!(
            lacing
                .iter()
                .map(|length| usize::from(*length))
                .sum::<usize>(),
            payload.len()
        );

        let mut header = [0_u8; 27];
        header[..4].copy_from_slice(b"OggS");
        header[4] = 0;
        header[5] = header_type;
        header[26] = lacing.len() as u8;

        let mut page = Vec::with_capacity(header.len() + lacing.len() + payload.len());
        page.extend_from_slice(&header);
        page.extend_from_slice(lacing);
        page.extend_from_slice(payload);
        page
    }
}
