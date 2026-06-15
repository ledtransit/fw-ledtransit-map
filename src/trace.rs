// Device error reporting
use alloc::string::{String, ToString};
use defmt::{error, info};
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::Instant;
use esp_hal::{Persistable, ram, rom::crc::crc32_le};

use crate::net::ws_client::{
    self,
    client_proto::{DeviceError, DeviceErrorType},
};

const MAX_NUM_ERRORS: usize = 4;
const PANIC_INFO_MAX_SERIALIZED_SIZE: usize = 256;

static ERRORS: Mutex<CriticalSectionRawMutex, heapless::Vec<DeviceError, MAX_NUM_ERRORS>> =
    Mutex::new(heapless::Vec::new());

// Panic info persisted across resets in RTC slow memory for error reporting after reboot
#[ram(unstable(rtc_fast, persistent))]
static mut PANIC_INFO: PersistablePanicInfo = PersistablePanicInfo {
    did_panic: false,
    instant_milliseconds: 0,
    file_name: heapless::String::new(),
    line_number: 0,
    column_number: 0,
    message: heapless::String::new(),
    crc32: 0,
};

#[derive(Serialize, Deserialize, Clone)]
struct PersistablePanicInfo {
    did_panic: bool,
    instant_milliseconds: u32,
    file_name: heapless::String<64>,
    line_number: u32,
    column_number: u32,
    message: heapless::String<128>,
    crc32: u32, // crc to verify integrity of panic info data
}

unsafe impl Persistable for PersistablePanicInfo {}

impl PersistablePanicInfo {
    fn try_from_panic_info(panic_info: &core::panic::PanicInfo) -> Option<Self> {
        let location = panic_info
            .location()
            .unwrap_or_else(|| core::panic::Location::caller());
        let mut file_name = heapless::String::<64>::new();
        let file_bytes = heapless::String::<64>::try_from(location.file())
            .unwrap_or_else(|_| heapless::String::<64>::try_from("unknown").unwrap_or_default())
            .into_bytes();
        let copy_len = core::cmp::min(file_bytes.len(), 64);
        file_name
            .push_str(core::str::from_utf8(&file_bytes[..copy_len]).unwrap_or("unknown"))
            .ok()?;

        let mut message = heapless::String::<128>::new();
        let msg_bytes =
            heapless::String::<128>::try_from(panic_info.message().to_string().as_str())
                .unwrap_or_else(|_| {
                    heapless::String::<128>::try_from("unknown").unwrap_or_default()
                })
                .into_bytes();
        let msg_len = core::cmp::min(msg_bytes.len(), 128);
        message
            .push_str(core::str::from_utf8(&msg_bytes[..msg_len]).unwrap_or("unknown"))
            .ok()?;

        let mut persist_info = PersistablePanicInfo {
            did_panic: true,
            instant_milliseconds: Instant::now().as_millis() as u32,
            file_name,
            line_number: location.line(),
            column_number: location.column(),
            message,
            crc32: 0, // to be filled after calculating CRC
        };

        // Serialize panic info with crc32=0 and calculate crc32 over the serialized data for integrity verification on recovery
        let serialized = postcard::to_vec::<_, PANIC_INFO_MAX_SERIALIZED_SIZE>(&persist_info)
            .expect("Failed to serialize panic info");
        persist_info.crc32 = crc32_le(0xFFFFFFFF, &serialized);

        Some(persist_info)
    }
}

#[panic_handler]
fn panic(panic: &core::panic::PanicInfo) -> ! {
    error!("Panic: {}", panic);
    if let Some(persist_panic) = PersistablePanicInfo::try_from_panic_info(panic) {
        unsafe {
            // Write panic info to RTC memory before reset
            PANIC_INFO = persist_panic;
        }
    }
    esp_hal::system::software_reset();
}

#[defmt::panic_handler]
fn defmt_panic() -> ! {
    esp_hal::system::software_reset();
}

pub fn init_on_boot() {
    // Read panic info from RTC memory and recover if valid panic info is found (can be garbage data)
    let mut panic_info = unsafe { core::ptr::read(&raw const PANIC_INFO) };

    if panic_info.did_panic {
        // Validate integrity of panic info using CRC32 before recovering
        let panic_info_crc32 = panic_info.crc32;
        panic_info.crc32 = 0; // serialize with crc32=0
        let serialized =
            if let Ok(data) = postcard::to_vec::<_, PANIC_INFO_MAX_SERIALIZED_SIZE>(&panic_info) {
                data
            } else {
                return; // serialization failed: discard panic info
            };
        let calculated_crc32 = crc32_le(0xFFFFFFFF, &serialized);
        if calculated_crc32 != panic_info_crc32 {
            return; // crc mismatch: discard panic info silently
        }

        info!("Recovering from panic");
        recover_from_panic(&panic_info);

        unsafe {
            // Clear persisted panic info
            PANIC_INFO.did_panic = false;
        }
    }
}

pub fn get_errors() -> heapless::Vec<DeviceError, MAX_NUM_ERRORS> {
    ERRORS.lock(|errors| errors.clone())
}

pub fn clear_errors() {
    unsafe {
        ERRORS.lock_mut(|errors| errors.clear());
    }
}

fn add_error(error: DeviceError) {
    unsafe {
        ERRORS.lock_mut(|errors: &mut heapless::Vec<DeviceError, MAX_NUM_ERRORS>| {
            _ = errors.push(error); // silently drop error if max capacity is reached
        });
    }
}

fn recover_from_panic(panic_info: &PersistablePanicInfo) {
    add_error(DeviceError {
        r#type: DeviceErrorType::Panic as i32,
        instant_milliseconds: panic_info.instant_milliseconds,
        message: panic_info.message.clone().to_string(),
        file_name: panic_info.file_name.clone().to_string(),
        line_number: panic_info.line_number,
        column_number: panic_info.column_number,
        unix_timestamp: 0, // to be filled later
    });
}

pub fn report_error(
    err_type: DeviceErrorType,
    message: String,
    file_name: String,
    line_number: u32,
    column_number: u32,
) {
    add_error(DeviceError {
        r#type: err_type as i32,
        instant_milliseconds: Instant::now().as_millis() as u32,
        message,
        file_name,
        line_number,
        column_number,
        unix_timestamp: 0, // to be filled later
    });
}

pub fn flush_errors() {
    if !get_errors().is_empty() {
        ws_client::send_errors();
    }
}

#[macro_export]
macro_rules! err {
    ($fmt:expr $(, $args:expr)*) => {{
            use $crate::net::ws_client::client_proto::{DeviceErrorType};
            use $crate::trace;
            use alloc::string::ToString;

            trace::report_error(
                DeviceErrorType::Error,
                alloc::format!($fmt $(, $args)*),
                file!().to_string(),
                line!(),
                column!(),
            );
            defmt::error!($fmt $(, $args)*);
    }};
}

#[macro_export]
macro_rules! wrn {
    ($fmt:expr $(, $args:expr)*) => {{
            use $crate::net::ws_client::client_proto::{DeviceErrorType};
            use $crate::trace;
            use alloc::string::ToString;

            trace::report_error(
                DeviceErrorType::Warning,
                alloc::format!($fmt $(, $args)*),
                file!().to_string(),
                line!(),
                column!(),
            );
            defmt::warn!($fmt $(, $args)*);
    }};
}

pub use {err, wrn};
