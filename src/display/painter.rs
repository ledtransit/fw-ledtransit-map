use defmt::warn;
use embassy_time::{Duration, Instant, Timer};
use rgb::RGB8;
use smart_leds::colors::BLACK;

use crate::{
    automation,
    config::CONFIG,
    display::{
        leds::{self, LedColor, LedPixels},
        renderer::RENDERER_FPS,
    },
    net::ws_client::client_proto::{DisruptionInterval, DisruptionMode, RenderMode},
    store::{app_settings, transit_data},
    util::{lerp, rgb8_brightness, rgb8_max},
};

const PAINTER_FPS: u32 = 30;

pub fn spawn(spawner: embassy_executor::Spawner) {
    spawner.spawn(draw_task().unwrap());
}

#[embassy_executor::task]
async fn draw_task() {
    loop {
        let begin_frame_time = Instant::now();
        let setup_complete = app_settings::persist::get_settings()
            .await
            .access_token
            .is_some();
        let settings = app_settings::session::get_settings().await;

        if setup_complete && !settings.test_mode_active {
            automation::step().await;

            if settings.light_on {
                draw_frame().await;
            }
        }

        // Frame delay
        let frame_duration = Instant::now() - begin_frame_time;
        let frame_delay = Duration::from_millis(1000 / PAINTER_FPS as u64);
        if frame_duration < frame_delay {
            Timer::after(frame_delay - frame_duration).await;
        } else {
            warn!(
                "Draw: Frame processing time {}ms exceeded time budget {}ms",
                frame_duration.as_millis(),
                frame_delay.as_millis()
            );
        }
    }
}

