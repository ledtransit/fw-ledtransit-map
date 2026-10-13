// WebSocket connection to the gateway: device authentication, then protobuf
// messages both ways
mod auth;
mod confirmation;
mod frames;
mod incoming;
mod outgoing;
mod pending;
mod tls;
mod transit_stream;

use core::{
    convert::Infallible,
    ffi::CStr,
    net::SocketAddr,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::format;
use condtype::{CondType, condval};
use defmt::{debug, error, info, warn};
use edge_http::io::client;
use edge_nal_embassy::{Tcp, TcpBuffers, TcpError};
use edge_nal_tls::TlsConnector;
use edge_ws::{FrameHeader, FrameType};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, Either4, select, select4};
use embassy_net::{Stack, dns};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::{Duration, Instant, TimeoutError, Timer, with_timeout};
use embedded_io_async::{Read, Write};
use envparse::parse_env;
use esp_hal::rng::Rng;
use mbedtls_rs::{Certificate, SavedSession, SessionError, Tls};
use prost::Message;
use smoltcp::wire::DnsQueryType;

use crate::{
    display::leds::{self, LedPixels, LedStatus},
    net::{
        stats::{self, CountingTcp},
        wifi_net::{self, SharedWifiController},
    },
    ota,
    profiling::{self, TimedTcp},
    store::{
        app_settings,
        transit_data::{self, TransitData},
    },
    trace,
};
use client_proto::{ClientMessage, Echo, client_message::Payload};
pub use confirmation::confirm_command;
use pending::{PendingMessages, StateMessage};
use tls::ResumingTls;
use transit_stream::FrameReader;

pub mod client_proto {
    pub const VERSION: u32 = 1;
    include!(concat!(env!("OUT_DIR"), "/ledtransit_client.rs"));
}

// The transit data (about 15 KB) arrives in a few round trips: with 4 KB it
// took about 70 ms longer. Taken from the main stack (12 KB left at its peak)
const TCP_RX_BUF_SIZE: usize = 8 * 1024;
// A whole HTTP request (in one TLS record, see tls::BufferedTlsSocket): else
// the rest of it waits for the server to acknowledge the first part
const TCP_TX_BUF_SIZE: usize = 1536;
const TCP_BUF_POOL_SIZE: usize = 1;

const HTTP_MAX_NUM_HEADERS: usize = 32;
// The HTTP requests and responses connecting, then each message (the config
// the largest, under 1 KB). Transit data is decoded while it's received
// instead, only each of its parts (e.g. a vehicle) needs to fit
const HTTP_WS_RX_BUF_SIZE: usize = 4 * 1024;

const PROTO_TRANSIT_DATA_MAGIC: [u8; 3] = [0x08, 0x01, 0x52]; // First 3 bytes of encoded ClientMessage.TransitData message

const GATEWAY_HOST: &str = match option_env!("GATEWAY_HOST") {
    Some(host) => host,
    None => "gateway.ledtransit.com", // Regional load balancer
};
const GATEWAY_PORT: u16 = parse_env!("GATEWAY_PORT" as u16 else 443);
const SSL_ENABLED: bool = parse_env!("SSL_ENABLED" as bool else true);

const RECONNECT_DELAY: Duration = Duration::from_secs(3);
// Connection failures are expected for a while, e.g. while the gateway
// restarts: only reported once this many attempts in a row failed (about 30 s)
const CONNECT_FAILURES_REPORTED_AFTER: u32 = 10;
// Transit data arrives every 30 s
const FRAME_HEADER_TIMEOUT: Duration = Duration::from_secs(45);
const FRAME_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(10);
// Telemetry while transit data flows, and while it's stale (the gateway stops
// sending it, e.g. with the light off)
const TELEMETRY_INTERVAL_ACTIVE: Duration = Duration::from_secs(15);
const TELEMETRY_INTERVAL_STALE: Duration = Duration::from_secs(5 * 60);

// Messages to send: the latest state (each sent once however often requested
// while pending), and echoes (each sent, in order)
static STATE_MESSAGES: PendingMessages = PendingMessages::new();
static ECHO_CHANNEL: Channel<CriticalSectionRawMutex, Echo, 4> = Channel::new();
static WS_CLIENT_QUIT_SIGNAL: Signal<CriticalSectionRawMutex, Result<(), WsClientError>> =
    Signal::new();
// After a TLS error connecting, the next attempt allows TLS 1.3 (and the other
// way around): for a gateway that refuses TLS 1.2 (see wifi_net::tls_session_config)
static ALLOW_TLS13: AtomicBool = AtomicBool::new(false);

type GatewayTcp<'a> = CountingTcp<Tcp<'a>>;
type HttpsConnection<'a> =
    client::Connection<'a, ResumingTls<'a, TimedTcp<GatewayTcp<'a>>>, HTTP_MAX_NUM_HEADERS>;
