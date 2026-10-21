// Messages to the gateway, encoded
use alloc::{boxed::Box, string::String, vec::Vec};
use embassy_time::Instant;
use prost::Message;

use super::client_proto::{
    self, ClientMessage, DeviceErrors, DeviceInfo, DeviceStatus, DeviceTelemetry, Echo, TimeSync,
    client_message::Payload,
};
use crate::{
    chip_temperature,
    config::CONFIG,
    display::leds,
    net::{stats as net_stats, wifi_net::SharedWifiController},
    profiling, stats,
    store::{
        app_settings::{self, info::InstallMethod},
        transit_data,
    },
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
    profiling::once!(first_status_sent);
    let persist_settings = app_settings::persist::get_settings().await;
    let wifi_ssid: String = persist_settings
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
        command_awaiting_confirmation: session_settings
            .pending_confirmation
            .map(|pending| pending.command as i32),
        is_remote_control_enabled: persist_settings.remote_control_enabled,
    }))
}

/// The boot profiling, once it's finished (only the first time).
pub fn profiling() -> Option<Vec<u8>> {
    profiling::take_finished().map(|profiling| encode(Payload::Profiling(Box::new(profiling))))
}

pub async fn config() -> Vec<u8> {
    let config = app_settings::persist::get_settings().await.config;
    encode(Payload::Config(config))
}

pub async fn telemetry(controller: &'static SharedWifiController) -> Vec<u8> {
    let transit_data_stats = transit_data::get_stats().await;
    let heap_stats = esp_alloc::HEAP.stats();
    let session_settings = app_settings::session::get_settings().await;
    let time_sync_stats = time::sync_stats();
    let followed_sync = time_sync_stats.followed.as_ref();

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
        heap_size_bytes: heap_stats.size as u32,
        heap_max_used_bytes: heap_stats.max_usage as u32,
        heap_current_used_bytes: heap_stats.current_usage as u32,
        feed_source: transit_data_stats.feed_source.as_str().into(),
        num_vehicles_visible_real_time: transit_data_stats.num_vehicles_visible_real_time,
        brightness_percent: leds::get_current_brightness_percent().await as u32,
        auto_update_scheduled_unix_timestamp: session_settings.auto_update_scheduled_unix_timestamp,
        command_confirmation_deadline_unix_timestamp: session_settings
            .pending_confirmation
            .map(|pending| pending.deadline_unix_timestamp),
        display_frames_per_second: session_settings.display_frames_per_second,
        gateway_rx_bytes_total: net_stats::GATEWAY_TRAFFIC.rx_bytes.get(),
        gateway_tx_bytes_total: net_stats::GATEWAY_TRAFFIC.tx_bytes.get(),
        ota_rx_bytes_total: net_stats::OTA_TRAFFIC.rx_bytes.get(),
        transit_data_rx_bytes_total: net_stats::TRANSIT_DATA_BYTES.get(),
        transit_data_rx_count_total: net_stats::TRANSIT_DATA_COUNT.get() as u32,
        transit_data_errors_total: net_stats::TRANSIT_DATA_ERRORS.get() as u32,
        gateway_connections_total: net_stats::GATEWAY_CONNECTIONS.get() as u32,
        wifi_connections_total: net_stats::WIFI_CONNECTIONS.get() as u32,
        gateway_connected_seconds: net_stats::gateway_connected_seconds(),
        last_transit_data_size_bytes: transit_data_stats.size_bytes,
        last_transit_data_receive_millis: transit_data_stats.receive_millis,
        reset_reason: esp_hal::system::reset_reason().map_or(0, |reason| reason as i32),
        boot_count_total: stats::boot_count(),
        settings_writes_total: stats::SETTINGS_WRITES.get() as u32,
        executor_stalled_micros_total: stats::EXECUTOR_STALLED_MICROS.get(),
        wifi_last_disconnect_reason: net_stats::wifi_last_disconnect_reason(),
        button_presses_total: stats::BUTTON_PRESSES.get() as u32,
        chip_temperature_celsius: chip_temperature::read_celsius(),
        time_sync_round_trip_micros: followed_sync.map(|sync| sync.round_trip_micros),
        time_sync_correction_micros: followed_sync.map(|sync| sync.correction_micros),
        time_sync_measured_unix_timestamp: followed_sync.map(|sync| sync.measured_unix_timestamp),
        time_sync_uncertainty_micros: followed_sync.map(|sync| sync.uncertainty_micros),
        time_sync_slew_remaining_micros: followed_sync.map(|sync| sync.slew_remaining_micros),
        time_sync_last_burst_round_trip_micros: time_sync_stats.last_burst_round_trip_micros,
        time_sync_requests_total: time_sync_stats.requests,
        time_sync_replies_total: time_sync_stats.replies,
        time_sync_bursts_total: time_sync_stats.bursts,
        time_sync_corrections_total: time_sync_stats.corrections,
        time_sync_steps_total: time_sync_stats.steps,
    }))
}

/// Stamped with the uptime now: encoded right before it's sent.
pub fn time_sync() -> Vec<u8> {
    time::SYNC_REQUESTS.inc();
    encode(Payload::TimeSync(TimeSync {
        client_send_micros_since_boot: Instant::now().as_micros(),
        server_receive_unix_timestamp_micros: None,
        server_send_unix_timestamp_micros: None,
    }))
}

/// The errors reported since the last time, which are cleared.
pub async fn errors() -> Vec<u8> {
    let errors = trace::get_errors();
    let unix_timestamp = time::unix_seconds();
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
    let (build_id, install_method) = app_settings::info::running_install();
    encode(Payload::DeviceInfo(DeviceInfo {
        firmware_version_major: CONFIG.fw_version.major,
        firmware_version_minor: CONFIG.fw_version.minor,
        firmware_version_patch: CONFIG.fw_version.patch,
        is_beta_firmware: CONFIG.fw_version.beta,
        is_rolled_back_firmware: session_settings.is_rolled_back_firmware,
        is_factory_firmware: session_settings.is_factory_firmware,
        build_id: build_id.into(),
        install_method: match install_method {
            InstallMethod::Unknown => client_proto::InstallMethod::Unknown,
            InstallMethod::Factory => client_proto::InstallMethod::Factory,
            InstallMethod::Usb => client_proto::InstallMethod::Usb,
            InstallMethod::Ota => client_proto::InstallMethod::Ota,
        } as i32,
    }))
}

pub fn echo(echo: Echo) -> Vec<u8> {
    encode(Payload::Echo(echo))
}
