use std::{fs, io::ErrorKind};

use anyhow::{Context, Result, anyhow};
use clap::ValueEnum;

/// The product saved with `cargo xtask select` (gitignored)
const SELECTED_PRODUCT_FILE: &str = ".selected-product";

/// Hardware product, as <PROD>-<MAJOR>-<MINOR> (see the firmware's build.rs)
#[derive(Debug, Clone, ValueEnum, PartialEq, Eq)]
pub enum ProductId {
    Bln1_2512_1,
    Bln2_2512_1,
}

impl ProductId {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProductId::Bln1_2512_1 => "bln1-2512-1",
            ProductId::Bln2_2512_1 => "bln2-2512-1",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::value_variants()
            .iter()
            .find(|product| product.as_str() == name)
            .cloned()
    }
}

/// The product saved with `cargo xtask select`, if any
pub fn selected() -> Result<Option<ProductId>> {
    let name = match fs::read_to_string(SELECTED_PRODUCT_FILE) {
        Ok(name) => name,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("Failed to read the selected product"),
    };
    let name = name.trim();
    ProductId::from_name(name).map(Some).ok_or_else(|| {
        anyhow!(
            "Unknown product '{name}' in {SELECTED_PRODUCT_FILE}, select one again with `cargo xtask select`"
        )
    })
}

/// Saves the product for `build` and `run` to use when none is given, or
/// with `None` clears it, so they auto-detect the connected device again
pub fn select(product_id: Option<ProductId>) -> Result<()> {
    match product_id {
        Some(product_id) => {
            fs::write(SELECTED_PRODUCT_FILE, format!("{}\n", product_id.as_str()))
                .context("Failed to save the selected product")?;
            log::info!("Selected product: {}", product_id.as_str());
        }
        None => {
            match fs::remove_file(SELECTED_PRODUCT_FILE) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("Failed to clear the selected product"),
            }
            log::info!("Cleared the selected product, using the connected device's");
        }
    }
    Ok(())
}
