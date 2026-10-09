// Draws the renderer's output to the LEDs, with transitions between the
// rendered states spread out over each renderer frame
use defmt::warn;
use embassy_time::{Duration, Instant, Timer};
use rgb::RGB8;
use smart_leds::colors::BLACK;

use crate::{
    automation,
    config::CONFIG,
    display::{
        leds::{self, LedPixels},
        renderer::{RENDERER_FPS, RenderedDisruption, RenderedVehicle},
    },
    net::ws_client::client_proto::{DeviceConfig, DisruptionInterval, DisruptionMode, RenderMode},
    store::{app_settings, transit_data},
    time,
    util::{lerp, rgb8_brightness, rgb8_lerp, rgb8_max},
    watchdog,
};

const PAINTER_FPS: u32 = 30;
const RENDERER_FRAME_TIME_MS: u64 = 1000 / RENDERER_FPS as u64;
// Offline after the end of the simulated data
const DATA_STALE_AFTER_SECS: u32 = 120;

pub fn spawn(spawner: embassy_executor::Spawner) {
    spawner.spawn(draw_task().unwrap());
}

#[embassy_executor::task]
async fn draw_task() {
    let frame_duration_budget = Duration::from_millis(1000 / PAINTER_FPS as u64);
    loop {
        let begin_frame_time = Instant::now();
        watchdog::heartbeat();

        let is_set_up = app_settings::persist::get_settings().await.claimed;
        let settings = app_settings::session::get_settings().await;
        if is_set_up && !settings.test_mode_active {
            automation::step().await;
            draw_frame().await;
        }

        let frame_duration = Instant::now() - begin_frame_time;
        if frame_duration < frame_duration_budget {
            Timer::after(frame_duration_budget - frame_duration).await;
        } else {
            warn!(
                "Draw: Frame processing time {}ms exceeded time budget {}ms",
                frame_duration.as_millis(),
                frame_duration_budget.as_millis()
            );
        }
    }
}

async fn draw_frame() {
    let settings = app_settings::session::get_settings().await;
    let mut store_guard = transit_data::get_mut().await;
    let Some(store) = store_guard.as_mut() else {
        return;
    };
    if store.state.is_data_stale {
        // The offline animation runs
        return;
    }

    let now_timestamp = time::get_unix_timestamp_seconds().await;
    if now_timestamp.saturating_sub(store.data.simulated_until_unix_timestamp)
        > DATA_STALE_AFTER_SECS
    {
        drop(store_guard);
        go_offline(settings.light_on).await;
        return;
    }

    let rendered_state = store.state.rendered_state.clone();
    let now_instant_ms = Instant::now().as_millis();

    // Before the first frame: fade out what was shown before
    if rendered_state.drawn_first_frame_at_instant_ms.is_none() {
        leds::set_pixels(LedPixels::FadeOut).await;
        leds::wait_pixels_animation_complete().await;
        store.state.rendered_state.drawn_first_frame_at_instant_ms = Some(now_instant_ms as u32);
    }

    let Some(rendered_data_first_at_instant_ms) = rendered_state.rendered_data_first_at_instant_ms
    else {
        return;
    };
    if !settings.light_on {
        return;
    }

    let config = app_settings::persist::get_settings().await.config;
    let animation_speed_unit = config.animation_speed_percent.min(200) as f32 / 200.0;
    let render_mode =
        RenderMode::try_from(config.render_mode).unwrap_or(RenderMode::SnapClosestTransition);
    let transition_duration_ms = match render_mode {
        RenderMode::SnapClosest => 0,
        RenderMode::SnapClosestTransition => lerp(50.0, 550.0, 1.0 - animation_speed_unit) as u64,
    };
    let config_changed_this_render_frame = now_instant_ms
        .saturating_sub(rendered_state.config_last_changed_at_instant_ms as u64)
        < RENDERER_FRAME_TIME_MS * 2;

    let mut pixel_buf = leds::get_mut_pixel_buffer().await;
    pixel_buf.fill(BLACK);
    if config.disruptions_enabled {
        draw_disruptions(
            &mut pixel_buf,
            &store.state.renderer_out.disruptions,
            &config,
            now_instant_ms,
            transition_duration_ms,
            animation_speed_unit,
        );
    }
    draw_vehicles(
        &mut pixel_buf,
        &store.state.renderer_out.vehicles,
        now_instant_ms,
        transition_duration_ms,
        config_changed_this_render_frame,
        rendered_data_first_at_instant_ms,
    );

    store.stats.num_pixels_on = pixel_buf.iter().filter(|pixel| **pixel != BLACK).count() as u32;
    leds::update();
}

