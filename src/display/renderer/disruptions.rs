// Disruptions: the pixels of the affected track section (or the direction at
// a stop), and their color
use alloc::{vec, vec::Vec};
use embassy_time::Instant;
use rgb::RGB8;
use smart_leds::colors::BLACK;

use super::{color::finish_color, is_line_enabled, line_config, stop_location};
use crate::{
    config::CONFIG,
    display::path_find::PathFinder,
    net::ws_client::client_proto::{
        DeviceConfig, Disruption, DisruptionFilter, DisruptionType, Line,
    },
    store::{app_settings, transit_data},
    trace,
    util::{NonMax, rgb8_from_packed},
};

const MAX_PATH_LEN: usize = 64;
const MAX_OPEN_NODES: usize = 128;

type DisruptionPathFinder = PathFinder<MAX_OPEN_NODES, MAX_PATH_LEN>;

pub(super) async fn render_disruptions() {
    let mut store_guard = transit_data::get_mut().await;
    let Some(store) = store_guard.as_mut() else {
        return;
    };

    let now_instant_ms = Instant::now().as_millis() as u32;
    let config = app_settings::persist::get_settings().await.config;
    let data = &store.data;
    let stop_locations = &store.state.stop_id_to_loc_id_map;
    let rendered_disruptions = &mut store.state.renderer_out.disruptions;
    let mut path_finder = DisruptionPathFinder::new();

    let mut num_disruptions_available: u32 = 0;
    let mut num_disruptions_visible: u32 = 0;

    for (disruption_idx, disruption) in data.disruptions.iter().enumerate() {
        // Turned off, unless rendered below
        let prev_rendered = rendered_disruptions[disruption_idx].clone();
        let rendered = &mut rendered_disruptions[disruption_idx];
        rendered.prev_rgb = rendered.cur_rgb;
        rendered.cur_rgb = BLACK;
        rendered.last_updated_instant_ms = now_instant_ms;
        num_disruptions_available += 1;

        let Some((pixels, rgb)) = render_disruption(
            disruption_idx,
            disruption,
            &data.lines,
            stop_locations,
            &config,
            &mut path_finder,
        ) else {
            continue;
        };

        if pixels != prev_rendered.pixels || rgb != prev_rendered.cur_rgb {
            rendered.prev_rgb = prev_rendered.cur_rgb;
            rendered.cur_rgb = rgb;
            rendered.pixels = pixels;
            rendered.last_updated_instant_ms = now_instant_ms;
        } else {
            *rendered = prev_rendered;
        }
        num_disruptions_visible += 1;
    }

    store.stats.num_disruptions_available = num_disruptions_available;
    store.stats.num_disruptions_visible = num_disruptions_visible;
    store.state.rendered_state.disruptions_render_pending = false;
}

/// The disruption's pixels and color. None if it isn't shown (filtered,
/// disabled line) or its data is malformed.
fn render_disruption(
    disruption_idx: usize,
    disruption: &Disruption,
    lines: &[Line],
    stop_locations: &[NonMax<u16>],
    config: &DeviceConfig,
    path_finder: &mut DisruptionPathFinder,
) -> Option<(Vec<u16>, RGB8)> {
    let line_id = disruption.line_id_x_disruption_id >> 16;
    let Some(line) = lines.get(line_id as usize) else {
        trace::err!(
            "Disruption {} references invalid line ID {} (malformed)",
            disruption_idx,
            line_id
        );
        return None;
    };
    let Some(line_config) = line_config(config, line) else {
        trace::err!(
            "Disruption {} line ID {} name '{}' has no line config",
            disruption_idx,
            line_id,
            line.name
        );
        return None;
    };
    if !is_line_enabled(line_config) {
        return None;
    }

    let is_suspended = disruption.r#type == DisruptionType::Suspended as i32;
    let color = rgb8_from_packed(if is_suspended {
        config.disruption_primary_color_rgb8
    } else {
        config.disruption_secondary_color_rgb8
    });
    let rgb = finish_color(
        color,
        config.color_temperature_shift,
        config.disruption_brightness_percent as f32 / 100.0,
    );

    let from_stop_id = (disruption.from_stop_id_x_to_stop_id >> 16) as u16;
    let to_stop_id = (disruption.from_stop_id_x_to_stop_id & 0xFFFF) as u16;
    let direction_hint_from_stop_id =
        (disruption.direction_hint_from_stop_id_x_to_stop_id >> 16) as u16;
    let direction_hint_to_stop_id =
        (disruption.direction_hint_from_stop_id_x_to_stop_id & 0xFFFF) as u16;
    let packed =
        disruption.bidirectional_x_entire_line_x_affects_all_lines_x_via_stop_id_x_stop_count;
    let is_bidirectional = (packed & 0x80000000) != 0;
    let affects_all_lines = (packed & 0x20000000) != 0;
    let via_stop_id = ((packed >> 13) & 0xFFFF) as u16;

    if !is_shown_by_filter(
        config.disruption_filter,
        disruption.r#type,
        affects_all_lines,
    ) {
        return None;
    }

    let Some(from_loc_id) = stop_location(stop_locations, from_stop_id as usize) else {
        trace::err!(
            "Disruption {} references invalid FROM stop ID {} (malformed)",
            disruption_idx,
            from_stop_id
        );
        return None;
    };
    let to_loc_id = optional_stop_location(disruption_idx, stop_locations, to_stop_id, "TO")?;
    let via_loc_id = optional_stop_location(disruption_idx, stop_locations, via_stop_id, "VIA")?;

    let pixels = match to_loc_id {
        // Between two stops
        Some(to_loc_id) => section_pixels(
            disruption_idx,
            path_finder,
            from_loc_id,
            via_loc_id,
            to_loc_id,
            is_bidirectional,
        )?,
        // At one stop, in a direction
        None => vec![direction_pixel(
            disruption_idx,
            path_finder,
            stop_locations,
            from_loc_id,
            direction_hint_from_stop_id,
            direction_hint_to_stop_id,
        )?],
    };
    Some((pixels, rgb))
}

