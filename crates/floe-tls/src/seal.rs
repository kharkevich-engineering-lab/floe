//! Sealing private keys at rest (the ACME account key, the certificate's key) with a key
//! from the bootstrap environment (`server.tls.acme.storage_key_env`).
//!
//! **AES-256-GCM through `ring`.** `ring` is already in the tree (it is the listener's rustls
//! provider), so this adds no dependency; its AEAD is `BoringSSL`'s, constant-time and
//! hardware-accelerated on both arches floe ships (AES-NI + CLMUL, `ARMv8` crypto extensions).
//! GCM's one sharp edge is the 96-bit nonce: a random nonce is safe up to ~2^32 messages per
//! key, and this key seals a handful of objects per renewal (tens per year), so random nonces
//! from the system CSPRNG are far inside the bound without any nonce state — which matters,
//! because instances keep no state (principle I) and could not keep a counter.
//! `XChaCha20-Poly1305` would lift that bound but is a new crate for no practical gain here.
//!
//! Format: `v1.` + base64(`key_id[8] ‖ nonce[12] ‖ ciphertext ‖ tag[16]`). `key_id` is a
//! fingerprint of the key so a wrong key is reported as such, not as corruption. The caller
//! binds every sealed value to where it lives (associated data = object key + field), so a
//! sealed blob copied into another object does not open.

use anyhow::{Context, Result};
use base64::Engine;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use sha2::Digest;

const PREFIX: &str = "v1.";
const KEY_ID_LEN: usize = 8;

/// A 256-bit sealing key. `Debug` never prints the key.
pub struct SealKey {
    key: LessSafeKey,
    id: [u8; KEY_ID_LEN],
}

impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealKey")
            .field("id", &hex::encode(self.id))
            .finish_non_exhaustive()
    }
}

impl SealKey {
    /// Read the key from the environment variable `var` (fail closed: unset, empty or not
    /// 32 bytes is an error naming the variable, never its value).
    pub fn from_env(var: &str) -> Result<Self> {
        let raw = std::env::var(var).with_context(|| {
            format!("{var} is not set: server.tls.acme.storage_key_env names the 32-byte key (base64 or hex) that seals private keys in the bucket; generate one with `openssl rand -base64 32`")
        })?;
        Self::parse(raw.trim()).with_context(|| format!("{var} does not hold a usable key"))
    }

    /// 32 bytes as base64 (standard or URL-safe, padding optional) or 64 hex digits.
    pub fn parse(text: &str) -> Result<Self> {
        let bytes = if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            hex::decode(text).context("hex key")?
        } else {
            let t = text.trim_end_matches('=');
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(t)
                .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(t))
                .context("the key is neither 64 hex digits nor base64")?
        };
        anyhow::ensure!(
            bytes.len() == 32,
            "the key must be exactly 32 bytes (got {})",
            bytes.len()
        );
        Self::from_bytes(&bytes)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let unbound = UnboundKey::new(&AES_256_GCM, bytes)
            .map_err(|_| anyhow::anyhow!("AES-256-GCM rejected the key"))?;
        let mut h = sha2::Sha256::new();
        h.update(b"floe-tls-seal-key-id\0");
        h.update(bytes);
        let digest = h.finalize();
        let mut id = [0u8; KEY_ID_LEN];
        for (d, s) in id.iter_mut().zip(digest.iter()) {
            *d = *s;
        }
        Ok(SealKey {
            key: LessSafeKey::new(unbound),
            id,
        })
    }

    /// Seal `plaintext`, bound to `aad` (where the value lives).
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("system RNG failed"))?;
        let mut buf = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut buf,
            )
            .map_err(|_| anyhow::anyhow!("sealing failed"))?;
        let mut out = Vec::with_capacity(KEY_ID_LEN + NONCE_LEN + buf.len());
        out.extend_from_slice(&self.id);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&buf);
        Ok(format!(
            "{PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(out)
        ))
    }

    /// Open a value sealed by [`SealKey::seal`] with the same `aad`.
    pub fn open(&self, aad: &[u8], sealed: &str) -> Result<Vec<u8>> {
        let body = sealed
            .strip_prefix(PREFIX)
            .context("sealed value has an unknown format")?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(body)
            .context("sealed value is not base64")?;
        anyhow::ensure!(
            raw.len() >= KEY_ID_LEN + NONCE_LEN + AES_256_GCM.tag_len(),
            "sealed value is truncated"
        );
        let (id, rest) = raw.split_at(KEY_ID_LEN);
        anyhow::ensure!(
            id == self.id,
            "sealed with a different storage key (key id {} ≠ {}): check server.tls.acme.storage_key_env",
            hex::encode(id),
            hex::encode(self.id)
        );
        let (nonce, ct) = rest.split_at(NONCE_LEN);
        let nonce =
            Nonce::try_assume_unique_for_key(nonce).map_err(|_| anyhow::anyhow!("bad nonce"))?;
        let mut buf = ct.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(aad), &mut buf)
            .map_err(|_| {
                anyhow::anyhow!(
                    "sealed value failed authentication (tampered, or moved from another object)"
                )
            })?;
        Ok(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    #[test]
    fn round_trip_and_binding() {
        let k = SealKey::parse(B64).unwrap();
        let s = k
            .seal(b"tls/acme/x/cert.json#key", b"-----BEGIN PRIVATE KEY-----")
            .unwrap();
        assert!(s.starts_with("v1."));
        assert!(!s.contains("PRIVATE"));
        assert_eq!(
            k.open(b"tls/acme/x/cert.json#key", &s).unwrap(),
            b"-----BEGIN PRIVATE KEY-----"
        );
        // Fresh nonce every time.
        assert_ne!(
            s,
            k.seal(b"tls/acme/x/cert.json#key", b"-----BEGIN PRIVATE KEY-----")
                .unwrap()
        );
        // Bound to its place.
        assert!(k.open(b"tls/acme/y/cert.json#key", &s).is_err());
        // Tampering is detected.
        let mut raw = base64::engine::general_purpose::STANDARD
            .decode(s.strip_prefix("v1.").unwrap())
            .unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        let bad = format!(
            "v1.{}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        );
        assert!(k.open(b"tls/acme/x/cert.json#key", &bad).is_err());
    }

    #[test]
    fn wrong_key_is_named_and_formats_parse() {
        let a = SealKey::parse(B64).unwrap();
        let hex_key = "1f".repeat(32);
        let b = SealKey::parse(&hex_key).unwrap();
        let s = a.seal(b"k", b"secret").unwrap();
        let e = b.open(b"k", &s).unwrap_err().to_string();
        assert!(e.contains("different storage key"), "{e}");
        assert!(
            SealKey::parse(B64.trim_end_matches('=')).is_ok(),
            "unpadded"
        );
        assert!(SealKey::parse("c2hvcnQ=").is_err(), "too short");
        assert!(SealKey::parse("not a key at all!").is_err());
        assert!(!format!("{a:?}").contains(B64));
    }
}
