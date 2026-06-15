pub mod app_settings;
pub mod ota;
pub mod transit_data;

use crate::mk_static;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;

pub type SharedFlashStorage = Mutex<CriticalSectionRawMutex, FlashStorage<'static>>;

pub async fn init(flash_peri: FLASH<'static>) -> &'static SharedFlashStorage {
    let mut flash_store = FlashStorage::new(flash_peri);

    // Init flash settings
    app_settings::persist::init(&mut flash_store).await;

    mk_static!(SharedFlashStorage, Mutex::new(flash_store))
}