fn is_shown_by_filter(filter: i32, disruption_type: i32, affects_all_lines: bool) -> bool {
    let is_suspended = disruption_type == DisruptionType::Suspended as i32;
    match DisruptionFilter::try_from(filter) {
        // Severe: no alternative (all lines affected), and more than minor delays
        Ok(DisruptionFilter::Severe) => {
            affects_all_lines && disruption_type != DisruptionType::MinorDelays as i32
        }
        Ok(DisruptionFilter::NoServiceSevere) => affects_all_lines && is_suspended,
        Ok(DisruptionFilter::NoServiceAll) => is_suspended,
        _ => true,
    }
}

/// The location of an optional stop: Some(None) if the stop isn't given, None
/// if it has no location (malformed).
fn optional_stop_location(
    disruption_idx: usize,
    stop_locations: &[NonMax<u16>],
    stop_id: u16,
    stop_kind: &str,
) -> Option<Option<u16>> {
    let Some(stop_id) = NonMax::new_unchecked(stop_id).as_option() else {
        return Some(None);
    };
    let location = stop_location(stop_locations, stop_id as usize);
    if location.is_none() {
        trace::err!(
            "Disruption {} references invalid {} stop ID {} (malformed)",
            disruption_idx,
            stop_kind,
            stop_id
        );
        return None;
    }
    Some(location)
}

fn loc_coord(loc_id: u16) -> (i32, i32) {
    let loc = &CONFIG.cfg.loc_pix_nodes[loc_id as usize];
    (loc.lat_e7, loc.lng_e7)
}

// The pixels of the track section, in both directions if bidirectional
fn section_pixels(
    disruption_idx: usize,
    path_finder: &mut DisruptionPathFinder,
    from_loc_id: u16,
    via_loc_id: Option<u16>,
    to_loc_id: u16,
    is_bidirectional: bool,
) -> Option<Vec<u16>> {
    let (from_coord, to_coord) = (loc_coord(from_loc_id), loc_coord(to_loc_id));
    let mut pixels: Vec<u16> = match path_finder.find(from_loc_id, via_loc_id, to_loc_id) {
        Ok(path) => path.iter().map(|edge| edge.from_pix).collect(),
        Err(e) => {
            trace::err!(
                "Disruption {} from loc ID {} at ({}, {}) to loc ID {} at ({}, {}): no path found ({})",
                disruption_idx,
                from_loc_id,
                from_coord.0,
                from_coord.1,
                to_loc_id,
                to_coord.0,
                to_coord.1,
                e
            );
            return None;
        }
    };
    if !is_bidirectional {
        return Some(pixels);
    }

    match path_finder.find(to_loc_id, via_loc_id, from_loc_id) {
        Ok(path) => {
            for edge in path {
                if !pixels.contains(&edge.from_pix) {
                    pixels.push(edge.from_pix);
                }
            }
            Some(pixels)
        }
        Err(e) => {
            trace::err!(
                "Disruption {} from loc ID {} at ({}, {}) to loc ID {} at ({}, {}): no reverse path found ({})",
                disruption_idx,
                from_loc_id,
                from_coord.0,
                from_coord.1,
                to_loc_id,
                to_coord.0,
                to_coord.1,
                e
            );
            None
        }
    }
}

// The pixel at the stop, toward the direction hint stop (or from it)
fn direction_pixel(
    disruption_idx: usize,
    path_finder: &mut DisruptionPathFinder,
    stop_locations: &[NonMax<u16>],
    from_loc_id: u16,
    direction_hint_from_stop_id: u16,
    direction_hint_to_stop_id: u16,
) -> Option<u16> {
    let hint_to = NonMax::new_unchecked(direction_hint_to_stop_id).as_option();
    let hint_from = NonMax::new_unchecked(direction_hint_from_stop_id).as_option();
    let Some(hint_stop_id) = hint_to.or(hint_from) else {
        trace::err!(
            "Disruption {} has no valid direction hint stop ID (malformed)",
            disruption_idx
        );
        return None;
    };
    let Some(hint_loc_id) = stop_location(stop_locations, hint_stop_id as usize) else {
        trace::err!(
            "Disruption {} references invalid direction stop ID {} (malformed)",
            disruption_idx,
            hint_stop_id
        );
        return None;
    };

    let (path_from_loc_id, path_to_loc_id) = if hint_to.is_some() {
        (from_loc_id, hint_loc_id)
    } else {
        (hint_loc_id, from_loc_id)
    };
    match path_finder.find(path_from_loc_id, None, path_to_loc_id) {
        Ok(path) => Some(path[0].from_pix),
        Err(e) => {
            let (from_coord, to_coord) = (loc_coord(path_from_loc_id), loc_coord(path_to_loc_id));
            trace::err!(
                "Disruption {} from loc ID {} at ({}, {}) to direction loc ID {} at ({}, {}): no path found ({})",
                disruption_idx,
                path_from_loc_id,
                from_coord.0,
                from_coord.1,
                path_to_loc_id,
                to_coord.0,
                to_coord.1,
                e
            );
            None
        }
    }
}
