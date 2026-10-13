// Detection of the connected device from the device info it stores in flash
use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::{product::ProductId, tools};

// partitions.csv "info" partition, see the firmware's store/app_settings/info.rs
const DEVICE_INFO_OFFSET: u32 = 0x3FF000;
const DEVICE_INFO_MAGIC: u32 = 0xEDA1BEE;
const DEVICE_INFO_READ_SIZE: &str = "64";
const DEVICE_INFO_FILE: &str = "target/tmp/product_info.bin";

// Must match the firmware's (postcard encoded)
#[derive(Deserialize)]
struct DeviceInfo {
    magic: u32,
    product_id: heapless::String<16>,
    firmware: FirmwareInfo,
}

#[derive(Deserialize)]
struct FirmwareInfo {
    version_major: u32,
    version_minor: u32,
    version_patch: u32,
    #[allow(dead_code)]
    is_beta: bool,
    is_factory: bool,
    #[allow(dead_code)]
    is_rolled_back: bool,
}

pub struct ConnectedDevice {
    pub product_id: ProductId,
    pub firmware_version: String,
    /// Boots its factory partition
    pub is_factory: bool,
}

pub fn detect() -> Result<ConnectedDevice> {
    let info_path = Path::new(DEVICE_INFO_FILE);
    let tmp_dir = info_path.parent().unwrap();
    fs::create_dir_all(tmp_dir)
        .with_context(|| format!("Failed to create temporary directory at {:?}", tmp_dir))?;

    tools::espflash(&[
        "read-flash",
        "--chip",
        "esp32c3",
        "--skip-update-check",
        &format!("0x{:X}", DEVICE_INFO_OFFSET),
        DEVICE_INFO_READ_SIZE,
        DEVICE_INFO_FILE,
    ])?;
    let data = fs::read(info_path).context("Failed to read product info from flash")?;
    let info: DeviceInfo =
        postcard::from_bytes(&data).context("Failed to deserialize product info from flash")?;
    if info.magic != DEVICE_INFO_MAGIC {
        bail!("Invalid product info magic value");
    }

    let product_id = ProductId::from_name(&info.product_id)
        .ok_or_else(|| anyhow!("Unknown product ID '{}' read from flash", info.product_id))?;
    let firmware_version = format!(
        "{}.{}.{}",
        info.firmware.version_major, info.firmware.version_minor, info.firmware.version_patch
    );
    log::info!(
        "Connected device: {:?} (FW v{}) [{}]",
        product_id,
        firmware_version,
        if info.firmware.is_factory {
            "factory"
        } else {
            "ota"
        }
    );
    Ok(ConnectedDevice {
        product_id,
        firmware_version,
        is_factory: info.firmware.is_factory,
    })
}
