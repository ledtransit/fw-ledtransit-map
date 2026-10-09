// LED animations of the status LED and the map's pixels, each running until
// done or canceled by the next one
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer};
use esp_hal::rng::Rng;
use rgb::RGB8;
use smart_leds::hsv::{Hsv, hsv2rgb};

use crate::{
    config::CONFIG,
    display::{
        leds::{self, LedColor},
        path_find,
    },
    util::{NonMax, rgb8_brightness, rgb8_dim},
};

static LED_STATUS_ANIM_SIGNAL: Signal<CriticalSectionRawMutex, LedStatusAnimationEvent> =
    Signal::new();
static LED_STATUS_ANIM_CANCEL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static LED_PIXELS_ANIM_SIGNAL: Signal<CriticalSectionRawMutex, LedPixelsAnimationEvent> =
    Signal::new();
static LED_PIXELS_ANIM_CANCEL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static LED_PIXELS_ANIM_COMPLETE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

const ANIM_FPS: u64 = 30;
const FRAME_DURATION: Duration = Duration::from_millis(1000 / ANIM_FPS);

pub enum LedStatusAnimationEvent {
    Constant(RGB8),
    Blink(RGB8, Duration),
    Alternate(RGB8, RGB8, Duration, Duration),
}

pub enum LedPixelsAnimationEvent {
    PlayStartup(RGB8),   // Spinner + Explosion
    ProgressPercent(u8), // Progress bar on special pixels
    FadeOut,             // Reduce brightness of all pixels to 0
    Identify,            // Flash all pixels green for 1s
    TestMode,            // Rainbow waves outwards forever
    DemoMode,            // Snake random walk forever
}

struct Canceled;

pub fn spawn(spawner: Spawner) {
    spawner.spawn(led_status_anim_task().unwrap());
    spawner.spawn(led_pixels_anim_task().unwrap());
}

pub fn start_status_animation(event: LedStatusAnimationEvent) {
    LED_STATUS_ANIM_SIGNAL.signal(event);
}

pub fn start_pixels_animation(event: LedPixelsAnimationEvent) {
    LED_PIXELS_ANIM_SIGNAL.signal(event);
}

pub fn cancel_status_animation() {
    LED_STATUS_ANIM_CANCEL.signal(());
}

pub fn cancel_pixels_animation() {
    LED_PIXELS_ANIM_CANCEL.signal(());
}

pub async fn wait_until_pixels_animation_complete() {
    LED_PIXELS_ANIM_COMPLETE.wait().await;
}

#[embassy_executor::task]
async fn led_status_anim_task() {
    loop {
        let event = LED_STATUS_ANIM_SIGNAL.wait().await;
        LED_STATUS_ANIM_CANCEL.reset();
        _ = match event {
            LedStatusAnimationEvent::Constant(color) => {
                show_status(color).await;
                Ok(())
            }
            LedStatusAnimationEvent::Blink(color, duration) => {
                alternate_status(color, LedColor::Black.as_rgb8(), duration, duration).await
            }
            LedStatusAnimationEvent::Alternate(color1, color2, duration1, duration2) => {
                alternate_status(color1, color2, duration1, duration2).await
            }
        };
    }
}

#[embassy_executor::task]
async fn led_pixels_anim_task() {
    loop {
        let event = LED_PIXELS_ANIM_SIGNAL.wait().await;
        LED_PIXELS_ANIM_CANCEL.reset();
        let result = match event {
            LedPixelsAnimationEvent::PlayStartup(color) => play_startup(color).await,
            LedPixelsAnimationEvent::ProgressPercent(progress) => {
                show_progress(progress).await;
                Ok(())
            }
            LedPixelsAnimationEvent::FadeOut => fade_out().await,
            LedPixelsAnimationEvent::Identify => identify().await,
            LedPixelsAnimationEvent::TestMode => play_test_mode().await,
            LedPixelsAnimationEvent::DemoMode => play_demo_mode().await,
        };
        if result.is_ok() {
            LED_PIXELS_ANIM_COMPLETE.signal(());
        }
    }
}

async fn wait_or_cancel(
    duration: Duration,
    cancel: &Signal<CriticalSectionRawMutex, ()>,
) -> Result<(), Canceled> {
    match select(Timer::after(duration), cancel.wait()).await {
        Either::First(()) => Ok(()),
        Either::Second(()) => Err(Canceled),
    }
}

async fn wait_status(duration: Duration) -> Result<(), Canceled> {
    wait_or_cancel(duration, &LED_STATUS_ANIM_CANCEL).await
}

async fn wait_pixels(duration: Duration) -> Result<(), Canceled> {
    wait_or_cancel(duration, &LED_PIXELS_ANIM_CANCEL).await
}

async fn show_status(color: RGB8) {
    leds::set_status_pixel(color).await;
    leds::update();
}

