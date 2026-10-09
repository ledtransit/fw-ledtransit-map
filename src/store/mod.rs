pub mod app_settings;
pub mod transit_data;

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_bootloader_esp_idf::partitions::{
    self, DataPartitionSubType, FlashRegion, PARTITION_TABLE_MAX_LEN, PartitionType,
};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;

use crate::mk_static;

pub type SharedFlashStorage = Mutex<CriticalSectionRawMutex, FlashStorage<'static>>;

pub async fn init(flash_peri: FLASH<'static>) -> &'static SharedFlashStorage {
    let mut flash_store = FlashStorage::new(flash_peri);
    app_settings::persist::init(&mut flash_store).await;
    mk_static!(SharedFlashStorage, Mutex::new(flash_store))
}

/// Runs `f` on the data partition with the label (see partitions.csv).
pub fn with_data_partition<'d, R>(
    flash_store: &mut FlashStorage<'d>,
    label: &str,
    f: impl FnOnce(&mut FlashRegion<'_, 'd>) -> R,
) -> R {
    let mut pt_mem = [0u8; PARTITION_TABLE_MAX_LEN];
    let partition_table = partitions::read_partition_table(flash_store, &mut pt_mem).unwrap();
    let mut storage = partition_table
        .iter()
        .find(|part| {
            part.partition_type() == PartitionType::Data(DataPartitionSubType::LittleFs)
                && part.label_as_str() == label
        })
        .unwrap_or_else(|| panic!("Partition {} not found", label))
        .as_flash_region(flash_store);
    f(&mut storage)
}
