use anyhow::{Context, Result, bail};
use chrono;
use clap::{Args, Parser};
use p256::ecdsa::signature::Verifier;
use p256::pkcs8::DecodePublicKey;
use p256::{ecdsa::signature::Signer, pkcs8::DecodePrivateKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::OsStr,
    path::Path,
    process::{Command, Stdio},
};

#[derive(Debug, Parser)]
enum Cli {
    #[clap(
        name = "doctor",
        about = "Check the development environment is set up correctly"
    )]
    Doctor,
    #[clap(name = "build", about = "Build the development firmware for a product")]
    Build(BuildCommand),
    #[clap(name = "run", about = "Run the development firmware on the device")]
    Run(BuildCommand),
    #[clap(name = "monitor", about = "Monitor the log output of the device")]
    Monitor,
    #[clap(name = "detect", about = "Detect which product is connected")]
    Detect,
    #[clap(name = "clippy", about = "Run clippy linter")]
    Clippy(ClippyCommand),

    #[clap(name = "bootloader", about = "Build the bootloader using ESP-IDF")]
    Bootloader,
    #[clap(name = "prov-server", about = "Build the provisioning server assets")]
    ProvServer,
    #[clap(name = "release", about = "Create a release build for a product")]
    Release(ReleaseCommand),
    #[clap(
        name = "factory",
        about = "Build and install the factory firmware on the device"
    )]
    Factory(FactoryCommand),
}

#[derive(Debug, Args)]
struct BuildCommand {
    #[arg(
        value_enum,
        help = "Hardware product to build_firmware for in format <PROD>-<MAJOR>-<MINOR>"
    )]
    product_id: Option<ProductId>,
    #[arg(long, value_enum, default_value_t = LogLevel::Info, help = "Set the logging level")]
    log: LogLevel,
    #[arg(
        long = "wifi-ssid",
        help = "WiFi SSID override to skip provisioning process"
    )]
    wifi_ssid: Option<String>,
    #[arg(
        long = "wifi-pw",
        help = "WiFi password override to skip provisioning process"
    )]
    wifi_password: Option<String>,
    #[arg(
        long = "prov-token",
        help = "Provisioning token override to use during provisioning"
    )]
    provisioning_token: Option<String>,
    #[arg(long = "api-token", help = "API token override to use for API access")]
    api_token: Option<String>,
    #[arg(
        long = "gw-host",
        help = "Gateway server host override to use for API access [default: built-in]"
    )]
    gateway_host: Option<String>,
    #[arg(
        long = "gw-port",
        help = "Gateway server port override to use for API access [default: built-in]"
    )]
    gateway_port: Option<u16>,
    #[arg(
        long = "ssl-enable",
        help = "Enable SSL override for API access [default: built-in]"
    )]
    ssl_enable: Option<bool>,
}

#[derive(Debug, Args)]
struct FactoryCommand {
    #[arg(
        value_enum,
        help = "Hardware product to build_firmware for in format <PROD>-<MAJOR>-<MINOR>"
    )]
    product_id: ProductId,
}

#[derive(Debug, Args)]
struct ReleaseCommand {
    #[arg(
        value_enum,
        required = true,
        help = "Hardware product to build_firmware for in format <PROD>-<MAJOR>-<MINOR>"
    )]
    product_id: ProductId,
    #[arg(
        long = "install",
        help = "Install the release build to connected device after building"
    )]
    install: bool,
}

#[derive(Debug, Args)]
struct ClippyCommand {
    #[arg(
        long = "fix",
        help = "Run clippy with --fix to automatically fix issues"
    )]
    fix: bool,
    #[arg(
        long = "allow-dirty",
        help = "Allow running clippy even if git repository is dirty"
    )]
    allow_dirty: bool,
}

#[derive(Debug, Clone, clap::ValueEnum, PartialEq, Eq)]
enum ProductId {
    Bln1_2512_1,
    Bln2_2512_1,
}

