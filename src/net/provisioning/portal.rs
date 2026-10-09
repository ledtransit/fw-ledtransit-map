// The setup portal: serves the setup page and its API
use core::fmt::{Debug, Display};

use defmt::info;
use edge_net::http::{
    Method,
    io::{
        Error,
        server::{Connection, Handler},
    },
};
use embassy_time::{Duration, Timer};
use embedded_io_async::{Read, Write};
use esp_radio::wifi::{
    AuthenticationMethod, Config, ap::AccessPointConfig, scan::ScanConfig, sta::StationConfig,
};
use serde::{Deserialize, Serialize};

use super::static_files::{self, StaticFile};
use crate::{
    display::leds::{self, LedPixels, LedStatus},
    net::wifi_net::{self, SharedWifiController},
    store::app_settings,
};

const MAX_SCANNED_NETWORKS: usize = 16;

pub struct HttpHandler {
    controller: &'static SharedWifiController,
    ap_ssid: &'static heapless::String<32>,
}

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
    /// Provisioning token from the app, in the body so it never shows up in
    /// a URL
    token: heapless::String<64>,
}

impl HttpHandler {
    pub fn new(
        controller: &'static SharedWifiController,
        ap_ssid: &'static heapless::String<32>,
    ) -> Self {
        Self {
            controller,
            ap_ssid,
        }
    }
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
        let method = headers.method;
        let (path, locale) = split_path_and_locale(headers.path);

        match method {
            Method::Get | Method::Head if path == "/probe" => {
                // Lets the app (on its own https page) tell the map is reachable
                send_response(conn, 204, "No Content", &[], None).await?;
            }
            Method::Get | Method::Head => match static_files::find(path, locale) {
                Some(file) => send_static_file(conn, file, method == Method::Get).await?,
                None => send_response(conn, 404, "Not Found", &[], None).await?,
            },
            Method::Post => match path {
                "/api/identify" => handle_identify(conn).await?,
                "/api/scan-wifi" => handle_scan(conn, self.controller).await?,
                "/api/connect-wifi" => handle_connect(conn, self.controller, self.ap_ssid).await?,
                _ => send_response(conn, 404, "Not Found", &[], None).await?,
            },
            _ => send_response(conn, 405, "Method Not Allowed", &[], None).await?,
        }

        conn.flush().await?;
        Ok(())
    }
}

// The path without the query, and the locale from its "lang" parameter
fn split_path_and_locale(path_and_query: &str) -> (&str, Option<&str>) {
    let mut parts = path_and_query.split('?');
    let path = parts.next().unwrap_or("");
    let query = parts.next().unwrap_or("");
    let locale = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "lang")
        .map(|(_, value)| value);
    (path, locale)
}

async fn handle_identify<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    info!("API: Identify");
    send_response(conn, 200, "OK", &[], None).await?;
    leds::set_pixels(LedPixels::Identify).await;
    leds::wait_pixels_animation_complete().await;
    leds::set_pixels(LedPixels::DemoMode).await;
    Ok(())
}

async fn handle_scan<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
    controller: &'static SharedWifiController,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    info!("API: Request WiFi scan");
    let mut controller = controller.lock().await;
    let mut ap_list = match controller.scan_async(&ScanConfig::default()).await {
        Ok(ap_list) => ap_list,
        Err(e) => {
            info!("WiFi scan error: {:?}", e);
            return send_response(conn, 500, "Internal Server Error", &[], None).await;
        }
    };

    // The strongest networks
    ap_list.sort_by_key(|ap| core::cmp::Reverse(ap.signal_strength));
    ap_list.truncate(MAX_SCANNED_NETWORKS);
    let ap_list_api: heapless::Vec<AccessPointInfoApi, MAX_SCANNED_NETWORKS> = ap_list
        .iter()
        .map(|ap| AccessPointInfoApi {
            ssid: heapless::String::try_from(ap.ssid.as_str()).unwrap_or_default(),
            rssi: ap.signal_strength,
            open: u8::from(ap.auth_method == Some(AuthenticationMethod::None)),
        })
        .collect();
    let json: serde_json_core::heapless::String<1200> =
        serde_json_core::to_string(&ap_list_api).expect("Failed to serialize AP list to JSON");
    send_response(conn, 200, "OK", json.as_bytes(), Some("application/json")).await
}

