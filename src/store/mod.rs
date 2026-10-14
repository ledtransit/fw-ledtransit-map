pub mod app_settings;
pub mod boot_count;
pub mod transit_data;

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_bootloader_esp_idf::partitions::{
    self, FlashRegion, PARTITION_TABLE_MAX_LEN, PartitionType,
};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;

use crate::mk_static;

pub type SharedFlashStorage = Mutex<CriticalSectionRawMutex, FlashStorage<'static>>;

pub async fn init(flash_peri: FLASH<'static>) -> &'static SharedFlashStorage {
    let mut flash_store = FlashStorage::new(flash_peri);
    app_settings::persist::init(&mut flash_store).await;
    boot_count::count_boot(&mut flash_store);
    app_settings::info::init(&mut flash_store);
    mk_static!(SharedFlashStorage, Mutex::new(flash_store))
}

/// Runs `f` on the data partition with the label (see partitions.csv).
/// `Error::Invalid` if the partition table can't be read or has no such
/// partition.
pub fn with_data_partition<'d, R>(
    flash_store: &mut FlashStorage<'d>,
    label: &str,
    f: impl FnOnce(&mut FlashRegion<'_, 'd>) -> Result<R, partitions::Error>,
) -> Result<R, partitions::Error> {
    let mut pt_mem = [0u8; PARTITION_TABLE_MAX_LEN];
    let partition_table = partitions::read_partition_table(flash_store, &mut pt_mem)?;
    let mut storage = partition_table
        .iter()
        .find(|part| {
            matches!(part.partition_type(), PartitionType::Data(_)) && part.label_as_str() == label
        })
        .ok_or(partitions::Error::Invalid)?
        .as_flash_region(flash_store);
    f(&mut storage)
}
