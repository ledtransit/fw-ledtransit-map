// Download of an update image over HTTPS into the inactive bank
use core::{ffi::CStr, net::SocketAddr, ops::DerefMut};

use defmt::{debug, error, info};
use edge_http::{Method, io::client};
use edge_nal_embassy::{Tcp, TcpBuffers};
use edge_nal_tls::TlsConnector;
use embassy_net::{IpAddress, Stack, dns};
use embassy_time::{Duration, Instant, with_timeout};
use embedded_io_async::Read;
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::{
    ota_updater::OtaUpdater,
    partitions::{self, FlashRegion, PARTITION_TABLE_MAX_LEN},
};
use esp_hal::sha::{Sha, Sha256};
use esp_storage::FlashStorage;
use mbedtls_rs::{Certificate, ClientSessionConfig, Tls};
use nb::block;
use nourl::{Url, UrlScheme};
use smoltcp::wire::DnsQueryType;

use super::OtaError;
use crate::{
    net::ws_client::{self, client_proto::DeviceUpdate},
    store::{SharedFlashStorage, app_settings},
    trace,
};

pub(super) const OTA_PARTITION_SIZE: usize = 0x14F000; // partitions.csv factory/ota0/ota1
const OTA_CHUNK_SIZE: usize = 4 * 1024; // Must be multiple of flash sector size (4KB) for efficient writes

const TCP_RX_SIZE: usize = 4 * 1024;
const TCP_TX_SIZE: usize = 512;
const HTTP_MAX_NUM_HEADERS: usize = 32;
const HTTP_BUFFER_SIZE: usize = 512;
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

type HttpsConnection<'a> = client::Connection<'a, TlsConnector<'a, Tcp<'a>>, HTTP_MAX_NUM_HEADERS>;
type FlashStorageRegion<'a> = FlashRegion<'a, FlashStorage<'static>>;

/// Downloads the image into the inactive bank and checks it against the
/// signed hash, read back from flash.
pub(super) async fn download_to_flash(
    update: &DeviceUpdate,
    stack: Stack<'_>,
    tls: &Tls<'_>,
    ca_cert: &Certificate<'static>,
    flash_store: &SharedFlashStorage,
    sha: &mut Sha<'_>,
) -> Result<(), OtaError> {
    let url = Url::parse(&update.image_url).map_err(|_| OtaError::UrlError)?;
    if url.scheme() != UrlScheme::HTTPS {
        error!("OTA URL scheme is not https");
        return Err(OtaError::UrlError);
    }
    let host = url.host();
    let path = url.path();

    let ip_addr = resolve(stack, host).await?;
    let socket_addr = SocketAddr::new(ip_addr.into(), 443);
    debug!("Resolved Ota IP to {}, path {}", ip_addr, path);

    let tcp_bufs = TcpBuffers::<1, TCP_TX_SIZE, TCP_RX_SIZE>::new();
    let tcp = Tcp::new(stack, &tcp_bufs);
    let host_zstr = heapless::format!(64; "{}\0", host).map_err(|_| OtaError::UrlError)?;
    let server_name =
        CStr::from_bytes_with_nul(host_zstr.as_bytes()).map_err(|_| OtaError::UrlError)?;
    let session_config = ClientSessionConfig {
        ca_chain: Some(ca_cert.clone()),
        server_name: Some(server_name),
        ..ClientSessionConfig::new()
    };
    let tls_connector = TlsConnector::new(tls.reference(), tcp, &session_config);
    let mut http_buf = [0u8; HTTP_BUFFER_SIZE];
    let mut conn = HttpsConnection::new(&mut http_buf, &tls_connector, socket_addr);

    let image_size = request_image(&mut conn, host, path, update).await?;

    let mut flash_store = flash_store.lock().await;
    let mut pt_mem = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut ota = OtaUpdater::new(flash_store.deref_mut(), &mut pt_mem).unwrap();
    // The bank that isn't running: factory/ota1 -> ota0, ota0 -> ota1
    let (mut partition, _) = ota.next_partition().unwrap();

    let start_instant = Instant::now();
    write_image_to_flash(&mut conn, &mut partition, image_size, start_instant).await?;
    app_settings::session::update_settings(|set| {
        set.update_progress_percent = 100;
    })
    .await;
    ws_client::send_telemetry();
    _ = conn.close().await;

    // Read back from flash: the hash then also covers the flash write
    let computed_hash =
        sha256_of_flash(sha, &mut partition, image_size).map_err(OtaError::FlashReadError)?;
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
        (Instant::now() - start_instant).as_secs()
    );
    Ok(())
}

