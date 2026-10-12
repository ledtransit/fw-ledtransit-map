// The LEDs: the status LED (first), then the map's pixels. Brightness, gamma
// correction and the current limit are applied when they're written out
use core::ops::{Deref, DerefMut};

use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    mutex::{Mutex, MutexGuard},
    signal::Signal,
};
use embassy_time::{Duration, Instant, with_deadline};
use esp_hal::{
    gpio::AnyPin,
    interrupt::Priority,
    peripherals::{DMA_CH0, FROM_CPU_INTR1, SPI2},
    ram,
};
use esp_rtos::embassy::InterruptExecutor;
use smart_leds::{RGB8, brightness, gamma};
use static_cell::{ConstStaticCell, StaticCell};

use crate::{
    automation,
    config::CONFIG,
    display::{
        animations::{self, LedPixelsAnimationEvent, LedStatusAnimationEvent},
        led_driver::{self, LedDriver},
    },
    net::ws_client,
    store::app_settings,
};

const LED_COUNT: usize = CONFIG.cfg.pixel_count + 1;
type LedBuffer = [RGB8; LED_COUNT];

static LED_DRIVER_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
#[ram(unstable(rtc_fast))]
static LED_BUFFER: Mutex<CriticalSectionRawMutex, LedBuffer> =
    Mutex::new([RGB8::new(0, 0, 0); LED_COUNT]);
// The LEDs' encoded data, sent by DMA
static LED_FRAME: ConstStaticCell<[u8; led_driver::frame_len(LED_COUNT)]> =
    ConstStaticCell::new([0; led_driver::frame_len(LED_COUNT)]);

// The frame rate is averaged over the last frames, and stored once a second
// (not each frame)
const FRAME_RATE_FRAMES: usize = 16;
const FRAME_RATE_STORE_INTERVAL: Duration = Duration::from_secs(1);

// Longer than the longest pixel animation ending by itself (the startup, 4 s)
const PIXELS_ANIMATION_TIMEOUT: Duration = Duration::from_secs(5);

const SHORT_INTERVAL: Duration = Duration::from_millis(200);
const MEDIUM_INTERVAL: Duration = Duration::from_millis(500);
const LONG_INTERVAL: Duration = Duration::from_secs(2);

pub enum LedStatus {
    // Static
    Idle,              // Boot
    Ok,                // Normal operation
    OkUpdateAvailable, // Normal operation but with firmware update available
    ToggleOff,         // LEDs turned off by toggle button (hardware button or online)
    TimerOff,          // Day/night timer is driving the LEDs off

    // Process
    Pairing,              // WiFi AP server started for provisioning
    ConnectingWifi,       // Connecting to WiFi station
    ConnectingServer,     // Connecting to HTTP/WS server
    UpdatingFirmware,     // Firmware update in progress
    AwaitingConfirmation, // Server command awaiting a button press on the map to confirm it

    // Error
    WifiError,    // WiFi connection error (wrong credentials, AP not found)
    ServerError,  // HTTP/WS server connection error (cannot reach server, server down)
    AuthError,    // Authentication error (device keys rejected or missing, device revoked)
    UpdateFailed, // Firmware update failed
}

#[allow(unused)]
pub enum LedPixels {
    Off,
    StartupAnimation,
    ProgressPercent(u8),
    FadeOut,
    Identify,
    TestMode,
    DemoMode,
}

pub enum LedColor {
    Black,
    Amber,
    Green,
    Blue,
    Yellow,
    Red,
    Pink,
    Purple,
    White,
}

impl LedColor {
    pub fn as_rgb8(&self) -> RGB8 {
        match self {
            LedColor::Black => RGB8::new(0, 0, 0),
            LedColor::Amber => RGB8::new(255, 163, 108),
            LedColor::Green => RGB8::new(0, 255, 80),
            LedColor::Blue => RGB8::new(0, 80, 255),
            LedColor::Yellow => RGB8::new(255, 180, 0),
            LedColor::Red => RGB8::new(255, 60, 0),
            LedColor::Pink => RGB8::new(255, 0, 200),
            LedColor::Purple => RGB8::new(140, 0, 255),
            LedColor::White => RGB8::new(255, 255, 255),
        }
    }
}

pub fn spawn(
    spawner: Spawner,
    interrupt: FROM_CPU_INTR1<'static>,
    gpio: AnyPin<'static>,
    spi_peri: SPI2<'static>,
    dma_channel: DMA_CH0<'static>,
) {
    // In an interrupt: the next frame starts right once the last one is sent,
    // not once the task polled on the thread executor before is done
    static EXECUTOR: StaticCell<InterruptExecutor<1>> = StaticCell::new();
    let executor = EXECUTOR.init(InterruptExecutor::new(interrupt));
    executor
        .start(Priority::Priority1)
        .spawn(led_driver_task(gpio, spi_peri, dma_channel).unwrap());
    animations::spawn(spawner);
}

