//! Encrypted export/import of the device list (devices with their passwords, and groups).
//!
//! File format (JSON):
//! `{"format": "nanokvm-control-export", "version": 1, "kdf": {"name": "argon2id", "salt", "m", "t", "p"},
//!   "cipher": "aes-256-gcm", "nonce", "data"}` (salt, nonce, data base64). The key is derived from the
//! passphrase with Argon2id; `data` is the AES-256-GCM-encrypted JSON of [`Contents`], so a wrong
//! passphrase or a modified file is detected.

use crate::store::{Device, Group};
use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};

const FORMAT: &str = "nanokvm-control-export";
pub const EXTENSION: &str = "nkcbackup";
/// Short passphrases make offline guessing easy.
pub const MIN_PASSPHRASE: usize = 8;

#[derive(Serialize, Deserialize)]
pub struct Contents {
    pub devices: Vec<Device>,
    pub groups: Vec<Group>,
}

#[derive(Serialize, Deserialize)]
struct Kdf {
    name: String,
    salt: String,
    m: u32,
    t: u32,
    p: u32,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    format: String,
    version: u32,
    kdf: Kdf,
    cipher: String,
    nonce: String,
    data: String,
}

fn derive_key(passphrase: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<[u8; 32], String> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| e.to_string())?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| e.to_string())?;
    Ok(key)
}

pub fn encrypt(contents: &Contents, passphrase: &str) -> Result<String, String> {
    if passphrase.chars().count() < MIN_PASSPHRASE {
        return Err(format!("Use a passphrase of at least {MIN_PASSPHRASE} characters"));
    }
    let (m, t, p) = (64 * 1024, 3, 1); // 64 MiB, 3 passes
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let key = derive_key(passphrase, &salt, m, t, p)?;
    let plain = serde_json::to_vec(contents).map_err(|e| e.to_string())?;
    let data = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| e.to_string())?
        .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
        .map_err(|_| "Encryption failed".to_string())?;
    let envelope = Envelope {
        format: FORMAT.into(),
        version: 1,
        kdf: Kdf { name: "argon2id".into(), salt: B64.encode(salt), m, t, p },
        cipher: "aes-256-gcm".into(),
        nonce: B64.encode(nonce),
        data: B64.encode(data),
    };
    serde_json::to_string_pretty(&envelope).map_err(|e| e.to_string())
}

pub fn decrypt(text: &str, passphrase: &str) -> Result<Contents, String> {
    let env: Envelope = serde_json::from_str(text).map_err(|_| "This isn't a NanoKVM Control export file".to_string())?;
    if env.format != FORMAT || env.version != 1 || env.kdf.name != "argon2id" || env.cipher != "aes-256-gcm" {
        return Err("This export file has an unsupported format".into());
    }
    // Refuse absurd parameters (a crafted file could otherwise make us allocate a lot of memory).
    if env.kdf.m > 1024 * 1024 || env.kdf.t > 16 || env.kdf.p > 8 {
        return Err("This export file has unsupported key settings".into());
    }
    let salt = B64.decode(&env.kdf.salt).map_err(|_| "Damaged export file")?;
    let nonce = B64.decode(&env.nonce).map_err(|_| "Damaged export file")?;
    let data = B64.decode(&env.data).map_err(|_| "Damaged export file")?;
    if nonce.len() != 12 {
        return Err("Damaged export file".into());
    }
    let key = derive_key(passphrase, &salt, env.kdf.m, env.kdf.t, env.kdf.p)?;
    let plain = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| e.to_string())?
        .decrypt(Nonce::from_slice(&nonce), data.as_slice())
        .map_err(|_| "Wrong passphrase, or the file was changed".to_string())?;
    serde_json::from_slice(&plain).map_err(|_| "The export file's contents are damaged".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Contents {
        let device: Device = serde_json::from_str(
            r#"{"id":"a","name":"x","url":"http://192.0.2.1","username":"u","password":"s3cret","group":"G"}"#,
        )
        .unwrap();
        Contents { devices: vec![device], groups: vec![Group { id: "g".into(), name: "G".into() }] }
    }

    #[test]
    fn round_trip_and_wrong_passphrase() {
        let file = encrypt(&sample(), "correct horse").unwrap();
        assert!(!file.contains("s3cret"));
        let back = decrypt(&file, "correct horse").unwrap();
        assert_eq!(back.devices[0].password, "s3cret");
        assert_eq!(back.groups[0].name, "G");
        match decrypt(&file, "wrong horse!") {
            Err(e) => assert!(e.contains("Wrong passphrase"), "{e}"),
            Ok(_) => panic!("decrypted with the wrong passphrase"),
        }
    }

    #[test]
    fn rejects_short_passphrases_and_tampering() {
        assert!(encrypt(&sample(), "short").is_err());
        let file = encrypt(&sample(), "correct horse").unwrap();
        let mut env: serde_json::Value = serde_json::from_str(&file).unwrap();
        let data = env["data"].as_str().unwrap().to_string();
        let flipped = if data.starts_with('A') { format!("B{}", &data[1..]) } else { format!("A{}", &data[1..]) };
        env["data"] = flipped.into();
        assert!(decrypt(&env.to_string(), "correct horse").is_err());
        assert!(decrypt("{}", "correct horse").is_err());
    }
}
