use alloc::{string::String, vec, vec::Vec};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    mutex::{Mutex, MutexGuard},
};
use embassy_time::Instant;

use crate::{
    config::CONFIG,
    display::renderer::{self, RenderedDisruption, RenderedVehicle, RendererOutput, RendererState},
    net::ws_client::{
        self,
        client_proto::{AvailableDisruptedLine, AvailableVehicleLine, LineConfig, TransitData},
    },
    store::app_settings,
    time, trace,
    util::NonMax,
};

static TRANSIT_DATA_STORE: Mutex<CriticalSectionRawMutex, Option<TransitDataStore>> =
    Mutex::new(None);

pub struct TransitDataStore {
    pub data: TransitData,
    pub state: TransitDataState,
    pub stats: TransitDataStats,
}

#[derive(Clone)]
pub struct TransitDataState {
    pub stop_id_to_loc_id_map: Vec<NonMax<u16>>, // Precomputed map of stop indices to pixel locations based on closest stop coordinates
    pub rendered_state: RendererState,
    pub renderer_out: RendererOutput,
}

impl Default for TransitDataState {
    fn default() -> Self {
        TransitDataState {
            stop_id_to_loc_id_map: vec![],
            rendered_state: RendererState {
                rendered_data_first_at_instant_ms: None,
                rendered_any_first_at_instant_ms: None,
                drawn_first_frame_at_instant_ms: None,
                config_last_changed_at_instant_ms: 0,
                disruptions_render_pending: false,
            },
            renderer_out: RendererOutput {
                vehicles: vec![],
                disruptions: vec![],
            },
        }
    }
}

#[derive(Clone, Default)]
pub struct TransitDataStats {
    pub telemetry_pending: bool,
    pub received_at_timestamp: u32, // When data was received from server
    pub sourced_at_timestamp: u32,  // When data was sourced by server from feed
    pub simulated_until_timestamp: u32,
    pub transit_data_downlink_bytes_per_second: u32,
    pub num_vehicles_available: u32,
    pub num_vehicles_visible: u32,
    pub num_vehicles_visible_real_time: u32,
    pub num_disruptions_available: u32,
    pub num_disruptions_visible: u32,
    pub available_vehicle_lines: Vec<AvailableVehicleLine>,
    pub available_disrupted_lines: Vec<AvailableDisruptedLine>,
    pub num_pixels_on: u32,
    pub feed_source: String,
}

pub async fn update_line_configs() {
    let store = TRANSIT_DATA_STORE.lock().await;
    let transit_data = match &*store {
        Some(data_store) => &data_store.data,
        None => {
            return;
        }
    };
    let lines = &transit_data.lines;
    let config = app_settings::persist::get_settings().await.config;
    let mut config_changed = false;

    // Check line configs against transit data lines
    for line in lines.iter() {
        if let Some(line_config) = config
            .line_configs
            .iter()
            .find(|over| over.line_name == line.name)
        {
            if line_config.original_color_rgb8 != line.color_rgb8 {
                // Update original color in line config to match original line color
                app_settings::persist::update_settings(|set| {
                    if let Some(lc) = set
                        .config
                        .line_configs
                        .iter_mut()
                        .find(|over| over.line_name == line.name)
                    {
                        lc.original_color_rgb8 = line.color_rgb8;
                        if !lc.has_override {
                            lc.override_color_rgb8 = line.color_rgb8;
                        }
                    }
                })
                .await;
                config_changed = true;
            }
        } else {
            // Line config does not exist -> create non-override line config
            app_settings::persist::update_settings(|set| {
                set.config.line_configs.push(LineConfig {
                    line_name: line.name.clone(),
                    enabled: true,
                    has_override: false,
                    brightness_percent: 100,
                    original_color_rgb8: line.color_rgb8,
                    override_color_rgb8: line.color_rgb8,
                });
            })
            .await;
            config_changed = true;
        }
    }

    if config_changed {
        ws_client::send_config();
    }
}