/// Writes the buffer out to the LEDs.
pub fn update() {
    LED_DRIVER_SIGNAL.signal(());
}

pub fn set_status(status: LedStatus) {
    animations::cancel_status_animation();
    animations::start_status_animation(status_animation(status));
}

fn status_animation(status: LedStatus) -> LedStatusAnimationEvent {
    use LedStatusAnimationEvent::{Alternate, Blink, Constant};
    let error = |color: LedColor| {
        Alternate(
            color.as_rgb8(),
            LedColor::Red.as_rgb8(),
            SHORT_INTERVAL,
            SHORT_INTERVAL,
        )
    };
    match status {
        LedStatus::Idle => Constant(LedColor::Black.as_rgb8()),
        LedStatus::Ok => Constant(LedColor::Green.as_rgb8()),
        LedStatus::OkUpdateAvailable => Alternate(
            LedColor::Green.as_rgb8(),
            LedColor::Pink.as_rgb8(),
            LONG_INTERVAL,
            SHORT_INTERVAL,
        ),
        LedStatus::ToggleOff => Constant(LedColor::Blue.as_rgb8()),
        LedStatus::TimerOff => Constant(LedColor::Purple.as_rgb8()),
        LedStatus::Pairing => Blink(LedColor::Blue.as_rgb8(), MEDIUM_INTERVAL),
        LedStatus::ConnectingWifi => Blink(LedColor::Yellow.as_rgb8(), MEDIUM_INTERVAL),
        LedStatus::ConnectingServer => Blink(LedColor::Green.as_rgb8(), MEDIUM_INTERVAL),
        LedStatus::UpdatingFirmware => Blink(LedColor::Pink.as_rgb8(), MEDIUM_INTERVAL),
        LedStatus::AwaitingConfirmation => Blink(LedColor::White.as_rgb8(), SHORT_INTERVAL),
        LedStatus::WifiError => error(LedColor::Yellow),
        LedStatus::ServerError => error(LedColor::Green),
        LedStatus::AuthError => error(LedColor::Blue),
        LedStatus::UpdateFailed => error(LedColor::Pink),
    }
}

/// The status matching the session state (once set up).
pub async fn set_status_led_from_session() {
    let settings = app_settings::session::get_settings().await;
    let is_provisioned = app_settings::persist::get_settings()
        .await
        .has_credentials_and_is_claimed();
    if !is_provisioned {
        return;
    }
    // A confirmation first: it needs someone at the map, and only lasts a moment
    set_status(if settings.pending_confirmation.is_some() {
        LedStatus::AwaitingConfirmation
    } else if settings.updating_firmware {
        LedStatus::UpdatingFirmware
    } else if settings.light_on && settings.firmware_update_available.is_some() {
        LedStatus::OkUpdateAvailable
    } else if settings.light_on {
        LedStatus::Ok
    } else if settings.night_timer_active {
        LedStatus::TimerOff
    } else {
        LedStatus::ToggleOff
    });
}

pub async fn set_pixels(pixels: LedPixels) {
    animations::cancel_pixels_animation();
    let event = match pixels {
        LedPixels::Off => {
            LED_BUFFER.lock().await[1..].fill(LedColor::Black.as_rgb8());
            update();
            return;
        }
        LedPixels::FadeOut => LedPixelsAnimationEvent::FadeOut,
        LedPixels::StartupAnimation => LedPixelsAnimationEvent::PlayStartup,
        LedPixels::ProgressPercent(progress) => LedPixelsAnimationEvent::ProgressPercent(progress),
        LedPixels::Identify => LedPixelsAnimationEvent::Identify,
        LedPixels::TestMode => LedPixelsAnimationEvent::TestMode,
        LedPixels::DemoMode => LedPixelsAnimationEvent::DemoMode,
    };
    animations::start_pixels_animation(event);
}

/// Waits until the pixel animation just started ended (or was replaced),
/// canceling it after a while: it never keeps running next to what's drawn
/// next.
pub async fn wait_pixels_animation_complete() {
    animations::wait_until_pixels_animation_ended(PIXELS_ANIMATION_TIMEOUT).await;
}

/// Whether a pixel animation has the pixels: nothing else may draw them.
pub fn is_pixels_animation_running() -> bool {
    animations::is_pixels_animation_running()
}

pub async fn set_status_pixel(color: RGB8) {
    LED_BUFFER.lock().await[0] = color;
}

