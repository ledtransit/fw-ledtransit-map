use clap::{Args, Parser, ValueEnum};

use crate::product::ProductId;

#[derive(Debug, Parser)]
pub enum Cli {
    #[clap(
        name = "doctor",
        about = "Check the development environment is set up correctly"
    )]
    Doctor,
    #[clap(name = "build", about = "Build the development firmware for a product")]
    Build(BuildArgs),
    #[clap(name = "run", about = "Run the development firmware on the device")]
    Run(BuildArgs),
    #[clap(name = "monitor", about = "Monitor the log output of the device")]
    Monitor,
    #[clap(name = "detect", about = "Detect which product is connected")]
    Detect,
    #[clap(name = "clippy", about = "Run clippy linter")]
    Clippy(ClippyArgs),

    #[clap(name = "bootloader", about = "Build the bootloader using ESP-IDF")]
    Bootloader,
    #[clap(name = "prov-server", about = "Build the provisioning server assets")]
    ProvServer,
    #[clap(name = "release", about = "Create a release build for a product")]
    Release(ReleaseArgs),
    #[clap(
        name = "factory",
        about = "Build and install the factory firmware on the device"
    )]
    Factory(FactoryArgs),
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    #[arg(
        value_enum,
        help = "Hardware product to build for in format <PROD>-<MAJOR>-<MINOR> [default: connected device]"
    )]
    pub product_id: Option<ProductId>,
    #[arg(long, value_enum, default_value_t = LogLevel::Info, help = "Set the logging level")]
    pub log: LogLevel,
    #[arg(
        long = "wifi-ssid",
        help = "WiFi SSID override to skip provisioning process"
    )]
    pub wifi_ssid: Option<String>,
    #[arg(
        long = "wifi-pw",
        help = "WiFi password override to skip provisioning process"
    )]
    pub wifi_password: Option<String>,
    #[arg(
        long = "prov-token",
        help = "Provisioning token override to use during provisioning"
    )]
    pub provisioning_token: Option<String>,
    #[arg(
        long = "gw-host",
        help = "Gateway server host override to use for API access [default: built-in]"
    )]
    pub gateway_host: Option<String>,
    #[arg(
        long = "gw-port",
        help = "Gateway server port override to use for API access [default: built-in]"
    )]
    pub gateway_port: Option<u16>,
    #[arg(
        long = "ssl-enable",
        help = "Enable SSL override for API access [default: built-in]"
    )]
    pub ssl_enable: Option<bool>,
}

#[derive(Debug, Args)]
pub struct FactoryArgs {
    #[arg(
        value_enum,
        help = "Hardware product to build for in format <PROD>-<MAJOR>-<MINOR>"
    )]
    pub product_id: ProductId,
}

#[derive(Debug, Args)]
pub struct ReleaseArgs {
    #[arg(
        value_enum,
        required = true,
        help = "Hardware product to build for in format <PROD>-<MAJOR>-<MINOR>"
    )]
    pub product_id: ProductId,
    #[arg(
        long = "install",
        help = "Install the release build to connected device after building"
    )]
    pub install: bool,
    #[arg(
        long = "staging",
        help = "Allow a dirty or untagged commit (with a warning), to test a release on staging before tagging it"
    )]
    pub staging: bool,
}

#[derive(Debug, Args)]
pub struct ClippyArgs {
    #[arg(
        long = "fix",
        help = "Run clippy with --fix to automatically fix issues"
    )]
    pub fix: bool,
    #[arg(
        long = "allow-dirty",
        help = "Allow running clippy even if git repository is dirty"
    )]
    pub allow_dirty: bool,
}

/// Log level of the firmware (defmt)
#[derive(Debug, Clone, ValueEnum)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
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