// Forever, until canceled
async fn alternate_status(
    color1: RGB8,
    color2: RGB8,
    duration1: Duration,
    duration2: Duration,
) -> Result<(), Canceled> {
    loop {
        show_status(color1).await;
        wait_status(duration1).await?;
        show_status(color2).await;
        wait_status(duration2).await?;
    }
}

async fn fill_pixels(color: RGB8) {
    leds::get_mut_pixel_buffer()
        .await
        .iter_mut()
        .for_each(|pixel| *pixel = color);
    leds::update();
}

fn map_center() -> (f32, f32) {
    (CONFIG.cfg.dimensions.0 / 2.0, CONFIG.cfg.dimensions.1 / 2.0)
}

fn dist_sq(pos: (f32, f32), center: (f32, f32)) -> f32 {
    (pos.0 - center.0) * (pos.0 - center.0) + (pos.1 - center.1) * (pos.1 - center.1)
}

// Of the pixel farthest from the center
fn max_dist_sq(center: (f32, f32)) -> f32 {
    CONFIG
        .cfg
        .pixel_positions
        .iter()
        .map(|&pos| dist_sq(pos, center))
        .fold(0.0, |a, b| a.max(b))
}

fn scaled(color: RGB8, brightness_percent: usize) -> RGB8 {
    RGB8 {
        r: (color.r as usize * brightness_percent / 100) as u8,
        g: (color.g as usize * brightness_percent / 100) as u8,
        b: (color.b as usize * brightness_percent / 100) as u8,
    }
}

// A spinner with a trail on the special pixels, then an explosion from the
// center over all pixels, fading in and out
async fn play_startup(color: RGB8) -> Result<(), Canceled> {
    let duration = Duration::from_secs(3);
    let spinner_period = Duration::from_millis(800);
    let explosion_delay = Duration::from_secs(2);
    let num_steps = ANIM_FPS * duration.as_secs();
    let trail_length_pct = 0.7;

    let center = map_center();
    let max_dist_sq = max_dist_sq(center);
    let special_indices = CONFIG.cfg.pixel_indices_special;
    let num_special = special_indices.len();

    for step in 0..num_steps {
        let animation_progress = step as f32 / num_steps as f32;
        let fade_out_factor = 1.0 - (animation_progress - 0.8).max(0.0) / 0.2;
        let fade_in_factor = (animation_progress / 0.2).min(1.0);
        let brightness_percent = (fade_out_factor.min(fade_in_factor) * 100.0) as usize;
        let spinner_current = step as f32 / (spinner_period.as_millis() * ANIM_FPS / 1000) as f32
            * num_special as f32;

        {
            let mut pixels = leds::get_mut_pixel_buffer().await;

            if step * (1000 / ANIM_FPS) >= explosion_delay.as_millis() {
                let explosion_progress = animation_progress
                    - explosion_delay.as_millis() as f32 / duration.as_millis() as f32;
                for (i, pixel) in pixels.iter_mut().enumerate() {
                    let norm_dist_sq = dist_sq(CONFIG.cfg.pixel_positions[i], center) / max_dist_sq;
                    let pixel_brightness = (brightness_percent as f32
                        * explosion_progress
                        * 10.0
                        * (animation_progress * 1.5 - norm_dist_sq))
                        as usize;
                    *pixel = scaled(color, pixel_brightness);
                }
            }

            for (i, &pixel_idx) in special_indices.iter().enumerate() {
                let distance =
                    (spinner_current + num_special as f32 - i as f32) % num_special as f32;
                let brightness = brightness_percent.saturating_sub(
                    ((brightness_percent as f32 * distance)
                        / (trail_length_pct * num_special as f32)) as usize,
                );
                pixels[pixel_idx as usize] = scaled(color, brightness);
            }
        }

        leds::update();
        wait_pixels(FRAME_DURATION).await?;
    }
    Ok(())
}

// A progress bar on the special pixels
async fn show_progress(progress: u8) {
    let special_indices = CONFIG.cfg.pixel_indices_special;
    let num_special = special_indices.len();
    {
        let mut pixels = leds::get_mut_pixel_buffer().await;
        for (i, &pixel_idx) in special_indices.iter().enumerate() {
            let pixel_progress = (i + 1) * 100 / num_special;
            pixels[pixel_idx as usize] = if pixel_progress <= progress as usize {
                LedColor::Pink.as_rgb8()
            } else {
                LedColor::Black.as_rgb8()
            };
        }
    }
    leds::update();
}

async fn fade_out() -> Result<(), Canceled> {
    for _ in 0..255 {
        leds::get_mut_pixel_buffer()
            .await
            .iter_mut()
            .for_each(|pixel| rgb8_dim(pixel, 5));
        leds::update();

        let all_off = leds::get_mut_pixel_buffer()
            .await
            .iter()
            .all(|pixel| pixel.r == 0 && pixel.g == 0 && pixel.b == 0);
        if all_off {
            break;
        }

        wait_pixels(Duration::from_millis(0)).await?;
    }
    Ok(())
}