/// The map's pixels (without the status LED).
pub async fn get_mut_pixel_buffer() -> impl DerefMut<Target = [RGB8]> {
    PixelBufferGuard(LED_BUFFER.lock().await)
}

struct PixelBufferGuard<'a>(MutexGuard<'a, CriticalSectionRawMutex, LedBuffer>);

impl Deref for PixelBufferGuard<'_> {
    type Target = [RGB8];

    fn deref(&self) -> &Self::Target {
        &self.0[1..]
    }
}

impl DerefMut for PixelBufferGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0[1..]
    }
}

/// The brightness from the sunlight automation, else the configured one.
pub async fn get_current_brightness_percent() -> u8 {
    let auto_brightness_percent =
        app_settings::session::read_settings(|set| set.auto_brightness_percent).await;
    let manual_brightness_percent =
        app_settings::persist::read_settings(|set| set.config.brightness_percent).await as u8;
    auto_brightness_percent.unwrap_or(manual_brightness_percent)
}

pub async fn get_current_estimate_milliamps() -> u32 {
    let maps = OutputMaps::new(output_brightnesses().await);
    let mut sums = ChannelSums::default();
    processed_leds(&*LED_BUFFER.lock().await, &maps).for_each(|led| sums.add(led));
    sums.estimate_milliamps()
}

// The brightnesses written out with, of the status LED and of the pixels:
// the status LED brighter, the pixels off with the light off
async fn output_brightnesses() -> (u8, u8) {
    let is_light_on = app_settings::session::read_settings(|set| set.light_on).await;
    let brightness_percent = get_current_brightness_percent().await;
    (
        brightness_percent.saturating_add(100),
        if is_light_on { brightness_percent } else { 0 },
    )
}

// Each channel's value as written out: gamma corrected, at the brightness.
// Looked up, not calculated, for each channel of each LED every frame
struct ChannelMap {
    brightness: u8,
    values: [u8; 256],
}

impl ChannelMap {
    fn new(brightness_level: u8) -> Self {
        let mut values = [0; 256];
        for (value, out) in (0..=u8::MAX).zip(values.iter_mut()) {
            let color = RGB8::new(value, value, value);
            *out = brightness(gamma(core::iter::once(color)), brightness_level)
                .next()
                .map_or(0, |color| color.r);
        }
        Self {
            brightness: brightness_level,
            values,
        }
    }

    fn map(&self, color: RGB8) -> RGB8 {
        RGB8::new(
            self.values[usize::from(color.r)],
            self.values[usize::from(color.g)],
            self.values[usize::from(color.b)],
        )
    }
}

// Of the status LED and of the pixels
struct OutputMaps {
    status: ChannelMap,
    pixels: ChannelMap,
}

impl OutputMaps {
    fn new((status_brightness, pixels_brightness): (u8, u8)) -> Self {
        Self {
            status: ChannelMap::new(status_brightness),
            pixels: ChannelMap::new(pixels_brightness),
        }
    }

    // Recalculated only when the brightnesses changed
    fn update(&mut self, (status_brightness, pixels_brightness): (u8, u8)) {
        if self.status.brightness != status_brightness {
            self.status = ChannelMap::new(status_brightness);
        }
        if self.pixels.brightness != pixels_brightness {
            self.pixels = ChannelMap::new(pixels_brightness);
        }
    }
}

// As written out
fn processed_leds<'a>(
    led_buffer: &'a [RGB8],
    maps: &'a OutputMaps,
) -> impl Iterator<Item = RGB8> + 'a {
    let status_led = core::iter::once(maps.status.map(led_buffer[0]));
    let pixels = led_buffer[1..].iter().map(|&led| maps.pixels.map(led));
    status_led.chain(pixels)
}

// The channels' values of the LEDs as written out, summed up
#[derive(Default)]
struct ChannelSums {
    r: u32,
    g: u32,
    b: u32,
}

impl ChannelSums {
    fn add(&mut self, led: RGB8) {
        self.r += u32::from(led.r);
        self.g += u32::from(led.g);
        self.b += u32::from(led.b);
    }

    fn estimate_milliamps(&self) -> u32 {
        // Heuristic linear approximation of current draw based on LED RGB channel brightnesses, through point at 50% brightness setting midday with fixed 5.00V supply
        const CHA_RED_MA: f32 = 5.2;
        const CHA_GREEN_MA: f32 = 5.05;
        const CHA_BLUE_MA: f32 = 5.125;
        const SYS_IDLE_MA: f32 = 239.0; // WiFi connected, actively rendering and drawing

        let leds_ma = (self.r as f32 * CHA_RED_MA
            + self.g as f32 * CHA_GREEN_MA
            + self.b as f32 * CHA_BLUE_MA)
            / 255.0;
        (leds_ma + SYS_IDLE_MA) as u32
    }
}

