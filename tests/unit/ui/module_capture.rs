//! Tests for `src/ui/module_capture.rs`, attached to it by `#[path]`.

use super::*;

#[test]
fn every_tidal_chunk_reaches_the_filter_and_nothing_else_does() {
    // The name says nothing about what a chunk holds: TIDAL regroups them between builds.
    let u = |s: &str| RequestUrl::new(s.to_string());
    for url in [
        "https://desktop.tidal.com/assets/framework-5UZfIiSp.js",
        "https://desktop.tidal.com/assets/react-Cif6l2Tn.js",
        "https://desktop.tidal.com/assets/polyfills-6IYD1ggq.js",
    ] {
        assert!(is_tidal_chunk(&u(url)), "{url}");
    }
    for url in [
        "https://resources.tidal.com/assets/framework-x.js",
        "https://desktop.tidal.com/assets/index-x.css",
        "https://desktop.tidal.com/framework-x.js",
    ] {
        assert!(!is_tidal_chunk(&u(url)), "{url}");
    }
}

// TIDAL's `framework-*` chunk in miniature: React, jsx-runtime, react-dom and react-dom/client
// behind the bundler's lazy CJS loaders under minified names, plus redux-saga's PUT runner.
const FRAMEWORK_CHUNK: &str = "var n=(e,t)=>()=>(t||(e((t={exports:{}}).exports,t),e=null),t.exports);\
var r=n((e=>{e.useState=function(){},e.createElement=function(){}})),\
m=n((e=>{e.jsx=function(){},e.jsxs=function(){}})),\
p=n((e=>{e.createPortal=function(e,t){return 1}})),\
y=n((e=>{e.createRoot=function(e,t){return 1}}));\
function Np(e,t,n){var r=t.channel,i=t.action;try{t=(r?r.put:e.dispatch)(i)}catch(e){}}\
export{r as _t,m as mt,p as pt,y as ft};";

#[test]
fn one_chunk_gets_every_rewrite_its_content_calls_for() {
    assert_eq!(
        chunk_rewrites(FRAMEWORK_CHUNK),
        [
            ChunkRewrite::TapSagaPut,
            ChunkRewrite::TagCreateRoot,
            ChunkRewrite::Capture("react"),
            ChunkRewrite::Capture("react/jsx-runtime"),
            ChunkRewrite::Capture("react-dom/client"),
        ]
    );
    let out = rewrite_chunk(FRAMEWORK_CHUNK).expect("the chunk carries signals");
    assert!(out.inert.is_empty(), "{:?}", out.inert);
    assert!(
        out.js
            .contains("globalThis.__lunaSagaPut(__lunaPutAction,e.dispatch)")
    );
    assert!(
        out.js
            .contains("e.createRoot=globalThis.__lunaCreateRoot=function(e,t){")
    );
    // One namespace, built once, registered under each id.
    assert!(out.js.ends_with(
        "\n;{const __lunaNs={_t: r, mt: m, pt: p, ft: y};globalThis.__LUNA_CAP&&(\
         globalThis.__LUNA_CAP(\"react\",__lunaNs),\
         globalThis.__LUNA_CAP(\"react/jsx-runtime\",__lunaNs),\
         globalThis.__LUNA_CAP(\"react-dom/client\",__lunaNs));}"
    ));
}

#[test]
fn a_chunk_without_any_signal_is_left_alone() {
    for src in [
        "var a=1;export{a as b};",
        // A call or a comparison of `createRoot` is not the assignment the tag needs.
        "ek.createRoot(Z9);if(m.createRoot==null){}",
        // The bare name is read by several chunks; only the one exporting it is the singleton.
        "o.outputDevices.activeDevice;",
        "console.log(1);",
    ] {
        assert!(chunk_rewrites(src).is_empty(), "{src}");
        assert!(rewrite_chunk(src).is_none(), "{src}");
    }
}

#[test]
fn a_signal_its_rewrite_finds_nothing_behind_is_reported_inert() {
    // The action type is there but not the restore saga; TIDAL changed the shape around it.
    let src = "console.log(`BOOTSTRAP_DONE`);";
    let out = rewrite_chunk(src).expect("the action type is a signal");
    assert_eq!(out.js, src);
    assert_eq!(out.inert, [ChunkRewrite::RouteVolumeRestore]);
    // A capture whose chunk exports nothing to capture.
    let src = "e.useState=function(){};";
    let out = rewrite_chunk(src).expect("the hook assignment is a signal");
    assert_eq!(out.js, src);
    assert_eq!(out.inert, [ChunkRewrite::Capture("react")]);
}

