//! Tests for `src/ui/token_filter/request.rs`, attached to it by `#[path]`.

use super::*;
use crate::platform::secure_store::ClientBinding;

fn generation(tag: &str) -> crate::platform::secure_store::TokenGeneration {
    crate::platform::secure_store::TokenGeneration {
        access_token: format!("real_at_{tag}"),
        refresh_token: format!("real_rt_{tag}"),
        opaque_at: format!("luna_at_{tag}"),
        opaque_rt: format!("luna_rt_{tag}"),
        version: 1,
        access_expires: u64::MAX,
        user_id: None,
        granted_scopes: vec![],
        client: ClientBinding::Proven("cid".into()),
    }
}

fn stored(
    current: crate::platform::secure_store::TokenGeneration,
    previous: Option<crate::platform::secure_store::TokenGeneration>,
    previous_valid_until: Option<u64>,
) -> crate::platform::secure_store::StoredTokenState {
    crate::platform::secure_store::StoredTokenState {
        current,
        previous,
        previous_valid_until,
    }
}

/// The client is the first `client_id`, the one the endpoint reads; empty, it names none, and a
/// later one never stands in for it.
#[test]
fn a_token_request_names_the_client_the_endpoint_reads() {
    for (body, expected) in [
        ("grant_type=refresh_token&client_id=a", Some("a")),
        ("grant_type=refresh_token&client_id=&client_id=b", None),
        ("grant_type=refresh_token&client_id=", None),
    ] {
        let request = GrantRequest::parse(body).unwrap();
        assert_eq!(request.client_id(), expected, "{body}");
    }
}

/// An opaque_rt the SDK held past our window must resolve to the current real refresh token,
/// never leave raw.
#[test]
fn an_opaque_refresh_resolves_to_its_generation_within_the_window_else_the_current_one() {
    let windowed = stored(generation("new"), Some(generation("old")), Some(100));
    let alone = stored(generation("new"), None, None);
    for (state, opaque, now, expected) in [
        (&windowed, "luna_rt_ancient", 50, "real_rt_new"),
        (&windowed, "luna_rt_old", 50, "real_rt_old"),
        (&windowed, "luna_rt_old", 200, "real_rt_new"),
        (&alone, "luna_rt_whatever", 0, "real_rt_new"),
    ] {
        assert_eq!(
            resolve_opaque_refresh(state, opaque, now).refresh_token,
            expected,
            "{opaque} at {now}"
        );
    }
}

fn generation_of(tag: &str, client_id: &str) -> crate::platform::secure_store::TokenGeneration {
    crate::platform::secure_store::TokenGeneration {
        client: ClientBinding::Proven(client_id.into()),
        ..generation(tag)
    }
}

fn unproven_generation_of(
    tag: &str,
    client_id: &str,
) -> crate::platform::secure_store::TokenGeneration {
    crate::platform::secure_store::TokenGeneration {
        client: ClientBinding::Unproven(client_id.into()),
        ..generation(tag)
    }
}

