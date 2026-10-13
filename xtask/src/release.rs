// Release builds: the OTA image with its signed metadata (see docs/SECURE_OTA.md)
use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use p256::{
    ecdsa::{
        Signature, SigningKey, VerifyingKey,
        signature::{Signer, Verifier},
    },
    pkcs8::{DecodePrivateKey, DecodePublicKey},
};
use serde::Serialize;

use crate::{
    cli::ReleaseArgs,
    firmware::{self, BOOTLOADER_PATH, ELF_PATH, PARTITION_TABLE_PATH},
    product::ProductId,
    tools::{self, copy, path_str},
};

const SIGNING_KEY_PATH: &str = "assets/secure_ota/p256_ota_private_key.p8";
const PUBLIC_KEY_PATH: &str = "assets/secure_ota/p256_ota_public_key.der";

type Version = (u32, u32, u32);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OtaMetadata {
    product_id: String,
    version: String,
    built_at: String,
    size_bytes: usize,
    sha256_hash: String,
    build_id: String,
    p256_signature: String,
}

pub fn build(args: ReleaseArgs) -> Result<()> {
    log::info!("Building release for product: {:?}", args.product_id);
    let cwd = env::current_dir()?;
    let product = args.product_id.as_str();
    let version = firmware::package_version()?;
    let version_numbers = parse_version(&version)?;

    if args.staging {
        log::warn!(
            "Staging release: may be built from a dirty or untagged commit, for testing only (don't publish it to production)"
        );
        if let Err(e) = tools::ensure_clean_tagged_commit(&version) {
            log::warn!("Not a release commit: {e}");
        }
    } else {
        tools::ensure_clean_tagged_commit(&version)?;
    }

    let mut env = firmware::release_env(&args.product_id);
    env.push(("RUSTFLAGS", release_rustflags(&cwd)?));
    firmware::cargo_build(&env)?;

    let ota_dir = Path::new("target/ota").join(product).join(&version);
    let elf_path = ota_dir.join("image.elf");
    let image_path = ota_dir.join("image.bin");
    let bootloader_path = ota_dir.join("bootloader.bin");
    fs::create_dir_all(&ota_dir)
        .with_context(|| format!("Failed to create ota binary directory at {:?}", ota_dir))?;
    copy(Path::new(ELF_PATH), &elf_path)?;
    copy(Path::new(BOOTLOADER_PATH), &bootloader_path)?;

    tools::espflash(&[
        "save-image",
        "--chip=esp32c3",
        &format!("--partition-table={PARTITION_TABLE_PATH}"),
        &format!("--bootloader={BOOTLOADER_PATH}"),
        path_str(&elf_path)?,
        path_str(&image_path)?,
    ])?;
    ensure_no_home_path(&[&elf_path, &image_path, &bootloader_path])?;

    let image = fs::read(&image_path)
        .with_context(|| format!("Failed to read OTA image at {:?}", image_path))?;
    let sha256_hex = sha256::digest(&image);
    let sha256 =
        hex::decode(&sha256_hex).context("Failed to decode SHA256 hash string into bytes")?;
    let signature = sign_update(
        &args.product_id,
        version_numbers,
        image.len() as u32,
        &sha256,
    )?;

    let metadata = OtaMetadata {
        product_id: product.to_string(),
        version,
        built_at: chrono::Utc::now().to_rfc3339(),
        size_bytes: image.len(),
        sha256_hash: sha256_hex,
        build_id: image_build_id(&image)?,
        p256_signature: signature.to_string(),
    };
    let metadata_path = ota_dir.join("metadata.json");
    fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)
        .with_context(|| format!("Failed to write metadata file at {:?}", metadata_path))?;
    log::info!("OTA image saved to {:?}", cwd.join(&image_path));

    if args.install {
        log::info!("Installing release build to connected device...");
        firmware::flash(path_str(&elf_path)?)?;
        firmware::monitor()?;
    }
    Ok(())
}

