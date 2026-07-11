use core::{ffi::CStr, net::SocketAddr};

use alloc::{format, string::String, vec::Vec};
use condtype::{CondType, condval};
use defmt::{debug, error, info, warn};
use edge_http::io::client;
use edge_nal_embassy::{Tcp, TcpBuffers, TcpError};
use edge_ws::{FrameHeader, FrameType};
use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_net::{Stack, dns};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::{Duration, Instant, TimeoutError, Timer, with_timeout};
use embedded_io_async::Write;
use envparse::parse_env;
use esp_hal::rng::Rng;
use mbedtls_rs::{Certificate, ClientSessionConfig, SessionError, Tls, TlsConnector};
use prost::Message;
use smoltcp::wire::DnsQueryType;

use crate::{
    config::CONFIG,
    display::leds::{self, LedPixels, LedStatus},
    net::{
        auth_client,
        wifi_net::{self, SharedWifiController},
        ws_client::client_proto::{
            ClientMessage, DeviceCommand, DeviceErrors, DeviceInfo, DeviceStatus, DeviceTelemetry,
            Echo, client_message::Payload,
        },
    },
    store::{
        app_settings::{self, persist::PersistSettings},
        ota, transit_data,
    },
    time, trace,
};

pub mod client_proto {
    pub const VERSION: u32 = 1;
    include!(concat!(env!("OUT_DIR"), "/ledtransit_client.rs"));
}

const TCP_RX_BUF_SIZE: usize = 512;
const TCP_TX_BUF_SIZE: usize = 512;
const TCP_BUF_POOL_SIZE: usize = 1;

const HTTP_MAX_NUM_HEADERS: usize = 32;
const HTTP_WS_RX_BUF_SIZE: usize = 20 * 1024;

const PROTO_TRANSIT_DATA_MAGIC: [u8; 3] = [0x08, 0x01, 0x52]; // First 3 bytes of encoded ClientMessage.TransitData message

const DEFAULT_GATEWAY_HOST: &str = "gateway.ledtransit.com"; // Regional load balancer
const GATEWAY_PORT: u16 = parse_env!("GATEWAY_PORT" as u16 else 443);
const SSL_ENABLED: bool = parse_env!("SSL_ENABLED" as bool else true);

static WS_CLIENT_CHANNEL: Channel<CriticalSectionRawMutex, WsClientNotify, 10> = Channel::new();
static WS_CLIENT_QUIT_SIGNAL: Signal<CriticalSectionRawMutex, Result<(), WsClientError>> =
    Signal::new();

type HttpsConnection<'a> = client::Connection<'a, TlsConnector<'a, Tcp<'a>>, HTTP_MAX_NUM_HEADERS>;
type HttpConnection<'a> = client::Connection<'a, Tcp<'a>, HTTP_MAX_NUM_HEADERS>;

pub type Connection<'a> = CondType<SSL_ENABLED, HttpsConnection<'a>, HttpConnection<'a>>;
type SocketError = CondType<SSL_ENABLED, SessionError, TcpError>;

type HttpError =
    CondType<SSL_ENABLED, edge_http::io::Error<SessionError>, edge_http::io::Error<TcpError>>;
type WsError = CondType<SSL_ENABLED, edge_ws::Error<SessionError>, edge_ws::Error<TcpError>>;

enum WsClientNotify {
    Status,
    Config,
    Telemetry,
    Errors,
    Echo(Echo),
    Info,
    Pong,
}

#[derive(defmt::Format, Debug)]
pub enum WsClientError {
    DnsError(dns::Error),
    HttpError(HttpError),
    WsError(WsError),
    Timeout(TimeoutError),
    DataError,
    AuthFailed,
    RestartProvisioning,
    FactoryReset,
    #[cfg(ssl_enabled)]
    SessionError(SessionError),
    Reboot,
    Reconnect,
}

pub fn spawn(
    spawner: Spawner,
    sta_stack: Stack<'static>,
    controller: &'static SharedWifiController,
    tls: &'static Tls<'static>,
    ca_cert: &'static Certificate<'static>,
) {
    spawner.spawn(ws_client_task(sta_stack, controller, tls, ca_cert).unwrap());
}

