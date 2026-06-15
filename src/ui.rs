use defmt::{debug, info};

use crate::{
    buttons::{Button, ButtonPress, wait_for_button_press},
    display::leds::{self, LedPixels},
    net::{
        wifi_net,
        ws_client::{self, client_proto::ColorMode},
    },
    store::{
        app_settings::{self, persist::PersistSettings},
        ota, transit_data,
    },
};

pub async fn handle_ui_forever() {
    loop {
        match wait_for_button_press().await {
            ButtonPress::Short(button) => {
                debug!("Short press detected on button: {:?}", button);
                match button {
                    Button::Up => {
                        info!("UI: Increasing brightness");
                        if app_settings::persist::update_settings_changed(|set| {
                            set.config.brightness_percent =
                                (set.config.brightness_percent + 5).min(100);
                        })
                        .await
                        {
                            leds::update();
                            ws_client::send_config();
                            transit_data::on_config_updated().await;
                        }
                    }
                    Button::Middle => {
                        info!("UI: Toggling LEDs on/off");
                        if app_settings::session::update_settings_changed(|set| {
                            set.light_on = !set.light_on;
                            set.light_on_override = Some(set.light_on);
                        })
                        .await
                        {
                            leds::set_status_led_from_session().await;
                            ws_client::send_status();
                        }
                    }
                    Button::Down => {
                        info!("UI: Decreasing brightness");
                        if app_settings::persist::update_settings_changed(|set| {
                            set.config.brightness_percent =
                                set.config.brightness_percent.saturating_sub(5).max(5);
                        })
                        .await
                        {
                            leds::update();
                            ws_client::send_config();
                            transit_data::on_config_updated().await;
                        }
                    }
                }
            }
            ButtonPress::Long(button) => {
                debug!("Long press detected on button: {:?}", button);
                match button {
                    Button::Up => {
                        info!("UI: Cycling color mode between original/delays");
                        app_settings::persist::update_settings_changed(|set| {
                            set.config.color_mode = match set.config.color_mode {
                                cm if cm == ColorMode::Original as i32 => {
                                    ColorMode::DelayHeatmap as i32
                                }
                                _ => ColorMode::Original as i32,
                            };
                        })
                        .await;
                        ws_client::send_config();
                        transit_data::on_config_updated().await;
                    }
                    Button::Middle => {
                        info!("UI: Restarting WiFi provisioning");
                        wifi_net::start_provisioning().await;
                    }
                    Button::Down => {
                        // Start a firmware update if available
                        let settings = app_settings::session::get_settings().await;
                        if let Some(update) = &settings.firmware_update_available {
                            info!("UI: Starting firmware update");
                            ota::start_firmware_update(update).await;
                        } else {
                            info!("UI: No firmware update available");
                        }
                    }
                }
            }
            ButtonPress::CombinedLong(buttons) => {
                debug!("Combined long press detected: {:?}", buttons);
                let settings = app_settings::session::get_settings().await;

                if buttons.up && buttons.select && buttons.down {
                    if !settings.test_mode_active {
                        info!("UI: Entering LED test mode");
                        app_settings::session::update_settings(|set| {
                            set.test_mode_active = true;
                        })
                        .await;
                        leds::set_pixels(leds::LedPixels::TestMode).await;
                    } else {
                        info!("UI: Exiting LED test mode");
                        app_settings::session::update_settings(|set| {
                            set.test_mode_active = false;
                        })
                        .await;
                        leds::set_pixels(LedPixels::FadeOut).await;
                        leds::wait_pixels_animation_complete().await;
                        let is_provisioned = app_settings::persist::get_settings()
                            .await
                            .has_credentials_and_is_authenticated();
                        if !is_provisioned {
                            leds::set_pixels(LedPixels::DemoMode).await;
                        }
                    }
                } else if buttons.up && buttons.down {
                    info!("UI: Performing factory reset");
                    app_settings::persist::update_settings(|set| *set = PersistSettings::default())
                        .await;
                    ota::boot_from_factory();
                }
            }
        }
    }
}
