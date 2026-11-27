use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::{PCWSTR, implement};

use super::Signal;

/// The official engine's policy, read from its binary: a device added, removed or made active
/// re-lists; a disable, a default change and a property change list nothing.
#[implement(IMMNotificationClient)]
struct EndpointWatcher {
    signal: Signal,
}

impl IMMNotificationClient_Impl for EndpointWatcher_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, state: DEVICE_STATE) -> windows::core::Result<()> {
        if state == DEVICE_STATE_ACTIVE {
            (self.signal)();
        }
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.signal)();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.signal)();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        _flow: EDataFlow,
        _role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Watching stops, and the callback is unregistered, when this drops.
pub(crate) struct Watch {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// Registers from a thread of its own, in its own apartment, which also unregisters it.
pub(crate) fn start(signal: Signal) -> Result<Watch, String> {
    let (stop, stopped) = mpsc::channel::<()>();
    let (ready_tx, ready) = mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("device-watch".into())
        .spawn(move || {
            let _com = match crate::platform::volume_sync::ComGuard::new() {
                Ok(guard) => guard,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("COM: {e}")));
                    return;
                }
            };
            let client: IMMNotificationClient = EndpointWatcher { signal }.into();
            // SAFETY: COM stays initialized on this thread for the enumerator's whole life.
            let registered = unsafe {
                CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .and_then(|e| e.RegisterEndpointNotificationCallback(&client).map(|()| e))
            };
            match registered {
                Ok(enumerator) => {
                    let _ = ready_tx.send(Ok(()));
                    // Returns once the `Watch` drops its sender.
                    let _ = stopped.recv();
                    // SAFETY: same thread and apartment as the registration.
                    let _ = unsafe { enumerator.UnregisterEndpointNotificationCallback(&client) };
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("endpoint notifications: {e}")));
                }
            }
        })
        .map_err(|e| format!("thread: {e}"))?;
    match ready.recv() {
        Ok(Ok(())) => Ok(Watch {
            stop: Some(stop),
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err("the watch thread ended before registering".into())
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
