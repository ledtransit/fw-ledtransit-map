// WiFi: station to the user's network, and the map's own network in setup mode
use core::{
    ffi::CStr,
    fmt::Write,
    net::Ipv4Addr,
    sync::atomic::{AtomicBool, Ordering},
};

use defmt::{error, info};
use embassy_executor::Spawner;
use embassy_net::{DhcpConfig, Ipv4Cidr, Runner, StackResources, StaticConfigV4};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Timer};
use esp_hal::{
    peripherals::{SHA, WIFI},
    ram,
    rng::{Rng, Trng},
};
use esp_radio::wifi::{
    AuthenticationMethod, AuthenticationMethodConfig, Config, ConnectionError, ControllerConfig,
    DisconnectReason, Interface, Password, Ssid, WifiController,
    ap::AccessPointConfig,
    event::{EventInfo, EventSubscriber},
    sta::{ConnectedInfo, DisconnectedInfo, StationConfig},
};
use esp_wifi_sys::include::{
    esp_wifi_get_config, esp_wifi_set_config, esp_wifi_set_mode, esp_wifi_stop,
    wifi_auth_mode_t_WIFI_AUTH_WPA2_PSK, wifi_config_t, wifi_interface_t_WIFI_IF_STA,
    wifi_mode_t_WIFI_MODE_NULL, wifi_mode_t_WIFI_MODE_STA,
};
use mbedtls_rs::{Certificate, ClientSessionConfig, SessionError, Tls, TlsVersion, X509};

use crate::{
    display::leds::{self, LedPixels, LedStatus},
    mk_static,
    net::{provisioning, stats, wifi_psk, ws_client},
    ota, profiling,
    store::{
        SharedFlashStorage,
        app_settings::{self, persist::PersistSettings},
        transit_data,
    },
};

const STA_SOCKET_COUNT: usize = 4;
const AP_GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
const CONNECT_RETRY_DELAY: Duration = Duration::from_secs(3);
const CONNECTION_CHECK_INTERVAL: Duration = Duration::from_secs(5);
// Failed connections in a row after which the WiFi restarts: clears the
// driver's connection state, and calibrates the radio again
const RESTART_AFTER_FAILURES: u32 = 3;
// Failed connections in a row with the network there (not the network
// missing) after which the device reboots: what fixed connections that kept
// failing (handshake timeouts) otherwise. At most this many reboots in a row,
// until connected: the password may just have changed
const REBOOT_AFTER_FAILURES: u32 = 9;
const MAX_RECOVERY_REBOOTS: u32 = 2;
// Elliptic-curve operations in a row in a TLS handshake (about 10 ms for P-256)
const TLS_ECP_MAX_OPS: u32 = 200;

pub const CA_BUNDLE: &CStr = match CStr::from_bytes_with_nul(
    concat!(include_str!("../../assets/certs/ca-bundle.pem"), "\0").as_bytes(),
) {
    Ok(bundle) => bundle,
    _ => panic!("CA bundle is not a valid text file"),
};

pub type SharedWifiController = Mutex<CriticalSectionRawMutex, WifiController<'static>>;

enum WifiNetEvent {
    StartProvisioning,
    FinishProvisioning,
    ConnectToAp,
}

static WIFI_NET_SIGNAL: Signal<CriticalSectionRawMutex, WifiNetEvent> = Signal::new();

// Set from the start of provisioning until the station connected with the
// credentials saved, and while the WiFi restarts as station only (if closing
// the setup network failed), which drops the station's link
static PROVISIONING: AtomicBool = AtomicBool::new(false);
static PROVISIONING_FINISHED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

// Recovery reboots in a row (see REBOOT_AFTER_FAILURES), kept across resets
// until connected: a count and its complement, RTC memory holds garbage after
// power on, which doesn't pass for one
#[ram(unstable(rtc_fast, persistent))]
static mut RECOVERY_REBOOTS: [u32; 2] = [0, !0];

// Set once connected with WPA2: the key is derived from the password, unless
// stored already (see wifi_psk)
static PSK_WANTED: AtomicBool = AtomicBool::new(false);

/// Whether provisioning is still going on: the credentials aren't saved yet,
/// or the station's link (if up) is about to drop, so nothing should connect
/// over it yet.
pub fn is_provisioning() -> bool {
    PROVISIONING.load(Ordering::Relaxed)
}

/// Waits until provisioning (maybe) finished: check with `is_provisioning`.
pub async fn wait_provisioning_finished() {
    PROVISIONING_FINISHED.wait().await
}

