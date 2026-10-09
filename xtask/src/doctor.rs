// Checks of the development environment
use std::process::Command;

use anyhow::{Context, Result};

struct Tool {
    name: &'static str,
    is_required: bool,
    checks: &'static [Check],
}

struct Check {
    description: &'static str,
    command: &'static str,
    suggested_fix: &'static str,
    // Checks the command's output, besides its success
    check_output: Option<fn(&str) -> bool>,
}

const MIN_RUST_VERSION: (u32, u32, u32) = (1, 88, 0);

const TOOLS: &[Tool] = &[
    Tool {
        name: "Rust toolchain",
        is_required: true,
        checks: &[
            Check {
                description: "Rust stable toolchain is installed",
                command: "rustup toolchain list",
                suggested_fix: "Install Rust stable toolchain using 'rustup toolchain install stable --component rust-src'",
                check_output: Some(has_stable_toolchain),
            },
            Check {
                description: "Minimum Rust version is 1.88.0",
                command: "rustc --version",
                suggested_fix: "Update Rust compiler to at least version 1.88.0 using 'rustup update stable'",
                check_output: Some(is_min_rust_version),
            },
            Check {
                description: "RV32IMC target is installed",
                command: "rustup target list --installed",
                suggested_fix: "Add the RV32IMC target using 'rustup target add riscv32imc-unknown-none-elf'",
                check_output: Some(has_riscv_target),
            },
        ],
    },
    Tool {
        name: "Protobuf compiler",
        is_required: true,
        checks: &[Check {
            description: "protoc is installed",
            command: "protoc --version",
            suggested_fix: "Install protobuf compiler (see https://protobuf.dev/installation/ for instructions)",
            check_output: None,
        }],
    },
    Tool {
        name: "probe-rs",
        is_required: true,
        checks: &[Check {
            description: "probe-rs is installed",
            command: "probe-rs --version",
            suggested_fix: "Install probe-rs (see https://probe.rs/docs/getting-started/installation/ for instructions)",
            check_output: None,
        }],
    },
    Tool {
        name: "ESP flash tool",
        is_required: true,
        checks: &[Check {
            description: "espflash is installed",
            command: "espflash --version",
            suggested_fix: "Install espflash (see https://github.com/esp-rs/espflash/blob/main/cargo-espflash/README.md for instructions)",
            check_output: None,
        }],
    },
];

pub fn run() -> Result<()> {
    log::info!("Running environment checks");
    let mut required_failures = 0;
    let mut optional_failures = 0;

    for tool in TOOLS {
        log::info!("Checking {}...", tool.name);
        for check in tool.checks {
            if passes(check)? {
                log::info!("  ✓ {}", check.description);
                continue;
            }
            if tool.is_required {
                log::error!("  ✗ {} (required) ..FAILED", check.description);
                required_failures += 1;
            } else {
                log::warn!("  ✗ {} (non-critical) ..FAILED", check.description);
                optional_failures += 1;
            }
            log::info!("    └ {}", check.suggested_fix);
        }
    }

    if required_failures > 0 {
        log::error!(
            "{} critical checks failed. Please fix these issues before proceeding.",
            required_failures
        );
    } else if optional_failures > 0 {
        log::warn!(
            "{} non-critical checks failed. It's recommended to fix these issues for the best development experience.",
            optional_failures
        );
    } else {
        log::info!("All checks passed! Your development environment is set up correctly.");
    }
    Ok(())
}

fn passes(check: &Check) -> Result<bool> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(check.command)
        .output()
        .with_context(|| format!("Failed to execute command '{}'", check.command))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(output.status.success() && check.check_output.is_none_or(|check| check(&stdout)))
}

fn has_stable_toolchain(output: &str) -> bool {
    output.contains("stable")
}

fn has_riscv_target(output: &str) -> bool {
    output.contains("riscv32imc-unknown-none-elf")
}

// From "rustc 1.88.0 (…)"
fn is_min_rust_version(output: &str) -> bool {
    let version = output.split_whitespace().nth(1).unwrap_or("").trim();
    let parts: Vec<u32> = version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    match parts[..] {
        [major, minor, patch] => (major, minor, patch) >= MIN_RUST_VERSION,
        _ => false,
    }
}
