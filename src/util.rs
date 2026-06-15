use rgb::RGB8;

// Put a value in a static cell
#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write($val);
        x
    }};
}

pub trait BoundedInteger: Copy + PartialEq {
    const MAX: Self;
}

impl BoundedInteger for u8 {
    const MAX: Self = u8::MAX;
}

impl BoundedInteger for i8 {
    const MAX: Self = i8::MAX;
}

impl BoundedInteger for u16 {
    const MAX: Self = u16::MAX;
}

impl BoundedInteger for i16 {
    const MAX: Self = i16::MAX;
}

impl BoundedInteger for u32 {
    const MAX: Self = u32::MAX;
}

impl BoundedInteger for i32 {
    const MAX: Self = i32::MAX;
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct NonMax<T: BoundedInteger>(T);

impl<T: BoundedInteger> NonMax<T> {
    pub const NONE: Self = NonMax(T::MAX);

    pub fn new(value: T) -> Option<Self> {
        if value == T::MAX {
            None
        } else {
            Some(NonMax(value))
        }
    }

    pub fn new_unchecked(value: T) -> Self {
        NonMax(value)
    }

    pub fn is_none(&self) -> bool {
        self.0 == T::MAX
    }

    pub fn is_some(&self) -> bool {
        self.0 != T::MAX
    }

    pub fn as_option(&self) -> Option<T> {
        if self.is_none() { None } else { Some(self.0) }
    }
}

pub const fn pack_rgb8(red: u8, green: u8, blue: u8) -> u32 {
    ((red as u32) << 16) | ((green as u32) << 8) | (blue as u32)
}

pub fn rgb8_from_packed(packed: u32) -> RGB8 {
    RGB8 {
        r: ((packed >> 16) & 0xFF) as u8,
        g: ((packed >> 8) & 0xFF) as u8,
        b: (packed & 0xFF) as u8,
    }
}

pub fn lerp(start: f32, end: f32, ratio: f32) -> f32 {
    start + (end - start) * ratio
}

pub fn rgb8_brightness(rgb: RGB8, ratio: f32) -> RGB8 {
    RGB8 {
        r: (rgb.r as f32 * ratio).min(255.0) as u8,
        g: (rgb.g as f32 * ratio).min(255.0) as u8,
        b: (rgb.b as f32 * ratio).min(255.0) as u8,
    }
}
