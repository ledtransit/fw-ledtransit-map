// Boot partition state: rollback of new images until they pass the boot check
// (ESP-IDF bootloader app rollback), and the switch to the factory image
use core::{
    ops::DerefMut,
    sync::atomic::{AtomicBool, Ordering},
};

use defmt::{error, info};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer};
use esp_bootloader_esp_idf::{
    ota::OtaImageState, ota_updater::OtaUpdater, partitions::AppPartitionSubType,
    partitions::PARTITION_TABLE_MAX_LEN,
};
use esp_storage::FlashStorage;

use crate::store::{SharedFlashStorage, app_settings};

/// A new image must pass the boot check within this time after booting, or
/// the device reboots, which makes the bootloader roll back
const BOOT_CHECK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

type OtaFlashUpdater<'a> = OtaUpdater<'a, FlashStorage<'static>>;

// Separate from the update events: must not replace a pending one, nor be replaced
pub(super) static OTA_CONFIRM: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static BOOT_CONFIRMED: AtomicBool = AtomicBool::new(false);

/// The boot check passed: the firmware booted, connected to WiFi,
/// authenticated with the gateway, and received, decoded and processed
/// transit data. A newly installed image is kept from now on.
pub fn confirm_boot() {
    // Load and store only: no atomic read-modify-write on this target, and
    // only the WebSocket task calls this
    if !BOOT_CONFIRMED.load(Ordering::Relaxed) {
        BOOT_CONFIRMED.store(true, Ordering::Relaxed);
        OTA_CONFIRM.signal(());
    }
}

/// Reboots an image that doesn't pass the boot check in time, e.g. because it
/// can't connect: the bootloader then boots the previous one.
#[embassy_executor::task]
pub(super) async fn boot_check_timeout_task(flash_store: &'static SharedFlashStorage) {
    Timer::after(BOOT_CHECK_TIMEOUT).await;
    if BOOT_CONFIRMED.load(Ordering::Relaxed) {
        return;
    }
    let state = with_ota_updater(flash_store, |ota| ota.current_ota_state()).await;
    if let Ok(OtaImageState::New | OtaImageState::PendingVerify) = state {
        error!("New firmware didn't pass the boot check in time, rebooting to roll back");
        super::reboot().await;
    }
}

async fn with_ota_updater<R>(
    flash_store: &SharedFlashStorage,
    f: impl for<'a> FnOnce(&mut OtaFlashUpdater<'a>) -> R,
) -> R {
    let mut flash_store = flash_store.lock().await;
    let mut pt_mem = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut ota = OtaUpdater::new(flash_store.deref_mut(), &mut pt_mem).unwrap();
    f(&mut ota)
}

/// Reports whether the running image is the factory one, or a rolled back one.
pub(super) async fn init_boot_partition(flash_store: &SharedFlashStorage) {
    let (is_factory, is_rolled_back) = with_ota_updater(flash_store, |ota| {
        let current_part = ota.selected_partition().unwrap();
        let current_state = ota.current_ota_state();
        // An aborted image is rolled back by the bootloader (to the other
        // bank, or the factory image)
        let is_rolled_back = matches!(current_state, Ok(OtaImageState::Aborted));
        if is_rolled_back {
            info!("Previous OTA was aborted, marking firmware as rolled back");
        }
        info!(
            "Current boot partition: {:?}, OTA state: {:?}",
            current_part, current_state
        );
        (current_part == AppPartitionSubType::Factory, is_rolled_back)
    })
    .await;

    app_settings::session::update_settings(|set| {
        set.is_factory_firmware = is_factory;
        set.is_rolled_back_firmware = is_rolled_back;
    })
    .await;
    app_settings::info::store();
}

pub(super) async fn confirm_current_partition(flash_store: &SharedFlashStorage) {
    with_ota_updater(flash_store, |ota| {
        if let Ok(state @ (OtaImageState::New | OtaImageState::PendingVerify)) =
            ota.current_ota_state()
        {
            info!(
                "Gateway reached, changing OTA image state from {:?} to Valid",
                state
            );
            ota.set_current_ota_state(OtaImageState::Valid).unwrap();
        }
    })
    .await;
}

pub(super) async fn set_factory_boot_partition(flash_store: &SharedFlashStorage) {
    with_ota_updater(flash_store, |ota| {
        info!("Set factory partition as boot partition");
        // Clears both OTA data entries. Only marking the running image aborted
        // would boot the other bank instead, if that holds a valid (older) image
        ota.ota_data()
            .and_then(|mut ota_data| {
                ota_data.set_current_app_partition(AppPartitionSubType::Factory)
            })
            .unwrap();
    })
    .await;
}

/// Boots the bank just written from now on, as a new image (unconfirmed).
pub(super) async fn activate_next_partition(flash_store: &SharedFlashStorage) {
    with_ota_updater(flash_store, |ota| {
        info!("Activating OTA boot partition");
        ota.activate_next_partition().unwrap();
        ota.set_current_ota_state(OtaImageState::New).unwrap();
    })
    .await;
}
