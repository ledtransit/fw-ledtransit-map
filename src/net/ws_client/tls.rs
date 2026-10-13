// TLS to the gateway, resuming the previous connection's session if it has
// one: the server then skips the certificate and the key exchange, the most of
// the handshake's time on this chip (no elliptic curve accelerator)
use core::net::SocketAddr;

use defmt::info;
use edge_nal::{Close, Readable, TcpConnect, TcpShutdown, TcpSplit};
use edge_nal_tls::{FromTcpSplit, TlsConnector, TlsSocket};
use embassy_time::Instant;
use embedded_io_async::{ErrorType, Read, Write};
use mbedtls_rs::{SavedSession, Session, SessionError};

// Room for a whole HTTP request to the gateway (the largest, the claim, about
// 600 bytes)
const WRITE_BUF_SIZE: usize = 1024;

use crate::profiling;

/// A TLS connector that handshakes on connecting, resuming the saved session.
/// The server falls back to a full handshake if it doesn't know it (any more).
pub struct ResumingTls<'d, T> {
    connector: TlsConnector<'d, T>,
    saved_session: Option<SavedSession>,
}

impl<'d, T> ResumingTls<'d, T> {
    pub fn new(connector: TlsConnector<'d, T>, saved_session: Option<SavedSession>) -> Self {
        Self {
            connector,
            saved_session,
        }
    }
}

impl<T: TcpConnect> TcpConnect for ResumingTls<'_, T> {
    type Error = SessionError;
    type Socket<'a>
        = BufferedTlsSocket<'a, T::Socket<'a>>
    where
        Self: 'a;

    async fn connect(&self, remote: SocketAddr) -> Result<Self::Socket<'_>, Self::Error> {
        let mut socket = self.connector.connect(remote).await?;
        let started = Instant::now();
        match &self.saved_session {
            Some(saved_session) => {
                socket
                    .session_mut()
                    .connect_with_session(saved_session)
                    .await?
            }
            None => socket.session_mut().connect().await?,
        }
        info!(
            "TLS handshake took {} ms (resuming: {})",
            started.elapsed().as_millis(),
            self.saved_session.is_some()
        );
        profiling::once!(gateway_tls_handshake_done);
        Ok(BufferedTlsSocket {
            socket,
            buf: heapless::Vec::new(),
        })
    }
}

/// A TLS socket sending what's written once flushed (or once it's more than
/// fits), in one TLS record: else each write is a record of its own, and an
/// HTTP request is written in pieces (each header's name, ": ", value...).
/// Those dozens of small records take round trips to send, as the server
/// acknowledges them late (delayed ACKs) while the next ones wait for it
/// (Nagle's algorithm, the small send buffer).
pub struct BufferedTlsSocket<'d, T>
where
    T: Read + Write,
{
    socket: TlsSocket<'d, T>,
    buf: heapless::Vec<u8, WRITE_BUF_SIZE>,
}

impl<'d, T> BufferedTlsSocket<'d, T>
where
    T: Read + Write,
{
    /// The TLS session, also for what's written next. Flush first.
    pub fn session_mut(&mut self) -> &mut Session<'d, FromTcpSplit<T>> {
        debug_assert!(self.buf.is_empty(), "Written data not flushed");
        self.socket.session_mut()
    }

    async fn send_buffered(&mut self) -> Result<(), SessionError> {
        if !self.buf.is_empty() {
            self.socket.write_all(&self.buf).await?;
            self.buf.clear();
        }
        Ok(())
    }
}

impl<T> ErrorType for BufferedTlsSocket<'_, T>
where
    T: Read + Write,
{
    type Error = SessionError;
}

impl<T> Read for BufferedTlsSocket<'_, T>
where
    T: Read + Write,
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        // What's written is sent before waiting for the answer to it
        self.send_buffered().await?;
        self.socket.read(buf).await
    }
}

impl<T> Write for BufferedTlsSocket<'_, T>
where
    T: Read + Write,
{
    async fn write(&mut self, data: &[u8]) -> Result<usize, Self::Error> {
        if self.buf.len() + data.len() > WRITE_BUF_SIZE {
            self.send_buffered().await?;
        }
        if data.len() > WRITE_BUF_SIZE {
            return self.socket.write(data).await;
        }
        // Fits: there's room for it now
        self.buf.extend_from_slice(data).ok();
        Ok(data.len())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.send_buffered().await?;
        self.socket.flush().await
    }
}

impl<T> Readable for BufferedTlsSocket<'_, T>
where
    T: Read + Write + Readable,
{
    async fn readable(&mut self) -> Result<(), Self::Error> {
        self.socket.readable().await
    }
}

// Not supported by the TLS socket either (it panics): the session splits
impl<'d, T> TcpSplit for BufferedTlsSocket<'d, T>
where
    T: Read + Write + Readable + TcpSplit,
{
    type Read<'a>
        = <TlsSocket<'d, T> as TcpSplit>::Read<'a>
    where
        Self: 'a;

    type Write<'a>
        = <TlsSocket<'d, T> as TcpSplit>::Write<'a>
    where
        Self: 'a;

    fn split(&mut self) -> (Self::Read<'_>, Self::Write<'_>) {
        self.socket.split()
    }
}

impl<T> TcpShutdown for BufferedTlsSocket<'_, T>
where
    T: Read + Write + TcpShutdown,
{
    async fn close(&mut self, what: Close) -> Result<(), Self::Error> {
        self.send_buffered().await.ok();
        self.socket.close(what).await
    }

    async fn abort(&mut self) -> Result<(), Self::Error> {
        self.socket.abort().await
    }
}
