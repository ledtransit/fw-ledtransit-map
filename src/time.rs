// World time: the uptime plus an offset, measured with the gateway (see
// TimeSync). Once measured, corrections are slewed: the clock doesn't jump, so
// animations timed by it stay smooth, and in sync across maps
use core::cell::Cell;

use defmt::info;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::Instant;

use crate::{
    net::ws_client::client_proto::{ServerInfo, TimeSync},
    stats::Counter,
    trace,
};

const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86400;
const UNIX_EPOCH_WEEKDAY_NUMBER: i64 = 4; // Thursday
// Slewing runs the clock up to 1 % faster or slower
const SLEW_RATE_DIVISOR: i64 = 100;
// Larger corrections step the clock instead of slewing it for minutes
const MAX_SLEW_MICROS: i64 = MICROS_PER_SECOND;
// Crystal tolerance: how fast a measurement's uncertainty grows with its age
const DRIFT_PPM: i64 = 25;

// Since boot, sent with the telemetry
pub static SYNC_REQUESTS: Counter = Counter::new();
static SYNC_REPLIES: Counter = Counter::new();
static SYNC_BURSTS: Counter = Counter::new();
static SYNC_CORRECTIONS: Counter = Counter::new();
static SYNC_STEPS: Counter = Counter::new();

// Of a TimeSync, in microseconds: measured_at by uptime
#[derive(Clone, Copy)]
struct Measurement {
    offset: i64,
    round_trip: i64,
    measured_at: i64,
}

impl Measurement {
    fn uncertainty_at(&self, uptime: i64) -> i64 {
        self.round_trip / 2 + (uptime - self.measured_at) * DRIFT_PPM / MICROS_PER_SECOND
    }
}

#[derive(Clone, Copy)]
struct Clock {
    // Unix minus uptime, in microseconds: offset at slew_start, slewing to
    // target_offset
    offset: i64,
    target_offset: i64,
    slew_start: i64,
    followed: Option<Measurement>,
    // Of the burst of TimeSyncs in progress, the most accurate
    burst_best: Option<Measurement>,
    last_burst_round_trip: Option<i64>,
    correction: i64,
    utc_offset_seconds: Option<i32>,
}

impl Clock {
    const fn new() -> Self {
        Self {
            offset: 0,
            target_offset: 0,
            slew_start: 0,
            followed: None,
            burst_best: None,
            last_burst_round_trip: None,
            correction: 0,
            utc_offset_seconds: None,
        }
    }

    fn offset_at(&self, uptime: i64) -> i64 {
        let max_slew = (uptime - self.slew_start).max(0) / SLEW_RATE_DIVISOR;
        self.offset + (self.target_offset - self.offset).clamp(-max_slew, max_slew)
    }

    fn unix_micros(&self) -> i64 {
        let uptime = uptime_micros();
        uptime + self.offset_at(uptime)
    }

    fn step(&mut self, offset: i64) {
        self.offset = offset;
        self.target_offset = offset;
    }

    /// The first measurement steps the clock, later ones slew it. Returns the
    /// correction.
    fn follow(&mut self, measurement: Measurement, uptime: i64) -> i64 {
        let correction = measurement.offset - self.offset_at(uptime);
        if self.followed.is_some() && correction.abs() <= MAX_SLEW_MICROS {
            self.offset = self.offset_at(uptime);
            self.target_offset = measurement.offset;
            self.slew_start = uptime;
        } else {
            self.step(measurement.offset);
            SYNC_STEPS.inc();
        }
        SYNC_CORRECTIONS.inc();
        self.followed = Some(measurement);
        self.correction = correction;
        correction
    }
}

static CLOCK: Mutex<CriticalSectionRawMutex, Cell<Clock>> = Mutex::new(Cell::new(Clock::new()));

fn uptime_micros() -> i64 {
    Instant::now().as_micros() as i64
}

fn clock() -> Clock {
    CLOCK.lock(Cell::get)
}

fn update_clock<R>(f: impl FnOnce(&mut Clock) -> R) -> R {
    CLOCK.lock(|cell| {
        let mut clock = cell.get();
        let result = f(&mut clock);
        cell.set(clock);
        result
    })
}

/// Unix time in milliseconds. The uptime until synchronized.
pub fn unix_millis() -> u64 {
    (clock().unix_micros() / 1000) as u64
}

/// Unix time in seconds. The uptime until synchronized.
pub fn unix_seconds() -> u32 {
    (clock().unix_micros() / MICROS_PER_SECOND) as u32
}

/// When the clock shows the Unix time.
pub fn instant_at_unix_millis(unix_millis: u64) -> Instant {
    let clock = clock();
    let unix_micros = unix_millis as i64 * 1000;
    // Slewing changes the offset slowly: two steps converge, up to rounding
    let mut uptime = unix_micros - clock.offset_at(uptime_micros());
    uptime = unix_micros - clock.offset_at(uptime);
    while uptime + clock.offset_at(uptime) < unix_micros {
        uptime += 1;
    }
    Instant::from_micros(uptime.max(0) as u64)
}

pub struct LocalTime {
    pub seconds_since_midnight: u32,
    pub weekday_number: u32, // 0 = Sunday
}

/// In the user's timezone, once the server sent it.
pub fn local_time() -> Option<LocalTime> {
    let clock = clock();
    let local_seconds =
        clock.unix_micros() / MICROS_PER_SECOND + i64::from(clock.utc_offset_seconds?);
    Some(LocalTime {
        seconds_since_midnight: local_seconds.rem_euclid(SECONDS_PER_DAY) as u32,
        weekday_number: (local_seconds.div_euclid(SECONDS_PER_DAY) + UNIX_EPOCH_WEEKDAY_NUMBER)
            .rem_euclid(7) as u32,
    })
}

