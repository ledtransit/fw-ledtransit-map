// Day-night timer and sunlight auto brightness automation logic
use defmt::info;

use crate::{
    display::leds::{self, LedPixels, LedStatus},
    net::ws_client::{self, client_proto::TimerSettings},
    store::app_settings,
    time,
};

const SECONDS_PER_DAY: u32 = 86400;

pub async fn step() {
    drive_day_night_timer_light_state().await;
    drive_sunlight_auto_brightness().await;
}

async fn drive_day_night_timer_light_state() {
    let session_settings = app_settings::session::get_settings().await;
    let persist_settings = app_settings::persist::get_settings().await;
    if !session_settings.is_time_synced {
        return;
    }

    let timer_settings = &persist_settings.config.timer_settings;
    let is_timer_driving_on = is_day_night_timer_driving_light_on(
        time::get_local_seconds_since_midnight().await,
        time::get_local_weekday_number().await,
        timer_settings,
    );

    // A manual light override ends once the timer agrees with it, or without a timer
    if let Some(light_on_override) = session_settings.light_on_override
        && (light_on_override == is_timer_driving_on || !timer_settings.enabled)
    {
        app_settings::session::update_settings_changed(|set| {
            set.light_on_override = None;
        })
        .await;
    }

    if !timer_settings.enabled
        || session_settings.light_on == is_timer_driving_on
        || session_settings.light_on_override.is_some()
    {
        return;
    }

    info!(
        "Day-night timer changing light on/off state to {}",
        is_timer_driving_on
    );
    if is_timer_driving_on {
        leds::set_status(LedStatus::Ok);
    } else {
        leds::set_status(LedStatus::TimerOff);
        leds::set_pixels(LedPixels::FadeOut).await;
        leds::wait_pixels_animation_complete().await;
    }
    let changed = app_settings::session::update_settings_changed(|set| {
        set.light_on = is_timer_driving_on;
        set.night_timer_active = !is_timer_driving_on;
    })
    .await;
    if changed {
        ws_client::send_status();
    }
}

async fn drive_sunlight_auto_brightness() {
    let session_settings = app_settings::session::get_settings().await;
    let persist_settings = app_settings::persist::get_settings().await;
    if !session_settings.is_time_synced {
        return;
    }

    let local_time_of_day_seconds = time::get_local_seconds_since_midnight().await;
    let auto_brightness = &persist_settings.config.sunlight_auto_brightness;
    let brightness_percent = auto_brightness.enabled.then(|| {
        calc_brightness_percent_from_sunlight(
            local_time_of_day_seconds,
            session_settings.local_sunrise_time_of_day_seconds,
            session_settings.local_sunset_time_of_day_seconds,
            auto_brightness.day_brightness_percent as u8,
            auto_brightness.night_brightness_percent as u8,
        )
    });

    if brightness_percent != session_settings.auto_brightness_percent
        && app_settings::session::update_settings_changed(|set| {
            set.auto_brightness_percent = brightness_percent;
        })
        .await
    {
        ws_client::send_status();
    }
}

fn is_day_night_timer_driving_light_on(
    local_time_of_day_seconds: u32,
    local_weekday_number: u32,
    timer_settings: &TimerSettings,
) -> bool {
    if !timer_settings.enabled {
        return false;
    }

    let is_weekday_enabled = |weekday: u32| (timer_settings.weekdays_bitmask & (1 << weekday)) != 0;
    let is_today_enabled = is_weekday_enabled(local_weekday_number);
    let is_yesterday_enabled = is_weekday_enabled((local_weekday_number + 6) % 7);
    let start = timer_settings.start_time_of_day_seconds;
    let end = timer_settings.end_time_of_day_seconds;

    if end > start {
        is_today_enabled && local_time_of_day_seconds >= start && local_time_of_day_seconds < end
    } else {
        // The active period wraps to the next day (e.g. 6pm-6am)
        (is_today_enabled && local_time_of_day_seconds >= start)
            || (is_yesterday_enabled && local_time_of_day_seconds < end)
    }
}

/// Brightness between night and day brightness, following the sun's height
/// between sunrise and sunset.
fn calc_brightness_percent_from_sunlight(
    local_time_of_day_seconds: u32,
    local_sunrise_time_of_day_seconds: u32,
    local_sunset_time_of_day_seconds: u32,
    day_brightness_percent: u8,
    night_brightness_percent: u8,
) -> u8 {
    let sunrise = local_sunrise_time_of_day_seconds;
    let sunset = local_sunset_time_of_day_seconds;
    let now = local_time_of_day_seconds;

    let does_sunset_wrap_next_day = sunset <= sunrise;
    let is_day_time = if does_sunset_wrap_next_day {
        now >= sunrise || now < sunset
    } else {
        now >= sunrise && now < sunset
    };
    if !is_day_time {
        return night_brightness_percent;
    }

    let day_length_seconds = if does_sunset_wrap_next_day {
        SECONDS_PER_DAY - sunrise + sunset
    } else {
        sunset - sunrise
    };
    let seconds_since_sunrise = if now >= sunrise {
        now - sunrise
    } else {
        SECONDS_PER_DAY - sunrise + now
    };
    let sun_path_unit = (seconds_since_sunrise as f32) / (day_length_seconds as f32);
    let sun_angle_unit = 4.0 * sun_path_unit * (1.0 - sun_path_unit); // Cheap approximation of sin(pi*x)
    let brightness_percent = sun_angle_unit
        * ((day_brightness_percent - night_brightness_percent) as f32)
        + (night_brightness_percent as f32);
    brightness_percent as u8
}
