// Volatile session settings (not persisted, reset on reboot)
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use serde::{Deserialize, Serialize};

use crate::net::ws_client::client_proto::DeviceUpdate;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSettings {
    pub light_on: bool,
    pub light_on_override: Option<bool>, // If set, overrides timer and other logic to force light on or off
    pub night_timer_active: bool, // Whether the day-night timer is currently driving the light off (night mode)
    pub is_time_synced: bool,     // Whether the device has successfully synced time with the server
    pub unix_epoch_offset_secs: u32, // Unix time minus system uptime in seconds
    pub local_time_of_day_offset_secs: u32, // Seconds since last local midnight minus system uptime in seconds
    pub local_weekday_number: u32,          // Local weekday number, 0 = Sunday
    pub local_sunrise_time_of_day_seconds: u32, // Sunrise time in seconds since local midnight
    pub local_sunset_time_of_day_seconds: u32, // Sunset time in seconds since local midnight
    pub updating_firmware: bool,
    pub update_progress_percent: u8,
    pub update_speed_bytes_per_sec: u32,
    pub update_has_failed: bool,
    pub test_mode_active: bool,
    pub is_rolled_back_firmware: bool,
    pub is_factory_firmware: bool,
    pub firmware_update_available: Option<DeviceUpdate>,
    pub auto_brightness_percent: Option<u8>, // Calculated brightness percent based on sun path automation
    pub auto_update_scheduled_unix_timestamp: Option<u32>, // Unix timestamp of next scheduled auto firmware update
}

static SETTINGS: Mutex<CriticalSectionRawMutex, SessionSettings> = Mutex::new(SessionSettings {
    light_on: true,
    light_on_override: None,
    night_timer_active: false,
    is_time_synced: false,
    unix_epoch_offset_secs: 0,
    local_time_of_day_offset_secs: 0,
    local_weekday_number: 0,
    local_sunrise_time_of_day_seconds: 0,
    local_sunset_time_of_day_seconds: 0,
    updating_firmware: false,
    update_progress_percent: 0,
    update_speed_bytes_per_sec: 0,
    update_has_failed: false,
    test_mode_active: false,
    is_rolled_back_firmware: false,
    is_factory_firmware: false,
    firmware_update_available: None,
    auto_brightness_percent: None,
    auto_update_scheduled_unix_timestamp: None,
});

pub async fn get_settings() -> SessionSettings {
    SETTINGS.lock().await.clone()
}

pub async fn update_settings<F>(f: F)
where
    F: FnOnce(&mut SessionSettings),
{
    _ = update_settings_changed(f).await;
}

/// Updates the settings, and whether that changed them.
pub async fn update_settings_changed<F>(f: F) -> bool
where
    F: FnOnce(&mut SessionSettings),
{
    let mut guard = SETTINGS.lock().await;
    let old = guard.clone();
    f(&mut guard);
    old != *guard
}
