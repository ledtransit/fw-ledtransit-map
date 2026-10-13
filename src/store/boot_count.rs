// Boots ever, counted in flash: in the otherwise unused nvs partition (its
// own sectors, so a reset while counting can't take the settings with it).
// Each boot appends its count to the next free slot, the partition is only
// erased once full: a write per boot would wear out a sector within days of a
// boot loop, this way it takes thousands of times as long. Survives factory
// resets and firmware updates
use defmt::{info, warn};
use embedded_storage::nor_flash::NorFlash;
use esp_bootloader_esp_idf::partitions::{Error, FlashRegion};
use esp_storage::FlashStorage;

use crate::{stats, store::with_data_partition};

const BOOT_COUNT_PARTITION: &str = "nvs";
// A count and its complement: a slot only partly written (reset while
// writing) or never written (erased, all ones) doesn't pass for one
const SLOT_SIZE: usize = 8;
const READ_CHUNK_SIZE: usize = 256;

/// Counts this boot.
pub fn count_boot(flash_store: &mut FlashStorage<'_>) {
    let count = with_data_partition(flash_store, BOOT_COUNT_PARTITION, |storage| {
        let size = storage.capacity();
        let (last_count, free_slot) = scan(storage, size)?;
        let count = last_count.wrapping_add(1);
        let offset = match free_slot {
            Some(offset) => offset,
            None => {
                storage.erase(0, size as u32)?;
                0
            }
        };
        let mut slot = [0u8; SLOT_SIZE];
        slot[..4].copy_from_slice(&count.to_le_bytes());
        slot[4..].copy_from_slice(&(!count).to_le_bytes());
        // Only programmed: a plain write would erase the sector first
        storage.as_nor_flash()?.write(offset as u32, &slot)?;
        Ok::<_, Error>(count)
    });
    match count {
        Ok(count) => {
            info!("Boot {} ever", count);
            stats::set_boot_count(count);
        }
        Err(e) => warn!("Failed to count the boot: {:?}", e),
    }
}

// The highest count stored (0 without one), and the first free slot after
// the written ones (none if full)
fn scan(storage: &mut FlashRegion<'_, '_>, size: usize) -> Result<(u32, Option<usize>), Error> {
    let mut last_count = 0;
    let mut free_slot = None;
    let mut chunk = [0u8; READ_CHUNK_SIZE];
    for chunk_offset in (0..size).step_by(READ_CHUNK_SIZE) {
        storage.read(chunk_offset as u32, &mut chunk)?;
        for (i, slot) in chunk.chunks_exact(SLOT_SIZE).enumerate() {
            let count = u32::from_le_bytes(slot[..4].try_into().unwrap());
            let complement = u32::from_le_bytes(slot[4..].try_into().unwrap());
            if slot.iter().all(|&byte| byte == 0xFF) {
                free_slot.get_or_insert(chunk_offset + i * SLOT_SIZE);
            } else if complement == !count {
                last_count = last_count.max(count);
                // Only free after the written ones
                free_slot = None;
            }
        }
    }
    Ok((last_count, free_slot))
}