#[test]
fn captures_named_specifiers() {
    for (src, expected) in [
        (
            "var a=1,b=2;export{a as createElement,b as useState};",
            &[
                "export{a as createElement,b as useState}",
                "createElement: a",
                "useState: b",
            ][..],
        ),
        // `export{X as default}` is a named specifier like the others.
        ("var X={};export{X as default};", &["default: X"]),
        // TIDAL's react chunk tail: minified named exports of CJS loader fns. Every binding is
        // captured for modules.ts to call the right one.
        (
            "var f=1,c=2,m=3;export{f as a,c as i,m as t};",
            &["a: f", "i: c", "t: m"],
        ),
    ] {
        let out = append_capture(src, &["react"]);
        assert!(
            out.contains(r#"globalThis.__LUNA_CAP&&globalThis.__LUNA_CAP("react",{"#),
            "{src}"
        );
        for part in expected {
            assert!(out.contains(part), "{src}: {part}");
        }
    }
}

/// The parser recovers from a `const` with no initializer and still yields the export; only its
/// diagnostics keep a chunk TIDAL ships broken from being captured half-read.
#[test]
fn a_chunk_the_parser_recovers_from_with_diagnostics_is_left_unchanged() {
    let src = "export const a=1;const b;";
    assert_eq!(append_capture(src, &["react"]), src);
}

#[test]
fn tags_the_create_root_assignment() {
    // The shape of the module that assigns createRoot: one assignment onto a CJS exports
    // object, every other mention of the name being a call.
    let src =
        "var ek=t();e.createRoot=function(e,t){return 1};window.__reactRoot??=ek.createRoot(Z9);";
    let out = tag_create_root(src);
    assert!(out.contains("e.createRoot=globalThis.__lunaCreateRoot=function(e,t){"));
    // Exactly one tag lands: the calls are left alone.
    assert_eq!(out.matches("__lunaCreateRoot").count(), 1);
    assert!(out.contains("ek.createRoot(Z9)"));
}

#[test]
fn tag_skips_calls_comparisons_and_longer_names() {
    for src in [
        "ek.createRoot(container);",
        "if(m.createRoot==null){}",
        "if(m.createRoot===f){}",
        "x.createRootContainer=1;",
        "console.log(1);",
    ] {
        assert_eq!(tag_create_root(src), src, "must not tag: {src}");
    }
}

#[test]
fn captures_the_player_sdk_device_state() {
    // TIDAL's chunk as captured: one class instance with private fields,
    // exported through a specifier.
    let src = "import{f as e,t}from\"./state-BujoNwcN-BX37F7Lj.js\";\
               var g=new class{#e;#n=`shared`;set activeDevice(t){this.#e=t,this.#n=`shared`}\
               get activeDevice(){return this.#e}get deviceMode(){return this.#n}};\
               export{g as outputDevices};";
    assert_eq!(
        chunk_rewrites(src),
        [ChunkRewrite::Capture(SDK_OUTPUT_DEVICES)]
    );
    let out = rewrite_chunk(src).expect("the export is a signal").js;
    assert!(out.ends_with(
        r#"globalThis.__LUNA_CAP("@tidal-music/player/output-devices",{outputDevices: g});"#
    ));
}

#[test]
fn taps_the_put_runner_and_nothing_else() {
    // redux-saga's PUT runner as captured from TIDAL's bundle, verbatim.
    let src = "function Np(e,t,n){var r=t.channel,i=t.action,a=t.resolve;cp(function(){var t;\
               try{t=(r?r.put:e.dispatch)(i)}catch(e){n(e,!0);return}a&&wd(t)?Op(t,n):n(t)})}";
    let out = tap_saga_put(src);
    assert!(out.contains(
        "t=(r?r.put:(__lunaPutAction=>globalThis.__lunaSagaPut\
         ?globalThis.__lunaSagaPut(__lunaPutAction,e.dispatch)\
         :(0,e.dispatch)(__lunaPutAction)))(i)}catch(e)"
    ));
    assert_eq!(out.matches("__lunaSagaPut(").count(), 1);
    assert!(out.starts_with("function Np(e,t,n){"));
}

#[test]
fn tap_leaves_lookalikes_untouched() {
    for src in [
        // Two different channels: not the pick.
        "t=(r?s.put:e.dispatch)(i)",
        // The pick, never called.
        "t=(r?r.put:e.dispatch);",
        // Another property after the colon.
        "t=(r?r.put:e.getState)(i)",
        // An object literal key.
        "var o={put:1,x:2};o.put:0",
        "console.log(1);",
    ] {
        assert_eq!(tap_saga_put(src), src, "must not tap: {src}");
    }
}

// TIDAL's saved-volume restore and the saga beside it, verbatim from the shipped bundle
// (index-QBitWpxd.js).
const VOLUME_RESTORE: &str = "function*JDe(){yield L(`BOOTSTRAP_DONE`);let e=yield z(Hd);\
if(!e)return;let t=yield H(fp),n=yield H(em),r=yield H(hm);t!==e.mute&&(yield U(yi())),\
n!==e.volume&&(yield U(Ai({volume:e.volume}))),r!==e.volumeUnmute&&(yield U(ca({volume:\
e.volumeUnmute})))}";
const PLAYER_RESTORE: &str = "function*YDe(){yield L(`BOOTSTRAP_DONE`);let e=yield z(wf);\
if(!e||(yield H(Fg)))return;let t=yield H(Ah);if(t?.productId!==e.mediaId||t.productType!==\
e.mediaType)return;let n=nD()-e.setTime;e.mediaTimestamp>=0&&(yield z(D6,t,{assetPosition:\
e.mediaTimestamp,autoplay:e.currentlyPlaying&&n<15e3}))}";

#[test]
fn routes_the_saved_volume_read_and_nothing_else() {
    let src = format!("{PLAYER_RESTORE}{VOLUME_RESTORE}function*XDe(){{}}");
    let out = route_volume_restore(&src);
    assert!(out.contains(
        "let e=yield z(()=>(globalThis.__lunaRestoreVolume||(__lunaSaved=>__lunaSaved))(Hd()));\
         if(!e)return;"
    ));
    assert_eq!(out.matches("__lunaRestoreVolume").count(), 1);
    // The player-state restore takes the same action first and is left alone.
    assert!(out.starts_with(PLAYER_RESTORE));
    assert!(out.ends_with("e.volumeUnmute})))}function*XDe(){}"));
}

#[test]
fn a_restore_that_reads_no_volume_is_left_alone() {
    // The same take-read-guard opening, but the fields it reads are not the saved volume's.
    let src = "function*A(){yield L(`BOOTSTRAP_DONE`);let e=yield z(q);if(!e)return;\
               yield U(x({volume:e.level}))}function*B(){e.mute;e.volume;e.volumeUnmute}";
    assert_eq!(route_volume_restore(src), src);
    for src in [
        PLAYER_RESTORE,
        "console.log(`BOOTSTRAP_DONE`);",
        "yield L(`BOOTSTRAP_DONE`)",
    ] {
        assert_eq!(route_volume_restore(src), src, "must not route: {src}");
    }
}

// What the filter does to each kind of chunk, through the one call it makes.

#[test]
fn a_chunk_with_both_signals_gets_both_rewrites() {
    let src = format!("e.createRoot=function(e,t){{return 1}};{VOLUME_RESTORE}");
    let out = rewrite_chunk(&src)
        .expect("the fixture carries both signals")
        .js;
    assert!(out.contains("e.createRoot=globalThis.__lunaCreateRoot=function(e,t){"));
    assert!(out.contains("(globalThis.__lunaRestoreVolume||(__lunaSaved=>__lunaSaved))(Hd())"));
}

#[test]
fn a_create_root_assignment_alone_is_tagged_not_captured() {
    // React 18's react-dom/client wrapper, which TIDAL's entry chunk once carried while exporting
    // nothing: react-dom itself lives in the chunk that assigns `createPortal`.
    let src = "e.createRoot=t.createRoot,e.hydrateRoot=t.hydrateRoot;";
    assert_eq!(chunk_rewrites(src), [ChunkRewrite::TagCreateRoot]);
    let out = rewrite_chunk(src).expect("the assignment is a signal");
    assert!(out.inert.is_empty(), "{:?}", out.inert);
}

#[test]
fn the_rewrite_epoch_hash_is_fnv1a_over_the_parts_joined() {
    // The published FNV-1a 64 vector for "a": the constant is the algorithm it claims to be.
    assert_eq!(fnv1a64(&["a"]), 0xaf63_dc4c_8601_ec8c);
    // Parts hash as one string: splitting a file's text differently cannot move the epoch.
    assert_eq!(fnv1a64(&["ab", "c"]), fnv1a64(&["a", "bc"]));
    // Any byte of difference moves it, which is the whole contract with the service worker cache.
    assert_ne!(fnv1a64(&["tap_saga_put"]), fnv1a64(&["tap_saga_pUt"]));
}

// The shape of the chunk that defines the SDK's API object: the bundler's export helper builds it from
// a table of getters named as the SDK's public functions, and the chunk exports the object under a
// minified name.
const SDK_CHUNK: &str = "var w=h({},{load:()=>m7,reset:()=>o7,setNext:()=>p7,startNativePlayer:()=>N7});\
                         export{w as o,p7 as F};";

#[test]
fn the_chunk_that_defines_the_sdk_api_has_its_exports_captured_under_its_id() {
    let out = rewrite_chunk(SDK_CHUNK)
        .expect("the getter table is a signal")
        .js;
    assert!(out.starts_with(SDK_CHUNK));
    assert!(out.ends_with(r#"globalThis.__LUNA_CAP("@tidal-music/player",{o: w, F: p7});"#));
}

#[test]
fn a_chunk_that_does_not_define_the_sdk_api_is_left_be() {
    // One of the two names is not the table: a chunk that merely mentions `setNext`.
    for partial in [
        "var a={setNext:()=>1};export{a as b};",
        "var a={startNativePlayer:()=>1};export{a as b};",
    ] {
        assert!(rewrite_chunk(partial).is_none(), "{partial}");
    }
}
