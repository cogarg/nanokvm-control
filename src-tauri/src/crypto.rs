//! Password encoding expected by the NanoKVM login API.
//!
//! The NanoKVM web UI runs `encodeURIComponent(CryptoJS.AES.encrypt(pw, "nanokvm-sipeed-2024"))`
//! before posting to `/api/auth/login`. The key is hard-coded on both sides, so this is
//! obfuscation, not security. CryptoJS passphrase mode uses the OpenSSL format:
//! base64("Salted__" || salt[8] || AES-256-CBC(PKCS7)), with key and IV derived by
//! EVP_BytesToKey(MD5, 1 iteration).

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use base64::{engine::general_purpose::STANDARD, Engine};
use md5::{Digest, Md5};

const NANOKVM_KEY: &[u8] = b"nanokvm-sipeed-2024";

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;

/// EVP_BytesToKey with MD5, producing a 32-byte key and a 16-byte IV.
fn evp_bytes_to_key(pass: &[u8], salt: &[u8; 8]) -> ([u8; 32], [u8; 16]) {
    let mut out = Vec::with_capacity(48);
    let mut prev: Vec<u8> = Vec::new();
    while out.len() < 48 {
        let mut h = Md5::new();
        h.update(&prev);
        h.update(pass);
        h.update(salt);
        prev = h.finalize().to_vec();
        out.extend_from_slice(&prev);
    }
    let mut key = [0u8; 32];
    let mut iv = [0u8; 16];
    key.copy_from_slice(&out[..32]);
    iv.copy_from_slice(&out[32..48]);
    (key, iv)
}

fn openssl_encrypt(plain: &str, salt: [u8; 8]) -> String {
    let (key, iv) = evp_bytes_to_key(NANOKVM_KEY, &salt);
    let ct = Aes256CbcEnc::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes());
    let mut blob = Vec::with_capacity(16 + ct.len());
    blob.extend_from_slice(b"Salted__");
    blob.extend_from_slice(&salt);
    blob.extend_from_slice(&ct);
    STANDARD.encode(blob)
}

/// Encodes a password exactly as the NanoKVM web UI does before login.
pub fn encode_password(plain: &str) -> String {
    urlencoding::encode(&openssl_encrypt(plain, rand::random())).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_openssl_salted_header() {
        let b64 = openssl_encrypt("admin", [1, 2, 3, 4, 5, 6, 7, 8]);
        let raw = STANDARD.decode(&b64).unwrap();
        assert_eq!(&raw[..8], b"Salted__");
        assert_eq!(&raw[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(raw.len(), 32); // 16 header + one AES block for "admin"
    }

    #[test]
    fn url_encoded() {
        let enc = encode_password("hunter2");
        assert!(!enc.contains('+') && !enc.contains('/') && !enc.contains('='));
    }

    /// Prints a sample for manual cross-checking with:
    /// `echo <b64> | openssl enc -d -aes-256-cbc -md md5 -a -A -k nanokvm-sipeed-2024`
    #[test]
    fn print_sample() {
        println!("SAMPLE={}", openssl_encrypt("admin", rand::random()));
    }
}
