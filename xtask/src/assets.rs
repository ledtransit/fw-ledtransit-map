// Builds of the assets embedded in or flashed with the firmware: the
// bootloader (ESP-IDF) and the setup portal's files
use std::{env, fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::{
    firmware::{BOOTLOADER_PATH, PARTITION_TABLE_PATH},
    tools::{self, copy},
};

const BOOTLOADER_PROJECT_DIR: &str = "bootloader";
const PROV_SERVER_DIR: &str = "prov_server";

/// Builds the bootloader with ESP-IDF (IDF_PATH) into assets/boot_image.
pub fn build_bootloader() -> Result<()> {
    log::info!("Building bootloader");
    let idf_path = env::var("IDF_PATH").context("IDF_PATH environment variable is not set")?;
    let project_dir = Path::new(BOOTLOADER_PROJECT_DIR);

    // The bootloader needs the same partition table
    copy(
        Path::new(PARTITION_TABLE_PATH),
        &project_dir.join(PARTITION_TABLE_PATH),
    )?;

    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "source \"{idf_path}/export.sh\" && idf.py build bootloader"
        ))
        .current_dir(project_dir)
        .status()
        .context("Failed to execute idf.py build bootloader")?;
    if !status.success() {
        bail!("idf.py build bootloader failed with status: {}", status);
    }

    let bootloader_dst = Path::new(BOOTLOADER_PATH);
    let bootloader_dst_dir = bootloader_dst.parent().unwrap();
    fs::create_dir_all(bootloader_dst_dir).with_context(|| {
        format!(
            "Failed to create bootloader assets directory at {:?}",
            bootloader_dst_dir
        )
    })?;
    copy(
        &project_dir.join("build/bootloader/bootloader.bin"),
        bootloader_dst,
    )
}

/// Builds the setup portal's files into assets/prov_public.
pub fn build_prov_server() -> Result<()> {
    log::info!("Building provisioning server");
    tools::cargo(&["run"], Path::new(PROV_SERVER_DIR), &[])
}
