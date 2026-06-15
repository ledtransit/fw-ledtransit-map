use core::fmt::{Debug, Display};

use defmt::info;
use edge_net::http::{
    Method,
    io::{
        Error,
        server::{Connection, Handler},
    },
};
use embedded_io_async::{Read, Write};
use esp_radio::wifi::{
    AuthenticationMethod, Config, ap::AccessPointConfig, scan::ScanConfig, sta::StationConfig,
};
use nourl::Url;
use serde::{Deserialize, Serialize};

use crate::{
    display::leds::{self, LedPixels, LedStatus},
    net::wifi_net::{self, SharedWifiController},
    store::app_settings,
};

pub struct HttpHandler {
    controller: &'static SharedWifiController,
    ap_ssid: heapless::String<32>,
}

struct StaticFile {
    path: &'static str,
    content: &'static [u8],
    mime_type: &'static str,
    locale: &'static str,
}

macro_rules! asset_path {
    ($file:expr) => {
        concat!(env!("CARGO_MANIFEST_DIR"), "/assets/prov_public/", $file)
    };
}

const DEFAULT_LOCALE: &str = "en";

// Static file system
const STATIC_FILES: &[StaticFile] = &[
    StaticFile {
        path: "/setup-wifi",
        content: include_bytes!(asset_path!("setup-wifi+en.html")),
        mime_type: "text/html",
        locale: "en",
    },
    StaticFile {
        path: "/setup-wifi",
        content: include_bytes!(asset_path!("setup-wifi+de.html")),
        mime_type: "text/html",
        locale: "de",
    },
    StaticFile {
        path: "/styles.css",
        content: include_bytes!(asset_path!("styles.css")),
        mime_type: "text/css",
        locale: DEFAULT_LOCALE,
    },
    StaticFile {
        path: "/favicon.ico",
        content: include_bytes!(asset_path!("favicon.ico")),
        mime_type: "image/x-icon",
        locale: DEFAULT_LOCALE,
    },
    StaticFile {
        path: "/background.webp",
        content: include_bytes!(asset_path!("background.webp")),
        mime_type: "image/webp",
        locale: DEFAULT_LOCALE,
    },
];

#[derive(Serialize, Deserialize)]
struct AccessPointInfoApi {
    ssid: heapless::String<32>,
    rssi: i8,
    open: u8,
}

#[derive(Serialize, Deserialize)]
struct ConnectWifiRequestApi {
    ssid: heapless::String<32>,
    password: heapless::String<64>,
}

impl HttpHandler {
    pub fn new(controller: &'static SharedWifiController, ap_ssid: heapless::String<32>) -> Self {
        Self {
            controller,
            ap_ssid,
        }
    }
}

fn get_static_file_with_locale(path: &str, locale: Option<&str>) -> Option<&'static StaticFile> {
    STATIC_FILES
        .iter()
        .find(|file| file.path == path && file.locale == locale.unwrap_or(DEFAULT_LOCALE))
        .or_else(|| {
            STATIC_FILES
                .iter()
                .find(|file| file.path == path && file.locale == DEFAULT_LOCALE)
        })
}

async fn send_response<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
    status_code: u16,
    status_message: &str,
    content: &[u8],
    mime_type: Option<&str>,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    let content_length_str =
        heapless::format!(20; "{}", content.len()).expect("Failed to format content length");
    let mut headers = heapless::Vec::<(&str, &str), 2>::new();
    headers
        .push(("Content-Length", content_length_str.as_str()))
        .unwrap();
    if let Some(mime) = mime_type {
        headers.push(("Content-Type", mime)).unwrap();
    }
    conn.initiate_response(status_code, Some(status_message), &headers)
        .await?;
    if !content.is_empty() {
        conn.write_all(content).await?;
    }
    Ok(())
}

impl Handler for HttpHandler {
    type Error<E>
        = Error<E>
    where
        E: Debug;

    async fn handle<T, const N: usize>(
        &self,
        _task_id: impl Display + Copy,
        conn: &mut Connection<'_, T, N>,
    ) -> Result<(), Self::Error<T::Error>>
    where
        T: Read + Write,
    {
        let headers = conn.headers()?;
        info!("HTTP request: {} {}", headers.method, headers.path);

        // Parse query parameters
        let mut url_parts = headers.path.split('?');
        let url_path = url_parts.next().unwrap_or("");
        let query_string = url_parts.next().unwrap_or("");
        let query_pairs = query_string
            .split('&')
            .filter_map(|pair| {
                let mut split = pair.splitn(2, '=');
                if let (Some(key), Some(value)) = (split.next(), split.next()) {
                    Some((key, value))
                } else {
                    None
                }
            })
            .collect::<heapless::Vec<(&str, &str), 16>>();

        let token = query_pairs
            .iter()
            .find(|(key, _)| *key == "tok")
            .map(|(_, value)| heapless::String::<64>::try_from(*value).unwrap_or_default());
        let locale = query_pairs
            .iter()
            .find(|(key, _)| *key == "lang")
            .map(|(_, value)| *value);

        // Serve static files and API routes
        match headers.method {
            Method::Get => {
                // Serve static file
                if let Some(file) = get_static_file_with_locale(url_path, locale) {
                    send_response(conn, 200, "OK", file.content, Some(file.mime_type)).await?;
                }
            }
            Method::Head => {
                // Serve static file without body
                if let Some(file) = get_static_file_with_locale(url_path, locale) {
                    send_response(conn, 200, "OK", &[], Some(file.mime_type)).await?;
                }
            }
            Method::Post => {
                // Handle API routes
                match url_path {
                    "/api/identify" => handle_api_route_identify(conn).await?,
                    "/api/scan-wifi" => handle_api_route_scan(conn, self.controller).await?,
                    "/api/connect-wifi" => {
                        handle_api_route_connect(conn, self.controller, self.ap_ssid.clone(), token)
                            .await?
                    }
                    _ => {
                        send_response(conn, 404, "Not Found", &[], None).await?;
                    }
                }
            }
            _ => {
                send_response(conn, 405, "Method Not Allowed", &[], None).await?;
            }
        }

        // Finish response
        conn.flush().await?;

        Ok(())
    }
}

