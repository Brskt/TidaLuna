//! Tests for `src/player/wasapi.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn push_reclaims_played_prefix() {
    let mut ctx = RenderContext::new();
    ctx.current_stream_id = Some(7);

    ctx.handle_push_pcm(7, vec![1u8; 1000]);
    assert_eq!(ctx.pcm_data.len(), 1000);

    // Simulate the render loop consuming 600 bytes.
    ctx.write_cursor = 600;
    ctx.handle_push_pcm(7, vec![2u8; 500]);
    // Played prefix reclaimed: only the 400 unplayed + 500 new bytes remain.
    assert_eq!(ctx.write_cursor, 0);
    assert_eq!(ctx.pcm_data.len(), 900);
    assert_eq!(ctx.pcm_data[0], 1);
    assert_eq!(ctx.pcm_data[400], 2);

    // A stale stream's push is dropped.
    ctx.handle_push_pcm(8, vec![3u8; 100]);
    assert_eq!(ctx.pcm_data.len(), 900);
}

#[test]
fn reset_for_seek_credits_discarded_audio_as_consumed() {
    let mut ctx = RenderContext::new();
    ctx.current_stream_id = Some(7);
    ctx.pcm_sample_rate = 44100;

    ctx.handle_push_pcm(7, vec![1u8; 1000]);
    ctx.write_cursor = 600;
    ctx.handle_reset_for_seek(7, 0.0);

    // The 400 unplayed bytes were discarded: credited to the throttle.
    assert_eq!(ctx.consumed.load(Relaxed), 400);
    assert_eq!(ctx.pcm_data.len(), 0);
    assert_eq!(ctx.write_cursor, 0);
}