async fn go_offline(light_on: bool) {
    transit_data::on_data_stale().await;
    leds::set_pixels(LedPixels::FadeOut).await;
    leds::wait_pixels_animation_complete().await;
    if light_on {
        leds::set_pixels(LedPixels::DemoMode).await;
    }
}

fn draw_disruptions(
    pixel_buf: &mut [RGB8],
    disruptions: &[RenderedDisruption],
    config: &DeviceConfig,
    now_instant_ms: u64,
    transition_duration_ms: u64,
    animation_speed_unit: f32,
) {
    let disruption_count = disruptions.len() as u64;
    let draw_interval_ms = RENDERER_FRAME_TIME_MS
        .checked_div(disruption_count)
        .unwrap_or(RENDERER_FRAME_TIME_MS);
    let interval_ms = match DisruptionInterval::try_from(config.disruption_interval) {
        Ok(DisruptionInterval::Every2s) => 2000,
        Ok(DisruptionInterval::Every3s) => 3000,
        Ok(DisruptionInterval::Every5s) | Err(_) => 5000,
        Ok(DisruptionInterval::Every10s) => 10000,
    };
    let pulse_period_ms = lerp(1000.0, 3000.0, 1.0 - animation_speed_unit) as u64;
    let ripple_delay_ms = lerp(10.0, 100.0, 1.0 - animation_speed_unit) as u64;
    // A pulse, then off for the rest of the interval
    let period_ms = pulse_period_ms.max(interval_ms);

    for disruption in disruptions {
        // Spread over the renderer frame, to look less synchronized
        let slot = pseudo_random_slot(
            disruption.disruption_id.as_option().unwrap_or_default() as u32,
            disruption_count as u32,
        );
        let start_instant_ms =
            disruption.last_updated_instant_ms as u64 + slot as u64 * draw_interval_ms;
        let rgb = transition_color(
            disruption.prev_rgb,
            disruption.cur_rgb,
            now_instant_ms,
            start_instant_ms,
            transition_duration_ms,
        );

        match DisruptionMode::try_from(config.disruption_mode) {
            Ok(DisruptionMode::Solid) => {
                for &pixel in disruption.pixels.iter() {
                    pixel_buf[pixel as usize] = rgb;
                }
            }
            Ok(DisruptionMode::Pulsing) => {
                if now_instant_ms % period_ms >= pulse_period_ms {
                    continue;
                }
                let pulse_phase_ms = now_instant_ms % period_ms % pulse_period_ms;
                let half_period_ms = pulse_period_ms / 2;
                let pulse_unit = if pulse_phase_ms < half_period_ms {
                    (pulse_phase_ms as f32) / (half_period_ms as f32)
                } else {
                    1.0 - ((pulse_phase_ms - half_period_ms) as f32) / (half_period_ms as f32)
                };
                for &pixel in disruption.pixels.iter() {
                    pixel_buf[pixel as usize] = rgb8_brightness(rgb, pulse_unit);
                }
            }
            Ok(DisruptionMode::Ripple) => {
                // Pulses running along the pixels
                for (i, &pixel) in disruption.pixels.iter().enumerate() {
                    let ripple_elapsed_ms = now_instant_ms.saturating_sub(start_instant_ms) as i64
                        - (i as i64 * ripple_delay_ms as i64);
                    if ripple_elapsed_ms < 0
                        || ripple_elapsed_ms as u64 % period_ms >= pulse_period_ms
                    {
                        continue;
                    }
                    let ripple_phase_unit =
                        ((ripple_elapsed_ms as u64 % period_ms % pulse_period_ms) as f32)
                            / (pulse_period_ms as f32);
                    let brightness_unit = if ripple_phase_unit < 0.5 {
                        ripple_phase_unit * 2.0
                    } else {
                        (1.0 - ripple_phase_unit) * 2.0
                    };
                    pixel_buf[pixel as usize] = rgb8_max(
                        pixel_buf[pixel as usize],
                        rgb8_brightness(rgb, brightness_unit),
                    );
                }
            }
            Err(_) => {}
        }
    }
}