impl ProductId {
    fn to_str(&self) -> &'static str {
        match self {
            ProductId::Bln1_2512_1 => "bln1-2512-1",
            ProductId::Bln2_2512_1 => "bln2-2512-1",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "bln1-2512-1" => Some(ProductId::Bln1_2512_1),
            "bln2-2512-1" => Some(ProductId::Bln2_2512_1),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Off => "off",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

const TARGET: &str = "riscv32imc-unknown-none-elf";

const OTADATA_OFFSET: u32 = 0xD000; // partitions.csv
const OTADATA_SIZE: u32 = 0x2000; // partitions.csv

const DEVICE_INFO_OFFSET: u32 = 0x3FF000; // partitions.csv
const DEVICE_INFO_MAGIC: u32 = 0xEDA1BEE;

fn main() -> Result<()> {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder.target(env_logger::Target::Stdout);
    builder.init();

    let cli = Cli::parse();

    match cli {
        Cli::Build(cmd) => build_firmware(cmd, false)?,
        Cli::Run(cmd) => build_firmware(cmd, true)?,
        Cli::Monitor => monitor_rtt()?,
        Cli::Release(cmd) => build_release(cmd)?,
        Cli::Bootloader => build_bootloader()?,
        Cli::ProvServer => build_prov_server()?,
        Cli::Doctor => run_doctor()?,
        Cli::Detect => _ = detect_connected_device()?,
        Cli::Factory(cmd) => install_factory_firmware(cmd)?,
        Cli::Clippy(cmd) => run_clippy(cmd)?,
    }

    Ok(())
}

fn run_clippy(cmd: ClippyCommand) -> Result<()> {
    log::info!("Running clippy linter");

    let mut args = vec![
        "clippy".to_string(),
        "--target".to_string(),
        TARGET.to_string(),
        "--release".to_string(),
    ];
    if cmd.fix {
        args.push("--fix".to_string());
    }
    if cmd.allow_dirty {
        args.push("--allow-dirty".to_string());
    }

    let env_vars: HashMap<&str, String> = HashMap::new();
    let cwd = std::env::current_dir()?;
    run_cargo(&args, &cwd, env_vars, true)?;

    Ok(())
}

fn run_doctor() -> Result<()> {
    log::info!("Running environment checks");

    struct ToolCheck {
        name: &'static str,
        is_critical: bool,
        steps: &'static [ToolCheckStep],
    }

    struct ToolCheckStep {
        description: &'static str,
        command: &'static str,
        suggested_fix: &'static str,
        check_output_fn: Option<fn(&str) -> bool>,
    }

    let tools = [
        ToolCheck {
            name: "Rust toolchain",
            is_critical: true,
            steps: &[
                ToolCheckStep {
                    description: "Rust stable toolchain is installed",
                    command: "rustup toolchain list",
                    suggested_fix: "Install Rust stable toolchain using 'rustup toolchain install stable --component rust-src'",
                    check_output_fn: Some(|output| output.contains("stable")),
                },
                ToolCheckStep {
                    description: "Minimum Rust version is 1.88.0",
                    command: "rustc --version",
                    suggested_fix: "Update Rust compiler to at least version 1.88.0 using 'rustup update stable'",
                    check_output_fn: Some(|output| {
                        let min_version = (1, 88, 0);
                        let version_str = output.split_whitespace().nth(1).unwrap_or("").trim();
                        let version_parts: Vec<&str> = version_str.split('.').collect();
                        if version_parts.len() != 3 {
                            return false;
                        }
                        let major = version_parts[0].parse::<u32>().unwrap_or(0);
                        let minor = version_parts[1].parse::<u32>().unwrap_or(0);
                        let patch = version_parts[2].parse::<u32>().unwrap_or(0);
                        (major, minor, patch) >= min_version
                    }),
                },
                ToolCheckStep {
                    description: "RV32IMC target is installed",
                    command: "rustup target list --installed",
                    suggested_fix: "Add the RV32IMC target using 'rustup target add riscv32imc-unknown-none-elf'",
                    check_output_fn: Some(|output| output.contains("riscv32imc-unknown-none-elf")),
                },
            ],
        },
        ToolCheck {
            name: "Protobuf compiler",
            is_critical: true,
            steps: &[ToolCheckStep {
                description: "protoc is installed",
                command: "protoc --version",
                suggested_fix: "Install protobuf compiler (see https://protobuf.dev/installation/ for instructions)",
                check_output_fn: None,
            }],
        },
        ToolCheck {
            name: "probe-rs",
            is_critical: true,
            steps: &[ToolCheckStep {
                description: "probe-rs is installed",
                command: "probe-rs --version",
                suggested_fix: "Install probe-rs (see https://probe.rs/docs/getting-started/installation/ for instructions)",
                check_output_fn: None,
            }],
        },
        ToolCheck {
            name: "ESP flash tool",
            is_critical: true,
            steps: &[ToolCheckStep {
                description: "espflash is installed",
                command: "espflash --version",
                suggested_fix: "Install espflash (see https://github.com/esp-rs/espflash/blob/main/cargo-espflash/README.md for instructions)",
                check_output_fn: None,
            }],
        },
    ];

    let mut critical_failures = 0;
    let mut non_critical_failures = 0;

    // one line per check. only print one line with log::error or log::warn if a check fails but non critical, or log::info
    for tool in &tools {
        log::info!("Checking {}...", tool.name);
        for step in tool.steps {
            let output = Command::new("sh")
                .arg("-c")
                .arg(step.command)
                .output()
                .with_context(|| format!("Failed to execute command '{}'", step.command))?;
            let output_str = String::from_utf8_lossy(&output.stdout);
            if output.status.success() && (step.check_output_fn.map_or(true, |f| f(&output_str))) {
                log::info!("  ✓ {}", step.description);
            } else {
                if tool.is_critical {
                    log::error!("  ✗ {} (required) ..FAILED", step.description);
                    critical_failures += 1;
                } else {
                    log::warn!("  ✗ {} (non-critical) ..FAILED", step.description);
                    non_critical_failures += 1;
                }
                log::info!("    └ {}", step.suggested_fix);
            }
        }
    }

    if critical_failures > 0 {
        log::error!(
            "{} critical checks failed. Please fix these issues before proceeding.",
            critical_failures
        );
    } else if non_critical_failures > 0 {
        log::warn!(
            "{} non-critical checks failed. It's recommended to fix these issues for the best development experience.",
            non_critical_failures
        );
    } else {
        log::info!("All checks passed! Your development environment is set up correctly.");
    }

    Ok(())
}

fn build_firmware(cmd: BuildCommand, run: bool) -> Result<()> {
    log::info!(
        "Building firmware for product: {:?} (log={:?})",
        cmd.product_id,
        cmd.log
    );

    // When running the firmware, also try to auto-detect the connected device
    let (detected_product_id, _detected_firmware_version, detected_is_factory) = if run {
        match detect_connected_device() {
            Ok((pid, fw_ver, is_factory)) => {
                log::info!(
                    "Auto-detected connected device: {:?} (firmware version: {}, factory: {})",
                    pid,
                    fw_ver,
                    is_factory
                );
                (Some(pid), Some(fw_ver), Some(is_factory))
            }
            Err(e) => {
                log::warn!("Failed to auto-detect connected device: {:?}", e);
                (None, None, None)
            }
        }
    } else {
        (None, None, None)
    };

    // If product id is provided and does not match auto-detected product id, error out to prevent flashing the wrong firmware
    let (product_id, is_factory) = if let Some(pid) = cmd.product_id {
        if let Some(dpid) = detected_product_id {
            if pid != dpid {
                log::error!(
                    "Specified product ID {:?} does not match auto-detected product ID {:?}",
                    pid,
                    dpid
                );
                bail!("Product ID mismatch");
            }
        }
        (pid, false)
    } else {
        // If no product id is provided, use auto-detected product id if available
        log::info!(
            "No product specified, using auto-detected product: {:?}",
            detected_product_id
        );
        (
            detected_product_id.expect("No product detected or specified"),
            detected_is_factory.unwrap_or(false),
        )
    };

    let cwd = std::env::current_dir()?;

    let mut args = vec![];
    args.push("build".to_string());
    args.push("--target".to_string());
    args.push(TARGET.to_string());
    args.push("--release".to_string());

    let env_vars: HashMap<&str, String> = {
        let mut m = HashMap::new();
        m.insert("PRODUCT", product_id.to_str().to_string());
        m.insert("DEFMT_LOG", cmd.log.as_str().to_string());
        if let Some(ssid) = cmd.wifi_ssid {
            m.insert("WIFI_SSID", ssid);
        }
        if let Some(pw) = cmd.wifi_password {
            m.insert("WIFI_PASSWORD", pw);
        }
        if let Some(token) = cmd.provisioning_token {
            m.insert("PROVISIONING_TOKEN", token);
        }
        if let Some(api_token) = cmd.api_token {
            m.insert("API_TOKEN", api_token);
        }
        if let Some(host) = cmd.gateway_host {
            m.insert("GATEWAY_HOST", host);
        }
        if let Some(port) = cmd.gateway_port {
            m.insert("GATEWAY_PORT", port.to_string());
        }
        if let Some(ssl_enable) = cmd.ssl_enable {
            m.insert("SSL_ENABLED", ssl_enable.to_string());
        }
        m
    };

    // If current boot partition is not factory (OTA0 or OTA1), erase the OTA data partition to force the device to boot into factory partition
    if run && !is_factory {
        log::info!(
            "Resetting OTA boot selection by erasing otadata (offset=0x{:X}, size=0x{:X})",
            OTADATA_OFFSET,
            OTADATA_SIZE
        );
        let erase_args = vec![
            "erase-region".to_string(),
            OTADATA_OFFSET.to_string(),
            OTADATA_SIZE.to_string(),
        ];
        run_espflash(
            &erase_args,
            &cwd,
            env_vars.iter().map(|(k, v)| (*k, v.as_str())),
        )?;
    }

    // Build the firmware
    run_cargo(
        &args,
        &cwd,
        env_vars.iter().map(|(k, v)| (*k, v.as_str())),
        false,
    )?;

    if !run {
        return Ok(());
    }

    let built_elf_path = cwd
        .join("target")
        .join(TARGET)
        .join("release")
        .join("fw-ledtransit-map");

    // Flash onto target
    let mut args = vec![];
    args.push("flash".to_string());
    args.push("--chip=esp32c3".to_string());
    args.push("--bootloader=assets/boot_image/bootloader.bin".to_string());
    args.push("--partition-table=partitions.csv".to_string());
    args.push("--skip-update-check".to_string());
    args.push("--baud=4000000".to_string());
    args.push(built_elf_path.to_str().unwrap().to_string());

    run_espflash(&args, &cwd, env_vars.iter().map(|(k, v)| (*k, v.as_str())))?;

    // Monitor
    monitor_rtt()?;

    Ok(())
}

fn install_factory_firmware(cmd: FactoryCommand) -> Result<()> {
    log::info!(
        "Installing factory firmware for product: {:?}",
        cmd.product_id
    );

    let cwd = std::env::current_dir()?;

    // Check git is clean and on tagged commit
    let git_status_output = Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .output()
        .with_context(|| "Failed to execute 'git status'")?;
    if !git_status_output.stdout.is_empty() {
        bail!("Git repository is not clean");
    }

    let git_tag_output = Command::new("git")
        .arg("describe")
        .arg("--tags")
        .arg("--exact-match")
        .output()
        .with_context(|| "Failed to execute 'git describe'")?;
    if !git_tag_output.status.success() {
        bail!("Current commit is not tagged");
    }

    // Build the firmware with factory configuration
    let mut args = vec![];
    args.push("build".to_string());
    args.push("--target".to_string());
    args.push(TARGET.to_string());
    args.push("--release".to_string());

    let env_vars: HashMap<&str, String> = {
        let mut m = HashMap::new();
        m.insert("PRODUCT", cmd.product_id.to_str().to_string());
        m.insert("DEFMT_LOG", "info".to_string());
        m.insert("RELEASE", "true".to_string());
        m
    };

    run_cargo(
        &args,
        &cwd,
        env_vars.iter().map(|(k, v)| (*k, v.as_str())),
        false,
    )?;

    // Flash the firmware
    let built_elf_path = cwd
        .join("target")
        .join(TARGET)
        .join("release")
        .join("fw-ledtransit-map");

    let mut args = vec![];
    args.push("flash".to_string());
    args.push("--chip=esp32c3".to_string());
    args.push("--bootloader=assets/boot_image/bootloader.bin".to_string());
    args.push("--partition-table=partitions.csv".to_string());
    args.push("--skip-update-check".to_string());
    args.push("--baud=4000000".to_string());
    args.push(built_elf_path.to_str().unwrap().to_string());

    run_espflash(&args, &cwd, env_vars.iter().map(|(k, v)| (*k, v.as_str())))?;

    // Reset the device
    let reset_args = vec!["reset".to_string()];
    run_probe_rs(
        &reset_args,
        &cwd,
        env_vars.iter().map(|(k, v)| (*k, v.as_str())),
    )?;

    Ok(())
}

fn parse_version_str(version: &str) -> Result<(u32, u32, u32)> {
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() != 3 {
        bail!("Version string must be in format MAJOR.MINOR.PATCH");
    }
    let major = parts[0]
        .parse::<u32>()
        .with_context(|| "Failed to parse major version as integer")?;
    let minor = parts[1]
        .parse::<u32>()
        .with_context(|| "Failed to parse minor version as integer")?;
    let patch = parts[2]
        .parse::<u32>()
        .with_context(|| "Failed to parse patch version as integer")?;
    Ok((major, minor, patch))
}

fn build_release(cmd: ReleaseCommand) -> Result<()> {
    log::info!("Building release for product: {:?}", cmd.product_id);

    let cwd = std::env::current_dir()?;

    // Build the firmware in release mode
    let mut args = vec![];
    args.push("build".to_string());
    args.push("--target".to_string());
    args.push(TARGET.to_string());
    args.push("--release".to_string());

    let rustup_toolchain = std::env::var("RUSTUP_TOOLCHAIN")
        .expect("RUSTUP_TOOLCHAIN environment variable is not set");
    let cargo_home =
        std::env::var("CARGO_HOME").expect("CARGO_HOME environment variable is not set");
    let cargo_crates_io_index = cargo_home.clone() + "/registry/src";
    let cargo_git_checkouts = cargo_home + "/git/checkouts";
    let rustup_home =
        std::env::var("RUSTUP_HOME").expect("RUSTUP_HOME environment variable is not set");
    let rustup_lib = format!(
        "{}/toolchains/{}/lib/rustlib/src/rust/library",
        rustup_home, rustup_toolchain
    );

    let env_vars: HashMap<&str, String> = {
        let mut m = HashMap::new();
        m.insert("PRODUCT", cmd.product_id.to_str().to_string());
        m.insert("DEFMT_LOG", "info".to_string());
        m.insert("RELEASE", "true".to_string());
        m.insert("RUSTFLAGS", format!("-C link-arg=-Tlinkall.x -C link-arg=-Tdefmt.x --remap-path-prefix={}=wd/ --remap-path-prefix={}=io/ --remap-path-prefix={}=co/ --remap-path-prefix={}=rl/ ", cwd.display(), cargo_crates_io_index, cargo_git_checkouts, rustup_lib));
        m
    };

    run_cargo(
        &args,
        &cwd,
        env_vars.iter().map(|(k, v)| (*k, v.as_str())),
        false,
    )?;

    // Parse version from cwd Cargo.toml
    let cargo_toml_path = cwd.join("Cargo.toml");
    let cargo_toml_contents = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("Failed to read Cargo.toml at {:?}", cargo_toml_path))?;
    let cargo_toml: toml::Value = toml::from_str(&cargo_toml_contents)
        .with_context(|| format!("Failed to parse Cargo.toml at {:?}", cargo_toml_path))?;
    let version = cargo_toml
        .get("package")
        .and_then(|pkg| pkg.get("version"))
        .and_then(|ver| ver.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to extract version from Cargo.toml at {:?}",
                cargo_toml_path
            )
        })?;
    let (version_major, version_minor, version_patch) = parse_version_str(version)?;

    // Create output folder structure and clear ota directory
    let ota_dir = cwd
        .join("target")
        .join("ota")
        .join(cmd.product_id.to_str())
        .join(version);
    let ota_img_path = ota_dir.join("image.bin");
    let ota_elf_path = ota_dir.join("image.elf");
    let bootloader_bin_path = ota_dir.join("bootloader.bin");

    std::fs::create_dir_all(&ota_dir)
        .with_context(|| format!("Failed to create ota binary directory at {:?}", ota_dir))?;

    // Copy elf to ota directory
    let built_elf_path = cwd
        .join("target")
        .join(TARGET)
        .join("release")
        .join("fw-ledtransit-map");
    std::fs::copy(&built_elf_path, &ota_elf_path).with_context(|| {
        format!(
            "Failed to copy built elf from {:?} to {:?}",
            built_elf_path, ota_elf_path
        )
    })?;

    // Copy bootloader binary to ota directory
    let built_bootloader_path = cwd.join("assets").join("boot_image").join("bootloader.bin");
    std::fs::copy(&built_bootloader_path, &bootloader_bin_path).with_context(|| {
        format!(
            "Failed to copy built bootloader from {:?} to {:?}",
            built_bootloader_path, bootloader_bin_path
        )
    })?;

    // Create the OTA image using espflash
    let mut args = vec![];
    args.push("save-image".to_string());
    args.push("--chip=esp32c3".to_string());
    args.push("--partition-table=partitions.csv".to_string());
    args.push("--bootloader=assets/boot_image/bootloader.bin".to_string());
    args.push(ota_elf_path.to_str().unwrap().to_string());
    args.push(ota_img_path.to_str().unwrap().to_string());

    run_espflash(&args, &cwd, env_vars.iter().map(|(k, v)| (*k, v.as_str())))?;

    // Confirm user home path is not in any of the binaries
    for path in [&ota_elf_path, &ota_img_path, &bootloader_bin_path] {
        let data =
            std::fs::read(path).with_context(|| format!("Failed to read file at {:?}", path))?;
        let user_home = std::env::var("HOME").unwrap_or_default();
        if data
            .windows(user_home.len())
            .any(|w| w == user_home.as_bytes())
        {
            bail!(
                "User home path found in file {:?}, aborting release build",
                path
            );
        }
    }

    // Calculate SHA256 of the OTA image
    let ota_image_data = std::fs::read(&ota_img_path)
        .with_context(|| format!("Failed to read OTA image at {:?}", ota_img_path))?;
    let ota_image_size = ota_image_data.len();
    let sha256_str = sha256::digest(&ota_image_data);
    let sha256_bytes = hex::decode(sha256_str.clone())
        .with_context(|| "Failed to decode SHA256 hash string into bytes")?;

    // Calculate p256 signature of the OTA image using the private key
    let signing_key_path = cwd
        .join("assets")
        .join("secure_ota")
        .join("p256_ota_private_key.p8");
    let signing_key_data = std::fs::read(&signing_key_path)
        .with_context(|| format!("Failed to read OTA signing key at {:?}", signing_key_path))?;
    let secret_key = p256::ecdsa::SigningKey::from_pkcs8_der(&signing_key_data)
        .with_context(|| format!("Failed to parse OTA signing key at {:?}", signing_key_path))?;
    let signing_key = p256::ecdsa::SigningKey::from(secret_key);

    // Message format: concat([u32le:MAJOR, u32le:MINOR, u32le:PATCH, u32le:SIZE, [u8:32]:SHA256, str:PRODUCT_ID])
    let sign_message = [
        &version_major.to_le_bytes(),
        &version_minor.to_le_bytes(),
        &version_patch.to_le_bytes(),
        &(ota_image_size as u32).to_le_bytes(),
        sha256_bytes.as_slice(),
        cmd.product_id.to_str().as_bytes(),
    ]
    .concat();
    let signature: p256::ecdsa::Signature = signing_key
        .try_sign(&sign_message)
        .map_err(|_| anyhow::anyhow!("Failed to sign OTA image with provided signing key"))?;

    // Verify the signature using public key
    let public_key_path = cwd
        .join("assets")
        .join("secure_ota")
        .join("p256_ota_public_key.der");
    let public_key_data = std::fs::read(&public_key_path)
        .with_context(|| format!("Failed to read OTA public key at {:?}", public_key_path))?;
    let verifying_key = p256::ecdsa::VerifyingKey::from_public_key_der(&public_key_data)
        .with_context(|| format!("Failed to parse OTA public key at {:?}", public_key_path))?;
    verifying_key
        .verify(&sign_message, &signature)
        .map_err(|_| {
            anyhow::anyhow!("Failed to verify OTA image signature with provided public key")
        })?;

    #[derive(Serialize, Deserialize, Debug)]
    struct OtaMetadata {
        #[serde(rename = "productId")]
        product_id: String,
        #[serde(rename = "version")]
        version: String,
        #[serde(rename = "builtAt")]
        built_at: String,
        #[serde(rename = "sizeBytes")]
        size_bytes: usize,
        #[serde(rename = "sha256Hash")]
        sha256_hash: String,
        #[serde(rename = "p256Signature")]
        p256_signature: String,
    }

    // Create a metadata file alongside the OTA image using serde json
    let metadata = OtaMetadata {
        product_id: cmd.product_id.to_str().to_string(),
        version: version.to_string(),
        built_at: chrono::Utc::now().to_rfc3339(),
        size_bytes: ota_image_size,
        sha256_hash: format!("{}", sha256_str),
        p256_signature: format!("{}", signature),
    };
    let metadata_filepath = ota_dir.join("metadata.json");
    std::fs::write(&metadata_filepath, serde_json::to_string_pretty(&metadata)?)
        .with_context(|| format!("Failed to write metadata file at {:?}", metadata_filepath))?;

    log::info!("OTA image saved to {:?}", ota_img_path);

    // Optional install
    if cmd.install {
        log::info!("Installing release build to connected device...");

        let mut args = vec![];
        args.push("flash".to_string());
        args.push("--chip=esp32c3".to_string());
        args.push("--bootloader=assets/boot_image/bootloader.bin".to_string());
        args.push("--partition-table=partitions.csv".to_string());
        args.push("--skip-update-check".to_string());
        args.push("--baud=4000000".to_string());
        args.push(ota_elf_path.to_str().unwrap().to_string());

        run_espflash(&args, &cwd, env_vars.iter().map(|(k, v)| (*k, v.as_str())))?;

        // Monitor
        monitor_rtt()?;
    }

    Ok(())
}

