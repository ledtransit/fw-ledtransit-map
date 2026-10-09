// WebSocket connection to the gateway: device authentication, then protobuf
// messages both ways
mod auth;
mod frames;
mod incoming;
mod outgoing;

use core::{ffi::CStr, net::SocketAddr};

use alloc::format;
use condtype::{CondType, condval};
use defmt::{debug, error, info, warn};
use edge_http::io::client;
use edge_nal_embassy::{Tcp, TcpBuffers, TcpError};
use edge_nal_tls::TlsConnector;
use edge_ws::{FrameHeader, FrameType};
use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_net::{Stack, dns};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::{Duration, TimeoutError, Timer, with_timeout};
#[cfg(ssl_enabled)]
use embedded_io_async::ErrorKind;
use embedded_io_async::{Read, Write};
use envparse::parse_env;
use esp_hal::rng::Rng;
use mbedtls_rs::{Certificate, ClientSessionConfig, SessionError, Tls};
use prost::Message;
use smoltcp::wire::DnsQueryType;

use crate::{
    display::leds::{self, LedStatus},
    net::wifi_net::{self, SharedWifiController},
    ota,
    store::{app_settings, transit_data},
    trace,
};
use client_proto::{ClientMessage, Echo, client_message::Payload};

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

const GATEWAY_HOST: &str = match option_env!("GATEWAY_HOST") {
    Some(host) => host,
    None => "gateway.ledtransit.com", // Regional load balancer
};
const GATEWAY_PORT: u16 = parse_env!("GATEWAY_PORT" as u16 else 443);
const SSL_ENABLED: bool = parse_env!("SSL_ENABLED" as bool else true);

const RECONNECT_DELAY: Duration = Duration::from_secs(3);
// Transit data arrives every 30 s
const FRAME_HEADER_TIMEOUT: Duration = Duration::from_secs(45);
const FRAME_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(10);

static WS_CLIENT_CHANNEL: Channel<CriticalSectionRawMutex, WsClientNotify, 10> = Channel::new();
static WS_CLIENT_QUIT_SIGNAL: Signal<CriticalSectionRawMutex, Result<(), WsClientError>> =
    Signal::new();

type HttpsConnection<'a> = client::Connection<'a, TlsConnector<'a, Tcp<'a>>, HTTP_MAX_NUM_HEADERS>;
type HttpConnection<'a> = client::Connection<'a, Tcp<'a>, HTTP_MAX_NUM_HEADERS>;

type Connection<'a> = CondType<SSL_ENABLED, HttpsConnection<'a>, HttpConnection<'a>>;
type SocketError = CondType<SSL_ENABLED, SessionError, TcpError>;
type HttpError =
    CondType<SSL_ENABLED, edge_http::io::Error<SessionError>, edge_http::io::Error<TcpError>>;
type WsError = CondType<SSL_ENABLED, edge_ws::Error<SessionError>, edge_ws::Error<TcpError>>;

// Messages to send
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
    /// The gateway refused a request (other than authentication)
    ServerRefused,
    /// No account has this device (any more): set up again
    Unlinked,
    /// The device is blocked from connecting
    Revoked,
    /// The device keys aren't burned: not provisioned at production
    NoDeviceKeys,
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

/// Ends the connection, with the reason to act on.
pub fn quit(result: Result<(), WsClientError>) {
    WS_CLIENT_QUIT_SIGNAL.signal(result);
}

// Dropped when the queue is full
fn try_queue_event(event: WsClientNotify) {
    WS_CLIENT_CHANNEL.try_send(event).ok();
}

#[embassy_executor::task]
async fn ws_client_task(
    sta_stack: Stack<'static>,
    controller: &'static SharedWifiController,
    tls: &'static Tls<'static>,
    ca_cert: &'static Certificate<'static>,
) {
    loop {
        sta_stack.wait_link_up().await;
        sta_stack.wait_config_up().await;

        // Provisioning still finishing: the link is about to drop, a
        // connection started over it now would fail
        if wifi_net::is_provisioning() {
            Timer::after(Duration::from_millis(250)).await;
            continue;
        }
        info!("Connecting to gateway");

        let is_updating = app_settings::session::get_settings()
            .await
            .updating_firmware;
        leds::set_status(if is_updating {
            LedStatus::UpdatingFirmware
        } else {
            LedStatus::ConnectingServer
        });

        let result = run(sta_stack, controller, tls, ca_cert).await;
        if on_connection_ended(result, sta_stack).await {
            Timer::after(RECONNECT_DELAY).await;
        }
    }
}

