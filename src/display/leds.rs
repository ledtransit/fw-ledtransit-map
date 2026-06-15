use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, with_timeout};
use esp_hal::{
    clock::Clocks,
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

static LED_DRIVER_SIGNAL: Signal<CriticalSectionRawMutex, LedDriverEvent> = Signal::new();
#[ram(unstable(rtc_fast))]
static LED_BUFFER: Mutex<CriticalSectionRawMutex, [RGB8; CONFIG.cfg.pixel_count + 1]> =
    Mutex::new([RGB8::new(0, 0, 0); CONFIG.cfg.pixel_count + 1]);

enum LedDriverEvent {
    UpdateLeds,
}

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
    AuthError,    // Authentication error (token rejected)
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
            LedColor::Black => RGB8 { r: 0, g: 0, b: 0 },
            LedColor::Amber => RGB8 {
                r: 255,
                g: 163,
                b: 108,
            },
            LedColor::Green => RGB8 {
                r: 0,
                g: 255,
                b: 80,
            },
            LedColor::Blue => RGB8 {
                r: 0,
                g: 80,
                b: 255,
            },
            LedColor::Yellow => RGB8 {
                r: 255,
                g: 180,
                b: 0,
            },
            LedColor::Red => RGB8 {
                r: 255,
                g: 60,
                b: 0,
            },
            LedColor::Pink => RGB8 {
                r: 255,
                g: 0,
                b: 200,
            },
            LedColor::Purple => RGB8 {
                r: 140,
                g: 0,
                b: 255,
            },
        }
    }
}

enum LedInterval {
    Short,
    Medium,
    Long,
}

impl LedInterval {
    pub fn as_duration(&self) -> Duration {
        match self {
            LedInterval::Short => Duration::from_millis(200),
            LedInterval::Medium => Duration::from_millis(500),
            LedInterval::Long => Duration::from_secs(2),
        }
    }
}

pub fn spawn(spawner: Spawner, gpio: AnyPin<'static>, rmt_peri: RMT<'static>) {
    spawner.spawn(led_driver_task(gpio, rmt_peri).unwrap());
    animations::spawn(spawner);
}

async fn led_buffer_iter_processed(led_buffer: &[RGB8]) -> impl Iterator<Item = RGB8> + '_ {
    let is_light_on = app_settings::session::get_settings().await.light_on;
    let brightness_percent = get_current_brightness_percent().await;

    let status_led_iter = brightness(
        gamma(core::iter::once(led_buffer[0])),
        brightness_percent.saturating_add(100),
    );
    let pixel_leds_iter = brightness(
        gamma(led_buffer.iter().cloned().skip(1)),
        if is_light_on { brightness_percent } else { 0 },
    );
    status_led_iter.chain(pixel_leds_iter)
}

pub async fn get_current_estimate_milliamps() -> u32 {
    const CHA_RED_MA: f32 = 5.175;
    const CHA_GREEN_MA: f32 = 5.375;
    const CHA_BLUE_MA: f32 = 5.9;
    const SYS_IDLE_MA: f32 = 233.0;

    let led_buffer = LED_BUFFER.lock().await;
    let leds_iter = led_buffer_iter_processed(&*led_buffer).await;

    let mut total_ma: f32 = 0.0;
    for led in leds_iter {
        total_ma += (led.r as f32 / 255.0) * CHA_RED_MA;
        total_ma += (led.g as f32 / 255.0) * CHA_GREEN_MA;
        total_ma += (led.b as f32 / 255.0) * CHA_BLUE_MA;
    }
    total_ma += SYS_IDLE_MA;
    total_ma as u32
}

pub async fn get_current_brightness_percent() -> u8 {
    let session_settings = app_settings::session::get_settings().await;
    let persist_settings = app_settings::persist::get_settings().await;

    let manual_brightness_percent = persist_settings.config.brightness_percent as u8;
    let auto_brightness_percent_opt = session_settings.auto_brightness_percent;
    if let Some(auto_brightness_percent) = auto_brightness_percent_opt {
        auto_brightness_percent
    } else {
        manual_brightness_percent
    }
}

