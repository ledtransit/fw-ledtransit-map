// Device info in its own flash partition: product and firmware of the
// running image, read by tools over USB to detect the device. Also how the
// image in each app slot got there (installed at the factory, over USB, or
// over the air), kept across factory resets
use core::cell::RefCell;

use critical_section::Mutex;
use defmt::{info, warn};
use esp_bootloader_esp_idf::{
    ota_updater::OtaUpdater,
    partitions::{self, AppPartitionSubType, FlashRegion, PARTITION_TABLE_MAX_LEN, PartitionType},
};
use esp_storage::FlashStorage;
use serde::{Deserialize, Serialize};

use super::{STORE_DEVICE_INFO_SIGNAL, session};
use crate::{config::CONFIG, store::with_data_partition};

const INFO_PARTITION: &str = "info";
const DEVICE_INFO_MAGIC: u32 = 0xEDA1BEE;
const DEVICE_INFO_MAX_SIZE: usize = 256;

// The app slots (see partitions.csv)
const SLOTS: [AppPartitionSubType; 3] = [
    AppPartitionSubType::Factory,
    AppPartitionSubType::Ota0,
    AppPartitionSubType::Ota1,
];
const FACTORY_SLOT: usize = 0;

// App image format (ESP-IDF esp_image_format.h)
const IMAGE_MAGIC: u8 = 0xE9;
const IMAGE_HEADER_LEN: u32 = 24;
const IMAGE_MAX_SEGMENTS: u8 = 16;
const IMAGE_HASH_APPENDED_BYTE: usize = 23;
const SEGMENT_HEADER_LEN: u32 = 8;

pub type BuildId = [u8; 32];

#[derive(Clone, Serialize, Deserialize)]
struct DeviceInfo {
    magic: u32,
    product_id: heapless::String<16>,
    firmware: FirmwareInfo,
    // After the fields the tools read (only the start)
    installs: [SlotInstall; 3],
}

#[derive(Clone, Serialize, Deserialize)]
struct FirmwareInfo {
    version_major: u32,
    version_minor: u32,
    version_patch: u32,
    is_beta: bool,
    is_factory: bool,
    is_rolled_back: bool,
}

