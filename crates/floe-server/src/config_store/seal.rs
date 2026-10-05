//! D61: secrets at rest in the config document — AES-256-GCM (`ring`) under the
//! 32-byte key in `FLOE_CONFIG_KEY` (`config_store.key_env`), a random 96-bit
//! nonce per value, the field path as associated data (a sealed value cannot be
//! moved to another field) and a key id so a wrong key fails with a clear
//! message. Format: `v1.<kid>.<base64url(nonce ‖ ciphertext ‖ tag)>`.

use anyhow::{Context, Result};
use base64::Engine;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use sha2::Digest;

/// The sealing key and its id.
pub struct SealKey {
    key: LessSafeKey,
    kid: String,
}

impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SealKey(kid {})", self.kid)
    }
}

impl SealKey {
    /// The key from `var`, base64 of exactly 32 bytes. `Ok(None)` when unset or empty.
    pub fn from_env(var: &str) -> Result<Option<SealKey>> {
        match std::env::var(var) {
            Ok(v) if !v.trim().is_empty() => Self::from_base64(v.trim())
                .with_context(|| var.to_string())
                .map(Some),
            _ => Ok(None),
        }
    }

    /// Parse a base64 (standard or URL-safe, padded or not) 32-byte key.
    pub fn from_base64(text: &str) -> Result<SealKey> {
        let raw = decode_any(text).context("the sealing key is not base64")?;
        anyhow::ensure!(
            raw.len() == 32,
            "the sealing key must be 32 bytes (openssl rand -base64 32), got {}",
            raw.len()
        );
        let kid = hex::encode(sha2::Sha256::digest(&raw))
            .chars()
            .take(8)
            .collect();
        let unbound = UnboundKey::new(&AES_256_GCM, &raw)
            .map_err(|_| anyhow::anyhow!("the sealing key was refused by AES-256-GCM"))?;
        Ok(SealKey {
            key: LessSafeKey::new(unbound),
            kid,
        })
    }

    /// The key id carried by every value this key seals.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Seal `plaintext` for the field at `path`.
    pub fn seal(&self, plaintext: &str, path: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("no randomness for a nonce"))?;
        let mut buf = plaintext.as_bytes().to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(path.as_bytes()),
                &mut buf,
            )
            .map_err(|_| anyhow::anyhow!("sealing {path} failed"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&buf);
        Ok(format!(
            "v1.{}.{}",
            self.kid,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out)
        ))
    }

    /// Open a value sealed for `path`.
    pub fn open(&self, sealed: &str, path: &str) -> Result<String> {
        let mut parts = sealed.splitn(3, '.');
        let (Some("v1"), Some(kid), Some(body)) = (parts.next(), parts.next(), parts.next()) else {
            anyhow::bail!("{path}: not a sealed value (expected v1.<kid>.<data>)");
        };
        anyhow::ensure!(
            kid == self.kid,
            "{path} was sealed with another key (kid {kid}; this key is {}): re-enter the secret",
            self.kid
        );
        let data =
            decode_any(body).with_context(|| format!("{path}: sealed data is not base64"))?;
        let (nonce, ct) = data
            .split_at_checked(NONCE_LEN)
            .ok_or_else(|| anyhow::anyhow!("{path}: sealed data is truncated"))?;
        let nonce: [u8; NONCE_LEN] = nonce
            .try_into()
            .map_err(|_| anyhow::anyhow!("{path}: bad nonce"))?;
        let mut buf = ct.to_vec();
        let plain = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(path.as_bytes()),
                &mut buf,
            )
            .map_err(|_| {
                anyhow::anyhow!("cannot open {path}: wrong key, wrong field or a tampered value")
            })?;
        String::from_utf8(plain.to_vec()).with_context(|| format!("{path}: not UTF-8"))
    }
}

fn decode_any(text: &str) -> Result<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    for engine in [&STANDARD, &URL_SAFE, &STANDARD_NO_PAD, &URL_SAFE_NO_PAD] {
        if let Ok(v) = engine.decode(text) {
            return Ok(v);
        }
    }
    anyhow::bail!("invalid base64")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="; // 32 ASCII bytes

    #[test]
    fn seal_open_round_trip_and_failures() {
        let k = SealKey::from_base64(KEY).unwrap();
        let s = k.seal("ghp_secret", "github_mirror.token").unwrap();
        assert!(s.starts_with(&format!("v1.{}.", k.kid())), "{s}");
        assert!(!s.contains("ghp_secret"));
        assert_ne!(
            s,
            k.seal("ghp_secret", "github_mirror.token").unwrap(),
            "fresh nonce"
        );
        assert_eq!(k.open(&s, "github_mirror.token").unwrap(), "ghp_secret");
        // Moved to another field: refused.
        assert!(k.open(&s, "events.webhook_secret").is_err());
        // Another key: a clear message.
        let other =
            SealKey::from_base64(&base64::engine::general_purpose::STANDARD.encode([7u8; 32]))
                .unwrap();
        let err = other
            .open(&s, "github_mirror.token")
            .unwrap_err()
            .to_string();
        assert!(err.contains("another key"), "{err}");
        // Tampered.
        let mut t = s.clone();
        let last = t.pop().unwrap();
        t.push(if last == 'A' { 'B' } else { 'A' });
        assert!(k.open(&t, "github_mirror.token").is_err());
        assert!(k.open("v2.x.y", "p").is_err());
        assert!(SealKey::from_base64("c2hvcnQ=").is_err(), "short key");
    }
}
