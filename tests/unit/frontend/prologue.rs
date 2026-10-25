//! Tests for `src/frontend/prologue.rs`, attached by `#[path]`.

use super::{InitGlobals, init_globals_js};

/// U+2028, built from its code point: written literally, a tool or editor may turn the escape into
/// the raw character this test exists to keep out.
fn line_separator() -> char {
    char::from_u32(0x2028).expect("valid code point")
}

fn globals(user_agent: Option<String>) -> InitGlobals {
    InitGlobals {
        platform: "linux",
        close_to_tray: true,
        auto_check: false,
        update_channel: "stable",
        receiver_always_on: true,
        volume_sync: false,
        asio: true,
        console: false,
        crossfade_enabled: true,
        crossfade_secs: 6,
        perf: false,
        window_maximized: true,
        credentials: serde_json::json!({ "codeVerifier": "v" }),
        rewrite_epoch: 0x0123_4567_89ab_cdef,
        user_agent,
    }
}

#[test]
fn a_hostile_user_agent_stays_one_string() {
    let ua = format!("a\"b\\c</script>{}", line_separator());
    let js = init_globals_js(&globals(Some(ua)));
    let escaped = concat!("\"a\\\"b\\\\c</script>", "\\", "u2028\"");
    assert!(js.contains(escaped), "{js}");
    assert!(
        !js.contains(line_separator()),
        "a raw line separator ends a JS line"
    );
}