type HttpConnection<'a> = client::Connection<'a, GatewayTcp<'a>, HTTP_MAX_NUM_HEADERS>;

type Connection<'a> = CondType<SSL_ENABLED, HttpsConnection<'a>, HttpConnection<'a>>;
type SocketError = CondType<SSL_ENABLED, SessionError, TcpError>;
type HttpError =
    CondType<SSL_ENABLED, edge_http::io::Error<SessionError>, edge_http::io::Error<TcpError>>;
type WsError = CondType<SSL_ENABLED, edge_ws::Error<SessionError>, edge_ws::Error<TcpError>>;

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
    confirmation::spawn(spawner);
}

pub fn send_status() {
    STATE_MESSAGES.request(StateMessage::Status);
}

pub fn send_config() {
    STATE_MESSAGES.request(StateMessage::Config);
}

pub fn send_telemetry() {
    STATE_MESSAGES.request(StateMessage::Telemetry);
}

pub fn send_errors() {
    STATE_MESSAGES.request(StateMessage::Errors);
}

/// Waits while earlier echoes are still being sent.
pub async fn send_echo(echo: Echo) {
    ECHO_CHANNEL.send(echo).await;
}

pub fn send_info() {
    STATE_MESSAGES.request(StateMessage::Info);
}

pub fn send_pong() {
    STATE_MESSAGES.request(StateMessage::Pong);
}

pub fn send_profiling() {
    STATE_MESSAGES.request(StateMessage::Profiling);
}

/// Ends the connection, with the reason to act on.
pub fn quit(result: Result<(), WsClientError>) {
    WS_CLIENT_QUIT_SIGNAL.signal(result);
}

#[embassy_executor::task]
async fn ws_client_task(
    sta_stack: Stack<'static>,
    controller: &'static SharedWifiController,
    tls: &'static Tls<'static>,
    ca_cert: &'static Certificate<'static>,
) {
    // The last connection's TLS session, to resume
    let mut saved_session = None;
    // Attempts in a row that didn't connect
    let mut failed_attempts: u32 = 0;
    loop {
        sta_stack.wait_link_up().await;
        sta_stack.wait_config_up().await;
        profiling::once!(ip_configured);

        // Provisioning still finishing: the credentials and token aren't
        // saved yet, or the link is about to drop
        if wifi_net::is_provisioning() {
            wifi_net::wait_provisioning_finished().await;
            continue;
        }
        info!("Connecting to gateway");
        profiling::count!(gateway_connect_attempts);
        profiling::once!(gateway_connect_started);

        let is_updating = app_settings::session::get_settings()
            .await
            .updating_firmware;
        leds::set_status(if is_updating {
            LedStatus::UpdatingFirmware
        } else {
            LedStatus::ConnectingServer
        });

        let connections = stats::GATEWAY_CONNECTIONS.get();
        let result = run(sta_stack, controller, tls, ca_cert, &mut saved_session).await;
        failed_attempts = if stats::GATEWAY_CONNECTIONS.get() == connections {
            failed_attempts.saturating_add(1)
        } else {
            0
        };
        if on_connection_ended(result, sta_stack, failed_attempts).await {
            Timer::after(RECONNECT_DELAY).await;
        }
    }
}

/// Acts on how the connection ended, and whether to wait before reconnecting.
async fn on_connection_ended(
    result: Result<(), WsClientError>,
    sta_stack: Stack<'static>,
    failed_attempts: u32,
) -> bool {
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
        // Connection lost, or couldn't connect: reported once it keeps failing
        error if is_connection_failure(&error) => {
            info!(
                "Gateway connection failed ({} attempts in a row): {:?}",
                failed_attempts, error
            );
            if failed_attempts == CONNECT_FAILURES_REPORTED_AFTER {
                trace::err!(
                    "Gateway unreachable, {} connection attempts failed: {:?}",
                    failed_attempts,
                    error
                );
            }
            if failed_attempts >= CONNECT_FAILURES_REPORTED_AFTER {
                leds::set_status(LedStatus::ServerError);
            }
        }
        error => {
            trace::err!("WebSocket connection error: {:?}", error);
            leds::set_status(LedStatus::ServerError);
        }
    }
    true
}

// Failed in the network, or the gateway isn't (fully) up: the connection
// refused, reset, closed or timing out, and the gateway refusing requests
// (e.g. its load balancer without a backend)
fn is_connection_failure(error: &WsClientError) -> bool {
    match error {
        WsClientError::DnsError(_)
        | WsClientError::Timeout(TimeoutError)
        | WsClientError::ServerRefused
        | WsClientError::WsError(WsError::Invalid)
        | WsClientError::HttpError(HttpError::ConnectionClosed | HttpError::IncompleteHeaders) => {
            true
        }
        WsClientError::HttpError(HttpError::Io(error))
        | WsClientError::WsError(WsError::Io(error)) => is_network_error(error),
        _ => false,
    }
}