#[embassy_executor::task]
async fn ws_client_task(
    sta_stack: Stack<'static>,
    controller: &'static SharedWifiController,
    tls: &'static Tls<'static>,
    ca_cert: &'static Certificate<'static>,
) {
    loop {
        // Wait for link and IP
        sta_stack.wait_link_up().await;
        sta_stack.wait_config_up().await;

        let is_updating = app_settings::session::get_settings()
            .await
            .updating_firmware;
        leds::set_status(if is_updating {
            LedStatus::UpdatingFirmware
        } else {
            LedStatus::ConnectingServer
        });

        match run(sta_stack, controller, tls, ca_cert).await {
            Ok(_) => info!("WebSocket task ended normally"),
            Err(e) => {
                match e {
                    WsClientError::AuthFailed => {
                        trace::err!("WebSocket authentication failed");
                        leds::set_status(LedStatus::AuthError);
                    }
                    WsClientError::RestartProvisioning => {
                        info!("Restarting WiFi provisioning as requested by WS user");
                        wifi_net::start_provisioning().await;
                        if sta_stack.is_link_up() {
                            sta_stack.wait_config_down().await;
                        }
                    }
                    WsClientError::FactoryReset => {
                        info!("Factory resetting as requested by WS user");
                        ota::boot_from_factory();
                    }
                    WsClientError::Reboot => {
                        info!("Rebooting as requested by WS user");
                        Timer::after(Duration::from_millis(100)).await;
                        esp_hal::system::software_reset();
                    }
                    WsClientError::Reconnect => {
                        info!("Reconnecting to server as requested by WS user");
                        continue;
                    }
                    WsClientError::WsError(WsError::Invalid)
                    | WsClientError::Timeout(TimeoutError) => {} // ignore
                    _ => {
                        trace::err!("WebSocket connection error: {:?}", e);
                        leds::set_status(LedStatus::ServerError);
                    }
                }
            }
        }

        // Throttle reconnection attempts
        Timer::after(Duration::from_secs(3)).await;
    }
}

