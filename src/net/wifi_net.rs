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
    rng::{Rng, Trng},
};
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config, ControllerConfig, Interface, Password, Ssid,
    WifiController, ap::AccessPointConfig, sta::StationConfig,
};
use mbedtls_rs::{Certificate, Tls, X509};

use crate::{
    display::leds::{self, LedPixels, LedStatus},
    mk_static,
    net::{provisioning, ws_client},
    ota,
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

// Set from the start of provisioning until the WiFi is connected again after
// it: finishing it reconfigures the WiFi (station only), which drops the link
// the station connected over during provisioning
static PROVISIONING: AtomicBool = AtomicBool::new(false);

/// Whether provisioning is still going on: the station's link (if up) is about
/// to drop, so nothing should connect over it yet.
pub fn is_provisioning() -> bool {
    PROVISIONING.load(Ordering::Relaxed)
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
    let tls = mk_static!(Tls, Tls::new(trng).unwrap());
    let ca_cert = mk_static!(Certificate<'static>, {
        Certificate::new(X509::PEM(CA_BUNDLE)).expect("Failed to parse CA bundle")
    });

    spawner.spawn(net_stack_task(ap_runner).unwrap());
    spawner.spawn(net_stack_task(sta_runner).unwrap());
    provisioning::spawn(
        spawner,
        ap_stack,
        AP_GATEWAY_IP,
        shared_controller,
        device_name,
    );
    ws_client::spawn(spawner, sta_stack, shared_controller, tls, ca_cert);
    ota::spawn(spawner, sta_stack, tls, ca_cert, flash_store, sha_peri);

    // Start in station mode
    let settings = app_settings::persist::get_settings().await;
    shared_controller
        .lock()
        .await
        .set_config(&Config::Station(stored_station_config(&settings)))
        .expect("Failed to set STA config");

    spawner.spawn(wifi_net_task(shared_controller, device_name).unwrap());
    spawner.spawn(wifi_conn_task(shared_controller).unwrap());

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

fn stored_station_config(settings: &PersistSettings) -> StationConfig {
    station_config(
        settings.wifi_ssid.as_deref().unwrap_or_default(),
        settings.wifi_password.as_deref().unwrap_or_default(),
    )
}

#[embassy_executor::task]
async fn wifi_net_task(
    controller: &'static SharedWifiController,
    ap_ssid: &'static heapless::String<32>,
) {
    loop {
        match WIFI_NET_SIGNAL.wait().await {
            WifiNetEvent::StartProvisioning => start_setup_network(controller, ap_ssid).await,
            WifiNetEvent::FinishProvisioning => {
                info!("Finishing WiFi provisioning mode");
                let settings = app_settings::persist::get_settings().await;
                controller
                    .lock()
                    .await
                    .set_config(&Config::Station(stored_station_config(&settings)))
                    .expect("Failed to set STA config");
                connect_ap();
            }
            WifiNetEvent::ConnectToAp => {
                if connect_to_ap(controller).await {
                    continue;
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
    PROVISIONING.store(true, Ordering::Relaxed);
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

/// Connects the station, if not connected. False if that failed.
async fn connect_to_ap(controller: &'static SharedWifiController) -> bool {
    info!("Connecting to WiFi access point");
    leds::set_status(LedStatus::ConnectingWifi);

    if controller.lock().await.is_connected() {
        info!("WiFi already connected");
        PROVISIONING.store(false, Ordering::Relaxed);
        return true;
    }

    let result = controller.lock().await.connect_async().await;
    match result {
        Ok(_) => {
            info!("WiFi connected successfully");
            PROVISIONING.store(false, Ordering::Relaxed);
            true
        }
        Err(e) => {
            error!("WiFi connection failed: {:?}", e);
            leds::set_status(LedStatus::WifiError);
            false
        }
    }
}

// Reconnects after the connection was lost
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
        Timer::after(CONNECTION_CHECK_INTERVAL).await;
    }
}

#[embassy_executor::task(pool_size = 2)]
async fn net_stack_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}