// With TLS, not failed in TLS itself (handshake, alert, certificate)
#[cfg(ssl_enabled)]
fn is_network_error(error: &SocketError) -> bool {
    matches!(error, SessionError::Io(_))
}

#[cfg(not(ssl_enabled))]
fn is_network_error(_: &SocketError) -> bool {
    true
}

// Failed in TLS (handshake, alert, certificate), not in the network
#[cfg(ssl_enabled)]
fn is_tls_error(error: &WsClientError) -> bool {
    match error {
        WsClientError::HttpError(edge_http::io::Error::Io(error))
        | WsClientError::WsError(edge_ws::Error::Io(error))
        | WsClientError::SessionError(error) => wifi_net::is_tls_error(error),
        _ => false,
    }
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
    saved_session: &mut Option<SavedSession>,
) -> Result<(), WsClientError> {
    WS_CLIENT_QUIT_SIGNAL.reset();

    let socket_addr = resolve_gateway(sta_stack).await?;
    profiling::once!(gateway_dns_resolved);
    debug!("Resolved gateway to {} (SSL={})", socket_addr, SSL_ENABLED);

    let tcp_bufs = TcpBuffers::<TCP_BUF_POOL_SIZE, TCP_TX_BUF_SIZE, TCP_RX_BUF_SIZE>::new();
    let tcp = TimedTcp(CountingTcp::new(
        Tcp::new(sta_stack, &tcp_bufs),
        &stats::GATEWAY_TRAFFIC,
    ));
    let host_zstr = format!("{}\0", GATEWAY_HOST);
    let server_name = CStr::from_bytes_with_nul(host_zstr.as_bytes()).unwrap();
    let allow_tls13 = ALLOW_TLS13.load(Ordering::Relaxed);
    let session_config = wifi_net::tls_session_config(ca_cert, server_name, allow_tls13);
    let tls_connector = ResumingTls::new(
        TlsConnector::new(tls.reference(), tcp, &session_config),
        saved_session.take(),
    );

    let mut ws_rx_buf = [0u8; HTTP_WS_RX_BUF_SIZE];
    let mut conn = condval!(if SSL_ENABLED {
        HttpsConnection::new(&mut ws_rx_buf, &tls_connector, socket_addr)
    } else {
        HttpConnection::new(&mut ws_rx_buf, &tcp.0, socket_addr)
    });

    let mut rng = Rng::new();
    let established = async {
        claim_if_just_set_up(&mut conn).await?;
        if !app_settings::persist::get_settings().await.claimed {
            error!("Device not claimed, cannot establish WebSocket connection");
            return Err(WsClientError::Unlinked);
        }
        auth::websocket_authenticate(GATEWAY_HOST, &mut conn, &mut rng).await
    }
    .await;
    #[cfg(ssl_enabled)]
    if let Err(error) = &established
        && is_tls_error(error)
    {
        ALLOW_TLS13.store(!allow_tls13, Ordering::Relaxed);
        warn!(
            "TLS error connecting with TLS {}, trying TLS {} next",
            if allow_tls13 { "1.3" } else { "1.2" },
            if allow_tls13 { "1.2" } else { "1.3" }
        );
    }
    established?;
    profiling::once!(gateway_ws_established);
    stats::on_gateway_connected();
    info!("WebSocket connection established");

    // Before the first transit data since boot: fade out what's shown
    // (startup animation, demo) already, while the data is on its way. On a
    // reconnect, the map keeps showing the data it has
    if !transit_data::is_set().await {
        leds::set_pixels(LedPixels::FadeOut).await;
    }

    let (mut socket, buf) = conn.release();
    #[cfg(ssl_enabled)]
    let session = socket.session_mut();
    #[cfg(ssl_enabled)]
    {
        *saved_session = session.save().ok();
    }
    #[cfg(ssl_enabled)]
    let (mut rx, mut tx) = session.split().await.map_err(WsClientError::SessionError)?;
    #[cfg(not(ssl_enabled))]
    let (mut rx, mut tx) = {
        use edge_nal::TcpSplit;
        socket.split()
    };

    STATE_MESSAGES.clear();
    ECHO_CHANNEL.clear();
    send_status();

    // Until the connection closes, fails, or the application quits it
    let result = match select4(
        receive_messages(&mut rx, buf),
        send_pending_messages(&mut tx, controller),
        send_telemetry_periodically(),
        WS_CLIENT_QUIT_SIGNAL.wait(),
    )
    .await
    {
        Either4::First(result) | Either4::Second(result) | Either4::Fourth(result) => result,
        Either4::Third(never) => match never {},
    };
    info!("WebSocket connection closed, shutting down ({:?})", result);
    stats::on_gateway_disconnected();
    // A command can't be confirmed any more once the connection it came over
    // ended: it would quit a later connection
    confirmation::cancel().await;

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
        let header_received = profiling::now();
        let header_received_at = Instant::now();
        let mut reader = FrameReader::new(&mut *rx, &header, buf);

        match header.frame_type {
            FrameType::Binary(_) => {
                // Handled once received: only receiving it has the timeout
                let payload_len = reader.remaining();
                let mut is_transit_data = false;
                let received = with_timeout(
                    FRAME_PAYLOAD_TIMEOUT,
                    receive_binary(&mut reader, &mut is_transit_data),
                )
                .await
                .map_err(WsClientError::Timeout)
                .and_then(|result| result);
                if received.is_err() && is_transit_data {
                    stats::TRANSIT_DATA_ERRORS.inc();
                }
                let received_in = header_received_at.elapsed();
                match received? {
                    Received::Message(message) => {
                        incoming::handle_message(message, payload_len, received_in).await
                    }
                    Received::TransitData(data) => {
                        profiling::once!(first_transit_data_header_received, header_received);
                        profiling::once!(first_transit_data_payload_received);
                        incoming::on_transit_data(data, payload_len, received_in).await
                    }
                    Received::Ignored => {}
                }
            }
            FrameType::Ping => {
                debug!("WS: Got ping, sending pong");
                with_timeout(FRAME_PAYLOAD_TIMEOUT, reader.skip(reader.remaining()))
                    .await
                    .map_err(WsClientError::Timeout)??;
                send_pong();
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

enum Received {
    Message(Payload),
    TransitData(TransitData),
    Ignored,
}

// Transit data decoded while it's received, any other message once it's
// received whole (into the buffer). Tells if it's transit data as soon as
// known, also if receiving it then fails
async fn receive_binary<R>(
    reader: &mut FrameReader<'_, R>,
    is_transit_data: &mut bool,
) -> Result<Received, WsClientError>
where
    R: Read<Error = SocketError>,
{
    let payload_len = reader.remaining();
    let magic_len = PROTO_TRANSIT_DATA_MAGIC.len().min(payload_len);
    if reader.peek(magic_len).await? != PROTO_TRANSIT_DATA_MAGIC {
        let payload = reader.take(payload_len).await?;
        return decode_message(payload).map(Received::Message);
    }
    *is_transit_data = true;

    if !prepare_for_transit_data().await {
        reader.skip(payload_len).await?;
        return Ok(Received::Ignored);
    }
    reader.skip(magic_len).await?;
    transit_stream::decode_transit_data(reader)
        .await
        .map(Received::TransitData)
}

/// False if transit data is ignored for now. Else the previous data stays
/// until the new one is decoded (both fit, compact): it's still shown
/// meanwhile, while the new one is received.
async fn prepare_for_transit_data() -> bool {
    let session_settings = app_settings::session::get_settings().await;
    if session_settings.updating_firmware {
        // The update needs the memory
        return false;
    }
    // The LED test mode doesn't show transit data
    if session_settings.test_mode_active {
        transit_data::clear().await;
        return false;
    }
    true
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

async fn send_pending_messages<W>(
    tx: &mut W,
    controller: &'static SharedWifiController,
) -> Result<(), WsClientError>
where
    W: Write<Error = SocketError>,
{
    let mut rng = Rng::new();
    loop {
        let state_message = match select(ECHO_CHANNEL.receive(), STATE_MESSAGES.next()).await {
            Either::First(echo) => {
                frames::send_binary(tx, &outgoing::echo(echo), &mut rng).await?;
                continue;
            }
            Either::Second(state_message) => state_message,
        };
        let message = match state_message {
            StateMessage::Pong => {
                frames::send_pong(tx, &mut rng).await?;
                continue;
            }
            StateMessage::Errors if trace::get_errors().is_empty() => continue,
            StateMessage::Errors => outgoing::errors().await,
            StateMessage::Status => outgoing::status().await,
            StateMessage::Config => outgoing::config().await,
            StateMessage::Telemetry => outgoing::telemetry(controller).await,
            StateMessage::Info => outgoing::info().await,
            StateMessage::Profiling => match outgoing::profiling() {
                Some(message) => message,
                None => continue,
            },
        };
        frames::send_binary(tx, &message, &mut rng).await?;
    }
}

/// Telemetry while connected, besides what's sent on changes.
async fn send_telemetry_periodically() -> Infallible {
    loop {
        let interval = if transit_data::is_stale().await {
            TELEMETRY_INTERVAL_STALE
        } else {
            TELEMETRY_INTERVAL_ACTIVE
        };
        // On a change, starts over with the other interval
        if let Either::First(()) =
            select(Timer::after(interval), transit_data::wait_stale_changed()).await
        {
            send_telemetry();
        }
    }
}
