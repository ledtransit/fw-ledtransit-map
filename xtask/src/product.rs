use clap::ValueEnum;

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
