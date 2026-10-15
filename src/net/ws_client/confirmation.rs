// Commands from the gateway that wipe the map's setup (factory reset,
// reprovision) or lock the server out (disabling remote control) only run once
// the owner confirms them at the map, with a short press on any button in time
// (see ui). A compromised gateway or account can't do that on its own
use defmt::info;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, with_timeout};

use super::{client_proto::DeviceCommand, incoming, send_status, send_telemetry};
use crate::{
    display::leds,
    store::app_settings::{self, session::PendingConfirmation},
    time,
};

const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(30);

static REQUEST_SIGNAL: Signal<CriticalSectionRawMutex, DeviceCommand> = Signal::new();
// True when confirmed, false when canceled
static ANSWER_SIGNAL: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub fn spawn(spawner: Spawner) {
    spawner.spawn(confirmation_task().unwrap());
}

/// Runs the command once confirmed. Replaces a command still awaiting
/// confirmation, with a new deadline.
pub fn request(command: DeviceCommand) {
    REQUEST_SIGNAL.signal(command);
}

/// Confirms the command awaiting confirmation. False if none is.
pub async fn confirm_command() -> bool {
    answer(true).await
}

/// Drops the command awaiting confirmation, if any: the server cleared it, or
/// it came over a connection that ended.
pub async fn cancel() {
    answer(false).await;
}

async fn answer(confirmed: bool) -> bool {
    let is_pending = app_settings::session::get_settings()
        .await
        .pending_confirmation
        .is_some();
    if is_pending {
        ANSWER_SIGNAL.signal(confirmed);
    }
    is_pending
}

#[embassy_executor::task]
async fn confirmation_task() {
    loop {
        let mut command = REQUEST_SIGNAL.wait().await;
        let confirmed = loop {
            // An answer to an earlier request (given just as it expired)
            // doesn't count for this one
            ANSWER_SIGNAL.reset();
            let deadline_unix_timestamp =
                time::get_unix_timestamp_seconds().await + CONFIRMATION_TIMEOUT.as_secs() as u32;
            set_pending(Some(PendingConfirmation {
                command,
                deadline_unix_timestamp,
            }))
            .await;
            info!(
                "Command {:?} awaiting confirmation with a button press",
                command as i32
            );

            match select(
                with_timeout(CONFIRMATION_TIMEOUT, ANSWER_SIGNAL.wait()),
                REQUEST_SIGNAL.wait(),
            )
            .await
            {
                Either::First(answer) => break answer.unwrap_or(false),
                Either::Second(next_command) => command = next_command,
            }
        };

        set_pending(None).await;
        if confirmed {
            info!("Command {:?} confirmed", command as i32);
            incoming::on_command_confirmed(command).await;
        } else {
            info!("Command {:?} not confirmed, dropped", command as i32);
        }
    }
}

async fn set_pending(pending_confirmation: Option<PendingConfirmation>) {
    let changed = app_settings::session::update_settings_changed(|set| {
        set.pending_confirmation = pending_confirmation;
    })
    .await;
    if changed {
        leds::set_status_led_from_session().await;
        send_status();
        send_telemetry();
    }
}
