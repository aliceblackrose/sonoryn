use std::{
    collections::VecDeque,
    io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
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
}

/// FFmpeg-backed transcoder that normalizes arbitrary supported audio inputs to
/// Discord-compatible 48 kHz stereo Opus with fixed 20 ms packets.
#[derive(Debug, Clone)]
pub struct FfmpegOpusDecoder {
    binary: PathBuf,
}

impl Default for FfmpegOpusDecoder {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("ffmpeg"),
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

    /// Starts FFmpeg and returns a stream of complete encoded Opus packets.
    ///
    /// The child is configured with `kill_on_drop`, so dropping the returned
    /// stream cancels the decoder instead of leaving a subprocess behind.
    pub fn open(&self, media: &PlayableMedia) -> Result<FfmpegOpusStream, DecodeError> {
        let binary = self.binary.display().to_string();
        let mut command = Command::new(&self.binary);
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .arg("-nostdin")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error");

        if let Some(headers) = render_http_headers(&media.http_headers) {
            command.arg("-headers").arg(headers);
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
            .arg("48000")
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

        Ok(FfmpegOpusStream {
            child,
            packets: OggPacketReader::new(stdout),
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
            match self.packets.next_packet().await? {
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
                    return Err(DecodeError::ProcessFailed { status });
                }
            }
        }
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

        let payload_len = lacing.iter().map(|length| usize::from(*length)).sum::<usize>();
        if payload_len > MAX_OGG_PAGE_BYTES {
            return Err(DecodeError::InvalidOgg(
                "Ogg page exceeded the protocol payload limit".to_owned(),
            ));
        }
        let mut payload = vec![0_u8; payload_len];
        self.reader.read_exact(&mut payload).await?;
        self.process_page(header_type, &lacing, &payload)?;
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
    use std::io::Cursor;

    use gloamwire::voice::VoiceOpusFrameDuration;

    use super::{EncodedOpusFrame, OggPacketReader, render_http_headers};

    #[tokio::test]
    async fn reads_multiple_packets_from_one_ogg_page() {
        let payload = [b"OpusHead".as_slice(), b"OpusTags".as_slice(), &[1, 2, 3]].concat();
        let bytes = ogg_page(0x02, &[8, 8, 3], &payload);
        let mut reader = OggPacketReader::new(Cursor::new(bytes));

        assert_eq!(reader.next_packet().await.expect("packet"), Some(b"OpusHead".to_vec()));
        assert_eq!(reader.next_packet().await.expect("packet"), Some(b"OpusTags".to_vec()));
        assert_eq!(reader.next_packet().await.expect("packet"), Some(vec![1, 2, 3]));
        assert_eq!(reader.next_packet().await.expect("eof"), None);
    }

    #[tokio::test]
    async fn reconstructs_packet_continued_across_pages() {
        let packet = (0..300).map(|value| (value % 251) as u8).collect::<Vec<_>>();
        let mut bytes = ogg_page(0x02, &[255], &packet[..255]);
        bytes.extend(ogg_page(0x01, &[45], &packet[255..]));
        let mut reader = OggPacketReader::new(Cursor::new(bytes));

        assert_eq!(reader.next_packet().await.expect("packet"), Some(packet));
        assert_eq!(reader.next_packet().await.expect("eof"), None);
    }

    #[test]
    fn frame_borrows_into_gloamwire_boundary() {
        let frame = EncodedOpusFrame::new(vec![1, 2, 3], VoiceOpusFrameDuration::TwentyMs);
        let voice = frame.as_voice_frame().expect("voice frame");
        assert_eq!(voice.payload(), &[1, 2, 3]);
        assert_eq!(voice.duration(), VoiceOpusFrameDuration::TwentyMs);
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
            lacing.iter().map(|length| usize::from(*length)).sum::<usize>(),
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
