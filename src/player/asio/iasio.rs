//! Hand-declared `IASIO` COM interface and ASIO ABI structs (Windows only).
//!
//! Talks to the installed ASIO driver through COM without the Steinberg SDK: the
//! interface and structs are declared from the documented ABI (cross-checked against
//! the clean-room cwASIO/go-asio/NAudio), keeping the project clear of the SDK license.
//!
//! ABI notes:
//! - ASIO scalars are C `long` (`i32`) except sample rate (`f64`) and the 64-bit
//!   `{hi, lo}` pairs.
//! - Vtable slots and callbacks use `extern "system"` (x64 unifies stdcall/thiscall).
//! - A driver uses its CLSID as the IID: `CoCreateInstance` passes it as both.
//!
//! FFI/ABI plumbing: some vtable slots and fields exist only for layout.
#![allow(dead_code)]

use core::ffi::{c_char, c_void};

use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows_sys::core::GUID;

// --- ASIO scalar typedefs ---------------------------------------------------

/// `ASIOBool`: a C `long`, `ASIOTrue` = 1, `ASIOFalse` = 0.
pub(crate) type AsioBool = i32;
/// `ASIOError`: a C `long`.
pub(crate) type AsioError = i32;
/// `ASIOSampleRate`: an IEEE-754 double.
pub(crate) type AsioSampleRate = f64;
/// `ASIOSampleType`: a C `long` (see the `ASIO_ST_*` discriminants).
pub(crate) type AsioSampleType = i32;

pub(crate) const ASE_OK: AsioError = 0;

// The LSB integer/float discriminants we support (the full enum is larger).
pub(crate) const ASIO_ST_INT16_LSB: AsioSampleType = 16;
pub(crate) const ASIO_ST_INT24_LSB: AsioSampleType = 17;
pub(crate) const ASIO_ST_INT32_LSB: AsioSampleType = 18;
pub(crate) const ASIO_ST_FLOAT32_LSB: AsioSampleType = 19;

/// True if an `ASIOError` indicates success.
pub(crate) fn asio_ok(err: AsioError) -> bool {
    err == ASE_OK
}