/// Acts on how the connection ended, and whether to wait before reconnecting.
async fn on_connection_ended(result: Result<(), WsClientError>, sta_stack: Stack<'static>) -> bool {
    let error = match result {
        Ok(()) => {
            info!("WebSocket task ended normally");
            return true;
        }
        Err(error) => error,
    };

    match error {
        WsClientError::AuthFailed => {
            trace::err!("WebSocket authentication failed");
            leds::set_status(LedStatus::AuthError);
        }
        WsClientError::Revoked => {
            trace::err!("Device is blocked from connecting");
            leds::set_status(LedStatus::AuthError);
        }
        WsClientError::NoDeviceKeys => {
            trace::err!("Device keys missing, can't authenticate");
            leds::set_status(LedStatus::AuthError);
        }
        WsClientError::Unlinked => {
            info!("Device no longer linked to an account, restarting WiFi provisioning");
            restart_provisioning(sta_stack).await;
        }
        WsClientError::RestartProvisioning => {
            info!("Restarting WiFi provisioning as requested by WS user");
            restart_provisioning(sta_stack).await;
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
            return false;
        }
        // Connection lost
        WsClientError::WsError(WsError::Invalid) | WsClientError::Timeout(TimeoutError) => {}
        // Only a TLS session error with TLS (builds without it, e.g. for a
        // local gateway, would not compile)
        #[cfg(ssl_enabled)]
        WsClientError::WsError(WsError::Io(SessionError::Io(ErrorKind::Other))) => {}
        error => {
            trace::err!("WebSocket connection error: {:?}", error);
            leds::set_status(LedStatus::ServerError);
        }
    }
    true
}

async fn restart_provisioning(sta_stack: Stack<'static>) {
    wifi_net::start_provisioning().await;
    if sta_stack.is_link_up() {
        sta_stack.wait_config_down().await;
    }
}

async fn run(
    sta_stack: Stack<'static>,
    controller: &'static SharedWifiController,
    tls: &Tls<'static>,
    ca_cert: &'static Certificate<'static>,
) -> Result<(), WsClientError> {
    WS_CLIENT_QUIT_SIGNAL.reset();

    let socket_addr = resolve_gateway(sta_stack).await?;
    debug!("Resolved gateway to {} (SSL={})", socket_addr, SSL_ENABLED);

    let tcp_bufs = TcpBuffers::<TCP_BUF_POOL_SIZE, TCP_TX_BUF_SIZE, TCP_RX_BUF_SIZE>::new();
    let tcp = Tcp::new(sta_stack, &tcp_bufs);
    let host_zstr = format!("{}\0", GATEWAY_HOST);
    let session_config = ClientSessionConfig {
        ca_chain: Some(ca_cert.clone()),
        server_name: Some(CStr::from_bytes_with_nul(host_zstr.as_bytes()).unwrap()),
        ..ClientSessionConfig::new()
    };
    let tls_connector = TlsConnector::new(tls.reference(), tcp, &session_config);

    let mut ws_rx_buf = [0u8; HTTP_WS_RX_BUF_SIZE];
    let mut conn = condval!(if SSL_ENABLED {
        HttpsConnection::new(&mut ws_rx_buf, &tls_connector, socket_addr)
    } else {
        client::Connection::<Tcp, HTTP_MAX_NUM_HEADERS>::new(&mut ws_rx_buf, &tcp, socket_addr)
    });

    claim_if_just_set_up(&mut conn).await?;
    if !app_settings::persist::get_settings().await.claimed {
        error!("Device not claimed, cannot establish WebSocket connection");
        return Err(WsClientError::Unlinked);
    }

    let mut rng = Rng::new();
    auth::websocket_authenticate(GATEWAY_HOST, &mut conn, &mut rng).await?;
    info!("WebSocket connection established");

    let (mut socket, buf) = conn.release();
    #[cfg(ssl_enabled)]
    let session = socket.session_mut();
    #[cfg(ssl_enabled)]
    let (mut rx, mut tx) = session.split().await.map_err(WsClientError::SessionError)?;
    #[cfg(not(ssl_enabled))]
    let (mut rx, mut tx) = {
        use edge_nal::TcpSplit;
        socket.split()
    };

    WS_CLIENT_CHANNEL.clear();
    send_status();

    // Until the connection closes, fails, or the application quits it
    let result = match select3(
        receive_messages(&mut rx, buf),
        send_queued_messages(&mut tx, controller),
        WS_CLIENT_QUIT_SIGNAL.wait(),
    )
    .await
    {
        Either3::First(result) | Either3::Second(result) | Either3::Third(result) => result,
    };
    info!("WebSocket connection closed, shutting down ({:?})", result);

    with_timeout(Duration::from_secs(1), async {
        frames::send_close(&mut tx, &mut rng).await.ok();
        tx.flush().await.ok();
    })
    .await
    .ok();
    drop((rx, tx));

    with_timeout(Duration::from_secs(1), async {
        #[cfg(ssl_enabled)]
        session.close().await.ok();
        #[cfg(not(ssl_enabled))]
        {
            use edge_nal::{Close, TcpShutdown};
            socket.close(Close::Both).await.ok();
        }
    })
    .await
    .ok();

    info!("WebSocket connection closed");
    result
}