fn set_provisioning(provisioning: bool) {
    PROVISIONING.store(provisioning, Ordering::Relaxed);
    if !provisioning {
        PROVISIONING_FINISHED.signal(());
    }
}

pub async fn spawn(
    spawner: Spawner,
    wifi_peri: WIFI<'static>,
    sha_peri: SHA<'static>,
    flash_store: &'static SharedFlashStorage,
) {
    let controller = WifiController::new(
        wifi_peri,
        ControllerConfig::default()
            .with_rx_queue_size(4)
            .with_tx_queue_size(2)
            .with_static_rx_buf_num(6)
            .with_dynamic_rx_buf_num(12)
            .with_dynamic_tx_buf_num(12)
            .with_ampdu_tx_enable(true)
            .with_ampdu_rx_enable(true)
            .with_rx_ba_win(4),
    )
    .unwrap();
    profiling::once!(wifi_radio_initialized);
    let wifi_ap_device = Interface::access_point();
    let wifi_sta_device = Interface::station();

    // Also the setup network's SSID
    let device_name = mk_static!(
        heapless::String<32>,
        device_name(wifi_ap_device.mac_address())
    );

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let ap_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(AP_GATEWAY_IP, 24),
        gateway: Some(AP_GATEWAY_IP),
        dns_servers: Default::default(),
    });
    let (ap_stack, ap_runner) = embassy_net::new(
        wifi_ap_device,
        ap_config,
        mk_static!(
            StackResources<{ provisioning::SOCKET_COUNT }>,
            StackResources::<{ provisioning::SOCKET_COUNT }>::new()
        ),
        seed,
    );

    let mut dhcp_config = DhcpConfig::default();
    dhcp_config.hostname = Some(heapless::String::try_from(device_name.as_str()).unwrap());
    let (sta_stack, sta_runner) = embassy_net::new(
        wifi_sta_device,
        embassy_net::Config::dhcpv4(dhcp_config),
        mk_static!(
            StackResources<STA_SOCKET_COUNT>,
            StackResources::<STA_SOCKET_COUNT>::new()
        ),
        seed,
    );

    let shared_controller = mk_static!(SharedWifiController, Mutex::new(controller));

    // TLS for HTTPS and WSS
    let trng = mk_static!(Trng, Trng::try_new().unwrap());
    mbedtls_rs::ecp::set_restartable_max_ops(TLS_ECP_MAX_OPS);
    let tls = mk_static!(Tls, Tls::new(trng).unwrap());
    let ca_cert = mk_static!(Certificate<'static>, {
        Certificate::new(X509::PEM(CA_BUNDLE)).expect("Failed to parse CA bundle")
    });
    profiling::once!(tls_initialized);

    spawner.spawn(net_stack_task(ap_runner).unwrap());
    spawner.spawn(net_stack_task(sta_runner).unwrap());
    provisioning::spawn(spawner, ap_stack, AP_GATEWAY_IP, shared_controller);
    ws_client::spawn(spawner, sta_stack, shared_controller, tls, ca_cert);
    ota::spawn(spawner, sta_stack, tls, ca_cert, flash_store, sha_peri);

    // Start in station mode
    let settings = app_settings::persist::get_settings().await;
    shared_controller
        .lock()
        .await
        .set_config(&Config::Station(stored_station_config(&settings)))
        .expect("Failed to set STA config");
    profiling::once!(station_configured);

    spawner.spawn(wifi_net_task(shared_controller, device_name).unwrap());
    spawner.spawn(wifi_conn_task(shared_controller).unwrap());
    spawner.spawn(wifi_events_task(shared_controller).unwrap());

    info!("WiFi network stack initialized");
}

pub async fn start_provisioning() {
    app_settings::persist::update_settings(|set| set.clear_wifi_credentials_and_auth()).await;
    ws_client::quit(Ok(()));
    transit_data::reset().await;
    WIFI_NET_SIGNAL.signal(WifiNetEvent::StartProvisioning);
    leds::set_pixels(LedPixels::FadeOut).await;
    leds::wait_pixels_animation_complete().await;
    leds::set_pixels(LedPixels::DemoMode).await;
}

/// The station connected during setup, with the credentials and token saved:
/// the gateway connection may start over it, while the setup network is
/// still open. It stays connected when that closes.
pub fn setup_station_connected(connected: &ConnectedInfo) {
    station_connected(connected);
    set_provisioning(false);
}

