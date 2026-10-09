// Vehicles: where each one is now along its movement, and the pixel and color
// it shows there
use core::iter;

use embassy_time::Instant;
use rgb::RGB8;
use smart_leds::hsv::Hsv;

use super::{
    PixelState,
    color::{finish_color, hsv_lerp, rgb2hsv},
    is_line_enabled, line_config, stop_location,
};
use crate::{
    config::CONFIG,
    display::path_find::{PathFindError, PathFinder},
    net::ws_client::{
        self,
        client_proto::{
            ColorMode, DeviceConfig, Line, MovementSegment, RealtimeFilter, Stop, VehicleFilter,
            VehicleMovement, ViaStop,
        },
    },
    store::{app_settings, transit_data},
    time, trace,
    util::{NonMax, lerp, rgb8_from_packed},
};

const MAX_PATH_LEN: usize = 8;
const MAX_OPEN_NODES: usize = 32;
// Without a known wait time, stationary vehicles show for this long before
// the end of the segment
const MIN_MISSING_WAIT_TIME_SECONDS: u32 = 30;

type Coord = (i32, i32); // Latitude, longitude * 10^7

// Where a vehicle is now
struct VehiclePosition<'a> {
    segment: &'a MovementSegment,
    segment_progress_secs: u32,
    from_stop_id: u32,
    to_stop_id: u32,
    from_coord: Coord,
    to_coord: Coord,
    coord: Coord,
}

// Values from the config, the same for all vehicles of a frame
struct VehicleStyle<'a> {
    config: &'a DeviceConfig,
    primary_color: RGB8,
    tertiary_color: RGB8,
    disruption_color: RGB8,
    primary_hsv: Hsv,
    secondary_hsv: Hsv,
    vehicle_filter_meters_sq: i64,
    lines_count: usize,
}

pub(super) async fn render_vehicles() {
    let mut store_guard = transit_data::get_mut().await;
    let Some(store) = store_guard.as_mut() else {
        return;
    };

    let now_timestamp_secs = time::get_unix_timestamp_seconds().await;
    let now_instant_ms = Instant::now().as_millis() as u32;
    let config = app_settings::persist::get_settings().await.config;
    let style = VehicleStyle {
        config: &config,
        primary_color: rgb8_from_packed(config.primary_color_rgb8),
        tertiary_color: rgb8_from_packed(config.tertiary_color_rgb8),
        disruption_color: rgb8_from_packed(config.disruption_primary_color_rgb8),
        primary_hsv: rgb2hsv(rgb8_from_packed(config.primary_color_rgb8)),
        secondary_hsv: rgb2hsv(rgb8_from_packed(config.secondary_color_rgb8)),
        vehicle_filter_meters_sq: (config.vehicle_distance_threshold_meters as i64).pow(2),
        lines_count: store.data.lines.len(),
    };

    let data = &store.data;
    let stop_locations = &store.state.stop_id_to_loc_id_map;
    let rendered_vehicles = &mut store.state.renderer_out.vehicles;
    let vehicles_with_rt_data_percent = real_time_percent(&data.vehicle_movements);
    let mut path_finder = PathFinder::<MAX_OPEN_NODES, MAX_PATH_LEN>::new();

    let mut num_vehicles_available: u32 = 0;
    let mut num_vehicles_visible: u32 = 0;
    let mut num_vehicles_visible_real_time: u32 = 0;

    for (vehicle_idx, vehicle) in data.vehicle_movements.iter().enumerate() {
        // Turned off, unless rendered below
        let prev_rendered = rendered_vehicles[vehicle_idx].clone();
        let rendered = &mut rendered_vehicles[vehicle_idx];
        rendered.prev = prev_rendered.cur;
        rendered.cur = PixelState::NONE;
        rendered.last_updated_instant_ms = now_instant_ms;

        let Some(position) = vehicle_position(
            vehicle_idx,
            vehicle,
            &data.stops,
            now_timestamp_secs,
            data.sourced_at_unix_timestamp,
        ) else {
            continue;
        };
        num_vehicles_available += 1;

        let Some((pixel_state, is_real_time)) = render_vehicle(
            vehicle_idx,
            vehicle,
            &position,
            &data.lines,
            stop_locations,
            &style,
            vehicles_with_rt_data_percent,
            &mut path_finder,
        ) else {
            continue;
        };

        if prev_rendered.cur != pixel_state {
            rendered.prev = prev_rendered.cur;
            rendered.cur = pixel_state;
            rendered.last_updated_instant_ms = now_instant_ms;
        } else {
            *rendered = prev_rendered;
        }

        num_vehicles_visible += 1;
        if is_real_time {
            num_vehicles_visible_real_time += 1;
        }
    }

    // On the first render, or when requested
    if store.stats.telemetry_pending {
        store.stats.telemetry_pending = false;
        ws_client::send_telemetry();
    }

    let rendered_state = &mut store.state.rendered_state;
    rendered_state
        .rendered_any_first_at_instant_ms
        .get_or_insert(now_instant_ms);
    rendered_state
        .rendered_data_first_at_instant_ms
        .get_or_insert(now_instant_ms);

    store.stats.num_vehicles_available = num_vehicles_available;
    store.stats.num_vehicles_visible = num_vehicles_visible;
    store.stats.num_vehicles_visible_real_time = num_vehicles_visible_real_time;
}

