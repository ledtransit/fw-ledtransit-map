// Persistent application settings (stored in flash)
use alloc::vec;
use defmt::{debug, info, warn};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_hal::rom::crc::crc32_le;
use esp_storage::FlashStorage;
use serde::{Deserialize, Serialize};

use super::STORE_SETTINGS_SIGNAL;
use crate::{
    config::CONFIG,
    net::ws_client::client_proto::{
        ColorMode, DeviceConfig, DisruptionFilter, DisruptionInterval, DisruptionMode,
        RealtimeFilter, RenderMode, SunlightAutoBrightness, TimerSettings, VehicleFilter,
    },
    store::with_data_partition,
    trace,
    util::pack_rgb8,
};

const SETTINGS_PARTITION: &str = "settings";
const SETTINGS_MAGIC: u32 = 0xA562E1B;
// 2: the access token replaced by `claimed` (devices authenticate with
// their device keys)
const SETTINGS_VERSION: u32 = 2;
const SETTINGS_MAX_BYTE_SIZE: usize = 1024;

// Environment overrides
const WIFI_SSID: Option<&str> = option_env!("WIFI_SSID");
const WIFI_PASSWORD: Option<&str> = option_env!("WIFI_PASSWORD");
const PROV_TOKEN: Option<&str> = option_env!("PROV_TOKEN");

#[derive(Serialize, Deserialize, Clone)]
pub struct PersistSettings {
    pub magic: u32,
    pub version: u32,
    pub wifi_ssid: Option<heapless::String<32>>,
    pub wifi_password: Option<heapless::String<64>>,
    pub prov_token: Option<heapless::String<64>>, // Short-lived provisioning used for initial WiFi setup identification with the API
    pub claimed: bool, // Claimed into its user's account by the gateway: connects with its device keys from then on
    pub config: DeviceConfig, // Device configuration settings synced with server user settings
}

impl Default for PersistSettings {
    fn default() -> Self {
        persist_settings_default()
    }
}

impl PersistSettings {
    fn crc32(&self) -> u32 {
        let serialized = postcard::to_vec::<_, SETTINGS_MAX_BYTE_SIZE>(self)
            .expect("Failed to serialize settings");
        crc32_le(0xffffffff, &serialized)
    }

    pub fn clear_wifi_credentials_and_auth(&mut self) {
        self.wifi_ssid = None;
        self.wifi_password = None;
        self.prov_token = None;
        self.claimed = false;
    }

    pub fn has_credentials_and_is_claimed(&self) -> bool {
        self.wifi_ssid.is_some() && self.wifi_password.is_some() && self.claimed
    }
}

const fn persist_settings_default() -> PersistSettings {
    PersistSettings {
        magic: SETTINGS_MAGIC,
        version: SETTINGS_VERSION,
        wifi_ssid: None,
        wifi_password: None,
        prov_token: None,
        claimed: false,
        config: DeviceConfig {
            brightness_percent: 25,
            current_limit_ma: 1000,
            color_mode: ColorMode::Original as i32,
            primary_color_rgb8: pack_rgb8(31, 255, 102),
            secondary_color_rgb8: pack_rgb8(255, 56, 20),
            tertiary_color_rgb8: pack_rgb8(107, 255, 179),
            color_temperature_shift: 0,
            disruption_mode: DisruptionMode::Ripple as i32,
            disruption_primary_color_rgb8: pack_rgb8(255, 0, 76),
            disruption_secondary_color_rgb8: pack_rgb8(255, 170, 0),
            disruption_brightness_percent: 80,
            disruption_interval: DisruptionInterval::Every5s as i32,
            animation_speed_percent: 100,
            render_mode: RenderMode::SnapClosestTransition as i32,
            vehicle_filter: VehicleFilter::All as i32,
            vehicle_distance_threshold_meters: 500,
            timer_settings: TimerSettings {
                enabled: false,
                start_time_of_day_seconds: 10 * 60 * 60, // 10:00 AM
                end_time_of_day_seconds: 18 * 60 * 60,   // 6:00 PM
                weekdays_bitmask: 0b01111111,            // Su-Sa
            },
            auto_firmware_update_enabled: false,
            min_delay_minutes: CONFIG.cfg.min_delay_minutes,
            max_delay_minutes: CONFIG.cfg.max_delay_minutes,
            min_speed_kmph: CONFIG.cfg.min_speed_kmph,
            max_speed_kmph: CONFIG.cfg.max_speed_kmph,
            line_configs: vec![],
            timezone_iana: None,
            realtime_filter: RealtimeFilter::ScheduledAndRealtime as i32,
            sunlight_auto_brightness: SunlightAutoBrightness {
                enabled: false,
                night_brightness_percent: 20,
                day_brightness_percent: 45,
            },
            location_coord: None,
            location_name: None,
            disruption_filter: DisruptionFilter::Severe as i32,
            disruptions_enabled: false,
        },
    }
}

