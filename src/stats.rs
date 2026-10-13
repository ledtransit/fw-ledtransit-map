// Device counters since boot, sent with the telemetry. Only ever counting
// up: the gateway derives rates from the difference between two telemetry
// messages, over any window. A reboot resets them, which the uptime shows
// (network counters: see net::stats)
use core::{
    cell::Cell,
    sync::atomic::{AtomicU32, Ordering},
};

use critical_section::Mutex;

// Settings written to flash (only changes are): one too many wears it out
pub static SETTINGS_WRITES: Counter = Counter::new();
// How long the executor couldn't run tasks (over 1 ms at a time), busy
// computing or preempted (e.g. by the WiFi driver), see profiling
pub static EXECUTOR_STALLED_MICROS: Counter = Counter::new();
// Each button, pressed down (not bounces)
pub static BUTTON_PRESSES: Counter = Counter::new();

// Boots ever, counted in flash (see store::boot_count)
static BOOT_COUNT: AtomicU32 = AtomicU32::new(0);

/// 64 bit: microseconds or bytes wouldn't fit into 32 bit for long. Without
/// 64 bit atomics on this chip, in a critical section.
pub struct Counter(Mutex<Cell<u64>>);

impl Counter {
    pub const fn new() -> Self {
        Self(Mutex::new(Cell::new(0)))
    }

    pub fn add(&self, n: u64) {
        critical_section::with(|cs| {
            let count = self.0.borrow(cs);
            count.set(count.get().wrapping_add(n));
        });
    }

    pub fn inc(&self) {
        self.add(1);
    }

    pub fn get(&self) -> u64 {
        critical_section::with(|cs| self.0.borrow(cs).get())
    }
}

pub fn set_boot_count(count: u32) {
    BOOT_COUNT.store(count, Ordering::Relaxed);
}

/// Boots ever, this one included (0 if it couldn't be counted).
pub fn boot_count() -> u32 {
    BOOT_COUNT.load(Ordering::Relaxed)
}
