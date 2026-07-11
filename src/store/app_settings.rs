// Application settings
//   Persisted settings are stored in flash and survive reboots e.g. WiFi credentials, brightness level, color mode, etc.
//   Session settings are stored in RAM and reset on reboot, e.g. server time sync, light on status etc.
//   Device info is stored in separate flash partition, e.g. product ID and firmware version, used for capability detection via probe
use crate::{
    config::CONFIG,
    net::ws_client::client_proto::{
        ColorMode, DeviceConfig, DeviceUpdate, DisruptionFilter, DisruptionInterval,
        DisruptionMode, RealtimeFilter, RenderMode, SunlightAutoBrightness, TimerSettings,
        VehicleFilter,
    },
    store::SharedFlashStorage,
    trace,
    util::pack_rgb8,
};
use alloc::vec;
use core::ops::Deref;
use defmt::debug;
use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, signal::Signal};
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::partitions::{self, PARTITION_TABLE_MAX_LEN};
use esp_hal::rom::crc::crc32_le;
use esp_storage::FlashStorage;
use serde::{Deserialize, Serialize};

enum AppSettingsEvent {
    StorePersistSettings,
    StoreDeviceInfo,
}

static SETTINGS_SIGNAL: Signal<CriticalSectionRawMutex, AppSettingsEvent> = Signal::new();

pub fn spawn(spawner: Spawner, flash_store: &'static SharedFlashStorage) {
    spawner.spawn(app_settings_task(flash_store).unwrap());
}

#[embassy_executor::task]
async fn app_settings_task(flash_store: &'static SharedFlashStorage) {
    loop {
        match SETTINGS_SIGNAL.wait().await {
            AppSettingsEvent::StorePersistSettings => {
                let mut flash_store = flash_store.lock().await;
                persist::store_settings(&mut flash_store).await;
            }
            AppSettingsEvent::StoreDeviceInfo => {
                let mut flash_store = flash_store.lock().await;
                info::write_to_flash(&mut flash_store).await;
            }
        }
    }
}

// Persistent application settings (stored in flash)
pub mod persist {
    use super::*;

    const SETTINGS_MAGIC: u32 = 0xA562E1B;
    const SETTINGS_VERSION: u32 = 1;
    const SETTINGS_MAX_BYTE_SIZE: usize = 1024;

    // Environment overrides
    const WIFI_SSID: Option<&str> = option_env!("WIFI_SSID");
    const WIFI_PASSWORD: Option<&str> = option_env!("WIFI_PASSWORD");
    const PROV_TOKEN: Option<&str> = option_env!("PROV_TOKEN");
    const API_TOKEN: Option<&str> = option_env!("API_TOKEN");

    #[derive(Serialize, Deserialize, Clone)]
    pub struct PersistSettings {
        pub magic: u32,
        pub version: u32,
        pub wifi_ssid: Option<heapless::String<32>>,
        pub wifi_password: Option<heapless::String<64>>,
        pub prov_token: Option<heapless::String<64>>, // Short-lived provisioning used for initial WiFi setup identification with the API
        pub access_token: Option<heapless::String<64>>, // Permanent access token obtained after provisioning, used for authenticating API requests
        pub config: DeviceConfig, // Device configuration settings synced with server user settings
    }

    impl Default for PersistSettings {
        fn default() -> Self {
            persist_settings_default()
        }
    }

    const fn persist_settings_default() -> PersistSettings {
        PersistSettings {
            magic: SETTINGS_MAGIC,
            version: SETTINGS_VERSION,
            wifi_ssid: None,
            wifi_password: None,
            prov_token: None,
            access_token: None,
            config: DeviceConfig {
                brightness_percent: 25,
                current_limit_ma: 1000,
                color_mode: ColorMode::Original as i32,
                primary_color_rgb8: pack_rgb8(31, 255, 102),
                secondary_color_rgb8: pack_rgb8(255, 56, 20),
                tertiary_color_rgb8: pack_rgb8(107, 255, 179),
                color_temperature_shift: 0,
                disruption_mode: DisruptionMode::Off as i32,
                disruption_color_rgb8: pack_rgb8(255, 0, 76),
                disruption_brightness_percent: 80,
                disruption_interval: DisruptionInterval::Every3s as i32,
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
                auto_firmware_update_enabled: true,
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
            },
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
            self.access_token = None;
        }

        pub fn has_credentials_and_is_authenticated(&self) -> bool {
            self.wifi_ssid.is_some() && self.wifi_password.is_some() && self.access_token.is_some()
        }
    }

    static SETTINGS: Mutex<CriticalSectionRawMutex, PersistSettings> =
        Mutex::new(persist_settings_default());

    pub async fn init(flash_store: &mut FlashStorage<'_>) {
        let mut pt_mem: [u8; PARTITION_TABLE_MAX_LEN] = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let partition_table = partitions::read_partition_table(flash_store, &mut pt_mem).unwrap();

        // Get flash storage from settings partition
        let mut storage = partition_table
            .iter()
            .find(|part| {
                part.partition_type()
                    == partitions::PartitionType::Data(partitions::DataPartitionSubType::LittleFs)
                    && part.label_as_str() == "settings"
            })
            .expect("Settings partition not found")
            .as_embedded_storage(flash_store);

        // Read settings data from storage
        let mut buf = [0u8; SETTINGS_MAX_BYTE_SIZE];
        storage
            .read(0, &mut buf)
            .expect("Failed to read settings from storage");
        let mut settings: PersistSettings =
            postcard::from_bytes(&buf).unwrap_or(PersistSettings::default());

        // Check magic is valid
        if settings.magic != SETTINGS_MAGIC {
            trace::wrn!(
                "Invalid settings magic (expected 0x{:X}, got 0x{:X}), resetting to defaults",
                SETTINGS_MAGIC,
                settings.magic
            );
            settings = PersistSettings::default();
        }

        // Check version mismatch
        if settings.version != SETTINGS_VERSION && migrate_settings(&mut settings).is_err() {
            trace::wrn!(
                "Failed to migrate settings from version {} to {}, resetting to defaults",
                settings.version,
                SETTINGS_VERSION
            );
            settings = PersistSettings::default();
        }

        // Apply optional overrides from environment
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
        if let Some(token) = API_TOKEN {
            settings.access_token = heapless::String::try_from(token).ok();
            warn!("Overriding API access token from environment variable");
        }

        info!("Settings loaded from flash (v{})", settings.version);
        *SETTINGS.lock().await = settings;
    }

