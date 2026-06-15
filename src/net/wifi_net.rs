use core::{ffi::CStr, net::Ipv4Addr, str::FromStr};

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
    self, Config, ControllerConfig, Interface, WifiController, ap::AccessPointConfig,
    sta::StationConfig,
};
use mbedtls_rs::{Certificate, Tls, X509};

use crate::{
    display::leds::{self, LedPixels, LedStatus},
    mk_static,
    net::{
        dhcp_server, http_server,
        ws_client::{self},
    },
    store::{SharedFlashStorage, app_settings, ota, transit_data},
};

pub const TCP_SERV_SOCKET_COUNT: usize = 4;
const DHCP_SERV_SOCKET_COUNT: usize = 1;
const DNS_SERV_SOCKET_COUNT: usize = 1;
const AP_SOCKET_COUNT: usize =
    DHCP_SERV_SOCKET_COUNT + DNS_SERV_SOCKET_COUNT + TCP_SERV_SOCKET_COUNT;

const STA_SOCKET_COUNT: usize = 4;

static WIFI_NET_SIGNAL: Signal<CriticalSectionRawMutex, WifiNetEvent> = Signal::new();

pub const CA_BUNDLE: &CStr = match CStr::from_bytes_with_nul(
    concat!(include_str!("../../assets/certs/ca-bundle.pem"), "\0").as_bytes(),
) {
    Ok(bundle) => bundle,
    _ => panic!("CA bundle is not a valid text file"),
};

enum WifiNetEvent {
    StartProvisioning,
    FinishProvisioning,
    ConnectToAp,
}

pub type SharedWifiController = Mutex<CriticalSectionRawMutex, WifiController<'static>>;

pub async fn spawn(
    spawner: Spawner,
    wifi_peri: WIFI<'static>,
    sha_peri: SHA<'static>,
    flash_store: &'static SharedFlashStorage,
) {
    // Create WiFi controller and interfaces
    let (controller, wifi_interfaces) = wifi::new(
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
    let wifi_ap_device = wifi_interfaces.access_point;
    let wifi_sta_device = wifi_interfaces.station;

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let gateway_ip = Ipv4Addr::from_str("192.168.4.1").expect("Invalid gateway IP");

    // Create AP stack
    let ap_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(gateway_ip, 24),
        gateway: Some(gateway_ip),
        dns_servers: Default::default(),
    });
    let (ap_stack, ap_runner) = embassy_net::new(
        wifi_ap_device,
        ap_config,
        mk_static!(
            StackResources<AP_SOCKET_COUNT>,
            StackResources::<AP_SOCKET_COUNT>::new()
        ),
        seed,
    );

    // Build AP SSID from MAC address
    let device_name = mk_static!(heapless::String<32>, {
        let ap_mac = wifi_ap_device.mac_address();
        let mut ssid = heapless::String::<32>::new();
        use core::fmt::Write;
        write!(
            ssid,
            "LEDTransit-{:02X}{:02X}{:02X}",
            ap_mac[3], ap_mac[4], ap_mac[5]
        )
        .expect("Failed to write SSID");
        ssid
    });

    // Create STA stack
    let mut dhcp_config: DhcpConfig = Default::default();
    dhcp_config.hostname = Some(heapless::String::try_from(device_name.as_str()).unwrap());
    let sta_config: embassy_net::Config = embassy_net::Config::dhcpv4(dhcp_config);
    let (sta_stack, sta_runner) = embassy_net::new(
        wifi_sta_device,
        sta_config,
        mk_static!(
            StackResources<STA_SOCKET_COUNT>,
            StackResources::<STA_SOCKET_COUNT>::new()
        ),
        seed,
    );

    let shared_controller = mk_static!(SharedWifiController, Mutex::new(controller));

    // Prepare TLS for HTTPS and WSS
    let trng = mk_static!(Trng, Trng::try_new().unwrap());
    let tls = mk_static!(Tls, Tls::new(trng).unwrap());
    let ca_cert = mk_static!(Certificate<'static>, {
        Certificate::new(X509::PEM(CA_BUNDLE)).expect("Failed to parse CA bundle")
    });

    // Spawn network tasks
    spawner.spawn(net_stack_task(ap_runner).unwrap());
    spawner.spawn(net_stack_task(sta_runner).unwrap());
    dhcp_server::spawn(spawner, ap_stack, gateway_ip);
    http_server::spawn(spawner, ap_stack, shared_controller, device_name);
    ws_client::spawn(spawner, sta_stack, shared_controller, tls, ca_cert);
    ota::spawn(spawner, sta_stack, tls, ca_cert, flash_store, sha_peri);

    let settings = app_settings::persist::get_settings().await;

    // Start in STA mode by default
    shared_controller
        .lock()
        .await
        .set_config(&Config::Station(
            StationConfig::default()
                .with_ssid(settings.wifi_ssid.unwrap_or_default().as_str())
                .with_password(settings.wifi_password.unwrap_or_default().as_str().into()),
        ))
        .expect("Failed to set STA config");

    // Spawn WiFi network event task
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

