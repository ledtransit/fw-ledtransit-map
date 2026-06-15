use embassy_executor::Spawner;
use embassy_futures::select::{self, select};
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
    util::{NonMax, rgb8_brightness},
};

static LED_STATUS_ANIM_SIGNAL: Signal<CriticalSectionRawMutex, LedStatusAnimationEvent> =
    Signal::new();
static LED_STATUS_ANIM_CANCEL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static LED_PIXELS_ANIM_SIGNAL: Signal<CriticalSectionRawMutex, LedPixelsAnimationEvent> =
    Signal::new();
static LED_PIXELS_ANIM_CANCEL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static LED_PIXELS_ANIM_COMPLETE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

const ANIM_FPS: u64 = 30;

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

pub fn spawn(spawner: Spawner) {
    spawner.spawn(led_status_anim_task().unwrap());
    spawner.spawn(led_pixels_anim_task().unwrap());
}

#[embassy_executor::task]
async fn led_status_anim_task() {
    'event_loop: loop {
        let event = LED_STATUS_ANIM_SIGNAL.wait().await;
        LED_STATUS_ANIM_CANCEL.reset();

        match event {
            LedStatusAnimationEvent::Constant(color) => {
                leds::set_status_pixel(color).await;
                leds::update();
            }
            LedStatusAnimationEvent::Blink(color, duration) => loop {
                leds::set_status_pixel(color).await;
                leds::update();
                if let Err(Canceled) =
                    wait_or_cancel(Timer::after(duration), LED_STATUS_ANIM_CANCEL.wait()).await
                {
                    continue 'event_loop;
                }

                leds::set_status_pixel(LedColor::Black.as_rgb8()).await;
                leds::update();
                if let Err(Canceled) =
                    wait_or_cancel(Timer::after(duration), LED_STATUS_ANIM_CANCEL.wait()).await
                {
                    continue 'event_loop;
                }
            },
            LedStatusAnimationEvent::Alternate(color1, color2, duration1, duration2) => loop {
                leds::set_status_pixel(color1).await;
                leds::update();
                if let Err(Canceled) =
                    wait_or_cancel(Timer::after(duration1), LED_STATUS_ANIM_CANCEL.wait()).await
                {
                    continue 'event_loop;
                }

                leds::set_status_pixel(color2).await;
                leds::update();
                if let Err(Canceled) =
                    wait_or_cancel(Timer::after(duration2), LED_STATUS_ANIM_CANCEL.wait()).await
                {
                    continue 'event_loop;
                }
            },
        }
    }
}

