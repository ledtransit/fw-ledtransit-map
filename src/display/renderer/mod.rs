// Renders the transit data into what each vehicle and disruption shows on
// the map (pixels and colors); the painter draws it
mod color;
mod disruptions;
mod vehicles;

use alloc::{vec, vec::Vec};
use defmt::warn;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, with_timeout};
use rgb::RGB8;
use smart_leds::colors::BLACK;

use crate::{
    net::ws_client::client_proto::{DeviceConfig, Line, LineConfig},
    store::{app_settings, transit_data},
    trace,
    util::NonMax,
};

pub const RENDERER_FPS: u32 = 1; // No need to render more than once per second, since vehicle departure times and delays have a granularity of 1 second

static RENDER_NOW_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[derive(Clone, Default)]
pub struct RendererState {
    pub rendered_data_first_at_instant_ms: Option<u32>,
    pub rendered_any_first_at_instant_ms: Option<u32>,
    pub drawn_first_frame_at_instant_ms: Option<u32>,
    pub config_last_changed_at_instant_ms: u32,
    pub disruptions_render_pending: bool,
}

#[derive(Clone, Default)]
pub struct RendererOutput {
    pub vehicles: Vec<RenderedVehicle>,
    pub disruptions: Vec<RenderedDisruption>,
}

#[derive(Clone)]
pub struct RenderedVehicle {
    pub prev: PixelState,
    pub cur: PixelState,
    pub last_updated_instant_ms: u32,
    pub trip_id: NonMax<u16>, // Stable unique ID, server-provided
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
    pub disruption_id: NonMax<u16>, // Stable unique ID, server-provided
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

pub fn render_now() {
    RENDER_NOW_SIGNAL.signal(());
}

#[embassy_executor::task]
async fn renderer_task() {
    let frame_duration_budget = Duration::from_millis(1000 / RENDERER_FPS as u64);
    loop {
        RENDER_NOW_SIGNAL.reset();
        let begin_frame_time = Instant::now();

        let is_set_up = app_settings::persist::get_settings().await.claimed;
        if is_set_up {
            trace::flush_errors();

            // Every frame: departure times have 1 s resolution, the painter
            // spaces the vehicles' changes out within the frame
            vehicles::render_vehicles().await;

            // Disruptions change rarely and need potentially long path finding:
            // only when the transit data or the config changed
            if is_disruptions_render_pending().await {
                disruptions::render_disruptions().await;
            }
        }

        // Until the next frame, or render_now()
        let frame_duration = Instant::now() - begin_frame_time;
        if frame_duration < frame_duration_budget {
            with_timeout(
                frame_duration_budget - frame_duration,
                RENDER_NOW_SIGNAL.wait(),
            )
            .await
            .ok();
        } else {
            warn!(
                "Renderer: Frame processing time {}ms exceeded time budget {}ms",
                frame_duration.as_millis(),
                frame_duration_budget.as_millis()
            );
        }
    }
}

async fn is_disruptions_render_pending() -> bool {
    transit_data::get_mut()
        .await
        .as_ref()
        .is_some_and(|store| store.state.rendered_state.disruptions_render_pending)
}

// Every line of the transit data has one (see transit_data::update_line_configs)
fn line_config<'a>(config: &'a DeviceConfig, line: &Line) -> Option<&'a LineConfig> {
    config
        .line_configs
        .iter()
        .find(|line_config| line_config.line_name == line.name)
}

fn is_line_enabled(line_config: &LineConfig) -> bool {
    !line_config.has_override || line_config.enabled
}

// The pixel location of the stop, if it's a station on the map
fn stop_location(stop_locations: &[NonMax<u16>], stop_id: usize) -> Option<u16> {
    stop_locations
        .get(stop_id)
        .and_then(|location| location.as_option())
}
