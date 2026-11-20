use std::sync::atomic::{AtomicBool, Ordering};

use crate::video::deadline::Deadline;
use crate::video::fragment::{FragmentDecode, demux, parse_body};

use super::{Answer, Slot};

/// Magic naming the fragment wire format: a page handed a foreign body says so instead of encoding
/// noise.
const MAGIC: &[u8; 4] = b"LVF1";

/// The `fragment/` body: the magic, then one record per AAC frame and per H.264 frame, decoded as
/// `read` asks. No count up front: it is only known once the decoder has drained.
pub(super) struct FragmentBody {
    decode: FragmentDecode,
    buf: Vec<u8>,
    offset: usize,
    records: usize,
    done: bool,
    _slot: Slot,
}

impl FragmentBody {
    pub(super) fn new(decode: FragmentDecode, slot: Slot) -> Self {
        Self {
            decode,
            buf: MAGIC.to_vec(),
            offset: 0,
            records: 0,
            done: false,
            _slot: slot,
        }
    }

    /// Copy the next bytes into `out`, decoding a record when the one in hand is spent. Zero at
    /// the end.
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
            self.offset = 0;
            if self.decode.next_record(&mut self.buf)? {
                self.records += 1;
            } else {
                self.done = true;
                crate::vprintln!(
                    "[VIDEO]   Video fragment decoded ({} records)",
                    self.records
                );
            }
        }
    }
}

/// Why `fragment/` could not open, by stage.
enum FragmentError {
    Input(anyhow::Error),
    Codec(anyhow::Error),
    Panicked(tokio::task::JoinError),
}

/// 422 names what the page sent; 503 waits for the codec; 500 is a fault of ours.
fn fragment_refusal(e: FragmentError) -> Answer {
    match e {
        FragmentError::Input(e) => {
            crate::verr!("[VIDEO]   Refused a video fragment: {e:#}");
            Answer::refusal(422, format!("{e:#}"))
        }
        FragmentError::Codec(e) => {
            // The cause stays in the log: a loader message names the codec's path on disk.
            crate::verr!("[VIDEO]   Video fragment decoder failed: {e:#}");
            // The supervisor proved the codec loads and starts: one that no longer does changed since.
            crate::video::codec::recheck();
            Answer::refusal(503, "the video codec could not be loaded")
        }
        FragmentError::Panicked(e) => {
            crate::verr!("[VIDEO]   The video fragment task failed: {e}");
            Answer::refusal(500, "the video fragment decode failed inside the app")
        }
    }
}

/// `fragment/`: the codec, the demux and the decoders, before the headers go out.
pub(super) async fn open_fragment(
    body: Vec<u8>,
    cancelled: &AtomicBool,
    deadline: Deadline,
) -> Result<FragmentDecode, Answer> {
    let path = crate::video::codec::decoder().await.map_err(|why| {
        // A clip asking is demand: a codec whose download failed is fetched again.
        crate::video::codec::note_demand();
        crate::vprintln!("[VIDEO]   Video fragment without a codec: {}", why.phrase());
        Answer::refusal(503, why.phrase())
    })?;
    if cancelled.load(Ordering::Relaxed) {
        // nginx's "client closed request".
        return Err(Answer::refusal(499, "the request was abandoned"));
    }
    tokio::task::spawn_blocking(move || {
        let request = parse_body(&body).map_err(FragmentError::Input)?;
        let fragment = demux(&request).map_err(FragmentError::Input)?;
        let api = crate::video::codec::load_api(&path).map_err(FragmentError::Codec)?;
        FragmentDecode::open(fragment, api, deadline).map_err(FragmentError::Codec)
    })
    .await
    .unwrap_or_else(|e| Err(FragmentError::Panicked(e)))
    .map_err(fragment_refusal)
}
