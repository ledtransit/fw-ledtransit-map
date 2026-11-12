use core::iter;

use rgb::RGB8;
use smart_leds::{
    gamma,
    hsv::{Hsv, hsv2rgb},
};

use crate::{
    display::leds,
    util::{lerp, rgb8_brightness},
};

/// The color as rendered: at full intensity, shifted in color temperature.
/// The painter shows it at its brightness (see `apply_brightness`).
pub fn full_color(color: RGB8, color_temperature_shift: i32) -> RGB8 {
    adjust_color_temp(equalize_color_intensity(color), color_temperature_shift)
}

/// The full color at the brightness percent, which spans from the dimmest at
/// which the color still shows true (0: its significant channels still lit,
/// see `min_lit_value`) to full intensity (100). Applied by the painter each
/// frame: the lowest brightness follows the pixels' brightness level right
/// away, not only once the renderer ran again.
pub fn apply_brightness(color: RGB8, brightness_percent: u8, min_lit_value: u8) -> RGB8 {
    let min_brightness = min_true_color_brightness(color, min_lit_value);
    let brightness = brightness_percent.min(100) as f32 / 100.0;
    rgb8_brightness(color, lerp(min_brightness, 1.0, brightness))
}

/// The lowest channel value still lit on the pixels at the brightness level
/// they're written out with.
pub fn min_lit_value(output_brightness_level: u8) -> u8 {
    (1..=u8::MAX)
        .find(|&value| leds::output_channel_value(value, output_brightness_level) > 0)
        .unwrap_or(u8::MAX)
}

// The lowest brightness (0 to 1) keeping the weakest significant channel lit:
// below, it turns off first (pink fading to red). Channels giving off less
// than a share of the strongest one's light only tint the color, and don't
// count: else, once the brightness level just lights one at full intensity,
// the lowest brightness would jump up to full.
fn min_true_color_brightness(color: RGB8, min_lit_value: u8) -> f32 {
    const MIN_LIGHT_SHARE_INV: u16 = 8;
    let channels = [color.r, color.g, color.b];
    let strongest_light = channels.into_iter().map(led_gamma).max().unwrap_or(0) as u16;
    let Some(weakest_lit) = channels
        .into_iter()
        .filter(|&channel| {
            channel >= min_lit_value
                && led_gamma(channel) as u16 * MIN_LIGHT_SHARE_INV >= strongest_light
        })
        .min()
    else {
        return 1.0;
    };
    // Half a step up: scaled back, the channel isn't truncated just below
    ((min_lit_value as f32 + 0.5) / weakest_lit as f32).min(1.0)
}

/// The color at the ratio (0 to 1) between the two colors, in HSV.
pub fn hsv_lerp(from: Hsv, to: Hsv, ratio: f32) -> RGB8 {
    hsv2rgb(Hsv {
        hue: lerp(from.hue as f32, to.hue as f32, ratio) as u8,
        sat: lerp(from.sat as f32, to.sat as f32, ratio) as u8,
        val: lerp(from.val as f32, to.val as f32, ratio) as u8,
    })
}

// The channel's value after the gamma correction the LEDs are written with
fn led_gamma(value: u8) -> u8 {
    gamma(iter::once(RGB8::new(value, value, value)))
        .next()
        .map_or(0, |color| color.r)
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
