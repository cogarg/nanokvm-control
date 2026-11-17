//! Dell iDRAC 9 virtual console (HTML5).
//!
//! On each connect: log in to the iDRAC web interface (`/sysmgmt/2015/bmc/session`), ask it for
//! the virtual console launch URL (`/sysmgmt/2015/server/vconsole`, which carries one-time console
//! keys), and log out again right away; the console URL keeps working after that. The console
//! page itself is the iDRAC's own, loaded in the console webview (with the certificate pinned via
//! `console_tls`).
//!
//! Requests use the device's pinned certificate (see `tls`), so the password is never sent to a
//! server presenting a different certificate. A failed login is never retried: iDRACs block an
//! address after repeated failures.

use crate::tls::{Reply, Tls};
use reqwest::header::HeaderValue;
use serde::Deserialize;

pub struct Idrac {
    /// Origin of the web interface, e.g. `https://idrac.example`.
    pub base: String,
    pub username: String,
    pub password: String,
    pub fingerprint: Option<String>,
}

#[derive(Deserialize)]
struct LoginReply {
    #[serde(rename = "authResult")]
    auth_result: Option<i64>,
}

#[derive(Deserialize)]
struct VconsoleReply {
    #[serde(rename = "Location")]
    location: Option<String>,
}

/// The iDRAC web interface sends the credentials as JSON strings in headers.
fn quoted(value: &str) -> Result<HeaderValue, String> {
    let json = serde_json::to_string(value).map_err(|e| e.to_string())?;
    HeaderValue::from_bytes(json.as_bytes()).map_err(|_| "The username or password contains characters iDRAC can't take".into())
}

fn login_error(code: i64) -> String {
    match code {
        // The web UI asks for a password change (default password) or extra steps for these.
        7 | 9 => "iDRAC wants this account's password changed first; log in once in its web interface".into(),
        _ => format!("iDRAC rejected the login (code {code}); check the saved username and password"),
    }
}

/// Logs in, gets the virtual console URL, logs out.
pub async fn console_url(dev: &Idrac) -> Reply<String> {
    let tls = match Tls::new(dev.fingerprint.clone()) {
        Ok(t) => t,
        Err(e) => return Reply { result: Err(e), fingerprint: None },
    };
    let result = async {
        let http = tls.http()?;
        let session = format!("{}/sysmgmt/2015/bmc/session", dev.base);
        let resp = http
            .post(&session)
            .header("user", quoted(&dev.username)?)
            .header("password", quoted(&dev.password)?)
            .send()
            .await
            .map_err(|e| format!("Could not reach the iDRAC ({e})"))?;
        let xsrf = resp.headers().get("XSRF-TOKEN").cloned();
        // The session is the XSRF token plus an HttpOnly session cookie; send both back.
        let cookies = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .map(str::trim)
            .filter(|c| c.contains('=') && !c.ends_with('='))
            .collect::<Vec<_>>()
            .join("; ");
        let status = resp.status();
        let reply: Option<LoginReply> = resp.json().await.ok();
        match reply.and_then(|r| r.auth_result) {
            Some(0) if status.is_success() => {}
            Some(code) => return Err(login_error(code)),
            None => return Err(format!("Unexpected login reply from the iDRAC (HTTP {status}); is this an iDRAC 9?")),
        }
        let xsrf = xsrf.ok_or("The iDRAC didn't return a session token")?;
        let launch = async {
            let resp = http
                .get(format!("{}/sysmgmt/2015/server/vconsole", dev.base))
                .header("XSRF-TOKEN", xsrf.clone())
                .header(reqwest::header::COOKIE, cookies.clone())
                .send()
                .await
                .map_err(|e| format!("Could not start the virtual console ({e})"))?;
            if !resp.status().is_success() {
                return Err(format!(
                    "The iDRAC refused the virtual console (HTTP {}); the account may lack the console privilege, \
                     or the license doesn't include it",
                    resp.status()
                ));
            }
            resp.json::<VconsoleReply>()
                .await
                .ok()
                .and_then(|r| r.location)
                .filter(|l| l.starts_with("https://"))
                .ok_or_else(|| "The iDRAC didn't return a virtual console address".to_string())
        }
        .await;
        // Log out either way; the console URL stays valid.
        let _ = http.delete(&session).header("XSRF-TOKEN", xsrf).header(reqwest::header::COOKIE, cookies).send().await;
        launch
    }
    .await
    .map_err(|e: String| tls.explain(e));
    Reply { result, fingerprint: tls.seen() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn credentials_are_sent_as_json_strings() {
        assert_eq!(super::quoted("root").unwrap(), "\"root\"");
        assert_eq!(super::quoted("a\"b").unwrap(), "\"a\\\"b\"");
    }
}