async fn run(
    sta_stack: Stack<'static>,
    controller: &'static SharedWifiController,
    tls: &Tls<'static>,
    ca_cert: &'static Certificate<'static>,
) -> Result<(), WsClientError> {
    WS_CLIENT_QUIT_SIGNAL.reset();

    // DNS resolve gateway host
    let host = option_env!("GATEWAY_HOST").unwrap_or(DEFAULT_GATEWAY_HOST);
    let ip_addr = *sta_stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(WsClientError::DnsError)?
        .first()
        .ok_or(WsClientError::DnsError(dns::Error::Failed))?;
    let socket_addr = SocketAddr::new(ip_addr.into(), GATEWAY_PORT);
    debug!("Resolved gateway to {} (SSL={})", socket_addr, SSL_ENABLED);

    // Create TCP connection
    let tcp_bufs = TcpBuffers::<TCP_BUF_POOL_SIZE, TCP_TX_BUF_SIZE, TCP_RX_BUF_SIZE>::new();
    let tcp = Tcp::new(sta_stack, &tcp_bufs);

    // Configure TLS session
    let host_zstr = format!("{}\0", host);
    let session_config = ClientSessionConfig {
        ca_chain: Some(ca_cert.clone()),
        server_name: Some(CStr::from_bytes_with_nul(host_zstr.as_bytes()).unwrap()),
        ..ClientSessionConfig::new()
    };
    let tls_connector = TlsConnector::new(tls.reference(), tcp, &session_config);

    // Create HTTP(S) connection
    let mut ws_rx_buf = [0u8; HTTP_WS_RX_BUF_SIZE];
    let mut conn = condval!(if SSL_ENABLED {
        HttpsConnection::new(&mut ws_rx_buf, &tls_connector, socket_addr)
    } else {
        client::Connection::<Tcp, HTTP_MAX_NUM_HEADERS>::new(&mut ws_rx_buf, &tcp, socket_addr)
    });

    // Check if need to authenticate first using provisioning token
    if let Some(prov_token) = app_settings::persist::get_settings().await.prov_token {
        match auth_client::provision_authenticate(&mut conn, &prov_token, host).await {
            Ok(()) => {} // Continue to WS authentication
            Err(WsClientError::AuthFailed) => {
                warn!("Provisioning authentication failed, clearing provisioning token");
                app_settings::persist::update_settings(|set| {
                    set.prov_token = None;
                })
                .await;
                return Err(WsClientError::RestartProvisioning);
            }
            Err(e) => return Err(e),
        }
    }

    // Check if access token is present
    let access_token = match app_settings::persist::get_settings().await.access_token {
        Some(token) => token,
        None => {
            error!("No access token stored, cannot establish WebSocket connection");
            return Err(WsClientError::AuthFailed);
        }
    };

    // Perform WebSocket upgrade and authentication
    let mut rng = Rng::new();
    auth_client::websocket_authenticate(host, &mut conn, &access_token, &mut rng).await?;
    info!("WebSocket connection established");

    // Get underlying raw socket
    let (mut socket, buf) = conn.release();

    #[cfg(ssl_enabled)]
    let (mut rx, mut tx) = socket.split().await.map_err(WsClientError::SessionError)?;
    #[cfg(not(ssl_enabled))]
    let (mut rx, mut tx) = {
        use edge_nal::TcpSplit;
        socket.split()
    };

    // Clear event channel
    WS_CLIENT_CHANNEL.clear();
    send_status();

    // Receive WS messages and queue outgoing messages
    let rx_fut = async {
        loop {
            // Receive WS frame header with timeout, expect to receive transit data every 30s
            let header = with_timeout(Duration::from_secs(45), FrameHeader::recv(&mut rx))
                .await
                .map_err(WsClientError::Timeout)?
                .map_err(WsClientError::WsError)?;

            // Receive WS frame payload with timeout
            let payload_buf =
                with_timeout(Duration::from_secs(10), header.recv_payload(&mut rx, buf))
                    .await
                    .map_err(WsClientError::Timeout)?
                    .map_err(WsClientError::WsError)?;

            match header.frame_type {
                FrameType::Binary(_) => {
                    // Peek if data contains transit data update
                    // -> To avoid having 2 large transit data objects in RAM at once,
                    // force the renderer to drop the previously allocated object first
                    let proto_has_transit_data =
                        payload_buf.len() >= 3 && payload_buf[0..3] == PROTO_TRANSIT_DATA_MAGIC;
                    let session_settings = app_settings::session::get_settings().await;
                    if proto_has_transit_data {
                        if session_settings.updating_firmware {
                            // During firmware update, don't update transit data to avoid memory pressure and potential OOM
                            continue;
                        }
                        transit_data::clear().await;
                        if session_settings.test_mode_active {
                            // During test mode, don't render transit data updates
                            continue;
                        }
                    }

                    // Decode protobuf message (alloc)
                    let message: ClientMessage = prost::Message::decode(payload_buf)
                        .map_err(|_| WsClientError::DataError)?;

                    // Check protocol version
                    if message.version != client_proto::VERSION {
                        trace::err!(
                            "WS: Version mismatch (got {}, expected {}), closing connection",
                            message.version,
                            client_proto::VERSION
                        );
                        return Err(WsClientError::DataError);
                    }

                    // Handle message
                    let payload = message.payload.ok_or(WsClientError::DataError)?;
                    handle_proto_message(payload, payload_buf.len()).await?;
                }
                FrameType::Ping => {
                    debug!("WS: Got ping, sending pong");
                    send_pong();
                    send_telemetry();
                }
                FrameType::Close => {
                    info!("WS: Got close frame from server");
                    return Ok(());
                }
                _ => {
                    trace::err!(
                        "WS: Received unsupported frame type: {:?}",
                        header.frame_type
                    );
                    return Err(WsClientError::DataError);
                }
            }

            if !header.frame_type.is_final() {
                trace::err!(
                    "WS: Received unsupported non-final frame: {:?}",
                    header.frame_type
                );
                return Err(WsClientError::DataError);
            }
        }
    };

    // Send queued WS messages
    let tx_fut = async {
        let mut rng = Rng::new();
        loop {
            match WS_CLIENT_CHANNEL.receive().await {
                WsClientNotify::Status => {
                    let payload = build_status().await?;
                    ws_send_binary_frame(&mut tx, &payload, &mut rng).await?;
                }
                WsClientNotify::Config => {
                    let payload = build_config().await?;
                    ws_send_binary_frame(&mut tx, &payload, &mut rng).await?;
                }
                WsClientNotify::Telemetry => {
                    let payload = build_telemetry(controller).await?;
                    ws_send_binary_frame(&mut tx, &payload, &mut rng).await?;
                }
                WsClientNotify::Errors => {
                    if trace::get_errors().is_empty() {
                        continue; // got no errors, discard event
                    }
                    let payload = build_errors().await?;
                    ws_send_binary_frame(&mut tx, &payload, &mut rng).await?;
                }
                WsClientNotify::Echo(echo) => {
                    let payload = build_echo(echo).await?;
                    ws_send_binary_frame(&mut tx, &payload, &mut rng).await?;
                }
                WsClientNotify::Info => {
                    let payload = build_info().await?;
                    ws_send_binary_frame(&mut tx, &payload, &mut rng).await?;
                }
                WsClientNotify::Pong => {
                    ws_send_pong_frame(&mut tx, &[], &mut rng).await?;
                }
            }
        }
    };

    // Wait for quit signal
    let quit_fut = async { WS_CLIENT_QUIT_SIGNAL.wait().await };

    // Receive and transmit until connection closed or error or quit signal
    let res = match select3(rx_fut, tx_fut, quit_fut).await {
        Either3::First(res) => res,
        Either3::Second(res) => res,
        Either3::Third(res) => res,
    };
    info!("WebSocket connection closed, shutting down ({:?})", res);

    // Send close frame and flush with timeout
    with_timeout(Duration::from_secs(1), async {
        ws_close(&mut tx, &mut rng).await.ok();
        tx.flush().await.ok();
    })
    .await
    .ok();

    drop((rx, tx));

    // Close socket with timeout
    with_timeout(Duration::from_secs(1), async {
        #[cfg(ssl_enabled)]
        socket.close().await.ok();
        #[cfg(not(ssl_enabled))]
        {
            use edge_nal::{Close, TcpShutdown};
            socket.close(Close::Both).await.ok();
        }
    })
    .await
    .ok();

    info!("WebSocket connection closed");
    res
}