fn station_connected(connected: &ConnectedInfo) {
    set_recovery_reboots(0);
    let is_wpa2 = matches!(
        connected.authmode,
        AuthenticationMethod::Wpa
            | AuthenticationMethod::Wpa2Personal
            | AuthenticationMethod::WpaWpa2Personal
    );
    PSK_WANTED.store(is_wpa2, Ordering::Relaxed);
}

pub fn finish_provisioning() {
    WIFI_NET_SIGNAL.signal(WifiNetEvent::FinishProvisioning);
}

pub fn connect_ap() {
    WIFI_NET_SIGNAL.signal(WifiNetEvent::ConnectToAp);
}

// "LEDTransit-" and the last 3 bytes of the MAC address
fn device_name(mac_address: [u8; 6]) -> heapless::String<32> {
    let mut name = heapless::String::<32>::new();
    write!(
        name,
        "LEDTransit-{:02X}{:02X}{:02X}",
        mac_address[3], mac_address[4], mac_address[5]
    )
    .expect("Failed to write SSID");
    name
}

/// TLS 1.2 unless `allow_tls13`: only there mbedtls splits the handshake's
/// elliptic-curve math (ECDHE-ECDSA) into slices, instead of blocking
/// everything (also the LEDs) for about a second. Connections fall back to TLS
/// 1.3 for servers that refuse 1.2.
pub fn tls_session_config<'a>(
    ca_cert: &Certificate<'a>,
    server_name: &'a CStr,
    allow_tls13: bool,
) -> ClientSessionConfig<'a> {
    ClientSessionConfig {
        ca_chain: Some(ca_cert.clone()),
        server_name: Some(server_name),
        max_version: (!allow_tls13).then_some(TlsVersion::Tls1_2),
        ..ClientSessionConfig::new()
    }
}

/// A TLS error (handshake, alert, certificate), not a network one.
pub fn is_tls_error(error: &SessionError) -> bool {
    matches!(error, SessionError::MbedTls(_))
}

/// The station config to connect with: WPA2 (or better) with the password.
pub fn station_config(ssid: &str, password: &str) -> StationConfig {
    // The credentials always fit: they're kept as at most 32 and 64 bytes
    StationConfig::default()
        .with_ssid(Ssid::try_from(ssid).expect("SSID too long"))
        .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
            Password::try_from(password).expect("Password too long"),
        ))
}

/// The setup network: open, named after the device.
pub fn access_point_config(ssid: &str) -> AccessPointConfig {
    AccessPointConfig::default().with_ssid(Ssid::try_from(ssid).expect("SSID too long"))
}

/// The station config with the stored credentials: with the key derived from
/// the password in its place, if stored.
fn stored_station_config(settings: &PersistSettings) -> StationConfig {
    let ssid = settings.wifi_ssid.as_deref().unwrap_or_default();
    match &settings.wifi_psk {
        Some(psk) => station_config(ssid, &wifi_psk::psk_hex(psk)),
        None => station_config(ssid, settings.wifi_password.as_deref().unwrap_or_default()),
    }
}

#[embassy_executor::task]
async fn wifi_net_task(
    controller: &'static SharedWifiController,
    ap_ssid: &'static heapless::String<32>,
) {
    // Failed connections in a row, and those with the network there
    let mut failures = 0;
    let mut failures_with_network = 0;
    loop {
        match WIFI_NET_SIGNAL.wait().await {
            WifiNetEvent::StartProvisioning => start_setup_network(controller, ap_ssid).await,
            WifiNetEvent::FinishProvisioning => finish_setup_network(controller).await,
            WifiNetEvent::ConnectToAp => {
                let Err(error) = connect_to_ap(controller).await else {
                    failures = 0;
                    failures_with_network = 0;
                    continue;
                };
                failures += 1;
                if !is_network_missing(&error) {
                    failures_with_network += 1;
                }
                if failures_with_network >= REBOOT_AFTER_FAILURES && take_recovery_reboot() {
                    error!("WiFi keeps failing to connect, rebooting");
                    Timer::after(Duration::from_millis(100)).await;
                    esp_hal::system::software_reset();
                }
                if failures % RESTART_AFTER_FAILURES == 0 {
                    restart_wifi(controller).await;
                }
                Timer::after(CONNECT_RETRY_DELAY).await;
                if !WIFI_NET_SIGNAL.signaled() {
                    connect_ap();
                }
            }
        }
    }
}