async fn handle_api_route_identify<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    info!("API: Identify");
    leds::set_pixels(LedPixels::Identify).await;
    send_response(conn, 200, "OK", &[], None).await
}

async fn handle_api_route_scan<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
    controller: &'static SharedWifiController,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    info!("API: Request WiFi scan");
    let mut controller_guard = controller.lock().await;

    // Start scanning for APs
    match controller_guard.scan_async(&ScanConfig::default()).await {
        Ok(mut ap_list) => {
            ap_list.sort_by_key(|b| core::cmp::Reverse(b.signal_strength));
            ap_list.truncate(16);

            // Serialize AP list to JSON and send response
            let ap_list_api: heapless::Vec<AccessPointInfoApi, 16> = ap_list
                .iter()
                .map(|ap| AccessPointInfoApi {
                    ssid: heapless::String::try_from(ap.ssid.as_str()).unwrap_or_default(),
                    rssi: ap.signal_strength,
                    open: ap
                        .auth_method
                        .map(|am| {
                            if am == AuthenticationMethod::None {
                                1
                            } else {
                                0
                            }
                        })
                        .unwrap_or(0),
                })
                .collect();
            let json: serde_json_core::heapless::String<1200> =
                serde_json_core::to_string(&ap_list_api)
                    .expect("Failed to serialize AP list to JSON");
            send_response(conn, 200, "OK", json.as_bytes(), Some("application/json")).await?;
        }
        Err(e) => {
            info!("WiFi scan error: {:?}", e);
            send_response(conn, 500, "Internal Server Error", &[], None).await?;
        }
    }
    Ok(())
}

async fn handle_api_route_connect<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
    controller: &'static SharedWifiController,
    ap_ssid: heapless::String<32>,
    token: Option<heapless::String<64>>,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    info!("API: Connect to WiFi");

    // Check have token from query parameters
    if token.is_none() {
        send_response(
            conn,
            401,
            "Unauthorized",
            b"Unauthorized: Missing token",
            None,
        )
        .await?;
        return Ok(());
    }

    // Read request body
    let mut body_buf = [0u8; 512];
    let body_len = conn
        .read(&mut body_buf)
        .await
        .map_err(|_| Error::IncompleteBody)?;
    let body_str = core::str::from_utf8(&body_buf[..body_len]).map_err(|_| Error::InvalidBody)?;

    // Parse JSON body
    let connect_request: ConnectWifiRequestApi = serde_json_core::from_str(body_str)
        .map_err(|_| Error::InvalidBody)?
        .0;
    info!(
        "Connecting to SSID: '{}' and token: {}",
        connect_request.ssid,
        token.clone().unwrap()
    );

    // Check already connect to an AP
    if controller.lock().await.is_connected() {
        info!("Already connected");
        send_response(conn, 200, "OK", &[], None).await?;
        wifi_net::finish_provisioning();
        return Ok(());
    }

    // Configure WiFi station config with provided credentials
    leds::set_status(LedStatus::ConnectingWifi);
    let res = controller
        .lock()
        .await
        .set_config(&Config::AccessPointStation(
            StationConfig::default()
                .with_ssid(connect_request.ssid.as_str())
                .with_password(connect_request.password.as_str().into()),
            AccessPointConfig::default().with_ssid(ap_ssid.as_str()),
        ));
    match res {
        Ok(_) => {
            info!("Starting WiFi connection");
            // Try to connect to WiFi with new credentials
            match controller.lock().await.connect_async().await {
                Ok(_) => {
                    info!("WiFi started");
                    send_response(conn, 200, "OK", &[], None).await?;
                    app_settings::persist::update_settings(move |set| {
                        set.wifi_ssid = Some(
                            heapless::String::try_from(connect_request.ssid.as_str())
                                .unwrap_or_default(),
                        );
                        set.wifi_password = Some(
                            heapless::String::try_from(connect_request.password.as_str())
                                .unwrap_or_default(),
                        );
                        set.prov_token = token.clone();
                    })
                    .await;
                    wifi_net::finish_provisioning();
                }
                Err(e) => {
                    info!("WiFi start error: {:?}", e);
                    leds::set_status(LedStatus::WifiError);
                    send_response(conn, 500, "Internal Server Error", &[], None).await?;
                }
            }
        }
        Err(e) => {
            info!("Set config error: {:?}", e);
            send_response(conn, 500, "Internal Server Error", &[], None).await?;
        }
    }
    Ok(())
}
