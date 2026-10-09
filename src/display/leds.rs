// The LEDs: the status LED (first), then the map's pixels. Brightness, gamma
// correction and the current limit are applied when they're written out
use core::ops::{Deref, DerefMut};

use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    mutex::{Mutex, MutexGuard},
    signal::Signal,
};
use embassy_time::{Duration, with_timeout};
use esp_hal::{
    gpio::{AnyPin, Level},
    peripherals::RMT,
    ram,
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::Rate,
};
use smart_leds::{RGB8, brightness, gamma};

use crate::{
    automation,
    config::CONFIG,
    display::{
        animations::{self, LedPixelsAnimationEvent, LedStatusAnimationEvent},
        led_driver::{LedDriver, ws2812_pulses},
    },
    net::ws_client,
    store::app_settings,
};

type LedBuffer = [RGB8; CONFIG.cfg.pixel_count + 1];

static LED_DRIVER_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
#[ram(unstable(rtc_fast))]
static LED_BUFFER: Mutex<CriticalSectionRawMutex, LedBuffer> =
    Mutex::new([RGB8::new(0, 0, 0); CONFIG.cfg.pixel_count + 1]);

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
    Pairing,          // WiFi AP server started for provisioning
    ConnectingWifi,   // Connecting to WiFi station
    ConnectingServer, // Connecting to HTTP/WS server
    UpdatingFirmware, // Firmware update in progress

    // Error
    WifiError,    // WiFi connection error (wrong credentials, AP not found)
    ServerError,  // HTTP/WS server connection error (cannot reach server, server down)
    AuthError,    // Authentication error (device keys rejected or missing, device revoked)
    UpdateFailed, // Firmware update failed
}

#[allow(unused)]
pub enum LedPixels {
    Off,
    StartupAnimation(RGB8),
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
        }
    }
}

pub fn spawn(spawner: Spawner, gpio: AnyPin<'static>, rmt_peri: RMT<'static>) {
    spawner.spawn(led_driver_task(gpio, rmt_peri).unwrap());
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
    set_status(if settings.updating_firmware {
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
        LedPixels::StartupAnimation(color) => LedPixelsAnimationEvent::PlayStartup(color),
        LedPixels::ProgressPercent(progress) => LedPixelsAnimationEvent::ProgressPercent(progress),
        LedPixels::Identify => LedPixelsAnimationEvent::Identify,
        LedPixels::TestMode => LedPixelsAnimationEvent::TestMode,
        LedPixels::DemoMode => LedPixelsAnimationEvent::DemoMode,
    };
    animations::start_pixels_animation(event);
}

pub async fn wait_pixels_animation_complete() {
    with_timeout(
        Duration::from_secs(3),
        animations::wait_until_pixels_animation_complete(),
    )
    .await
    .ok();
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
    let auto_brightness_percent = app_settings::session::get_settings()
        .await
        .auto_brightness_percent;
    let manual_brightness_percent = app_settings::persist::get_settings()
        .await
        .config
        .brightness_percent as u8;
    auto_brightness_percent.unwrap_or(manual_brightness_percent)
}

pub async fn get_current_estimate_milliamps() -> u32 {
    // Heuristic linear approximation of current draw based on LED RGB channel brightnesses, through point at 50% brightness setting midday with fixed 5.00V supply
    const CHA_RED_MA: f32 = 5.2;
    const CHA_GREEN_MA: f32 = 5.05;
    const CHA_BLUE_MA: f32 = 5.125;
    const SYS_IDLE_MA: f32 = 239.0; // WiFi connected, actively rendering and drawing

    let led_buffer = LED_BUFFER.lock().await;
    let leds_ma = processed_leds(&*led_buffer)
        .await
        .fold(0.0, |total_ma, led| {
            total_ma
                + (led.r as f32 / 255.0) * CHA_RED_MA
                + (led.g as f32 / 255.0) * CHA_GREEN_MA
                + (led.b as f32 / 255.0) * CHA_BLUE_MA
        });
    (leds_ma + SYS_IDLE_MA) as u32
}

// As written out: gamma corrected, at the brightness (the pixels off with the
// light off, the status LED brighter)
async fn processed_leds(led_buffer: &[RGB8]) -> impl Iterator<Item = RGB8> + '_ {
    let is_light_on = app_settings::session::get_settings().await.light_on;
    let brightness_percent = get_current_brightness_percent().await;

    let status_led = brightness(
        gamma(core::iter::once(led_buffer[0])),
        brightness_percent.saturating_add(100),
    );
    let pixels = brightness(
        gamma(led_buffer.iter().cloned().skip(1)),
        if is_light_on { brightness_percent } else { 0 },
    );
    status_led.chain(pixels)
}

#[embassy_executor::task]
async fn led_driver_task(gpio: AnyPin<'static>, rmt_peri: RMT<'static>) {
    let rmt = Rmt::new(rmt_peri, Rate::from_mhz(80)).expect("Failed to initialize RMT");
    let clock_mhz = rmt.frequency().as_mhz();
    let rmt_tx_config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(Level::Low)
        .with_carrier_modulation(false)
        .with_idle_output(true);
    let rmt_channel = rmt
        .channel0
        .configure_tx(&rmt_tx_config)
        .unwrap()
        .with_pin(gpio);
    let mut led_driver = LedDriver::new(rmt_channel, ws2812_pulses(clock_mhz));

    // Clear all LEDs
    led_driver.write(LED_BUFFER.lock().await.iter().cloned());

    loop {
        LED_DRIVER_SIGNAL.wait().await;
        limit_current().await;

        let led_buffer = LED_BUFFER.lock().await;
        let leds = processed_leds(&*led_buffer).await;
        // Interrupts would break the LEDs' timing
        critical_section::with(|_| {
            led_driver.write(leds);
        });
    }
}

// Lowers the configured brightnesses until the estimated current is within
// the configured limit
async fn limit_current() {
    const BRIGHTNESS_STEP_PERCENT: u32 = 5;
    let current_limit_ma = app_settings::persist::get_settings()
        .await
        .config
        .current_limit_ma;

    let mut brightness_changed = false;
    while get_current_estimate_milliamps().await > current_limit_ma {
        app_settings::persist::update_settings(|set| {
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
        brightness_changed = true;
        // Recalculates the auto brightness
        automation::step().await;
    }
    if brightness_changed {
        ws_client::send_config();
    }
}
