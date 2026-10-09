// Messages to the gateway, encoded
use alloc::{string::String, vec::Vec};
use embassy_time::Instant;
use prost::Message;

use super::client_proto::{
    self, ClientMessage, DeviceErrors, DeviceInfo, DeviceStatus, DeviceTelemetry, Echo,
    client_message::Payload,
};
use crate::{
    config::CONFIG,
    display::leds,
    net::wifi_net::SharedWifiController,
    store::{app_settings, transit_data},
    time, trace,
};

fn encode(payload: Payload) -> Vec<u8> {
    ClientMessage {
        version: client_proto::VERSION,
        payload: Some(payload),
    }
    .encode_to_vec()
}

pub async fn status() -> Vec<u8> {
    let wifi_ssid: String = app_settings::persist::get_settings()
        .await
        .wifi_ssid
        .unwrap_or_default()
        .as_str()
        .into();
    let session_settings = app_settings::session::get_settings().await;
    encode(Payload::Status(DeviceStatus {
        is_light_on: session_settings.light_on,
        is_updating_firmware: session_settings.updating_firmware,
        has_update_failed: session_settings.update_has_failed,
        wifi_ssid,
    }))
}

pub async fn config() -> Vec<u8> {
    let config = app_settings::persist::get_settings().await.config;
    encode(Payload::Config(config))
}

pub async fn telemetry(controller: &'static SharedWifiController) -> Vec<u8> {
    let transit_data_stats = transit_data::get_stats().await;
    let heap_stats = esp_alloc::HEAP.stats();
    let session_settings = app_settings::session::get_settings().await;

    encode(Payload::Telemetry(DeviceTelemetry {
        uptime_seconds: Instant::now().as_secs() as u32,
        wifi_rssi_dbm: controller.lock().await.rssi().unwrap_or(0),
        current_estimate_milliamps: leds::get_current_estimate_milliamps().await,
        update_progress_percent: session_settings.update_progress_percent as u32,
        update_speed_bytes_per_second: session_settings.update_speed_bytes_per_sec,
        num_vehicles_available: transit_data_stats.num_vehicles_available,
        num_vehicles_visible: transit_data_stats.num_vehicles_visible,
        num_disruptions_available: transit_data_stats.num_disruptions_available,
        num_disruptions_visible: transit_data_stats.num_disruptions_visible,
        num_pixels_on: transit_data_stats.num_pixels_on,
        last_transit_data_received_unix_timestamp: transit_data_stats.received_at_timestamp,
        last_transit_data_sourced_unix_timestamp: transit_data_stats.sourced_at_timestamp,
        last_transit_data_simulated_until_unix_timestamp: transit_data_stats
            .simulated_until_timestamp,
        transit_data_downlink_bytes_per_second: transit_data_stats
            .transit_data_downlink_bytes_per_second,
        heap_size_bytes: heap_stats.size as u32,
        heap_max_used_bytes: heap_stats.max_usage as u32,
        heap_current_used_bytes: heap_stats.current_usage as u32,
        feed_source: transit_data_stats.feed_source.as_str().into(),
        num_vehicles_visible_real_time: transit_data_stats.num_vehicles_visible_real_time,
        brightness_percent: leds::get_current_brightness_percent().await as u32,
        auto_update_scheduled_unix_timestamp: session_settings.auto_update_scheduled_unix_timestamp,
    }))
}

/// The errors reported since the last time, which are cleared.
pub async fn errors() -> Vec<u8> {
    let errors = trace::get_errors();
    let unix_timestamp = time::get_unix_timestamp_seconds().await;
    let message = encode(Payload::Errors(DeviceErrors {
        errors: errors
            .into_iter()
            .map(|mut error| {
                error.unix_timestamp = unix_timestamp;
                error
            })
            .collect(),
    }));
    trace::clear_errors();
    message
}

pub async fn info() -> Vec<u8> {
    let session_settings = app_settings::session::get_settings().await;
    encode(Payload::DeviceInfo(DeviceInfo {
        firmware_version_major: CONFIG.fw_version.major,
        firmware_version_minor: CONFIG.fw_version.minor,
        firmware_version_patch: CONFIG.fw_version.patch,
        is_beta_firmware: CONFIG.fw_version.beta,
        is_rolled_back_firmware: session_settings.is_rolled_back_firmware,
        is_factory_firmware: session_settings.is_factory_firmware,
    }))
}

pub fn echo(echo: Echo) -> Vec<u8> {
    encode(Payload::Echo(echo))
}
