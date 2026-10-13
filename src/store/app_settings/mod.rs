// Application settings
//   Persisted settings are stored in flash and survive reboots, e.g. WiFi credentials, brightness, color mode.
//   Session settings are kept in RAM and reset on reboot, e.g. server time sync, light on status.
//   Device info is stored in its own flash partition, e.g. product ID and firmware version.
pub mod info;
pub mod persist;
pub mod session;

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};

use crate::store::SharedFlashStorage;

// One per kind of write: requests of one kind merge into one write of the
// latest state, and never replace a request of the other kind
static STORE_SETTINGS_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static STORE_DEVICE_INFO_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub fn spawn(spawner: Spawner, flash_store: &'static SharedFlashStorage) {
    spawner.spawn(app_settings_task(flash_store).unwrap());
}

// Writes to flash in the background
#[embassy_executor::task]
async fn app_settings_task(flash_store: &'static SharedFlashStorage) {
    loop {
        let event = select(
            STORE_SETTINGS_SIGNAL.wait(),
            STORE_DEVICE_INFO_SIGNAL.wait(),
        )
        .await;
        let mut flash_store = flash_store.lock().await;
        match event {
            Either::First(()) => persist::store_settings(&mut flash_store).await,
            Either::Second(()) => info::write_to_flash(&mut flash_store).await,
        }
    }
}
