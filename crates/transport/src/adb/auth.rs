//! ADB RSA authentication.
//!
//! Signing matches adbd (`RSA_sign` SHA-1 / PKCS#1 v1.5). The public key blob is
//! Android's `RSAPublicKey` struct (not PKCS#1), which is what the on-device
//! authorization prompt actually checks.

use std::fs;
use std::path::PathBuf;

use base64::{engine::general_purpose, Engine as _};
use rsa::pkcs1::{DecodeRsaPrivateKey, EncodeRsaPrivateKey};
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, LineEnding};
use rsa::signature::{SignatureEncoding, Signer};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;

use super::protocol::AdbError;

const MODULUS_BYTES: usize = 256;
const ANDROID_PUBKEY_LEN: usize = 4 + 4 + MODULUS_BYTES + MODULUS_BYTES + 4;

pub struct AdbKeyPair {
    private_key: RsaPrivateKey,
    public_key: RsaPublicKey,
}

impl AdbKeyPair {
    pub fn generate() -> Result<Self, AdbError> {
        let mut rng = rsa::rand_core::OsRng;
        let private_key = RsaPrivateKey::new(&mut rng, 2048)
            .map_err(|e| AdbError::Auth(format!("generate: {e}")))?;
        let public_key = RsaPublicKey::from(&private_key);
        Ok(Self {
            private_key,
            public_key,
        })
    }

    pub fn from_pem(pem: &str) -> Result<Self, AdbError> {
        let private_key = if let Ok(key) = RsaPrivateKey::from_pkcs1_pem(pem) {
            key
        } else {
            RsaPrivateKey::from_pkcs8_pem(pem)
                .map_err(|e| AdbError::Auth(format!("parse pem: {e}")))?
        };
        let public_key = RsaPublicKey::from(&private_key);
        Ok(Self {
            private_key,
            public_key,
        })
    }

    pub fn is_2048(&self) -> bool {
        self.public_key.n().to_bytes_be().len() == MODULUS_BYTES
    }

    pub fn sign_token(&self, token: &[u8]) -> Result<Vec<u8>, AdbError> {
        let signing_key = SigningKey::<Sha1>::new(self.private_key.clone());
        let signature = signing_key.sign(token);
        Ok(signature.to_bytes().as_ref().to_vec())
    }

    pub fn private_key_pem(&self) -> Result<String, AdbError> {
        self.private_key
            .to_pkcs1_pem(LineEnding::LF)
            .map(|p| p.to_string())
            .map_err(|e| AdbError::Auth(format!("encode pem: {e}")))
    }

    /// `base64(android RSAPublicKey) || " " || name || "\0"`.
    pub fn android_public_key(&self, name: &str) -> Result<Vec<u8>, AdbError> {
        let raw = encode_android_pubkey(&self.public_key)?;
        let mut out = general_purpose::STANDARD.encode(raw).into_bytes();
        out.push(b' ');
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        Ok(out)
    }
}

/// Load `~/.android/adbkey` when it is a 2048-bit key the user already authorized,
/// otherwise `~/.droidmirror/adbkey`, otherwise generate one (the device will prompt).
pub fn load_or_create_key() -> Result<AdbKeyPair, AdbError> {
    if let Some(home) = home_dir() {
        let android = home.join(".android").join("adbkey");
        if let Some(key) = try_load(&android) {
            if key.is_2048() {
                log::info!("using existing ADB key {}", android.display());
                return Ok(key);
            }
            log::warn!(
                "{} is not 2048-bit; using a droidmirror key instead",
                android.display()
            );
        }
        let ours = home.join(".droidmirror").join("adbkey");
        if let Some(key) = try_load(&ours) {
            return Ok(key);
        }
        let key = AdbKeyPair::generate()?;
        if let Some(parent) = ours.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match key.private_key_pem() {
            Ok(pem) => {
                if fs::write(&ours, pem).is_ok() {
                    log::info!("wrote new ADB key to {} — accept the RSA prompt on the device", ours.display());
                }
            }
            Err(e) => log::warn!("could not persist ADB key: {e}"),
        }
        return Ok(key);
    }
    AdbKeyPair::generate()
}

fn try_load(path: &PathBuf) -> Option<AdbKeyPair> {
    let pem = fs::read_to_string(path).ok()?;
    AdbKeyPair::from_pem(&pem).ok()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub fn encode_android_pubkey(key: &RsaPublicKey) -> Result<Vec<u8>, AdbError> {
    let n = key.n();
    let mut modulus = n.to_bytes_le();
    if modulus.len() > MODULUS_BYTES {
        return Err(AdbError::Auth(format!(
            "modulus is {} bytes; ADB wants 2048-bit keys",
            modulus.len()
        )));
    }
    modulus.resize(MODULUS_BYTES, 0);

    let n0 = u32::from_le_bytes(modulus[0..4].try_into().unwrap());
    let inv = mod_inverse_2_32(n0).ok_or_else(|| AdbError::Auth("modulus is even".into()))?;
    let n0inv = inv.wrapping_neg();

    let r = BigUint::from(1u8) << (MODULUS_BYTES * 8);
    let rr_mod = (&r * &r) % n;
    let mut rr = rr_mod.to_bytes_le();
    if rr.len() > MODULUS_BYTES {
        return Err(AdbError::Auth("rr overflow".into()));
    }
    rr.resize(MODULUS_BYTES, 0);

    let exp_le = key.e().to_bytes_le();
    let mut exp_buf = [0u8; 4];
    let ncopy = exp_le.len().min(4);
    exp_buf[..ncopy].copy_from_slice(&exp_le[..ncopy]);
    let exponent = u32::from_le_bytes(exp_buf);

    let mut out = Vec::with_capacity(ANDROID_PUBKEY_LEN);
    out.extend_from_slice(&((MODULUS_BYTES / 4) as u32).to_le_bytes());
    out.extend_from_slice(&n0inv.to_le_bytes());
    out.extend_from_slice(&modulus);
    out.extend_from_slice(&rr);
    out.extend_from_slice(&exponent.to_le_bytes());
    debug_assert_eq!(out.len(), ANDROID_PUBKEY_LEN);
    Ok(out)
}

fn mod_inverse_2_32(a: u32) -> Option<u32> {
    if a & 1 == 0 {
        return None;
    }
    let mut x = 1u32;
    for _ in 0..5 {
        x = x.wrapping_mul(2u32.wrapping_sub(a.wrapping_mul(x)));
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pubkey_shape_and_n0inv() {
        let key = AdbKeyPair::generate().unwrap();
        let raw = encode_android_pubkey(&key.public_key).unwrap();
        assert_eq!(raw.len(), ANDROID_PUBKEY_LEN);
        let words = u32::from_le_bytes(raw[0..4].try_into().unwrap());
        assert_eq!(words, 64);
        let n0 = u32::from_le_bytes(key.public_key.n().to_bytes_le()[0..4].try_into().unwrap());
        let n0inv = u32::from_le_bytes(raw[4..8].try_into().unwrap());
        assert_eq!(n0.wrapping_mul(n0inv), u32::MAX);
        let blob = key.android_public_key("droidmirror@host").unwrap();
        assert!(blob.ends_with(&[0]));
        assert!(blob.windows(1).any(|b| b == b" "));
        let sig = key.sign_token(&[1, 2, 3, 4]).unwrap();
        assert_eq!(sig.len(), MODULUS_BYTES);
    }
}
