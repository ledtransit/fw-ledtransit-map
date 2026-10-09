// Sending WebSocket frames (masked, as a client must)
use defmt::{debug, info};
use edge_ws::{FrameHeader, FrameType};
use embedded_io_async::Write;
use esp_hal::rng::Rng;

use super::{SocketError, WsClientError};

pub async fn send_binary<W>(tx: &mut W, payload: &[u8], rng: &mut Rng) -> Result<(), WsClientError>
where
    W: Write<Error = SocketError>,
{
    send_frame(tx, FrameType::Binary(false), payload, rng).await?;
    debug!("WS: Sent binary frame");
    Ok(())
}

pub async fn send_pong<W>(tx: &mut W, rng: &mut Rng) -> Result<(), WsClientError>
where
    W: Write<Error = SocketError>,
{
    send_frame(tx, FrameType::Pong, &[], rng).await?;
    debug!("WS: Sent pong frame");
    Ok(())
}

pub async fn send_close<W>(tx: &mut W, rng: &mut Rng) -> Result<(), WsClientError>
where
    W: Write<Error = SocketError>,
{
    let header = frame_header(FrameType::Close, 0, rng);
    info!("WS: Sending close frame");
    header.send(&mut *tx).await.map_err(WsClientError::WsError)
}

async fn send_frame<W>(
    tx: &mut W,
    frame_type: FrameType,
    payload: &[u8],
    rng: &mut Rng,
) -> Result<(), WsClientError>
where
    W: Write<Error = SocketError>,
{
    let header = frame_header(frame_type, payload.len(), rng);
    header
        .send(&mut *tx)
        .await
        .map_err(WsClientError::WsError)?;
    header
        .send_payload(&mut *tx, payload)
        .await
        .map_err(WsClientError::WsError)
}

fn frame_header(frame_type: FrameType, payload_len: usize, rng: &mut Rng) -> FrameHeader {
    FrameHeader {
        frame_type,
        payload_len: payload_len as _,
        mask_key: rng.random().into(),
    }
}
