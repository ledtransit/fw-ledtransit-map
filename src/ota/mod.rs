// Secure over-the-air firmware updates (see docs/SECURE_OTA.md)
// Updates are offered over the WebSocket and downloaded over HTTPS into the
// inactive bank. Their metadata is signed (NIST P-256), the image is checked
// against the signed SHA-256 read back from flash, and a new image is only
// kept once it passed the boot check.
mod boot;
mod download;
mod verify;

pub use boot::confirm_boot;

use defmt::info;
use edge_http::io::Error as HttpError;
use embassy_executor::Spawner;
use embassy_futures::select::{Either3, select3};
use embassy_net::{Stack, dns};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, TimeoutError, Timer, with_timeout};
use embedded_io_async::ReadExactError;
use esp_bootloader_esp_idf::partitions;
use esp_hal::{peripherals::SHA, sha::Sha};
use mbedtls_rs::{Certificate, SessionError, Tls};

use crate::{
    display::leds::{self, LedStatus},
    net::ws_client::{self, WsClientError, client_proto::DeviceUpdate},
    store::{SharedFlashStorage, app_settings, transit_data},
    time, trace,
};

// Senders cancel a running scheduled wait first (see signal): it then ends
// without starting its update, which would replace their event
static OTA_SIGNAL: Signal<CriticalSectionRawMutex, OtaEvent> = Signal::new();
static OTA_CANCEL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
// Separate from the events: must not be replaced by one
static OTA_INIT_BOOT_PARTITION: Signal<CriticalSectionRawMutex, ()> = Signal::new();

enum OtaEvent {
    BootFromFactory,
    StartUpdate(DeviceUpdate),
    ScheduleUpdate(DeviceUpdate, Duration),
}

// The download's TLS session needs 33 KB while downloading and 42 KB at its peak
// during the handshake (measured), plus margin for fragmentation
const MIN_FREE_HEAP_FOR_DOWNLOAD: usize = 48 * 1024;

#[derive(defmt::Format, Debug)]
pub enum OtaError {
    UrlError,
    DnsError(dns::Error),
    HttpError(HttpError<SessionError>),
    HttpReadError(ReadExactError<HttpError<SessionError>>),
    Timeout(TimeoutError),
    StatusCodeError(u16),
    HeaderMissingError,
    FlashWriteError(partitions::Error),
    FlashReadError(partitions::Error),
    HashMismatchError,
    SizeMismatchError,
    SignatureMalformedError,
    SignatureVerificationError,
}

pub fn spawn(
    spawner: Spawner,
    sta_stack: Stack<'static>,
    tls: &'static Tls<'static>,
    ca_cert: &'static Certificate<'static>,
    flash_store: &'static SharedFlashStorage,
    sha_peri: SHA<'static>,
) {
    spawner.spawn(ota_task(sta_stack, tls, ca_cert, flash_store, sha_peri).unwrap());
    spawner.spawn(boot::boot_check_timeout_task(flash_store).unwrap());
}

pub async fn start_firmware_update(update: &DeviceUpdate) {
    if app_settings::session::get_settings()
        .await
        .updating_firmware
    {
        return;
    }
    signal(OtaEvent::StartUpdate(update.clone()));
}

pub async fn schedule_firmware_update(update: &DeviceUpdate, delay: Duration) {
    let settings = app_settings::session::get_settings().await;
    if settings.updating_firmware || settings.auto_update_scheduled_unix_timestamp.is_some() {
        return;
    }
    signal(OtaEvent::ScheduleUpdate(update.clone(), delay));
}

pub fn init_boot_partition() {
    OTA_INIT_BOOT_PARTITION.signal(());
}

pub fn boot_from_factory() {
    signal(OtaEvent::BootFromFactory);
}

pub fn cancel() {
    OTA_CANCEL.signal(());
}

fn signal(event: OtaEvent) {
    cancel();
    OTA_SIGNAL.signal(event);
}

#[embassy_executor::task]
async fn ota_task(
    sta_stack: Stack<'static>,
    tls: &'static Tls<'static>,
    ca_cert: &'static Certificate<'static>,
    flash_store: &'static SharedFlashStorage,
    sha_peri: SHA<'static>,
) {
    let mut sha = Sha::new(sha_peri);

    // Events are handled from the first connection on
    sta_stack.wait_link_up().await;
    sta_stack.wait_config_up().await;

    loop {
        // Confirming first: when an update arrives right after connecting, the
        // running image is confirmed before it's replaced
        let event = match select3(
            boot::OTA_CONFIRM.wait(),
            OTA_INIT_BOOT_PARTITION.wait(),
            OTA_SIGNAL.wait(),
        )
        .await
        {
            Either3::First(()) => {
                boot::confirm_current_partition(flash_store).await;
                continue;
            }
            Either3::Second(()) => {
                boot::init_boot_partition(flash_store).await;
                continue;
            }
            Either3::Third(event) => event,
        };
        OTA_CANCEL.reset();

        match event {
            OtaEvent::BootFromFactory => {
                boot::set_factory_boot_partition(flash_store).await;
                reboot().await;
            }
            OtaEvent::ScheduleUpdate(update, delay) => schedule_update(update, delay).await,
            OtaEvent::StartUpdate(update) => {
                install_update(&update, sta_stack, tls, ca_cert, flash_store, &mut sha).await
            }
        }
    }
}