async fn draw_frame() {
    let mut store_guard = transit_data::get_mut().await;
    let store = match store_guard.as_mut() {
        Some(guard) => guard,
        None => return, // No data
    };

    let drawn_first_frame_at_instant_ms =
        store.state.rendered_state.drawn_first_frame_at_instant_ms;
    let rendered_data_first_at_instant_ms =
        store.state.rendered_state.rendered_data_first_at_instant_ms;
    let now_instant_ms = Instant::now().as_millis();

    // If never rendered any data before, fade out LEDs first
    if drawn_first_frame_at_instant_ms.is_none() {
        leds::set_pixels(LedPixels::FadeOut).await;
        leds::wait_pixels_animation_complete().await;
        store.state.rendered_state.drawn_first_frame_at_instant_ms = Some(now_instant_ms as u32);
    }

    if rendered_data_first_at_instant_ms.is_none() {
        // Not rendered data yet, skip drawing
        return;
    }

    let config = app_settings::persist::get_settings().await.config;
    let animation_speed_unit = config.animation_speed_percent.min(200) as f32 / 200.0;
    let renderer_frame_time_ms = 1000 / RENDERER_FPS as u64;
    let render_mode =
        RenderMode::try_from(config.render_mode).unwrap_or(RenderMode::SnapClosestTransition);
    let transition_duration_ms = match render_mode {
        RenderMode::SnapClosest => 0,
        RenderMode::SnapClosestTransition => lerp(50.0, 550.0, 1.0 - animation_speed_unit) as u64,
    };

    // Clear pixel buffer
    let mut pixel_buf = leds::get_mut_pixel_buffer().await;
    pixel_buf.fill(LedColor::Black.as_rgb8());

    let disruption_count = store.state.renderer_out.disruptions.len() as u64;
    let disruption_draw_interval_ms = renderer_frame_time_ms
        .checked_div(disruption_count)
        .unwrap_or(renderer_frame_time_ms);
    let disruption_interval_ms = match DisruptionInterval::try_from(config.disruption_interval) {
        Ok(DisruptionInterval::Every2s) => 2000,
        Ok(DisruptionInterval::Every3s) => 3000,
        Ok(DisruptionInterval::Every5s) => 5000,
        Ok(DisruptionInterval::Every10s) => 10000,
        Err(_) => 5000,
    };
    let pulse_period_ms = lerp(1000.0, 3000.0, 1.0 - animation_speed_unit) as u64;
    let ripple_delay_ms = lerp(10.0, 100.0, 1.0 - animation_speed_unit) as u64;

    // Draw disruptions
    for (disruption_idx, disruption) in store.state.renderer_out.disruptions.iter().enumerate() {
        let disruption_draw_delay_ms = (disruption_idx as u64) * disruption_draw_interval_ms;
        let start_instant_ms = disruption.last_updated_instant_ms as u64 + disruption_draw_delay_ms;

        let mut rgb = RGB8 { r: 0, g: 0, b: 0 };

        if now_instant_ms < start_instant_ms {
            // Transition has not started yet, draw previous color
            if disruption.prev_rgb != BLACK {
                rgb = disruption.prev_rgb;
            }
        } else {
            let transition_elapsed_ms = now_instant_ms - start_instant_ms;
            if transition_elapsed_ms >= transition_duration_ms {
                // Transition has completed, draw current color
                if disruption.cur_rgb != BLACK {
                    rgb = disruption.cur_rgb;
                }
            } else {
                // Transition in progress
                let transition_progress_unit =
                    (transition_elapsed_ms as f32) / (transition_duration_ms as f32);

                let from_rgb = disruption.prev_rgb;
                let to_rgb = disruption.cur_rgb;

                rgb = RGB8 {
                    r: lerp(from_rgb.r as f32, to_rgb.r as f32, transition_progress_unit) as u8,
                    g: lerp(from_rgb.g as f32, to_rgb.g as f32, transition_progress_unit) as u8,
                    b: lerp(from_rgb.b as f32, to_rgb.b as f32, transition_progress_unit) as u8,
                };
            }
        }

        match DisruptionMode::try_from(config.disruption_mode) {
            Ok(DisruptionMode::Off) => {}
            Ok(DisruptionMode::Solid) => {
                for pixel in disruption.pixels.iter() {
                    pixel_buf[*pixel as usize] = rgb;
                }
            }
            Ok(DisruptionMode::Pulsing) => {
                let period_ms = pulse_period_ms.max(disruption_interval_ms);
                if (now_instant_ms % period_ms) >= pulse_period_ms {
                    // Off phase of disruption interval
                    continue;
                }
                let pulse_phase_ms = now_instant_ms % period_ms % pulse_period_ms;
                let pulse_unit = if pulse_phase_ms < pulse_period_ms / 2 {
                    // Rising
                    (pulse_phase_ms as f32) / ((pulse_period_ms / 2) as f32)
                } else {
                    // Falling
                    1.0 - ((pulse_phase_ms - (pulse_period_ms / 2)) as f32)
                        / ((pulse_period_ms / 2) as f32)
                };
                for pixel in disruption.pixels.iter() {
                    pixel_buf[*pixel as usize] = rgb8_brightness(rgb, pulse_unit);
                }
            }
            Ok(DisruptionMode::Ripple) => {
                let period_ms = pulse_period_ms.max(disruption_interval_ms);
                for (i, pixel) in disruption.pixels.iter().enumerate() {
                    let ripple_elapsed_ms = now_instant_ms.saturating_sub(start_instant_ms) as i64
                        - (i as i64 * ripple_delay_ms as i64);
                    if ripple_elapsed_ms < 0 {
                        continue;
                    }
                    if ripple_elapsed_ms as u64 % period_ms >= pulse_period_ms {
                        // Off phase of disruption interval
                        continue;
                    }
                    let ripple_phase_unit =
                        ((ripple_elapsed_ms as u64 % period_ms % pulse_period_ms) as f32)
                            / (pulse_period_ms as f32);
                    let brightness_unit = if ripple_phase_unit < 0.5 {
                        // Rising
                        ripple_phase_unit * 2.0
                    } else {
                        // Falling
                        (1.0 - ripple_phase_unit) * 2.0
                    };
                    pixel_buf[*pixel as usize] = rgb8_max(
                        pixel_buf[*pixel as usize],
                        rgb8_brightness(rgb, brightness_unit),
                    );
                }
            }
            Err(_) => {}
        }
    }

    // Space transitions over renderer frame time for more random look
    let vehicle_count = store.state.renderer_out.vehicles.len() as u64;
    let vehicle_draw_interval_ms = renderer_frame_time_ms
        .checked_div(vehicle_count)
        .unwrap_or(renderer_frame_time_ms);

    let mut z_buffer = [0u8; CONFIG.cfg.pixel_count];

    // Draw vehicles
    for (vehicle_idx, vehicle) in store.state.renderer_out.vehicles.iter().enumerate() {
        // Equally space out vehicle draw calls over renderer frame time
        let vehicle_draw_delay_ms = (vehicle_idx as u64) * vehicle_draw_interval_ms;
        let start_instant_ms = vehicle.last_updated_instant_ms as u64 + vehicle_draw_delay_ms;
        let time_since_rendered_first_sec = (start_instant_ms
            .saturating_sub(rendered_data_first_at_instant_ms.unwrap_or(0) as u64)
            / 1000)
            .min(u8::MAX as u64) as u8;

        // Check transition has not started yet, draw previous pixel
        if now_instant_ms < start_instant_ms {
            if let Some(pixel_prev) = vehicle.prev.to_option() {
                write_pixel_z(
                    &mut pixel_buf,
                    pixel_prev.idx as usize,
                    pixel_prev.rgb,
                    &mut z_buffer,
                    time_since_rendered_first_sec,
                );
            }
            continue;
        }

        // Check transition has completed, draw current pixel
        let transition_elapsed_ms = now_instant_ms - start_instant_ms;
        if transition_elapsed_ms >= transition_duration_ms {
            if let Some(pixel_cur) = vehicle.cur.to_option() {
                write_pixel_z(
                    &mut pixel_buf,
                    pixel_cur.idx as usize,
                    pixel_cur.rgb,
                    &mut z_buffer,
                    time_since_rendered_first_sec,
                );
            }
            continue;
        }

        // Transition in progress
        let transition_progress_unit =
            (transition_elapsed_ms as f32) / (transition_duration_ms as f32);

        // Interpolate colors in place
        if let Some(pixel_prev) = vehicle.prev.to_option()
            && let Some(pixel_cur) = vehicle.cur.to_option()
            && pixel_prev.idx == pixel_cur.idx
        {
            let interpolated_rgb = RGB8 {
                r: lerp(
                    pixel_prev.rgb.r as f32,
                    pixel_cur.rgb.r as f32,
                    transition_progress_unit,
                ) as u8,
                g: lerp(
                    pixel_prev.rgb.g as f32,
                    pixel_cur.rgb.g as f32,
                    transition_progress_unit,
                ) as u8,
                b: lerp(
                    pixel_prev.rgb.b as f32,
                    pixel_cur.rgb.b as f32,
                    transition_progress_unit,
                ) as u8,
            };
            write_pixel_z(
                &mut pixel_buf,
                pixel_cur.idx as usize,
                interpolated_rgb,
                &mut z_buffer,
                time_since_rendered_first_sec,
            );
            continue;
        }

        if let Some(pixel_prev) = vehicle.prev.to_option() {
            write_pixel_z(
                &mut pixel_buf,
                pixel_prev.idx as usize,
                rgb8_brightness(pixel_prev.rgb, 1.0 - transition_progress_unit),
                &mut z_buffer,
                time_since_rendered_first_sec,
            );
        }
        if let Some(pixel_cur) = vehicle.cur.to_option() {
            write_pixel_z(
                &mut pixel_buf,
                pixel_cur.idx as usize,
                rgb8_brightness(pixel_cur.rgb, transition_progress_unit),
                &mut z_buffer,
                time_since_rendered_first_sec,
            );
        }
    }

    // Count non-black pixels
    let mut pixels_on_count = 0;
    for pixel in pixel_buf.iter() {
        if *pixel != BLACK {
            pixels_on_count += 1;
        }
    }
    store.stats.num_pixels_on = pixels_on_count;

    // Update LEDs
    leds::update();
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
