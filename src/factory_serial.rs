// Commands over the USB serial port, for production (the release tool's
// register-device): "ledtransit hardware-id" and "ledtransit self-test
// <challenge>", answered "ok <fields>" or "error <reason>". The self-test
// proves both device keys over the tool's challenge, under their own label:
// nothing this answers authenticates to the gateway.

use core::fmt::Write as _;

use embassy_executor::Spawner;
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read, Write};
use esp_hal::{
    Async,
    peripherals::USB_DEVICE,
    usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagTx},
};

use crate::{
    config,
    device_auth::{self, DeviceAuthError},
};

const MAX_LINE_LENGTH: usize = 128;

pub fn spawn(spawner: Spawner, usb_device: USB_DEVICE<'static>) {
    spawner.spawn(factory_serial_task(usb_device).unwrap());
}

#[embassy_executor::task]
async fn factory_serial_task(usb_device: USB_DEVICE<'static>) {
    let (mut rx, mut tx) = UsbSerialJtag::new(usb_device).into_async().split();
    let mut line: heapless::Vec<u8, MAX_LINE_LENGTH> = heapless::Vec::new();
    let mut buf = [0u8; 64];
    loop {
        // Reading USB serial can't fail
        let Ok(len) = rx.read(&mut buf).await;
        for &byte in &buf[..len] {
            if byte == b'\n' || byte == b'\r' {
                if !line.is_empty() {
                    handle_line(&line, &mut tx).await;
                    line.clear();
                }
            } else if line.push(byte).is_err() {
                // Too long to be a command
                line.clear();
            }
        }
    }
}

async fn handle_line(line: &[u8], tx: &mut UsbSerialJtagTx<'static, Async>) {
    let Ok(line) = core::str::from_utf8(line) else {
        return;
    };
    let mut words = line.split_whitespace();
    if words.next() != Some("ledtransit") {
        return;
    }

    let hardware_id = config::get_hardware_id_str();
    let mut answer: heapless::String<160> = heapless::String::new();
    let _ = match (words.next(), words.next(), words.next()) {
        (Some("hardware-id"), None, None) => write!(answer, "ok {}", hardware_id),
        (Some("self-test"), Some(challenge), None) if challenge.len() <= 64 => {
            match device_auth::self_test_proofs(challenge, &hardware_id).await {
                Ok((primary, backup)) => write!(
                    answer,
                    "ok {} {}",
                    device_auth::to_hex(&primary),
                    device_auth::to_hex(&backup)
                ),
                Err(DeviceAuthError::NoKey) => write!(answer, "error no-key"),
                Err(DeviceAuthError::NotInitialized) => write!(answer, "error not-initialized"),
            }
        }
        _ => write!(answer, "error unknown-command"),
    };
    let _ = answer.push('\n');

    // Nobody may be reading: don't wait on that
    let _ = with_timeout(Duration::from_secs(1), async {
        let _ = tx.write_all(answer.as_bytes()).await;
        let _ = tx.flush().await;
    })
    .await;
}