/// The image's build ID: the SHA-256 espflash appends to it, which the
/// firmware reports for its running image (see the firmware's
/// store/app_settings/info.rs).
fn image_build_id(image: &[u8]) -> Result<String> {
    // App image format (ESP-IDF esp_image_format.h)
    const IMAGE_MAGIC: u8 = 0xE9;
    const HEADER_LEN: usize = 24;
    const HASH_APPENDED_BYTE: usize = 23;
    const SEGMENT_HEADER_LEN: usize = 8;
    const HASH_LEN: usize = 32;

    let header = image.get(..HEADER_LEN).context("OTA image too short")?;
    ensure!(header[0] == IMAGE_MAGIC, "Invalid OTA image magic");
    ensure!(
        header[HASH_APPENDED_BYTE] != 0,
        "OTA image has no SHA-256 appended"
    );
    let mut len = HEADER_LEN;
    for _ in 0..header[1] {
        let segment_header = image
            .get(len..len + SEGMENT_HEADER_LEN)
            .context("OTA image truncated in its segments")?;
        let data_len = u32::from_le_bytes(segment_header[4..].try_into()?) as usize;
        len += SEGMENT_HEADER_LEN + data_len;
    }
    // The checksum byte, padded to a full 16 byte block
    len = (len + 1).next_multiple_of(16);
    let appended = image
        .get(len..len + HASH_LEN)
        .context("OTA image truncated before its appended SHA-256")?;
    let build_id = hex::encode(appended);
    ensure!(
        sha256::digest(&image[..len]) == build_id,
        "OTA image's appended SHA-256 doesn't match its contents"
    );
    Ok(build_id)
}

// Linker scripts, and local paths remapped so they don't end up in the binary
fn release_rustflags(cwd: &Path) -> Result<String> {
    let env_var = |name: &str| env::var(name).with_context(|| format!("{name} is not set"));
    let cargo_home = env_var("CARGO_HOME")?;
    let rustup_lib = format!(
        "{}/toolchains/{}/lib/rustlib/src/rust/library",
        env_var("RUSTUP_HOME")?,
        env_var("RUSTUP_TOOLCHAIN")?
    );
    Ok(format!(
        "-C link-arg=-Tlinkall.x -C link-arg=-Tdefmt.x \
         --remap-path-prefix={}=wd/ --remap-path-prefix={}/registry/src=io/ \
         --remap-path-prefix={}/git/checkouts=co/ --remap-path-prefix={}=rl/ ",
        cwd.display(),
        cargo_home,
        cargo_home,
        rustup_lib
    ))
}

fn parse_version(version: &str) -> Result<Version> {
    let parts: Vec<&str> = version.split('.').collect();
    let [major, minor, patch] = parts[..] else {
        bail!("Version string must be in format MAJOR.MINOR.PATCH");
    };
    let parse = |part: &str, name: &str| {
        part.parse::<u32>()
            .with_context(|| format!("Failed to parse {name} version as integer"))
    };
    Ok((
        parse(major, "major")?,
        parse(minor, "minor")?,
        parse(patch, "patch")?,
    ))
}

// A build path in the binaries would reveal the builder's user name
fn ensure_no_home_path(paths: &[&PathBuf]) -> Result<()> {
    let home = env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Ok(());
    }
    for path in paths {
        let data = fs::read(path).with_context(|| format!("Failed to read file at {:?}", path))?;
        if data
            .windows(home.len())
            .any(|window| window == home.as_bytes())
        {
            bail!(
                "User home path found in file {:?}, aborting release build",
                path
            );
        }
    }
    Ok(())
}

/// Signs the update metadata, and verifies the signature with the public
/// key the firmware has compiled in.
fn sign_update(
    product_id: &ProductId,
    (major, minor, patch): Version,
    image_size: u32,
    sha256: &[u8],
) -> Result<Signature> {
    let signing_key_data = fs::read(SIGNING_KEY_PATH)
        .with_context(|| format!("Failed to read OTA signing key at {:?}", SIGNING_KEY_PATH))?;
    let signing_key = SigningKey::from_pkcs8_der(&signing_key_data)
        .with_context(|| format!("Failed to parse OTA signing key at {:?}", SIGNING_KEY_PATH))?;

    // concat([u32le:MAJOR, u32le:MINOR, u32le:PATCH, u32le:SIZE, [u8:32]:SHA256, str:PRODUCT_ID])
    let message = [
        &major.to_le_bytes(),
        &minor.to_le_bytes(),
        &patch.to_le_bytes(),
        &image_size.to_le_bytes(),
        sha256,
        product_id.as_str().as_bytes(),
    ]
    .concat();
    let signature: Signature = signing_key
        .try_sign(&message)
        .map_err(|_| anyhow!("Failed to sign OTA image with provided signing key"))?;

    let public_key_data = fs::read(PUBLIC_KEY_PATH)
        .with_context(|| format!("Failed to read OTA public key at {:?}", PUBLIC_KEY_PATH))?;
    let verifying_key = VerifyingKey::from_public_key_der(&public_key_data)
        .with_context(|| format!("Failed to parse OTA public key at {:?}", PUBLIC_KEY_PATH))?;
    verifying_key
        .verify(&message, &signature)
        .map_err(|_| anyhow!("Failed to verify OTA image signature with provided public key"))?;
    Ok(signature)
}
