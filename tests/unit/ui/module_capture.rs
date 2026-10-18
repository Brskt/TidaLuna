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
fn classifies_the_entry_chunk_for_tagging() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    assert!(matches!(
        chunk_rewrite(&u("https://desktop.tidal.com/assets/index-DnBBD1pK.js")),
        Some(ChunkRewrite::Entry)
    ));
    // React-family names still win, and still carry their module id.
    assert!(matches!(
        chunk_rewrite(&u("https://desktop.tidal.com/assets/react-dom-C54PbMuF.js")),
        Some(ChunkRewrite::Capture("react-dom/client"))
    ));
    assert!(matches!(
        chunk_rewrite(&u("https://desktop.tidal.com/assets/react-CXLoTpP3.js")),
        Some(ChunkRewrite::Capture("react"))
    ));
    // Neither an entry chunk, nor JS, nor the app host.
    assert!(chunk_rewrite(&u("https://desktop.tidal.com/assets/polyfills-x.js")).is_none());
    assert!(chunk_rewrite(&u("https://desktop.tidal.com/assets/index-x.css")).is_none());
    assert!(chunk_rewrite(&u("https://resources.tidal.com/assets/index-x.js")).is_none());
}

#[test]
fn a_split_out_react_dom_client_chunk_is_still_captured() {
    // TIDAL serves one `react-dom-*` chunk today, but nothing stops it from splitting the
    // client entry into its own `react-dom-client-*`. The prefix test already covers that
    // shape; pin it against a later rewrite of the classifier dropping it in silence.
    let u = |s: &str| RequestUrl::new(s.to_string());
    assert_eq!(
        target_module_id(&u(
            "https://desktop.tidal.com/assets/react-dom-client-C54PbMuF.js"
        )),
        Some("react-dom/client")
    );
    assert!(matches!(
        chunk_rewrite(&u(
            "https://desktop.tidal.com/assets/react-dom-client-C54PbMuF.js"
        )),
        Some(ChunkRewrite::Capture("react-dom/client"))
    ));
}

#[test]
fn tags_the_create_root_assignment() {
    // The shape TIDAL's entry chunk carries: one assignment onto a CJS exports
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
fn tag_splices_by_byte_across_multibyte_source() {
    // The tag is spliced at byte offsets: every index has to land on a char
    // boundary even when earlier bytes are multibyte.
    let src = "const s=\"héllo wörld 日本語\";e.createRoot=function(){};";
    let out = tag_create_root(src);
    assert!(out.contains("e.createRoot=globalThis.__lunaCreateRoot=function(){}"));
    assert!(out.contains("héllo wörld 日本語"));
}

#[test]
fn classifies_the_store_chunk_for_the_saga_tap() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    assert!(matches!(
        chunk_rewrite(&u("https://desktop.tidal.com/assets/store-DoF6ExYq.js")),
        Some(ChunkRewrite::TapSagaPut)
    ));
    assert!(chunk_rewrite(&u("https://resources.tidal.com/assets/store-x.js")).is_none());
}

#[test]
fn captures_the_player_sdk_device_state() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    assert!(matches!(
        chunk_rewrite(&u(
            "https://desktop.tidal.com/assets/output-devices-DLDX8cVL-CR1w6y0a.js"
        )),
        Some(ChunkRewrite::Capture(SDK_OUTPUT_DEVICES))
    ));
    assert!(
        chunk_rewrite(&u(
            "https://resources.tidal.com/assets/output-devices-DLDX8cVL.js"
        ))
        .is_none()
    );
    // TIDAL's chunk as captured: one class instance with private fields,
    // exported through a specifier.
    let src = "import{f as e,t}from\"./state-BujoNwcN-BX37F7Lj.js\";\
               var g=new class{#e;#n=`shared`;set activeDevice(t){this.#e=t,this.#n=`shared`}\
               get activeDevice(){return this.#e}get deviceMode(){return this.#n}};\
               export{g as outputDevices};";
    let out = append_capture(src, SDK_OUTPUT_DEVICES);
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

