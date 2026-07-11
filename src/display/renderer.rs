use core::iter;

use alloc::{vec, vec::Vec};
use defmt::warn;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use rgb::RGB8;
use smart_leds::{
    colors::BLACK,
    hsv::{Hsv, hsv2rgb},
};

use crate::{
    config::CONFIG,
    display::path_find,
    net::ws_client::{
        self,
        client_proto::{
            AvailableDisruptedLine, AvailableVehicleLine, ColorMode, Coordinates, DisruptionFilter,
            RealtimeFilter, VehicleFilter, ViaStop,
        },
    },
    store::{
        app_settings,
        transit_data::{self, TransitDataStore},
    },
    time, trace,
    util::{NonMax, lerp, rgb8_brightness, rgb8_from_packed},
};

pub const RENDERER_FPS: u32 = 1; // No need to render more than once per second, since vehicle departure times and delays have a granularity of 1 second

pub static RENDER_NOW_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[derive(Clone)]
pub struct RendererState {
    pub rendered_data_first_at_instant_ms: Option<u32>,
    pub rendered_any_first_at_instant_ms: Option<u32>,
    pub drawn_first_frame_at_instant_ms: Option<u32>,
    pub disruptions_render_pending: bool,
}

#[derive(Clone)]
pub struct RendererOutput {
    pub vehicles: Vec<RenderedVehicle>,
    pub disruptions: Vec<RenderedDisruption>,
}

#[derive(Clone)]
pub struct RenderedVehicle {
    pub prev: PixelState,
    pub cur: PixelState,
    pub last_updated_instant_ms: u32,
    pub trip_id: NonMax<u16>, // Stable unique ID
}

impl Default for RenderedVehicle {
    fn default() -> Self {
        RenderedVehicle {
            prev: PixelState::NONE,
            cur: PixelState::NONE,
            last_updated_instant_ms: 0,
            trip_id: NonMax::NONE,
        }
    }
}

impl RenderedVehicle {
    pub fn new(trip_id: u16) -> Self {
        RenderedVehicle {
            prev: PixelState::NONE,
            cur: PixelState::NONE,
            last_updated_instant_ms: 0,
            trip_id: NonMax::new_unchecked(trip_id),
        }
    }
}

#[derive(Clone)]
pub struct RenderedDisruption {
    pub pixels: Vec<u16>,
    pub prev_rgb: RGB8,
    pub cur_rgb: RGB8,
    pub last_updated_instant_ms: u32,
    pub disruption_id: NonMax<u16>, // Stable unique ID
}

impl Default for RenderedDisruption {
    fn default() -> Self {
        RenderedDisruption {
            pixels: vec![],
            prev_rgb: BLACK,
            cur_rgb: BLACK,
            last_updated_instant_ms: 0,
            disruption_id: NonMax::NONE,
        }
    }
}