async fn resolve(stack: Stack<'_>, host: &str) -> Result<IpAddress, OtaError> {
    with_timeout(NETWORK_TIMEOUT, stack.dns_query(host, DnsQueryType::A))
        .await
        .map_err(OtaError::Timeout)?
        .map_err(OtaError::DnsError)?
        .first()
        .copied()
        .ok_or(OtaError::DnsError(dns::Error::Failed))
}

/// Requests the image, returning its size: the signed size, as the server
/// must confirm.
async fn request_image(
    conn: &mut HttpsConnection<'_>,
    host: &str,
    path: &str,
    update: &DeviceUpdate,
) -> Result<usize, OtaError> {
    with_timeout(
        NETWORK_TIMEOUT,
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

    with_timeout(NETWORK_TIMEOUT, conn.initiate_response())
        .await
        .map_err(OtaError::Timeout)?
        .map_err(OtaError::HttpError)?;
    let response = conn.headers().map_err(OtaError::HttpError)?;

    if response.code != 200 {
        trace::err!(
            "OTA HTTP failed with status code {}, url: {}",
            response.code,
            update.image_url
        );
        return Err(OtaError::StatusCodeError(response.code));
    }

    let Some(content_length) = response
        .headers
        .get("Content-Length")
        .and_then(|v| v.parse::<usize>().ok())
    else {
        trace::err!(
            "OTA content length header missing, url: {}",
            update.image_url
        );
        return Err(OtaError::HeaderMissingError);
    };
    // The signed size was checked against the partition size before
    if content_length != update.size_bytes as usize {
        trace::err!(
            "OTA content length {} differs from update size {}, url: {}",
            content_length,
            update.size_bytes,
            update.image_url
        );
        return Err(OtaError::SizeMismatchError);
    }
    debug!("OTA content length: {} bytes", content_length);
    Ok(content_length)
}

async fn write_image_to_flash(
    conn: &mut HttpsConnection<'_>,
    partition: &mut FlashStorageRegion<'_>,
    image_size: usize,
    start_instant: Instant,
) -> Result<(), OtaError> {
    let mut bytes_written: usize = 0;
    let mut progress_percent: u8 = 0;

    while bytes_written < image_size {
        let read_size = core::cmp::min(OTA_CHUNK_SIZE, image_size - bytes_written);
        // Zeroed for each chunk: the last one is written in full
        let mut chunk = [0u8; OTA_CHUNK_SIZE];
        with_timeout(NETWORK_TIMEOUT, conn.read_exact(&mut chunk[..read_size]))
            .await
            .map_err(OtaError::Timeout)?
            .map_err(OtaError::HttpReadError)?;
        write_chunk(partition, bytes_written, &chunk).map_err(OtaError::FlashWriteError)?;
        bytes_written += read_size;

        let elapsed_secs = (Instant::now() - start_instant).as_secs();
        let speed_bps = (bytes_written as f32 / elapsed_secs.max(1) as f32) as u32;
        let percent = ((bytes_written as f32 / image_size.max(1) as f32) * 100.0) as u8;
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
                bytes_written, progress_percent, speed_bps
            );
        }
    }
    Ok(())
}

fn write_chunk(
    partition: &mut FlashStorageRegion<'_>,
    byte_offset: usize,
    data: &[u8; OTA_CHUNK_SIZE],
) -> Result<(), partitions::Error> {
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
    partition.write(byte_offset as u32, data)
}

// Blocking, but fast with the SHA accelerator
fn sha256_of_flash(
    sha: &mut Sha<'_>,
    partition: &mut FlashStorageRegion<'_>,
    size: usize,
) -> Result<[u8; 32], partitions::Error> {
    let mut sha = sha.start::<Sha256>();
    let mut buf = [0u8; OTA_CHUNK_SIZE];
    let mut read_offset: usize = 0;

    while read_offset < size {
        let read_size = core::cmp::min(OTA_CHUNK_SIZE, size - read_offset);
        partition.read(read_offset as u32, &mut buf[..read_size])?;
        let mut remaining = &buf[..read_size];
        while !remaining.is_empty() {
            remaining = block!(sha.update(remaining)).unwrap();
        }
        read_offset += read_size;
    }

    let mut digest = [0u8; 32];
    block!(sha.finish(&mut digest)).unwrap();
    Ok(digest)
}
