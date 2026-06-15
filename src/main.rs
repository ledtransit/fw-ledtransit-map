// LEDTransit Firmware powering the public transport LED maps.
// Copyright (C) 2026 Tim Holzhey
//
// Main entry point. Initializes system, RTOS, peripherals and spawns all tasks.
#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate serde;

mod automation;
mod buttons;
mod config;
mod display;
mod net;
mod store;
mod time;
mod trace;
mod ui;
mod util;

use crate::{
    buttons::Button,
    display::{
        leds::{self, LedColor, LedPixels, LedStatus},
        painter, renderer,
    },
    net::wifi_net,
    store::{app_settings, ota, transit_data},
};
use config::CONFIG;
use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, ram, timer::timg::TimerGroup,
};

// Place ESP-IDF app descriptor in flash section
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // Init product hardware configuration
    config::init();

    // Init system
    let hal_config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(hal_config);

    // Setup heap allocator
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 66320); // max reclaimed dram2_seg from bootloader
    esp_alloc::heap_allocator!(size: 136 * 1024);

    // Start RTOS
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let swi = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, swi.software_interrupt0);

    // Init flash storage
    let flash_store = store::init(peripherals.FLASH).await;

    // Spawn Embassy tasks taking peripheral ownership as needed
    leds::spawn(spawner, peripherals.GPIO1.into(), peripherals.RMT);
    buttons::spawn(spawner, peripherals.GPIO3.into(), Button::Up);
    buttons::spawn(spawner, peripherals.GPIO9.into(), Button::Middle);
    buttons::spawn(spawner, peripherals.GPIO4.into(), Button::Down);
    renderer::spawn(spawner);
    painter::spawn(spawner);
    wifi_net::spawn(spawner, peripherals.WIFI, peripherals.SHA, flash_store).await;
    app_settings::spawn(spawner, flash_store);

    info!(
        "LEDTransit Map initialized: {:?} (HW v{}.{}, FW v{}.{}.{}{})",
        CONFIG.product,
        CONFIG.hw_version.major,
        CONFIG.hw_version.minor,
        CONFIG.fw_version.major,
        CONFIG.fw_version.minor,
        CONFIG.fw_version.patch,
        CONFIG.fw_version.beta.then_some("-beta").unwrap_or("")
    );

    leds::set_status(LedStatus::Idle);
    trace::init_on_boot();

    // Start WiFi provisioning if needed, otherwise connect to WiFi AP
    let is_provisioned = app_settings::persist::get_settings()
        .await
        .has_credentials_and_is_authenticated();
    if is_provisioned {
        info!("WiFi is provisioned, attempting to connect to AP");
        wifi_net::connect_ap();
    } else {
        info!("No WiFi credentials or API token stored, entering provisioning mode");
        wifi_net::start_provisioning().await;
    }

    // Wait a bit for WiFi initialization to complete before sending timing-sensitive LED data
    leds::set_pixels(LedPixels::Off).await;
    Timer::after(Duration::from_millis(2900)).await;

    // Play startup animation if not already received transit data in the meantime
    if !transit_data::is_set().await {
        leds::set_pixels(LedPixels::StartupAnimation(LedColor::Amber.as_rgb8())).await;
        leds::wait_pixels_animation_complete().await;
        if !is_provisioned {
            leds::set_pixels(LedPixels::DemoMode).await;
        }
    }

    // Initialize boot partition, mark OTA valid
    ota::init_boot_partition();

    // Handle buttons UI in main task forever
    ui::handle_ui_forever().await;
    unreachable!();
}
