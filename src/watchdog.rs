// Hardware watchdog (RTC watchdog): resets the chip when the firmware hangs.
// Fed only while the LED draw loop makes progress, which runs at all times,
// also without WiFi or setup: a task blocking the executor, and the draw loop
// stuck on an await, both stop the feeding. A reset before the boot check also
// rolls back a newly installed firmware (see ota).

use core::sync::atomic::{AtomicU32, Ordering};

use defmt::warn;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::{
    peripherals::LPWR,
    rtc_cntl::{Rtc, Rwdt, RwdtStage},
};

/// No progress for this long resets the chip
const TIMEOUT: esp_hal::time::Duration = esp_hal::time::Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(5);

static HEARTBEAT: AtomicU32 = AtomicU32::new(0);

/// Signals progress, from the draw loop on every frame.
pub fn heartbeat() {
    // Load and store only: no atomic read-modify-write on this target, and
    // only the draw task calls this
    let count = HEARTBEAT.load(Ordering::Relaxed);
    HEARTBEAT.store(count.wrapping_add(1), Ordering::Relaxed);
}

pub fn spawn(spawner: Spawner, lpwr: LPWR<'static>) {
    let mut rwdt = Rtc::new(lpwr).rwdt;
    rwdt.set_timeout(RwdtStage::Stage0, TIMEOUT);
    rwdt.enable(); // Stage 0 resets the system
    rwdt.feed();
    spawner.spawn(watchdog_task(rwdt).unwrap());
}

#[embassy_executor::task]
async fn watchdog_task(mut rwdt: Rwdt) {
    let mut last_heartbeat = HEARTBEAT.load(Ordering::Relaxed);
    loop {
        Timer::after(CHECK_INTERVAL).await;
        let heartbeat = HEARTBEAT.load(Ordering::Relaxed);
        if heartbeat != last_heartbeat {
            rwdt.feed();
            last_heartbeat = heartbeat;
        } else {
            warn!("Watchdog: no progress of the draw loop, not feeding");
        }
    }
}