// Share of vehicles with real-time data (a delay on any segment)
fn real_time_percent(vehicles: &[VehicleMovement]) -> f32 {
    let with_real_time_data = vehicles
        .iter()
        .filter(|vehicle| {
            vehicle
                .segments
                .iter()
                .any(|segment| delayed_seconds(segment).is_some())
        })
        .count();
    with_real_time_data as f32 / vehicles.len().max(1) as f32 * 100.0
}

fn delayed_seconds(segment: &MovementSegment) -> NonMax<i16> {
    NonMax::new_unchecked((segment.delayed_seconds_x_average_speed_kmph >> 16) as i16)
}

fn move_and_wait_seconds(segment: &MovementSegment) -> (u32, u32) {
    (
        segment.move_seconds_x_wait_seconds >> 16,
        segment.move_seconds_x_wait_seconds & 0xFFFF,
    )
}

fn stop_coord(stops: &[Stop], stop_id: u32) -> Option<Coord> {
    stops.get(stop_id as usize).map(|stop| {
        (
            stop.is_station_x_latitude_e7 & 0x7FFFFFFF,
            stop.longitude_e7,
        )
    })
}

fn lerp_coord(from: Coord, to: Coord, ratio: f32) -> Coord {
    (
        lerp(from.0 as f32, to.0 as f32, ratio) as i32,
        lerp(from.1 as f32, to.1 as f32, ratio) as i32,
    )
}

/// Where the vehicle is now. None if its movement hasn't begun or has ended,
/// or its data is malformed.
fn vehicle_position<'a>(
    vehicle_idx: usize,
    vehicle: &'a VehicleMovement,
    stops: &[Stop],
    now_timestamp_secs: u32,
    source_timestamp: u32,
) -> Option<VehiclePosition<'a>> {
    let total_duration_seconds: u32 = vehicle
        .segments
        .iter()
        .map(|segment| {
            let (move_seconds, wait_seconds) = move_and_wait_seconds(segment);
            move_seconds + wait_seconds
        })
        .sum();
    let start_offset_seconds = vehicle
        .segments
        .first()
        .map(|segment| {
            // Sign extend i15 to i16
            (((segment.canceled_x_start_offset_seconds_x_via_total_move_seconds >> 16) & 0x7FFF)
                as i16)
                << 1
                >> 1
        })
        .unwrap_or(0);
    let movement_start_timestamp = (source_timestamp as i32 + start_offset_seconds as i32) as u32;
    let movement_end_timestamp = movement_start_timestamp + total_duration_seconds;
    if now_timestamp_secs < movement_start_timestamp || now_timestamp_secs >= movement_end_timestamp
    {
        return None;
    }

    let (segment, segment_progress_secs) =
        current_segment(vehicle, movement_start_timestamp, now_timestamp_secs)?;
    let from_stop_id = segment.from_stop_id_x_to_stop_id >> 16;
    let to_stop_id = segment.from_stop_id_x_to_stop_id & 0xFFFF;

    let Some(from_coord) = stop_coord(stops, from_stop_id) else {
        trace::err!(
            "Vehicle {} segment references invalid FROM stop ID {} (malformed)",
            vehicle_idx,
            from_stop_id
        );
        return None;
    };
    let Some(to_coord) = stop_coord(stops, to_stop_id) else {
        trace::err!(
            "Vehicle {} segment references invalid TO stop ID {} (malformed)",
            vehicle_idx,
            to_stop_id
        );
        return None;
    };

    let (move_seconds, _) = move_and_wait_seconds(segment);
    let coord = if segment_progress_secs >= move_seconds {
        // Waiting at the end of the segment
        to_coord
    } else if segment.via_stops.is_empty() {
        lerp_coord(
            from_coord,
            to_coord,
            segment_progress_secs as f32 / move_seconds as f32,
        )
    } else {
        coord_along_via_stops(
            vehicle_idx,
            segment,
            stops,
            segment_progress_secs,
            from_coord,
            to_coord,
            to_stop_id,
        )
    };

    Some(VehiclePosition {
        segment,
        segment_progress_secs,
        from_stop_id,
        to_stop_id,
        from_coord,
        to_coord,
        coord,
    })
}