fn draw_vehicles(
    pixel_buf: &mut [RGB8],
    vehicles: &[RenderedVehicle],
    now_instant_ms: u64,
    transition_duration_ms: u64,
    config_changed_this_render_frame: bool,
    rendered_data_first_at_instant_ms: u32,
) {
    let vehicle_count = vehicles.len() as u64;
    let draw_interval_ms = RENDERER_FRAME_TIME_MS
        .checked_div(vehicle_count)
        .unwrap_or(RENDERER_FRAME_TIME_MS);
    let mut z_buffer = [0u8; CONFIG.cfg.pixel_count];

    for vehicle in vehicles {
        // Spread over the renderer frame, to look less synchronized. Right
        // away if the config just changed (could overlap the previous frame)
        let updated_this_render_frame = now_instant_ms
            .saturating_sub(vehicle.last_updated_instant_ms as u64)
            < RENDERER_FRAME_TIME_MS * 2;
        let slot = pseudo_random_slot(
            vehicle.trip_id.as_option().unwrap_or_default() as u32,
            vehicle_count as u32,
        );
        let draw_delay_ms = if config_changed_this_render_frame && updated_this_render_frame {
            0
        } else {
            slot as u64 * draw_interval_ms
        };
        let start_instant_ms = vehicle.last_updated_instant_ms as u64 + draw_delay_ms;

        // Later transitions are drawn over earlier ones
        let z_value = (start_instant_ms.saturating_sub(rendered_data_first_at_instant_ms as u64)
            / 1000)
            .min(u8::MAX as u64) as u8;
        let mut draw = |idx: u16, color: RGB8| {
            write_pixel_z(pixel_buf, idx as usize, color, &mut z_buffer, z_value)
        };

        let prev = vehicle.prev.to_option();
        let cur = vehicle.cur.to_option();
        if now_instant_ms < start_instant_ms {
            if let Some(prev) = prev {
                draw(prev.idx, prev.rgb);
            }
            continue;
        }
        let transition_elapsed_ms = now_instant_ms - start_instant_ms;
        if transition_elapsed_ms >= transition_duration_ms {
            if let Some(cur) = cur {
                draw(cur.idx, cur.rgb);
            }
            continue;
        }

        // Transition: in place, or fading from one pixel to the other
        let progress = (transition_elapsed_ms as f32) / (transition_duration_ms as f32);
        match (prev, cur) {
            (Some(prev), Some(cur)) if prev.idx == cur.idx => {
                draw(cur.idx, rgb8_lerp(prev.rgb, cur.rgb, progress));
            }
            (prev, cur) => {
                if let Some(prev) = prev {
                    draw(prev.idx, rgb8_brightness(prev.rgb, 1.0 - progress));
                }
                if let Some(cur) = cur {
                    draw(cur.idx, rgb8_brightness(cur.rgb, progress));
                }
            }
        }
    }
}

// The color of a transition from prev to cur starting at the instant
fn transition_color(
    prev: RGB8,
    cur: RGB8,
    now_instant_ms: u64,
    start_instant_ms: u64,
    transition_duration_ms: u64,
) -> RGB8 {
    if now_instant_ms < start_instant_ms {
        return prev;
    }
    let transition_elapsed_ms = now_instant_ms - start_instant_ms;
    if transition_elapsed_ms >= transition_duration_ms {
        return cur;
    }
    rgb8_lerp(
        prev,
        cur,
        (transition_elapsed_ms as f32) / (transition_duration_ms as f32),
    )
}

fn write_pixel_z(
    pix_buffer: &mut [RGB8],
    idx: usize,
    color: RGB8,
    z_buffer: &mut [u8],
    z_value: u8,
) {
    if z_buffer[idx] <= z_value {
        z_buffer[idx] = z_value;
        pix_buffer[idx] = color;
    }
}

fn pseudo_random_slot(id: u32, num_slots: u32) -> u32 {
    // Knuth multiplicative hash
    id.wrapping_mul(0x9E3779B1).wrapping_add(0x7F4A7C15) % num_slots
}
