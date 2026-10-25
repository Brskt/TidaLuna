//! Tests for `src/plugins/transpile.rs`, attached to it by `#[path]`.

use super::*;

/// Lower then transpile, asserting the result is ESM-free and parses.
fn lower(code: &str) -> String {
    transpile_ts(code, "plugin.ts").expect("transpile")
}

/// Lowers `code` and checks what must survive (`kept`) and what must not (`absent`).
fn assert_lowered(code: &str, kept: &[&str], absent: &[&str]) -> String {
    let js = lower(code);
    for needle in kept {
        assert!(js.contains(needle), "{code}: {js}");
    }
    for needle in absent {
        assert!(!js.contains(needle), "{code}: {js}");
    }
    js
}

#[test]
fn export_block_becomes_exports_object() {
    let js = lower("var x=1;var y=2;export{x as Settings,y as unloads};");
    assert!(js.contains("__exports"));
    assert!(js.contains("Settings"));
    assert!(js.contains("unloads"));
    // `__exports` legitimately contains the substring "export"; ensure no
    // real `export` statement survives once that token is removed.
    assert!(!js.replace("__exports", "").contains("export"));
}

#[test]
fn an_export_is_lowered_without_its_keyword() {
    let rows: [(&str, &[&str], &[&str]); 8] = [
        ("export const foo = 42;", &["const foo = 42"], &[]),
        (
            "export function hello() { return 1; }",
            &["function hello"],
            &[],
        ),
        ("export class Foo {}", &["class Foo"], &[]),
        ("export default 42;", &["__default", "42"], &[]),
        // A declaration, not an expression bound to the anonymous default's name.
        (
            "export default class Foo {}",
            &["class Foo"],
            &["__default"],
        ),
        // An anonymous default must lower to `var __default = ...`, not a bare invalid declaration.
        (
            "export default function() { return 1; }",
            &["__default", "function"],
            &[],
        ),
        (
            "export default class { method() {} }",
            &["__default", "class"],
            &[],
        ),
        // A named default function stays a hoisted declaration.
        (
            "export default function Foo() { return 1; }",
            &["function Foo"],
            &["__default"],
        ),
    ];
    for (code, kept, absent) in rows {
        let js = assert_lowered(code, kept, absent);
        assert!(!js.contains("export"), "{code}: {js}");
    }
}

#[test]
fn an_import_is_lowered_to_a_modules_lookup() {
    let rows: [(&str, &[&str], &[&str]); 5] = [
        (
            r#"import { storage, intercept } from "m";console.log(1);"#,
            &[
                r#"luna?.core?.modules?.["m"]"#,
                "storage",
                "intercept",
                "console.log(1)",
            ],
            &[],
        ),
        // Valid destructuring renames with `:`, never the invalid `{ a as b }`.
        (r#"import { a as b } from "m";b();"#, &["a: b"], &[" as "]),
        (
            r#"import * as core from "@luna/core";foo();"#,
            &[r#"const core = luna?.core?.modules?.["@luna/core"]"#],
            &[],
        ),
        (
            r#"import React from "react";foo();"#,
            &[r#"luna?.core?.modules?.["react"]?.default"#],
            &[],
        ),
        (r#"import "./polyfill";foo();"#, &["foo()"], &[]),
    ];
    for (code, kept, absent) in rows {
        let js = assert_lowered(code, kept, absent);
        assert!(!js.contains("import"), "{code}: {js}");
    }
}

/// `@luna/lib` is the exception, and the reason is identity. Its `ipcRenderer.invoke` closes over
/// the app bundle's scope; a shadow injected into a plugin's wrapper cannot reach it, and the
/// channels that act on the caller's identity used to arrive with none, which is how
/// `registerNative` came to read the acting plugin off an argument. Lowering the import to a call
/// carrying `__cap` is what makes the route attributable: this is the shape the Rust-side gate
/// depends on.
#[test]
fn the_lib_import_is_bound_to_the_calling_plugins_capability() {
    let js = lower(r#"import { ipcRenderer } from "@luna/lib";ipcRenderer.invoke("x");"#);
    assert!(
        js.contains(r#"luna?.core?.modules?.["__lunaLibFor"]?.(__cap)"#),
        "the lib import must resolve to the per-plugin copy, not the shared object:\n{js}"
    );
    assert!(
        !js.contains(r#"luna?.core?.modules?.["@luna/lib"]"#),
        "the shared object carries no identity and must not be what a plugin gets:\n{js}"
    );
    assert!(js.contains("ipcRenderer"));
    assert!(!js.contains("import"));
}

/// The rebinding is for that one specifier and nothing near it: a plugin importing the native half
/// still gets the shared object, and nothing else changes shape.
#[test]
fn the_lib_rebinding_does_not_catch_its_neighbours() {
    let js = lower(r#"import { x } from "@luna/lib.native";x();"#);
    assert!(js.contains(r#"luna?.core?.modules?.["@luna/lib.native"]"#));
    assert!(!js.contains("__lunaLibFor"));
}

#[test]
fn exports_that_bind_nothing_at_runtime_are_dropped() {
    let rows: [(&str, &[&str], &[&str]); 4] = [
        (
            r#"export * from "@luna/core";var z = 1;"#,
            &["z = 1"],
            &["export"],
        ),
        ("type T = number; export type { T };", &[], &["export"]),
        // The exports object may spell a kept specifier as shorthand: no `T` may appear at all.
        (
            "var x = 1; type T = number; export { x as X, type T };",
            &["X: x"],
            &["T"],
        ),
        (
            r#"export { foo, bar } from "@luna/lib";var z = 1;"#,
            &[],
            &["export"],
        ),
    ];
    for (code, kept, absent) in rows {
        assert_lowered(code, kept, absent);
    }
}
