//! Tests for `src/platform/process_args.rs`, attached by `#[path]`.

fn args(items: &[&str]) -> Vec<std::ffi::OsString> {
    items.iter().map(std::ffi::OsString::from).collect()
}

#[test]
fn a_chromium_subprocess_is_told_by_its_type_switch() {
    assert!(super::is_helper_process(
        args(&["/path/to/exe", "--type=renderer", "--lang=en-US"]).into_iter()
    ));
}

#[test]
fn a_switch_that_only_resembles_the_type_switch_is_the_browser() {
    // Chromium always spells it `--type=<kind>`; neither of these is that.
    assert!(!super::is_helper_process(
        args(&["/path/to/exe", "--type", "--user-agent=x--type=renderer"]).into_iter()
    ));
}

#[cfg(not(target_os = "windows"))]
#[test]
fn an_argument_that_is_not_valid_unicode_does_not_hide_the_type_switch() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut items = args(&["/path/to/exe"]);
    items.push(OsString::from_vec(vec![0x2d, 0x2d, 0xff, 0xfe]));
    items.push(OsString::from("--type=gpu-process"));

    assert!(super::is_helper_process(items.into_iter()));
}