async fn handle_proto_message(payload: Payload, payload_len: usize) -> Result<(), WsClientError> {
    match payload {
        Payload::ServerInfo(info) => {
            // On server info: Update session state (epoch time)
            if app_settings::session::update_settings_changed(|set| {
                set.unix_epoch_offset_secs = info.unix_timestamp - Instant::now().as_secs() as u32;
                set.local_time_of_day_offset_secs =
                    info.local_time_of_day_seconds - Instant::now().as_secs() as u32;
                set.local_weekday_number = info.local_weekday_number;
                set.local_sunrise_time_of_day_seconds = info.local_sunrise_time_of_day_seconds;
                set.local_sunset_time_of_day_seconds = info.local_sunset_time_of_day_seconds;
                set.is_time_synced = true;
            })
            .await
            {
                info!(
                    "WS: Updated unix: {}, local secs: {}, weekday no: {}, sunrise secs: {}, sunset secs: {}",
                    info.unix_timestamp,
                    info.local_time_of_day_seconds,
                    info.local_weekday_number,
                    info.local_sunrise_time_of_day_seconds,
                    info.local_sunset_time_of_day_seconds,
                );
            }
            send_info();
            leds::set_status_led_from_session().await;
        }
        Payload::Status(status) => {
            // On status update: Update session state (light on/off)
            if app_settings::session::update_settings_changed(|set| {
                set.light_on = status.is_light_on;
                set.light_on_override = Some(status.is_light_on);
            })
            .await
            {
                info!("WS: Updated light on state: {}", status.is_light_on);
                send_telemetry();
            }
            leds::set_status_led_from_session().await;
        }
        Payload::Config(config) => {
            // On auto-update setting disabled: Cancel scheduled update if any
            if !config.auto_firmware_update_enabled
                && app_settings::session::get_settings()
                    .await
                    .auto_update_scheduled_unix_timestamp
                    .is_some()
            {
                info!("WS: Auto firmware update disabled, clearing scheduled update timestamp");
                ota::cancel();
            }

            // On config update: Update persisted config
            if app_settings::persist::update_settings_changed(move |set| {
                set.config = config.clone();
            })
            .await
            {
                info!("WS: Updated device config from server");
                transit_data::on_config_updated().await;
            }
        }
        Payload::Command(command) => {
            match DeviceCommand::try_from(command) {
                Ok(DeviceCommand::Reboot) => {
                    info!("WS: Received reboot command from server, restarting device...");
                    transit_data::reset().await;
                    leds::set_pixels(LedPixels::Off).await;
                    quit(Err(WsClientError::Reboot));
                }
                Ok(DeviceCommand::Identify) => {
                    info!(
                        "WS: Received identify command from server, starting LED identification sequence..."
                    );
                    leds::set_pixels(leds::LedPixels::Identify).await;
                }
                Ok(DeviceCommand::FactoryReset) => {
                    info!(
                        "WS: Received factory reset command from server, resetting device settings..."
                    );
                    app_settings::persist::update_settings(|set| *set = PersistSettings::default())
                        .await;
                    // Send config update after reset to defaults
                    send_config();
                    quit(Err(WsClientError::FactoryReset));
                }
                Ok(DeviceCommand::Reprovision) => {
                    info!(
                        "WS: Received reprovision command from server, restarting WiFi provisioning..."
                    );
                    quit(Err(WsClientError::RestartProvisioning));
                }
                Ok(DeviceCommand::ResetConfigDefaults) => {
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
                    // Send config/status update after reset to defaults
                    send_config();
                    send_status();
                }
                Ok(DeviceCommand::StartFirmwareUpdate) => {
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
                Ok(DeviceCommand::TestLeds) => {
                    info!(
                        "WS: Received test LEDs command from server, starting LED test sequence..."
                    );
                    app_settings::session::update_settings(|set| {
                        set.test_mode_active = true;
                    })
                    .await;
                    leds::set_pixels(leds::LedPixels::TestMode).await;
                }
                Ok(DeviceCommand::Reconnect) => {
                    info!("WS: Received reconnect command from server, reconnecting to server...");
                    quit(Err(WsClientError::Reconnect));
                }
                Err(_) => {
                    info!("WS: Received unknown device command: {}", command);
                }
            }
        }
        Payload::Echo(echo) => {
            // Send back the same echo message
            send_echo(echo);
            send_telemetry();
        }
        Payload::TransitData(transit_data) => {
            // On transit data update: Update renderer with new data
            info!(
                "WS: Received transit data update: {} lines, {} stops, {} vehicles, {} disruptions",
                transit_data.lines.len(),
                transit_data.stops.len(),
                transit_data.vehicle_movements.len(),
                transit_data.disruptions.len()
            );
            transit_data::on_data(transit_data, payload_len).await;
        }
        Payload::DeviceUpdate(update) => {
            // On device update: Store firmware update info for later use when update command is received
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
                ota::schedule_firmware_update(&update, Duration::from_secs(5 * 60)).await;
            }
            app_settings::session::update_settings(|set| {
                set.firmware_update_available = Some(update);
            })
            .await;
            leds::set_status_led_from_session().await;
        }
        _ => {
            trace::err!("WS: Unhandled server message payload");
        }
    }
    Ok(())
}