#[embassy_executor::task]
async fn led_driver_task(
    gpio: AnyPin<'static>,
    spi_peri: SPI2<'static>,
    dma_channel: DMA_CH0<'static>,
) {
    let mut led_driver = LedDriver::new(spi_peri, dma_channel, gpio, LED_FRAME.take());

    // Clear all LEDs
    led_driver.encode(LED_BUFFER.lock().await.iter().cloned());
    led_driver.transmit().await;

    let mut maps = OutputMaps::new(output_brightnesses().await);
    let mut frame_rate = FrameRate::new();
    let mut store_frame_rate_at = Instant::now() + FRAME_RATE_STORE_INTERVAL;
    loop {
        if with_deadline(store_frame_rate_at, LED_DRIVER_SIGNAL.wait())
            .await
            .is_ok()
        {
            encode_within_current_limit(&mut led_driver, &mut maps).await;
            led_driver.transmit().await;
            frame_rate.on_frame();
        }

        if Instant::now() >= store_frame_rate_at {
            let frames_per_second = frame_rate.frames_per_second();
            app_settings::session::update_settings(|set| {
                set.display_frames_per_second = frames_per_second;
            })
            .await;
            store_frame_rate_at = Instant::now() + FRAME_RATE_STORE_INTERVAL;
        }
    }
}

// Encodes the LEDs, first lowering the configured brightnesses until the
// estimated current is within the configured limit
async fn encode_within_current_limit(led_driver: &mut LedDriver, maps: &mut OutputMaps) {
    let current_limit_ma =
        app_settings::persist::read_settings(|set| set.config.current_limit_ma).await;

    let mut brightness_changed = false;
    loop {
        maps.update(output_brightnesses().await);
        let led_buffer = LED_BUFFER.lock().await;
        let status_sums = led_driver.encode_mapped(0, &led_buffer[..1], &maps.status.values);
        let pixels_sums = led_driver.encode_mapped(1, &led_buffer[1..], &maps.pixels.values);
        drop(led_buffer);
        let sums = ChannelSums {
            r: status_sums[0] + pixels_sums[0],
            g: status_sums[1] + pixels_sums[1],
            b: status_sums[2] + pixels_sums[2],
        };

        if sums.estimate_milliamps() <= current_limit_ma || !lower_brightness().await {
            break;
        }
        brightness_changed = true;
    }
    if brightness_changed {
        ws_client::send_config();
    }
}

// Lowers the configured brightnesses one step, and whether that changed them
// (not once all are off)
async fn lower_brightness() -> bool {
    const BRIGHTNESS_STEP_PERCENT: u32 = 5;
    let changed = app_settings::persist::update_settings_changed(|set| {
        let auto_brightness = &mut set.config.sunlight_auto_brightness;
        let limited_brightness_percent = set
            .config
            .brightness_percent
            .max(auto_brightness.day_brightness_percent)
            .max(auto_brightness.night_brightness_percent)
            .saturating_sub(BRIGHTNESS_STEP_PERCENT);
        set.config.brightness_percent = set
            .config
            .brightness_percent
            .min(limited_brightness_percent);
        auto_brightness.day_brightness_percent = auto_brightness
            .day_brightness_percent
            .min(limited_brightness_percent);
        auto_brightness.night_brightness_percent = auto_brightness
            .night_brightness_percent
            .min(limited_brightness_percent);
    })
    .await;
    // Recalculates the auto brightness
    automation::step().await;
    changed
}

// The times the last frames were written out to the LEDs
struct FrameRate {
    frame_times: heapless::Deque<Instant, FRAME_RATE_FRAMES>,
}

impl FrameRate {
    fn new() -> Self {
        Self {
            frame_times: heapless::Deque::new(),
        }
    }

    fn on_frame(&mut self) {
        if self.frame_times.is_full() {
            self.frame_times.pop_front();
        }
        self.frame_times.push_back(Instant::now()).ok();
    }

    /// Frames per second over the last frames. 0 when nothing was written out
    /// for a while: the LEDs don't change.
    fn frames_per_second(&self) -> f32 {
        let (Some(&oldest), Some(&newest)) = (self.frame_times.front(), self.frame_times.back())
        else {
            return 0.0;
        };
        if Instant::now() - newest > FRAME_RATE_STORE_INTERVAL || newest == oldest {
            return 0.0;
        }
        let frame_intervals = (self.frame_times.len() - 1) as f32;
        frame_intervals * 1_000_000.0 / (newest - oldest).as_micros() as f32
    }
}
