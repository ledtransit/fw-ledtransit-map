// Development, release and production tasks for the firmware, run as
// `cargo xtask <command>` from the firmware repository
mod assets;
mod cli;
mod device;
mod doctor;
mod firmware;
mod product;
mod release;
mod tools;

use anyhow::Result;
use clap::Parser;

use crate::cli::Cli;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stdout)
        .init();

    match Cli::parse() {
        Cli::Doctor => doctor::run(),
        Cli::Build(args) => firmware::build(args, false),
        Cli::Run(args) => firmware::build(args, true),
        Cli::Monitor => firmware::monitor(),
        Cli::Detect => device::detect().map(|_| ()),
        Cli::Clippy(args) => firmware::clippy(args),
        Cli::Bootloader => assets::build_bootloader(),
        Cli::ProvServer => assets::build_prov_server(),
        Cli::Release(args) => release::build(args),
        Cli::Factory(args) => firmware::install_factory(args),
    }
}