static SETTINGS: Mutex<CriticalSectionRawMutex, PersistSettings> =
    Mutex::new(persist_settings_default());

pub async fn init(flash_store: &mut FlashStorage<'_>) {
    let mut buf = [0u8; SETTINGS_MAX_BYTE_SIZE];
    with_data_partition(flash_store, SETTINGS_PARTITION, |storage| {
        storage
            .read(0, &mut buf)
            .expect("Failed to read settings from storage")
    });
    let mut settings: PersistSettings = postcard::from_bytes(&buf).unwrap_or_default();

    if settings.magic != SETTINGS_MAGIC {
        trace::wrn!(
            "Invalid settings magic (expected 0x{:X}, got 0x{:X}), resetting to defaults",
            SETTINGS_MAGIC,
            settings.magic
        );
        settings = PersistSettings::default();
    }

    if settings.version != SETTINGS_VERSION && migrate_settings(&mut settings).is_err() {
        trace::wrn!(
            "Failed to migrate settings from version {} to {}, resetting to defaults",
            settings.version,
            SETTINGS_VERSION
        );
        settings = PersistSettings::default();
    }

    apply_env_overrides(&mut settings);

    info!("Settings loaded from flash (v{})", settings.version);
    *SETTINGS.lock().await = settings;
}

fn migrate_settings(_old: &mut PersistSettings) -> Result<(), ()> {
    // No migrations yet
    Ok(())
}

fn apply_env_overrides(settings: &mut PersistSettings) {
    if let Some(ssid) = WIFI_SSID {
        settings.wifi_ssid = heapless::String::try_from(ssid).ok();
        warn!("Overriding WiFi SSID from environment variable");
    }
    if let Some(password) = WIFI_PASSWORD {
        settings.wifi_password = heapless::String::try_from(password).ok();
        warn!("Overriding WiFi password from environment variable");
    }
    if let Some(token) = PROV_TOKEN {
        settings.prov_token = heapless::String::try_from(token).ok();
        warn!("Overriding provisioning token from environment variable");
    }
}

pub async fn get_settings() -> PersistSettings {
    SETTINGS.lock().await.clone()
}

pub async fn update_settings<F>(f: F)
where
    F: FnOnce(&mut PersistSettings),
{
    _ = update_settings_changed(f).await;
}

/// Updates the settings, and whether that changed them. Only changes are
/// written to flash, to save write cycles.
pub async fn update_settings_changed<F>(f: F) -> bool
where
    F: FnOnce(&mut PersistSettings),
{
    let changed = {
        let mut guard = SETTINGS.lock().await;
        let old_crc = guard.crc32();
        f(&mut guard);
        old_crc != guard.crc32()
    };
    if changed {
        STORE_SETTINGS_SIGNAL.signal(());
    }
    changed
}

pub(super) async fn store_settings(flash_store: &mut FlashStorage<'_>) {
    let settings = SETTINGS.lock().await;
    let serialized = postcard::to_vec::<_, SETTINGS_MAX_BYTE_SIZE>(&*settings)
        .expect("Failed to serialize settings");
    with_data_partition(flash_store, SETTINGS_PARTITION, |storage| {
        storage
            .write(0, &serialized)
            .expect("Failed to write settings to storage")
    });
    debug!("Settings stored to flash (v{})", settings.version);
}
