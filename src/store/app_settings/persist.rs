// Persistent application settings (stored in flash)
use alloc::vec;
use core::sync::atomic::{AtomicBool, Ordering};

use defmt::{debug, info, warn};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_hal::rom::crc::crc32_le;
use esp_storage::FlashStorage;
use postcard::ser_flavors::Flavor;
use serde::{Deserialize, Serialize};

use super::STORE_SETTINGS_SIGNAL;
use crate::{
    config::CONFIG,
    net::{
        wifi_psk::Psk,
        ws_client::client_proto::{
            ColorMode, DeviceConfig, DisruptionFilter, DisruptionInterval, DisruptionMode,
            RealtimeFilter, RenderMode, SunlightAutoBrightness, TimerSettings, VehicleFilter,
        },
    },
    stats,
    store::with_data_partition,
    trace,
    util::pack_rgb8,
};

const SETTINGS_PARTITION: &str = "settings";
const SETTINGS_MAGIC: u32 = 0xA562E1B;
const SETTINGS_VERSION: u32 = 2;
const SETTINGS_MAX_BYTE_SIZE: usize = 1024;
// Before falling back to the defaults
const READ_ATTEMPTS: usize = 3;

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
    pub remote_control_enabled: bool, // Whether the server may switch the light, change the config and run commands. Only the buttons turn it back on
    pub config: DeviceConfig, // Device configuration settings synced with server user settings
    pub wifi_psk: Option<Psk>, // The WPA2 key derived from the WiFi credentials, connected with in place of the password (see wifi_psk)
}

// Version 1: without the WiFi key
#[derive(Deserialize)]
struct PersistSettingsV1 {
    magic: u32,
    _version: u32,
    wifi_ssid: Option<heapless::String<32>>,
    wifi_password: Option<heapless::String<64>>,
    prov_token: Option<heapless::String<64>>,
    claimed: bool,
    remote_control_enabled: bool,
    config: DeviceConfig,
}

impl From<PersistSettingsV1> for PersistSettings {
    fn from(v1: PersistSettingsV1) -> Self {
        PersistSettings {
            magic: v1.magic,
            version: SETTINGS_VERSION,
            wifi_ssid: v1.wifi_ssid,
            wifi_password: v1.wifi_password,
            prov_token: v1.prov_token,
            claimed: v1.claimed,
            remote_control_enabled: v1.remote_control_enabled,
            config: v1.config,
            wifi_psk: None,
        }
    }
}

// The start of every version, to tell which one is stored
#[derive(Deserialize)]
struct SettingsHeader {
    magic: u32,
    version: u32,
}

impl Default for PersistSettings {
    fn default() -> Self {
        persist_settings_default()
    }
}

// Settings too large to store were refused before: only reported once
static TOO_LARGE_REPORTED: AtomicBool = AtomicBool::new(false);

// The size and CRC of the serialized settings, computed while serializing
struct LenCrcFlavor {
    len: usize,
    crc: u32,
}

impl Flavor for LenCrcFlavor {
    type Output = (usize, u32);

    fn try_push(&mut self, data: u8) -> postcard::Result<()> {
        self.try_extend(&[data])
    }

    fn try_extend(&mut self, data: &[u8]) -> postcard::Result<()> {
        self.len += data.len();
        self.crc = crc32_le(self.crc, data);
        Ok(())
    }

    fn finalize(self) -> postcard::Result<Self::Output> {
        Ok((self.len, self.crc))
    }
}

impl PersistSettings {
    /// The serialized size and CRC, without a buffer: also of settings too
    /// large to store.
    fn serialized_len_and_crc(&self) -> Option<(usize, u32)> {
        let flavor = LenCrcFlavor {
            len: 0,
            crc: 0xffffffff,
        };
        postcard::serialize_with_flavor(self, flavor).ok()
    }

    pub fn clear_wifi_credentials_and_auth(&mut self) {
        self.wifi_ssid = None;
        self.wifi_password = None;
        self.wifi_psk = None;
        self.prov_token = None;
        self.claimed = false;
    }

    /// Sets the WiFi credentials, dropping the key derived from the previous
    /// ones (if they differ).
    pub fn set_wifi_credentials(&mut self, ssid: &str, password: &str) {
        let ssid = Some(heapless::String::try_from(ssid).unwrap_or_default());
        let password = Some(heapless::String::try_from(password).unwrap_or_default());
        if self.wifi_ssid != ssid || self.wifi_password != password {
            self.wifi_ssid = ssid;
            self.wifi_password = password;
            self.wifi_psk = None;
        }
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
        remote_control_enabled: true,
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
            disruptions_enabled: false,
        },
        wifi_psk: None,
    }
}

static SETTINGS: Mutex<CriticalSectionRawMutex, PersistSettings> =
    Mutex::new(persist_settings_default());

