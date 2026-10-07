use core::fmt::Write as _;

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
    device_auth::{self, DeviceAuthError, KeySlot},
    net::ws_client::{Connection, WsClientError},
    store::app_settings,
};

const CHALLENGE_ENDPOINT: &str = "/challenge";
const CLAIM_ENDPOINT: &str = "/claim";
const WS_ENDPOINT: &str = "/ws";

const MAX_NONCE_LEN: usize = 128;

#[derive(Deserialize)]
struct ChallengeResponse<'a> {
    nonce: &'a str,
    #[serde(rename = "keySlot")]
    key_slot: u8,
}

/// A challenge of the gateway to prove this device's identity over, and
/// which device key to prove it with.
struct Challenge {
    nonce: heapless::String<MAX_NONCE_LEN>,
    key_slot: KeySlot,
}

#[derive(Serialize)]
struct ClaimBody<'a> {
    hardware_id: &'a str,
    product_ident: &'a str,
    model: &'a str,
    hw_major: u32,
    hw_minor: u32,
    fw_major: u32,
    fw_minor: u32,
    fw_patch: u32,
    feed_ident: &'a str,
    nonce: &'a str,
    proof: &'a str,
}

fn map_device_auth_error(error: DeviceAuthError) -> WsClientError {
    match error {
        DeviceAuthError::NoKey => WsClientError::NoDeviceKeys,
        DeviceAuthError::NotInitialized => WsClientError::DataError,
    }
}

async fn fetch_challenge(
    conn: &mut Connection<'_>,
    host: &str,
    hardware_id: &str,
) -> Result<Challenge, WsClientError> {
    let mut uri: heapless::String<96> = heapless::String::new();
    write!(uri, "{}?hardware_id={}", CHALLENGE_ENDPOINT, hardware_id)
        .map_err(|_| WsClientError::DataError)?;
    with_timeout(
        Duration::from_secs(5),
        conn.initiate_request(true, Method::Get, &uri, &[("Host", host)]),
    )
    .await
    .map_err(WsClientError::Timeout)?
    .map_err(WsClientError::HttpError)?;
    conn.initiate_response()
        .await
        .map_err(WsClientError::HttpError)?;
    if conn.headers().map_err(WsClientError::HttpError)?.code != 200 {
        return Err(WsClientError::ServerRefused);
    }

    let mut body = [0u8; 256];
    let mut len = 0;
    loop {
        let read = conn
            .read(&mut body[len..])
            .await
            .map_err(WsClientError::HttpError)?;
        if read == 0 {
            break;
        }
        len += read;
        if len == body.len() {
            return Err(WsClientError::DataError);
        }
    }
    let response: ChallengeResponse = serde_json_core::from_slice(&body[..len])
        .map_err(|_| WsClientError::DataError)?
        .0;
    Ok(Challenge {
        nonce: heapless::String::try_from(response.nonce).map_err(|_| WsClientError::DataError)?,
        key_slot: KeySlot::from_block(response.key_slot).ok_or(WsClientError::DataError)?,
    })
}

/// Claims this device into the account of the provisioning token's user,
/// proving it's a genuine device. On success it's claimed from then on and
/// the token is used up.
pub async fn claim(
    conn: &mut Connection<'_>,
    prov_token: &str,
    host: &str,
) -> Result<(), WsClientError> {
    info!("Claiming device with provisioning token");
    let hardware_id = config::get_hardware_id_str();
    let challenge = fetch_challenge(conn, host, &hardware_id).await?;
    let proof = device_auth::claim_proof(
        challenge.key_slot,
        &challenge.nonce,
        &hardware_id,
        prov_token,
    )
    .await
    .map_err(map_device_auth_error)?;
    let proof_hex = device_auth::to_hex(&proof);

    let body = ClaimBody {
        hardware_id: &hardware_id,
        product_ident: CONFIG.product.as_str(),
        model: CONFIG.product.as_model_str(),
        hw_major: CONFIG.hw_version.major,
        hw_minor: CONFIG.hw_version.minor,
        fw_major: CONFIG.fw_version.major,
        fw_minor: CONFIG.fw_version.minor,
        fw_patch: CONFIG.fw_version.patch,
        feed_ident: CONFIG.cfg.data_feed,
        nonce: &challenge.nonce,
        proof: &proof_hex,
    };
    let body_bytes: serde_json_core::heapless::Vec<u8, 1024> =
        serde_json_core::to_vec(&body).map_err(|_| WsClientError::DataError)?;
    let mut content_length: heapless::String<8> = heapless::String::new();
    write!(content_length, "{}", body_bytes.len()).map_err(|_| WsClientError::DataError)?;

    with_timeout(
        Duration::from_secs(5),
        conn.initiate_request(
            true,
            Method::Post,
            CLAIM_ENDPOINT,
            &[
                ("Host", host),
                ("Provisioning-Token", prov_token),
                ("Content-Type", "application/json"),
                ("Content-Length", &content_length),
            ],
        ),
    )
    .await
    .map_err(WsClientError::Timeout)?
    .map_err(WsClientError::HttpError)?;
    conn.write_all(&body_bytes)
        .await
        .map_err(WsClientError::HttpError)?;
    conn.initiate_response()
        .await
        .map_err(WsClientError::HttpError)?;
    let code = conn.headers().map_err(WsClientError::HttpError)?.code;
    if !(200..300).contains(&code) {
        info!("Claim refused with status code {}", code);
        // The provisioning token isn't (or no longer) valid, or the device
        // doesn't pass as genuine: either way, set up again
        return Err(WsClientError::AuthFailed);
    }

    app_settings::persist::update_settings(|set| {
        set.prov_token = None;
        set.claimed = true;
    })
    .await;
    info!("Device claimed");
    Ok(())
}

/// Performs the WebSocket upgrade, authenticated by proving this device's
/// identity over a fresh challenge.
///
/// On success, the connection is ready for authenticated WS communication with the server.
pub async fn websocket_authenticate(
    host: &str,
    conn: &mut Connection<'_>,
    rng: &mut Rng,
) -> Result<(), WsClientError> {
    let hardware_id = config::get_hardware_id_str();
    let challenge = fetch_challenge(conn, host, &hardware_id).await?;
    let proof = device_auth::connect_proof(challenge.key_slot, &challenge.nonce, &hardware_id)
        .await
        .map_err(map_device_auth_error)?;
    let proof_hex = device_auth::to_hex(&proof);

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
    let mut headers_vec: heapless::Vec<(&str, &str), 12> =
        heapless::Vec::from_slice(&headers).unwrap();
    headers_vec.push(("Hardware-Id", &hardware_id)).unwrap();
    headers_vec
        .push(("Device-Nonce", &challenge.nonce))
        .unwrap();
    headers_vec.push(("Device-Proof", &proof_hex)).unwrap();

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
    match conn.headers().map_err(WsClientError::HttpError)?.code {
        101 => {}
        // No account has this device (any more): it needs to be set up again
        404 => return Err(WsClientError::Unlinked),
        // Blocked from connecting
        403 => return Err(WsClientError::Revoked),
        401 => return Err(WsClientError::AuthFailed),
        _ => return Err(WsClientError::ServerRefused),
    }
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