/// How the image in an app slot got there.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, defmt::Format)]
pub enum InstallMethod {
    /// Found there when the records started (or flashed over a blank info
    /// partition), until the production self-test passes
    Unknown,
    /// Flashed at production: passed its self-test (see factory_serial)
    Factory,
    /// Flashed over USB (e.g. with espflash) after production
    Usb,
    /// Downloaded by a firmware update
    Ota,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
struct SlotInstall {
    // The image's SHA-256, zeros without an image
    build_id: BuildId,
    method: InstallMethod,
}

struct Installs {
    slots: [SlotInstall; 3],
    running_slot: usize,
}

// Set at boot (see init)
static INSTALLS: Mutex<RefCell<Option<Installs>>> = Mutex::new(RefCell::new(None));

/// Records how the running image got into its slot: a slot with a different
/// image than recorded was installed since. Only the factory slot is flashed
/// over USB, only firmware updates write the OTA slots.
pub fn init(flash_store: &mut FlashStorage<'_>) {
    let running_slot = running_slot(flash_store);
    let build_ids = SLOTS.map(|slot| image_build_id(flash_store, slot).unwrap_or_default());
    let slots = match read_installs(flash_store) {
        Some(mut slots) => {
            let running = &mut slots[running_slot];
            if running.build_id != build_ids[running_slot] {
                *running = SlotInstall {
                    build_id: build_ids[running_slot],
                    method: if running_slot == FACTORY_SLOT {
                        InstallMethod::Usb
                    } else {
                        InstallMethod::Ota
                    },
                };
            }
            slots
        }
        // No records yet: what the slots hold, but only the OTA slots tell
        // how it got there
        None => core::array::from_fn(|slot| SlotInstall {
            build_id: build_ids[slot],
            method: if slot == FACTORY_SLOT || build_ids[slot] == BuildId::default() {
                InstallMethod::Unknown
            } else {
                InstallMethod::Ota
            },
        }),
    };
    info!(
        "Running {:?}, installed by {:?}",
        SLOTS[running_slot], slots[running_slot].method
    );
    critical_section::with(|cs| {
        *INSTALLS.borrow_ref_mut(cs) = Some(Installs {
            slots,
            running_slot,
        });
    });
}

/// The running image's build ID (its SHA-256), and how it was installed.
pub fn running_install() -> (BuildId, InstallMethod) {
    critical_section::with(|cs| {
        INSTALLS.borrow_ref(cs).as_ref().map_or(
            (BuildId::default(), InstallMethod::Unknown),
            |installs| {
                let install = installs.slots[installs.running_slot];
                (install.build_id, install.method)
            },
        )
    })
}

/// The production self-test passed: the factory image was flashed there.
pub fn on_factory_self_test() {
    let changed = critical_section::with(|cs| {
        let mut installs = INSTALLS.borrow_ref_mut(cs);
        let Some(installs) = installs.as_mut() else {
            return false;
        };
        let install = &mut installs.slots[installs.running_slot];
        let is_factory_flashed =
            installs.running_slot == FACTORY_SLOT && install.method != InstallMethod::Factory;
        if is_factory_flashed {
            install.method = InstallMethod::Factory;
        }
        is_factory_flashed
    });
    if changed {
        store();
    }
}

pub fn store() {
    STORE_DEVICE_INFO_SIGNAL.signal(());
}

/// Writes the device info, if it changed: each write erases the sector.
pub(super) async fn write_to_flash(flash_store: &mut FlashStorage<'_>) {
    let Some(installs) = critical_section::with(|cs| {
        INSTALLS
            .borrow_ref(cs)
            .as_ref()
            .map(|installs| installs.slots)
    }) else {
        return;
    };
    let session_settings = session::get_settings().await;
    let info = DeviceInfo {
        magic: DEVICE_INFO_MAGIC,
        product_id: heapless::String::try_from(CONFIG.product.as_str()).unwrap(),
        firmware: FirmwareInfo {
            version_major: CONFIG.fw_version.major,
            version_minor: CONFIG.fw_version.minor,
            version_patch: CONFIG.fw_version.patch,
            is_beta: CONFIG.fw_version.beta,
            is_factory: session_settings.is_factory_firmware,
            is_rolled_back: session_settings.is_rolled_back_firmware,
        },
        installs,
    };
    let serialized = postcard::to_vec::<_, DEVICE_INFO_MAX_SIZE>(&info)
        .expect("Failed to serialize device info");
    let written = with_data_partition(flash_store, INFO_PARTITION, |storage| {
        let mut stored = [0u8; DEVICE_INFO_MAX_SIZE];
        let stored = &mut stored[..serialized.len()];
        if storage.read(0, stored).is_ok() && *stored == *serialized {
            return false;
        }
        storage
            .write(0, &serialized)
            .expect("Failed to write device info to storage");
        true
    });
    if written {
        info!("Device info stored to flash");
    }
}

// The stored install records, if there are any (none in a blank partition,
// or one written by firmware from before they were kept)
fn read_installs(flash_store: &mut FlashStorage<'_>) -> Option<[SlotInstall; 3]> {
    let mut buf = [0u8; DEVICE_INFO_MAX_SIZE];
    with_data_partition(flash_store, INFO_PARTITION, |storage| {
        storage.read(0, &mut buf)
    })
    .ok()?;
    postcard::from_bytes::<DeviceInfo>(&buf)
        .ok()
        .filter(|info| info.magic == DEVICE_INFO_MAGIC)
        .map(|info| info.installs)
}

// The slot the bootloader selected (see ota::boot)
fn running_slot(flash_store: &mut FlashStorage<'_>) -> usize {
    let mut pt_mem = [0u8; PARTITION_TABLE_MAX_LEN];
    let selected = OtaUpdater::new(flash_store, &mut pt_mem)
        .and_then(|mut ota| ota.selected_partition())
        .unwrap_or(AppPartitionSubType::Factory);
    SLOTS
        .iter()
        .position(|&slot| slot == selected)
        .unwrap_or(FACTORY_SLOT)
}

// The SHA-256 appended to the slot's image (espflash appends it, the
// bootloader checks it at boot). None without an image or hash
fn image_build_id(
    flash_store: &mut FlashStorage<'_>,
    slot: AppPartitionSubType,
) -> Option<BuildId> {
    let mut pt_mem = [0u8; PARTITION_TABLE_MAX_LEN];
    let partition = partitions::read_partition_table(flash_store, &mut pt_mem)
        .ok()?
        .find_partition(PartitionType::App(slot))
        .ok()??;
    let mut image = partition.as_flash_region(flash_store);
    match read_appended_hash(&mut image) {
        Ok(hash) => hash,
        Err(e) => {
            warn!("Failed to read the image in {:?}: {:?}", slot, e);
            None
        }
    }
}

// Walks the image's segments to its end, as ESP-IDF's esp_image_get_metadata
fn read_appended_hash(
    image: &mut FlashRegion<'_, '_>,
) -> Result<Option<BuildId>, partitions::Error> {
    let mut header = [0u8; IMAGE_HEADER_LEN as usize];
    image.read(0, &mut header)?;
    if header[0] != IMAGE_MAGIC || header[1] > IMAGE_MAX_SEGMENTS {
        return Ok(None);
    }
    let mut len = IMAGE_HEADER_LEN;
    for _ in 0..header[1] {
        let mut segment_header = [0u8; SEGMENT_HEADER_LEN as usize];
        image.read(len, &mut segment_header)?;
        let data_len = u32::from_le_bytes(segment_header[4..].try_into().unwrap());
        len = len
            .checked_add(SEGMENT_HEADER_LEN + data_len)
            .ok_or(partitions::Error::InvalidImage)?;
    }
    // The checksum byte, padded to a full 16 byte block
    len = (len + 1).next_multiple_of(16);
    if header[IMAGE_HASH_APPENDED_BYTE] == 0 {
        return Ok(None);
    }
    let mut hash = BuildId::default();
    image.read(len, &mut hash)?;
    Ok(Some(hash))
}
