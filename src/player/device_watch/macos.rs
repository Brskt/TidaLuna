use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};

use super::Signal;

/// The system's device list: it changes when a device is added or removed.
const DEVICES: AudioObjectPropertyAddress = AudioObjectPropertyAddress {
    mSelector: kAudioHardwarePropertyDevices,
    mScope: kAudioObjectPropertyScopeGlobal,
    mElement: kAudioObjectPropertyElementMain,
};

/// The listener is removed when this drops, before the signal it points at.
pub(crate) struct Watch {
    signal: Box<Signal>,
}

pub(crate) fn start(signal: Signal) -> Result<Watch, String> {
    let signal = Box::new(signal);
    // SAFETY: the boxed signal outlives the registration: `Drop` removes it first.
    let status = unsafe {
        AudioObjectAddPropertyListener(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&DEVICES),
            Some(on_devices),
            client_data(&signal),
        )
    };
    if status != 0 {
        return Err(format!("CoreAudio listener: OSStatus {status}"));
    }
    Ok(Watch { signal })
}

fn client_data(signal: &Signal) -> *mut c_void {
    std::ptr::from_ref(signal).cast_mut().cast()
}

unsafe extern "C-unwind" fn on_devices(
    _: AudioObjectID,
    _: u32,
    _: NonNull<AudioObjectPropertyAddress>,
    data: *mut c_void,
) -> i32 {
    // SAFETY: `data` is the `Signal` the registration boxed, alive until it is removed.
    let signal = unsafe { &*data.cast::<Signal>() };
    signal();
    0
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: the same address, listener and client data as the registration.
        let _ = unsafe {
            AudioObjectRemovePropertyListener(
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&DEVICES),
                Some(on_devices),
                client_data(&self.signal),
            )
        };
    }
}
