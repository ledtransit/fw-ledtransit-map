// Secure over-the-air firmware updates
// Available firmware updates are notified via the WebSocket protobuf and downloaded via HTTPS from the CDN.
// The OTA updates are dual banked, with rollback support and integrity is verified by SHA256 hash read back from flash and NIST P-256 signature of the update metadata.
use core::{
    ffi::CStr,
    net::SocketAddr,
    ops::DerefMut,
    sync::atomic::{AtomicBool, Ordering},
};

use defmt::{debug, error, info};
use edge_http::{Method, io::client};
use edge_nal_embassy::{Tcp, TcpBuffers};
use edge_nal_tls::TlsConnector;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_net::{Stack, dns};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Instant, TimeoutError, Timer, with_timeout};
use embedded_io_async::{Read, ReadExactError};
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::{
    ota::OtaImageState,
    ota_updater::OtaUpdater,
    partitions::{self, AppPartitionSubType, FlashRegion, PARTITION_TABLE_MAX_LEN},
};
use esp_hal::{
    peripherals::SHA,
    sha::{Sha, Sha256},
};
use esp_storage::FlashStorage;
use mbedtls_rs::{Certificate, ClientSessionConfig, SessionError, Tls};
use nb::block;
use nourl::{Url, UrlScheme};
use p256::{ecdsa::signature::Verifier, pkcs8::DecodePublicKey};
use smoltcp::wire::DnsQueryType;

use crate::{
    config::CONFIG,
    display::leds::{self, LedStatus},
    net::ws_client::{self, WsClientError, client_proto::DeviceUpdate, send_telemetry},
    store::{SharedFlashStorage, app_settings},
    time, trace,
};

const TCP_RX_SIZE: usize = 4 * 1024;
const TCP_TX_SIZE: usize = 512;

const HTTP_MAX_NUM_HEADERS: usize = 32;
const HTTP_BUFFER_SIZE: usize = 512;

const OTA_PARTITION_SIZE: usize = 0x14F000; // partitions.csv factory/ota0/ota1
const OTA_CHUNK_SIZE: usize = 4 * 1024; // Must be multiple of flash sector size (4KB) for efficient writes

/// A new image must pass the boot check (see confirm_boot) within this time
/// after booting, or the device reboots, which makes the bootloader roll back
const BOOT_CHECK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

type HttpsConnection<'a> = client::Connection<'a, TlsConnector<'a, Tcp<'a>>, HTTP_MAX_NUM_HEADERS>;
type OtaFlashUpdater<'a> = OtaUpdater<'a, FlashStorage<'static>>;
type FlashStorageRegion<'a> = FlashRegion<'a, FlashStorage<'static>>;