pub async fn init(flash_store: &mut FlashStorage<'_>) {
    let mut buf = [0u8; SETTINGS_MAX_BYTE_SIZE];
    let mut result = Ok(());
    for _ in 0..READ_ATTEMPTS {
        result = with_data_partition(flash_store, SETTINGS_PARTITION, |storage| {
            storage.read(0, &mut buf)
        });
        if result.is_ok() {
            break;
        }
    }
    // Unreadable: the defaults (the zeroed buffer), the map has to be set up
    // again. Better than a reboot loop
    if let Err(e) = result {
        trace::err!(
            "Failed to read settings from flash, using defaults: {:?}",
            e
        );
        buf.fill(0);
    }
    let mut settings = deserialize_settings(&buf);
    apply_env_overrides(&mut settings);

    info!("Settings loaded from flash (v{})", settings.version);
    *SETTINGS.lock().await = settings;
}

// The stored settings. The fields aren't named in the stored data, so each
// version is read as stored, and migrated to the current one
fn deserialize_settings(buf: &[u8]) -> PersistSettings {
    let header = postcard::from_bytes::<SettingsHeader>(buf).unwrap_or(SettingsHeader {
        magic: 0,
        version: 0,
    });
    if header.magic != SETTINGS_MAGIC {
        trace::wrn!(
            "Invalid settings magic (expected 0x{:X}, got 0x{:X}), resetting to defaults",
            SETTINGS_MAGIC,
            header.magic
        );
        return PersistSettings::default();
    }

    let settings = match header.version {
        SETTINGS_VERSION => postcard::from_bytes::<PersistSettings>(buf).ok(),
        1 => postcard::from_bytes::<PersistSettingsV1>(buf)
            .ok()
            .map(PersistSettings::from),
        _ => None,
    };
    settings.unwrap_or_else(|| {
        trace::wrn!(
            "Failed to load settings version {} (current {}), resetting to defaults",
            header.version,
            SETTINGS_VERSION
        );
        PersistSettings::default()
    })
}

// The WiFi key stays when the credentials are the stored ones
fn apply_env_overrides(settings: &mut PersistSettings) {
    if WIFI_SSID.is_some() || WIFI_PASSWORD.is_some() {
        let (ssid, password) = (settings.wifi_ssid.clone(), settings.wifi_password.clone());
        settings.set_wifi_credentials(
            WIFI_SSID.or(ssid.as_deref()).unwrap_or_default(),
            WIFI_PASSWORD.or(password.as_deref()).unwrap_or_default(),
        );
        warn!("Overriding WiFi credentials from environment variable");
    }
    if let Some(token) = PROV_TOKEN {
        settings.prov_token = heapless::String::try_from(token).ok();
        warn!("Overriding provisioning token from environment variable");
    }
}

pub async fn get_settings() -> PersistSettings {
    SETTINGS.lock().await.clone()
}

/// A value from the settings, without copying them all (for frequent reads).
pub async fn read_settings<R>(f: impl FnOnce(&PersistSettings) -> R) -> R {
    f(&*SETTINGS.lock().await)
}

pub async fn update_settings<F>(f: F)
where
    F: FnOnce(&mut PersistSettings),
{
    _ = update_settings_changed(f).await;
}

/// Updates the settings, and whether that changed them. Only changes are
/// written to flash, to save write cycles. An update that would make them too
/// large to store is refused (e.g. a config from the server with long names):
/// the settings stay as they were.
pub async fn update_settings_changed<F>(f: F) -> bool
where
    F: FnOnce(&mut PersistSettings),
{
    let changed = {
        let mut guard = SETTINGS.lock().await;
        let old = guard.clone();
        f(&mut guard);
        match guard.serialized_len_and_crc() {
            Some((len, crc)) if len <= SETTINGS_MAX_BYTE_SIZE => {
                old.serialized_len_and_crc().map(|(_, old_crc)| old_crc) != Some(crc)
            }
            len => {
                *guard = old;
                report_too_large(len.map(|(len, _)| len));
                false
            }
        }
    };
    if changed {
        STORE_SETTINGS_SIGNAL.signal(());
    }
    changed
}

fn report_too_large(len: Option<usize>) {
    if TOO_LARGE_REPORTED.load(Ordering::Relaxed) {
        warn!(
            "Settings update refused, too large to store: {:?} bytes",
            len
        );
        return;
    }
    // Load and store only: no atomic read-modify-write on this target
    TOO_LARGE_REPORTED.store(true, Ordering::Relaxed);
    trace::err!(
        "Settings update refused, too large to store: {:?} bytes (max {})",
        len,
        SETTINGS_MAX_BYTE_SIZE
    );
}

/// Writes the settings to flash. If that fails, they're kept in RAM, and
/// written with the next change.
pub(super) async fn store_settings(flash_store: &mut FlashStorage<'_>) {
    let settings = SETTINGS.lock().await;
    let serialized = match postcard::to_vec::<_, SETTINGS_MAX_BYTE_SIZE>(&*settings) {
        Ok(serialized) => serialized,
        Err(_) => {
            trace::err!("Failed to serialize settings");
            return;
        }
    };
    let written = with_data_partition(flash_store, SETTINGS_PARTITION, |storage| {
        storage.write(0, &serialized)
    });
    if let Err(e) = written {
        trace::err!("Failed to write settings to flash: {:?}", e);
        return;
    }
    stats::SETTINGS_WRITES.inc();
    debug!(
        "Settings stored to flash (v{}, {} bytes)",
        settings.version,
        serialized.len()
    );
}
