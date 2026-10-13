// Receiving a frame's payload through the receive buffer as a window, and
// transit data decoded while it's received: each line, stop, vehicle and
// disruption on its own, straight into the store's layout. Transit data is
// by far the largest message, and only a single one of its parts has to fit
// into the buffer at once (not the whole message, nor all of it decoded next
// to it)
use alloc::string::String;

use edge_ws::FrameHeader;
use embedded_io_async::Read;
use prost::Message;

use super::{
    SocketError, WsClientError,
    client_proto::{Disruption, Line, Stop, VehicleMovement},
};
use crate::store::transit_data::TransitData;

// Protobuf wire types
const WIRE_VARINT: u64 = 0;
const WIRE_I64: u64 = 1;
const WIRE_LEN: u64 = 2;
const WIRE_I32: u64 = 5;

/// A frame's payload, received into the buffer as needed.
pub struct FrameReader<'a, R> {
    rx: &'a mut R,
    header: &'a FrameHeader,
    buf: &'a mut [u8],
    // Received but not consumed yet
    start: usize,
    end: usize,
    received: usize,
}

impl<'a, R> FrameReader<'a, R>
where
    R: Read<Error = SocketError>,
{
    pub fn new(rx: &'a mut R, header: &'a FrameHeader, buf: &'a mut [u8]) -> Self {
        Self {
            rx,
            header,
            buf,
            start: 0,
            end: 0,
            received: 0,
        }
    }

    /// Bytes of the payload not consumed yet.
    pub fn remaining(&self) -> usize {
        self.header.payload_len as usize - self.received + (self.end - self.start)
    }

    /// The next bytes, received first if needed, without consuming them.
    pub async fn peek(&mut self, len: usize) -> Result<&[u8], WsClientError> {
        self.fill(len).await?;
        Ok(&self.buf[self.start..self.start + len])
    }

    /// The next bytes, consumed. Must fit into the buffer.
    pub async fn take(&mut self, len: usize) -> Result<&[u8], WsClientError> {
        self.fill(len).await?;
        let start = self.start;
        self.start += len;
        Ok(&self.buf[start..start + len])
    }

    /// Consumes the next bytes (any number), receiving them if needed.
    pub async fn skip(&mut self, mut len: usize) -> Result<(), WsClientError> {
        while len > 0 {
            let chunk = len.min(self.buf.len());
            self.take(chunk).await?;
            len -= chunk;
        }
        Ok(())
    }

    async fn varint(&mut self) -> Result<u64, WsClientError> {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1).await?[0];
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(WsClientError::DataError)
    }

    // At least the bytes buffered (all received if they're the payload's last)
    async fn fill(&mut self, len: usize) -> Result<(), WsClientError> {
        if len > self.buf.len() || len > self.remaining() {
            return Err(WsClientError::DataError);
        }
        if self.start + len > self.buf.len() {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        while self.end - self.start < len {
            let unreceived = self.header.payload_len as usize - self.received;
            let space = (self.buf.len() - self.end).min(unreceived);
            let read = self
                .rx
                .read(&mut self.buf[self.end..self.end + space])
                .await
                .map_err(|e| WsClientError::WsError(edge_ws::Error::Io(e)))?;
            if read == 0 {
                return Err(WsClientError::DataError);
            }
            self.header
                .mask(&mut self.buf[self.end..self.end + read], self.received);
            self.end += read;
            self.received += read;
        }
        Ok(())
    }
}

/// The transit data of a ClientMessage, its tag consumed already (the version
/// before it was checked).
pub async fn decode_transit_data<R>(
    reader: &mut FrameReader<'_, R>,
) -> Result<TransitData, WsClientError>
where
    R: Read<Error = SocketError>,
{
    let len = reader.varint().await? as usize;
    let end = reader
        .remaining()
        .checked_sub(len)
        .ok_or(WsClientError::DataError)?;

    let mut data = TransitData::default();
    while reader.remaining() > end {
        let key = reader.varint().await?;
        let (field, wire_type) = (key >> 3, key & 0b111);
        if wire_type == WIRE_VARINT {
            let value = reader.varint().await?;
            match field {
                1 => data.sourced_at_unix_timestamp = value as u32,
                2 => data.simulated_until_unix_timestamp = value as u32,
                _ => {}
            }
            continue;
        }
        let len = match wire_type {
            WIRE_LEN => reader.varint().await? as usize,
            WIRE_I64 => 8,
            WIRE_I32 => 4,
            _ => return Err(WsClientError::DataError),
        };
        if wire_type != WIRE_LEN || !(3..=7).contains(&field) {
            reader.skip(len).await?;
            continue;
        }
        let part = reader.take(len).await?;
        let decoded = match field {
            3 => String::from_utf8(part.into())
                .map(|feed_source| data.feed_source = feed_source)
                .is_ok(),
            4 => Line::decode(part).map(|line| data.lines.push(line)).is_ok(),
            5 => Stop::decode(part)
                .ok()
                .is_some_and(|stop| data.push_stop(stop).is_ok()),
            6 => VehicleMovement::decode(part)
                .ok()
                .is_some_and(|vehicle| data.push_vehicle(vehicle).is_ok()),
            _ => Disruption::decode(part)
                .ok()
                .is_some_and(|disruption| data.push_disruption(disruption).is_ok()),
        };
        if !decoded {
            return Err(WsClientError::DataError);
        }
    }
    if reader.remaining() != end {
        return Err(WsClientError::DataError);
    }
    // Nothing else is expected after it
    reader.skip(end).await?;
    data.shrink_to_fit();
    Ok(data)
}
