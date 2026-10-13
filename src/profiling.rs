// Boot profiling for development: when each step from reset to the first
// transit data painted was first reached (and the free heap then), sent to the
// gateway once finished. Recorded with `once!(point)` and `count!(counter)`,
// the fields of DeviceProfiling, and logged.
//
// Kept here, not in the session settings: those are copied whole on every
// read, also into the tasks keeping a copy, which takes the RAM from the stack
use core::{
    cell::RefCell,
    net::SocketAddr,
    sync::atomic::{AtomicBool, Ordering},
};

use critical_section::Mutex;
use defmt::debug;
use edge_nal::{Close, Readable, TcpConnect, TcpShutdown, TcpSplit};
use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{ErrorType, Read, Write};
use esp_hal::timer::systimer::{SystemTimer, Unit};

use crate::{
    net::ws_client::{
        self,
        client_proto::{DeviceProfiling, ProfilingPoint},
    },
    stats,
};

// The system timer counts from reset, at 16 MHz on the ESP32-C3 (from the 40
// MHz crystal). Read directly: esp-hal only converts it once esp_hal::init ran
const SYSTIMER_TICKS_PER_MICRO: u64 = 16;

// The executor held up for longer than this at a time counts as stalled: busy
// computing, or preempted by a higher-priority thread (e.g. the WiFi driver's)
const STALL_MONITOR_INTERVAL: Duration = Duration::from_millis(1);
const STALL_THRESHOLD: Duration = Duration::from_millis(1);

static PROFILING: Mutex<RefCell<Option<DeviceProfiling>>> = Mutex::new(RefCell::new(None));
// Set once finished: recording does nothing from then on, not even lock
static FINISHED: AtomicBool = AtomicBool::new(false);

/// Records the point (a field of DeviceProfiling) as reached now, or at the
/// point given, unless reached before.
macro_rules! once {
    ($point:ident) => {
        $crate::profiling::once!($point, $crate::profiling::now())
    };
    ($point:ident, $at:expr) => {
        $crate::profiling::record(stringify!($point), |profiling| &mut profiling.$point, $at)
    };
}

/// Counts one more for the counter (a field of DeviceProfiling).
macro_rules! count {
    ($counter:ident) => {
        $crate::profiling::update(|profiling| profiling.$counter += 1)
    };
}

pub(crate) use {count, once};

pub fn spawn(spawner: Spawner) {
    spawner.spawn(stall_monitor_task().unwrap());
}

pub fn now() -> ProfilingPoint {
    ProfilingPoint {
        micros_since_boot: SystemTimer::unit_value(Unit::Unit0) / SYSTIMER_TICKS_PER_MICRO,
        free_heap_bytes: esp_alloc::HEAP.free() as u32,
        executor_stalled_micros: stats::EXECUTOR_STALLED_MICROS.get() as u32,
    }
}

pub fn record(
    name: &str,
    point: fn(&mut DeviceProfiling) -> &mut Option<ProfilingPoint>,
    at: ProfilingPoint,
) {
    let mut recorded = false;
    update(|profiling| {
        let point = point(profiling);
        if point.is_none() {
            *point = Some(at);
            recorded = true;
        }
    });
    // Logged outside the critical section: the logger takes its own lock,
    // which isn't reentrant
    if recorded {
        debug!(
            "Profiling: {} at {} us ({} us stalled, {} B heap free)",
            name, at.micros_since_boot, at.executor_stalled_micros, at.free_heap_bytes
        );
    }
}

/// Updates the profiling, until it finished: once the first transit data is
/// painted, or its painting skipped (light off).
pub fn update(f: impl FnOnce(&mut DeviceProfiling)) {
    if FINISHED.load(Ordering::Relaxed) {
        return;
    }
    let just_finished = critical_section::with(|cs| {
        let mut profiling = PROFILING.borrow_ref_mut(cs);
        let profiling = profiling.get_or_insert_with(Default::default);
        f(profiling);
        let finished = profiling.first_transit_data_painted.is_some()
            || profiling.first_transit_data_paint_skipped.is_some();
        FINISHED.store(finished, Ordering::Relaxed);
        finished
    });
    if just_finished {
        debug!("Profiling: finished, sending");
        ws_client::send_profiling();
    }
}

/// The finished profiling, taken out: it's sent once.
pub fn take_finished() -> Option<DeviceProfiling> {
    critical_section::with(|cs| {
        FINISHED
            .load(Ordering::Relaxed)
            .then(|| PROFILING.borrow_ref_mut(cs).take())
            .flatten()
    })
}