#[embassy_executor::task]
async fn led_pixels_anim_task() {
    'event_loop: loop {
        let event = LED_PIXELS_ANIM_SIGNAL.wait().await;
        LED_PIXELS_ANIM_CANCEL.reset();

        match event {
            LedPixelsAnimationEvent::PlayStartup(color) => {
                // Timings
                let duration: Duration = Duration::from_secs(3);
                let spinner_period = Duration::from_millis(800);
                let explosion_delay = Duration::from_secs(2);
                let num_steps = ANIM_FPS * duration.as_secs();

                let center = (CONFIG.cfg.dimensions.0 / 2.0, CONFIG.cfg.dimensions.1 / 2.0);
                let special_indices = CONFIG.cfg.pixel_indices_special;
                let num_special = special_indices.len();
                let trail_length_pct = 0.7;

                let max_dist_sq: f32 = CONFIG
                    .cfg
                    .pixel_positions
                    .iter()
                    .map(|&pos| {
                        let dx = pos.0 - center.0;
                        let dy = pos.1 - center.1;
                        dx * dx + dy * dy
                    })
                    .fold(0.0, |a, b| a.max(b));

                // Compute animation frames
                for step in 0..num_steps {
                    let animation_progress = step as f32 / num_steps as f32;
                    let fade_out_factor = 1.0 - (animation_progress - 0.8).max(0.0) / 0.2;
                    let fade_in_factor = (animation_progress / 0.2).min(1.0);
                    let fade_factor = fade_out_factor.min(fade_in_factor);
                    let brightness_percent = (fade_factor * 100.0) as usize;
                    let spinner_current = step as f32
                        / (spinner_period.as_millis() * ANIM_FPS / 1000) as f32
                        * num_special as f32;

                    {
                        let mut pixels = leds::get_mut_pixel_buffer().await;

                        // Explosion effect on all pixels from center, starts later
                        if step * (1000 / ANIM_FPS) >= explosion_delay.as_millis() {
                            for (i, pixel) in pixels.iter_mut().enumerate() {
                                let pos = CONFIG.cfg.pixel_positions[i];
                                let dist_sq = (pos.0 - center.0) * (pos.0 - center.0)
                                    + (pos.1 - center.1) * (pos.1 - center.1);
                                let norm_dist_sq = dist_sq / max_dist_sq;
                                let pixel_brightness = (brightness_percent as f32
                                    * (animation_progress
                                        - explosion_delay.as_millis() as f32
                                            / duration.as_millis() as f32)
                                    * 10.0
                                    * (animation_progress * 1.5 - norm_dist_sq))
                                    as usize;
                                *pixel = RGB8 {
                                    r: (color.r as usize * pixel_brightness / 100) as u8,
                                    g: (color.g as usize * pixel_brightness / 100) as u8,
                                    b: (color.b as usize * pixel_brightness / 100) as u8,
                                };
                            }
                        }

                        // Loading spinner with trail on special pixels
                        for (i, &pixel_idx) in special_indices.iter().enumerate() {
                            let distance = (spinner_current + num_special as f32 - i as f32)
                                % num_special as f32;
                            let brightness = brightness_percent.saturating_sub(
                                ((brightness_percent as f32 * distance)
                                    / (trail_length_pct * num_special as f32))
                                    as usize,
                            );
                            pixels[pixel_idx as usize] = RGB8 {
                                r: (color.r as usize * brightness / 100) as u8,
                                g: (color.g as usize * brightness / 100) as u8,
                                b: (color.b as usize * brightness / 100) as u8,
                            };
                        }
                    }

                    // Update LEDs
                    leds::update();

                    // Frame delay or cancellation
                    if let Err(Canceled) = wait_or_cancel(
                        Timer::after(Duration::from_millis(1000 / ANIM_FPS)),
                        LED_PIXELS_ANIM_CANCEL.wait(),
                    )
                    .await
                    {
                        continue 'event_loop;
                    }
                }
            }
            LedPixelsAnimationEvent::ProgressPercent(progress) => {
                let special_indices = CONFIG.cfg.pixel_indices_special;
                let num_special = special_indices.len();

                {
                    let mut pixels = leds::get_mut_pixel_buffer().await;

                    // Loading bar on special pixels
                    for (i, &pixel_idx) in special_indices.iter().enumerate() {
                        let pixel_progress = (i + 1) * 100 / num_special;
                        let pixel_on = pixel_progress <= progress as usize;
                        pixels[pixel_idx as usize] = if pixel_on {
                            LedColor::Pink.as_rgb8()
                        } else {
                            LedColor::Black.as_rgb8()
                        };
                    }
                }

                // Update LEDs
                leds::update();
            }
            LedPixelsAnimationEvent::FadeOut => {
                let num_steps = 255;
                let step_delay = Duration::from_millis(0);

                for _ in 0..num_steps {
                    {
                        let mut pixels = leds::get_mut_pixel_buffer().await;
                        for pixel in pixels.iter_mut() {
                            pixel.r = pixel.r.saturating_sub(5);
                            pixel.g = pixel.g.saturating_sub(5);
                            pixel.b = pixel.b.saturating_sub(5);
                        }
                    }

                    // Update LEDs
                    leds::update();

                    // Short circuit if already fully off
                    let all_off = {
                        let pixels = leds::get_mut_pixel_buffer().await;
                        pixels.iter().all(|p| p.r == 0 && p.g == 0 && p.b == 0)
                    };
                    if all_off {
                        break;
                    }

                    // Step delay or cancellation
                    if let Err(Canceled) =
                        wait_or_cancel(Timer::after(step_delay), LED_PIXELS_ANIM_CANCEL.wait())
                            .await
                    {
                        continue 'event_loop;
                    }
                }
            }
            LedPixelsAnimationEvent::Identify => {
                // All green
                leds::get_mut_pixel_buffer()
                    .await
                    .iter_mut()
                    .for_each(|c| *c = LedColor::Green.as_rgb8());
                leds::update();

                // Wait 1s cancelable
                if let Err(Canceled) = wait_or_cancel(
                    Timer::after(Duration::from_secs(1)),
                    LED_PIXELS_ANIM_CANCEL.wait(),
                )
                .await
                {
                    continue 'event_loop;
                }

                // Clear all
                leds::get_mut_pixel_buffer()
                    .await
                    .iter_mut()
                    .for_each(|c| *c = LedColor::Black.as_rgb8());
                leds::update();
            }
            LedPixelsAnimationEvent::TestMode => {
                // Rainbow cycle animation forever until canceled
                let mut step: u32 = 0;
                let center = (CONFIG.cfg.dimensions.0 / 2.0, CONFIG.cfg.dimensions.1 / 2.0);

                let max_dist_sq: f32 = CONFIG
                    .cfg
                    .pixel_positions
                    .iter()
                    .map(|&pos| {
                        let dx = pos.0 - center.0;
                        let dy = pos.1 - center.1;
                        dx * dx + dy * dy
                    })
                    .fold(0.0, |a, b| a.max(b));

                loop {
                    {
                        let mut pixels = leds::get_mut_pixel_buffer().await;
                        let pix_pos = &CONFIG.cfg.pixel_positions;

                        for (i, pixel) in pixels.iter_mut().enumerate() {
                            let pos = pix_pos[i];
                            let dist_sq = (pos.0 - center.0) * (pos.0 - center.0)
                                + (pos.1 - center.1) * (pos.1 - center.1);
                            let hue = ((step as f32 / ANIM_FPS as f32 * 100.0
                                + dist_sq / max_dist_sq * 255.0)
                                as u16)
                                % 256;
                            let color = hsv2rgb(Hsv {
                                hue: hue as u8,
                                sat: 255,
                                val: 255,
                            });
                            *pixel = color;
                        }
                    }

                    // Update LEDs
                    leds::update();

                    // Frame delay or cancellation
                    if let Err(Canceled) = wait_or_cancel(
                        Timer::after(Duration::from_millis(1000 / ANIM_FPS)),
                        LED_PIXELS_ANIM_CANCEL.wait(),
                    )
                    .await
                    {
                        continue 'event_loop;
                    }

                    step = step.wrapping_add(1);
                }
            }
            LedPixelsAnimationEvent::DemoMode => {
                // Moving snakes random walk on the map forever until canceled
                #[derive(Copy, Clone)]
                struct SnakeState {
                    loc: u16,              // Location node of head
                    dir: NonMax<u16>,      // Direction discriminator of edge
                    modes: u8,             // Allowed transit modes on current path
                    cur_pix: NonMax<u16>,  // Current pixel index of head
                    next_pix: NonMax<u16>, // Next pixel index of head
                    col: RGB8,             // Color of the snake
                }
                const NUM_SNAKES: usize = 10;
                const SUB_SAMPLE_STEPS: u32 = 3;
                let mut snakes = [SnakeState {
                    loc: 0,
                    dir: NonMax::NONE,
                    modes: 0xFF,
                    cur_pix: NonMax::NONE,
                    next_pix: NonMax::NONE,
                    col: LedColor::Black.as_rgb8(),
                }; NUM_SNAKES];
                let mut rng = Rng::new();
                let mut step: u32 = 0;

                loop {
                    let step_sample = step % SUB_SAMPLE_STEPS;

                    {
                        let mut pixels = leds::get_mut_pixel_buffer().await;

                        // Dim all pixels slightly for motion trail effect
                        for pixel in pixels.iter_mut() {
                            pixel.r = pixel.r.saturating_sub(5);
                            pixel.g = pixel.g.saturating_sub(5);
                            pixel.b = pixel.b.saturating_sub(5);
                        }

                        // Update snake positions and draw pixels accordingly
                        for snake in snakes.iter_mut() {
                            if step_sample == 0 {
                                // Try to move snake one random step forward
                                let has_next = if let Some((cur_pix, next_pix)) =
                                    path_find::do_random_step_from_pixel_location(
                                        &mut snake.loc,
                                        &mut snake.dir,
                                        &mut snake.modes,
                                        &mut rng,
                                    ) {
                                    if pixels[cur_pix as usize] == LedColor::Black.as_rgb8() {
                                        snake.cur_pix = NonMax::new(cur_pix).unwrap();
                                        snake.next_pix = NonMax::new(next_pix).unwrap();
                                        true
                                    } else {
                                        false // Pixel occupied, spawn new snake instead
                                    }
                                } else {
                                    if let Some(next_pix) = snake.next_pix.as_option() {
                                        // Arrived at last pixel
                                        snake.cur_pix = NonMax::new(next_pix).unwrap();
                                        snake.next_pix = NonMax::NONE;
                                        true
                                    } else {
                                        false // No where to go, spawn new snake instead
                                    }
                                };

                                if !has_next {
                                    // Spawn new snake at random location with random color
                                    snake.loc = (rng.random() as usize
                                        % CONFIG.cfg.loc_pix_nodes.len())
                                        as u16;
                                    snake.dir = NonMax::NONE;
                                    snake.modes = 0xFF;
                                    snake.cur_pix = NonMax::NONE;
                                    snake.next_pix = NonMax::NONE;
                                    snake.col = hsv2rgb(Hsv {
                                        hue: rng.random() as u8,
                                        sat: (rng.random() as u8).saturating_mul(3), // Bias towards more saturated colors
                                        val: 255,
                                    });
                                }
                            }

                            // Draw snake at current position with brightness based on sub-sample step for smoother motion
                            if let Some(pix) = snake.cur_pix.as_option() {
                                let col = rgb8_brightness(
                                    snake.col,
                                    (step_sample as f32 + 1.0) / SUB_SAMPLE_STEPS as f32,
                                );
                                pixels[pix as usize] = col;
                            }
                        }
                    }

                    // Update LEDs
                    leds::update();

                    // Frame delay or cancellation
                    if let Err(Canceled) = wait_or_cancel(
                        Timer::after(Duration::from_millis(1000 / ANIM_FPS)),
                        LED_PIXELS_ANIM_CANCEL.wait(),
                    )
                    .await
                    {
                        continue 'event_loop;
                    }

                    step = step.wrapping_add(1);
                }
            }
        }

        LED_PIXELS_ANIM_COMPLETE.signal(());
    }
}

struct Canceled;

async fn wait_or_cancel<Fut, T, C>(fut: Fut, cancel: C) -> Result<T, Canceled>
where
    Fut: Future<Output = T>,
    C: Future<Output = ()>,
{
    match select(fut, cancel).await {
        select::Either::First(result) => Ok(result),
        select::Either::Second(_) => Err(Canceled),
    }
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
