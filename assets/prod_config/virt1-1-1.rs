// Default dummy product for rust-analyzer to compile without env
pub const PRODUCT: Product = Product::Virt1_1_1;
pub const DATA_FEED: &str = "virtual";
pub const MIN_DELAY_MINUTES: u32 = 0;
pub const MAX_DELAY_MINUTES: u32 = 10;
pub const MIN_SPEED_KMPH: u32 = 0;
pub const MAX_SPEED_KMPH: u32 = 100;
pub const DIMENSIONS: (f32, f32) = (50.0, 20.0);
pub const COS_LAT_Q15: i16 = 1;
pub const PIXEL_COUNT: usize = 2;
pub const PIXEL_POSITIONS: [(f32, f32); PIXEL_COUNT] = [
    (0.0, 0.0),
    (1.0, 0.0),
];
pub const PIXEL_INDICES_SPECIAL: &[u16] = &[];
pub const LOC_GEO_KD_TREE: [LocGeoKdNode; 0] = [];
pub const LOC_PIX_NODES: [LocPixNode; 0] = [];
