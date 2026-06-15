use embassy_time::Instant;

use crate::app_settings;

/// Get the current Unix timestamp in seconds (since epoch 1970-01-01T00:00:00Z) based on server-synchronized world time.
pub async fn get_unix_timestamp_seconds() -> u32 {
    let unix_epoch_offset_secs = app_settings::session::get_settings()
        .await
        .unix_epoch_offset_secs;
    let uptime_secs = Instant::now().as_secs() as u32;
    unix_epoch_offset_secs + uptime_secs
}

/// Get the current seconds elapsed since local midnight based on the user's configured timezone and server-synchronized world time.
pub async fn get_local_seconds_since_midnight() -> u32 {
    let local_time_of_day_offset_secs = app_settings::session::get_settings()
        .await
        .local_time_of_day_offset_secs;
    let uptime_secs = Instant::now().as_secs() as u32;
    (local_time_of_day_offset_secs + uptime_secs) % 86400
}

/// Get today's weekday number (0=Sunday, 6=Saturday) based on local time of the user's configured timezone and server-synchronized world time.
pub async fn get_local_weekday_number() -> u32 {
    let settings = app_settings::session::get_settings().await;
    let local_weekday_number = settings.local_weekday_number;
    let local_time_of_day_seconds = get_local_seconds_since_midnight().await;
    // Once local time of day seconds exceeds 24 hours, weekday number must be incremented
    let days_passed = (local_time_of_day_seconds / 86400) as u32;
    (local_weekday_number + days_passed) % 7
}
