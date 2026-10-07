// Device authentication: proves to the gateway that this is a genuine
// LEDTransit device, by HMAC-SHA256 over the gateway's challenges with keys
// burned into the eFuses at production (KEY5, and KEY4 as a backup the
// gateway can switch to). The keys are read protected: no software, this
// firmware or any other flashed, can read them, only have the HMAC peripheral
// compute with them. Each message is bound to what it's for by its label, and
// to a fresh challenge, so a proof can't be reused or computed ahead of time.
//
// Must match the gateway (device_auth) and the release tool (register-device).

use alloc::vec::Vec;
use core::fmt::Write;

use defmt::{error, info};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use esp_hal::{
    efuse::{self, EfuseField},
    hmac::{Hmac, HmacPurpose, KeyId},
    peripherals::HMAC,
};

const CLAIM_LABEL: &[u8] = b"ledtransit/claim/v1";
const CONNECT_LABEL: &[u8] = b"ledtransit/connect/v1";
const SELF_TEST_LABEL: &[u8] = b"ledtransit/self-test/v1";

/// eFuse key purpose HMAC_UP: the HMAC result is read by software
const KEY_PURPOSE_HMAC_UP: u8 = 8;

/// eFuse key block holding a device key
#[derive(defmt::Format, Debug, Clone, Copy, PartialEq)]
pub enum KeySlot {
    Primary,
    Backup,
}

impl KeySlot {
    /// The slot by its key block number, as the gateway names it
    pub fn from_block(block: u8) -> Option<Self> {
        match block {
            5 => Some(Self::Primary),
            4 => Some(Self::Backup),
            _ => None,
        }
    }

    fn key_id(self) -> KeyId {
        match self {
            Self::Primary => KeyId::Key5,
            Self::Backup => KeyId::Key4,
        }
    }

    fn block(self) -> u8 {
        match self {
            Self::Primary => 5,
            Self::Backup => 4,
        }
    }

    fn purpose_field(self) -> EfuseField {
        match self {
            Self::Primary => efuse::KEY_PURPOSE_5,
            Self::Backup => efuse::KEY_PURPOSE_4,
        }
    }
}

pub type Proof = [u8; 32];

#[derive(defmt::Format, Debug, Clone, Copy)]
pub enum DeviceAuthError {
    /// The key isn't burned (for HMAC): a device not provisioned at production
    NoKey,
    NotInitialized,
}

static HMAC_PERIPHERAL: Mutex<CriticalSectionRawMutex, Option<Hmac<'static>>> = Mutex::new(None);

pub async fn init(hmac: HMAC<'static>) {
    *HMAC_PERIPHERAL.lock().await = Some(Hmac::new(hmac));
    if has_keys() {
        info!("Device keys present");
    } else {
        error!("Device keys missing: this device can't authenticate (not provisioned)");
    }
}

/// Whether both device keys are burned: for HMAC, and read protected.
pub fn has_keys() -> bool {
    let read_disabled = efuse::read_field_le::<u8>(efuse::RD_DIS);
    [KeySlot::Primary, KeySlot::Backup].into_iter().all(|slot| {
        // RD_DIS bit n protects key block n (KEY0 at bit 0)
        efuse::read_field_le::<u8>(slot.purpose_field()) == KEY_PURPOSE_HMAC_UP
            && read_disabled & (1 << slot.block()) != 0
    })
}

/// HMAC-SHA256 with the slot's key over the parts, concatenated.
async fn hmac(slot: KeySlot, parts: &[&[u8]]) -> Result<Proof, DeviceAuthError> {
    // Passed to the driver as one message: it decides whether the next block
    // is the last one by what's left of the data given in the call (esp-hal
    // 1.1), so a message given in parts gives a wrong HMAC once its parts
    // don't happen to end where that guess holds
    let message: Vec<u8> = parts.concat();
    let mut peripheral = HMAC_PERIPHERAL.lock().await;
    let hmac = peripheral.as_mut().ok_or(DeviceAuthError::NotInitialized)?;
    // Computed without awaiting: the HMAC peripheral uses the SHA accelerator,
    // nothing else may use that meanwhile
    hmac.init();
    nb::block!(hmac.configure(HmacPurpose::ToUser, slot.key_id()))
        .map_err(|_| DeviceAuthError::NoKey)?;
    let mut remaining: &[u8] = &message;
    while !remaining.is_empty() {
        remaining = nb::block!(hmac.update(remaining)).unwrap();
    }
    let mut proof = [0u8; 32];
    nb::block!(hmac.finalize(&mut proof)).unwrap();
    Ok(proof)
}

/// Proof for claiming this device into the account of the provisioning
/// token's user.
pub async fn claim_proof(
    slot: KeySlot,
    nonce: &str,
    hardware_id: &str,
    prov_token: &str,
) -> Result<Proof, DeviceAuthError> {
    hmac(
        slot,
        &[
            CLAIM_LABEL,
            &[0],
            nonce.as_bytes(),
            &[0],
            hardware_id.as_bytes(),
            &[0],
            prov_token.as_bytes(),
        ],
    )
    .await
}

/// Proof for connecting to the gateway.
pub async fn connect_proof(
    slot: KeySlot,
    nonce: &str,
    hardware_id: &str,
) -> Result<Proof, DeviceAuthError> {
    hmac(
        slot,
        &[CONNECT_LABEL, &[0], nonce.as_bytes(), &[0], hardware_id.as_bytes()],
    )
    .await
}

/// Proofs of both keys for the release tool's self-test at production.
pub async fn self_test_proofs(
    challenge: &str,
    hardware_id: &str,
) -> Result<(Proof, Proof), DeviceAuthError> {
    let parts: [&[u8]; 5] = [
        SELF_TEST_LABEL,
        &[0],
        challenge.as_bytes(),
        &[0],
        hardware_id.as_bytes(),
    ];
    Ok((
        hmac(KeySlot::Primary, &parts).await?,
        hmac(KeySlot::Backup, &parts).await?,
    ))
}

pub fn to_hex(proof: &Proof) -> heapless::String<64> {
    let mut hex = heapless::String::new();
    for byte in proof {
        // Fits exactly: 32 bytes, 2 chars each
        let _ = write!(hex, "{:02x}", byte);
    }
    hex
}
