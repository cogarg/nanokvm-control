//! Self-signed HTTPS certificates in console webviews (NanoKVM, PiKVM, iDRAC), with the same
//! "trust on first use" pinning as for Proxmox (see `tls`): the first certificate a device presents
//! is pinned; later ones must match, otherwise the console isn't loaded.
//!
//! - Linux (WebKitGTK): `load-failed-with-tls-errors` hands over the certificate; if it's accepted,
//!   it is allowed for that host in the webview's context and the page is loaded again. That also
//!   covers the page's WebSocket and other requests to the same host.
//! - Windows (WebView2): `ServerCertificateErrorDetected`, answered with "always allow" or "cancel".
//! - macOS (WKWebView): Tauri gives no hook for this, so self-signed consoles aren't supported there.
//!
//! The hook must be installed before the console URL is loaded, so console webviews with HTTPS are
//! created on `about:blank` and navigated afterwards.

use std::sync::Arc;

/// What to do with a certificate (by SHA-256 fingerprint).
pub enum Verdict {
    Allow,
    /// Refuse, with an explanation for the user.
    Refuse(String),
}

pub type Check = Arc<dyn Fn(&str) -> Verdict + Send + Sync>;

#[cfg(target_os = "linux")]
pub fn install(webview: &webkit2gtk::WebView, check: Check) {
    use webkit2gtk::{gio::prelude::TlsCertificateExt, WebContextExt, WebViewExt};
    webview.connect_load_failed_with_tls_errors(move |wv, uri, cert, _errors| {
        let Some(der) = cert.certificate() else { return false };
        let fp = crate::tls::fingerprint(&der);
        match check(&fp) {
            Verdict::Allow => {
                let host = tauri::Url::parse(uri).ok().and_then(|u| u.host_str().map(str::to_string));
                if let (Some(ctx), Some(host)) = (wv.context(), host) {
                    ctx.allow_tls_certificate_for_host(cert, &host);
                    wv.load_uri(uri);
                    return true;
                }
                false
            }
            Verdict::Refuse(message) => {
                wv.load_alternate_html(&error_page(&message), uri, None);
                true
            }
        }
    });
}

#[cfg(windows)]
pub fn install(webview: &tauri::webview::PlatformWebview, check: Check) -> Result<(), String> {
    use webview2_com::{
        take_pwstr, Microsoft::Web::WebView2::Win32::*, ServerCertificateErrorDetectedEventHandler,
    };
    use windows_core::{Interface, PWSTR};
    unsafe {
        let core = webview.controller().CoreWebView2().map_err(|e| e.to_string())?;
        let core14: ICoreWebView2_14 = core.cast().map_err(|e| e.to_string())?;
        let handler = ServerCertificateErrorDetectedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            let cert = args.ServerCertificate()?;
            let mut pem = PWSTR::null();
            cert.ToPemEncoding(&mut pem)?;
            let der = pem_to_der(&take_pwstr(pem));
            let action = match der.map(|d| check(&crate::tls::fingerprint(&d))) {
                Some(Verdict::Allow) => COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW,
                _ => COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_CANCEL,
            };
            args.SetAction(action)
        }));
        let mut token = 0i64;
        core14.add_ServerCertificateErrorDetected(&handler, &mut token).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The DER bytes of the first certificate in a PEM text.
#[cfg_attr(not(windows), allow(dead_code))]
fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN CERTIFICATE-----"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    base64::engine::general_purpose::STANDARD.decode(body.trim()).ok()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn error_page(message: &str) -> String {
    let escaped = message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<!doctype html><meta charset=utf-8><title>Certificate changed</title>\
         <body style=\"font:15px/1.5 system-ui,sans-serif;background:#12151c;color:#e6e8ee;padding:40px;max-width:720px\">\
         <h2 style=\"color:#ef6b5f\">Console not loaded: the certificate changed</h2><p>{escaped}</p></body>"
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_der_from_pem() {
        let pem = "-----BEGIN CERTIFICATE-----\r\nYWJj\r\n-----END CERTIFICATE-----\r\n";
        assert_eq!(super::pem_to_der(pem).unwrap(), b"abc");
    }
}
