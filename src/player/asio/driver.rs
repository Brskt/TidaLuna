//! ASIO driver discovery: enumerate `HKLM\SOFTWARE\ASIO\<name>` and parse each
//! `CLSID`. The CLSID is a `u128` so the parsing stays platform-independent; the
//! Windows loader turns it into a `GUID` at the COM boundary.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

/// An installed ASIO driver: its display name and class id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AsioDriverInfo {
    pub(crate) name: String,
    pub(crate) clsid: u128,
}

/// Parse a registry CLSID string (`{8-4-4-4-12}` hex groups, braces optional)
/// into a `u128` ordered so `GUID::from_u128` reproduces the textual form.
/// Returns `None` for any malformed value.
fn parse_clsid(s: &str) -> Option<u128> {
    let t = s.trim();
    let t = t.strip_prefix('{').unwrap_or(t);
    let t = t.strip_suffix('}').unwrap_or(t);
    let groups: Vec<&str> = t.split('-').collect();
    let want = [8usize, 4, 4, 4, 12];
    if groups.len() != want.len() {
        return None;
    }
    let mut hex = String::with_capacity(32);
    for (group, &len) in groups.iter().zip(want.iter()) {
        if group.len() != len || !group.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        hex.push_str(group);
    }
    u128::from_str_radix(&hex, 16).ok()
}

/// Build an `AsioDriverInfo` from a registry row (the subkey name and its
/// `CLSID` value). Returns `None` for an empty name or an unparseable CLSID.
pub(crate) fn parse_driver_row(name: &str, clsid_str: &str) -> Option<AsioDriverInfo> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let clsid = parse_clsid(clsid_str)?;
    Some(AsioDriverInfo {
        name: name.to_string(),
        clsid,
    })
}

