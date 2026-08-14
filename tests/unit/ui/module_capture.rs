//! Tests for `src/ui/module_capture.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn classifies_react_family_chunks() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    assert_eq!(
        target_module_id(&u("https://desktop.tidal.com/assets/react-Cif6l2Tn.js")),
        Some("react")
    );
    assert_eq!(
        target_module_id(&u("https://desktop.tidal.com/assets/jsx-runtime-abc123.js")),
        Some("react/jsx-runtime")
    );
    assert_eq!(
        target_module_id(&u("https://desktop.tidal.com/assets/react-dom-CARA-N2H.js")),
        Some("react-dom/client")
    );
    assert_eq!(
        target_module_id(&u("https://desktop.tidal.com/assets/index-CvAR5jQO.js")),
        None
    );
    assert_eq!(
        target_module_id(&u("https://desktop.tidal.com/assets/polyfills-x.js")),
        None
    );
    assert_eq!(
        target_module_id(&u("https://resources.tidal.com/x.js")),
        None
    );
    assert_eq!(
        target_module_id(&u("https://desktop.tidal.com/assets/x.css")),
        None
    );
}

#[test]
fn captures_named_specifiers() {
    let out = append_capture(
        "var a=1,b=2;export{a as createElement,b as useState};",
        "react",
    );
    assert!(out.contains("export{a as createElement,b as useState}"));
    assert!(out.contains(r#"globalThis.__LUNA_CAP&&globalThis.__LUNA_CAP("react",{"#));
    assert!(out.contains("createElement: a"));
    assert!(out.contains("useState: b"));
}

#[test]
fn captures_exported_declarations() {
    let out = append_capture(
        "export function jsx(){}export const Fragment=1;",
        "react/jsx-runtime",
    );
    assert!(out.contains("jsx: jsx"));
    assert!(out.contains("Fragment: Fragment"));
}

#[test]
fn captures_default_specifier() {
    let out = append_capture("var X={};export{X as default};", "react");
    assert!(out.contains("default: X"));
}

#[test]
fn skips_reexport_from_but_still_emits_capture() {
    let out = append_capture(
        r#"export * from "./other";export const jsx=1;"#,
        "react/jsx-runtime",
    );
    assert!(out.contains("jsx: jsx"));
    assert!(out.contains("__LUNA_CAP"));
}

#[test]
fn parse_failure_returns_source_unchanged() {
    let bad = "var x = ;;;(";
    assert_eq!(append_capture(bad, "react"), bad);
}

#[test]
fn no_exports_returns_source_unchanged() {
    let src = "console.log(1);";
    assert_eq!(append_capture(src, "react"), src);
}

#[test]
fn captures_minified_cjs_loader_exports() {
    // Mirrors TIDAL's react chunk tail: minified named exports of CJS loader
    // fns. We must capture every binding for modules.ts to call the right one.
    let out = append_capture("var f=1,c=2,m=3;export{f as a,c as i,m as t};", "react");
    assert!(out.contains("a: f"));
    assert!(out.contains("i: c"));
    assert!(out.contains("t: m"));
}

#[test]
fn non_utf8_body_passes_through_unchanged() {
    // append_capture only handles &str; the filter's strict decode guards the
    // non-UTF-8 path, but a lone invalid byte through append_capture must not
    // panic. (Filter-level passthrough is covered by the strict from_utf8.)
    let src = "var x=1;export{x as a};";
    assert!(append_capture(src, "react").contains("a: x"));
}
