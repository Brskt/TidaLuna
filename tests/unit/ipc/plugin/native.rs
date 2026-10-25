//! Tests for `src/ipc/plugin/native.rs`, attached to it by `#[path]`.

use super::*;

fn owner(raw: &str) -> PluginName {
    PluginName::parse(raw).unwrap()
}

/// The module name is caller-chosen: without a bound one plugin grows this ledger AND the Bun
/// child's module table without limit. Refused rather than evicted: evicting the token would leave
/// the module loaded in Bun; the child leaks either way. The bound belongs before Bun is asked
/// to load anything.
#[test]
fn a_plugin_cannot_hold_unlimited_modules() {
    let held: Vec<String> = (0..MAX_MODULES_PER_PLUGIN)
        .map(|n| format!("Foo/{n}.native.ts"))
        .collect();
    let names: Vec<&str> = held.iter().map(String::as_str).collect();

    assert!(!admits_new_module(names.iter().copied(), &owner("Foo")));
    assert!(
        admits_new_module(names.iter().copied(), &owner("Bar")),
        "one plugin's modules must not exhaust another plugin's budget"
    );

    let under = &names[..MAX_MODULES_PER_PLUGIN - 1];
    assert!(admits_new_module(under.iter().copied(), &owner("Foo")));
}

// `NATIVE_MODULES` is a process-global and libtest runs these as threads of one process; every
// test owns its module names: issuing supersedes the previous entry for a name, which is right in
// production and evicts a neighbour's token here.

/// The channel must not be derivable from the module name: it used to be `__LunaNative.{name}`;
/// any plugin could type another's name to call its native exports.
#[test]
fn the_channel_token_is_not_the_module_name() {
    let token =
        issue_native_channel("TokenShape/secret.native.ts", "hash").expect("entropy available");
    assert!(!token.contains("TokenShape"));
    assert!(!token.contains("secret"));
    assert_eq!(
        module_for_native_channel(&token).as_deref(),
        Some("TokenShape/secret.native.ts")
    );
}

#[test]
fn a_module_name_is_not_a_channel() {
    issue_native_channel("NameGuess/secret.native.ts", "hash").expect("entropy available");
    assert!(
        module_for_native_channel("NameGuess/secret.native.ts").is_none(),
        "guessing the name must not reach the module"
    );
}

/// `DiscordRPC` and `DiscordRPC+` are two published plugins, and so are `@s/p` and `@s/pq`: a name
/// that starts another one owns none of its modules, bare entry included.
#[test]
fn a_plugin_owns_its_own_modules_only() {
    let cases = [
        ("Foo/x.native.ts", "Foo", true),
        ("Foo", "Foo", true),
        ("Foobar/x.native.ts", "Foo", false),
        ("Other/x.native.ts", "Foo", false),
        ("DiscordRPC+", "DiscordRPC", false),
        ("DiscordRPC+/x.native.ts", "DiscordRPC", false),
        ("@s/pq/x.native.ts", "@s/p", false),
        ("@s/p/x.native.ts", "@s/p", true),
    ];
    for (module, name, owned) in cases {
        assert_eq!(
            module_belongs_to(module, &owner(name)),
            owned,
            "{module} by {name}"
        );
    }
}

/// Uninstall clears pending trust by the key's module, which the key must give back whole, a
/// dependency with a `/` of its own included.
#[test]
fn a_pending_trust_key_names_its_module() {
    assert_eq!(
        pending_trust_module("Foo/x.native.ts::fs"),
        "Foo/x.native.ts"
    );
    assert_eq!(pending_trust_module("Foo::dns/promises"), "Foo");
}

/// Uninstall withdraws the plugin's persisted trust. A leftover token must stop reaching exports
/// whose grants were just revoked.
#[test]
fn uninstalling_clears_every_token_the_plugin_held() {
    let a = issue_native_channel("Gone/one.native.ts", "hash").expect("entropy available");
    let b = issue_native_channel("Gone/two.native.ts", "hash").expect("entropy available");

    clear_native_channels(&CleanupScope::Plugin(owner("Gone")));

    assert!(module_for_native_channel(&a).is_none());
    assert!(module_for_native_channel(&b).is_none());
}

