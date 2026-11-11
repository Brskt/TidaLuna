//! The helper the sidecars of `src/player/thread/output/` share, attached once to `output/mod.rs`
//! by `#[path]`.

pub(super) fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}