fn monitor_rtt() -> Result<()> {
    log::info!("Starting RTT monitor");

    let cwd = std::env::current_dir()?;

    let mut args = vec![];
    args.push("attach".to_string());
    args.push("--chip".to_string());
    args.push("esp32c3".to_string());
    args.push("--preverify".to_string());
    args.push("--always-print-stacktrace".to_string());
    args.push("--no-location".to_string());
    args.push("--catch-hardfault".to_string());
    args.push("--idf-partition-table=partitions.csv".to_string());
    args.push("--idf-bootloader=assets/boot_image/bootloader.bin".to_string());
    args.push("target/riscv32imc-unknown-none-elf/release/fw-ledtransit-map".to_string());

    run_probe_rs(&args, &cwd, std::env::vars())?;
    Ok(())
}

fn detect_connected_device() -> Result<(ProductId, String, bool)> {
    let cwd = std::env::current_dir()?;
    let tmp_dir = cwd.join("target").join("tmp");
    std::fs::create_dir_all(&tmp_dir)
        .with_context(|| format!("Failed to create temporary directory at {:?}", tmp_dir))?;

    // Auto-detect the connected product by reading product info from flash
    let mut args = vec![];
    args.push("read-flash".to_string());
    args.push("--chip".to_string());
    args.push("esp32c3".to_string());
    args.push("--skip-update-check".to_string());
    args.push(format!("0x{:X}", DEVICE_INFO_OFFSET));
    args.push("64".to_string());
    args.push(format!(
        "{}",
        tmp_dir.join("product_info.bin").to_str().unwrap()
    ));

    run_espflash(&args, &cwd, std::env::vars())?;

    let product_info_data = std::fs::read(tmp_dir.join("product_info.bin"))
        .with_context(|| "Failed to read product info from flash")?;

    // Deserialize using postcard
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

    let device_info: DeviceInfo = postcard::from_bytes(&product_info_data)
        .with_context(|| "Failed to deserialize product info from flash")?;
    if device_info.magic != DEVICE_INFO_MAGIC {
        bail!("Invalid product info magic value");
    }
    let product_id = ProductId::from_str(device_info.product_id.as_str()).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown product ID '{}' read from flash",
            device_info.product_id
        )
    })?;
    let fw_version = format!(
        "{}.{}.{}",
        device_info.firmware.version_major,
        device_info.firmware.version_minor,
        device_info.firmware.version_patch
    );
    log::info!(
        "Connected device: {:?} (FW v{}) [{}]",
        product_id,
        fw_version,
        if device_info.firmware.is_factory {
            "factory"
        } else {
            "ota"
        }
    );
    Ok((product_id, fw_version, device_info.firmware.is_factory))
}

