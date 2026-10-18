//! Tests for `src/platform/process_args.rs`, attached by `#[path]`.

#[cfg(not(target_os = "windows"))]
#[test]
fn every_argument_keeps_its_bytes_even_when_they_are_not_utf8() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let args = [
        b"/opt/tidalunar/tidalunar".to_vec(),
        b"tidal://album/1".to_vec(),
        b"\xff".to_vec(),
        b"Jos\xe9".to_vec(),
    ];
    let buffers = super::argv_buffers(args.iter().cloned().map(OsString::from_vec));

    assert_eq!(buffers.len(), args.len());
    for (buffer, arg) in buffers.iter().zip(&args) {
        assert_eq!(buffer.last(), Some(&0), "{buffer:?} is not NUL-terminated");
        assert_eq!(&buffer[..buffer.len() - 1], arg.as_slice());
    }
}

#[cfg(target_os = "windows")]
#[test]
fn a_command_line_that_is_not_valid_unicode_is_borrowed_as_it_is() {
    // A lone surrogate: what `std::env::args()` refuses with a panic.
    static WIDE: [u16; 3] = [b'a' as u16, 0xD800, b'b' as u16];
    let line = super::borrowed_utf16(&WIDE);
    assert_eq!(line.str_.cast_const(), WIDE.as_ptr());
    assert_eq!(line.length, WIDE.len());
    assert!(
        line.dtor.is_none(),
        "a borrowed line must not be freed by CEF"
    );
}

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
