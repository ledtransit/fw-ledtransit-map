// Button controls on the back of the map
use defmt::{debug, info};

use crate::{
    buttons::{Button, ButtonPress, ButtonsPressedState, wait_for_button_press},
    display::leds::{self, LedPixels},
    net::{
        wifi_net,
        ws_client::{self, client_proto::ColorMode},
    },
    ota,
    store::{
        app_settings::{self, persist::PersistSettings},
        transit_data,
    },
};

const BRIGHTNESS_STEP_PERCENT: u32 = 5;

pub async fn handle_ui_forever() -> ! {
    loop {
        match wait_for_button_press().await {
            ButtonPress::Short(button) => on_short_press(button).await,
            ButtonPress::Long(button) => on_long_press(button).await,
            ButtonPress::CombinedLong(buttons) => on_combined_long_press(buttons).await,
        }
    }
}

async fn on_short_press(button: Button) {
    debug!("Short press detected on button: {:?}", button);
    match button {
        Button::Up => {
            info!("UI: Increasing brightness");
            change_brightness(|percent| (percent + BRIGHTNESS_STEP_PERCENT).min(100)).await;
        }
        Button::Middle => {
            info!("UI: Toggling LEDs on/off");
            toggle_light().await;
        }
        Button::Down => {
            info!("UI: Decreasing brightness");
            change_brightness(|percent| {
                percent
                    .saturating_sub(BRIGHTNESS_STEP_PERCENT)
                    .max(BRIGHTNESS_STEP_PERCENT)
            })
            .await;
        }
    }
}

async fn on_long_press(button: Button) {
    debug!("Long press detected on button: {:?}", button);
    match button {
        Button::Up => {
            info!("UI: Cycling color mode between original/delays");
            cycle_color_mode().await;
        }
        Button::Middle => {
            info!("UI: Restarting WiFi provisioning");
            wifi_net::start_provisioning().await;
        }
        Button::Down => start_available_update().await,
    }
}

async fn on_combined_long_press(buttons: ButtonsPressedState) {
    debug!("Combined long press detected: {:?}", buttons);
    if buttons.up && buttons.middle && buttons.down {
        toggle_test_mode().await;
    } else if buttons.up && buttons.down {
        info!("UI: Performing factory reset");
        app_settings::persist::update_settings(|set| *set = PersistSettings::default()).await;
        ota::boot_from_factory();
    }
}

async fn change_brightness(change: impl FnOnce(u32) -> u32) {
    let changed = app_settings::persist::update_settings_changed(|set| {
        set.config.brightness_percent = change(set.config.brightness_percent);
    })
    .await;
    if changed {
        leds::update();
        ws_client::send_config();
        transit_data::on_config_updated().await;
    }
}

async fn toggle_light() {
    let changed = app_settings::session::update_settings_changed(|set| {
        set.light_on = !set.light_on;
        set.light_on_override = Some(set.light_on);
    })
    .await;
    if changed {
        leds::set_status_led_from_session().await;
        ws_client::send_status();
    }
}

async fn cycle_color_mode() {
    app_settings::persist::update_settings_changed(|set| {
        set.config.color_mode = if set.config.color_mode == ColorMode::Original as i32 {
            ColorMode::DelayHeatmap as i32
        } else {
            ColorMode::Original as i32
        };
    })
    .await;
    ws_client::send_config();
    transit_data::on_config_updated().await;
}

async fn start_available_update() {
    let settings = app_settings::session::get_settings().await;
    if let Some(update) = &settings.firmware_update_available {
        info!("UI: Starting firmware update");
        ota::start_firmware_update(update).await;
    } else {
        info!("UI: No firmware update available");
    }
}

async fn toggle_test_mode() {
    let test_mode_active = app_settings::session::get_settings().await.test_mode_active;
    if !test_mode_active {
        info!("UI: Entering LED test mode");
        app_settings::session::update_settings(|set| set.test_mode_active = true).await;
        leds::set_pixels(LedPixels::TestMode).await;
        return;
    }

    info!("UI: Exiting LED test mode");
    app_settings::session::update_settings(|set| set.test_mode_active = false).await;
    leds::set_pixels(LedPixels::FadeOut).await;
    leds::wait_pixels_animation_complete().await;
    let is_provisioned = app_settings::persist::get_settings()
        .await
        .has_credentials_and_is_claimed();
    if !is_provisioned {
        leds::set_pixels(LedPixels::DemoMode).await;
    }
}