// Adds up how late it wakes: the time the executor couldn't run any task.
// Also after profiling finished, for the telemetry
#[embassy_executor::task]
async fn stall_monitor_task() {
    loop {
        let wake_at = Instant::now() + STALL_MONITOR_INTERVAL;
        Timer::at(wake_at).await;
        let late = Instant::now().saturating_duration_since(wake_at);
        if late > STALL_THRESHOLD {
            stats::EXECUTOR_STALLED_MICROS.add(late.as_micros());
        }
    }
}

/// The gateway's TCP connector (under TLS), recording when it connected.
#[derive(Clone, Copy)]
pub struct TimedTcp<T>(pub T);

impl<T: TcpConnect> TcpConnect for TimedTcp<T> {
    type Error = T::Error;
    type Socket<'a>
        = TimedSocket<T::Socket<'a>>
    where
        Self: 'a;

    async fn connect(&self, remote: SocketAddr) -> Result<Self::Socket<'_>, Self::Error> {
        let socket = self.0.connect(remote).await?;
        once!(gateway_tcp_connected);
        Ok(TimedSocket {
            inner: socket,
            flight: Flight::ClientHello,
            handshake_header: None,
        })
    }
}

/// The gateway's TCP socket, under TLS: records when the TLS 1.2 handshake's
/// flights go out and come in, from where writes and reads take turns. The
/// device processes the server's flight as it reads it, record by record (a
/// 5 byte header read, then the body): it asks for the server key exchange
/// once the certificate chain is checked, and for the last record once the
/// signature is.
pub struct TimedSocket<S> {
    inner: S,
    flight: Flight,
    // When the last handshake record header was asked for, if just read
    handshake_header: Option<ProfilingPoint>,
}

const TLS_CONTENT_TYPE_HANDSHAKE: u8 = 22;
const TLS_HANDSHAKE_SERVER_KEY_EXCHANGE: u8 = 12;

enum Flight {
    ClientHello,
    ServerHello,
    // With the server's last read so far
    Server(ProfilingPoint),
    Done,
}

impl<S: ErrorType> ErrorType for TimedSocket<S> {
    type Error = S::Error;
}

impl<S: Read> Read for TimedSocket<S> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if !matches!(self.flight, Flight::ServerHello | Flight::Server(_)) {
            return self.inner.read(buf).await;
        }

        let asked = now();
        let len = self.inner.read(buf).await?;
        if len == 0 {
            return Ok(len);
        }
        if matches!(self.flight, Flight::ServerHello) {
            once!(gateway_tls_server_hello_received);
        }
        if let Some(header_asked) = self.handshake_header.take()
            && buf[0] == TLS_HANDSHAKE_SERVER_KEY_EXCHANGE
        {
            once!(gateway_tls_certificate_checked, header_asked);
        }
        if len == 5 && buf[0] == TLS_CONTENT_TYPE_HANDSHAKE {
            self.handshake_header = Some(asked);
        }
        self.flight = Flight::Server(now());
        Ok(len)
    }
}

impl<S: Write> Write for TimedSocket<S> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        match core::mem::replace(&mut self.flight, Flight::Done) {
            Flight::ClientHello => {
                once!(gateway_tls_client_hello_sent);
                self.flight = Flight::ServerHello;
            }
            Flight::Server(last_read) => {
                once!(gateway_tls_server_flight_processed, last_read);
                once!(gateway_tls_client_key_exchange_sent);
            }
            flight => self.flight = flight,
        }
        self.inner.write(buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush().await
    }
}

impl<S: Readable> Readable for TimedSocket<S> {
    async fn readable(&mut self) -> Result<(), Self::Error> {
        self.inner.readable().await
    }
}

impl<S: TcpSplit> TcpSplit for TimedSocket<S> {
    type Read<'a>
        = S::Read<'a>
    where
        Self: 'a;
    type Write<'a>
        = S::Write<'a>
    where
        Self: 'a;

    fn split(&mut self) -> (Self::Read<'_>, Self::Write<'_>) {
        self.inner.split()
    }
}

impl<S: TcpShutdown> TcpShutdown for TimedSocket<S> {
    async fn close(&mut self, what: Close) -> Result<(), Self::Error> {
        self.inner.close(what).await
    }

    async fn abort(&mut self) -> Result<(), Self::Error> {
        self.inner.abort().await
    }
}
