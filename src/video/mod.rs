//! Video decoding. The decoder itself is not compiled into this binary: `codec` downloads
//! Cisco's build at run time.

pub(crate) mod codec;
pub(crate) mod cover;
pub(crate) mod cover_cache;
pub(crate) mod deadline;
pub(crate) mod fragment;
pub(crate) mod webm;