#[embassy_executor::task]
async fn led_driver_task(gpio: AnyPin<'static>, rmt_peri: RMT<'static>) {
    // Initialize RMT channel for LED driving
    let rmt = Rmt::new(rmt_peri, Rate::from_mhz(80)).expect("Failed to initialize RMT");
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
    let clock_mhz = Clocks::get().apb_clock.as_mhz();

    let mut led_driver = LedDriver::new(rmt_channel, ws2812_pulses(clock_mhz));

    // Clear all LEDs
    led_driver.write(LED_BUFFER.lock().await.iter().cloned());

    loop {
        match LED_DRIVER_SIGNAL.wait().await {
            LedDriverEvent::UpdateLeds => {
                let current_limit_ma = app_settings::persist::get_settings()
                    .await
                    .config
                    .current_limit_ma;

                // Reduce configured brightnesses until within configured current limit
                let mut brightness_changed = false;
                while get_current_estimate_milliamps().await > current_limit_ma {
                    app_settings::persist::update_settings(|set| {
                        let max_brightness_percent = set
                            .config
                            .brightness_percent
                            .max(set.config.sunlight_auto_brightness.day_brightness_percent)
                            .max(set.config.sunlight_auto_brightness.night_brightness_percent);
                        let limited_brightness_percent = max_brightness_percent.saturating_sub(5);
                        set.config.brightness_percent = set
                            .config
                            .brightness_percent
                            .min(limited_brightness_percent);
                        set.config.sunlight_auto_brightness.day_brightness_percent = set
                            .config
                            .sunlight_auto_brightness
                            .day_brightness_percent
                            .min(limited_brightness_percent);
                        set.config.sunlight_auto_brightness.night_brightness_percent = set
                            .config
                            .sunlight_auto_brightness
                            .night_brightness_percent
                            .min(limited_brightness_percent);
                    })
                    .await;
                    brightness_changed = true;
                    automation::step().await; // recalculate auto brightness percent if needed
                }
                if brightness_changed {
                    ws_client::send_config();
                }

                let led_buffer = LED_BUFFER.lock().await;
                let leds_iter = led_buffer_iter_processed(&*led_buffer).await;

                // Transmit LED RMT data in critical section to avoid timing issues with interrupts
                critical_section::with(|_| {
                    led_driver.write(leds_iter);
                });
            }
        }
    }
}

pub fn set_status(status: LedStatus) {
    animations::cancel_status_animation();

    match status {
        LedStatus::Idle => {
            animations::start_status_animation(LedStatusAnimationEvent::Constant(
                LedColor::Black.as_rgb8(),
            ));
        }
        LedStatus::Ok => {
            animations::start_status_animation(LedStatusAnimationEvent::Constant(
                LedColor::Green.as_rgb8(),
            ));
        }
        LedStatus::OkUpdateAvailable => {
            animations::start_status_animation(LedStatusAnimationEvent::Alternate(
                LedColor::Green.as_rgb8(),
                LedColor::Pink.as_rgb8(),
                LedInterval::Long.as_duration(),
                LedInterval::Short.as_duration(),
            ));
        }
        LedStatus::ToggleOff => {
            animations::start_status_animation(LedStatusAnimationEvent::Constant(
                LedColor::Blue.as_rgb8(),
            ));
        }
        LedStatus::TimerOff => {
            animations::start_status_animation(LedStatusAnimationEvent::Constant(
                LedColor::Purple.as_rgb8(),
            ));
        }
        LedStatus::Pairing => {
            animations::start_status_animation(LedStatusAnimationEvent::Blink(
                LedColor::Blue.as_rgb8(),
                LedInterval::Medium.as_duration(),
            ));
        }
        LedStatus::ConnectingWifi => {
            animations::start_status_animation(LedStatusAnimationEvent::Blink(
                LedColor::Yellow.as_rgb8(),
                LedInterval::Medium.as_duration(),
            ));
        }
        LedStatus::ConnectingServer => {
            animations::start_status_animation(LedStatusAnimationEvent::Blink(
                LedColor::Green.as_rgb8(),
                LedInterval::Medium.as_duration(),
            ));
        }
        LedStatus::UpdatingFirmware => {
            animations::start_status_animation(LedStatusAnimationEvent::Blink(
                LedColor::Pink.as_rgb8(),
                LedInterval::Medium.as_duration(),
            ));
        }
        LedStatus::WifiError => {
            animations::start_status_animation(LedStatusAnimationEvent::Alternate(
                LedColor::Yellow.as_rgb8(),
                LedColor::Red.as_rgb8(),
                LedInterval::Short.as_duration(),
                LedInterval::Short.as_duration(),
            ));
        }
        LedStatus::ServerError => {
            animations::start_status_animation(LedStatusAnimationEvent::Alternate(
                LedColor::Green.as_rgb8(),
                LedColor::Red.as_rgb8(),
                LedInterval::Short.as_duration(),
                LedInterval::Short.as_duration(),
            ));
        }
        LedStatus::AuthError => {
            animations::start_status_animation(LedStatusAnimationEvent::Alternate(
                LedColor::Blue.as_rgb8(),
                LedColor::Red.as_rgb8(),
                LedInterval::Short.as_duration(),
                LedInterval::Short.as_duration(),
            ));
        }
        LedStatus::UpdateFailed => {
            animations::start_status_animation(LedStatusAnimationEvent::Alternate(
                LedColor::Pink.as_rgb8(),
                LedColor::Red.as_rgb8(),
                LedInterval::Short.as_duration(),
                LedInterval::Short.as_duration(),
            ));
        }
    }
}