fn build_bootloader() -> Result<()> {
    log::info!("Building bootloader");

    let idf_path = std::env::var("IDF_PATH").context("IDF_PATH environment variable is not set")?;

    let cwd = std::env::current_dir()?;
    let bootloader_path = cwd.join("bootloader");

    // Sync partitions.csv file
    let partitions_src = cwd.join("partitions.csv");
    let partitions_dst = bootloader_path.join("partitions.csv");
    std::fs::copy(&partitions_src, &partitions_dst).with_context(|| {
        format!(
            "Failed to copy partitions.csv from {:?} to {:?}",
            partitions_src, partitions_dst
        )
    })?;

    // Load the ESP-IDF environment using IDF export script and build using idf.py
    let mut command = Command::new("sh");
    command.arg("-c");
    let bash_command = format!(
        "source \"{}/export.sh\" && idf.py build bootloader",
        idf_path
    );
    command.arg(bash_command);
    command.current_dir(&bootloader_path);
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    command.stdin(Stdio::inherit());

    let status = command
        .status()
        .context("Failed to execute idf.py build bootloader")?;

    if !status.success() {
        bail!("idf.py build bootloader failed with status: {}", status);
    }

    // Copy bootloader binary to assets directory
    let bootloader_bin_src = bootloader_path.join("build/bootloader/bootloader.bin");
    let bootloader_bin_dst_dir = cwd.join("assets/boot_image");
    std::fs::create_dir_all(&bootloader_bin_dst_dir).with_context(|| {
        format!(
            "Failed to create bootloader assets directory at {:?}",
            bootloader_bin_dst_dir
        )
    })?;
    let bootloader_bin_dst = bootloader_bin_dst_dir.join("bootloader.bin");
    std::fs::copy(&bootloader_bin_src, &bootloader_bin_dst).with_context(|| {
        format!(
            "Failed to copy bootloader binary from {:?} to {:?}",
            bootloader_bin_src, bootloader_bin_dst
        )
    })?;

    Ok(())
}