#[derive(defmt::Format, Debug)]
pub enum OtaError {
    UrlError,
    DnsError(dns::Error),
    HttpError(edge_http::io::Error<SessionError>),
    HttpReadError(ReadExactError<edge_http::io::Error<SessionError>>),
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

static OTA_SIGNAL: Signal<CriticalSectionRawMutex, OtaEvent> = Signal::new();
static OTA_CANCEL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
// Separate from the events: must not replace a pending one, nor be replaced
static OTA_CONFIRM: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static BOOT_CONFIRMED: AtomicBool = AtomicBool::new(false);

enum OtaEvent {
    InitBootPartition,
    BootFromFactory,
    StartUpdate(DeviceUpdate),
    ScheduleUpdate(DeviceUpdate, Duration),
}

pub fn spawn(
    spawner: Spawner,
    sta_stack: Stack<'static>,
    tls: &'static Tls<'static>,
    ca_cert: &'static Certificate<'static>,
    flash_store: &'static SharedFlashStorage,
    sha_peri: SHA<'static>,
) {
    spawner.spawn(ota_client_task(sta_stack, tls, ca_cert, flash_store, sha_peri).unwrap());
    spawner.spawn(boot_check_timeout_task(flash_store).unwrap());
}

/// Rolls back a new image that doesn't pass the boot check in time, e.g.
/// because it hangs or can't connect: rebooting an unconfirmed image makes the
/// bootloader boot the previous one.
#[embassy_executor::task]
async fn boot_check_timeout_task(flash_store: &'static SharedFlashStorage) {
    Timer::after(BOOT_CHECK_TIMEOUT).await;
    if BOOT_CONFIRMED.load(Ordering::Relaxed) {
        return;
    }
    let state = with_ota_updater(flash_store, |ota| ota.current_ota_state()).await;
    if let Ok(OtaImageState::New | OtaImageState::PendingVerify) = state {
        error!("New firmware didn't pass the boot check in time, rebooting to roll back");
        ws_client::quit(Err(WsClientError::Reboot));
        Timer::after(Duration::from_secs(2)).await;
        esp_hal::system::software_reset();
    }
}

#[embassy_executor::task]
async fn ota_client_task(
    sta_stack: Stack<'static>,
    tls: &'static Tls<'static>,
    ca_cert: &'static Certificate<'static>,
    flash_store: &'static SharedFlashStorage,
    sha_peri: SHA<'static>,
) {
    let mut sha = Sha::new(sha_peri);
    loop {
        sta_stack.wait_link_up().await;
        sta_stack.wait_config_up().await;

        match run(sta_stack, tls, ca_cert, flash_store, &mut sha).await {
            Ok(_) => unreachable!(),
            Err(e) => {
                trace::err!("OTA client error: {:?}", e);
                leds::set_status(LedStatus::UpdateFailed);
            }
        }
    }
}

async fn run(
    sta_stack: Stack<'static>,
    tls: &Tls<'static>,
    ca_cert: &Certificate<'static>,
    flash_store: &'static SharedFlashStorage,
    sha: &mut Sha<'_>,
) -> Result<(), OtaError> {
    loop {
        // Confirming first: when an update arrives right after connecting, the
        // running image is confirmed before it's replaced
        let event = match select(OTA_CONFIRM.wait(), OTA_SIGNAL.wait()).await {
            Either::First(()) => {
                with_ota_updater(flash_store, confirm_current_partition).await;
                continue;
            }
            Either::Second(event) => event,
        };
        OTA_CANCEL.reset();

        match event {
            OtaEvent::InitBootPartition => {
                let result = with_ota_updater(flash_store, init_current_partition).await;
                app_settings::session::update_settings(|set| {
                    set.is_factory_firmware = result.is_factory;
                    set.is_rolled_back_firmware = result.is_aborted;
                })
                .await;

                // Store device info in flash
                app_settings::info::store();
            }
            OtaEvent::BootFromFactory => {
                with_ota_updater(flash_store, |ota| {
                    info!("Set factory partition as boot partition");
                    set_factory_boot_partition(ota);
                })
                .await;
                ws_client::quit(Err(WsClientError::Reboot));
                Timer::after(Duration::from_secs(2)).await;
                esp_hal::system::software_reset();
            }
            OtaEvent::ScheduleUpdate(update, delay) => {
                info!(
                    "Scheduling OTA update to v{}.{}.{} in {} seconds",
                    update.firmware_version_major,
                    update.firmware_version_minor,
                    update.firmware_version_patch,
                    delay.as_secs()
                );

                // Store scheduled update timestamp and send telemetry
                let now_unix = time::get_unix_timestamp_seconds().await;
                app_settings::session::update_settings(|set| {
                    set.auto_update_scheduled_unix_timestamp =
                        Some(now_unix + delay.as_secs() as u32);
                })
                .await;
                send_telemetry();

                // Wait for scheduled delay or cancel signal
                match with_timeout(delay, OTA_CANCEL.wait()).await {
                    Err(TimeoutError) => {
                        info!("OTA update: scheduled delay elapsed, starting update");
                    }
                    Ok(_) => {
                        info!("OTA update: scheduled delay canceled, aborting update");
                        app_settings::session::update_settings(|set| {
                            set.auto_update_scheduled_unix_timestamp = None;
                        })
                        .await;
                        send_telemetry();
                        continue;
                    }
                }

                OTA_SIGNAL.signal(OtaEvent::StartUpdate(update));
            }
            OtaEvent::StartUpdate(update) => {
                let update_version = (
                    update.firmware_version_major,
                    update.firmware_version_minor,
                    update.firmware_version_patch,
                );
                let installed_version = (
                    CONFIG.fw_version.major,
                    CONFIG.fw_version.minor,
                    CONFIG.fw_version.patch,
                );
                // Check version must be newer: no downgrades allowed
                // Exception: Beta firmware allows updating to the same version
                if update_version < installed_version
                    || (update_version == installed_version && !CONFIG.fw_version.beta)
                {
                    trace::wrn!(
                        "OTA update refused: v{}.{}.{} isn't newer than installed",
                        update.firmware_version_major,
                        update.firmware_version_minor,
                        update.firmware_version_patch
                    );
                    continue;
                }

                // Check size must fit ota partition
                if update.size_bytes as usize > OTA_PARTITION_SIZE {
                    trace::err!(
                        "OTA update size {} exceeds partition size {}",
                        update.size_bytes,
                        OTA_PARTITION_SIZE
                    );
                    continue;
                }

                // Check the signature before downloading: only signed metadata
                // gets to overwrite the other bank (the previous firmware). The
                // signed hash is checked against the image read back from flash
                if let Err(e) = verify_update_signature(&update) {
                    trace::err!("OTA update refused, signature invalid: {:?}", e);
                    app_settings::session::update_settings(|set| {
                        set.update_has_failed = true;
                    })
                    .await;
                    ws_client::send_status();
                    ws_client::send_telemetry();
                    leds::set_status(LedStatus::UpdateFailed);
                    continue;
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

                // Download OTA image and write to flash
                match download_ota_update_to_flash(
                    flash_store,
                    sta_stack,
                    tls,
                    ca_cert,
                    sha,
                    &update,
                )
                .await
                {
                    Ok(()) => {
                        // Activate new boot partition and trigger reboot by WS quit signal
                        with_ota_updater(flash_store, activate_boot_partition).await;
                        ws_client::quit(Err(WsClientError::Reboot));

                        // In case WS client is not running or not capable of graceful shutdown, force reboot after short delay
                        Timer::after(Duration::from_secs(2)).await;
                        esp_hal::system::software_reset();
                    }
                    Err(e) => {
                        trace::err!("OTA update failed: {:?}", e);
                        app_settings::session::update_settings(|set| {
                            set.updating_firmware = false;
                            set.update_has_failed = true;
                        })
                        .await;
                        ws_client::send_status();
                        ws_client::send_telemetry();
                        leds::set_status(LedStatus::UpdateFailed);
                    }
                }
            }
        }
    }
}

/// Verifies the update metadata is signed by LEDTransit (NIST P-256), for
/// this product. The image is covered by the signed SHA-256.
fn verify_update_signature(update: &DeviceUpdate) -> Result<(), OtaError> {
    let pubkey = p256::ecdsa::VerifyingKey::from_public_key_der(include_bytes!(
        "../../assets/secure_ota/p256_ota_public_key.der"
    ))
    .expect("Failed to load OTA public key");
    let signature = p256::ecdsa::Signature::from_slice(&update.p256_signature)
        .map_err(|_| OtaError::SignatureMalformedError)?;
    // Message format: concat([u32le:MAJOR, u32le:MINOR, u32le:PATCH, u32le:SIZE, [u8:32]:SHA256, str:PRODUCT_ID])
    let message = [
        &update.firmware_version_major.to_le_bytes(),
        &update.firmware_version_minor.to_le_bytes(),
        &update.firmware_version_patch.to_le_bytes(),
        &update.size_bytes.to_le_bytes(),
        update.sha256_hash.as_slice(),
        CONFIG.product.as_str().as_bytes(),
    ]
    .concat();
    pubkey
        .verify(&message, &signature)
        .map_err(|_| OtaError::SignatureVerificationError)
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

struct InitBootPartitionResult {
    is_aborted: bool,
    is_factory: bool,
}

fn init_current_partition(ota: &mut OtaFlashUpdater) -> InitBootPartitionResult {
    let current_part = ota.selected_partition().unwrap();
    let current_state = ota.current_ota_state();

    let mut result = InitBootPartitionResult {
        is_aborted: false,
        is_factory: false,
    };

    // A newly installed image is only marked valid once it reached the
    // gateway (see confirm_boot)

    // If previous OTA was aborted, it will be rolled back to the other OTA partition or factory by the bootloader
    if let Ok(OtaImageState::Aborted) = current_state {
        info!("Previous OTA was aborted, marking firmware as rolled back");
        result.is_aborted = true;
    }

    if current_part == AppPartitionSubType::Factory {
        result.is_factory = true;
    }

    info!(
        "Current boot partition: {:?}, OTA state: {:?}",
        current_part, current_state
    );

    result
}

/// Marks a newly installed image valid: it booted, connected to WiFi,
/// authenticated with the gateway over TLS, and received, decoded and
/// processed transit data. Until then, the bootloader rolls back to the
/// previous image on the next reboot.
fn confirm_current_partition(ota: &mut OtaFlashUpdater) {
    if let Ok(state @ (OtaImageState::New | OtaImageState::PendingVerify)) = ota.current_ota_state()
    {
        info!(
            "Gateway reached, changing OTA image state from {:?} to Valid",
            state
        );
        ota.set_current_ota_state(OtaImageState::Valid).unwrap();
    }
}

fn set_factory_boot_partition(ota: &mut OtaFlashUpdater) {
    // Clears both OTA data entries, so the bootloader boots the factory app.
    // Only marking the running image aborted would make it boot the other OTA
    // bank instead, as long as that holds a valid (older) image
    ota.ota_data()
        .and_then(|mut ota_data| ota_data.set_current_app_partition(AppPartitionSubType::Factory))
        .unwrap();
}

fn write_ota_chunk_to_flash(
    partition: &mut FlashStorageRegion<'_>,
    byte_offset: usize,
    data: &[u8; OTA_CHUNK_SIZE],
) -> Result<(), partitions::Error> {
    // Check chunks fits in partition
    let part_size = partition.partition_size();
    if byte_offset + data.len() > part_size {
        panic!(
            "OTA chunk at offset {} with size {} exceeds partition size {}",
            byte_offset,
            data.len(),
            part_size
        );
    }

    debug!(
        "Writing OTA chunk to flash at offset {}, size {}",
        byte_offset,
        data.len()
    );
    partition.write(byte_offset as u32, data)?;
    Ok(())
}

async fn sha256_hash_ota_flash(
    sha: &mut Sha<'_>,
    partition: &mut FlashStorageRegion<'_>,
    part_size: usize,
) -> Result<[u8; 32], partitions::Error> {
    let mut sha = sha.start::<Sha256>();
    let mut digest = [0u8; 32];

    let mut read_offset: usize = 0;
    let mut buf: [u8; OTA_CHUNK_SIZE] = [0u8; OTA_CHUNK_SIZE];

    // Read OTA partition flash in chunks and update SHA256 hash, blocking but fast
    while read_offset < part_size {
        let read_size = core::cmp::min(OTA_CHUNK_SIZE, part_size - read_offset);
        partition.read(read_offset as u32, &mut buf[0..read_size])?;

        let mut remaining = &buf[0..read_size];
        while !remaining.is_empty() {
            remaining = block!(sha.update(remaining)).unwrap();
        }

        read_offset += read_size;
    }

    block!(sha.finish(&mut digest)).unwrap();
    Ok(digest)
}

fn activate_boot_partition(ota: &mut OtaFlashUpdater) {
    info!("Activating OTA boot partition");
    ota.activate_next_partition().unwrap();
    ota.set_current_ota_state(OtaImageState::New).unwrap();
}

async fn download_ota_update_to_flash(
    flash_store: &'static SharedFlashStorage,
    stack: Stack<'_>,
    tls: &Tls<'_>,
    ca_cert: &Certificate<'static>,
    sha: &mut Sha<'_>,
    update: &DeviceUpdate,
) -> Result<(), OtaError> {
    // Parse URL
    let url = Url::parse(&update.image_url).map_err(|_| OtaError::UrlError)?;
    if url.scheme() != UrlScheme::HTTPS {
        error!("OTA URL scheme is not https");
        return Err(OtaError::UrlError);
    }
    let host = url.host();
    let path = url.path();

    // DNS resolve IP with timeout
    let ip_addr = *with_timeout(
        Duration::from_secs(10),
        stack.dns_query(host, DnsQueryType::A),
    )
    .await
    .map_err(OtaError::Timeout)?
    .map_err(OtaError::DnsError)?
    .first()
    .ok_or(OtaError::DnsError(dns::Error::Failed))?;
    let socket_addr = SocketAddr::new(ip_addr.into(), 443);
    debug!("Resolved Ota IP to {}, path {}", ip_addr, path);

    // Create TCP connection
    let tcp_bufs = TcpBuffers::<1, TCP_TX_SIZE, TCP_RX_SIZE>::new();
    let tcp = Tcp::new(stack, &tcp_bufs);

    // Configure TLS session
    let host_zstr = heapless::format!(64; "{}\0", host).map_err(|_| OtaError::UrlError)?;
    let server_name =
        CStr::from_bytes_with_nul(host_zstr.as_bytes()).map_err(|_| OtaError::UrlError)?;
    let session_config = ClientSessionConfig {
        ca_chain: Some(ca_cert.clone()),
        server_name: Some(server_name),
        ..ClientSessionConfig::new()
    };

    let tls_connector = TlsConnector::new(tls.reference(), tcp, &session_config);

    let mut buf: [u8; HTTP_BUFFER_SIZE] = [0u8; HTTP_BUFFER_SIZE];
    let mut conn = HttpsConnection::new(&mut buf, &tls_connector, socket_addr);

    // GET request with timeout
    with_timeout(
        Duration::from_secs(10),
        conn.initiate_request(
            true,
            Method::Get,
            path,
            &[("Host", host), ("Connection", "close")],
        ),
    )
    .await
    .map_err(OtaError::Timeout)?
    .map_err(OtaError::HttpError)?;

    // Start response with timeout
    with_timeout(Duration::from_secs(10), conn.initiate_response())
        .await
        .map_err(OtaError::Timeout)?
        .map_err(OtaError::HttpError)?;
    let response = conn.headers().map_err(OtaError::HttpError)?;

    // Check status code is OK
    if response.code != 200 {
        trace::err!(
            "OTA HTTP failed with status code {}, url: {}",
            response.code,
            update.image_url
        );
        return Err(OtaError::StatusCodeError(response.code));
    }

    // Use content length header to determine OTA image size
    let content_length = response
        .headers
        .get("Content-Length")
        .and_then(|v| v.parse::<usize>().ok());
    if let Some(len) = content_length {
        // The signed size, checked against the partition size before
        if len != update.size_bytes as usize {
            trace::err!(
                "OTA content length {} differs from update size {}, url: {}",
                len,
                update.size_bytes,
                update.image_url
            );
            return Err(OtaError::SizeMismatchError);
        }
        debug!("OTA content length: {} bytes", len);
    } else {
        trace::err!(
            "OTA content length header missing, url: {}",
            update.image_url
        );
        return Err(OtaError::HeaderMissingError);
    }

    // Read response
    let total_length = content_length.unwrap();
    let start_instant = Instant::now();
    let mut total_bytes_read: usize = 0;
    let mut progress_percent: u8 = 0;

    let mut flash_store = flash_store.lock().await;
    let mut pt_mem = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut ota = OtaUpdater::new(flash_store.deref_mut(), &mut pt_mem).unwrap();

    // Write OTA image to next dual-bank OTA partition (factory/ota1 -> ota0, ota0 -> ota1)
    let (mut next_partition, _) = ota.next_partition().unwrap();

    while total_bytes_read < total_length {
        let num_remaining = total_length - total_bytes_read;
        let read_size = core::cmp::min(OTA_CHUNK_SIZE, num_remaining);

        // Receive chunk with timeout
        let mut buf = [0u8; OTA_CHUNK_SIZE];
        with_timeout(
            Duration::from_secs(10),
            conn.read_exact(&mut buf[0..read_size]),
        )
        .await
        .map_err(OtaError::Timeout)?
        .map_err(OtaError::HttpReadError)?;

        // Write chunk to flash
        write_ota_chunk_to_flash(&mut next_partition, total_bytes_read, &buf)
            .map_err(OtaError::FlashWriteError)?;

        total_bytes_read += read_size;
        let elapsed_secs = (Instant::now() - start_instant).as_secs();
        let speed_bps = (total_bytes_read as f32 / elapsed_secs.max(1) as f32) as u32;
        let percent = ((total_bytes_read as f32 / total_length.max(1) as f32) * 100.0) as u8;

        if percent >= progress_percent + 2 {
            progress_percent = percent;
            app_settings::session::update_settings(|set| {
                set.update_progress_percent = progress_percent;
                set.update_speed_bytes_per_sec = speed_bps;
            })
            .await;
            ws_client::send_telemetry();
            info!(
                "OTA: Downloaded {} bytes, {}%, {} B/s",
                total_bytes_read, progress_percent, speed_bps
            );
        }
    }

    app_settings::session::update_settings(|set| {
        set.update_progress_percent = 100;
    })
    .await;
    ws_client::send_telemetry();

    // Close connection
    _ = conn.close().await;

    // Read back ota bank flash and compute SHA256 hash matches
    // This ensures 1) the signed hash matches the actual image contents and 2) the image was correctly written to flash without corruption
    let computed_hash = sha256_hash_ota_flash(sha, &mut next_partition, total_length)
        .await
        .map_err(OtaError::FlashReadError)?;
    if update.sha256_hash != computed_hash {
        trace::err!(
            "OTA SHA256 mismatch: expected {:?}, calculated {:?}",
            update.sha256_hash,
            computed_hash
        );
        return Err(OtaError::HashMismatchError);
    }

    info!(
        "OTA downloaded to flash and verified in {} seconds",
        (embassy_time::Instant::now() - start_instant).as_secs()
    );
    Ok(())
}

pub async fn start_firmware_update(update: &DeviceUpdate) {
    if app_settings::session::get_settings()
        .await
        .updating_firmware
    {
        // OTA already in progress, ignore new request
        return;
    }
    signal(OtaEvent::StartUpdate(update.clone()));
}

pub async fn schedule_firmware_update(update: &DeviceUpdate, delay: Duration) {
    let settings = app_settings::session::get_settings().await;
    if settings.updating_firmware || settings.auto_update_scheduled_unix_timestamp.is_some() {
        // OTA already in progress or already scheduled, ignore new request
        return;
    }
    signal(OtaEvent::ScheduleUpdate(update.clone(), delay));
}

pub fn init_boot_partition() {
    OTA_SIGNAL.signal(OtaEvent::InitBootPartition);
}

pub fn boot_from_factory() {
    OTA_SIGNAL.signal(OtaEvent::BootFromFactory);
}

/// The firmware works with the gateway (see confirm_current_partition): a
/// newly installed image is kept. Once per boot.
pub fn confirm_boot() {
    // Load and store only: no atomic read-modify-write on this target, and
    // only the WebSocket task calls this
    if !BOOT_CONFIRMED.load(Ordering::Relaxed) {
        BOOT_CONFIRMED.store(true, Ordering::Relaxed);
        OTA_CONFIRM.signal(());
    }
}

pub fn cancel() {
    OTA_CANCEL.signal(());
}

fn signal(event: OtaEvent) {
    cancel();
    OTA_SIGNAL.signal(event);
}