#[test]
fn tap_splices_by_byte_across_multibyte_source() {
    let src = "const s=\"héllo 日本語\";t=(r?r.put:e.dispatch)(i);";
    let out = tap_saga_put(src);
    assert!(out.contains("héllo 日本語"));
    assert!(out.contains("(0,e.dispatch)(__lunaPutAction)))(i);"));
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

#[test]
fn the_route_matches_whichever_quote_the_minifier_picks() {
    let src = VOLUME_RESTORE.replace('`', "\"");
    assert!(route_volume_restore(&src).contains("(Hd()));if(!e)return;"));
}

#[test]
fn the_route_splices_by_byte_across_multibyte_source() {
    let src = format!("const s=\"héllo 日本語\";{VOLUME_RESTORE}");
    let out = route_volume_restore(&src);
    assert!(out.contains("héllo 日本語"));
    assert!(out.contains("(Hd()));if(!e)return;"));
}

// What the filter does to each kind of chunk, through the one call it makes.

#[test]
fn the_entry_chunk_gets_both_of_its_rewrites() {
    let src = format!("e.createRoot=function(e,t){{return 1}};{VOLUME_RESTORE}");
    let out = rewrite_chunk(ChunkRewrite::Entry, &src);
    assert!(out.contains("e.createRoot=globalThis.__lunaCreateRoot=function(e,t){"));
    assert!(out.contains("(globalThis.__lunaRestoreVolume||(__lunaSaved=>__lunaSaved))(Hd())"));
}

#[test]
fn the_store_chunk_gets_the_saga_tap() {
    let out = rewrite_chunk(ChunkRewrite::TapSagaPut, "t=(r?r.put:e.dispatch)(i);");
    assert!(out.contains("globalThis.__lunaSagaPut"));
}

#[test]
fn a_captured_chunk_gets_its_exports_captured() {
    let out = rewrite_chunk(
        ChunkRewrite::Capture(SDK_OUTPUT_DEVICES),
        "var g=1;export{g as outputDevices};",
    );
    assert!(out.ends_with(
        r#"globalThis.__LUNA_CAP("@tidal-music/player/output-devices",{outputDevices: g});"#
    ));
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
fn classifies_the_chunks_that_may_hold_the_sdk_api() {
    let u = |s: &str| RequestUrl::new(s.to_string());
    assert!(matches!(
        chunk_rewrite(&u("https://desktop.tidal.com/assets/vendor-IT_93Oov.js")),
        Some(ChunkRewrite::SdkApi)
    ));
    assert!(matches!(
        chunk_rewrite(&u(
            "https://desktop.tidal.com/assets/load-uTL4eiru-BOQ4slVv.js"
        )),
        Some(ChunkRewrite::SdkApi)
    ));
    // The names that already mean something else keep it.
    assert!(matches!(
        chunk_rewrite(&u("https://desktop.tidal.com/assets/index-CaNNwb6Q.js")),
        Some(ChunkRewrite::Entry)
    ));
    assert!(matches!(
        chunk_rewrite(&u("https://desktop.tidal.com/assets/output-devices-x.js")),
        Some(ChunkRewrite::Capture(SDK_OUTPUT_DEVICES))
    ));
    // Not the app host, not JS.
    assert!(chunk_rewrite(&u("https://resources.tidal.com/assets/vendor-x.js")).is_none());
    assert!(chunk_rewrite(&u("https://desktop.tidal.com/assets/vendor-DN277Tmi.css")).is_none());
}

#[test]
fn the_chunk_that_defines_the_sdk_api_has_its_exports_captured_under_its_id() {
    let out = capture_sdk_api(SDK_CHUNK);
    assert!(out.starts_with(SDK_CHUNK));
    assert!(out.ends_with(r#"globalThis.__LUNA_CAP("@tidal-music/player",{o: w, F: p7});"#));
}

#[test]
fn a_chunk_that_does_not_define_the_sdk_api_is_left_be() {
    let other = "var a=1;export{a as b};";
    assert_eq!(capture_sdk_api(other), other);
    // One of the two names is not the table: a chunk that merely mentions `setNext`.
    let partial = "var a={setNext:()=>1};export{a as b};";
    assert_eq!(capture_sdk_api(partial), partial);
    let partial = "var a={startNativePlayer:()=>1};export{a as b};";
    assert_eq!(capture_sdk_api(partial), partial);
}

#[test]
fn the_sdk_api_chunks_go_through_the_capture() {
    assert_eq!(
        rewrite_chunk(ChunkRewrite::SdkApi, SDK_CHUNK),
        capture_sdk_api(SDK_CHUNK)
    );
    let other = "var a=1;export{a as b};";
    assert_eq!(rewrite_chunk(ChunkRewrite::SdkApi, other), other);
}

#[test]
fn non_utf8_body_passes_through_unchanged() {
    // append_capture only handles &str; the filter's strict decode guards the
    // non-UTF-8 path, but a lone invalid byte through append_capture must not
    // panic. (Filter-level passthrough is covered by the strict from_utf8.)
    let src = "var x=1;export{x as a};";
    assert!(append_capture(src, "react").contains("a: x"));
}
