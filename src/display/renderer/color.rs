use rgb::RGB8;
use smart_leds::hsv::{Hsv, hsv2rgb};

use crate::util::{lerp, rgb8_brightness};

/// The color as shown: at full intensity, shifted in color temperature, then
/// at the brightness (0 to 1).
pub fn finish_color(color: RGB8, color_temperature_shift: i32, brightness: f32) -> RGB8 {
    rgb8_brightness(
        adjust_color_temp(equalize_color_intensity(color), color_temperature_shift),
        brightness,
    )
}

/// The color at the ratio (0 to 1) between the two colors, in HSV.
pub fn hsv_lerp(from: Hsv, to: Hsv, ratio: f32) -> RGB8 {
    hsv2rgb(Hsv {
        hue: lerp(from.hue as f32, to.hue as f32, ratio) as u8,
        sat: lerp(from.sat as f32, to.sat as f32, ratio) as u8,
        val: lerp(from.val as f32, to.val as f32, ratio) as u8,
    })
}

pub fn rgb2hsv(rgb: RGB8) -> Hsv {
    let r = rgb.r as i16;
    let g = rgb.g as i16;
    let b = rgb.b as i16;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let val = max as u8;
    let sat = if max == 0 {
        0
    } else {
        ((delta * 255) / max) as u8
    };
    let hue = if delta == 0 {
        0
    } else {
        let (sector, offset) = if max == r {
            (0, (g - b) * 43 / delta)
        } else if max == g {
            (2, (b - r) * 43 / delta)
        } else {
            (4, (r - g) * 43 / delta)
        };
        let mut h = sector * 43 + offset;
        if h < 0 {
            h += 256;
        }
        (h & 0xFF) as u8
    };

    Hsv { hue, sat, val }
}

// Scaled up until the strongest channel is at full intensity
fn equalize_color_intensity(color: RGB8) -> RGB8 {
    let max_component = color.r.max(color.g).max(color.b) as f32;
    if max_component == 0.0 {
        return RGB8 { r: 0, g: 0, b: 0 };
    }
    let scale = 255.0 / max_component;
    RGB8 {
        r: (color.r as f32 * scale).min(255.0) as u8,
        g: (color.g as f32 * scale).min(255.0) as u8,
        b: (color.b as f32 * scale).min(255.0) as u8,
    }
}

// Positive shifts warmer, negative colder
fn adjust_color_temp(color: RGB8, temp_shift: i32) -> RGB8 {
    RGB8 {
        r: (color.r as i32 + temp_shift).clamp(0, 255) as u8,
        g: (color.g as i32 + temp_shift / 2).clamp(0, 255) as u8,
        b: (color.b as i32 - temp_shift).clamp(0, 255) as u8,
    }
}
