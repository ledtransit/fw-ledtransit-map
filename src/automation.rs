// Day-night timer and sunlight auto brightness automation logic
use defmt::info;

use crate::{
    app_settings::{self},
    leds::{self, LedStatus},
    net::ws_client::{self, client_proto::TimerSettings},
    time,
};

// Given the current local time and timer settings, determine if the day-night timer should be driving the lights on
fn is_day_night_timer_driving_light_on(
    local_time_of_day_seconds: u32,
    local_weekday_number: u32,
    timer_settings: &TimerSettings,
) -> bool {
    // Check day-night timer not enabled
    if !timer_settings.enabled {
        return false;
    }

    // Is the current day of the week enabled
    let is_today_weekday_enabled =
        (timer_settings.weekdays_bitmask & (1 << local_weekday_number)) != 0;
    let is_yesterday_weekday_enabled =
        (timer_settings.weekdays_bitmask & (1 << ((local_weekday_number + 6) % 7))) != 0;

    // Special case: End time is less than start time, meaning the active period wraps to the next day (e.g. 6pm-6am)
    let does_end_time_wrap_next_day =
        timer_settings.end_time_of_day_seconds <= timer_settings.start_time_of_day_seconds;

    // Current time is within active period today
    let is_normal_active = !does_end_time_wrap_next_day
        && is_today_weekday_enabled
        && local_time_of_day_seconds >= timer_settings.start_time_of_day_seconds
        && local_time_of_day_seconds < timer_settings.end_time_of_day_seconds;

    // Current time is within active period that wraps to the next day
    let is_wrapping_active = does_end_time_wrap_next_day
        && ((is_today_weekday_enabled
            && local_time_of_day_seconds >= timer_settings.start_time_of_day_seconds)
            || (is_yesterday_weekday_enabled
                && local_time_of_day_seconds < timer_settings.end_time_of_day_seconds));

    is_normal_active || is_wrapping_active
}

// Given the current local time, sunrise/sunset times and day/night brightness levels, calculate the brightness level approximated by the the sun angle
fn calc_brightness_percent_from_sunlight(
    local_time_of_day_seconds: u32,
    local_sunrise_time_of_day_seconds: u32,
    local_sunset_time_of_day_seconds: u32,
    day_brightness_percent: u8,
    night_brightness_percent: u8,
) -> u8 {
    let does_sunset_wrap_next_day =
        local_sunset_time_of_day_seconds <= local_sunrise_time_of_day_seconds;
    let is_day_time = if does_sunset_wrap_next_day {
        local_time_of_day_seconds >= local_sunrise_time_of_day_seconds
            || local_time_of_day_seconds < local_sunset_time_of_day_seconds
    } else {
        local_time_of_day_seconds >= local_sunrise_time_of_day_seconds
            && local_time_of_day_seconds < local_sunset_time_of_day_seconds
    };
    if !is_day_time {
        return night_brightness_percent;
    }

    let day_length_seconds = if does_sunset_wrap_next_day {
        86400 - local_sunrise_time_of_day_seconds + local_sunset_time_of_day_seconds
    } else {
        local_sunset_time_of_day_seconds - local_sunrise_time_of_day_seconds
    };
    let seconds_since_sunrise = if local_time_of_day_seconds >= local_sunrise_time_of_day_seconds {
        local_time_of_day_seconds - local_sunrise_time_of_day_seconds
    } else {
        86400 - local_sunrise_time_of_day_seconds + local_time_of_day_seconds
    };
    let sun_path_unit = (seconds_since_sunrise as f32) / (day_length_seconds as f32);
    let sun_angle_unit = 4.0 * sun_path_unit * (1.0 - sun_path_unit); // cheap approximation of sin(pi*x)
    let brightness_percent = sun_angle_unit
        * ((day_brightness_percent - night_brightness_percent) as f32)
        + (night_brightness_percent as f32);
    brightness_percent as u8
}

async fn drive_day_night_timer_light_state() {
    let session_settings = app_settings::session::get_settings().await;
    let persist_settings = app_settings::persist::get_settings().await;

    if !session_settings.is_time_synced {
        return; // Wait for server time sync first
    }

    let local_time_of_day_seconds = time::get_local_seconds_since_midnight().await;
    let local_weekday_number = time::get_local_weekday_number().await;
    let is_timer_driving_on = is_day_night_timer_driving_light_on(
        local_time_of_day_seconds,
        local_weekday_number,
        &persist_settings.config.timer_settings,
    );
    let timer_enabled = persist_settings.config.timer_settings.enabled;

    // Clear manual light on/off override if day-night timer state matches override or timer not enabled
    if let Some(light_on_override) = session_settings.light_on_override
        && (light_on_override == is_timer_driving_on || !timer_enabled)
    {
        app_settings::session::update_settings_changed(|set| {
            set.light_on_override = None;
        })
        .await;
    }

    // Drive light on/off state from day-night timer if enabled and no override is set
    if timer_enabled
        && session_settings.light_on != is_timer_driving_on
        && session_settings.light_on_override.is_none()
    {
        info!(
            "Day-night timer changing light on/off state to {}",
            is_timer_driving_on
        );
        if !is_timer_driving_on {
            leds::set_status(LedStatus::TimerOff);
            leds::set_pixels(leds::LedPixels::FadeOut).await;
            leds::wait_pixels_animation_complete().await;
        } else {
            leds::set_status(LedStatus::Ok);
        }
        if app_settings::session::update_settings_changed(|set| {
            set.light_on = is_timer_driving_on;
            set.night_timer_active = !is_timer_driving_on;
        })
        .await
        {
            ws_client::send_status();
        }
    }
}

async fn drive_sunlight_auto_brightness() {
    let session_settings = app_settings::session::get_settings().await;
    let persist_settings = app_settings::persist::get_settings().await;

    if !session_settings.is_time_synced {
        return; // Wait for server time sync first
    }

    let local_time_of_day_seconds = time::get_local_seconds_since_midnight().await;
    let local_sunrise_time_of_day_seconds = session_settings.local_sunrise_time_of_day_seconds;
    let local_sunset_time_of_day_seconds = session_settings.local_sunset_time_of_day_seconds;
    let day_brightness_percent = persist_settings
        .config
        .sunlight_auto_brightness
        .day_brightness_percent as u8;
    let night_brightness_percent = persist_settings
        .config
        .sunlight_auto_brightness
        .night_brightness_percent as u8;
    let auto_brightness_enabled = persist_settings.config.sunlight_auto_brightness.enabled;

    let brightness_percent_opt = if auto_brightness_enabled {
        Some(calc_brightness_percent_from_sunlight(
            local_time_of_day_seconds,
            local_sunrise_time_of_day_seconds,
            local_sunset_time_of_day_seconds,
            day_brightness_percent,
            night_brightness_percent,
        ))
    } else {
        None
    };

    // Update session brightness percent if changed
    if brightness_percent_opt != session_settings.auto_brightness_percent
        && app_settings::session::update_settings_changed(|set| {
            set.auto_brightness_percent = brightness_percent_opt;
        })
        .await
    {
        ws_client::send_status();
    }
}

pub async fn step() {
    drive_day_night_timer_light_state().await;
    drive_sunlight_auto_brightness().await;
}