async fn handle_connect<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
    controller: &'static SharedWifiController,
    ap_ssid: &str,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    info!("API: Connect to WiFi");

    // JSON only: from a page of another origin, that needs a CORS preflight,
    // which fails here
    let is_json = conn
        .headers()?
        .headers
        .content_type()
        .and_then(|content_type| content_type.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return send_response(conn, 415, "Unsupported Media Type", &[], None).await;
    }

    let request = read_connect_request(conn).await?;
    if request.token.is_empty() {
        return send_response(
            conn,
            401,
            "Unauthorized",
            b"Unauthorized: Missing token",
            None,
        )
        .await;
    }
    info!("Connecting to SSID: '{}'", request.ssid);

    if controller.lock().await.is_connected() {
        info!("Already connected");
        send_response(conn, 200, "OK", &[], None).await?;
        wifi_net::finish_provisioning();
        return Ok(());
    }

    // Fade out first, so the lag of the blocking WiFi connect doesn't show
    leds::set_pixels(LedPixels::FadeOut).await;
    leds::wait_pixels_animation_complete().await;
    leds::set_status(LedStatus::ConnectingWifi);

    let set_config_result = controller
        .lock()
        .await
        .set_config(&Config::AccessPointStation(
            StationConfig::default()
                .with_ssid(request.ssid.as_str())
                .with_password(request.password.as_str().into()),
            AccessPointConfig::default().with_ssid(ap_ssid),
        ));
    if let Err(e) = set_config_result {
        info!("Set config error: {:?}", e);
        return respond_connect_failed(conn).await;
    }

    info!("Starting WiFi connection");
    let connect_result = controller.lock().await.connect_async().await;
    if let Err(e) = connect_result {
        info!("WiFi start error: {:?}", e);
        return respond_connect_failed(conn).await;
    }

    info!("WiFi started");
    send_response(conn, 200, "OK", &[], None).await?;
    app_settings::persist::update_settings(move |set| {
        set.wifi_ssid = Some(heapless::String::try_from(request.ssid.as_str()).unwrap_or_default());
        set.wifi_password =
            Some(heapless::String::try_from(request.password.as_str()).unwrap_or_default());
        set.prov_token = Some(request.token.clone());
    })
    .await;
    Timer::after(Duration::from_secs(8)).await;
    wifi_net::finish_provisioning();
    Ok(())
}

async fn read_connect_request<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
) -> Result<ConnectWifiRequestApi, Error<T::Error>>
where
    T: Read + Write,
{
    let mut body_buf = [0u8; 512];
    let body_len = conn
        .read(&mut body_buf)
        .await
        .map_err(|_| Error::IncompleteBody)?;
    let body = core::str::from_utf8(&body_buf[..body_len]).map_err(|_| Error::InvalidBody)?;
    let (request, _) = serde_json_core::from_str(body).map_err(|_| Error::InvalidBody)?;
    Ok(request)
}

async fn respond_connect_failed<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    leds::set_status(LedStatus::WifiError);
    leds::set_pixels(LedPixels::DemoMode).await;
    send_response(conn, 500, "Internal Server Error", &[], None).await
}

/// Serves a static file, with its body when `with_body` (GET, not HEAD).
async fn send_static_file<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
    file: &StaticFile,
    with_body: bool,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    let content = if with_body { file.content } else { &[] };
    let content_encoding = file.gzip.then_some("gzip");
    send_response_encoded(
        conn,
        200,
        "OK",
        content,
        Some(file.mime_type),
        content_encoding,
    )
    .await
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
    send_response_encoded(conn, status_code, status_message, content, mime_type, None).await
}

async fn send_response_encoded<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
    status_code: u16,
    status_message: &str,
    content: &[u8],
    mime_type: Option<&str>,
    content_encoding: Option<&str>,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    let content_length =
        heapless::format!(20; "{}", content.len()).expect("Failed to format content length");
    let mut headers = heapless::Vec::<(&str, &str), 3>::new();
    headers
        .push(("Content-Length", content_length.as_str()))
        .unwrap();
    if let Some(mime) = mime_type {
        headers.push(("Content-Type", mime)).unwrap();
    }
    if let Some(encoding) = content_encoding {
        headers.push(("Content-Encoding", encoding)).unwrap();
    }
    conn.initiate_response(status_code, Some(status_message), &headers)
        .await?;
    if !content.is_empty() {
        conn.write_all(content).await?;
    }
    Ok(())
}