/// Different code is a real supersession: trust is keyed by code hash, and Bun's one slot now holds
/// the new code.
#[test]
fn re_registering_different_code_revokes_the_previous_token() {
    let old = issue_native_channel("Reload/x.native.ts", "hash-old").expect("entropy available");
    let new = issue_native_channel("Reload/x.native.ts", "hash-new").expect("entropy available");
    assert!(module_for_native_channel(&old).is_none());
    assert_eq!(
        module_for_native_channel(&new).as_deref(),
        Some("Reload/x.native.ts")
    );
}

/// Minting a fresh token per call evicted the one already handed to a concurrent caller, leaving it
/// holding a live success whose channel answered 403.
#[test]
fn the_same_registration_hands_back_the_same_token() {
    let first = issue_native_channel("Twice/x.native.ts", "hash-same").expect("entropy available");
    let second = issue_native_channel("Twice/x.native.ts", "hash-same").expect("entropy available");
    assert_eq!(first, second);
    assert_eq!(
        module_for_native_channel(&first).as_deref(),
        Some("Twice/x.native.ts"),
        "the token handed to the first caller must stay live"
    );
}

/// A plugin at its bound re-registering a module it already holds takes no new slot: refusing it
/// would answer a reload with a 403, while a name it does not hold is still refused.
#[test]
fn a_plugin_at_its_bound_can_reserve_a_module_it_already_holds() {
    let names: Vec<String> = (0..MAX_MODULES_PER_PLUGIN)
        .map(|n| format!("ReserveAtBound/{n}.native.ts"))
        .collect();
    let at_bound = owner("ReserveAtBound");
    let held: Vec<_> = names
        .iter()
        .map(|name| PendingRegistration::reserve(name, &at_bound).expect("under the bound"))
        .collect();

    assert!(
        PendingRegistration::reserve("ReserveAtBound/extra.native.ts", &at_bound).is_none(),
        "a ninth distinct module is refused"
    );
    let again = PendingRegistration::reserve(&names[0], &at_bound);
    assert!(again.is_some(), "a module already held needs no new slot");

    drop(again);
    drop(held);
}

fn entry_with(in_flight: usize, with_channel: bool) -> ModuleEntry {
    let mut entry = ModuleEntry::fresh();
    entry.in_flight = in_flight;
    if with_channel {
        entry.channel = Some(Channel {
            token: "t".to_string(),
            code_hash: "h".to_string(),
        });
    }
    entry
}

/// The unload goes to every module the scope names, the ones still being acquired as well as the
/// settled ones, and to no module of another plugin or of a plugin whose name merely starts the same.
#[test]
fn a_plugin_scope_names_every_module_it_holds_and_only_those() {
    let mut modules = HashMap::from([
        ("Gone/one.native.ts".to_string(), entry_with(0, true)),
        ("Gone".to_string(), entry_with(1, false)),
        ("Goner/x.native.ts".to_string(), entry_with(0, true)),
        ("Stays/x.native.ts".to_string(), entry_with(0, true)),
    ]);
    let mut gone = remove_scope(&mut modules, &CleanupScope::Plugin(owner("Gone")));
    gone.sort();
    assert_eq!(gone, ["Gone", "Gone/one.native.ts"]);
    let mut kept: Vec<_> = modules.keys().cloned().collect();
    kept.sort();
    assert_eq!(kept, ["Goner/x.native.ts", "Stays/x.native.ts"]);
}

#[test]
fn the_all_scope_names_every_module() {
    let mut modules = HashMap::from([
        ("A/x.native.ts".to_string(), entry_with(0, true)),
        ("B/x.native.ts".to_string(), entry_with(2, false)),
    ]);
    let mut gone = remove_scope(&mut modules, &CleanupScope::All);
    gone.sort();
    assert_eq!(gone, ["A/x.native.ts", "B/x.native.ts"]);
    assert!(modules.is_empty());
}

