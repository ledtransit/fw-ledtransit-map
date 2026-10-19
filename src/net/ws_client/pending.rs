// Messages that only report the device's current state: requesting one that's
// still pending doesn't queue it again, so a burst of requests (e.g. while the
// connection is down) sends it once, with the state at the time of sending
use core::cell::Cell;

use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};

#[derive(Clone, Copy, PartialEq, Eq, defmt::Format)]
#[repr(u8)]
pub enum StateMessage {
    Pong,
    TimeSync,
    Status,
    Config,
    Info,
    Telemetry,
    Errors,
    Profiling,
}

impl StateMessage {
    // Sending order when several are pending: the keepalive first, then the
    // time sync, which measures the round trip from when it's sent
    const BY_PRIORITY: [Self; 8] = [
        Self::Pong,
        Self::TimeSync,
        Self::Status,
        Self::Config,
        Self::Info,
        Self::Telemetry,
        Self::Errors,
        Self::Profiling,
    ];

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// The set of pending state messages, for a single receiver.
pub struct PendingMessages {
    pending: Mutex<CriticalSectionRawMutex, Cell<u8>>,
    // Raised whenever a message is added; may be stale, the set is what counts
    added: Signal<CriticalSectionRawMutex, ()>,
}

impl PendingMessages {
    pub const fn new() -> Self {
        Self {
            pending: Mutex::new(Cell::new(0)),
            added: Signal::new(),
        }
    }

    /// Marks the message as pending, if it isn't already.
    pub fn request(&self, message: StateMessage) {
        self.pending
            .lock(|pending| pending.set(pending.get() | message.bit()));
        self.added.signal(());
    }

    pub fn clear(&self) {
        self.pending.lock(|pending| pending.set(0));
        self.added.reset();
    }

    /// Takes the pending message to send next, waiting for one if none is.
    pub async fn next(&self) -> StateMessage {
        loop {
            if let Some(message) = self.take() {
                return message;
            }
            self.added.wait().await;
        }
    }

    fn take(&self) -> Option<StateMessage> {
        self.pending.lock(|pending| {
            let bits = pending.get();
            let message = StateMessage::BY_PRIORITY
                .into_iter()
                .find(|message| bits & message.bit() != 0)?;
            pending.set(bits & !message.bit());
            Some(message)
        })
    }
}