// The segment the vehicle is on now, and the seconds since it started it
fn current_segment(
    vehicle: &VehicleMovement,
    movement_start_timestamp: u32,
    now_timestamp_secs: u32,
) -> Option<(&MovementSegment, u32)> {
    let mut segment_start_timestamp = movement_start_timestamp;
    for segment in vehicle.segments.iter() {
        let (move_seconds, wait_seconds) = move_and_wait_seconds(segment);
        let segment_end_timestamp = segment_start_timestamp + move_seconds + wait_seconds;
        if now_timestamp_secs >= segment_start_timestamp
            && now_timestamp_secs < segment_end_timestamp
        {
            return Some((segment, now_timestamp_secs - segment_start_timestamp));
        }
        segment_start_timestamp = segment_end_timestamp;
    }
    None
}

// Linear interpolation along the legs between the via stops (and to the end)
fn coord_along_via_stops(
    vehicle_idx: usize,
    segment: &MovementSegment,
    stops: &[Stop],
    segment_progress_secs: u32,
    from_coord: Coord,
    to_coord: Coord,
    to_stop_id: u32,
) -> Coord {
    let (move_seconds, _) = move_and_wait_seconds(segment);
    let total_via_move_secs =
        segment.canceled_x_start_offset_seconds_x_via_total_move_seconds & 0xFFFF;
    let last_leg = ViaStop {
        stop_id_x_move_seconds: (to_stop_id << 16) | (move_seconds - total_via_move_secs),
    };

    let mut leg_start_coord = from_coord;
    let mut leg_start_secs = 0;
    for via_stop in segment.via_stops.iter().chain(iter::once(&last_leg)) {
        let via_stop_id = via_stop.stop_id_x_move_seconds >> 16;
        let leg_move_secs = via_stop.stop_id_x_move_seconds & 0xFFFF;
        let Some(via_coord) = stop_coord(stops, via_stop_id) else {
            trace::err!(
                "Vehicle {} segment references invalid VIA stop ID {} (malformed)",
                vehicle_idx,
                via_stop_id
            );
            continue;
        };
        if segment_progress_secs < leg_start_secs + leg_move_secs {
            let ratio = (segment_progress_secs - leg_start_secs) as f32 / leg_move_secs as f32;
            return lerp_coord(leg_start_coord, via_coord, ratio);
        }
        leg_start_secs += leg_move_secs;
        leg_start_coord = via_coord;
    }
    to_coord
}

