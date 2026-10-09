// Messages from the gateway
use defmt::info;
use embassy_time::{Duration, Instant};

use super::{
    WsClientError,
    client_proto::{
        DeviceCommand, DeviceConfig, DeviceStatus, DeviceUpdate, ServerInfo, TransitData,
        client_message::Payload,
    },
    quit, send_config, send_echo, send_info, send_status, send_telemetry,
};
use crate::{
    display::leds::{self, LedPixels},
    ota,
    store::{
        app_settings::{self, persist::PersistSettings},
        transit_data,
    },
    trace,
};

const AUTO_UPDATE_DELAY: Duration = Duration::from_secs(5 * 60);

pub async fn handle_message(payload: Payload, payload_len: usize) {
    match payload {
        Payload::ServerInfo(server_info) => on_server_info(server_info).await,
        Payload::Status(status) => on_status(status).await,
        Payload::Config(config) => on_config(config).await,
        Payload::Command(command) => on_command(command).await,
        Payload::Echo(echo) => send_echo(echo),
        Payload::TransitData(data) => on_transit_data(data, payload_len).await,
        Payload::DeviceUpdate(update) => on_device_update(update).await,
        _ => trace::err!("WS: Unhandled server message payload"),
    }
}

// World time, as offsets to the uptime
async fn on_server_info(server_info: ServerInfo) {
    let changed = app_settings::session::update_settings_changed(|set| {
        let uptime_secs = Instant::now().as_secs() as u32;
        set.unix_epoch_offset_secs = server_info.unix_timestamp.wrapping_sub(uptime_secs);
        set.local_time_of_day_offset_secs = server_info
            .local_time_of_day_seconds
            .wrapping_sub(uptime_secs);
        set.local_weekday_number = server_info.local_weekday_number;
        set.local_sunrise_time_of_day_seconds = server_info.local_sunrise_time_of_day_seconds;
        set.local_sunset_time_of_day_seconds = server_info.local_sunset_time_of_day_seconds;
        set.is_time_synced = true;
    })
    .await;
    if changed {
        info!(
            "WS: Updated unix: {}, local secs: {}, weekday no: {}, sunrise secs: {}, sunset secs: {}",
            server_info.unix_timestamp,
            server_info.local_time_of_day_seconds,
            server_info.local_weekday_number,
            server_info.local_sunrise_time_of_day_seconds,
            server_info.local_sunset_time_of_day_seconds,
        );
    }
    send_info();
    send_config();
    leds::set_status_led_from_session().await;
}

// Light switched on or off in the app
async fn on_status(status: DeviceStatus) {
    let changed = app_settings::session::update_settings_changed(|set| {
        set.light_on = status.is_light_on;
        set.light_on_override = Some(status.is_light_on);
    })
    .await;
    if changed {
        info!("WS: Updated light on state: {}", status.is_light_on);
        send_telemetry();
    }
    leds::set_status_led_from_session().await;
}

async fn on_config(config: DeviceConfig) {
    let has_scheduled_update = app_settings::session::get_settings()
        .await
        .auto_update_scheduled_unix_timestamp
        .is_some();
    if !config.auto_firmware_update_enabled && has_scheduled_update {
        info!("WS: Auto firmware update disabled, clearing scheduled update timestamp");
        ota::cancel();
    }

    let changed = app_settings::persist::update_settings_changed(move |set| {
        set.config = config.clone();
    })
    .await;
    if changed {
        info!("WS: Updated device config from server");
        transit_data::on_config_updated().await;
    }
}

async fn on_command(command: i32) {
    let Ok(command) = DeviceCommand::try_from(command) else {
        info!("WS: Received unknown device command: {}", command);
        return;
    };

    match command {
        DeviceCommand::Reboot => {
            info!("WS: Received reboot command from server, restarting device...");
            transit_data::reset().await;
            leds::set_pixels(LedPixels::Off).await;
            quit(Err(WsClientError::Reboot));
        }
        DeviceCommand::Identify => {
            info!(
                "WS: Received identify command from server, starting LED identification sequence..."
            );
            leds::set_pixels(LedPixels::Identify).await;
        }
        DeviceCommand::FactoryReset => {
            info!("WS: Received factory reset command from server, resetting device settings...");
            app_settings::persist::update_settings(|set| *set = PersistSettings::default()).await;
            send_config();
            quit(Err(WsClientError::FactoryReset));
        }
        DeviceCommand::Reprovision => {
            info!("WS: Received reprovision command from server, restarting WiFi provisioning...");
            quit(Err(WsClientError::RestartProvisioning));
        }
        DeviceCommand::ResetConfigDefaults => {
            info!(
                "WS: Received reset config to defaults command from server, resetting device config..."
            );
            app_settings::persist::update_settings(|set| {
                set.config = PersistSettings::default().config;
            })
            .await;
            app_settings::session::update_settings(|set| {
                set.light_on = true;
                set.light_on_override = None;
            })
            .await;
            transit_data::update_line_configs().await;
            send_config();
            send_status();
        }
        DeviceCommand::StartFirmwareUpdate => {
            let settings = app_settings::session::get_settings().await;
            if let Some(update) = &settings.firmware_update_available {
                info!(
                    "WS: Received start firmware update command from server, starting update process..."
                );
                ota::start_firmware_update(update).await;
            } else {
                trace::wrn!(
                    "WS: Received start firmware update command but no update info available"
                );
            }
        }
        DeviceCommand::TestLeds => {
            info!("WS: Received test LEDs command from server, starting LED test sequence...");
            app_settings::session::update_settings(|set| {
                set.test_mode_active = true;
            })
            .await;
            leds::set_pixels(LedPixels::TestMode).await;
        }
        DeviceCommand::Reconnect => {
            info!("WS: Received reconnect command from server, reconnecting to server...");
            quit(Err(WsClientError::Reconnect));
        }
    }
}

async fn on_transit_data(data: TransitData, payload_len: usize) {
    info!(
        "WS: Received transit data update: {} lines, {} stops, {} vehicles, {} disruptions",
        data.lines.len(),
        data.stops.len(),
        data.vehicle_movements.len(),
        data.disruptions.len()
    );
    transit_data::on_data(data, payload_len).await;

    // Received, decoded and processed transit data: the boot check passed
    ota::confirm_boot();
}

// Kept for when the update is started, which is automatic if enabled
async fn on_device_update(update: DeviceUpdate) {
    info!(
        "WS: Received device firmware update info: version {}.{}.{}, url: {}",
        update.firmware_version_major,
        update.firmware_version_minor,
        update.firmware_version_patch,
        update.image_url,
    );
    let settings = app_settings::persist::get_settings().await;
    if settings.config.auto_firmware_update_enabled {
        info!("WS: Auto firmware update is enabled, scheduling update in 5 minutes...");
        ota::schedule_firmware_update(&update, AUTO_UPDATE_DELAY).await;
    }
    app_settings::session::update_settings(|set| {
        set.firmware_update_available = Some(update);
    })
    .await;
    leds::set_status_led_from_session().await;
}