async fn resolve_gateway(sta_stack: Stack<'static>) -> Result<SocketAddr, WsClientError> {
    let ip_addr = *sta_stack
        .dns_query(GATEWAY_HOST, DnsQueryType::A)
        .await
        .map_err(WsClientError::DnsError)?
        .first()
        .ok_or(WsClientError::DnsError(dns::Error::Failed))?;
    Ok(SocketAddr::new(ip_addr.into(), GATEWAY_PORT))
}

/// Just set up: claims the device into its user's account first.
async fn claim_if_just_set_up(conn: &mut Connection<'_>) -> Result<(), WsClientError> {
    let Some(prov_token) = app_settings::persist::get_settings().await.prov_token else {
        return Ok(());
    };
    match auth::claim(conn, &prov_token, GATEWAY_HOST).await {
        Err(WsClientError::AuthFailed) => {
            warn!("Claim refused, clearing provisioning token");
            app_settings::persist::update_settings(|set| {
                set.prov_token = None;
            })
            .await;
            Err(WsClientError::RestartProvisioning)
        }
        result => result,
    }
}

async fn receive_messages<R>(rx: &mut R, buf: &mut [u8]) -> Result<(), WsClientError>
where
    R: Read<Error = SocketError>,
{
    loop {
        let header = with_timeout(FRAME_HEADER_TIMEOUT, FrameHeader::recv(&mut *rx))
            .await
            .map_err(WsClientError::Timeout)?
            .map_err(WsClientError::WsError)?;
        let payload = with_timeout(FRAME_PAYLOAD_TIMEOUT, header.recv_payload(&mut *rx, buf))
            .await
            .map_err(WsClientError::Timeout)?
            .map_err(WsClientError::WsError)?;

        match header.frame_type {
            FrameType::Binary(_) => {
                if payload.starts_with(&PROTO_TRANSIT_DATA_MAGIC)
                    && !prepare_for_transit_data().await
                {
                    continue;
                }
                let message = decode_message(payload)?;
                incoming::handle_message(message, payload.len()).await;
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
}

/// Frees the previous transit data before decoding new data, so both don't
/// have to fit into memory at once. False if transit data is ignored for now.
async fn prepare_for_transit_data() -> bool {
    let session_settings = app_settings::session::get_settings().await;
    if session_settings.updating_firmware {
        // The update needs the memory
        return false;
    }
    transit_data::clear().await;
    // The LED test mode doesn't show transit data
    !session_settings.test_mode_active
}

fn decode_message(payload: &[u8]) -> Result<Payload, WsClientError> {
    let message = ClientMessage::decode(payload).map_err(|_| WsClientError::DataError)?;
    if message.version != client_proto::VERSION {
        trace::err!(
            "WS: Version mismatch (got {}, expected {}), closing connection",
            message.version,
            client_proto::VERSION
        );
        return Err(WsClientError::DataError);
    }
    message.payload.ok_or(WsClientError::DataError)
}

async fn send_queued_messages<W>(
    tx: &mut W,
    controller: &'static SharedWifiController,
) -> Result<(), WsClientError>
where
    W: Write<Error = SocketError>,
{
    let mut rng = Rng::new();
    loop {
        let message = match WS_CLIENT_CHANNEL.receive().await {
            WsClientNotify::Pong => {
                frames::send_pong(tx, &mut rng).await?;
                continue;
            }
            WsClientNotify::Errors if trace::get_errors().is_empty() => continue,
            WsClientNotify::Errors => outgoing::errors().await,
            WsClientNotify::Status => outgoing::status().await,
            WsClientNotify::Config => outgoing::config().await,
            WsClientNotify::Telemetry => outgoing::telemetry(controller).await,
            WsClientNotify::Echo(echo) => outgoing::echo(echo),
            WsClientNotify::Info => outgoing::info().await,
        };
        frames::send_binary(tx, &message, &mut rng).await?;
    }
}
