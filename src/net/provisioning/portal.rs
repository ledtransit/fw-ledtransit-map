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
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read, Write};
use esp_radio::wifi::{AuthenticationMethod, scan::ScanConfig};
use serde::{Deserialize, Serialize};

use super::static_files::{self, StaticFile};
use crate::{
    display::leds::{self, LedPixels, LedStatus},
    net::wifi_net::{self, SharedWifiController},
    store::app_settings,
};

const MAX_SCANNED_NETWORKS: usize = 20;
const SCAN_JSON_MAX_LEN: usize = 1200;
const RESPONSE_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct HttpHandler {
    controller: &'static SharedWifiController,
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
    pub fn new(controller: &'static SharedWifiController) -> Self {
        Self { controller }
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
                "/api/connect-wifi" => handle_connect(conn, self.controller).await?,
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
    let mut ap_list_api: heapless::Vec<AccessPointInfoApi, MAX_SCANNED_NETWORKS> = ap_list
        .iter()
        .map(|ap| AccessPointInfoApi {
            ssid: heapless::String::try_from(ap.ssid.as_str()).unwrap_or_default(),
            rssi: ap.signal_strength,
            open: u8::from(ap.auth_method == Some(AuthenticationMethod::None)),
        })
        .collect();
    // Escaped SSIDs (quotes, control characters) can take up to 6 bytes per
    // character: the weakest networks are left out until the list fits
    let json: serde_json_core::heapless::String<SCAN_JSON_MAX_LEN> = loop {
        match serde_json_core::to_string(&ap_list_api) {
            Ok(json) => break json,
            Err(_) => _ = ap_list_api.pop(),
        }
    };
    send_response(conn, 200, "OK", json.as_bytes(), Some("application/json")).await
}

async fn handle_connect<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
    controller: &'static SharedWifiController,
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
        return respond_connected(conn).await;
    }

    // Fades out while connecting, without waiting for it: the transit data
    // takes longer to arrive, and the painter waits for the fade before its
    // first frame
    leds::set_pixels(LedPixels::FadeOut).await;
    leds::set_status(LedStatus::ConnectingWifi);

    let set_credentials_result = wifi_net::set_setup_station_credentials(
        &*controller.lock().await,
        &request.ssid,
        &request.password,
    );
    if let Err(e) = set_credentials_result {
        info!("Set station credentials error: {}", e);
        return respond_connect_failed(conn).await;
    }

    info!("Starting WiFi connection");
    let connected = match controller.lock().await.connect_async().await {
        Ok(connected) => connected,
        Err(e) => {
            info!("WiFi start error: {:?}", e);
            return respond_connect_failed(conn).await;
        }
    };

    info!("WiFi started");
    app_settings::persist::update_settings(move |set| {
        set.set_wifi_credentials(&request.ssid, &request.password);
        set.prov_token = Some(request.token.clone());
    })
    .await;
    // The gateway connection starts meanwhile, while the page gets its answer
    wifi_net::setup_station_connected(&connected);
    respond_connected(conn).await
}

/// Answers the page, then closes the setup network: once the page
/// acknowledged the answer (the phone may take a second, dozing), or after a
/// while without.
async fn respond_connected<T, const N: usize>(
    conn: &mut Connection<'_, T, N>,
) -> Result<(), Error<T::Error>>
where
    T: Read + Write,
{
    let result = with_timeout(RESPONSE_DELIVERY_TIMEOUT, async {
        send_response(conn, 200, "OK", &[], None).await?;
        conn.flush().await
    })
    .await;
    wifi_net::finish_provisioning();
    result.unwrap_or(Err(Error::ConnectionClosed))
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
