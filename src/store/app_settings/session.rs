// Volatile session settings (not persisted, reset on reboot)
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use serde::{Deserialize, Serialize};

use crate::net::ws_client::client_proto::{DeviceCommand, DeviceUpdate};

/// A command from the server that runs once a button on the map is pressed
/// before the deadline
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingConfirmation {
    pub command: DeviceCommand,
    pub deadline_unix_timestamp: u32,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSettings {
    pub light_on: bool,
    pub light_on_override: Option<bool>, // If set, overrides timer and other logic to force light on or off
    pub night_timer_active: bool, // Whether the day-night timer is currently driving the light off (night mode)
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
    pub pending_confirmation: Option<PendingConfirmation>, // Server command awaiting a button press on the map
    pub display_frames_per_second: f32, // Measured LED updates per second, averaged over the last frames
}

static SETTINGS: Mutex<CriticalSectionRawMutex, SessionSettings> = Mutex::new(SessionSettings {
    light_on: true,
    light_on_override: None,
    night_timer_active: false,
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
    pending_confirmation: None,
    display_frames_per_second: 0.0,
});

pub async fn get_settings() -> SessionSettings {
    SETTINGS.lock().await.clone()
}

/// A value from the settings, without copying them all (for frequent reads).
pub async fn read_settings<R>(f: impl FnOnce(&SessionSettings) -> R) -> R {
    f(&*SETTINGS.lock().await)
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
