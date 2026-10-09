// Checks of an update's metadata before it's downloaded
use p256::{
    ecdsa::{Signature, VerifyingKey, signature::Verifier},
    pkcs8::DecodePublicKey,
};

use super::OtaError;
use crate::{config::CONFIG, net::ws_client::client_proto::DeviceUpdate};

/// No downgrades. The same version only over a beta build of it.
pub(super) fn is_newer_than_installed(update: &DeviceUpdate) -> bool {
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
    update_version > installed_version
        || (update_version == installed_version && CONFIG.fw_version.beta)
}

/// Verifies the metadata is signed by LEDTransit (NIST P-256), for this
/// product. The image itself is covered by the signed SHA-256.
pub(super) fn verify_signature(update: &DeviceUpdate) -> Result<(), OtaError> {
    let public_key = VerifyingKey::from_public_key_der(include_bytes!(
        "../../assets/secure_ota/p256_ota_public_key.der"
    ))
    .expect("Failed to load OTA public key");
    let signature = Signature::from_slice(&update.p256_signature)
        .map_err(|_| OtaError::SignatureMalformedError)?;
    // concat([u32le:MAJOR, u32le:MINOR, u32le:PATCH, u32le:SIZE, [u8:32]:SHA256, str:PRODUCT_ID])
    let message = [
        &update.firmware_version_major.to_le_bytes(),
        &update.firmware_version_minor.to_le_bytes(),
        &update.firmware_version_patch.to_le_bytes(),
        &update.size_bytes.to_le_bytes(),
        update.sha256_hash.as_slice(),
        CONFIG.product.as_str().as_bytes(),
    ]
    .concat();
    public_key
        .verify(&message, &signature)
        .map_err(|_| OtaError::SignatureVerificationError)
}