pub async fn on_data(transit_data: TransitData, transit_data_proto_size_bytes: usize) {
    let sourced_at_timestamp = transit_data.sourced_at_unix_timestamp;
    let feed_source = transit_data.feed_source.clone();
    let simulated_until_timestamp = transit_data.simulated_until_unix_timestamp;
    let now_timestamp = time::get_unix_timestamp_seconds().await;

    // Calculate data throughput (B/s) since last update
    let prev_updated_timestamp = get_stats().await.received_at_timestamp;
    let transit_data_downlink_bytes_per_second = if prev_updated_timestamp != 0 {
        let time_diff_seconds = now_timestamp.saturating_sub(prev_updated_timestamp);
        (transit_data_proto_size_bytes as u32)
            .checked_div(time_diff_seconds)
            .unwrap_or(0)
    } else {
        0
    };

    // Precompute stop ID (station) to pixel location ID map
    let stops = &transit_data.stops;
    let mut stop_id_to_loc_id_map: Vec<NonMax<u16>> = vec![NonMax::NONE; stops.len()];
    const MAX_STOP_TO_LOC_DIST_DEG_SQ_E7: i64 = 4_000_000_000; // < 1km

    for (stop_idx, stop) in stops.iter().enumerate() {
        let is_station = (stop.is_station_x_latitude_e7 as u32 & 0x80000000) != 0;
        let latitude_e7 = stop.is_station_x_latitude_e7 & 0x7FFFFFFF;
        let longitude_e7 = stop.longitude_e7;

        if is_station {
            let loc_id = match lookup_nearest_pixel_location_id_to_coord(latitude_e7, longitude_e7)
            {
                Some(id) => id,
                None => {
                    trace::err!(
                        "Stop ID {} at ({}, {}) has no nearby pixel location",
                        stop_idx,
                        latitude_e7,
                        longitude_e7
                    );
                    continue;
                }
            };
            let loc = &CONFIG.cfg.loc_pix_nodes[loc_id as usize];
            let dist_sq = {
                let dlat = latitude_e7 as i64 - loc.lat_e7 as i64;
                let dlng = longitude_e7 as i64 - loc.lng_e7 as i64;
                dlat * dlat + dlng * dlng
            };
            if dist_sq > MAX_STOP_TO_LOC_DIST_DEG_SQ_E7 {
                trace::err!(
                    "Stop ID {} at ({}, {}) too far from nearest Loc ID {} at ({}, {})",
                    stop_idx,
                    latitude_e7,
                    longitude_e7,
                    loc_id,
                    loc.lat_e7,
                    loc.lng_e7
                );
                continue;
            }
            stop_id_to_loc_id_map[stop_idx] = match NonMax::new(loc_id) {
                Some(nm) => nm,
                None => {
                    trace::err!(
                        "Loc ID {} for stop ID {} at ({}, {}) is invalid",
                        loc_id,
                        stop_idx,
                        latitude_e7,
                        longitude_e7
                    );
                    continue;
                }
            };
        }
    }

    let num_vehicles = transit_data.vehicle_movements.len();
    let num_disruptions = transit_data.disruptions.len();

    let mut store = TRANSIT_DATA_STORE.lock().await;

    let prev_rendered_vehicles = store
        .as_ref()
        .map(|s| s.state.renderer_out.vehicles.clone())
        .unwrap_or_default();
    let mut rendered_vehicles: Vec<RenderedVehicle> = vec![Default::default(); num_vehicles];

    // Populate previously rendered vehicles for matching trip ids
    for (vehicle_idx, vehicle) in transit_data.vehicle_movements.iter().enumerate() {
        let trip_id = (vehicle.line_id_x_trip_id & 0xFFFF) as u16;
        if let Some(prev_vehicle) = prev_rendered_vehicles
            .iter()
            .find(|v| v.trip_id.as_option() == Some(trip_id))
        {
            rendered_vehicles[vehicle_idx] = prev_vehicle.clone();
        } else {
            rendered_vehicles[vehicle_idx] = RenderedVehicle::new(trip_id);
        }
    }

    let prev_rendered_disruptions = store
        .as_ref()
        .map(|s| s.state.renderer_out.disruptions.clone())
        .unwrap_or_default();
    let mut rendered_disruptions: Vec<RenderedDisruption> =
        vec![Default::default(); num_disruptions];

    // Populate previously rendered disruptions for matching disruption ids
    for (disruption_idx, disruption) in transit_data.disruptions.iter().enumerate() {
        let disruption_id = (disruption.line_id_x_disruption_id & 0xFFFF) as u16;
        if let Some(prev_disruption) = prev_rendered_disruptions
            .iter()
            .find(|d| d.disruption_id.as_option() == Some(disruption_id))
        {
            rendered_disruptions[disruption_idx] = prev_disruption.clone();
        } else {
            rendered_disruptions[disruption_idx] = RenderedDisruption::new(disruption_id);
        }
    }

    // Store transit data
    *store = Some(TransitDataStore {
        data: transit_data,
        state: TransitDataState {
            stop_id_to_loc_id_map,
            renderer_out: RendererOutput {
                vehicles: rendered_vehicles,
                disruptions: rendered_disruptions,
            },
            rendered_state: RendererState {
                rendered_data_first_at_instant_ms: None,
                disruptions_render_pending: true,
                ..store
                    .as_ref()
                    .map(|s| s.state.clone())
                    .unwrap_or_default()
                    .rendered_state
            },
        },
        stats: TransitDataStats {
            telemetry_pending: true,
            received_at_timestamp: now_timestamp,
            sourced_at_timestamp,
            simulated_until_timestamp,
            transit_data_downlink_bytes_per_second,
            feed_source,
            ..store.as_ref().map(|s| s.stats.clone()).unwrap_or_default()
        },
    });
    drop(store);

    // Update local config
    update_line_configs().await;
}

