// Device error reporting: errors, warnings and panics are reported to the server
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

// Survives the reset after a panic, to be reported after the reboot. Kept as
// bytes, checked before they're decoded: RTC memory holds garbage after power
// on (or a reset during a write)
#[ram(unstable(rtc_fast, persistent))]
static mut PANIC_INFO: PersistedPanicInfo = PersistedPanicInfo {
    len: 0,
    crc32: 0,
    bytes: [0; PANIC_INFO_MAX_SERIALIZED_SIZE],
};

// Valid for any bit pattern: only integers
#[repr(C)]
struct PersistedPanicInfo {
    len: u32, // Of the serialized info, 0 without one
    crc32: u32,
    bytes: [u8; PANIC_INFO_MAX_SERIALIZED_SIZE],
}

// SAFETY: Only integers and an array of them, valid for any bit pattern
unsafe impl Persistable for PersistedPanicInfo {}

impl PersistedPanicInfo {
    fn store(&mut self, info: &PanicInfo) {
        self.len = 0;
        if let Ok(serialized) = postcard::to_slice(info, &mut self.bytes) {
            let len = serialized.len();
            self.crc32 = crc32_le(0xFFFFFFFF, &self.bytes[..len]);
            self.len = len as u32;
        }
    }

    // None without one, or if it's garbage
    fn load(&self) -> Option<PanicInfo> {
        let serialized = self.bytes.get(..self.len as usize)?;
        if serialized.is_empty() || crc32_le(0xFFFFFFFF, serialized) != self.crc32 {
            return None;
        }
        postcard::from_bytes(serialized).ok()
    }
}

#[derive(Serialize, Deserialize)]
struct PanicInfo {
    instant_milliseconds: u32,
    file_name: heapless::String<64>,
    line_number: u32,
    column_number: u32,
    message: heapless::String<128>,
}

impl PanicInfo {
    fn new(panic_info: &core::panic::PanicInfo) -> Self {
        let location = panic_info
            .location()
            .unwrap_or_else(|| core::panic::Location::caller());
        PanicInfo {
            instant_milliseconds: Instant::now().as_millis() as u32,
            file_name: truncated_or_unknown(location.file()),
            line_number: location.line(),
            column_number: location.column(),
            message: truncated_or_unknown(&panic_info.message().to_string()),
        }
    }
}

// "unknown" if the text doesn't fit
fn truncated_or_unknown<const N: usize>(text: &str) -> heapless::String<N> {
    heapless::String::try_from(text)
        .unwrap_or_else(|_| heapless::String::try_from("unknown").unwrap_or_default())
}

#[panic_handler]
fn panic(panic: &core::panic::PanicInfo) -> ! {
    error!("Panic: {}", panic);
    let info = PanicInfo::new(panic);
    let persisted = &raw mut PANIC_INFO;
    // SAFETY: Single core, and nothing else runs once panicking
    unsafe { (*persisted).store(&info) };
    esp_hal::system::software_reset();
}

#[defmt::panic_handler]
fn defmt_panic() -> ! {
    esp_hal::system::software_reset();
}

/// Reports a panic from before the last reset, if any.
pub fn init_on_boot() {
    let persisted = &raw mut PANIC_INFO;
    // SAFETY: Only read and cleared here, before anything else could panic
    // (single core); valid for any bit pattern
    let panic_info = unsafe { (*persisted).load() };
    unsafe { (*persisted).len = 0 };
    let Some(panic_info) = panic_info else {
        return;
    };

    info!("Recovering from panic");
    add_error(DeviceError {
        r#type: DeviceErrorType::Panic as i32,
        instant_milliseconds: panic_info.instant_milliseconds,
        message: panic_info.message.to_string(),
        file_name: panic_info.file_name.to_string(),
        line_number: panic_info.line_number,
        column_number: panic_info.column_number,
        unix_timestamp: 0, // Set when sent
    });
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
        ERRORS.lock_mut(|errors| {
            _ = errors.push(error); // Dropped when full
        });
    }
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
        unix_timestamp: 0, // Set when sent
    });
}

pub fn flush_errors() {
    if !get_errors().is_empty() {
        ws_client::send_errors();
    }
}

// Logs and reports an error or warning (see err! and wrn!)
#[doc(hidden)]
#[macro_export]
macro_rules! report {
    ($err_type:ident, $level:ident, $fmt:expr $(, $args:expr)*) => {{
        $crate::trace::report_error(
            $crate::net::ws_client::client_proto::DeviceErrorType::$err_type,
            alloc::format!($fmt $(, $args)*),
            alloc::string::ToString::to_string(file!()),
            line!(),
            column!(),
        );
        defmt::$level!($fmt $(, $args)*);
    }};
}

#[macro_export]
macro_rules! err {
    ($fmt:expr $(, $args:expr)*) => {
        $crate::report!(Error, error, $fmt $(, $args)*)
    };
}

#[macro_export]
macro_rules! wrn {
    ($fmt:expr $(, $args:expr)*) => {
        $crate::report!(Warning, warn, $fmt $(, $args)*)
    };
}

pub use {err, wrn};