async fn start_setup_network(
    controller: &'static SharedWifiController,
    ap_ssid: &'static heapless::String<32>,
) {
    info!("Starting WiFi provisioning mode");
    set_provisioning(true);
    leds::set_status(LedStatus::Pairing);

    if controller.lock().await.is_connected() {
        info!("Disconnecting from current WiFi network");
        controller.lock().await.disconnect_async().await.ok();
    }

    // Access point and station: the station connects to the user's network
    // during setup
    controller
        .lock()
        .await
        .set_config(&Config::AccessPointStation(
            StationConfig::default(),
            access_point_config(ap_ssid),
        ))
        .expect("Failed to set AP+STA config");
}

/// Closes the setup network. The station, connected during setup, stays
/// connected.
async fn finish_setup_network(controller: &'static SharedWifiController) {
    info!("Finishing WiFi provisioning mode");
    {
        let controller = controller.lock().await;
        if controller.is_connected() && close_access_point(&controller).is_ok() {
            info!("Setup network closed, WiFi stays connected");
            set_provisioning(false);
            return;
        }
    }

    // Else start over as station only: restarts the WiFi, the station
    // connects again
    set_provisioning(true);
    let settings = app_settings::persist::get_settings().await;
    controller
        .lock()
        .await
        .set_config(&Config::Station(stored_station_config(&settings)))
        .expect("Failed to set STA config");
    connect_ap();
}

/// Switches from access point and station to station only, without stopping
/// the WiFi like `set_config` does (which drops the station's connection).
/// The driver stops the access point (its stop event updates the controller's
/// state), the station's config is kept.
fn close_access_point(_controller: &WifiController<'static>) -> Result<(), ()> {
    // SAFETY: Called with the controller locked, the WiFi started
    let result = unsafe { esp_wifi_set_mode(wifi_mode_t_WIFI_MODE_STA) };
    if result != 0 {
        error!("Failed to close the setup network: {}", result);
        return Err(());
    }
    Ok(())
}

/// Sets the station's credentials (as `station_config`) while the setup
/// network is open, leaving that as it is: `set_config` applies its config too,
/// which restarts it (the driver never takes it for unchanged), dropping the
/// phone on the setup page.
pub fn set_setup_station_credentials(
    _controller: &WifiController<'static>,
    ssid: &str,
    password: &str,
) -> Result<(), i32> {
    // SAFETY: Called with the controller locked, the WiFi started. The rest of
    // the station's config stays as the controller set it
    let result = unsafe {
        let mut config: wifi_config_t = core::mem::zeroed();
        let result = esp_wifi_get_config(wifi_interface_t_WIFI_IF_STA, &mut config);
        if result != 0 {
            return Err(result);
        }
        // The credentials always fit: they're kept as at most 32 and 64 bytes
        config.sta.ssid = [0; 32];
        config.sta.ssid[..ssid.len()].copy_from_slice(ssid.as_bytes());
        config.sta.password = [0; 64];
        config.sta.password[..password.len()].copy_from_slice(password.as_bytes());
        config.sta.threshold.authmode = wifi_auth_mode_t_WIFI_AUTH_WPA2_PSK;
        esp_wifi_set_config(wifi_interface_t_WIFI_IF_STA, &mut config)
    };
    if result != 0 {
        return Err(result);
    }
    Ok(())
}

/// Connects the station, if not connected.
async fn connect_to_ap(controller: &'static SharedWifiController) -> Result<(), ConnectionError> {
    info!("Connecting to WiFi access point");
    leds::set_status(LedStatus::ConnectingWifi);

    if controller.lock().await.is_connected() {
        info!("WiFi already connected");
        set_provisioning(false);
        return Ok(());
    }

    profiling::count!(wifi_connect_attempts);
    profiling::once!(wifi_connect_started);
    let result = controller.lock().await.connect_async().await;
    match result {
        Ok(info) => {
            info!("WiFi connected successfully (channel {})", info.channel);
            profiling::once!(wifi_connected);
            stats::WIFI_CONNECTIONS.inc();
            station_connected(&info);
            set_provisioning(false);
            Ok(())
        }
        Err(e) => {
            error!("WiFi connection failed: {:?}", e);
            leds::set_status(LedStatus::WifiError);
            if !is_network_missing(&e) {
                drop_psk(controller).await;
            }
            Err(e)
        }
    }
}

fn is_network_missing(error: &ConnectionError) -> bool {
    matches!(
        error,
        ConnectionError::Failed(DisconnectedInfo {
            reason: DisconnectReason::NoAccessPointFound,
            ..
        })
    )
}

