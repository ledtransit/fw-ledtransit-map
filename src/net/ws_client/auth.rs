// Device authentication to the gateway: proofs with the device keys over the
// gateway's challenges, to claim the device and to connect
use core::fmt::Write as _;

use defmt::info;
use edge_http::{
    Method,
    ws::{self, MAX_BASE64_KEY_LEN, MAX_BASE64_KEY_RESPONSE_LEN, NONCE_LEN},
};
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read, Write};
use esp_hal::rng::Rng;

use super::{Connection, WsClientError};
use crate::{
    config::{self, CONFIG},
    device_auth::{self, DeviceAuthError, KeySlot},
    store::app_settings,
};

const CHALLENGE_ENDPOINT: &str = "/challenge";
const CLAIM_ENDPOINT: &str = "/claim";
const WS_ENDPOINT: &str = "/ws";

const MAX_NONCE_LEN: usize = 128;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

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

    send_request(
        conn,
        Method::Post,
        CLAIM_ENDPOINT,
        &[
            ("Host", host),
            ("Provisioning-Token", prov_token),
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
    )
    .await?;
    conn.write_all(&body_bytes)
        .await
        .map_err(WsClientError::HttpError)?;
    let code = response_code(conn).await?;
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

    let mut nonce = [0u8; NONCE_LEN];
    for byte in nonce.iter_mut() {
        *byte = rng.random() as u8;
    }
    let mut nonce_b64_buf = [0u8; MAX_BASE64_KEY_LEN];
    let upgrade_headers = ws::upgrade_request_headers(
        Some(host),
        Some("ledtransit-client"),
        None,
        &nonce,
        &mut nonce_b64_buf,
    );
    let mut headers: heapless::Vec<(&str, &str), 12> =
        heapless::Vec::from_slice(&upgrade_headers).unwrap();
    headers.push(("Hardware-Id", &hardware_id)).unwrap();
    headers.push(("Device-Nonce", &challenge.nonce)).unwrap();
    headers.push(("Device-Proof", &proof_hex)).unwrap();

    send_request(conn, Method::Get, WS_ENDPOINT, &headers).await?;
    match response_code(conn).await? {
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

async fn fetch_challenge(
    conn: &mut Connection<'_>,
    host: &str,
    hardware_id: &str,
) -> Result<Challenge, WsClientError> {
    let mut uri: heapless::String<96> = heapless::String::new();
    write!(uri, "{}?hardware_id={}", CHALLENGE_ENDPOINT, hardware_id)
        .map_err(|_| WsClientError::DataError)?;
    send_request(conn, Method::Get, &uri, &[("Host", host)]).await?;
    if response_code(conn).await? != 200 {
        return Err(WsClientError::ServerRefused);
    }

    let mut body = [0u8; 256];
    let len = read_body(conn, &mut body).await?;
    let (response, _): (ChallengeResponse, _) =
        serde_json_core::from_slice(&body[..len]).map_err(|_| WsClientError::DataError)?;
    Ok(Challenge {
        nonce: heapless::String::try_from(response.nonce).map_err(|_| WsClientError::DataError)?,
        key_slot: KeySlot::from_block(response.key_slot).ok_or(WsClientError::DataError)?,
    })
}

async fn send_request(
    conn: &mut Connection<'_>,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> Result<(), WsClientError> {
    with_timeout(
        REQUEST_TIMEOUT,
        conn.initiate_request(true, method, uri, headers),
    )
    .await
    .map_err(WsClientError::Timeout)?
    .map_err(WsClientError::HttpError)
}

async fn response_code(conn: &mut Connection<'_>) -> Result<u16, WsClientError> {
    conn.initiate_response()
        .await
        .map_err(WsClientError::HttpError)?;
    Ok(conn.headers().map_err(WsClientError::HttpError)?.code)
}

// The whole body: an error if it doesn't fit (with room to spare)
async fn read_body(conn: &mut Connection<'_>, body: &mut [u8]) -> Result<usize, WsClientError> {
    let mut len = 0;
    loop {
        let read = conn
            .read(&mut body[len..])
            .await
            .map_err(WsClientError::HttpError)?;
        if read == 0 {
            return Ok(len);
        }
        len += read;
        if len == body.len() {
            return Err(WsClientError::DataError);
        }
    }
}

fn map_device_auth_error(error: DeviceAuthError) -> WsClientError {
    match error {
        DeviceAuthError::NoKey => WsClientError::NoDeviceKeys,
        DeviceAuthError::NotInitialized => WsClientError::DataError,
    }
}
