// Device info in its own flash partition: product and firmware of the
// running image, read by tools over USB to detect the device
use defmt::info;
use esp_storage::FlashStorage;
use serde::{Deserialize, Serialize};

use super::{STORE_DEVICE_INFO_SIGNAL, session};
use crate::{config::CONFIG, store::with_data_partition};

const INFO_PARTITION: &str = "info";
const DEVICE_INFO_MAGIC: u32 = 0xEDA1BEE;

#[derive(Clone, Serialize, Deserialize)]
struct DeviceInfo {
    magic: u32,
    product_id: heapless::String<16>,
    firmware: FirmwareInfo,
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

pub fn store() {
    STORE_DEVICE_INFO_SIGNAL.signal(());
}

pub(super) async fn write_to_flash(flash_store: &mut FlashStorage<'_>) {
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
    };
    let serialized = postcard::to_vec::<_, 64>(&info).expect("Failed to serialize device info");
    with_data_partition(flash_store, INFO_PARTITION, |storage| {
        storage
            .write(0, &serialized)
            .expect("Failed to write device info to storage")
    });
    info!("Device info stored to flash");
}