/// Enumerate installed ASIO drivers from `HKLM\SOFTWARE\ASIO`.
#[cfg(target_os = "windows")]
pub(crate) fn enumerate_asio_drivers() -> Vec<AsioDriverInfo> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };

    let mut drivers = Vec::new();
    let path = to_wide("SOFTWARE\\ASIO");
    let mut root: HKEY = core::ptr::null_mut();
    // SAFETY: open the ASIO hive for read; `root` is closed below.
    let opened =
        unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut root) };
    if opened != ERROR_SUCCESS {
        return drivers; // no ASIO hive -> no drivers installed
    }

    let mut index = 0u32;
    loop {
        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        // SAFETY: enumerate the subkeys of the open root key.
        let rc = unsafe {
            RegEnumKeyExW(
                root,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                core::ptr::null(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            break; // ERROR_NO_MORE_ITEMS or a read error -> stop
        }
        let subkey = String::from_utf16_lossy(&name[..name_len as usize]);
        if let Some(clsid) = read_clsid_value(root, &subkey)
            && let Some(info) = parse_driver_row(&subkey, &clsid)
        {
            drivers.push(info);
        }
        index += 1;
    }

    // SAFETY: close the key opened above.
    unsafe { RegCloseKey(root) };
    drivers
}

/// Read the `CLSID` string value of a subkey under the open ASIO root key.
#[cfg(target_os = "windows")]
fn read_clsid_value(
    root: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
) -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };

    let sub = to_wide(subkey);
    let mut hk: HKEY = core::ptr::null_mut();
    // SAFETY: open the named subkey for read.
    if unsafe { RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ, &mut hk) } != ERROR_SUCCESS {
        return None;
    }

    let value = to_wide("CLSID");
    let mut buf = [0u16; 128];
    let mut bytes = (buf.len() * 2) as u32; // capacity in bytes
    // SAFETY: read the CLSID string value into the fixed buffer.
    let rc = unsafe {
        RegQueryValueExW(
            hk,
            value.as_ptr(),
            core::ptr::null(),
            core::ptr::null_mut(),
            buf.as_mut_ptr() as *mut u8,
            &mut bytes,
        )
    };
    // SAFETY: close the subkey.
    unsafe { RegCloseKey(hk) };
    if rc != ERROR_SUCCESS {
        return None;
    }

    // `bytes` is the byte length including the trailing NUL.
    let len = (bytes as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Log detected ASIO drivers at startup. Env vars run a smoke test against the first
/// driver: `ASIO_PROBE` (capabilities), `ASIO_TONE`/`ASIO_RING` (tone), `ASIO_FILE=<path>`
/// (decode a file).
#[cfg(target_os = "windows")]
pub(crate) fn log_asio_drivers() {
    let drivers = enumerate_asio_drivers();
    crate::vprintln!("[ASIO] {} driver(s) detected", drivers.len());
    for d in &drivers {
        crate::vprintln!("[ASIO]   {} (clsid {:032x})", d.name, d.clsid);
    }
    if std::env::var_os("ASIO_PROBE").is_some()
        && let Some(first) = drivers.first()
    {
        probe_driver(first);
    }
    if std::env::var_os("ASIO_TONE").is_some()
        && let Some(first) = drivers.first()
    {
        super::host::run_tone_test(first);
    }
    if std::env::var_os("ASIO_RING").is_some()
        && let Some(first) = drivers.first()
    {
        super::host::run_ring_tone_test(first);
    }
    if let Some(path) = std::env::var_os("ASIO_FILE")
        && let Some(first) = drivers.first()
    {
        super::host::run_flac_test(first, path);
    }
}

/// Open a driver via COM and log its capabilities. Verifies the COM call
/// sequence against a real driver; the driver is released on drop.
#[cfg(target_os = "windows")]
fn probe_driver(info: &AsioDriverInfo) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    use super::convert::AsioSampleType;
    use super::iasio::{AsioChannelInfo, AsioDriver, asio_ok};

    // SAFETY: a standard ASIO capability probe; the driver is disposed on drop.
    unsafe {
        let driver = match AsioDriver::create(info.clsid) {
            Ok(d) => d,
            Err(hr) => {
                crate::vprintln!("[ASIO] '{}' create failed: hr={hr:#010x}", info.name);
                return;
            }
        };
        // The driver uses the window handle to parent its control panel; the
        // desktop window is a valid parent for a probe.
        if driver.init(GetDesktopWindow()) == 0 {
            crate::vprintln!("[ASIO] '{}' init failed", info.name);
            return;
        }
        crate::vprintln!("[ASIO] '{}' opened:", info.name);

        let (mut num_in, mut num_out) = (0i32, 0i32);
        if asio_ok(driver.get_channels(&mut num_in, &mut num_out)) {
            crate::vprintln!("[ASIO]   channels: {num_in} in / {num_out} out");
        }

        let (mut min, mut max, mut pref, mut gran) = (0i32, 0i32, 0i32, 0i32);
        if asio_ok(driver.get_buffer_size(&mut min, &mut max, &mut pref, &mut gran)) {
            crate::vprintln!("[ASIO]   buffer frames: min={min} max={max} pref={pref} gran={gran}");
        }

        let mut rate = 0.0f64;
        if asio_ok(driver.get_sample_rate(&mut rate)) {
            crate::vprintln!("[ASIO]   current sample rate: {rate} Hz");
        }

        // Output channel 0's sample type (channel index 0, is_input = false).
        let mut ch = AsioChannelInfo {
            channel: 0,
            is_input: 0,
            is_active: 0,
            channel_group: 0,
            sample_type: 0,
            name: [0; 32],
        };
        if asio_ok(driver.get_channel_info(&mut ch)) {
            let supported = if AsioSampleType::from_asio(ch.sample_type).is_some() {
                "supported"
            } else {
                "UNSUPPORTED"
            };
            crate::vprintln!(
                "[ASIO]   out ch0 sample type: {} ({supported})",
                ch.sample_type
            );
        }
    }
}

/// Encode a string as a NUL-terminated UTF-16 buffer for the wide registry API.
#[cfg(target_os = "windows")]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_braced_clsid() {
        let row = parse_driver_row("RME ASIO", "{12345678-1234-1234-1234-123456789ABC}").unwrap();
        assert_eq!(row.name, "RME ASIO");
        assert_eq!(row.clsid, 0x12345678_1234_1234_1234_123456789ABC);
    }

    #[test]
    fn parses_a_braceless_lowercase_clsid() {
        let row = parse_driver_row("ASIO4ALL v2", "abcdef01-2345-6789-abcd-ef0123456789").unwrap();
        assert_eq!(row.clsid, 0xABCDEF01_2345_6789_ABCD_EF0123456789);
    }

    #[test]
    fn rejects_malformed_clsid() {
        // Too few groups.
        assert!(parse_driver_row("x", "{12345678-1234}").is_none());
        // Wrong group length.
        assert!(parse_driver_row("x", "{1234567-1234-1234-1234-123456789ABC}").is_none());
        // Non-hex digit.
        assert!(parse_driver_row("x", "{1234567G-1234-1234-1234-123456789ABC}").is_none());
        // Empty.
        assert!(parse_driver_row("x", "").is_none());
    }

    #[test]
    fn rejects_empty_name() {
        assert!(parse_driver_row("   ", "{12345678-1234-1234-1234-123456789ABC}").is_none());
    }
}
