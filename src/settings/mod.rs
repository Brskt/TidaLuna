mod catalog;
mod store;
mod swap;

pub(crate) use catalog::{
    BootSettings, WindowState, load_boot_settings, load_output_level, load_plugin_update_at_boot,
    load_update_auto_check, load_update_channel, load_update_skip_version, load_window_state,
    save_audio_cache_enabled, save_audio_cache_gb, save_close_to_tray, save_console,
    save_crossfade_enabled, save_crossfade_secs, save_log_level, save_maximized, save_output_level,
    save_plugin_update_at_boot, save_receiver_always_on, save_update_auto_check,
    save_update_channel, save_update_skip_version, save_video_codec_enabled, save_window_state,
};
#[cfg(target_os = "windows")]
pub(crate) use catalog::{load_volume_sync, save_asio, save_volume_sync};
#[cfg(test)]
pub(crate) use store::init_schema;
pub(crate) use store::{RepairNotice, acknowledge_repair_notice, corruption, open, pending_notice};
pub(crate) use swap::{aside_name, replace_naming};

#[cfg(test)]
#[path = "../../tests/unit/settings/fixture.rs"]
pub(crate) mod fixture;