impl RenderedDisruption {
    pub fn new(disruption_id: u16) -> Self {
        RenderedDisruption {
            pixels: vec![],
            prev_rgb: BLACK,
            cur_rgb: BLACK,
            last_updated_instant_ms: 0,
            disruption_id: NonMax::new_unchecked(disruption_id),
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
pub struct PixelState {
    pub rgb: RGB8,
    pub idx: NonMax<u16>,
}

pub struct PixelStateSome {
    pub rgb: RGB8,
    pub idx: u16,
}

impl PixelState {
    pub const NONE: Self = PixelState {
        rgb: BLACK,
        idx: NonMax::NONE,
    };

    pub fn to_option(self) -> Option<PixelStateSome> {
        self.idx
            .as_option()
            .map(|idx| PixelStateSome { rgb: self.rgb, idx })
    }
}

pub fn spawn(spawner: Spawner) {
    spawner.spawn(renderer_task().unwrap());
}

#[embassy_executor::task]
async fn renderer_task() {
    loop {
        RENDER_NOW_SIGNAL.reset();
        let begin_frame_time = Instant::now();
        let setup_complete = app_settings::persist::get_settings()
            .await
            .access_token
            .is_some();
        let settings = app_settings::session::get_settings().await;

        if setup_complete {
            trace::flush_errors();

            if settings.light_on {
                render_vehicles().await;

                if is_disruptions_render_pending().await {
                    render_disruptions().await;
                }
            }
        }

        // Wait until next frame time, or until render_now() is called
        let frame_duration = Instant::now() - begin_frame_time;
        let frame_delay = Duration::from_millis(1000 / RENDERER_FPS as u64);
        if frame_duration < frame_delay {
            with_timeout(frame_delay - frame_duration, RENDER_NOW_SIGNAL.wait())
                .await
                .ok();
        } else {
            warn!(
                "Renderer: Frame processing time {}ms exceeded time budget {}ms",
                frame_duration.as_millis(),
                frame_delay.as_millis()
            );
        }
    }
}

async fn is_disruptions_render_pending() -> bool {
    return match transit_data::get_mut().await.as_mut() {
        Some(s) => s.state.rendered_state.disruptions_render_pending,
        None => false,
    };
}

pub fn render_now() {
    RENDER_NOW_SIGNAL.signal(());
}

async fn render_vehicles() {
    let mut store_guard = transit_data::get_mut().await;
    let store = match store_guard.as_mut() {
        Some(guard) => guard,
        None => return, // No data
    };

    let lines = &store.data.lines;
    let stops = &store.data.stops;
    let vehicles = &store.data.vehicle_movements;

    let now_timestamp_secs = time::get_unix_timestamp_seconds().await;
    let now_instant_ms = Instant::now().as_millis() as u32;
    let source_timestamp = store.data.sourced_at_unix_timestamp;

    let config = app_settings::persist::get_settings().await.config;
    let primary_color = rgb8_from_packed(config.primary_color_rgb8);
    let secondary_color = rgb8_from_packed(config.secondary_color_rgb8);
    let tertiary_color = rgb8_from_packed(config.tertiary_color_rgb8);
    let primary_hsv = rgb2hsv(primary_color);
    let secondary_hsv = rgb2hsv(secondary_color);
    let vehicle_filter_meters_sq = (config.vehicle_distance_threshold_meters as i64).pow(2);

    let mut num_vehicles_available: u32 = 0;
    let mut num_vehicles_visible: u32 = 0;
    let mut num_vehicles_visible_real_time: u32 = 0;

    // Render each vehicle
    for (vehicle_idx, vehicle) in vehicles.iter().enumerate() {
        // In case vehicle will not be rendered, turn off pixel
        let prev_rendered_vehicle = store.state.renderer_out.vehicles[vehicle_idx].clone();
        store.state.renderer_out.vehicles[vehicle_idx].prev = prev_rendered_vehicle.cur;
        store.state.renderer_out.vehicles[vehicle_idx].cur = PixelState::NONE;
        store.state.renderer_out.vehicles[vehicle_idx].last_updated_instant_ms = now_instant_ms;

        let total_duration_seconds = vehicle.segments.iter().fold(0u32, |acc, seg| {
            let move_seconds = seg.move_seconds_x_wait_seconds >> 16;
            let wait_seconds = seg.move_seconds_x_wait_seconds & 0xFFFF;
            acc + move_seconds + wait_seconds
        });

        let line_id = vehicle.line_id_x_trip_id >> 16;
        let start_offset_seconds = vehicle
            .segments
            .first()
            .map(|seg| {
                (((seg.canceled_x_start_offset_seconds_x_via_total_move_seconds >> 16) & 0x7FFF)
                    as i16)
                    << 1
                    >> 1 // Sign extend i15 to i16
            })
            .unwrap_or(0);

        let movement_start_timestamp =
            (source_timestamp as i32 + start_offset_seconds as i32) as u32;
        let movement_end_timestamp = movement_start_timestamp + total_duration_seconds;

        // Check movement has not begun or already ended
        if now_timestamp_secs < movement_start_timestamp
            || now_timestamp_secs >= movement_end_timestamp
        {
            continue;
        }

        // Find current movement segment
        let (segment, segment_progress_secs) = {
            let mut accum_timestamp = movement_start_timestamp;
            let mut seg_opt = None;
            for seg in vehicle.segments.iter() {
                let seg_start_timestamp = accum_timestamp;
                let move_seconds = seg.move_seconds_x_wait_seconds >> 16;
                let wait_seconds = seg.move_seconds_x_wait_seconds & 0xFFFF;
                let seg_end_timestamp = seg_start_timestamp + move_seconds + wait_seconds;
                // Check current time within segment
                if now_timestamp_secs >= seg_start_timestamp
                    && now_timestamp_secs < seg_end_timestamp
                {
                    seg_opt = Some((seg, now_timestamp_secs - seg_start_timestamp));
                    break;
                }
                accum_timestamp = seg_end_timestamp;
            }
            match seg_opt {
                Some(s) => s,
                None => continue,
            }
        };

        let segment_from_stop_id = segment.from_stop_id_x_to_stop_id >> 16;
        let segment_to_stop_id = segment.from_stop_id_x_to_stop_id & 0xFFFF;
        let segment_move_seconds = segment.move_seconds_x_wait_seconds >> 16;
        let segment_wait_seconds = segment.move_seconds_x_wait_seconds & 0xFFFF;
        let segment_total_via_move_secs =
            segment.canceled_x_start_offset_seconds_x_via_total_move_seconds & 0xFFFF;
        let _segment_is_canceled =
            (segment.canceled_x_start_offset_seconds_x_via_total_move_seconds & 0x80000000) != 0;

        // Get segment from and to coordinates
        let from_coord = match stops.get(segment_from_stop_id as usize) {
            Some(c) => (c.is_station_x_latitude_e7 & 0x7FFFFFFF, c.longitude_e7),
            None => {
                trace::err!(
                    "Vehicle {} segment references invalid FROM stop ID {} (malformed)",
                    vehicle_idx,
                    segment_from_stop_id
                );
                continue;
            }
        };
        let to_coord = match stops.get(segment_to_stop_id as usize) {
            Some(c) => (c.is_station_x_latitude_e7 & 0x7FFFFFFF, c.longitude_e7),
            None => {
                trace::err!(
                    "Vehicle {} segment references invalid TO stop ID {} (malformed)",
                    vehicle_idx,
                    segment_to_stop_id
                );
                continue;
            }
        };

        // Get current position within segment
        let current_coord = if segment_progress_secs < segment_move_seconds {
            // Moving
            if !segment.via_stops.is_empty() {
                // Linear interpolation via via stops
                let mut last_coord = from_coord;
                let mut accum_move_secs = 0;
                let mut found_coord_opt = None;

                for via_stop in segment.via_stops.iter().chain(iter::once(&ViaStop {
                    stop_id_x_move_seconds: (segment_to_stop_id << 16)
                        | (segment_move_seconds - segment_total_via_move_secs),
                })) {
                    let via_stop_id = via_stop.stop_id_x_move_seconds >> 16;
                    let via_move_seconds = via_stop.stop_id_x_move_seconds & 0xFFFF;

                    let via_coord = match stops.get(via_stop_id as usize) {
                        Some(c) => (c.is_station_x_latitude_e7 & 0x7FFFFFFF, c.longitude_e7),
                        None => {
                            trace::err!(
                                "Vehicle {} segment references invalid VIA stop ID {} (malformed)",
                                vehicle_idx,
                                via_stop_id
                            );
                            continue;
                        }
                    };
                    let leg_move_secs = via_move_seconds;
                    if segment_progress_secs < accum_move_secs + leg_move_secs {
                        // Current position within this leg
                        let ratio =
                            (segment_progress_secs - accum_move_secs) as f32 / leg_move_secs as f32;
                        found_coord_opt = Some((
                            lerp(last_coord.0 as f32, via_coord.0 as f32, ratio) as i32,
                            lerp(last_coord.1 as f32, via_coord.1 as f32, ratio) as i32,
                        ));
                        break;
                    }
                    accum_move_secs += leg_move_secs;
                    last_coord = via_coord;
                }
                match found_coord_opt {
                    Some(c) => c,
                    None => to_coord,
                }
            } else {
                // Linear interpolation between from and to
                let ratio = segment_progress_secs as f32 / segment_move_seconds as f32;
                (
                    lerp(from_coord.0 as f32, to_coord.0 as f32, ratio) as i32,
                    lerp(from_coord.1 as f32, to_coord.1 as f32, ratio) as i32,
                )
            }
        } else {
            // Waiting at end position
            to_coord
        };
        num_vehicles_available += 1;

        if config.vehicle_filter == VehicleFilter::StationaryOnly as i32 {
            let is_moving = segment_progress_secs < segment_move_seconds;
            let has_wait_time = segment_wait_seconds > 0;
            if has_wait_time && is_moving {
                // Vehicle is moving, followed by a known wait time -> skip
                continue;
            }
            let total_secs = segment_move_seconds + segment_wait_seconds;
            const MIN_MISSING_WAIT_TIME_SECONDS: u32 = 30;
            if !has_wait_time
                && segment_progress_secs < total_secs.saturating_sub(MIN_MISSING_WAIT_TIME_SECONDS)
            {
                // Vehicle has no wait time defined, and is not within last N seconds of segment -> skip
                continue;
            }
        }

        // Lookup line
        let line = match lines.get(line_id as usize) {
            Some(l) => l,
            None => {
                trace::err!(
                    "Vehicle {} references invalid line ID {} (malformed)",
                    vehicle_idx,
                    line_id
                );
                continue;
            }
        };

        // Lookup line config
        let line_config = match config
            .line_configs
            .iter()
            .find(|over| over.line_name == line.name)
        {
            Some(lc) => lc,
            _ => {
                trace::err!(
                    "Vehicle {} line ID {} name '{}' has no line config",
                    vehicle_idx,
                    line_id,
                    line.name
                );
                continue;
            }
        };

        // Get line enabled
        let line_enabled = if line_config.has_override {
            line_config.enabled
        } else {
            true
        };
        if !line_enabled {
            continue;
        }

        // Get line color
        let line_color = if line_config.has_override {
            rgb8_from_packed(line_config.override_color_rgb8)
        } else {
            rgb8_from_packed(line.color_rgb8)
        };

        // Get line brightness
        let line_brightness_percent = if line_config.has_override {
            line_config.brightness_percent.min(100)
        } else {
            100
        };

        let delayed_seconds =
            NonMax::new_unchecked((segment.delayed_seconds_x_average_speed_kmph >> 16) as i16);
        let average_speed_kmph = segment.delayed_seconds_x_average_speed_kmph & 0xFFFF;

        // Check should render vehicles without real time data
        if config.realtime_filter == RealtimeFilter::RealtimeOnly as i32
            && delayed_seconds.is_none()
        {
            continue;
        }

        // Get from and to pixel locations
        let from_loc_id = match store
            .state
            .stop_id_to_loc_id_map
            .get(segment_from_stop_id as usize)
            .and_then(|opt| opt.as_option())
        {
            Some(loc_id) => loc_id,
            _ => {
                trace::err!(
                    "Vehicle {} ({}) segment from stop ID {} at ({}, {}) has no pixel location mapping",
                    vehicle_idx,
                    line.name,
                    segment_from_stop_id,
                    from_coord.0,
                    from_coord.1
                );
                continue;
            }
        };
        let to_loc_id = match store
            .state
            .stop_id_to_loc_id_map
            .get(segment_to_stop_id as usize)
            .and_then(|opt| opt.as_option())
        {
            Some(loc_id) => loc_id,
            _ => {
                trace::err!(
                    "Vehicle {} ({}) segment to stop ID {} at ({}, {}) has no pixel location mapping",
                    vehicle_idx,
                    line.name,
                    segment_to_stop_id,
                    to_coord.0,
                    to_coord.1
                );
                continue;
            }
        };

        const MAX_PATH_LEN: usize = 8;
        const MAX_OPEN_NODES: usize = 32;

        let mut open_heap = [Default::default(); MAX_OPEN_NODES];
        let mut g_score = [0i64; CONFIG.cfg.loc_pix_nodes.len()];
        let mut came_from = [Default::default(); CONFIG.cfg.loc_pix_nodes.len()];
        let mut closed_set = [Default::default(); CONFIG.cfg.loc_pix_nodes.len()];
        let mut path_buf = [Default::default(); MAX_PATH_LEN];

        // Find valid path between pixel locations to determine which pixels belong to the line the vehicle is on
        let result = path_find::find_shortest_path_between_pixel_locations(
            from_loc_id,
            to_loc_id,
            &mut open_heap,
            &mut g_score,
            &mut came_from,
            &mut closed_set,
            &mut path_buf,
        );
        let pixel_cur = match result {
            Ok(path) => {
                // Find the edge where the originating location is closest to the current coordinates
                let loc_nodes = CONFIG.cfg.loc_pix_nodes;
                let closest_edge_from = path
                    .iter()
                    .map(|edge| {
                        let loc_node = &loc_nodes[edge.from_loc as usize];
                        (
                            approx_dist_sq_meters_e7(
                                current_coord.0,
                                current_coord.1,
                                loc_node.lat_e7,
                                loc_node.lng_e7,
                                CONFIG.cfg.cos_lat_q15,
                            ),
                            edge.from_pix,
                        )
                    })
                    .filter(|(dist_sq_meters, _)| {
                        if config.vehicle_filter == VehicleFilter::WithinDistance as i32 {
                            *dist_sq_meters <= vehicle_filter_meters_sq
                        } else {
                            true
                        }
                    })
                    .min();
                match closest_edge_from {
                    Some((_, pix_idx)) => pix_idx,
                    None => {
                        // No edge within distance threshold, skip rendering this vehicle
                        continue;
                    }
                }
            }
            Err(e) => {
                trace::err!(
                    "Vehicle {} ({}) segment from loc ID {} at ({}, {}) to loc ID {} at ({}, {}): no path found ({})",
                    vehicle_idx,
                    line.name,
                    from_loc_id,
                    from_coord.0,
                    from_coord.1,
                    to_loc_id,
                    to_coord.0,
                    to_coord.1,
                    e
                );
                continue;
            }
        };

        // Determine color
        let color = match ColorMode::try_from(config.color_mode) {
            Ok(ColorMode::Original) => line_color,
            Ok(ColorMode::Monochrome) => primary_color,
            Ok(ColorMode::RangeSpacedApart) => {
                // Place line index within primary to secondary HSV range
                let lines_count = lines.len().max(1) as f32;
                let ratio = (line_id as f32) / lines_count;
                hsv2rgb(Hsv {
                    hue: (lerp(primary_hsv.hue as f32, secondary_hsv.hue as f32, ratio) as u8),
                    sat: (lerp(primary_hsv.sat as f32, secondary_hsv.sat as f32, ratio) as u8),
                    val: (lerp(primary_hsv.val as f32, secondary_hsv.val as f32, ratio) as u8),
                })
            }
            Ok(ColorMode::DelayHeatmap) => {
                // Color based on delay seconds
                if let Some(delay_secs) = delayed_seconds.as_option() {
                    let delay_secs = delay_secs as i32;
                    let min_delay_secs = (config.min_delay_minutes as i32).max(0) * 60;
                    let max_delay_secs =
                        (config.max_delay_minutes as i32 * 60).max(min_delay_secs + 1);
                    let delay_secs = delay_secs.clamp(min_delay_secs, max_delay_secs);
                    let ratio = (delay_secs - min_delay_secs) as f32
                        / (max_delay_secs - min_delay_secs).max(1) as f32;
                    hsv2rgb(Hsv {
                        hue: (lerp(primary_hsv.hue as f32, secondary_hsv.hue as f32, ratio) as u8),
                        sat: (lerp(primary_hsv.sat as f32, secondary_hsv.sat as f32, ratio) as u8),
                        val: (lerp(primary_hsv.val as f32, secondary_hsv.val as f32, ratio) as u8),
                    })
                } else {
                    tertiary_color
                }
            }
            Ok(ColorMode::SpeedHeatmap) => {
                // Color based on speed kmph
                let min_speed_kmph = config.min_speed_kmph as i32;
                let max_speed_kmph = (config.max_speed_kmph as i32).max(min_speed_kmph + 1);
                let speed_kmph = average_speed_kmph.clamp(min_speed_kmph, max_speed_kmph);
                let ratio = 1.0
                    - (speed_kmph - min_speed_kmph) as f32
                        / (max_speed_kmph - min_speed_kmph).max(1) as f32;
                hsv2rgb(Hsv {
                    hue: (lerp(primary_hsv.hue as f32, secondary_hsv.hue as f32, ratio) as u8),
                    sat: (lerp(primary_hsv.sat as f32, secondary_hsv.sat as f32, ratio) as u8),
                    val: (lerp(primary_hsv.val as f32, secondary_hsv.val as f32, ratio) as u8),
                })
            }
            Err(_) => line_color,
        };

        // Compute final RGB color
        let rgb_color = rgb8_brightness(
            adjust_color_temp(
                equalize_color_intensity(color),
                config.color_temperature_shift,
            ),
            line_brightness_percent as f32 / 100.0,
        );

        // Updated rendered vehicle state if pixel changed
        let pix_idx = match NonMax::new(pixel_cur) {
            Some(idx) => idx,
            None => {
                trace::err!(
                    "Vehicle {} ({}) segment from loc ID {} at ({}, {}) to loc ID {} at ({}, {}): pixel index is invalid",
                    vehicle_idx,
                    line.name,
                    from_loc_id,
                    from_coord.0,
                    from_coord.1,
                    to_loc_id,
                    to_coord.0,
                    to_coord.1
                );
                continue;
            }
        };
        let pixel_state = PixelState {
            rgb: rgb_color,
            idx: pix_idx,
        };
        if prev_rendered_vehicle.cur != pixel_state {
            store.state.renderer_out.vehicles[vehicle_idx].prev = prev_rendered_vehicle.cur;
            store.state.renderer_out.vehicles[vehicle_idx].cur = PixelState {
                rgb: rgb_color,
                idx: pix_idx,
            };
            store.state.renderer_out.vehicles[vehicle_idx].last_updated_instant_ms = now_instant_ms;
        } else {
            // Restore previous state
            store.state.renderer_out.vehicles[vehicle_idx] = prev_rendered_vehicle;
        }

        num_vehicles_visible += 1;
        if delayed_seconds.is_some() {
            num_vehicles_visible_real_time += 1;
        }
    }

    // Collect stats
    let available_vehicle_lines: Vec<AvailableVehicleLine> = lines
        .iter()
        .enumerate()
        .map(|(idx, line)| AvailableVehicleLine {
            line_name: line.name.clone(),
            vehicle_count: vehicles
                .iter()
                .filter(|veh| (veh.line_id_x_trip_id >> 16) as usize == idx)
                .count() as u32,
        })
        .collect();

    // Store stats
    if store.stats.telemetry_pending {
        store.stats.telemetry_pending = false;
        // Notify telemetry on first render or when requested
        ws_client::send_telemetry();
    }

    if store
        .state
        .rendered_state
        .rendered_any_first_at_instant_ms
        .is_none()
    {
        store.state.rendered_state.rendered_any_first_at_instant_ms = Some(now_instant_ms);
    }

    if store
        .state
        .rendered_state
        .rendered_data_first_at_instant_ms
        .is_none()
    {
        store.state.rendered_state.rendered_data_first_at_instant_ms = Some(now_instant_ms);
    }

    store.stats.num_vehicles_available = num_vehicles_available;
    store.stats.num_vehicles_visible = num_vehicles_visible;
    store.stats.num_vehicles_visible_real_time = num_vehicles_visible_real_time;
    store.stats.available_vehicle_lines = available_vehicle_lines;
}

async fn render_disruptions() {
    let mut store_guard = transit_data::get_mut().await;
    let store = match store_guard.as_mut() {
        Some(guard) => guard,
        None => return, // No data
    };

    let disruptions = &store.data.disruptions;
    let lines = &store.data.lines;

    let now_instant_ms = Instant::now().as_millis() as u32;
    let config = app_settings::persist::get_settings().await.config;

    let mut num_disruptions_available: u32 = 0;
    let mut num_disruptions_visible: u32 = 0;

    // Render disruptions
    for (disruption_idx, disruption) in disruptions.iter().enumerate() {
        let prev_disruption = store.state.renderer_out.disruptions[disruption_idx].clone();
        store.state.renderer_out.disruptions[disruption_idx].prev_rgb =
            store.state.renderer_out.disruptions[disruption_idx].cur_rgb;
        store.state.renderer_out.disruptions[disruption_idx].cur_rgb = BLACK;
        store.state.renderer_out.disruptions[disruption_idx].last_updated_instant_ms =
            now_instant_ms;

        num_disruptions_available += 1;

        let line_id = disruption.line_id_x_disruption_id >> 16;

        // Lookup line
        let line = match lines.get(line_id as usize) {
            Some(l) => l,
            None => {
                trace::err!(
                    "Disruption {} references invalid line ID {} (malformed)",
                    disruption_idx,
                    line_id
                );
                continue;
            }
        };

        // Lookup line config
        let line_config = match config
            .line_configs
            .iter()
            .find(|over| over.line_name == line.name)
        {
            Some(lc) => lc,
            _ => {
                trace::err!(
                    "Disruption {} line ID {} name '{}' has no line config",
                    disruption_idx,
                    line_id,
                    line.name
                );
                continue;
            }
        };

        // Get line enabled
        let line_enabled = if line_config.has_override {
            line_config.enabled
        } else {
            true
        };
        if !line_enabled {
            continue; // Don't render disruption of disabled line
        }

        // Determine color
        let color = rgb8_from_packed(config.disruption_color_rgb8);

        // Compute final RGB color
        let rgb_color = rgb8_brightness(
            adjust_color_temp(
                equalize_color_intensity(color),
                config.color_temperature_shift,
            ),
            config.disruption_brightness_percent as f32 / 100.0,
        );

        let from_stop_id = (disruption.from_stop_id_x_to_stop_id >> 16) as u16;
        let to_stop_id = (disruption.from_stop_id_x_to_stop_id & 0xFFFF) as u16;
        let direction_hint_from_stop_id =
            (disruption.direction_hint_from_stop_id_x_to_stop_id >> 16) as u16;
        let direction_hint_to_stop_id =
            (disruption.direction_hint_from_stop_id_x_to_stop_id & 0xFFFF) as u16;
        let is_bidirectional =
            (disruption.bidirectional_x_entire_line_x_affects_all_lines_x_stop_count & 0x80000000)
                != 0;
        let affects_all_lines =
            (disruption.bidirectional_x_entire_line_x_affects_all_lines_x_stop_count & 0x20000000)
                != 0;

        // Check if disruption is visible based on disruption filter
        match DisruptionFilter::try_from(config.disruption_filter) {
            Ok(DisruptionFilter::Severe) => {
                if !affects_all_lines {
                    continue; // In severe mode, only render disruptions with no alternative lines for same route
                }
            }
            _ => {}
        }

        // Get from and to pixel locations
        let from_loc_id = match store
            .state
            .stop_id_to_loc_id_map
            .get(from_stop_id as usize)
            .and_then(|opt| opt.as_option())
        {
            Some(loc_id) => loc_id,
            _ => {
                trace::err!(
                    "Disruption {} references invalid FROM stop ID {} (malformed)",
                    disruption_idx,
                    from_stop_id
                );
                continue;
            }
        };
        let to_loc_id_opt = if let Some(to_stop_id) = NonMax::new_unchecked(to_stop_id).as_option()
        {
            match store
                .state
                .stop_id_to_loc_id_map
                .get(to_stop_id as usize)
                .and_then(|opt| opt.as_option())
            {
                Some(loc_id) => Some(loc_id),
                _ => {
                    trace::err!(
                        "Disruption {} references invalid TO stop ID {} (malformed)",
                        disruption_idx,
                        to_stop_id
                    );
                    continue;
                }
            }
        } else {
            None
        };

        const MAX_PATH_LEN: usize = 64;
        const MAX_OPEN_NODES: usize = 128;

        let mut open_heap = [Default::default(); MAX_OPEN_NODES];
        let mut g_score = [0i64; CONFIG.cfg.loc_pix_nodes.len()];
        let mut came_from = [Default::default(); CONFIG.cfg.loc_pix_nodes.len()];
        let mut closed_set = [Default::default(); CONFIG.cfg.loc_pix_nodes.len()];
        let mut path_buf = [Default::default(); MAX_PATH_LEN];

        if let Some(to_loc_id) = to_loc_id_opt {
            // 1. Disruption between two points: Find valid path between disruption from and to stops
            let result = path_find::find_shortest_path_between_pixel_locations(
                from_loc_id,
                to_loc_id,
                &mut open_heap,
                &mut g_score,
                &mut came_from,
                &mut closed_set,
                &mut path_buf,
            );
            let path_fw = match result {
                Ok(path) => path,
                Err(e) => {
                    trace::err!(
                        "Disruption {} from loc ID {} at ({}, {}) to loc ID {} at ({}, {}): no path found ({})",
                        disruption_idx,
                        from_loc_id,
                        CONFIG.cfg.loc_pix_nodes[from_loc_id as usize].lat_e7,
                        CONFIG.cfg.loc_pix_nodes[from_loc_id as usize].lng_e7,
                        to_loc_id,
                        CONFIG.cfg.loc_pix_nodes[to_loc_id as usize].lat_e7,
                        CONFIG.cfg.loc_pix_nodes[to_loc_id as usize].lng_e7,
                        e
                    );
                    continue;
                }
            };
            let pixels_fw: Vec<u16> = path_fw.iter().map(|edge| edge.from_pix).collect();

            // If bidirectional, also find reverse path for bidirectional disruptions
            let pixels_rev: Vec<u16> = if is_bidirectional {
                let result_rev = path_find::find_shortest_path_between_pixel_locations(
                    to_loc_id,
                    from_loc_id,
                    &mut open_heap,
                    &mut g_score,
                    &mut came_from,
                    &mut closed_set,
                    &mut path_buf,
                );
                let path_rev = match result_rev {
                    Ok(path) => path,
                    Err(e) => {
                        trace::err!(
                            "Disruption {} from loc ID {} at ({}, {}) to loc ID {} at ({}, {}): no reverse path found ({})",
                            disruption_idx,
                            from_loc_id,
                            CONFIG.cfg.loc_pix_nodes[from_loc_id as usize].lat_e7,
                            CONFIG.cfg.loc_pix_nodes[from_loc_id as usize].lng_e7,
                            to_loc_id,
                            CONFIG.cfg.loc_pix_nodes[to_loc_id as usize].lat_e7,
                            CONFIG.cfg.loc_pix_nodes[to_loc_id as usize].lng_e7,
                            e
                        );
                        continue;
                    }
                };
                path_rev.iter().map(|edge| edge.from_pix).collect()
            } else {
                Vec::new()
            };

            // Join pixles
            let mut pixels = pixels_fw;
            for pix in pixels_rev.into_iter() {
                if !pixels.contains(&pix) {
                    pixels.push(pix);
                }
            }

            // Check rendered disruption has changed (color or pixels)
            if pixels != prev_disruption.pixels || rgb_color != prev_disruption.cur_rgb {
                store.state.renderer_out.disruptions[disruption_idx].prev_rgb =
                    prev_disruption.cur_rgb;
                store.state.renderer_out.disruptions[disruption_idx].cur_rgb = rgb_color;
                store.state.renderer_out.disruptions[disruption_idx].pixels = pixels;
                store.state.renderer_out.disruptions[disruption_idx].last_updated_instant_ms =
                    now_instant_ms;
            } else {
                // Restore previous state
                store.state.renderer_out.disruptions[disruption_idx] = prev_disruption;
            }

            num_disruptions_visible += 1;
        } else {
            // 2. Directed disruption at one point: Find valid path between disruption stop and direction stop
            let dir_hint_to_opt = NonMax::new_unchecked(direction_hint_to_stop_id).as_option();
            let dir_hint_from_opt = NonMax::new_unchecked(direction_hint_from_stop_id).as_option();

            let dir_hint_stop_id = if let Some(dir_hint_to_stop_id) = dir_hint_to_opt {
                dir_hint_to_stop_id
            } else if let Some(dir_hint_from_stop_id) = dir_hint_from_opt {
                dir_hint_from_stop_id
            } else {
                trace::err!(
                    "Disruption {} has no valid direction hint stop ID (malformed)",
                    disruption_idx
                );
                continue;
            };

            let dir_loc_id = match store
                .state
                .stop_id_to_loc_id_map
                .get(dir_hint_stop_id as usize)
                .and_then(|opt| opt.as_option())
            {
                Some(loc_id) => loc_id,
                _ => {
                    trace::err!(
                        "Disruption {} references invalid direction stop ID {} (malformed)",
                        disruption_idx,
                        dir_hint_stop_id
                    );
                    continue;
                }
            };

            let path_from_loc_id = if dir_hint_to_opt.is_some() {
                from_loc_id
            } else {
                dir_loc_id
            };

            let path_to_loc_id = if dir_hint_to_opt.is_some() {
                dir_loc_id
            } else {
                from_loc_id
            };

            let result = path_find::find_shortest_path_between_pixel_locations(
                path_from_loc_id,
                path_to_loc_id,
                &mut open_heap,
                &mut g_score,
                &mut came_from,
                &mut closed_set,
                &mut path_buf,
            );
            let pixel_cur = match result {
                Ok(path) => {
                    // Path found, take first pixel
                    path[0].from_pix
                }
                Err(e) => {
                    trace::err!(
                        "Disruption {} from loc ID {} at ({}, {}) to direction loc ID {} at ({}, {}): no path found ({})",
                        disruption_idx,
                        path_from_loc_id,
                        CONFIG.cfg.loc_pix_nodes[path_from_loc_id as usize].lat_e7,
                        CONFIG.cfg.loc_pix_nodes[path_from_loc_id as usize].lng_e7,
                        path_to_loc_id,
                        CONFIG.cfg.loc_pix_nodes[path_to_loc_id as usize].lat_e7,
                        CONFIG.cfg.loc_pix_nodes[path_to_loc_id as usize].lng_e7,
                        e
                    );
                    continue;
                }
            };
            let pixels = vec![pixel_cur];

            // Check rendered disruption has changed (color or pixels)
            if pixels != prev_disruption.pixels || rgb_color != prev_disruption.cur_rgb {
                store.state.renderer_out.disruptions[disruption_idx].prev_rgb =
                    prev_disruption.cur_rgb;
                store.state.renderer_out.disruptions[disruption_idx].cur_rgb = rgb_color;
                store.state.renderer_out.disruptions[disruption_idx].pixels = pixels;
                store.state.renderer_out.disruptions[disruption_idx].last_updated_instant_ms =
                    now_instant_ms;
            } else {
                // Restore previous state
                store.state.renderer_out.disruptions[disruption_idx] = prev_disruption;
            }

            num_disruptions_visible += 1;
        }
    }

    // Collect stats
    let available_disrupted_lines: Vec<AvailableDisruptedLine> = disruptions
        .iter()
        .filter_map(|disruption| {
            let line_id = disruption.line_id_x_disruption_id >> 16;
            let line = lines.get(line_id as usize)?;
            let bidirectional = (disruption
                .bidirectional_x_entire_line_x_affects_all_lines_x_stop_count
                & 0x80000000)
                != 0;
            let entire_line = (disruption
                .bidirectional_x_entire_line_x_affects_all_lines_x_stop_count
                & 0x40000000)
                != 0;
            let affects_all_lines = (disruption
                .bidirectional_x_entire_line_x_affects_all_lines_x_stop_count
                & 0x20000000)
                != 0;
            let stop_count = disruption
                .bidirectional_x_entire_line_x_affects_all_lines_x_stop_count
                & 0x1FFFFFFF;
            let from_stop_id = (disruption.from_stop_id_x_to_stop_id >> 16) as u16;
            let to_stop_id = (disruption.from_stop_id_x_to_stop_id & 0xFFFF) as u16;
            let direction_hint_from_stop_id =
                (disruption.direction_hint_from_stop_id_x_to_stop_id >> 16) as u16;
            let direction_hint_to_stop_id =
                (disruption.direction_hint_from_stop_id_x_to_stop_id & 0xFFFF) as u16;

            fn get_coord_from_stop_id(
                store: &TransitDataStore,
                stop_id: u16,
            ) -> Option<Coordinates> {
                let loc_id = store
                    .state
                    .stop_id_to_loc_id_map
                    .get(stop_id as usize)
                    .and_then(|opt| opt.as_option())?;
                let loc_node = CONFIG.cfg.loc_pix_nodes.get(loc_id as usize)?;
                Some(Coordinates {
                    latitude_e7: loc_node.lat_e7,
                    longitude_e7: loc_node.lng_e7,
                })
            }

            let from_coord = get_coord_from_stop_id(store, from_stop_id)?;
            let to_coord: Option<Coordinates> =
                if let Some(to_stop_id) = NonMax::new_unchecked(to_stop_id).as_option() {
                    get_coord_from_stop_id(store, to_stop_id)
                } else {
                    None
                };
            let direction_hint_from_coord: Option<Coordinates> =
                if let Some(direction_hint_from_stop_id) =
                    NonMax::new_unchecked(direction_hint_from_stop_id).as_option()
                {
                    get_coord_from_stop_id(store, direction_hint_from_stop_id)
                } else {
                    None
                };
            let direction_hint_to_coord: Option<Coordinates> =
                if let Some(direction_hint_to_stop_id) =
                    NonMax::new_unchecked(direction_hint_to_stop_id).as_option()
                {
                    get_coord_from_stop_id(store, direction_hint_to_stop_id)
                } else {
                    None
                };

            Some(AvailableDisruptedLine {
                line_name: line.name.clone(),
                stop_count,
                bidirectional,
                entire_line,
                from_coord,
                to_coord,
                direction_hint_from_coord,
                direction_hint_to_coord,
                affects_all_lines,
            })
        })
        .collect();

    store.stats.num_disruptions_available = num_disruptions_available;
    store.stats.num_disruptions_visible = num_disruptions_visible;
    store.state.rendered_state.disruptions_render_pending = false;
    store.stats.available_disrupted_lines = available_disrupted_lines;
}

fn rgb2hsv(rgb: RGB8) -> Hsv {
    let r = rgb.r as i16;
    let g = rgb.g as i16;
    let b = rgb.b as i16;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    // Value
    let val = max as u8;

    // Saturation
    let sat = if max == 0 {
        0
    } else {
        ((delta * 255) / max) as u8
    };

    // Hue
    let hue = if delta == 0 {
        0
    } else {
        let sector: i16;
        let offset: i16;

        if max == r {
            sector = 0;
            offset = (g - b) * 43 / delta;
        } else if max == g {
            sector = 2;
            offset = (b - r) * 43 / delta;
        } else {
            sector = 4;
            offset = (r - g) * 43 / delta;
        }

        let mut h = sector * 43 + offset;
        if h < 0 {
            h += 256;
        }
        (h & 0xFF) as u8
    };

    Hsv { hue, sat, val }
}

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

fn adjust_color_temp(color: RGB8, temp_shift: i32) -> RGB8 {
    let mut r = color.r as i32;
    let mut g = color.g as i32;
    let mut b = color.b as i32;

    r = (r + temp_shift).clamp(0, 255);
    g = (g + temp_shift / 2).clamp(0, 255);
    b = (b - temp_shift).clamp(0, 255);

    RGB8 {
        r: r as u8,
        g: g as u8,
        b: b as u8,
    }
}

fn approx_dist_sq_meters_e7(
    lat1_e7: i32,
    lng1_e7: i32,
    lat2_e7: i32,
    lng2_e7: i32,
    cos_lat_q15: i16, // from node (mean or start)
) -> i64 {
    let dlat = (lat2_e7 - lat1_e7) as i64;
    let dlng = (lng2_e7 - lng1_e7) as i64;

    // meters per E7 degree in Q16
    const E7_TO_M_Q16: i64 = 729; // ≈0.011132 m * 2^16

    let dy = (dlat * E7_TO_M_Q16) >> 16;
    let dx = (dlng * E7_TO_M_Q16 * cos_lat_q15 as i64) >> (16 + 15);

    dx * dx + dy * dy
}