async fn ws_send_binary_frame<Tx>(
    tx: &mut Tx,
    payload: &[u8],
    rng: &mut Rng,
) -> Result<(), WsClientError>
where
    Tx: Write<Error = SocketError>,
{
    let header = FrameHeader {
        frame_type: FrameType::Binary(false),
        payload_len: payload.len() as _,
        mask_key: rng.random().into(),
    };

    header
        .send(&mut *tx)
        .await
        .map_err(WsClientError::WsError)?;
    header
        .send_payload(&mut *tx, payload)
        .await
        .map_err(WsClientError::WsError)?;
    debug!("WS: Sent binary frame");

    Ok(())
}

async fn ws_send_pong_frame<Tx>(
    tx: &mut Tx,
    payload: &[u8],
    rng: &mut Rng,
) -> Result<(), WsClientError>
where
    Tx: Write<Error = SocketError>,
{
    let pong_header = FrameHeader {
        frame_type: FrameType::Pong,
        payload_len: payload.len() as _,
        mask_key: rng.random().into(),
    };
    pong_header
        .send(&mut *tx)
        .await
        .map_err(WsClientError::WsError)?;
    pong_header
        .send_payload(&mut *tx, payload)
        .await
        .map_err(WsClientError::WsError)?;
    debug!("WS: Sent pong frame");

    Ok(())
}

async fn ws_close<Tx>(socket: &mut Tx, rng: &mut Rng) -> Result<(), WsClientError>
where
    Tx: Write<Error = SocketError>,
{
    let header = FrameHeader {
        frame_type: FrameType::Close,
        payload_len: 0,
        mask_key: rng.random().into(),
    };

    info!("WS: Sending close frame");

    header
        .send(&mut *socket)
        .await
        .map_err(WsClientError::WsError)?;
    Ok(())
}

