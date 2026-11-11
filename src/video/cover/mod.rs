//! Decoding an animated album cover: a plain MP4 on `resources.tidal.com`, H.264, no audio.
//!
//! Distinct from the clip path in every way that matters: no manifest, no signed URL, no
//! seek, and the file is small enough to hold whole in memory. The decoded frames are never held
//! whole: [`DecodeStream`] hands them out one at a time to the page, which transcodes them once.

mod decode;
mod fetch;

pub(crate) use decode::{CoverFrame, DecodeStream, OpenError};
pub(crate) use fetch::{CoverFetchError, CoverKey, fetch_cover};
