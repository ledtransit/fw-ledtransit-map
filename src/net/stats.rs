// Network counters since boot, sent with the telemetry (see stats): rates
// derived from them are zero while nothing is received
use core::{
    cell::Cell,
    net::SocketAddr,
    sync::atomic::{AtomicU32, Ordering},
};

use critical_section::Mutex;
use edge_nal::{Close, Readable, TcpConnect, TcpShutdown, TcpSplit};
use embassy_time::Instant;
use embedded_io_async::{ErrorType, Read, Write};

use crate::stats::Counter;

pub static GATEWAY_TRAFFIC: Traffic = Traffic::new();
pub static OTA_TRAFFIC: Traffic = Traffic::new();
pub static GATEWAY_CONNECTIONS: Counter = Counter::new();
pub static WIFI_CONNECTIONS: Counter = Counter::new();
// Transit data received and stored, and failed to (decode, timeout, too large)
pub static TRANSIT_DATA_COUNT: Counter = Counter::new();
pub static TRANSIT_DATA_BYTES: Counter = Counter::new();
pub static TRANSIT_DATA_ERRORS: Counter = Counter::new();

// The last reason the WiFi station was disconnected, or failed to connect
// (ESP-IDF's wifi_err_reason_t), 0 without one
static WIFI_LAST_DISCONNECT_REASON: AtomicU32 = AtomicU32::new(0);

// When the current gateway connection was established, if connected
static GATEWAY_CONNECTED_AT: Mutex<Cell<Option<Instant>>> = Mutex::new(Cell::new(None));

/// Bytes of a connection's TCP stream (TLS records, or HTTP and WebSocket
/// frames without TLS), not counting the TCP/IP headers.
pub struct Traffic {
    pub rx_bytes: Counter,
    pub tx_bytes: Counter,
}

impl Traffic {
    pub const fn new() -> Self {
        Self {
            rx_bytes: Counter::new(),
            tx_bytes: Counter::new(),
        }
    }
}

pub fn on_wifi_disconnected(reason: u16) {
    WIFI_LAST_DISCONNECT_REASON.store(reason.into(), Ordering::Relaxed);
}

pub fn wifi_last_disconnect_reason() -> u32 {
    WIFI_LAST_DISCONNECT_REASON.load(Ordering::Relaxed)
}

pub fn on_gateway_connected() {
    GATEWAY_CONNECTIONS.inc();
    critical_section::with(|cs| GATEWAY_CONNECTED_AT.borrow(cs).set(Some(Instant::now())));
}

pub fn on_gateway_disconnected() {
    critical_section::with(|cs| GATEWAY_CONNECTED_AT.borrow(cs).set(None));
}

/// How long the current gateway connection is up, 0 if not connected.
pub fn gateway_connected_seconds() -> u32 {
    critical_section::with(|cs| GATEWAY_CONNECTED_AT.borrow(cs).get())
        .map_or(0, |connected_at| connected_at.elapsed().as_secs() as u32)
}

/// A TCP connector whose sockets count their traffic.
#[derive(Clone, Copy)]
pub struct CountingTcp<T> {
    inner: T,
    traffic: &'static Traffic,
}

impl<T> CountingTcp<T> {
    pub fn new(inner: T, traffic: &'static Traffic) -> Self {
        Self { inner, traffic }
    }
}

impl<T: TcpConnect> TcpConnect for CountingTcp<T> {
    type Error = T::Error;
    type Socket<'a>
        = CountingSocket<T::Socket<'a>>
    where
        Self: 'a;

    async fn connect(&self, remote: SocketAddr) -> Result<Self::Socket<'_>, Self::Error> {
        let socket = self.inner.connect(remote).await?;
        Ok(CountingSocket {
            inner: socket,
            traffic: self.traffic,
        })
    }
}

/// A socket, or one of its halves once split, counting its traffic.
pub struct CountingSocket<S> {
    inner: S,
    traffic: &'static Traffic,
}

impl<S: ErrorType> ErrorType for CountingSocket<S> {
    type Error = S::Error;
}

impl<S: Read> Read for CountingSocket<S> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let len = self.inner.read(buf).await?;
        self.traffic.rx_bytes.add(len as u64);
        Ok(len)
    }
}

impl<S: Write> Write for CountingSocket<S> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let len = self.inner.write(buf).await?;
        self.traffic.tx_bytes.add(len as u64);
        Ok(len)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush().await
    }
}

impl<S: Readable> Readable for CountingSocket<S> {
    async fn readable(&mut self) -> Result<(), Self::Error> {
        self.inner.readable().await
    }
}

impl<S: TcpSplit> TcpSplit for CountingSocket<S> {
    type Read<'a>
        = CountingSocket<S::Read<'a>>
    where
        Self: 'a;
    type Write<'a>
        = CountingSocket<S::Write<'a>>
    where
        Self: 'a;

    fn split(&mut self) -> (Self::Read<'_>, Self::Write<'_>) {
        let (read, write) = self.inner.split();
        (
            CountingSocket {
                inner: read,
                traffic: self.traffic,
            },
            CountingSocket {
                inner: write,
                traffic: self.traffic,
            },
        )
    }
}

impl<S: TcpShutdown> TcpShutdown for CountingSocket<S> {
    async fn close(&mut self, what: Close) -> Result<(), Self::Error> {
        self.inner.close(what).await
    }

    async fn abort(&mut self) -> Result<(), Self::Error> {
        self.inner.abort().await
    }
}