async fn build_telemetry(
    controller: &'static SharedWifiController,
) -> Result<Vec<u8>, WsClientError> {
    let transit_data_stats = transit_data::get_stats().await;
    let heap_stats = esp_alloc::HEAP.stats();
    let sessions_settings = app_settings::session::get_settings().await;

    let telemetry_message = ClientMessage {
        version: client_proto::VERSION,
        payload: Some(Payload::Telemetry(DeviceTelemetry {
            uptime_seconds: Instant::now().as_secs() as u32,
            wifi_rssi_dbm: controller.lock().await.rssi().unwrap_or(0),
            current_estimate_milliamps: leds::get_current_estimate_milliamps().await,
            update_progress_percent: sessions_settings.update_progress_percent as u32,
            update_speed_bytes_per_second: sessions_settings.update_speed_bytes_per_sec,
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
            available_vehicle_lines: transit_data_stats.available_vehicle_lines,
            heap_size_bytes: heap_stats.size as u32,
            heap_max_used_bytes: heap_stats.max_usage as u32,
            heap_current_used_bytes: heap_stats.current_usage as u32,
            feed_source: transit_data_stats.feed_source.as_str().into(),
            num_vehicles_visible_real_time: transit_data_stats.num_vehicles_visible_real_time,
            available_disrupted_lines: transit_data_stats.available_disrupted_lines,
            brightness_percent: leds::get_current_brightness_percent().await as u32,
            auto_update_scheduled_unix_timestamp: sessions_settings
                .auto_update_scheduled_unix_timestamp,
        })),
    };
    Ok(telemetry_message.encode_to_vec())
}

async fn build_errors() -> Result<Vec<u8>, WsClientError> {
    let errors = trace::get_errors();
    let unix_timestamp = time::get_unix_timestamp_seconds().await;

    let errors_message = ClientMessage {
        version: client_proto::VERSION,
        payload: Some(Payload::Errors(DeviceErrors {
            errors: errors
                .into_iter()
                .map(|mut e| {
                    e.unix_timestamp = unix_timestamp;
                    e
                })
                .collect(),
        })),
    };
    trace::clear_errors();
    Ok(errors_message.encode_to_vec())
}

async fn build_info() -> Result<Vec<u8>, WsClientError> {
    let sessions_settings = app_settings::session::get_settings().await;
    let device_info_message = ClientMessage {
        version: client_proto::VERSION,
        payload: Some(Payload::DeviceInfo(DeviceInfo {
            firmware_version_major: CONFIG.fw_version.major,
            firmware_version_minor: CONFIG.fw_version.minor,
            firmware_version_patch: CONFIG.fw_version.patch,
            is_beta_firmware: CONFIG.fw_version.beta,
            is_rolled_back_firmware: sessions_settings.is_rolled_back_firmware,
            is_factory_firmware: sessions_settings.is_factory_firmware,
        })),
    };
    Ok(device_info_message.encode_to_vec())
}

async fn build_config() -> Result<Vec<u8>, WsClientError> {
    let config = app_settings::persist::get_settings().await.config;
    let config_message = ClientMessage {
        version: client_proto::VERSION,
        payload: Some(Payload::Config(config)),
    };
    Ok(config_message.encode_to_vec())
}

async fn build_status() -> Result<Vec<u8>, WsClientError> {
    let sta_ssid: String = app_settings::persist::get_settings()
        .await
        .wifi_ssid
        .unwrap_or_default()
        .as_str()
        .into();
    let sessions_settings = app_settings::session::get_settings().await;
    let status_message = ClientMessage {
        version: client_proto::VERSION,
        payload: Some(Payload::Status(DeviceStatus {
            is_light_on: sessions_settings.light_on,
            is_updating_firmware: sessions_settings.updating_firmware,
            has_update_failed: sessions_settings.update_has_failed,
            wifi_ssid: sta_ssid,
        })),
    };
    Ok(status_message.encode_to_vec())
}

async fn build_echo(echo: Echo) -> Result<Vec<u8>, WsClientError> {
    let echo_message = ClientMessage {
        version: client_proto::VERSION,
        payload: Some(Payload::Echo(echo)),
    };
    Ok(echo_message.encode_to_vec())
}

fn try_queue_event(event: WsClientNotify) {
    WS_CLIENT_CHANNEL.try_send(event).ok();
}

pub fn send_status() {
    try_queue_event(WsClientNotify::Status);
}

pub fn send_config() {
    try_queue_event(WsClientNotify::Config);
}

pub fn send_telemetry() {
    try_queue_event(WsClientNotify::Telemetry);
}

pub fn send_errors() {
    try_queue_event(WsClientNotify::Errors);
}

pub fn send_echo(echo: Echo) {
    try_queue_event(WsClientNotify::Echo(echo));
}

pub fn send_info() {
    try_queue_event(WsClientNotify::Info);
}

pub fn send_pong() {
    try_queue_event(WsClientNotify::Pong);
}

pub fn quit(result: Result<(), WsClientError>) {
    WS_CLIENT_QUIT_SIGNAL.signal(result);
}