fn pairs(body: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn pair(key: &str, value: &str) -> (String, String) {
    (key.into(), value.into())
}

fn substituted(
    body: &str,
    state: &crate::platform::secure_store::StoredTokenState,
    now: u64,
) -> Option<RefreshRewrite> {
    GrantRequest::parse(body)
        .ok()?
        .substitute_refresh(Some(state), now)
}

/// A refresh leaves with the client of the generation its nonce names, whatever the page or a
/// plugin put in the body; `update_client` leaves as such a refresh (sent as itself, the SDK
/// retried it forever), and the scope stays behind.
#[test]
fn a_substituted_refresh_sends_the_client_its_token_was_minted_under() {
    let state = stored(
        generation_of("new", "store"),
        Some(generation_of("old", "old-client")),
        Some(100),
    );
    for (body, expected_body, expected_client, why) in [
        (
            "grant_type=refresh_token&refresh_token=luna_rt_new&client_id=page&scope=r+w",
            vec![
                pair("grant_type", "refresh_token"),
                pair("refresh_token", "real_rt_new"),
                pair("client_id", "store"),
            ],
            "store",
            "the page's client is not the token's, and its scope stays behind",
        ),
        (
            "client_id=page&grant_type=update_client&refresh_token=luna_rt_new&client_secret=s",
            vec![
                pair("client_id", "store"),
                pair("grant_type", "refresh_token"),
                pair("refresh_token", "real_rt_new"),
            ],
            "store",
            "the SDK's migration leaves as a refresh, without the page client's secret",
        ),
        (
            "grant_type=refresh_token&refresh_token=luna_rt_new&client_id=store&client_secret=s",
            vec![
                pair("grant_type", "refresh_token"),
                pair("refresh_token", "real_rt_new"),
                pair("client_id", "store"),
                pair("client_secret", "s"),
            ],
            "store",
            "a secret for the very client sent stays",
        ),
        (
            "grant_type=refresh_token&refresh_token=luna_rt_old&client_id=page",
            vec![
                pair("grant_type", "refresh_token"),
                pair("refresh_token", "real_rt_old"),
                pair("client_id", "old-client"),
            ],
            "old-client",
            "a nonce of the generation just replaced keeps that generation's client",
        ),
        (
            "grant_type=refresh_token&refresh_token=luna_rt_new",
            vec![
                pair("grant_type", "refresh_token"),
                pair("refresh_token", "real_rt_new"),
                pair("client_id", "store"),
            ],
            "store",
            "a body naming no client gets the token's",
        ),
    ] {
        let rewrite = substituted(body, &state, 50).expect(why);
        assert_eq!(pairs(&rewrite.body), expected_body, "{why}");
        assert_eq!(rewrite.client_id.as_deref(), Some(expected_client), "{why}");
        assert!(!rewrite.body.contains("luna_"), "{why}");
    }
}

/// A client TIDAL never accepted for the token is not pinned: the page's own goes and the server
/// judges it. `update_client`, the SDK's move to the page's client, still goes with the stored one.
#[test]
fn an_unproven_client_lets_the_page_choose_except_on_update_client() {
    let state = stored(unproven_generation_of("new", "store"), None, None);
    for (body, expected_client, why) in [
        (
            "grant_type=refresh_token&refresh_token=luna_rt_new&client_id=page",
            Some("page"),
            "a refresh sends the page's client",
        ),
        (
            "grant_type=update_client&refresh_token=luna_rt_new&client_id=page",
            Some("store"),
            "update_client sends the stored client",
        ),
        (
            "grant_type=refresh_token&refresh_token=luna_rt_new",
            None,
            "a refresh naming no client sends none",
        ),
    ] {
        let rewrite = substituted(body, &state, 0).expect(why);
        assert_eq!(rewrite.client_id.as_deref(), expected_client, "{why}");
        let sent = pairs(&rewrite.body)
            .into_iter()
            .find(|(k, _)| k == "client_id")
            .map(|(_, v)| v);
        assert_eq!(sent.as_deref(), expected_client, "{why}");
    }
}

/// Our callback, as the SDK sends it in a code exchange.
const REDIRECT: &str = "redirect_uri=tidal%3A%2F%2Flogin%2Fauth";

const NONCE: &str = "luna_0123456789abcdef0123456789abcdef";
const NONCE_2: &str = "luna_fedcba9876543210fedcba9876543210";

/// `TokenFate` without its payloads, comparable in a table.
#[derive(Debug, PartialEq, Eq)]
enum Fate {
    Substitute,
    Neutralise,
    Reissue,
    Withhold(TokenRefusal),
}

fn fate(decided: &TokenFate) -> Fate {
    match decided {
        TokenFate::Substitute(_) => Fate::Substitute,
        TokenFate::Neutralise(_) => Fate::Neutralise,
        TokenFate::Reissue { .. } => Fate::Reissue,
        TokenFate::Withhold(why) => Fate::Withhold(*why),
    }
}

/// Whatever a plugin puts in a token request, a nonce never leaves: only the first `refresh_token`
/// of a refresh may carry one, resolved, emptied or held back. A body without a nonce's shape is
/// reissued. A value that merely contains `luna_` is never refused for it.
#[test]
fn a_token_request_never_sends_a_nonce_whatever_the_body() {
    use Fate::{Neutralise as N, Reissue as R, Substitute as S, Withhold as W};
    use TokenRefusal::{NonceMisplaced, NotUtf8, UpdateClientUnresolved as Unresolved};
    let live = stored(generation_of("new", "store"), None, None);
    let mut app_level = generation_of("new", "store");
    app_level.refresh_token = String::new();
    let app_level = stored(app_level, None, None);

    let row = |s: &str| {
        s.replace("{N}", NONCE)
            .replace("{N2}", NONCE_2)
            .replace("{R}", REDIRECT)
            .into_bytes()
    };
    let misplaced = || [W(NonceMisplaced), W(NonceMisplaced), W(NonceMisplaced)];
    let reissued = || [R, R, R];
    let mut not_utf8 = b"grant_type=client_credentials&x=".to_vec();
    not_utf8.push(0xff);

    // Expected fate with no session, a session holding a refresh token, an app-level session.
    let table: Vec<(Vec<u8>, [Fate; 3], &str)> = vec![
        (
            row("client_id=c&grant_type=refresh_token&refresh_token={N}&scope=r"),
            [N, S, N],
            "the SDK's refresh",
        ),
        (
            row("client_id=c&grant_type=update_client&refresh_token={N}"),
            [W(Unresolved), S, W(Unresolved)],
            "the SDK's update_client",
        ),
        (
            row("grant_type=refresh_token&grant_type=authorization_code&refresh_token={N}"),
            [N, S, N],
            "the first grant_type is a refresh",
        ),
        (
            row("grant_type=authorization_code&grant_type=refresh_token&refresh_token={N}"),
            misplaced(),
            "the first grant_type is not a refresh",
        ),
        (
            row("grant_type=refresh_token&refresh_token=real&refresh_token={N}"),
            misplaced(),
            "a real token then a nonce",
        ),
        (
            row("grant_type=refresh_token&refresh_token={N}&refresh_token={N2}"),
            misplaced(),
            "a nonce then another",
        ),
        (
            row("grant_type=Refresh_Token&refresh_token={N}"),
            misplaced(),
            "the grant matches with its case",
        ),
        (
            row("grant_type=refresh_token&refresh_token="),
            reissued(),
            "an empty refresh token",
        ),
        (
            row("grant_type=refresh_token&refresh_token={N}&client_id={N2}"),
            misplaced(),
            "a nonce in client_id",
        ),
        (
            b"grant_type=client_credentials&client_secret=%6cuna_0123456789abcdef0123456789abcdef"
                .to_vec(),
            misplaced(),
            "a percent-encoded nonce",
        ),
        (
            row(r#"{"grant_type":"refresh_token","refresh_token":"{N}"}"#),
            misplaced(),
            "a JSON body",
        ),
        (
            row("grant_type=client_credentials&client_id=c&client_secret=s"),
            reissued(),
            "no nonce",
        ),
        (
            row("grant_type=authorization_code&code=luna_1&code_verifier=abc_luna_xyz&{R}"),
            reissued(),
            "a code and a verifier containing luna_",
        ),
        (
            not_utf8,
            [W(NotUtf8), W(NotUtf8), W(NotUtf8)],
            "a body that is not UTF-8",
        ),
    ];
    for (body, expected, why) in table {
        for (state, expected) in [None, Some(&live), Some(&app_level)]
            .into_iter()
            .zip(expected)
        {
            let decided = decide(&body, state, 0);
            assert_eq!(fate(&decided), expected, "{why}");
            match &decided {
                TokenFate::Substitute(rewrite) => {
                    assert!(!carries_nonce_shape(&rewrite.body), "{why}");
                    assert!(
                        pairs(&rewrite.body).contains(&pair("refresh_token", "real_rt_new")),
                        "{why}"
                    );
                }
                TokenFate::Neutralise(sent) => {
                    assert!(!carries_nonce_shape(sent), "{why}");
                    assert!(pairs(sent).contains(&pair("refresh_token", "")), "{why}");
                }
                TokenFate::Reissue { body: sent, .. } => {
                    assert!(!any_pair_carries_a_nonce(sent.as_bytes()), "{why}");
                }
                TokenFate::Withhold(_) => {}
            }
        }
    }
}

/// The endpoint reads the query before the body: a nonce there is held back, whatever the body.
/// A body CEF cannot hand over as bytes is held back too (what would leave is unread), as is no
/// body at all: no grant the app sends has none.
#[test]
fn a_nonce_in_the_query_and_an_unreadable_body_are_held_back() {
    let live = stored(generation_of("new", "store"), None, None);
    let refresh =
        TokenBody::Bytes(format!("grant_type=refresh_token&refresh_token={NONCE}").into_bytes());
    let query = format!("refresh_token={NONCE}");
    for (query, body, expected, why) in [
        (
            Some(query.as_str()),
            &refresh,
            Fate::Withhold(TokenRefusal::NonceMisplaced),
            "a nonce in the query and the body",
        ),
        (
            None,
            &TokenBody::Absent,
            Fate::Withhold(TokenRefusal::Unreadable),
            "no body at all",
        ),
        (
            None,
            &TokenBody::Unreadable,
            Fate::Withhold(TokenRefusal::Unreadable),
            "a blob, a stream or a file",
        ),
        (None, &refresh, Fate::Substitute, "the SDK's refresh"),
    ] {
        assert_eq!(
            fate(&fate_of(query, body, Some(&live), 0)),
            expected,
            "{why}"
        );
    }
}

/// A refresh Rust did not substitute carries no nonce and names no generation of ours: its answer
/// never commits, a real refresh token's included. The app-level grant may only produce an app
/// token, and only a code exchange is a sign-in.
#[test]
fn a_reissued_request_answers_only_as_its_grant_allows() {
    let code = format!("grant_type=authorization_code&code=x&{REDIRECT}");
    for (body, expected, why) in [
        (
            "grant_type=refresh_token&refresh_token=real",
            ExchangeOrigin::Inert,
            "a real refresh token",
        ),
        (
            "grant_type=update_client&refresh_token=real",
            ExchangeOrigin::Inert,
            "update_client",
        ),
        (
            "grant_type=client_credentials&client_id=c&client_secret=s",
            ExchangeOrigin::ClientCredentials,
            "client_credentials",
        ),
        (code.as_str(), ExchangeOrigin::Code, "a code exchange"),
    ] {
        match decide(body.as_bytes(), None, 0) {
            TokenFate::Reissue { origin, .. } => assert_eq!(origin, expected, "{why}"),
            _ => panic!("{why}: not reissued"),
        }
    }
}

/// What a grant may carry. A refresh and a code exchange lose every key outside their own, the
/// refresh its scope; the app-level grant is refused for anything but its three keys, each once;
/// a code exchange names our callback exactly once, as written; any other grant is refused.
#[test]
fn a_grant_leaves_with_its_own_keys_and_our_callback_only() {
    let sent = |body: &str| GrantRequest::parse(body).map(|request| pairs(&request.encode()));
    assert_eq!(
        sent("grant_type=refresh_token&refresh_token=r&client_id=c&scope=s&x=1&refresh_token=q"),
        Ok(vec![
            pair("grant_type", "refresh_token"),
            pair("refresh_token", "r"),
            pair("client_id", "c"),
        ])
    );
    assert_eq!(
        sent(&format!(
            "grant_type=authorization_code&code=c&code_verifier=v&client_unique_key=k&scope=s&x=1&{REDIRECT}"
        )),
        Ok(vec![
            pair("grant_type", "authorization_code"),
            pair("code", "c"),
            pair("code_verifier", "v"),
            pair("client_unique_key", "k"),
            pair("scope", "s"),
            pair("redirect_uri", "tidal://login/auth"),
        ])
    );
    for (body, why) in [
        (
            "grant_type=client_credentials&client_id=c&client_secret=s&x=1",
            "a fourth key",
        ),
        (
            "grant_type=client_credentials&client_id=c&client_secret=s&client_id=d",
            "a key twice",
        ),
        ("grant_type=client_credentials&client_id=c", "a key missing"),
    ] {
        assert_eq!(sent(body).err(), Some(TokenRefusal::ForeignKeys), "{why}");
    }
    for (uri, accepted) in [
        ("tidal%3A%2F%2Flogin%2Fauth", true),
        ("tidal%3A%2F%2Flogin%2Fauth%3Falbum%3D1", true),
        ("tidal%3A%2F%2FLOGIN%2Fauth", false),
        ("tidal%3A%2F%2Flogin%2Fauth%2F", false),
        ("tidal%3A%2F%2Flogin%2Fauth%23x", false),
        ("tidal%3A%2F%2Fu%40login%2Fauth", false),
        ("tidal%3A%2F%2Flogin%3A1%2Fauth", false),
        ("https%3A%2F%2Flogin%2Fauth", false),
        ("%20tidal%3A%2F%2Flogin%2Fauth", false),
    ] {
        let body = format!("grant_type=authorization_code&code=c&redirect_uri={uri}");
        assert_eq!(sent(&body).is_ok(), accepted, "{uri}");
    }
    assert_eq!(
        sent(&format!(
            "grant_type=authorization_code&code=c&{REDIRECT}&{REDIRECT}"
        ))
        .err(),
        Some(TokenRefusal::BadRedirect),
        "our callback twice"
    );
    assert_eq!(
        sent("grant_type=authorization_code&code=c").err(),
        Some(TokenRefusal::BadRedirect),
        "no callback"
    );
    assert_eq!(
        sent("grant_type=password&username=u").err(),
        Some(TokenRefusal::UnknownGrant)
    );
    assert_eq!(
        fate(&decide(
            b"grant_type=refresh_token&refresh_token=r\n",
            None,
            0
        )),
        Fate::Withhold(TokenRefusal::ControlByte),
        "a control byte"
    );
}

/// Only one element of bytes is read; anything else CEF holds is refused, never read in part.
#[test]
fn only_one_element_of_bytes_is_a_readable_body() {
    let bytes = Some(PostdataelementType::BYTES);
    for (excluded, kinds, expected) in [
        (false, vec![], BodyLayout::Absent),
        (false, vec![bytes], BodyLayout::OneBytes),
        (true, vec![bytes], BodyLayout::Unreadable),
        (true, vec![], BodyLayout::Unreadable),
        (
            false,
            vec![Some(PostdataelementType::FILE)],
            BodyLayout::Unreadable,
        ),
        (false, vec![bytes, bytes], BodyLayout::Unreadable),
        (false, vec![None], BodyLayout::Unreadable),
    ] {
        assert_eq!(
            body_layout(excluded, &kinds),
            expected,
            "{excluded} {kinds:?}"
        );
    }
}

/// A bearer nonce resolves to the session's access token, or the header goes: the placeholder
/// never leaves, nor does a nonce's shape under any other scheme.
#[test]
fn a_bearer_nonce_is_resolved_or_the_header_is_stripped() {
    let live = stored(generation("new"), None, None);
    let in_basic = format!("Basic {NONCE}");
    for (header, with_session, without, why) in [
        (
            "Bearer luna_at_new",
            BearerFate::Substitute("Bearer real_at_new".into()),
            BearerFate::Strip,
            "a bearer nonce",
        ),
        (
            "Bearer real_token",
            BearerFate::Leave,
            BearerFate::Leave,
            "a real token",
        ),
        (
            in_basic.as_str(),
            BearerFate::Strip,
            BearerFate::Strip,
            "a nonce under another scheme",
        ),
    ] {
        let session = HeldTokens {
            user: Some(&live),
            app: None,
            user_live: true,
        };
        let nothing = HeldTokens {
            user: None,
            app: None,
            user_live: false,
        };
        assert_eq!(
            BearerFate::decide(header, &session, 0),
            with_session,
            "{why}"
        );
        assert_eq!(BearerFate::decide(header, &nothing, 0), without, "{why}");
    }
}

/// Which token an access nonce stands for. A nonce one of them minted names it exactly; an
/// unknown one means the live user's current token, else the app token, else nothing, and the
/// header then goes rather than the placeholder.
#[test]
fn an_access_nonce_resolves_to_the_token_that_minted_it_else_by_who_is_live() {
    let user = stored(generation("new"), Some(generation("old")), Some(100));
    let app = AppToken {
        opaque_at: "luna_at_app".into(),
        access_token: "real_at_app".into(),
    };
    let target = |user_state, app_state, user_live, nonce: &str| {
        let held = HeldTokens {
            user: user_state,
            app: app_state,
            user_live,
        };
        match NonceTarget::of(&held, nonce, 50) {
            NonceTarget::User(generation) => generation.access_token.clone(),
            NonceTarget::Application(app) => app.access_token.clone(),
            NonceTarget::Unresolvable => "stripped".into(),
        }
    };
    for (user_state, app_state, user_live, nonce, expected, why) in [
        (
            Some(&user),
            Some(&app),
            true,
            "luna_at_new",
            "real_at_new",
            "the current generation's nonce",
        ),
        (
            Some(&user),
            Some(&app),
            true,
            "luna_at_old",
            "real_at_old",
            "the previous one, in its window",
        ),
        (
            Some(&user),
            Some(&app),
            true,
            "luna_at_app",
            "real_at_app",
            "the app token's nonce",
        ),
        (
            Some(&user),
            Some(&app),
            true,
            "luna_at_unknown",
            "real_at_new",
            "unknown, a live user",
        ),
        (
            Some(&user),
            Some(&app),
            false,
            "luna_at_new",
            "real_at_new",
            "a held session's own nonce",
        ),
        (
            Some(&user),
            Some(&app),
            false,
            "luna_at_unknown",
            "real_at_app",
            "unknown, the user held",
        ),
        (
            Some(&user),
            None,
            false,
            "luna_at_unknown",
            "stripped",
            "unknown, held, no app token",
        ),
    ] {
        assert_eq!(
            target(user_state, app_state, user_live, nonce),
            expected,
            "{why}"
        );
    }
}

/// Only TIDAL's own hydrate POST is the surface held back; nothing near it is.
#[test]
fn the_hydrate_surface_is_the_exact_post_and_nothing_near_it() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    for (method, url, expected) in [
        ("POST", "https://tidal.com/api/session/hydrate", true),
        ("POST", "https://tidal.com/api/session/hydrate?x=1", true),
        ("GET", "https://tidal.com/api/session/hydrate", false),
        ("POST", "http://tidal.com/api/session/hydrate", false),
        (
            "POST",
            "https://desktop.tidal.com/api/session/hydrate",
            false,
        ),
        ("POST", "https://tidal.com./api/session/hydrate", false),
        ("POST", "https://tidal.com/api/session/hydrate/", false),
    ] {
        assert_eq!(
            BodyTokenSurface::of(method, &u(url)).is_some(),
            expected,
            "{method} {url}"
        );
    }
}