// All green for a second
async fn identify() -> Result<(), Canceled> {
    fill_pixels(LedColor::Green.as_rgb8()).await;
    wait_pixels(Duration::from_secs(1)).await?;
    fill_pixels(LedColor::Black.as_rgb8()).await;
    Ok(())
}

// Rainbow waves from the center, forever until canceled
async fn play_test_mode() -> Result<(), Canceled> {
    let center = map_center();
    let max_dist_sq = max_dist_sq(center);
    let mut step: u32 = 0;

    loop {
        {
            let mut pixels = leds::get_mut_pixel_buffer().await;
            for (i, pixel) in pixels.iter_mut().enumerate() {
                let dist_sq = dist_sq(CONFIG.cfg.pixel_positions[i], center);
                let hue = ((step as f32 / ANIM_FPS as f32 * 100.0 + dist_sq / max_dist_sq * 255.0)
                    as u16)
                    % 256;
                *pixel = hsv2rgb(Hsv {
                    hue: hue as u8,
                    sat: 255,
                    val: 255,
                });
            }
        }

        leds::update();
        wait_pixels(FRAME_DURATION).await?;
        step = step.wrapping_add(1);
    }
}

// Snakes walking randomly along the tracks, forever until canceled
async fn play_demo_mode() -> Result<(), Canceled> {
    const NUM_SNAKES: usize = 10;
    // Steps per move, drawn brighter each step for smoother motion
    const SUB_SAMPLE_STEPS: u32 = 3;

    let mut snakes = [Snake::NONE; NUM_SNAKES];
    let mut rng = Rng::new();
    let mut step: u32 = 0;

    loop {
        let step_sample = step % SUB_SAMPLE_STEPS;
        {
            let mut pixels = leds::get_mut_pixel_buffer().await;
            // Motion trails
            pixels.iter_mut().for_each(|pixel| rgb8_dim(pixel, 5));

            for snake in snakes.iter_mut() {
                if step_sample == 0 && !snake.move_forward(&pixels, &mut rng) {
                    snake.respawn(&mut rng);
                }
                if let Some(pixel) = snake.cur_pix.as_option() {
                    pixels[pixel as usize] = rgb8_brightness(
                        snake.color,
                        (step_sample as f32 + 1.0) / SUB_SAMPLE_STEPS as f32,
                    );
                }
            }
        }

        leds::update();
        wait_pixels(FRAME_DURATION).await?;
        step = step.wrapping_add(1);
    }
}

#[derive(Copy, Clone)]
struct Snake {
    loc: u16,              // Location node of head
    dir: NonMax<u16>,      // Direction discriminator of edge
    modes: u8,             // Allowed transit modes on current path
    cur_pix: NonMax<u16>,  // Current pixel index of head
    next_pix: NonMax<u16>, // Next pixel index of head
    color: RGB8,
}

impl Snake {
    const NONE: Snake = Snake {
        loc: 0,
        dir: NonMax::NONE,
        modes: 0xFF,
        cur_pix: NonMax::NONE,
        next_pix: NonMax::NONE,
        color: RGB8 { r: 0, g: 0, b: 0 },
    };

    /// Moves one random step, or to the pixel ahead at the end of the path.
    /// False if it can't (blocked by a lit pixel, or nowhere to go).
    fn move_forward(&mut self, pixels: &[RGB8], rng: &mut Rng) -> bool {
        if let Some((cur_pix, next_pix)) = path_find::do_random_step_from_pixel_location(
            &mut self.loc,
            &mut self.dir,
            &mut self.modes,
            rng,
        ) {
            if pixels[cur_pix as usize] != LedColor::Black.as_rgb8() {
                return false;
            }
            self.cur_pix = NonMax::new(cur_pix).unwrap();
            self.next_pix = NonMax::new(next_pix).unwrap();
            return true;
        }

        let Some(next_pix) = self.next_pix.as_option() else {
            return false;
        };
        self.cur_pix = NonMax::new(next_pix).unwrap();
        self.next_pix = NonMax::NONE;
        true
    }

    // At a random location, in a random color
    fn respawn(&mut self, rng: &mut Rng) {
        self.loc = (rng.random() as usize % CONFIG.cfg.loc_pix_nodes.len()) as u16;
        self.dir = NonMax::NONE;
        self.modes = 0xFF;
        self.cur_pix = NonMax::NONE;
        self.next_pix = NonMax::NONE;
        self.color = hsv2rgb(Hsv {
            hue: rng.random() as u8,
            sat: (rng.random() as u8).saturating_mul(3), // Bias towards more saturated colors
            val: 255,
        });
    }
}
