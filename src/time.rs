// World time, synchronized with the server as offsets to the uptime. The
// offsets wrap around (an offset to the local time of day is "negative" once the
// uptime exceeds it), so they're applied with wrapping arithmetic
use embassy_time::Instant;

use crate::store::app_settings;

const SECONDS_PER_DAY: u32 = 86400;

fn uptime_secs() -> u32 {
    Instant::now().as_secs() as u32
}

pub async fn get_unix_timestamp_seconds() -> u32 {
    let unix_epoch_offset_secs = app_settings::session::get_settings()
        .await
        .unix_epoch_offset_secs;
    unix_epoch_offset_secs.wrapping_add(uptime_secs())
}

// Seconds since local midnight of the day the time was last synchronized
async fn local_seconds_since_sync_midnight() -> u32 {
    let local_time_of_day_offset_secs = app_settings::session::get_settings()
        .await
        .local_time_of_day_offset_secs;
    local_time_of_day_offset_secs.wrapping_add(uptime_secs())
}

/// Seconds since midnight in the user's configured timezone.
pub async fn get_local_seconds_since_midnight() -> u32 {
    local_seconds_since_sync_midnight().await % SECONDS_PER_DAY
}

/// Today's weekday in the user's configured timezone (0 = Sunday).
pub async fn get_local_weekday_number() -> u32 {
    let local_weekday_number = app_settings::session::get_settings()
        .await
        .local_weekday_number;
    // Midnights passed since the last synchronization
    let days_passed = local_seconds_since_sync_midnight().await / SECONDS_PER_DAY;
    (local_weekday_number + days_passed) % 7
}
