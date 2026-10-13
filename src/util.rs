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

macro_rules! impl_bounded_integer {
    ($($t:ty),*) => {
        $(impl BoundedInteger for $t {
            const MAX: Self = <$t>::MAX;
        })*
    };
}

impl_bounded_integer!(u8, i8, u16, i16, u32, i32);

/// An optional integer without extra space: the maximum value stands for none.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct NonMax<T: BoundedInteger>(T);

impl<T: BoundedInteger> NonMax<T> {
    pub const NONE: Self = NonMax(T::MAX);

    pub fn new(value: T) -> Option<Self> {
        (value != T::MAX).then_some(NonMax(value))
    }

    pub fn new_unchecked(value: T) -> Self {
        NonMax(value)
    }

    pub fn is_none(&self) -> bool {
        self.0 == T::MAX
    }

    pub fn is_some(&self) -> bool {
        !self.is_none()
    }

    pub fn as_option(&self) -> Option<T> {
        self.is_some().then_some(self.0)
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

pub fn rgb8_lerp(from: RGB8, to: RGB8, ratio: f32) -> RGB8 {
    RGB8 {
        r: lerp(from.r as f32, to.r as f32, ratio) as u8,
        g: lerp(from.g as f32, to.g as f32, ratio) as u8,
        b: lerp(from.b as f32, to.b as f32, ratio) as u8,
    }
}

pub fn rgb8_brightness(rgb: RGB8, ratio: f32) -> RGB8 {
    RGB8 {
        r: (rgb.r as f32 * ratio).min(255.0) as u8,
        g: (rgb.g as f32 * ratio).min(255.0) as u8,
        b: (rgb.b as f32 * ratio).min(255.0) as u8,
    }
}

pub fn rgb8_max(rgb1: RGB8, rgb2: RGB8) -> RGB8 {
    RGB8 {
        r: rgb1.r.max(rgb2.r),
        g: rgb1.g.max(rgb2.g),
        b: rgb1.b.max(rgb2.b),
    }
}

pub fn rgb8_dim(rgb: &mut RGB8, step: u8) {
    rgb.r = rgb.r.saturating_sub(step);
    rgb.g = rgb.g.saturating_sub(step);
    rgb.b = rgb.b.saturating_sub(step);
}