pub async fn set_pixels(pixels: LedPixels) {
    animations::cancel_pixels_animation();

    match pixels {
        LedPixels::Off => {
            LED_BUFFER.lock().await[1..]
                .iter_mut()
                .for_each(|c| *c = LedColor::Black.as_rgb8());
            LED_DRIVER_SIGNAL.signal(LedDriverEvent::UpdateLeds);
        }
        LedPixels::FadeOut => {
            animations::start_pixels_animation(LedPixelsAnimationEvent::FadeOut);
        }
        LedPixels::StartupAnimation(color) => {
            animations::start_pixels_animation(LedPixelsAnimationEvent::PlayStartup(color));
        }
        LedPixels::ProgressPercent(progress) => {
            animations::start_pixels_animation(LedPixelsAnimationEvent::ProgressPercent(progress));
        }
        LedPixels::Identify => {
            animations::start_pixels_animation(LedPixelsAnimationEvent::Identify);
        }
        LedPixels::TestMode => {
            animations::start_pixels_animation(LedPixelsAnimationEvent::TestMode);
        }
        LedPixels::DemoMode => {
            animations::start_pixels_animation(LedPixelsAnimationEvent::DemoMode);
        }
    }
}

pub async fn wait_pixels_animation_complete() {
    with_timeout(
        Duration::from_secs(3),
        animations::wait_until_pixels_animation_complete(),
    )
    .await
    .ok();
}

pub fn update() {
    LED_DRIVER_SIGNAL.signal(LedDriverEvent::UpdateLeds);
}

pub async fn get_mut_pixel_buffer() -> impl core::ops::DerefMut<Target = [RGB8]> {
    struct PixelBufferGuard<'a> {
        guard: embassy_sync::mutex::MutexGuard<
            'a,
            CriticalSectionRawMutex,
            [RGB8; CONFIG.cfg.pixel_count + 1],
        >,
    }

    impl<'a> core::ops::Deref for PixelBufferGuard<'a> {
        type Target = [RGB8];

        fn deref(&self) -> &Self::Target {
            &self.guard[1..]
        }
    }

    impl<'a> core::ops::DerefMut for PixelBufferGuard<'a> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.guard[1..]
        }
    }

    let guard = LED_BUFFER.lock().await;
    PixelBufferGuard { guard }
}

pub async fn set_status_pixel(color: RGB8) {
    LED_BUFFER.lock().await[0] = color;
}

/// Set status LED based on current session state
pub async fn set_status_led_from_session() {
    let settings = app_settings::session::get_settings().await;
    let is_provisioned = app_settings::persist::get_settings()
        .await
        .has_credentials_and_is_authenticated();
    if !is_provisioned {
        return;
    }
    set_status(if settings.updating_firmware {
        LedStatus::UpdatingFirmware
    } else {
        if settings.light_on {
            if settings.firmware_update_available.is_some() {
                LedStatus::OkUpdateAvailable
            } else {
                LedStatus::Ok
            }
        } else {
            if settings.night_timer_active {
                LedStatus::TimerOff
            } else {
                LedStatus::ToggleOff
            }
        }
    });
}