/// Stops and starts the WiFi, as station with the stored credentials: clears
/// the driver's connection state, and calibrates the radio again.
async fn restart_wifi(controller: &'static SharedWifiController) {
    info!("Restarting WiFi");
    let settings = app_settings::persist::get_settings().await;
    let mut controller = controller.lock().await;
    // SAFETY: Called with the controller locked. The driver's stop event
    // updates the controller's state; without a mode, setting the config
    // starts the driver again
    let result = unsafe {
        esp_wifi_stop();
        esp_wifi_set_mode(wifi_mode_t_WIFI_MODE_NULL)
    };
    if result != 0 {
        error!("Failed to stop the WiFi: {}", result);
    }
    controller
        .set_config(&Config::Station(stored_station_config(&settings)))
        .expect("Failed to set STA config");
}

/// Whether another recovery reboot is allowed, counting it.
fn take_recovery_reboot() -> bool {
    // SAFETY: Only accessed from the executor's tasks, never across an await
    let [count, complement] = unsafe { RECOVERY_REBOOTS };
    let count = if complement == !count { count } else { 0 };
    if count >= MAX_RECOVERY_REBOOTS {
        return false;
    }
    set_recovery_reboots(count + 1);
    true
}

fn set_recovery_reboots(count: u32) {
    // SAFETY: Only accessed from the executor's tasks, never across an await
    unsafe { RECOVERY_REBOOTS = [count, !count] };
}

/// Derives the key from the stored password, unless stored already. In the
/// background, once connected: the next connections take it in its place.
async fn derive_stored_psk() {
    let credentials = app_settings::persist::read_settings(|set| {
        set.wifi_psk
            .is_none()
            .then(|| set.wifi_ssid.clone().zip(set.wifi_password.clone()))
            .flatten()
    })
    .await;
    let Some((ssid, password)) = credentials else {
        return;
    };
    let psk = wifi_psk::derive_psk(&ssid, &password).await;
    // Unless the credentials changed meanwhile
    let stored = app_settings::persist::update_settings_changed(|set| {
        if set.wifi_ssid.as_ref() == Some(&ssid) && set.wifi_password.as_ref() == Some(&password) {
            set.wifi_psk = Some(psk);
        }
    })
    .await;
    if stored {
        info!("WiFi key derived from the password, connecting with it from now on");
    }
}

/// Connects with the password again, not the key derived from it: the
/// network's security may have changed (WPA3 takes the password itself). The
/// key is derived again once connected with WPA2.
async fn drop_psk(controller: &'static SharedWifiController) {
    if !app_settings::persist::update_settings_changed(|set| set.wifi_psk = None).await {
        return;
    }
    info!("Connecting with the WiFi password, not the key derived from it");
    let settings = app_settings::persist::get_settings().await;
    controller
        .lock()
        .await
        .set_config(&Config::Station(stored_station_config(&settings)))
        .expect("Failed to set STA config");
}

// Reconnects after the connection was lost. Derives the WiFi key once
// connected
#[embassy_executor::task]
async fn wifi_conn_task(controller: &'static SharedWifiController) {
    loop {
        let should_connect_ap = app_settings::persist::get_settings()
            .await
            .has_credentials_and_is_claimed();
        let is_connected = controller.lock().await.is_connected();
        if should_connect_ap && !is_connected {
            connect_ap();
        }
        if is_connected && PSK_WANTED.load(Ordering::Relaxed) {
            PSK_WANTED.store(false, Ordering::Relaxed);
            derive_stored_psk().await;
        }
        Timer::after(CONNECTION_CHECK_INTERVAL).await;
    }
}

// Records why the station was disconnected, or failed to connect: the
// driver's events, the only place with the reason
#[embassy_executor::task]
async fn wifi_events_task(controller: &'static SharedWifiController) {
    let mut events = {
        let controller = controller.lock().await;
        let events = controller
            .subscribe()
            .expect("No WiFi event subscriber left");
        // SAFETY: Only the lifetime changes. The subscriber borrows the
        // driver's static event channel, not the controller (which is static
        // too, never moved or dropped), so it may outlive the lock
        unsafe { core::mem::transmute::<EventSubscriber<'_>, EventSubscriber<'static>>(events) }
    };
    loop {
        // Read right away: the driver drops new events while one isn't read
        // by every subscriber
        if let EventInfo::StationDisconnected { reason, .. } = events.next_event_pure().await {
            info!("WiFi disconnected (reason {})", reason);
            stats::on_wifi_disconnected(reason);
        }
    }
}

#[embassy_executor::task(pool_size = 2)]
async fn net_stack_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}
