// TLS to the gateway, resuming the previous connection's session if it has
// one: the server then skips the certificate and the key exchange, the most of
// the handshake's time on this chip (no elliptic curve accelerator)
use core::net::SocketAddr;

use defmt::info;
use edge_nal::TcpConnect;
use edge_nal_tls::{TlsConnector, TlsSocket};
use embassy_time::Instant;
use mbedtls_rs::{SavedSession, SessionError};

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
        = TlsSocket<'a, T::Socket<'a>>
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
        Ok(socket)
    }
}