pub struct SyncStats {
    pub requests: u32,
    pub replies: u32,
    pub bursts: u32,
    pub corrections: u32,
    pub steps: u32,
    pub last_burst_round_trip_micros: Option<u32>,
    pub followed: Option<FollowedSyncStats>,
}

/// Of the TimeSync the clock follows.
pub struct FollowedSyncStats {
    pub round_trip_micros: u32,
    pub correction_micros: i32,
    pub measured_unix_timestamp: u32,
    pub uncertainty_micros: u32,
    pub slew_remaining_micros: i32,
}

pub fn sync_stats() -> SyncStats {
    let clock = clock();
    let uptime = uptime_micros();
    let to_u32 = |value: i64| value.clamp(0, u32::MAX.into()) as u32;
    let to_i32 = |value: i64| value.clamp(i32::MIN.into(), i32::MAX.into()) as i32;
    SyncStats {
        requests: SYNC_REQUESTS.get() as u32,
        replies: SYNC_REPLIES.get() as u32,
        bursts: SYNC_BURSTS.get() as u32,
        corrections: SYNC_CORRECTIONS.get() as u32,
        steps: SYNC_STEPS.get() as u32,
        last_burst_round_trip_micros: clock.last_burst_round_trip.map(to_u32),
        followed: clock.followed.map(|followed| FollowedSyncStats {
            round_trip_micros: to_u32(followed.round_trip),
            correction_micros: to_i32(clock.correction),
            measured_unix_timestamp: to_u32(
                (followed.measured_at + followed.offset) / MICROS_PER_SECOND,
            ),
            uncertainty_micros: to_u32(followed.uncertainty_at(uptime)),
            slew_remaining_micros: to_i32(clock.target_offset - clock.offset_at(uptime)),
        }),
    }
}

/// Takes the timezone, and the time until a TimeSync measured it.
pub fn on_server_info(server_info: &ServerInfo, received_at: Instant) {
    let unix_seconds = i64::from(server_info.unix_timestamp);
    let utc_seconds_since_midnight = unix_seconds.rem_euclid(SECONDS_PER_DAY);
    let utc_weekday_number =
        (unix_seconds.div_euclid(SECONDS_PER_DAY) + UNIX_EPOCH_WEEKDAY_NUMBER).rem_euclid(7);
    // The weekdays tell whether the local day is UTC's, or the one before or after
    let days_ahead =
        match (i64::from(server_info.local_weekday_number) - utc_weekday_number).rem_euclid(7) {
            1 => 1,
            6 => -1,
            _ => 0,
        };
    let utc_offset_seconds = days_ahead * SECONDS_PER_DAY
        + i64::from(server_info.local_time_of_day_seconds)
        - utc_seconds_since_midnight;

    update_clock(|clock| {
        clock.utc_offset_seconds = Some(utc_offset_seconds as i32);
        if clock.followed.is_none() {
            // Halfway into the (truncated) second
            let unix_micros = unix_seconds * MICROS_PER_SECOND + MICROS_PER_SECOND / 2;
            clock.step(unix_micros - received_at.as_micros() as i64);
        }
    });
}

/// The first one is followed right away. Later ones are collected: the most
/// accurate of a burst is followed once it's finished.
pub fn on_time_sync(time_sync: &TimeSync, received_at: Instant) {
    let (Some(server_received), Some(server_sent)) = (
        time_sync.server_receive_unix_timestamp_micros,
        time_sync.server_send_unix_timestamp_micros,
    ) else {
        trace::wrn!("Time sync: Server times missing");
        return;
    };
    let sent = time_sync.client_send_micros_since_boot as i64;
    let received = received_at.as_micros() as i64;
    let (server_received, server_sent) = (server_received as i64, server_sent as i64);
    let round_trip = (received - sent) - (server_sent - server_received);
    if sent > received || round_trip < 0 {
        trace::wrn!("Time sync: Invalid round trip");
        return;
    }
    SYNC_REPLIES.inc();
    let measurement = Measurement {
        offset: ((server_received - sent) + (server_sent - received)) / 2,
        round_trip,
        measured_at: received,
    };

    let uptime = uptime_micros();
    let correction = update_clock(|clock| {
        if clock.followed.is_none() {
            return Some(clock.follow(measurement, uptime));
        }
        if clock
            .burst_best
            .is_none_or(|best| measurement.round_trip < best.round_trip)
        {
            clock.burst_best = Some(measurement);
        }
        None
    });
    match correction {
        Some(correction) => info!(
            "Time sync: Corrected by {} us (round trip {} us)",
            correction, round_trip
        ),
        None => info!("Time sync: Round trip {} us", round_trip),
    }
}

/// Follows the burst's most accurate measurement, unless the one followed is
/// still more accurate.
pub fn finish_sync_burst() {
    SYNC_BURSTS.inc();
    let uptime = uptime_micros();
    let result = update_clock(|clock| {
        let best = clock.burst_best.take()?;
        clock.last_burst_round_trip = Some(best.round_trip);
        let is_more_accurate = clock
            .followed
            .is_none_or(|followed| best.uncertainty_at(uptime) <= followed.uncertainty_at(uptime));
        Some((
            best.round_trip,
            is_more_accurate.then(|| clock.follow(best, uptime)),
        ))
    });
    match result {
        Some((round_trip, Some(correction))) => info!(
            "Time sync: Corrected by {} us (best round trip {} us)",
            correction, round_trip
        ),
        Some((round_trip, None)) => info!(
            "Time sync: Kept, more accurate (best round trip {} us)",
            round_trip
        ),
        None => info!("Time sync: No replies"),
    }
}
