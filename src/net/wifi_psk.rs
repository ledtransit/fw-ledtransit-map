// The WPA2 pre-shared key, derived from the network's password: the WiFi
// driver's own derivation (PBKDF2-HMAC-SHA1, 4096 rounds) takes about 0.9 s in
// its thread, above the executor's, so nothing else runs meanwhile (the LEDs
// freeze). Given the key as 64 hex digits, the driver skips it. Derived here
// in slices instead, once per network (the key is stored with the password)
use core::fmt::Write;

use embassy_futures::yield_now;
use sha1_smol::Sha1;

const PSK_ROUNDS: u32 = 4096;
const PSK_BYTES: usize = 32;
// Rounds in a row (two SHA-1 blocks each, about 0.1 ms)
const ROUNDS_PER_SLICE: u32 = 4;
const SHA1_BLOCK_BYTES: usize = 64;
const SHA1_DIGEST_BYTES: usize = 20;

pub type Psk = [u8; PSK_BYTES];

/// The pre-shared key for the network and password (PBKDF2-HMAC-SHA1 of the
/// password, salted with the SSID), yielding between slices of rounds.
pub async fn derive_psk(ssid: &str, password: &str) -> Psk {
    let hmac = HmacSha1::new(password.as_bytes());
    let mut psk = [0; PSK_BYTES];
    for (index, chunk) in psk.chunks_mut(SHA1_DIGEST_BYTES).enumerate() {
        let block_index = (index as u32 + 1).to_be_bytes();
        let mut u = hmac.digest(&[ssid.as_bytes(), &block_index]);
        let mut t = u;
        for round in 1..PSK_ROUNDS {
            u = hmac.digest(&[&u]);
            t.iter_mut().zip(u).for_each(|(t, u)| *t ^= u);
            if round % ROUNDS_PER_SLICE == 0 {
                yield_now().await;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
    psk
}

/// The key as the driver takes it in place of the password: 64 hex digits.
pub fn psk_hex(psk: &Psk) -> heapless::String<64> {
    let mut hex = heapless::String::new();
    psk.iter()
        .for_each(|byte| write!(hex, "{:02x}", byte).expect("PSK hex fits"));
    hex
}

// HMAC-SHA1 with the key's padded blocks hashed once, not for every message
struct HmacSha1 {
    inner: Sha1,
    outer: Sha1,
}

impl HmacSha1 {
    // Keys longer than a block aren't hashed first: WPA2 passwords are 63
    // bytes at most
    fn new(key: &[u8]) -> Self {
        let mut inner_pad = [0x36; SHA1_BLOCK_BYTES];
        let mut outer_pad = [0x5c; SHA1_BLOCK_BYTES];
        for (i, byte) in key.iter().take(SHA1_BLOCK_BYTES).enumerate() {
            inner_pad[i] ^= byte;
            outer_pad[i] ^= byte;
        }
        let mut inner = Sha1::new();
        inner.update(&inner_pad);
        let mut outer = Sha1::new();
        outer.update(&outer_pad);
        Self { inner, outer }
    }

    fn digest(&self, message: &[&[u8]]) -> [u8; SHA1_DIGEST_BYTES] {
        let mut inner = self.inner.clone();
        message.iter().for_each(|part| inner.update(part));
        let mut outer = self.outer.clone();
        outer.update(&inner.digest().bytes());
        outer.digest().bytes()
    }
}