#[embassy_executor::task]
async fn wifi_net_task(
    controller: &'static SharedWifiController,
    ap_ssid: &'static heapless::String<32>,
) {
    loop {
        match WIFI_NET_SIGNAL.wait().await {
            WifiNetEvent::StartProvisioning => {
                info!("Starting WiFi provisioning mode");
                leds::set_status(LedStatus::Pairing);

                // Check if already connected to AP
                if controller.lock().await.is_connected() {
                    info!("Disconnecting from current WiFi network");
                    controller.lock().await.disconnect_async().await.ok();
                }

                // Configure AP+STA mode for provisioning
                controller
                    .lock()
                    .await
                    .set_config(&Config::AccessPointStation(
                        StationConfig::default(),
                        AccessPointConfig::default().with_ssid(ap_ssid.as_str()),
                    ))
                    .expect("Failed to set AP+STA config");
            }
            WifiNetEvent::FinishProvisioning => {
                info!("Finishing WiFi provisioning mode");
                let settings = app_settings::persist::get_settings().await;

                // Configure STA mode with new credentials
                controller
                    .lock()
                    .await
                    .set_config(&Config::Station(
                        StationConfig::default()
                            .with_ssid(settings.wifi_ssid.unwrap_or_default().as_str())
                            .with_password(
                                settings.wifi_password.unwrap_or_default().as_str().into(),
                            ),
                    ))
                    .expect("Failed to set STA config");
                connect_ap();
            }
            WifiNetEvent::ConnectToAp => {
                info!("Connecting to WiFi access point");
                leds::set_status(LedStatus::ConnectingWifi);

                // Check if already connected to AP
                if controller.lock().await.is_connected() {
                    continue;
                }

                // Connect to AP
                match controller.lock().await.connect_async().await {
                    Ok(_) => {
                        info!("WiFi connected successfully");
                        continue;
                    }
                    Err(e) => {
                        error!("WiFi connection failed: {:?}", e);
                        leds::set_status(LedStatus::WifiError);
                    }
                }

                // On failure, try again after a delay
                Timer::after(Duration::from_secs(1)).await;
                if !WIFI_NET_SIGNAL.signaled() {
                    connect_ap();
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn wifi_conn_task(controller: &'static SharedWifiController) {
    loop {
        // Wifi re-connection loop (without blocking controller mutex)
        let should_connect_ap = app_settings::persist::get_settings()
            .await
            .has_credentials_and_is_authenticated();
        let is_connected = controller.lock().await.is_connected();

        if should_connect_ap && !is_connected {
            connect_ap();
        }

        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task(pool_size = 2)]
async fn net_stack_task(mut runner: Runner<'static, Interface<'static>>) {
    runner.run().await
}
