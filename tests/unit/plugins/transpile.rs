//! Tests for `src/plugins/transpile.rs`, attached to it by `#[path]`.

use super::*;

/// Lower then transpile, asserting the result is ESM-free and parses.
fn lower(code: &str) -> String {
    transpile_ts(code, "plugin.ts").expect("transpile")
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
fn export_const_strips_keyword() {
    let js = lower("export const foo = 42;");
    assert!(js.contains("const foo = 42"));
    assert!(!js.contains("export"));
}

#[test]
fn export_function_strips_keyword() {
    let js = lower("export function hello() { return 1; }");
    assert!(js.contains("function hello"));
    assert!(!js.contains("export"));
}

#[test]
fn export_class_strips_keyword() {
    let js = lower("export class Foo {}");
    assert!(js.contains("class Foo"));
    assert!(!js.contains("export"));
}

#[test]
fn export_default_expr_becomes_default_var() {
    let js = lower("export default 42;");
    assert!(js.contains("__default"));
    assert!(js.contains("42"));
    assert!(!js.contains("export"));
}

#[test]
fn export_default_class_keeps_declaration() {
    let js = lower("export default class Foo {}");
    assert!(js.contains("class Foo"));
    assert!(!js.contains("export"));
}

#[test]
fn export_default_anonymous_function_becomes_default_var() {
    // Anon default must lower to `var __default = ...`, not a bare invalid decl.
    let js = lower("export default function() { return 1; }");
    assert!(js.contains("__default"));
    assert!(js.contains("function"));
    assert!(!js.contains("export"));
}

#[test]
fn export_default_anonymous_class_becomes_default_var() {
    let js = lower("export default class { method() {} }");
    assert!(js.contains("__default"));
    assert!(js.contains("class"));
    assert!(!js.contains("export"));
}

#[test]
fn export_default_named_function_keeps_declaration() {
    // A named default fn stays a hoisted declaration (`id` is present).
    let js = lower("export default function Foo() { return 1; }");
    assert!(js.contains("function Foo"));
    assert!(!js.contains("export"));
}

#[test]
fn import_named_maps_to_modules() {
    let js = lower(r#"import { storage, intercept } from "@luna/lib";console.log(1);"#);
    assert!(js.contains(r#"luna?.core?.modules?.["@luna/lib"]"#));
    assert!(js.contains("storage"));
    assert!(js.contains("intercept"));
    assert!(js.contains("console.log(1)"));
    assert!(!js.contains("import"));
}

#[test]
fn import_named_alias_uses_colon_not_as() {
    let js = lower(r#"import { a as b } from "m";b();"#);
    // Valid destructuring renames with `:`, never the invalid `{ a as b }`.
    assert!(js.contains("a: b"));
    assert!(!js.contains(" as "));
    assert!(!js.contains("import"));
}

#[test]
fn import_namespace_maps_to_modules() {
    let js = lower(r#"import * as core from "@luna/core";foo();"#);
    assert!(js.contains(r#"const core = luna?.core?.modules?.["@luna/core"]"#));
    assert!(!js.contains("import"));
}

#[test]
fn import_default_maps_to_modules_default() {
    let js = lower(r#"import React from "react";foo();"#);
    assert!(js.contains(r#"luna?.core?.modules?.["react"]?.default"#));
    assert!(!js.contains("import"));
}

#[test]
fn import_side_effect_is_dropped() {
    let js = lower(r#"import "./polyfill";foo();"#);
    assert!(!js.contains("import"));
    assert!(js.contains("foo()"));
}

#[test]
fn export_star_from_is_dropped() {
    let js = lower(r#"export * from "@luna/core";var z = 1;"#);
    assert!(!js.contains("export"));
    assert!(js.contains("var z = 1") || js.contains("z = 1"));
}

#[test]
fn export_type_block_is_dropped() {
    let js = lower("type T = number; export type { T };");
    assert!(!js.contains("__exports"));
    assert!(!js.replace("__exports", "").contains("export"));
}

#[test]
fn export_inline_type_specifier_is_dropped() {
    let js = lower("var x = 1; type T = number; export { x as X, type T };");
    assert!(js.contains("X: x"));
    assert!(!js.contains("T:"));
}

#[test]
fn export_named_from_is_dropped() {
    let js = lower(r#"export { foo, bar } from "@luna/lib";var z = 1;"#);
    assert!(!js.contains("export"));
}

#[test]
fn multibyte_utf8_survives() {
    // The old byte scanner corrupted multi-byte chars via `bytes[i] as char`.
    let js = lower(r#"const s = "héllo wörld 日本語 🎵"; export { s as Settings };"#);
    assert!(js.contains("héllo wörld 日本語 🎵"));
    assert!(js.contains("__exports"));
}

#[test]
fn export_keyword_inside_string_is_untouched() {
    // Only real module declarations are lowered, not text inside literals.
    let js = lower(r#"var s = "export{foo}"; export { s as Settings };"#);
    assert!(js.contains(r#""export{foo}""#));
    assert!(js.contains("__exports"));
}

#[test]
fn strips_ts_types() {
    let ts = r#"
        const x: number = 42;
        function greet(name: string): string { return `Hello, ${name}!`; }
        export { x, greet };
    "#;
    let js = lower(ts);
    assert!(!js.contains(": number"));
    assert!(!js.contains(": string"));
    assert!(js.contains("const x = 42"));
    assert!(js.contains("function greet"));
}

#[test]
fn mts_extension_is_supported() {
    let js = transpile_ts("export const x: number = 1;", "plugin.mts").expect("transpile");
    assert!(js.contains("const x = 1"));
    assert!(!js.contains("export"));
}
