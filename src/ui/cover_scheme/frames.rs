use std::time::Instant;

use crate::video::codec::FailureClass;
use crate::video::cover::{CoverFrame, CoverKey, DecodeStream};

use super::{Answer, CoverShared, GRANTS, Grant, Slot};

/// Magic naming the frames wire format: a page handed a foreign body says so instead of encoding
/// noise.
const MAGIC: &[u8; 4] = b"LVC2";

/// Bytes before the first frame: the magic, then width, height and frame count as u32.
const HEADER_LEN: usize = 16;

/// Bytes of the record in front of each frame's planes: pts (i64), three strides and three plane
/// lengths (u32).
const RECORD_LEN: usize = 32;

/// The `frames/` body: the header, then each frame's record and planes, decoded as `read` asks.
pub(super) struct FrameBody {
    stream: DecodeStream,
    key: CoverKey,
    buf: Vec<u8>,
    offset: usize,
    done: bool,
    _slot: Slot,
}

impl FrameBody {
    pub(super) fn new(stream: DecodeStream, key: CoverKey, slot: Slot) -> Self {
        let (width, height) = stream.size();
        let mut buf = Vec::with_capacity(HEADER_LEN);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(width as u32).to_le_bytes());
        buf.extend_from_slice(&(height as u32).to_le_bytes());
        buf.extend_from_slice(&(stream.frame_count() as u32).to_le_bytes());
        Self {
            stream,
            key,
            buf,
            offset: 0,
            done: false,
            _slot: slot,
        }
    }

    /// Copy the next bytes into `out`, decoding a frame when the one in hand is spent. Zero at
    /// the end, when the pass's grant is opened.
    pub(super) fn fill(&mut self, out: &mut [u8]) -> anyhow::Result<usize> {
        loop {
            let left = &self.buf[self.offset..];
            if !left.is_empty() {
                let n = left.len().min(out.len());
                out[..n].copy_from_slice(&left[..n]);
                self.offset += n;
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            match self.stream.next_frame()? {
                Some(frame) => {
                    encode_frame(&frame, &mut self.buf);
                    self.offset = 0;
                }
                None => {
                    self.done = true;
                    let (width, height) = self.stream.size();
                    GRANTS.lock().unwrap_or_else(|e| e.into_inner()).insert(
                        self.key.clone(),
                        Grant {
                            width,
                            height,
                            frames: self.stream.frame_count(),
                            span_micros: self.stream.span_micros(),
                            at: Instant::now(),
                        },
                    );
                    crate::vprintln!(
                        "[VIDEO]   Cover frames sent ({} at {width}x{height})",
                        self.stream.frame_count()
                    );
                }
            }
        }
    }
}

/// One frame's record and planes, over `buf`'s previous contents.
fn encode_frame(frame: &CoverFrame, buf: &mut Vec<u8>) {
    buf.clear();
    buf.reserve(RECORD_LEN + frame.y.len() + frame.u.len() + frame.v.len());
    buf.extend_from_slice(&frame.pts_micros.to_le_bytes());
    for value in [
        frame.y_stride,
        frame.u_stride,
        frame.v_stride,
        frame.y.len(),
        frame.u.len(),
        frame.v.len(),
    ] {
        buf.extend_from_slice(&(value as u32).to_le_bytes());
    }
    buf.extend_from_slice(&frame.y);
    buf.extend_from_slice(&frame.u);
    buf.extend_from_slice(&frame.v);
}

/// Why `frames/` could not open, by stage.
enum FramesError {
    Download(crate::video::cover::CoverFetchError),
    Load(anyhow::Error),
    Open(crate::video::cover::OpenError),
    Panicked(tokio::task::JoinError),
}

impl FramesError {
    fn class(&self) -> FailureClass {
        match self {
            Self::Download(e) => e.class(),
            Self::Load(_) => FailureClass::Unloadable,
            Self::Open(e) => e.class(),
            Self::Panicked(_) => FailureClass::Crashed,
        }
    }
}

impl std::fmt::Display for FramesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Download(e) => write!(f, "downloading the cover failed: {e}"),
            Self::Load(e) => write!(f, "loading the codec failed: {e:#}"),
            Self::Open(e) => write!(f, "decoding the cover failed: {e}"),
            Self::Panicked(e) => write!(f, "the cover decode task failed: {e}"),
        }
    }
}

/// The status tells the page what to do: 502 asks again soon (the network), 503 waits for the
/// codec, 422 and 500 spend an attempt (the cover, or a fault of ours).
fn frames_refusal(class: FailureClass) -> (u16, &'static str) {
    match class {
        FailureClass::Network => (502, "the cover could not be fetched right now"),
        FailureClass::Refused => (422, "the artwork host does not serve this cover"),
        FailureClass::Corrupt => (422, "the cover cannot be transcoded"),
        FailureClass::Unloadable => (503, "the video codec could not be loaded"),
        FailureClass::Disk | FailureClass::Crashed => {
            (500, "the cover transcode failed inside the app")
        }
    }
}

/// `frames/`: the decoder, the file, and the first frame, before the headers go out.
pub(super) async fn open_frames(
    shared: &CoverShared,
    key: &CoverKey,
) -> Result<DecodeStream, Answer> {
    let path = crate::video::codec::decoder().await.map_err(|why| {
        crate::vprintln!("[VIDEO]   Cover without a codec: {}", why.phrase());
        Answer::refusal(503, why.phrase())
    })?;
    let refusal = |e: FramesError| {
        // The cause stays in the log: a loader message names the codec's path on disk.
        crate::verr!("[VIDEO]   Cover frames failed: {e}");
        let (status, reason) = frames_refusal(e.class());
        Answer::refusal(status, reason)
    };
    let file = crate::video::cover::fetch_cover(key.url(), &shared.cancelled)
        .await
        .map_err(|e| refusal(FramesError::Download(e)))?
        // nginx's "client closed request".
        .ok_or_else(|| Answer::refusal(499, "the request was abandoned"))?;
    let opened = tokio::task::spawn_blocking(move || {
        let api = crate::video::codec::load_api(&path).map_err(FramesError::Load)?;
        DecodeStream::open(&file, api).map_err(FramesError::Open)
    })
    .await
    .unwrap_or_else(|e| Err(FramesError::Panicked(e)));
    opened.map_err(|e| {
        // The supervisor proved the codec loads and starts: one that no longer does changed since.
        if e.class() == FailureClass::Unloadable {
            crate::video::codec::recheck();
        }
        refusal(e)
    })
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/cover_scheme/frames.rs"]
mod tests;
