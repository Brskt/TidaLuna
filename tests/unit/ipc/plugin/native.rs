//! Tests for `src/ipc/plugin/native.rs`, attached to it by `#[path]`.

use super::*;

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

    assert!(!admits_new_module(names.iter().copied(), "Foo"));
    assert!(
        admits_new_module(names.iter().copied(), "Bar"),
        "one plugin's modules must not exhaust another plugin's budget"
    );

    let under = &names[..MAX_MODULES_PER_PLUGIN - 1];
    assert!(admits_new_module(under.iter().copied(), "Foo"));
}

/// The bound counts the ledger's entries, and the ledger is keyed by module: a module cannot be
/// counted twice however many attempts it has in flight. Splitting settled and in-flight state across
/// two ledgers double-counted a module being re-registered and refused a genuinely new one with a 403
/// while the plugin was nowhere near the bound.
#[test]
fn a_module_is_counted_once_however_many_attempts_it_has() {
    let mut ledger: std::collections::HashMap<String, ModuleEntry> =
        std::collections::HashMap::new();

    // Seven distinct modules, one of them being re-registered while already registered.
    for n in 0..MAX_MODULES_PER_PLUGIN - 1 {
        let entry = ledger.entry(format!("Foo/{n}.native.ts")).or_default();
        entry.in_flight = if n == 0 { 2 } else { 0 };
    }

    assert_eq!(ledger.len(), MAX_MODULES_PER_PLUGIN - 1);
    assert!(
        admits_new_module(ledger.keys().map(String::as_str), "Foo"),
        "an eighth distinct module must still be admitted"
    );
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

#[test]
fn an_unknown_token_resolves_to_nothing() {
    assert!(module_for_native_channel("deadbeef").is_none());
    assert!(module_for_native_channel("").is_none());
}

#[test]
fn two_modules_never_share_a_token() {
    let a = issue_native_channel("PairA/x.native.ts", "hash").expect("entropy available");
    let b = issue_native_channel("PairB/x.native.ts", "hash").expect("entropy available");
    assert_ne!(a, b);
    assert_eq!(
        module_for_native_channel(&b).as_deref(),
        Some("PairB/x.native.ts")
    );
}

/// `plugin.uninstall_all` passes an empty prefix. Appending the separator first made it match nothing:
/// a full uninstall left every token callable after its trust rows were deleted. Tests the rule,
/// not the map, which the other tests here are holding tokens in.
#[test]
fn an_empty_prefix_owns_every_module() {
    assert!(module_belongs_to("Anything/x.native.ts", ""));
    assert!(module_belongs_to("bare", ""));
}

#[test]
fn a_prefix_owns_its_own_modules_only() {
    assert!(module_belongs_to("Foo/x.native.ts", "Foo"));
    assert!(module_belongs_to("Foo", "Foo"));
    assert!(
        !module_belongs_to("Foobar/x.native.ts", "Foo"),
        "the separator must stop a name-prefix neighbour"
    );
    assert!(!module_belongs_to("Other/x.native.ts", "Foo"));
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
