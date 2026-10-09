// Firmware builds for development and the factory image, flashing, monitoring
// and linting
use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    cli::{BuildArgs, ClippyArgs, FactoryArgs},
    device::{self, ConnectedDevice},
    product::ProductId,
    tools,
};

pub const TARGET: &str = "riscv32imc-unknown-none-elf";
pub const ELF_PATH: &str = "target/riscv32imc-unknown-none-elf/release/fw-ledtransit-map";
pub const BOOTLOADER_PATH: &str = "assets/boot_image/bootloader.bin";
pub const PARTITION_TABLE_PATH: &str = "partitions.csv";

// partitions.csv
const OTADATA_OFFSET: u32 = 0xD000;
const OTADATA_SIZE: u32 = 0x2000;

/// Builds the development firmware, and with `run` also flashes it and
/// monitors its log.
pub fn build(args: BuildArgs, run: bool) -> Result<()> {
    log::info!(
        "Building firmware for product: {:?} (log={:?})",
        args.product_id,
        args.log
    );

    // When running, the connected device decides (and must match a given product)
    let detected = if run { detect_device() } else { None };
    let (product_id, is_factory) = select_product(args.product_id.clone(), detected)?;
    let env = development_env(&args, &product_id);

    // Flashing writes the factory partition: unless that's booted already,
    // reset the boot selection so the bootloader boots it
    if run && !is_factory {
        erase_otadata()?;
    }

    cargo_build(&env)?;
    if !run {
        return Ok(());
    }
    flash(ELF_PATH)?;
    monitor()
}

/// Builds the factory image (from a clean, tagged commit) and installs it.
pub fn install_factory(args: FactoryArgs) -> Result<()> {
    log::info!(
        "Installing factory firmware for product: {:?}",
        args.product_id
    );
    tools::ensure_clean_tagged_commit(&package_version()?)?;
    cargo_build(&release_env(&args.product_id))?;
    flash(ELF_PATH)?;
    tools::probe_rs(&["reset"])
}

pub fn clippy(args: ClippyArgs) -> Result<()> {
    log::info!("Running clippy linter");
    let mut cargo_args = vec!["clippy", "--target", TARGET, "--release"];
    if args.fix {
        cargo_args.push("--fix");
    }
    if args.allow_dirty {
        cargo_args.push("--allow-dirty");
    }
    tools::cargo(&cargo_args, Path::new("."), &[])
}

pub fn monitor() -> Result<()> {
    log::info!("Starting RTT monitor");
    tools::probe_rs(&[
        "attach",
        "--chip",
        "esp32c3",
        "--preverify",
        "--always-print-stacktrace",
        "--no-location",
        "--catch-hardfault",
        &format!("--idf-partition-table={PARTITION_TABLE_PATH}"),
        &format!("--idf-bootloader={BOOTLOADER_PATH}"),
        ELF_PATH,
    ])
}

pub fn cargo_build(env: tools::Env) -> Result<()> {
    tools::cargo(
        &["build", "--target", TARGET, "--release"],
        Path::new("."),
        env,
    )
}

/// Flashes the firmware with the bootloader and partition table.
pub fn flash(elf_path: &str) -> Result<()> {
    tools::espflash(&[
        "flash",
        "--chip=esp32c3",
        &format!("--bootloader={BOOTLOADER_PATH}"),
        &format!("--partition-table={PARTITION_TABLE_PATH}"),
        "--skip-update-check",
        "--baud=4000000",
        elf_path,
    ])
}

/// The firmware's version, from its Cargo.toml (e.g. "1.1.1").
pub fn package_version() -> Result<String> {
    let cargo_toml_path = Path::new("Cargo.toml");
    let contents = fs::read_to_string(cargo_toml_path)
        .with_context(|| format!("Failed to read Cargo.toml at {:?}", cargo_toml_path))?;
    let cargo_toml: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse Cargo.toml at {:?}", cargo_toml_path))?;
    cargo_toml
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(|version| version.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "Failed to extract version from Cargo.toml at {:?}",
                cargo_toml_path
            )
        })
}

/// Environment of a release build (also the factory image).
pub fn release_env(product_id: &ProductId) -> Vec<(&'static str, String)> {
    vec![
        ("PRODUCT", product_id.as_str().to_string()),
        ("DEFMT_LOG", "info".to_string()),
        ("RELEASE", "true".to_string()),
    ]
}

fn development_env(args: &BuildArgs, product_id: &ProductId) -> Vec<(&'static str, String)> {
    let mut env = vec![
        ("PRODUCT", product_id.as_str().to_string()),
        ("DEFMT_LOG", args.log.as_str().to_string()),
    ];
    let overrides = [
        ("WIFI_SSID", args.wifi_ssid.clone()),
        ("WIFI_PASSWORD", args.wifi_password.clone()),
        ("PROV_TOKEN", args.provisioning_token.clone()),
        ("GATEWAY_HOST", args.gateway_host.clone()),
        (
            "GATEWAY_PORT",
            args.gateway_port.map(|port| port.to_string()),
        ),
        ("SSL_ENABLED", args.ssl_enable.map(|ssl| ssl.to_string())),
    ];
    env.extend(
        overrides
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value))),
    );
    env
}

fn detect_device() -> Option<ConnectedDevice> {
    match device::detect() {
        Ok(device) => {
            log::info!(
                "Auto-detected connected device: {:?} (firmware version: {}, factory: {})",
                device.product_id,
                device.firmware_version,
                device.is_factory
            );
            Some(device)
        }
        Err(e) => {
            log::warn!("Failed to auto-detect connected device: {:?}", e);
            None
        }
    }
}

/// The product to build for, and whether the device boots its factory
/// partition: the given product (which must match the connected device), else
/// the connected device's.
fn select_product(
    specified: Option<ProductId>,
    detected: Option<ConnectedDevice>,
) -> Result<(ProductId, bool)> {
    match (specified, detected) {
        (Some(specified), Some(device)) if specified != device.product_id => {
            log::error!(
                "Specified product ID {:?} does not match auto-detected product ID {:?}",
                specified,
                device.product_id
            );
            bail!("Product ID mismatch")
        }
        (Some(specified), _) => Ok((specified, false)),
        (None, detected) => {
            log::info!(
                "No product specified, using auto-detected product: {:?}",
                detected.as_ref().map(|device| &device.product_id)
            );
            let device = detected.ok_or_else(|| anyhow!("No product detected or specified"))?;
            Ok((device.product_id, device.is_factory))
        }
    }
}

fn erase_otadata() -> Result<()> {
    log::info!(
        "Resetting OTA boot selection by erasing otadata (offset=0x{:X}, size=0x{:X})",
        OTADATA_OFFSET,
        OTADATA_SIZE
    );
    tools::espflash(&[
        "erase-region",
        &OTADATA_OFFSET.to_string(),
        &OTADATA_SIZE.to_string(),
    ])
}