/// An uninstall unloads each module it clears, through the body `clear_native_channels` runs.
#[test]
fn uninstalling_unloads_each_module_it_cleared() {
    let _a = issue_native_channel("Unload/one.native.ts", "hash").expect("entropy available");
    let reservation =
        PendingRegistration::reserve("Unload/two.native.ts", &owner("Unload")).expect("admitted");

    let mut unloaded = Vec::new();
    clear_native_channels_with(&CleanupScope::Plugin(owner("Unload")), |module| {
        unloaded.push(module.to_string())
    });
    unloaded.sort();
    assert_eq!(unloaded, ["Unload/one.native.ts", "Unload/two.native.ts"]);
    drop(reservation);
}

/// The trust dialog can sit open past an uninstall; its retry would then write a register line for
/// a plugin that is gone. Refused here, and nothing is sent.
#[test]
fn a_register_admitted_before_an_uninstall_is_not_sent_after_it() {
    let reservation =
        PendingRegistration::reserve("LateRegister/x.native.ts", &owner("LateRegister"))
            .expect("admitted");
    assert_eq!(reservation.send_if_current(|| Ok(1)), Ok(1));

    clear_native_channels_with(&CleanupScope::Plugin(owner("LateRegister")), |_| {});

    let mut sent = false;
    let refused = reservation.send_if_current(|| {
        sent = true;
        Ok(())
    });
    assert!(refused.is_err());
    assert!(!sent, "no line reaches the child for a cleared entry");
}

/// A reinstall makes a new entry under the old name. An attempt admitted for the old one must
/// neither send under the new one nor retire its slot when it finally drops.
#[test]
fn a_reinstall_is_not_taken_for_the_attempt_that_preceded_it() {
    let name = "Reinstall/x.native.ts";
    let stale = PendingRegistration::reserve(name, &owner("Reinstall")).expect("admitted");
    clear_native_channels_with(&CleanupScope::Plugin(owner("Reinstall")), |_| {});
    let fresh = PendingRegistration::reserve(name, &owner("Reinstall")).expect("admitted");

    assert!(stale.send_if_current(|| Ok(())).is_err());
    assert!(fresh.send_if_current(|| Ok(())).is_ok());

    drop(stale);
    let held = NATIVE_MODULES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .map(|entry| entry.in_flight);
    assert_eq!(
        held,
        Some(1),
        "the stale attempt left the new entry's count alone"
    );
    drop(fresh);
}

/// Bun's scan runs where plugins run, and its list is a plugin's to shape: only the rows of the
/// closed list reach a dialog, a prompt key or the trust table.
#[test]
fn only_a_known_row_is_ever_asked() {
    for (dependency, row) in [
        ("fs", Some("fs")),
        ("fs/promises", Some("fs")),
        ("https", Some("net")),
        ("dns/promises", Some("net")),
        ("worker_threads", Some("worker_threads")),
        ("child_process", None),
        ("fs::P/x.native.ts", None),
        ("<b>fs</b>", None),
    ] {
        assert_eq!(trust_row(dependency), row, "{dependency}");
    }
}

/// The trust dialog can sit open past a reinstall: its answer, written after the reset, kept a
/// decision for identical bytes. The reset shuts the gate that write asks on the actor.
#[tokio::test]
async fn a_grant_read_before_a_reinstall_is_not_written_after_it() {
    let mut entered = PENDING_TRUST.enter("LateGrant/x.native.ts", "fs", 1);
    entered.answerer.take().expect("opener").answer(vec![true]);
    assert!(recordable(entered.waiting.decide().await).is_some());
    let gate = entered.waiting.gate();
    let mut sc = rusqlite::Connection::open_in_memory().unwrap();
    crate::settings::init_schema(&mut sc).unwrap();

    reset_trust(&mut sc, &owner("LateGrant")).unwrap();

    assert!(!gate.is_open());
}
