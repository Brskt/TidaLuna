//! One H.264 + AAC-LC fMP4 append, decoded for the page's transcoding MediaSource: the page posts
//! each fragment, reads back raw I420 frames and PCM, and encodes them to VP9 and Opus.
//!
//! Any trusted frame may post here. Every length and count read from the body is bounded.

mod demux;

/// One request's body, init and preroll included: a 6 s segment at 1080p and 6 Mbps is 4.5 MB.
pub(crate) const MAX_FRAGMENT_BYTES: usize = 24 * 1024 * 1024;

/// Samples one append may declare, both tracks together: 6 s at 60 fps is 360 frames, and 48 kHz
/// AAC 282 more.
pub(crate) const MAX_FRAGMENT_SAMPLES: usize = 4_000;

/// One init, the request's or one inside an append: hls.js writes 1,204 bytes for audio and video,
/// plus parameter sets of 16 KiB at most. The crate allocates up to 21 bytes per byte of `moov`.
pub(crate) const MAX_INIT_BYTES: usize = 64 * 1024;

/// One `moof`: hls.js writes 100 bytes and 17 per sample, 68,100 at the sample ceiling. The crate
/// allocates up to 64 bytes per 8-byte child; 32 per sample leaves room for a second track's traf.
pub(crate) const MAX_MOOF_BYTES: usize = 4 * 1024 + 32 * MAX_FRAGMENT_SAMPLES;