fn build_prov_server() -> Result<()> {
    log::info!("Building provisioning server");

    let cwd = std::env::current_dir()?;
    let server_path = cwd.join("prov_server");

    let args = vec!["run".to_string()];

    run_cargo(&args, &server_path, std::env::vars(), false)?;

    Ok(())
}

fn run_cargo<I, K, V>(args: &[String], cwd: &Path, envs: I, capture: bool) -> Result<String>
where
    I: IntoIterator<Item = (K, V)> + core::fmt::Debug,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    if !cwd.is_dir() {
        bail!("The specified cwd {:?} is not a directory", cwd);
    }

    log::debug!(
        "Running `cargo {}` in {:?} - Environment {:?}",
        args.join(" "),
        cwd,
        envs
    );

    let mut command = Command::new("cargo");

    command
        .args(args)
        .current_dir(cwd)
        .envs(envs)
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });

    if args.iter().any(|a| a.starts_with('+')) {
        command.env_remove("CARGO");
    }

    let output = command
        .stdin(Stdio::inherit())
        .output()
        .with_context(|| format!("Couldn't get output for command {command:?}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        bail!(
            "Failed to execute cargo subcommand `cargo {}`",
            args.join(" "),
        )
    }
}

fn run_espflash<I, K, V>(args: &[String], cwd: &Path, envs: I) -> Result<()>
where
    I: IntoIterator<Item = (K, V)> + core::fmt::Debug,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    if !cwd.is_dir() {
        bail!("The specified cwd {:?} is not a directory", cwd);
    }

    log::debug!(
        "Running `espflash {}` in {:?} - Environment {:?}",
        args.join(" "),
        cwd,
        envs
    );

    let mut command = Command::new("espflash");

    command
        .args(args)
        .current_dir(cwd)
        .envs(envs)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command
        .stdin(Stdio::inherit())
        .status()
        .with_context(|| format!("Couldn't get status for command {command:?}"))?;

    if status.success() {
        Ok(())
    } else {
        bail!(
            "Failed to execute espflash subcommand `espflash {}`",
            args.join(" "),
        )
    }
}

fn run_probe_rs<I, K, V>(args: &[String], cwd: &Path, envs: I) -> Result<()>
where
    I: IntoIterator<Item = (K, V)> + core::fmt::Debug,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    if !cwd.is_dir() {
        bail!("The specified cwd {:?} is not a directory", cwd);
    }

    log::debug!(
        "Running `probe-rs {}` in {:?} - Environment {:?}",
        args.join(" "),
        cwd,
        envs
    );

    let mut command = Command::new("probe-rs");

    command
        .args(args)
        .current_dir(cwd)
        .envs(envs)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command
        .stdin(Stdio::inherit())
        .status()
        .with_context(|| format!("Couldn't get status for command {command:?}"))?;

    if status.success() {
        Ok(())
    } else {
        bail!(
            "Failed to execute probe-rs subcommand `probe-rs {}`",
            args.join(" "),
        )
    }
}
