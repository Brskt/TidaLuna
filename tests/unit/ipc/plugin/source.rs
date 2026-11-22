//! Tests for `src/ipc/plugin/source.rs`, attached to it by `#[path]`.

use super::{SourceFault, bump_capped, get_judged, serves_stored_code};
use crate::plugins::store::content_hash;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A source names a URL a plugin chose. On a sign-in host it is refused before any request, and a
/// redirect there is refused at the hop, on the source client with its `read_timeout` armed. Both
/// read as a refusal, not as a network failure.
#[tokio::test]
async fn a_source_on_or_redirected_to_a_sign_in_host_is_refused() {
    let client = &*crate::state::HTTP_CLIENT_SOURCE;
    let first = get_judged(client, "https://auth.tidal.com/p.json")
        .await
        .expect_err("a sign-in host is never fetched");
    assert!(
        matches!(SourceFault::of(&first), SourceFault::Refused),
        "{first:#}"
    );

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 1024];
        let _ = sock.read(&mut request).await;
        let _ = sock
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: https://login.tidal.com/p.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
    });
    let hop = get_judged(client, &format!("http://127.0.0.1:{port}/p.json"))
        .await
        .expect_err("the hop to a sign-in host is refused");
    assert!(
        matches!(SourceFault::of(&hop), SourceFault::Refused),
        "{hop:#}"
    );
}

/// A wrong yes leaves a plugin on old code without any error.
/// `zc/AUeTSimY` is Bun's `xxHash64` of `new`, little-endian, in base64 without padding, the
/// form esbuild writes.
#[test]
fn stored_code_is_reused_only_while_the_manifest_hash_names_it() {
    let (old, new) = ("export const x = 1;\n", "export const x = 2;\n");
    let named_new = r#"{"name":"p","hash":"zc/AUeTSimY"}"#;
    for (stored_code, fetched_manifest, reused, case) in [
        (new, named_new, true, "unchanged"),
        (old, named_new, false, "changed, or stored mid-publish"),
        (new, r#"{"name":"p"}"#, false, "no hash"),
        (new, r#"{"hash":7}"#, false, "not a string"),
    ] {
        assert_eq!(
            serves_stored_code(&content_hash(stored_code), fetched_manifest),
            reused,
            "{case}"
        );
    }
}

/// The plugin-driven byte cap holds exactly at the boundary and cannot be wrapped past:
/// `usize::MAX + 2` would read as 1, well under the cap, if the sum did not saturate.
#[test]
fn bump_capped_stops_at_the_cap_and_never_wraps() {
    for (running, add, max, expected) in [
        (0, 100, 1000, Some(100)),
        (100, 50, 1000, Some(150)),
        (60, 4, 64, Some(64)),
        (60, 5, 64, None),
        (usize::MAX, 2, 1024, None),
    ] {
        let outcome = bump_capped(running, add, max);
        match expected {
            Some(total) => assert_eq!(outcome.unwrap(), total, "{running} + {add} under {max}"),
            None => {
                let err = outcome.unwrap_err();
                assert!(
                    err.to_string().contains("cap"),
                    "{running} + {add} under {max}: got {err}"
                );
            }
        }
    }
}
