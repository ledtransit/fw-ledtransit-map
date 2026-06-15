use defmt::info;
use edge_http::{
    Method,
    ws::{self, MAX_BASE64_KEY_LEN, MAX_BASE64_KEY_RESPONSE_LEN, NONCE_LEN},
};
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read, Write};
use esp_hal::rng::Rng;

use crate::{
    config::{self, CONFIG},
    net::ws_client::{Connection, WsClientError},
    store::app_settings,
};

const AUTH_ENDPOINT: &str = "/auth";
const WS_ENDPOINT: &str = "/ws";

#[derive(Serialize)]
struct AuthBody<'a> {
    hardware_id: &'a str,
    product_ident: &'a str,
    model: &'a str,
    hw_major: u32,
    hw_minor: u32,
    fw_major: u32,
    fw_minor: u32,
    fw_patch: u32,
    feed_ident: &'a str,
}

#[derive(Deserialize)]
struct AuthResponse<'a> {
    access_token: &'a str,
}

/// Performs device authentication with the HTTP auth server using the provided provisioning token.
///
/// On success, stores the received long-lived access token and clears the provisioning token.
pub async fn provision_authenticate(
    conn: &mut Connection<'_>,
    prov_token: &str,
    host: &str,
) -> Result<(), WsClientError> {
    info!("Performing provisioning authentication");

    // HTTP GET request to auth endpoint with provisioning token header
    with_timeout(
        Duration::from_secs(5),
        conn.initiate_request(
            true,
            Method::Get,
            AUTH_ENDPOINT,
            &[("Host", host), ("Provisioning-Token", prov_token)],
        ),
    )
    .await
    .map_err(WsClientError::Timeout)?
    .map_err(WsClientError::HttpError)?;

    // Serialize auth body with device info and send as request body
    let auth_body = AuthBody {
        hardware_id: &config::get_hardware_id_str(),
        product_ident: CONFIG.product.as_str(),
        model: CONFIG.product.as_model_str(),
        hw_major: CONFIG.hw_version.major,
        hw_minor: CONFIG.hw_version.minor,
        fw_major: CONFIG.fw_version.major,
        fw_minor: CONFIG.fw_version.minor,
        fw_patch: CONFIG.fw_version.patch,
        feed_ident: CONFIG.cfg.data_feed,
    };
    let body_bytes: serde_json_core::heapless::Vec<u8, 1024> =
        serde_json_core::to_vec(&auth_body).expect("Auth body serialization failed");
    conn.write_all(&body_bytes)
        .await
        .map_err(WsClientError::HttpError)?;

    // Read HTTP response and check status code
    conn.initiate_response()
        .await
        .map_err(WsClientError::HttpError)?;
    let response = conn.headers().map_err(WsClientError::HttpError)?;
    if response.code != 200 {
        info!("Authentication failed with status code {}", response.code);
        return Err(WsClientError::AuthFailed);
    }

    // Read response JSON body and deserialize access token
    let mut resp_body_buf = [0u8; 512];
    let resp_body_len = conn
        .read(&mut resp_body_buf)
        .await
        .map_err(WsClientError::HttpError)?;
    let auth_resp: AuthResponse = serde_json_core::from_slice(&resp_body_buf[..resp_body_len])
        .map_err(|_| WsClientError::DataError)?
        .0;
    let access_token =
        heapless::String::try_from(auth_resp.access_token).map_err(|_| WsClientError::DataError)?;

    // Store access token and clear provisioning token
    app_settings::persist::update_settings(|set| {
        set.prov_token = None;
        set.access_token = Some(access_token.clone());
    })
    .await;

    info!("Authentication successful, access token stored");
    Ok(())
}

/// Performs WebSocket upgrade and authentication using the provided long-lived access token.
///
/// On success, the connection is ready for authenticated WS communication with the server.
pub async fn websocket_authenticate(
    host: &str,
    conn: &mut Connection<'_>,
    access_token: &str,
    rng: &mut Rng,
) -> Result<(), WsClientError> {
    // Build HTTP->WS upgrade headers
    let mut nonce = [0u8; NONCE_LEN];
    for byte in nonce.iter_mut() {
        *byte = rng.random() as u8;
    }
    let mut nonce_b64_buf = [0u8; MAX_BASE64_KEY_LEN];
    let headers = ws::upgrade_request_headers(
        Some(host),
        Some("ledtransit-client"),
        None,
        &nonce,
        &mut nonce_b64_buf,
    );
    let mut headers_vec: heapless::Vec<(&str, &str), 8> =
        heapless::Vec::from_slice(&headers).unwrap();
    headers_vec.push(("Access-Token", access_token)).unwrap();

    // HTTP GET request to ws endpoint
    with_timeout(
        Duration::from_secs(5),
        conn.initiate_request(true, Method::Get, WS_ENDPOINT, headers_vec.as_slice()),
    )
    .await
    .map_err(WsClientError::Timeout)?
    .map_err(WsClientError::HttpError)?;

    // Check for successful WS upgrade response
    conn.initiate_response()
        .await
        .map_err(WsClientError::HttpError)?;
    let mut buf = [0_u8; MAX_BASE64_KEY_RESPONSE_LEN];
    if !conn
        .is_ws_upgrade_accepted(&nonce, &mut buf)
        .map_err(WsClientError::HttpError)?
    {
        return Err(WsClientError::AuthFailed);
    }

    conn.complete().await.map_err(WsClientError::HttpError)?;
    Ok(())
}