async fn schedule_update(update: DeviceUpdate, delay: Duration) {
    info!(
        "Scheduling OTA update to v{}.{}.{} in {} seconds",
        update.firmware_version_major,
        update.firmware_version_minor,
        update.firmware_version_patch,
        delay.as_secs()
    );
    let now_unix = time::get_unix_timestamp_seconds().await;
    app_settings::session::update_settings(|set| {
        set.auto_update_scheduled_unix_timestamp = Some(now_unix + delay.as_secs() as u32);
    })
    .await;
    ws_client::send_telemetry();

    if with_timeout(delay, OTA_CANCEL.wait()).await.is_ok() {
        info!("OTA update: scheduled delay canceled, aborting update");
        app_settings::session::update_settings(|set| {
            set.auto_update_scheduled_unix_timestamp = None;
        })
        .await;
        ws_client::send_telemetry();
        return;
    }

    info!("OTA update: scheduled delay elapsed, starting update");
    // No longer scheduled, also if the update fails: it's scheduled again
    // when offered again
    app_settings::session::update_settings(|set| {
        set.auto_update_scheduled_unix_timestamp = None;
    })
    .await;
    ws_client::send_telemetry();
    OTA_SIGNAL.signal(OtaEvent::StartUpdate(update));
}

async fn install_update(
    update: &DeviceUpdate,
    sta_stack: Stack<'static>,
    tls: &Tls<'static>,
    ca_cert: &Certificate<'static>,
    flash_store: &'static SharedFlashStorage,
    sha: &mut Sha<'_>,
) {
    if !verify::is_newer_than_installed(update) {
        trace::wrn!(
            "OTA update refused: v{}.{}.{} isn't newer than installed",
            update.firmware_version_major,
            update.firmware_version_minor,
            update.firmware_version_patch
        );
        return;
    }

    if update.size_bytes as usize > download::OTA_PARTITION_SIZE {
        trace::err!(
            "OTA update size {} exceeds partition size {}",
            update.size_bytes,
            download::OTA_PARTITION_SIZE
        );
        return;
    }

    // Before downloading: only signed metadata gets to overwrite the other bank
    // (the previous firmware)
    if let Err(e) = verify::verify_signature(update) {
        trace::err!("OTA update refused, signature invalid: {:?}", e);
        on_update_failed().await;
        return;
    }

    info!(
        "Starting OTA update to v{}.{}.{} ({} bytes)",
        update.firmware_version_major,
        update.firmware_version_minor,
        update.firmware_version_patch,
        update.size_bytes
    );
    app_settings::session::update_settings(|set| {
        set.updating_firmware = true;
        set.update_progress_percent = 0;
        set.update_speed_bytes_per_sec = 0;
    })
    .await;
    ws_client::send_status();
    ws_client::send_telemetry();
    leds::set_status(LedStatus::UpdatingFirmware);
    Timer::after(Duration::from_secs(2)).await;
    free_heap_for_download().await;

    match download::download_to_flash(update, sta_stack, tls, ca_cert, flash_store, sha).await {
        Ok(()) => {
            boot::activate_next_partition(flash_store).await;
            reboot().await;
        }
        Err(e) => {
            trace::err!("OTA update failed: {:?}", e);
            on_update_failed().await;
        }
    }
}

// Frees the transit data only if the download wouldn't fit next to it (new
// transit data is ignored while updating). The map then stays blank until the
// reboot, or the next transit data after a failed update.
async fn free_heap_for_download() {
    let free_heap = esp_alloc::HEAP.free();
    info!("OTA update: {} bytes of heap free", free_heap);
    if free_heap < MIN_FREE_HEAP_FOR_DOWNLOAD {
        trace::wrn!(
            "OTA update: only {} bytes of heap free, freeing transit data",
            free_heap
        );
        transit_data::clear().await;
    }
}

async fn on_update_failed() {
    app_settings::session::update_settings(|set| {
        set.updating_firmware = false;
        set.update_has_failed = true;
    })
    .await;
    ws_client::send_status();
    ws_client::send_telemetry();
    leds::set_status(LedStatus::UpdateFailed);
}

// Lets the WebSocket client close the connection first, then resets anyway
async fn reboot() -> ! {
    ws_client::quit(Err(WsClientError::Reboot));
    Timer::after(Duration::from_secs(2)).await;
    esp_hal::system::software_reset();
}
