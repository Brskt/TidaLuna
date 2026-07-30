//! Tests for `src/ui/buffering_filter.rs`, attached to it by `#[path]`.

use super::*;

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn emits_across_small_output_chunks() {
    let input = b"HELLO BUFFERING WORLD".to_vec();
    let transform = |b: Vec<u8>| FilterOutcome::Emit(b);
    let mut state = FilterState::Accumulating(Vec::new());

    // Accumulate the body in two input chunks.
    let mut read = 0usize;
    let mut w = 0usize;
    let mut chunk_a = input[..8].to_vec();
    run_filter(
        &mut state,
        &transform,
        Some(&mut chunk_a),
        Some(&mut read),
        None,
        &mut w,
    );
    assert_eq!(read, 8);
    let mut chunk_b = input[8..].to_vec();
    run_filter(
        &mut state,
        &transform,
        Some(&mut chunk_b),
        Some(&mut read),
        None,
        &mut w,
    );
    assert_eq!(read, input.len() - 8);

    // EOF (data_in = None) then drain across 4-byte output buffers.
    let mut collected = Vec::new();
    for _ in 0..100 {
        let mut out = vec![0u8; 4];
        let mut written = 0usize;
        run_filter(
            &mut state,
            &transform,
            None,
            None,
            Some(&mut out),
            &mut written,
        );
        if written == 0 {
            break;
        }
        collected.extend_from_slice(&out[..written]);
    }
    assert_eq!(collected, input);
}

#[test]
fn drop_fails_closed_and_never_emits_input() {
    let secret = b"{\"access_token\":\"SUPER-SECRET-OAUTH\"}".to_vec();
    let transform = |_b: Vec<u8>| FilterOutcome::Drop;
    let mut state = FilterState::Accumulating(secret.clone());

    // First EOF call: must write nothing and leave the output untouched.
    let mut out = vec![0u8; 256];
    let mut w = 7usize; // non-zero up front to prove it gets reset
    run_filter(&mut state, &transform, None, None, Some(&mut out), &mut w);
    assert_eq!(w, 0, "fail-closed must write zero bytes");
    assert!(out.iter().all(|&b| b == 0), "no bytes may reach the output");
    assert!(
        !contains(&out, b"SUPER-SECRET-OAUTH"),
        "the plaintext token must never be emitted"
    );

    // Sticky: a subsequent call still writes nothing.
    let mut out2 = vec![0u8; 256];
    let mut w2 = 9usize;
    run_filter(&mut state, &transform, None, None, Some(&mut out2), &mut w2);
    assert_eq!(w2, 0);
    assert!(out2.iter().all(|&b| b == 0));
}