/// The vehicle's pixel and color, and whether it has real-time data. None
/// if it isn't shown (filtered, disabled line) or its data is malformed.
#[allow(clippy::too_many_arguments)]
fn render_vehicle(
    vehicle_idx: usize,
    vehicle: &VehicleMovement,
    position: &VehiclePosition,
    lines: &[Line],
    stop_locations: &[NonMax<u16>],
    style: &VehicleStyle,
    vehicles_with_rt_data_percent: f32,
    path_finder: &mut PathFinder<MAX_OPEN_NODES, MAX_PATH_LEN>,
) -> Option<(PixelState, bool)> {
    let config = style.config;
    let segment = position.segment;
    if config.vehicle_filter == VehicleFilter::StationaryOnly as i32
        && !is_stationary(segment, position.segment_progress_secs)
    {
        return None;
    }

    let line_id = vehicle.line_id_x_trip_id >> 16;
    let Some(line) = lines.get(line_id as usize) else {
        trace::err!(
            "Vehicle {} references invalid line ID {} (malformed)",
            vehicle_idx,
            line_id
        );
        return None;
    };
    let Some(line_config) = line_config(config, line) else {
        trace::err!(
            "Vehicle {} line ID {} name '{}' has no line config",
            vehicle_idx,
            line_id,
            line.name
        );
        return None;
    };
    if !is_line_enabled(line_config) {
        return None;
    }
    let (line_color, line_brightness_percent) = if line_config.has_override {
        (
            rgb8_from_packed(line_config.override_color_rgb8),
            line_config.brightness_percent.min(100),
        )
    } else {
        (rgb8_from_packed(line.color_rgb8), 100)
    };

    // In real-time only mode, vehicles without real-time data are hidden,
    // unless too few have it (the map would be all empty)
    let delayed_seconds = delayed_seconds(segment);
    if config.realtime_filter == RealtimeFilter::RealtimeOnly as i32
        && delayed_seconds.is_none()
        && vehicles_with_rt_data_percent > 50.0
    {
        return None;
    }

    let Some(from_loc_id) = stop_location(stop_locations, position.from_stop_id as usize) else {
        trace::err!(
            "Vehicle {} ({}) segment from stop ID {} at ({}, {}) has no pixel location mapping",
            vehicle_idx,
            line.name,
            position.from_stop_id,
            position.from_coord.0,
            position.from_coord.1
        );
        return None;
    };
    let Some(to_loc_id) = stop_location(stop_locations, position.to_stop_id as usize) else {
        trace::err!(
            "Vehicle {} ({}) segment to stop ID {} at ({}, {}) has no pixel location mapping",
            vehicle_idx,
            line.name,
            position.to_stop_id,
            position.to_coord.0,
            position.to_coord.1
        );
        return None;
    };

    let pixel = match vehicle_pixel(path_finder, from_loc_id, to_loc_id, position.coord, style) {
        Ok(pixel) => pixel?,
        Err(e) => {
            trace::err!(
                "Vehicle {} ({}) segment from loc ID {} at ({}, {}) to loc ID {} at ({}, {}): no path found ({})",
                vehicle_idx,
                line.name,
                from_loc_id,
                position.from_coord.0,
                position.from_coord.1,
                to_loc_id,
                position.to_coord.0,
                position.to_coord.1,
                e
            );
            return None;
        }
    };

    let color = vehicle_color(style, line_color, line_id, segment, delayed_seconds);
    let rgb = finish_color(
        color,
        config.color_temperature_shift,
        line_brightness_percent as f32 / 100.0,
    );

    let Some(idx) = NonMax::new(pixel) else {
        trace::err!(
            "Vehicle {} ({}) segment from loc ID {} at ({}, {}) to loc ID {} at ({}, {}): pixel index is invalid",
            vehicle_idx,
            line.name,
            from_loc_id,
            position.from_coord.0,
            position.from_coord.1,
            to_loc_id,
            position.to_coord.0,
            position.to_coord.1
        );
        return None;
    };
    Some((PixelState { rgb, idx }, delayed_seconds.is_some()))
}

// Waiting at a stop, or about to (when the wait time isn't known)
fn is_stationary(segment: &MovementSegment, segment_progress_secs: u32) -> bool {
    let (move_seconds, wait_seconds) = move_and_wait_seconds(segment);
    let is_moving = segment_progress_secs < move_seconds;
    let has_wait_time = wait_seconds > 0;
    if has_wait_time {
        return !is_moving;
    }
    let total_secs = move_seconds + wait_seconds;
    segment_progress_secs >= total_secs.saturating_sub(MIN_MISSING_WAIT_TIME_SECONDS)
}

