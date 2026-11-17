//! Certificate pinning ("trust on first use") for devices with self-signed HTTPS certificates
//! (Proxmox VE, iDRAC, and HTTPS consoles in general). Instead of a CA check, the SHA-256
//! fingerprint of the certificate seen on first contact is saved with the device; later
//! connections are refused if the certificate differs. See also `console_tls` for the same check
//! in console webviews.

use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};


/// `AB:CD:…` SHA-256 fingerprint, formatted like Proxmox shows it (node > System > Certificates).
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

#[derive(Debug)]
struct PinVerifier {
    expected: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity.as_ref());
        *self.seen.lock().unwrap() = Some(fp.clone());
        match &self.expected {
            Some(expected) if *expected != fp => Err(rustls::Error::General("certificate fingerprint changed".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// TLS setup for one server, remembering which certificate it presented.
pub struct Tls {
    pub config: Arc<ClientConfig>,
    verifier: Arc<PinVerifier>,
}

impl Tls {
    pub fn new(expected: Option<String>) -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinVerifier { expected, seen: Mutex::new(None), provider: provider.clone() });
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth();
        Ok(Tls { config: Arc::new(config), verifier })
    }

    pub fn seen(&self) -> Option<String> {
        self.verifier.seen.lock().unwrap().clone()
    }

    /// Turns a failed request into a clear message when the certificate is the reason.
    pub fn explain(&self, err: String) -> String {
        match (&self.verifier.expected, self.seen()) {
            (Some(expected), Some(seen)) if *expected != seen => format!(
                "The server's certificate has changed, so the app refused to connect. New fingerprint: {seen}. \
                 If you expected this (e.g. a renewed certificate), use Edit > Forget certificate."
            ),
            _ => err,
        }
    }

    pub fn http(&self) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .use_preconfigured_tls((*self.config).clone())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())
    }
}

/// Result of talking to a server, plus the certificate fingerprint it presented (to pin it).
pub struct Reply<T> {
    pub result: Result<T, String>,
    pub fingerprint: Option<String>,
}


#[cfg(test)]
mod tests {
    #[test]
    fn formats_fingerprints_like_proxmox() {
        let fp = super::fingerprint(b"abc");
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(fp.starts_with("BA:78:16:BF:"));
    }
}
