//! Tests for `src/ui/proxy_auth_dialog.rs`, attached to it by `#[path]`.

use super::*;

/// The prompt a page script could raise: a server's 401 never opens a window, nor does a proxy
/// challenge on a request that does not go through a tunnel.
#[test]
fn only_a_tunnelled_proxy_challenge_asks_the_user() {
    let cases = [
        (
            false,
            "https://auth.example.test/v1/token",
            AuthPolicy::Refuse,
        ),
        (true, "http://plugin.example.test/", AuthPolicy::Refuse),
        (true, "not a url", AuthPolicy::Refuse),
        (
            true,
            "https://api.example.test/v1/tracks",
            AuthPolicy::AskUser,
        ),
        (true, "wss://ws.example.test/socket", AuthPolicy::AskUser),
    ];
    for (is_proxy, origin, expected) in cases {
        assert_eq!(
            AuthPolicy::for_challenge(is_proxy, origin),
            expected,
            "{is_proxy} {origin}"
        );
    }
}

fn key(host: &str) -> ChallengeKey {
    ChallengeKey {
        host: host.to_string(),
        port: 3128,
        realm: "corp".to_string(),
        scheme: "basic".to_string(),
    }
}

/// Requests that hit the same proxy share one window and its answer; once settled, the next
/// challenge opens a new one.
#[test]
fn challenges_of_one_proxy_share_a_window_until_it_is_settled() {
    let pending = Pending::new();
    assert_eq!(pending.join(key("proxy.example.test"), 1), Joined::First);
    assert_eq!(pending.join(key("proxy.example.test"), 2), Joined::Waiting);
    assert_eq!(pending.join(key("other.example.test"), 3), Joined::First);
    assert_eq!(pending.settle(&key("proxy.example.test")), vec![1, 2]);
    assert_eq!(pending.join(key("proxy.example.test"), 4), Joined::First);
}
