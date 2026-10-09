// The latest transit data from the server, with the renderer's state of it
mod stops;

use alloc::{string::String, vec, vec::Vec};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    mutex::{Mutex, MutexGuard},
};
use embassy_time::Instant;

use crate::{
    display::renderer::{self, RenderedDisruption, RenderedVehicle, RendererOutput, RendererState},
    net::ws_client::{
        self,
        client_proto::{LineConfig, TransitData},
    },
    store::app_settings,
    time,
    util::NonMax,
};

static TRANSIT_DATA_STORE: Mutex<CriticalSectionRawMutex, Option<TransitDataStore>> =
    Mutex::new(None);

pub struct TransitDataStore {
    pub data: TransitData,
    pub state: TransitDataState,
    pub stats: TransitDataStats,
}

#[derive(Clone, Default)]
pub struct TransitDataState {
    pub stop_id_to_loc_id_map: Vec<NonMax<u16>>, // Precomputed map of stop indices to pixel locations based on closest stop coordinates
    pub rendered_state: RendererState,
    pub renderer_out: RendererOutput,
    pub is_data_stale: bool, // No new transit data received in a while
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
    pub num_pixels_on: u32,
    pub feed_source: String,
}

pub async fn on_data(transit_data: TransitData, transit_data_proto_size_bytes: usize) {
    let now_timestamp = time::get_unix_timestamp_seconds().await;
    let prev_received_timestamp = get_stats().await.received_at_timestamp;
    let transit_data_downlink_bytes_per_second = if prev_received_timestamp != 0 {
        let time_diff_seconds = now_timestamp.saturating_sub(prev_received_timestamp);
        (transit_data_proto_size_bytes as u32)
            .checked_div(time_diff_seconds)
            .unwrap_or(0)
    } else {
        0
    };
    let stop_id_to_loc_id_map = stops::map_stations_to_locations(&transit_data.stops);

    let mut store = TRANSIT_DATA_STORE.lock().await;
    let prev = store.as_ref();
    let renderer_out = RendererOutput {
        vehicles: carry_over_vehicles(&transit_data, prev),
        disruptions: carry_over_disruptions(&transit_data, prev),
    };
    let rendered_state = RendererState {
        rendered_data_first_at_instant_ms: None,
        disruptions_render_pending: true,
        ..prev
            .map(|s| s.state.rendered_state.clone())
            .unwrap_or_default()
    };
    let stats = TransitDataStats {
        telemetry_pending: true,
        received_at_timestamp: now_timestamp,
        sourced_at_timestamp: transit_data.sourced_at_unix_timestamp,
        simulated_until_timestamp: transit_data.simulated_until_unix_timestamp,
        transit_data_downlink_bytes_per_second,
        feed_source: transit_data.feed_source.clone(),
        ..prev.map(|s| s.stats.clone()).unwrap_or_default()
    };
    *store = Some(TransitDataStore {
        data: transit_data,
        state: TransitDataState {
            stop_id_to_loc_id_map,
            rendered_state,
            renderer_out,
            is_data_stale: false,
        },
        stats,
    });
    drop(store);

    update_line_configs().await;
}

// The vehicles keep their rendered state from the previous data, by trip ID
fn carry_over_vehicles(
    transit_data: &TransitData,
    prev: Option<&TransitDataStore>,
) -> Vec<RenderedVehicle> {
    let prev_vehicles = prev.map_or(&[][..], |s| s.state.renderer_out.vehicles.as_slice());
    transit_data
        .vehicle_movements
        .iter()
        .map(|vehicle| {
            let trip_id = (vehicle.line_id_x_trip_id & 0xFFFF) as u16;
            prev_vehicles
                .iter()
                .find(|v| v.trip_id.as_option() == Some(trip_id))
                .cloned()
                .unwrap_or_else(|| RenderedVehicle::new(trip_id))
        })
        .collect()
}

// The disruptions keep their rendered state from the previous data, by
// disruption ID
fn carry_over_disruptions(
    transit_data: &TransitData,
    prev: Option<&TransitDataStore>,
) -> Vec<RenderedDisruption> {
    let prev_disruptions = prev.map_or(&[][..], |s| s.state.renderer_out.disruptions.as_slice());
    transit_data
        .disruptions
        .iter()
        .map(|disruption| {
            let disruption_id = (disruption.line_id_x_disruption_id & 0xFFFF) as u16;
            prev_disruptions
                .iter()
                .find(|d| d.disruption_id.as_option() == Some(disruption_id))
                .cloned()
                .unwrap_or_else(|| RenderedDisruption::new(disruption_id))
        })
        .collect()
}

/// Keeps a line config for each line of the transit data, following the
/// lines' original colors.
pub async fn update_line_configs() {
    let store = TRANSIT_DATA_STORE.lock().await;
    let Some(store) = store.as_ref() else {
        return;
    };
    let config = app_settings::persist::get_settings().await.config;
    let mut config_changed = false;

    for line in store.data.lines.iter() {
        match config
            .line_configs
            .iter()
            .find(|line_config| line_config.line_name == line.name)
        {
            Some(line_config) if line_config.original_color_rgb8 != line.color_rgb8 => {
                app_settings::persist::update_settings(|set| {
                    if let Some(line_config) = set
                        .config
                        .line_configs
                        .iter_mut()
                        .find(|line_config| line_config.line_name == line.name)
                    {
                        line_config.original_color_rgb8 = line.color_rgb8;
                        if !line_config.has_override {
                            line_config.override_color_rgb8 = line.color_rgb8;
                        }
                    }
                })
                .await;
                config_changed = true;
            }
            Some(_) => {}
            None => {
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
    }

    if config_changed {
        ws_client::send_config();
    }
}

/// Frees the data, keeping what was rendered: new data then doesn't have to
/// fit into memory next to it.
pub async fn clear() {
    let mut store = TRANSIT_DATA_STORE.lock().await;
    if let Some(store) = store.as_mut() {
        store.state.stop_id_to_loc_id_map = vec![];
        store.state.rendered_state.rendered_data_first_at_instant_ms = None;
        store.data = TransitData::default();
    }
}

pub async fn on_data_stale() {
    clear().await;
    let mut store = TRANSIT_DATA_STORE.lock().await;
    if let Some(store) = store.as_mut() {
        store.state.is_data_stale = true;
        store.state.rendered_state.drawn_first_frame_at_instant_ms = None;
        store.state.renderer_out = RendererOutput::default();
    }
}

pub async fn reset() {
    *TRANSIT_DATA_STORE.lock().await = None;
}

pub async fn is_set() -> bool {
    TRANSIT_DATA_STORE.lock().await.is_some()
}

pub async fn get_stats() -> TransitDataStats {
    TRANSIT_DATA_STORE
        .lock()
        .await
        .as_ref()
        .map(|store| store.stats.clone())
        .unwrap_or_default()
}

pub async fn get_mut<'a>() -> MutexGuard<'a, CriticalSectionRawMutex, Option<TransitDataStore>> {
    TRANSIT_DATA_STORE.lock().await
}

pub async fn on_config_updated() {
    let mut store = TRANSIT_DATA_STORE.lock().await;
    if let Some(store) = store.as_mut() {
        store.stats.telemetry_pending = true;
        store.state.rendered_state.disruptions_render_pending = true;
        store.state.rendered_state.config_last_changed_at_instant_ms =
            Instant::now().as_millis() as u32;
    }
    renderer::render_now();
}
