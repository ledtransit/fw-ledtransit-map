// Product configuration and hardware info
#![allow(unexpected_cfgs)]

use envparse::parse_env;
use esp_hal::efuse;

#[allow(dead_code)]
#[derive(Debug, PartialEq, defmt::Format)]
pub enum Product {
    Virt1_1_1,
    Bln1_2512_1,
    Bln2_2512_1,
}

impl Product {
    pub fn as_str(&self) -> &'static str {
        match self {
            Product::Virt1_1_1 => "virt-1-1",
            Product::Bln1_2512_1 => "bln1-2512-1",
            Product::Bln2_2512_1 => "bln2-2512-1",
        }
    }

    pub fn as_model_str(&self) -> &'static str {
        match self {
            Product::Virt1_1_1 => "Virtual",
            Product::Bln1_2512_1 => "Berlin Rapid Transit Lightmap",
            Product::Bln2_2512_1 => "Berlin Rapid Transit Lightmap XL",
        }
    }
}

pub struct HardwareVersion {
    pub major: u32,
    pub minor: u32,
}

pub struct FirmwareVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub beta: bool,
}

type Millimeters = f32;
pub struct ProductConfig {
    pub data_feed: &'static str, // Data feed identifier to select the data source
    pub dimensions: (Millimeters, Millimeters), // Physical width/height of the map
    pub pixel_count: usize,      // Number of LEDs excluding the status LED
    pub pixel_positions: [(Millimeters, Millimeters); PIXEL_COUNT], // Physical position of each pixel from top-left corner excluding the status LED
    pub pixel_indices_special: &'static [u16], // Indices of a set of special pixels used for spinner/progress animations
    pub loc_geo_kd_tree: &'static [LocGeoKdNode], // K-D tree for geo coordinates to location mapping (0:lat/1:lng)
    pub loc_pix_nodes: &'static [LocPixNode],     // Geo nodes for location to pixel mapping
    pub min_delay_minutes: u32,
    pub max_delay_minutes: u32,
    pub min_speed_kmph: u32,
    pub max_speed_kmph: u32,
    pub cos_lat_q15: i16, // Precomputed cosine of the latitude of the location, int(cos(radians(center_lat)) * 32768)
}

pub struct LocGeoKdNode {
    pub left: Option<u16>, // Index of left child node in the K-D tree, None if leaf
    pub right: Option<u16>, // Index of right child node in the K-D tree, None if leaf
    pub loc: u16,          // Index of the location this node represents
}

pub struct LocPixNode {
    pub lat_e7: i32,                  // Integer latitude in degrees * 10^7
    pub lng_e7: i32,                  // Integer longitude in degrees * 10^7
    pub modes: u8, // Bitmask of supported transit modes e.g. subway vs light rail
    pub edges: &'static [LocPixEdge], // Directed edges to neighboring locations
}

pub struct LocPixEdge {
    pub to_loc: u16,   // Index of the destination location
    pub from_pix: u16, // Pixel index at the source location
    pub to_pix: u16,   // Pixel index at the destination location
    pub dir: u16,      // Track direction discriminator bitmask to encode possible routing 4 x [b4]
}

pub struct Config {
    pub product: Product,
    pub hw_version: HardwareVersion,
    pub fw_version: FirmwareVersion,
    pub cfg: ProductConfig,
}

const HW_MAJOR: u32 = parse_env!("HW_MAJOR" as u32);
const HW_MINOR: u32 = parse_env!("HW_MINOR" as u32);
const FW_MAJOR: u32 = parse_env!("CARGO_PKG_VERSION_MAJOR" as u32);
const FW_MINOR: u32 = parse_env!("CARGO_PKG_VERSION_MINOR" as u32);
const FW_PATCH: u32 = parse_env!("CARGO_PKG_VERSION_PATCH" as u32);
const FW_BETA: bool = !parse_env!("RELEASE" as bool else false);

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/prod_config/",
    env!("PRODUCT"),
    ".rs"
));

pub const CONFIG: Config = Config {
    product: PRODUCT,
    hw_version: HardwareVersion {
        major: HW_MAJOR,
        minor: HW_MINOR,
    },
    fw_version: FirmwareVersion {
        major: FW_MAJOR,
        minor: FW_MINOR,
        patch: FW_PATCH,
        beta: FW_BETA,
    },
    cfg: ProductConfig {
        data_feed: DATA_FEED,
        dimensions: DIMENSIONS,
        pixel_count: PIXEL_COUNT,
        pixel_positions: PIXEL_POSITIONS,
        pixel_indices_special: PIXEL_INDICES_SPECIAL,
        loc_geo_kd_tree: &LOC_GEO_KD_TREE,
        loc_pix_nodes: &LOC_PIX_NODES,
        min_delay_minutes: MIN_DELAY_MINUTES,
        max_delay_minutes: MAX_DELAY_MINUTES,
        min_speed_kmph: MIN_SPEED_KMPH,
        max_speed_kmph: MAX_SPEED_KMPH,
        cos_lat_q15: COS_LAT_Q15,
    },
};

pub fn init() {
    if CONFIG.product == Product::Virt1_1_1 {
        panic!(
            "Virtual product configuration is not meant to be flashed on hardware. Please set PRODUCT environment variable to a valid product."
        );
    }
}

pub fn get_hardware_id_str() -> heapless::String<32> {
    let unique_id_128 = efuse::read_field_le::<[u8; 16]>(efuse::OPTIONAL_UNIQUE_ID);
    let mut str = heapless::String::<32>::new();
    use core::fmt::Write;
    for byte in unique_id_128.iter() {
        write!(str, "{:02x}", byte).expect("Failed to write hardware ID string");
    }
    str
}
