use std::sync::mpsc;

use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::core::*;

// PKEY_Device_FriendlyName: {a45c254e-df1c-4efd-8020-67d146a850e0}, pid 14
const PKEY_DEVICE_FRIENDLY_NAME: windows::Win32::Foundation::PROPERTYKEY =
    windows::Win32::Foundation::PROPERTYKEY {
        fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };

const TIDALUNAR_VOLUME_GUID: GUID = GUID::from_u128(0x5449_4441_4c55_4e41_5256_4f4c_5359_4e43);

pub struct ComGuard {
    _needs_uninit: bool,
}

impl ComGuard {
    pub fn new() -> Result<Self> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_ok() {
                Ok(Self {
                    _needs_uninit: true,
                })
            } else if hr == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
                crate::vprintln!("[COM]    RPC_E_CHANGED_MODE, using existing apartment model");
                Ok(Self {
                    _needs_uninit: false,
                })
            } else {
                let e = Error::from(hr);
                crate::vprintln!("[COM]    CoInitializeEx failed: {e}");
                Err(e)
            }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self._needs_uninit {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

#[implement(IAudioSessionEvents)]
struct VolumeCallback {
    tx: mpsc::Sender<f64>,
}

impl IAudioSessionEvents_Impl for VolumeCallback_Impl {
    fn OnSimpleVolumeChanged(
        &self,
        newvolume: f32,
        _newmute: windows_core::BOOL,
        eventcontext: *const GUID,
    ) -> Result<()> {
        if !eventcontext.is_null() {
            let ctx = unsafe { *eventcontext };
            if ctx == TIDALUNAR_VOLUME_GUID {
                return Ok(());
            }
        }
        let level = (newvolume * 100.0) as f64;
        let _ = self.tx.send(level);
        Ok(())
    }

    fn OnDisplayNameChanged(
        &self,
        _newdisplayname: &PCWSTR,
        _eventcontext: *const GUID,
    ) -> Result<()> {
        Ok(())
    }

    fn OnIconPathChanged(&self, _newiconpath: &PCWSTR, _eventcontext: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnChannelVolumeChanged(
        &self,
        _channelcount: u32,
        _newchannelvolumearray: *const f32,
        _changedchannel: u32,
        _eventcontext: *const GUID,
    ) -> Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(
        &self,
        _newgroupingparam: *const GUID,
        _eventcontext: *const GUID,
    ) -> Result<()> {
        Ok(())
    }

    fn OnStateChanged(&self, _newstate: AudioSessionState) -> Result<()> {
        Ok(())
    }

    fn OnSessionDisconnected(&self, _disconnectreason: AudioSessionDisconnectReason) -> Result<()> {
        Ok(())
    }
}

pub struct VolumeSync {
    simple_volume: ISimpleAudioVolume,
    session_ctl: IAudioSessionControl,
    _callback: IAudioSessionEvents,
}

impl VolumeSync {
    pub fn new(device_id: &str, tx: mpsc::Sender<f64>) -> Result<Self> {
        unsafe {
            let dev_enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

            let device = if device_id == "default" {
                dev_enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?
            } else {
                find_device_by_name(&dev_enumerator, device_id)?
            };

            // Get the default process-specific session (GUID_NULL, StreamFlags=0).
            // This matches standard WASAPI behavior and should match cpal's default session.
            let session_mgr: IAudioSessionManager = device.Activate(CLSCTX_ALL, None)?;
            let session_ctl = session_mgr.GetAudioSessionControl(None, 0)?;

            let simple_volume: ISimpleAudioVolume = session_ctl.cast()?;

            let callback: IAudioSessionEvents = VolumeCallback { tx }.into();
            session_ctl.RegisterAudioSessionNotification(&callback)?;

            Ok(Self {
                simple_volume,
                session_ctl,
                _callback: callback,
            })
        }
    }

    pub fn set(&self, level: f32) -> Result<()> {
        unsafe {
            self.simple_volume
                .SetMasterVolume(level, &TIDALUNAR_VOLUME_GUID)
        }
    }

    pub fn get(&self) -> Result<f32> {
        unsafe { self.simple_volume.GetMasterVolume() }
    }
}

impl Drop for VolumeSync {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .session_ctl
                .UnregisterAudioSessionNotification(&self._callback);
        }
    }
}

unsafe fn find_device_by_name(enumerator: &IMMDeviceEnumerator, name: &str) -> Result<IMMDevice> {
    let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)? };
    let count = unsafe { collection.GetCount()? };

    let mut found: Option<IMMDevice> = None;
    let mut duplicates = 0u32;

    for i in 0..count {
        let device = unsafe { collection.Item(i)? };
        if let Ok(friendly) = unsafe { get_device_friendly_name(&device) } {
            if friendly == name {
                if found.is_none() {
                    found = Some(device);
                } else {
                    duplicates += 1;
                }
            }
        }
    }

    if duplicates > 0 {
        crate::vprintln!(
            "[VOLUME] Warning: {} devices share the name \"{}\", using the first match",
            duplicates + 1,
            name
        );
    }

    found.ok_or_else(|| {
        windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            &format!("No audio device found with name \"{}\"", name),
        )
    })
}

unsafe fn get_device_friendly_name(device: &IMMDevice) -> Result<String> {
    let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM_READ)? };
    let prop: PROPVARIANT = unsafe { store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME)? };
    Ok(prop.to_string())
}