// --- 64-bit split types (Windows uses {hi, lo}, not a native i64) -----------

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct AsioSamples {
    pub hi: u32,
    pub lo: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct AsioTimeStamp {
    pub hi: u32,
    pub lo: u32,
}

impl AsioSamples {
    /// Recombine the `{hi, lo}` split into a single 64-bit frame count.
    pub(crate) fn to_u64(self) -> u64 {
        ((self.hi as u64) << 32) | self.lo as u64
    }
}

// --- ASIO structs (exact `#[repr(C)]` layouts) ------------------------------

#[repr(C)]
pub(crate) struct AsioClockSource {
    pub index: i32,
    pub associated_channel: i32,
    pub associated_group: i32,
    pub is_current_source: AsioBool,
    pub name: [c_char; 32],
}

#[repr(C)]
pub(crate) struct AsioChannelInfo {
    /// In: the channel index to query.
    pub channel: i32,
    /// In: `ASIOTrue` for an input channel.
    pub is_input: AsioBool,
    /// Out: `ASIOTrue` if the channel is active.
    pub is_active: AsioBool,
    /// Out: the channel group.
    pub channel_group: i32,
    /// Out: the driver's sample type (`ASIO_ST_*`).
    pub sample_type: AsioSampleType,
    /// Out: the channel name.
    pub name: [c_char; 32],
}

#[repr(C)]
pub(crate) struct AsioBufferInfo {
    /// In: `ASIOTrue` for an input channel.
    pub is_input: AsioBool,
    /// In: the channel index.
    pub channel_num: i32,
    /// Out: the two (ping/pong) buffer addresses filled by `createBuffers`.
    pub buffers: [*mut c_void; 2],
}

/// The host callbacks the driver invokes. These are plain C function pointers
/// (no `this`), filled in by the host before `createBuffers`.
#[repr(C)]
pub(crate) struct AsioCallbacks {
    pub buffer_switch:
        unsafe extern "system" fn(double_buffer_index: i32, direct_process: AsioBool),
    pub sample_rate_did_change: unsafe extern "system" fn(s_rate: AsioSampleRate),
    pub asio_message: unsafe extern "system" fn(
        selector: i32,
        value: i32,
        message: *mut c_void,
        opt: *mut f64,
    ) -> i32,
    pub buffer_switch_time_info: unsafe extern "system" fn(
        params: *mut AsioTime,
        double_buffer_index: i32,
        direct_process: AsioBool,
    ) -> *mut AsioTime,
}

#[repr(C)]
pub(crate) struct AsioTimeInfo {
    pub speed: f64,
    pub system_time: AsioTimeStamp,
    pub sample_position: AsioSamples,
    pub sample_rate: AsioSampleRate,
    pub flags: u32,
    pub reserved: [c_char; 12],
}

#[repr(C)]
pub(crate) struct AsioTimeCode {
    pub speed: f64,
    pub time_code_samples: AsioSamples,
    pub flags: u32,
    pub future: [c_char; 64],
}

#[repr(C)]
pub(crate) struct AsioTime {
    pub reserved: [i32; 4],
    pub time_info: AsioTimeInfo,
    pub time_code: AsioTimeCode,
}

// --- the COM vtable ---------------------------------------------------------

/// The `IUnknown` base, declared here to keep the whole vtable in terms of the
/// windows-sys `GUID` (no cross-crate GUID mixing).
#[repr(C)]
pub(crate) struct IUnknownVtbl {
    pub query_interface: unsafe extern "system" fn(
        this: *mut c_void,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> i32,
    pub add_ref: unsafe extern "system" fn(this: *mut c_void) -> u32,
    pub release: unsafe extern "system" fn(this: *mut c_void) -> u32,
}

/// The `IASIO` vtable: `IUnknown` (3 slots) followed by the 21 ASIO methods in
/// their canonical order. The order and signatures are load-bearing.
#[repr(C)]
pub(crate) struct IAsioVtbl {
    pub base: IUnknownVtbl,
    pub init: unsafe extern "system" fn(this: *mut c_void, sys_handle: *mut c_void) -> AsioBool,
    pub get_driver_name: unsafe extern "system" fn(this: *mut c_void, name: *mut c_char),
    pub get_driver_version: unsafe extern "system" fn(this: *mut c_void) -> i32,
    pub get_error_message: unsafe extern "system" fn(this: *mut c_void, string: *mut c_char),
    pub start: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
    pub stop: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
    pub get_channels: unsafe extern "system" fn(
        this: *mut c_void,
        num_in: *mut i32,
        num_out: *mut i32,
    ) -> AsioError,
    pub get_latencies: unsafe extern "system" fn(
        this: *mut c_void,
        in_lat: *mut i32,
        out_lat: *mut i32,
    ) -> AsioError,
    pub get_buffer_size: unsafe extern "system" fn(
        this: *mut c_void,
        min: *mut i32,
        max: *mut i32,
        pref: *mut i32,
        gran: *mut i32,
    ) -> AsioError,
    pub can_sample_rate:
        unsafe extern "system" fn(this: *mut c_void, rate: AsioSampleRate) -> AsioError,
    pub get_sample_rate:
        unsafe extern "system" fn(this: *mut c_void, rate: *mut AsioSampleRate) -> AsioError,
    pub set_sample_rate:
        unsafe extern "system" fn(this: *mut c_void, rate: AsioSampleRate) -> AsioError,
    pub get_clock_sources: unsafe extern "system" fn(
        this: *mut c_void,
        clocks: *mut AsioClockSource,
        num: *mut i32,
    ) -> AsioError,
    pub set_clock_source: unsafe extern "system" fn(this: *mut c_void, reference: i32) -> AsioError,
    pub get_sample_position: unsafe extern "system" fn(
        this: *mut c_void,
        s_pos: *mut AsioSamples,
        ts: *mut AsioTimeStamp,
    ) -> AsioError,
    pub get_channel_info:
        unsafe extern "system" fn(this: *mut c_void, info: *mut AsioChannelInfo) -> AsioError,
    pub create_buffers: unsafe extern "system" fn(
        this: *mut c_void,
        infos: *mut AsioBufferInfo,
        num_ch: i32,
        buf_size: i32,
        cb: *const AsioCallbacks,
    ) -> AsioError,
    pub dispose_buffers: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
    pub control_panel: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
    pub future:
        unsafe extern "system" fn(this: *mut c_void, selector: i32, opt: *mut c_void) -> AsioError,
    pub output_ready: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
}

// --- the driver handle ------------------------------------------------------

/// An owned `IASIO` COM object. `Drop` releases the single reference
/// `CoCreateInstance` handed us. ASIO loads one driver globally at a time, so
/// only one of these is alive at once.
#[repr(transparent)]
pub(crate) struct AsioDriver(*mut c_void);

impl AsioDriver {
    #[inline]
    fn vtbl(&self) -> &IAsioVtbl {
        // The COM pointer points at a `*const IAsioVtbl`.
        unsafe { &**(self.0 as *mut *const IAsioVtbl) }
    }

    /// Create a driver from its registry CLSID. The CLSID is passed as both the
    /// class id and the interface id (the ASIO quirk). Returns the failing
    /// `HRESULT` on error.
    pub(crate) unsafe fn create(clsid: u128) -> Result<AsioDriver, i32> {
        let guid = guid_from_u128(clsid);
        // STA. S_OK or S_FALSE (already initialized on this thread) are both fine.
        let hr = unsafe { CoInitializeEx(core::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        if hr < 0 {
            return Err(hr);
        }
        let mut ptr: *mut c_void = core::ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &guid,
                core::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &guid,
                &mut ptr,
            )
        };
        if hr < 0 {
            return Err(hr);
        }
        if ptr.is_null() {
            return Err(i32::MIN); // E_POINTER-ish: succeeded but no object
        }
        Ok(AsioDriver(ptr))
    }

    pub(crate) unsafe fn init(&self, sys_handle: *mut c_void) -> AsioBool {
        unsafe { (self.vtbl().init)(self.0, sys_handle) }
    }

    pub(crate) unsafe fn get_driver_name(&self, name: *mut c_char) {
        unsafe { (self.vtbl().get_driver_name)(self.0, name) }
    }

    pub(crate) unsafe fn get_error_message(&self, string: *mut c_char) {
        unsafe { (self.vtbl().get_error_message)(self.0, string) }
    }

    pub(crate) unsafe fn start(&self) -> AsioError {
        unsafe { (self.vtbl().start)(self.0) }
    }

    pub(crate) unsafe fn stop(&self) -> AsioError {
        unsafe { (self.vtbl().stop)(self.0) }
    }

    pub(crate) unsafe fn get_channels(&self, num_in: *mut i32, num_out: *mut i32) -> AsioError {
        unsafe { (self.vtbl().get_channels)(self.0, num_in, num_out) }
    }

    pub(crate) unsafe fn get_buffer_size(
        &self,
        min: *mut i32,
        max: *mut i32,
        pref: *mut i32,
        gran: *mut i32,
    ) -> AsioError {
        unsafe { (self.vtbl().get_buffer_size)(self.0, min, max, pref, gran) }
    }

    pub(crate) unsafe fn can_sample_rate(&self, rate: AsioSampleRate) -> AsioError {
        unsafe { (self.vtbl().can_sample_rate)(self.0, rate) }
    }

    pub(crate) unsafe fn get_sample_rate(&self, rate: *mut AsioSampleRate) -> AsioError {
        unsafe { (self.vtbl().get_sample_rate)(self.0, rate) }
    }

    pub(crate) unsafe fn set_sample_rate(&self, rate: AsioSampleRate) -> AsioError {
        unsafe { (self.vtbl().set_sample_rate)(self.0, rate) }
    }

    pub(crate) unsafe fn get_sample_position(
        &self,
        s_pos: *mut AsioSamples,
        ts: *mut AsioTimeStamp,
    ) -> AsioError {
        unsafe { (self.vtbl().get_sample_position)(self.0, s_pos, ts) }
    }

    pub(crate) unsafe fn get_channel_info(&self, info: *mut AsioChannelInfo) -> AsioError {
        unsafe { (self.vtbl().get_channel_info)(self.0, info) }
    }

    pub(crate) unsafe fn create_buffers(
        &self,
        infos: *mut AsioBufferInfo,
        num_ch: i32,
        buf_size: i32,
        cb: *const AsioCallbacks,
    ) -> AsioError {
        unsafe { (self.vtbl().create_buffers)(self.0, infos, num_ch, buf_size, cb) }
    }

    pub(crate) unsafe fn dispose_buffers(&self) -> AsioError {
        unsafe { (self.vtbl().dispose_buffers)(self.0) }
    }

    pub(crate) unsafe fn control_panel(&self) -> AsioError {
        unsafe { (self.vtbl().control_panel)(self.0) }
    }

    pub(crate) unsafe fn output_ready(&self) -> AsioError {
        unsafe { (self.vtbl().output_ready)(self.0) }
    }

    /// The raw `IASIO` COM pointer; the real-time callback (which holds no
    /// `AsioDriver`) can signal `outputReady` via [`output_ready_raw`].
    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for AsioDriver {
    fn drop(&mut self) {
        unsafe { (self.vtbl().base.release)(self.0) };
    }
}

/// Call `IASIO::outputReady` from the real-time callback, where only the raw COM
/// pointer is available. `outputReady` is the one driver call the ASIO spec allows
/// from within `bufferSwitch`; its absence merely costs one buffer of latency, but
/// some KS-backed drivers need it to begin emitting.
///
/// # Safety
/// `this` must be a live `IASIO` COM pointer (from [`AsioDriver::as_ptr`]) that
/// outlives the call.
pub(crate) unsafe fn output_ready_raw(this: *mut c_void) -> AsioError {
    // SAFETY: `this` points at a live IASIO whose layout matches `IAsioVtbl`; the
    // caller guarantees the pointer is valid for the duration of this call.
    let vtbl = unsafe { &**(this as *mut *const IAsioVtbl) };
    unsafe { (vtbl.output_ready)(this) }
}

/// Build a windows-sys `GUID` from a `u128` in the textual GUID order
/// (matching `GUID::from_u128`).
fn guid_from_u128(v: u128) -> GUID {
    GUID {
        data1: (v >> 96) as u32,
        data2: (v >> 80) as u16,
        data3: (v >> 64) as u16,
        data4: [
            (v >> 56) as u8,
            (v >> 48) as u8,
            (v >> 40) as u8,
            (v >> 32) as u8,
            (v >> 24) as u8,
            (v >> 16) as u8,
            (v >> 8) as u8,
            v as u8,
        ],
    }
}