/// The pixel of the path from the segment's start to its end that leaves
/// from the location closest to the vehicle. Ok(None) if none is within the
/// distance filter.
fn vehicle_pixel(
    path_finder: &mut PathFinder<MAX_OPEN_NODES, MAX_PATH_LEN>,
    from_loc_id: u16,
    to_loc_id: u16,
    coord: Coord,
    style: &VehicleStyle,
) -> Result<Option<u16>, PathFindError> {
    let path = path_finder.find(from_loc_id, None, to_loc_id)?;
    let loc_nodes = CONFIG.cfg.loc_pix_nodes;
    let within_distance_only = style.config.vehicle_filter == VehicleFilter::WithinDistance as i32;
    let closest = path
        .iter()
        .map(|edge| {
            let loc_node = &loc_nodes[edge.from_loc as usize];
            let dist_sq_meters = approx_dist_sq_meters_e7(
                coord.0,
                coord.1,
                loc_node.lat_e7,
                loc_node.lng_e7,
                CONFIG.cfg.cos_lat_q15,
            );
            (dist_sq_meters, edge.from_pix)
        })
        .filter(|(dist_sq_meters, _)| {
            !within_distance_only || *dist_sq_meters <= style.vehicle_filter_meters_sq
        })
        .min();
    Ok(closest.map(|(_, pixel)| pixel))
}

fn vehicle_color(
    style: &VehicleStyle,
    line_color: RGB8,
    line_id: u32,
    segment: &MovementSegment,
    delayed_seconds: NonMax<i16>,
) -> RGB8 {
    let config = style.config;
    match ColorMode::try_from(config.color_mode) {
        Ok(ColorMode::Original) | Err(_) => line_color,
        Ok(ColorMode::Monochrome) => style.primary_color,
        Ok(ColorMode::RangeSpacedApart) => {
            // The line's place within the primary to secondary range
            let ratio = (line_id as f32) / style.lines_count.max(1) as f32;
            hsv_lerp(style.primary_hsv, style.secondary_hsv, ratio)
        }
        Ok(ColorMode::DelayHeatmap) => {
            let Some(delay_secs) = delayed_seconds.as_option() else {
                let is_canceled =
                    (segment.canceled_x_start_offset_seconds_x_via_total_move_seconds & 0x80000000)
                        != 0;
                return if is_canceled {
                    style.disruption_color
                } else {
                    style.tertiary_color
                };
            };
            let min_delay_secs = (config.min_delay_minutes as i32).max(0) * 60;
            let max_delay_secs = (config.max_delay_minutes as i32 * 60).max(min_delay_secs + 1);
            let delay_secs = (delay_secs as i32).clamp(min_delay_secs, max_delay_secs);
            let ratio = (delay_secs - min_delay_secs) as f32
                / (max_delay_secs - min_delay_secs).max(1) as f32;
            hsv_lerp(style.primary_hsv, style.secondary_hsv, ratio)
        }
        Ok(ColorMode::SpeedHeatmap) => {
            let average_speed_kmph = segment.delayed_seconds_x_average_speed_kmph & 0xFFFF;
            let min_speed_kmph = config.min_speed_kmph as i32;
            let max_speed_kmph = (config.max_speed_kmph as i32).max(min_speed_kmph + 1);
            let speed_kmph = average_speed_kmph.clamp(min_speed_kmph, max_speed_kmph);
            let ratio = 1.0
                - (speed_kmph - min_speed_kmph) as f32
                    / (max_speed_kmph - min_speed_kmph).max(1) as f32;
            hsv_lerp(style.primary_hsv, style.secondary_hsv, ratio)
        }
    }
}

// Squared distance in meters, approximated for short distances
fn approx_dist_sq_meters_e7(
    lat1_e7: i32,
    lng1_e7: i32,
    lat2_e7: i32,
    lng2_e7: i32,
    cos_lat_q15: i16,
) -> i64 {
    // Meters per E7 degree in Q16: ≈0.011132 m * 2^16
    const E7_TO_M_Q16: i64 = 729;

    let dlat = (lat2_e7 - lat1_e7) as i64;
    let dlng = (lng2_e7 - lng1_e7) as i64;
    let dy = (dlat * E7_TO_M_Q16) >> 16;
    let dx = (dlng * E7_TO_M_Q16 * cos_lat_q15 as i64) >> (16 + 15);
    dx * dx + dy * dy
}