pub async fn clear() {
    let mut store = TRANSIT_DATA_STORE.lock().await;
    if let Some(store_ref) = store.as_mut() {
        store_ref.state.stop_id_to_loc_id_map = vec![];
        store_ref
            .state
            .rendered_state
            .rendered_data_first_at_instant_ms = None;
        store_ref.data = TransitData::default();
    }
}

pub async fn reset() {
    let mut store = TRANSIT_DATA_STORE.lock().await;
    *store = None;
}

pub async fn is_set() -> bool {
    let store = TRANSIT_DATA_STORE.lock().await;
    store.as_ref().is_some()
}

pub async fn get_stats() -> TransitDataStats {
    let store = TRANSIT_DATA_STORE.lock().await;
    store
        .as_ref()
        .map(|store| store.stats.clone())
        .unwrap_or_default()
}

pub async fn get_mut<'a>() -> MutexGuard<'a, CriticalSectionRawMutex, Option<TransitDataStore>> {
    TRANSIT_DATA_STORE.lock().await
}

pub async fn on_config_updated() {
    let mut store = TRANSIT_DATA_STORE.lock().await;
    if let Some(store_ref) = store.as_mut() {
        store_ref.stats.telemetry_pending = true;
        store_ref.state.rendered_state.disruptions_render_pending = true;
        store_ref
            .state
            .rendered_state
            .config_last_changed_at_instant_ms = Instant::now().as_millis() as u32;
    }
    renderer::render_now();
}

fn lookup_nearest_pixel_location_id_to_coord(latitude_e7: i32, longitude_e7: i32) -> Option<u16> {
    let kd_tree = CONFIG.cfg.loc_geo_kd_tree;
    let loc_nodes = CONFIG.cfg.loc_pix_nodes;

    let mut best_dist_sq: i64 = i64::MAX;
    let mut best_loc_idx: Option<u16> = None;

    // Explicit stack: (node_index, depth)
    let mut stack: [Option<(usize, usize)>; 32] = [None; 32];
    let mut sp = 0;

    // Start at root
    stack[sp] = Some((0, 0));
    sp += 1;

    // Iterative K-D tree traversal
    while sp > 0 {
        sp -= 1;
        let (node_idx, depth) = stack[sp].unwrap();
        let kd_node = &kd_tree[node_idx];
        let loc_node = &loc_nodes[kd_node.loc as usize];

        // Distance to this node
        let dlat = latitude_e7 as i64 - loc_node.lat_e7 as i64;
        let dlng = longitude_e7 as i64 - loc_node.lng_e7 as i64;
        let dist_sq = dlat * dlat + dlng * dlng;

        // Update best if closer
        if dist_sq < best_dist_sq {
            best_dist_sq = dist_sq;
            best_loc_idx = Some(kd_node.loc);
        }

        // Decide traversal order, split by latitude (depth even) or longitude (depth odd)
        let (query_coord, node_coord) = if depth % 2 == 0 {
            (latitude_e7, loc_node.lat_e7)
        } else {
            (longitude_e7, loc_node.lng_e7)
        };

        let diff = (query_coord - node_coord) as i64;
        let diff_sq = diff * diff;

        let (near, far) = if query_coord < node_coord {
            (kd_node.left, kd_node.right)
        } else {
            (kd_node.right, kd_node.left)
        };

        // Always visit nearer side first
        if let Some(idx) = near {
            stack[sp] = Some((idx as usize, depth + 1));
            sp += 1;
        }

        // Visit far side only if it can contain a closer point
        if diff_sq < best_dist_sq
            && let Some(idx) = far
        {
            stack[sp] = Some((idx as usize, depth + 1));
            sp += 1;
        }
    }

    best_loc_idx
}