    fn migrate_settings(_old: &mut PersistSettings) -> Result<(), ()> {
        // No migrations yet
        Ok(())
    }

    pub async fn get_settings() -> PersistSettings {
        SETTINGS.lock().await.clone()
    }

    // If settings changed, schedule write to flash
    pub async fn update_settings<F>(f: F)
    where
        F: FnOnce(&mut PersistSettings),
    {
        _ = update_settings_changed(f).await;
    }

    // If settings changed, schedule write to flash and return whether changed
    pub async fn update_settings_changed<F>(f: F) -> bool
    where
        F: FnOnce(&mut PersistSettings),
    {
        let changed = {
            let mut guard = SETTINGS.lock().await;
            let old_crc = guard.crc32();
            f(&mut guard);
            let new_crc = guard.crc32();
            old_crc != new_crc
        };
        if changed {
            // Reduces write cycles by only writing when changed
            SETTINGS_SIGNAL.signal(AppSettingsEvent::StorePersistSettings);
        }
        changed
    }

    pub async fn store_settings(flash_store: &mut FlashStorage<'_>) {
        let mut pt_mem: [u8; PARTITION_TABLE_MAX_LEN] = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let partition_table = partitions::read_partition_table(flash_store, &mut pt_mem).unwrap();

        let mut storage = partition_table
            .iter()
            .find(|part| {
                part.partition_type()
                    == partitions::PartitionType::Data(partitions::DataPartitionSubType::LittleFs)
                    && part.label_as_str() == "settings"
            })
            .expect("Settings partition not found")
            .as_embedded_storage(flash_store);

        let settings = SETTINGS.lock().await;
        let serialized = postcard::to_vec::<_, SETTINGS_MAX_BYTE_SIZE>(&*settings)
            .expect("Failed to serialize settings");

        storage
            .write(0, serialized.deref())
            .expect("Failed to write settings to storage");
        debug!("Settings stored to flash (v{})", settings.version);
    }
}

// Volatile session settings (not persisted, reset on reboot)
pub mod session {
    use super::*;

    #[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct SessionSettings {
        pub light_on: bool,
        pub light_on_override: Option<bool>, // If set, overrides timer and other logic to force light on or off
        pub night_timer_active: bool, // Whether the day-night timer is currently driving the light off (night mode)
        pub is_time_synced: bool, // Whether the device has successfully synced time with the server
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
    }

    static SETTINGS: Mutex<CriticalSectionRawMutex, SessionSettings> =
        Mutex::new(SessionSettings {
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

    pub async fn update_settings_changed<F>(f: F) -> bool
    where
        F: FnOnce(&mut SessionSettings),
    {
        {
            let mut guard = SETTINGS.lock().await;
            let old = guard.clone();
            f(&mut guard);
            old != *guard
        }
    }
}

pub mod info {
    use super::*;

    const DEVICE_INFO_MAGIC: u32 = 0xEDA1BEE;

    #[derive(Clone, Serialize, Deserialize)]
    struct DeviceInfo {
        magic: u32,
        product_id: heapless::String<16>,
        firmware: FirmwareInfo,
    }

    #[derive(Clone, Serialize, Deserialize)]
    struct FirmwareInfo {
        version_major: u32,
        version_minor: u32,
        version_patch: u32,
        is_beta: bool,
        is_factory: bool,
        is_rolled_back: bool,
    }

    pub fn store() {
        SETTINGS_SIGNAL.signal(AppSettingsEvent::StoreDeviceInfo);
    }

    pub(super) async fn write_to_flash(flash_store: &mut FlashStorage<'_>) {
        let mut pt_mem: [u8; PARTITION_TABLE_MAX_LEN] = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let partition_table = partitions::read_partition_table(flash_store, &mut pt_mem).unwrap();

        let mut storage = partition_table
            .iter()
            .find(|part| {
                part.partition_type()
                    == partitions::PartitionType::Data(partitions::DataPartitionSubType::LittleFs)
                    && part.label_as_str() == "info"
            })
            .expect("Device info partition not found")
            .as_embedded_storage(flash_store);

        let session_settings = session::get_settings().await;
        let info = DeviceInfo {
            magic: DEVICE_INFO_MAGIC,
            product_id: heapless::String::try_from(CONFIG.product.as_str()).unwrap(),
            firmware: FirmwareInfo {
                version_major: CONFIG.fw_version.major,
                version_minor: CONFIG.fw_version.minor,
                version_patch: CONFIG.fw_version.patch,
                is_beta: CONFIG.fw_version.beta,
                is_factory: session_settings.is_factory_firmware,
                is_rolled_back: session_settings.is_rolled_back_firmware,
            },
        };
        let serialized = postcard::to_vec::<_, 64>(&info).expect("Failed to serialize device info");

        storage
            .write(0, serialized.deref())
            .expect("Failed to write device info to storage");
        info!("Device info stored to flash");
    }
}
