//! Supermicro (ATEN) legacy iKVM support.
//!
//! These BMCs only offer their console to a Java viewer, which speaks ATEN's variant of VNC on
//! TCP port 5900. The console tab instead runs noVNC-iKVM (a noVNC fork by Kevin Kelley with ATEN
//! support, bundled under `ui/vendor/novnc-ikvm`) and connects to a WebSocket relay started here,
//! because browsers can't open raw TCP connections.
//!
//! For each WebSocket connection the relay:
//! 1. logs in to the BMC's web UI and fetches the Java viewer's launch file (JNLP), which carries
//!    a one-time iKVM username/password for this web session;
//! 2. connects to the BMC's iKVM port and pipes bytes both ways;
//! 3. writes those one-time credentials into the ATEN login message the page sends (the page only
//!    sends placeholders, so it never sees any credentials);
//! 4. logs the web session out again once the iKVM login is done. The iKVM session keeps running;
//!    the one-time credentials stop working, which is fine because every reconnect fetches new ones.

use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tokio_tungstenite::tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode},
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message,
};

/// BMC address and web login.
#[derive(Clone)]
pub struct Bmc {
    /// Origin of the web UI, e.g. `https://192.168.2.240`.
    pub base: String,
    pub username: String,
    pub password: String,
}

/// One-time iKVM credentials from the JNLP, plus the web session to log out afterwards.
struct Ticket {
    user: String,
    pass: String,
    port: u16,
    sid: String,
}

/// Client for talking to BMC web UIs: they use self-signed certificates, are slow, and older
/// firmware only speaks TLS 1.2 with CBC ciphers (e.g. ECDHE-RSA-AES256-SHA on X10 / firmware
/// 3.24), which rustls deliberately doesn't support. So this client uses the system's TLS library
/// (OpenSSL / Secure Transport / SChannel) instead.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_native_tls()
        // Their web servers drop idle keep-alive connections without notice; reusing one fails
        // with "IncompleteMessage". Open a fresh connection per request instead.
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(20))
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

fn session_cookie(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.strip_prefix("SID="))
        .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
        .find(|sid| !sid.is_empty())
}

/// Text of every `<argument>` element of the JNLP, in order.
fn jnlp_arguments(jnlp: &str) -> Vec<String> {
    jnlp.split("<argument>")
        .skip(1)
        .filter_map(|rest| rest.split("</argument>").next())
        .map(|a| a.trim().to_string())
        .collect()
}

async fn fetch_ticket(http: &reqwest::Client, bmc: &Bmc) -> Result<Ticket, String> {
    let body = format!(
        "name={}&pwd={}",
        urlencoding::encode(&bmc.username),
        urlencoding::encode(&bmc.password)
    );
    let resp = http
        .post(format!("{}/cgi/login.cgi", bmc.base))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("Could not reach the BMC web interface ({e})"))?;
    // A rejected login answers 200 too, just without a session cookie. Never retry here: these
    // BMCs lock the account after a few failed attempts.
    let sid = session_cookie(&resp).ok_or("The BMC rejected the saved username or password")?;
    let jnlp = async {
        http.get(format!("{}/cgi/url_redirect.cgi?url_name=ikvm&url_type=jwsk", bmc.base))
            .header(reqwest::header::COOKIE, format!("SID={sid}"))
            .send()
            .await?
            .text()
            .await
    }
    .await;
    let args = match jnlp {
        Ok(text) => jnlp_arguments(&text),
        Err(e) => {
            logout(http, &bmc.base, &sid).await;
            return Err(format!("Could not get the iKVM launch file ({e})"));
        }
    };
    // Arguments: host, user, password, ?, KVM port, virtual media port, ...
    if args.len() < 5 || args[1].is_empty() || args[2].is_empty() {
        logout(http, &bmc.base, &sid).await;
        return Err("The BMC did not offer an iKVM session (is the account allowed to use the console?)".into());
    }
    Ok(Ticket {
        user: args[1].clone(),
        pass: args[2].clone(),
        port: args[4].parse().unwrap_or(5900),
        sid,
    })
}

/// Ends the BMC web session (BMCs allow only a few at a time). Retried once; logging out twice is
/// harmless.
async fn logout(http: &reqwest::Client, base: &str, sid: &str) {
    for attempt in 1..=2 {
        let result = http
            .get(format!("{base}/cgi/logout.cgi"))
            .header(reqwest::header::COOKIE, format!("SID={sid}"))
            .send()
            .await;
        match result {
            Ok(r) => {
                if cfg!(debug_assertions) {
                    eprintln!("[ikvm {base}] web session logout: {}", r.status());
                }
                return;
            }
            Err(e) => eprintln!("[ikvm {base}] web session logout failed (attempt {attempt}): {e}"),
        }
    }
}

/// Byte offsets of the ATEN credential block in the client-to-server stream: after the 12-byte
/// protocol version and the 1-byte security type (16) come 24 bytes user + 24 bytes password.
const CRED_START: usize = 13;
const CRED_END: usize = CRED_START + 48;
const ATEN_SECURITY_TYPE: u8 = 16;

fn credential_block(user: &str, pass: &str) -> [u8; 48] {
    let mut block = [0u8; 48];
    let u = user.as_bytes();
    let p = pass.as_bytes();
    block[..u.len().min(24)].copy_from_slice(&u[..u.len().min(24)]);
    block[24..24 + p.len().min(24)].copy_from_slice(&p[..p.len().min(24)]);
    block
}

/// Overwrites the credential block wherever `chunk` (starting at stream offset `offset`) overlaps
/// it. Returns true once the whole block has been passed on.
fn substitute_credentials(chunk: &mut [u8], offset: usize, block: &[u8; 48], aten: bool) -> bool {
    if aten {
        for (i, byte) in chunk.iter_mut().enumerate() {
            let pos = offset + i;
            if (CRED_START..CRED_END).contains(&pos) {
                *byte = block[pos - CRED_START];
            }
        }
    }
    offset + chunk.len() >= CRED_END
}

/// A running relay for one console tab or window. Dropping it stops the relay and closes its
/// connections.
pub struct Relay {
    pub port: u16,
    /// Random path the page must connect to, so other local programs can't use the relay.
    pub token: String,
    shutdown: watch::Sender<bool>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

pub async fn start_relay(http: reqwest::Client, bmc: Bmc, kvm_host: String) -> Result<Relay, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("Could not start the console relay: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let (shutdown, mut stop) = watch::channel(false);
    let path = format!("/{token}");
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let (http, bmc, host, path, stop) =
                        (http.clone(), bmc.clone(), kvm_host.clone(), path.clone(), stop.clone());
                    tauri::async_runtime::spawn(async move {
                        serve(stream, &path, &http, &bmc, &host, stop).await;
                    });
                }
            }
        }
    });
    Ok(Relay { port, token, shutdown })
}

async fn serve(
    stream: TcpStream,
    path: &str,
    http: &reqwest::Client,
    bmc: &Bmc,
    kvm_host: &str,
    mut stop: watch::Receiver<bool>,
) {
    let check = |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
        if req.uri().path() != path {
            let mut err = ErrorResponse::new(None);
            *err.status_mut() = StatusCode::FORBIDDEN;
            return Err(err);
        }
        // noVNC asks for the "binary" subprotocol and treats the connection as base64 otherwise.
        let wants_binary = req
            .headers()
            .get_all("sec-websocket-protocol")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.split(',').any(|p| p.trim() == "binary"));
        if wants_binary {
            resp.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static("binary"));
        }
        Ok(resp)
    };
    let ws = match tokio_tungstenite::accept_hdr_async(stream, check).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[ikvm {}] rejected a relay connection: {e}", bmc.base);
            return;
        }
    };
    let (mut ws_tx, mut ws_rx) = ws.split();

    let fail = |reason: String| {
        eprintln!("[ikvm {}] {reason}", bmc.base);
        CloseFrame { code: CloseCode::Error, reason: reason.chars().take(120).collect::<String>().into() }
    };
    let ticket = match fetch_ticket(http, bmc).await {
        Ok(t) => t,
        Err(e) => {
            let _ = ws_tx.send(Message::Close(Some(fail(e)))).await;
            return;
        }
    };
    let tcp = match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((kvm_host, ticket.port))).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            logout(http, &bmc.base, &ticket.sid).await;
            let _ = ws_tx.send(Message::Close(Some(fail(format!("Could not connect to the iKVM port ({e})"))))).await;
            return;
        }
        Err(_) => {
            logout(http, &bmc.base, &ticket.sid).await;
            let _ = ws_tx.send(Message::Close(Some(fail("Timed out connecting to the iKVM port".into())))).await;
            return;
        }
    };
    let _ = tcp.set_nodelay(true);
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();
    let block = credential_block(&ticket.user, &ticket.pass);

    // BMC -> page
    let down = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = ws_tx.send(Message::Close(Some(fail("The BMC closed the console connection".into())))).await;
    };
    // Page -> BMC, with the real credentials written into the ATEN login.
    let up = async {
        let mut offset = 0usize;
        let mut aten = false;
        let mut logged_out = false;
        while let Some(Ok(msg)) = ws_rx.next().await {
            let mut data = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            if offset <= CRED_START - 1 && offset + data.len() > CRED_START - 1 {
                aten = data[CRED_START - 1 - offset] == ATEN_SECURITY_TYPE;
            }
            let done = substitute_credentials(&mut data, offset, &block, aten);
            offset += data.len();
            if tcp_tx.write_all(&data).await.is_err() {
                break;
            }
            if done && !logged_out {
                logged_out = true;
                // Give the BMC a moment to accept the iKVM login, then end the web session. The
                // iKVM session survives this (tested on X10 / firmware 3.24).
                let (http, base, sid) = (http.clone(), bmc.base.clone(), ticket.sid.clone());
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    logout(&http, &base, &sid).await;
                });
            }
        }
        if !logged_out {
            logout(http, &bmc.base, &ticket.sid).await;
        }
    };
    tokio::select! {
        _ = down => {}
        _ = up => {}
        _ = stop.changed() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jnlp_arguments() {
        let jnlp = "<application-desc main-class=\"x\">\n<argument>192.168.2.240</argument>\n\
                    <argument>AbC</argument>\n<argument>dEf</argument>\n<argument>null</argument>\n\
                    <argument>5900</argument>\n<argument>623</argument>\n</application-desc>";
        assert_eq!(jnlp_arguments(jnlp), ["192.168.2.240", "AbC", "dEf", "null", "5900", "623"]);
    }

    #[test]
    fn substitutes_credentials_across_chunks() {
        let block = credential_block("user", "pass");
        // version (12) + security type, then placeholder credentials "x:x" style, split oddly.
        let mut stream = b"RFB 003.008\n".to_vec();
        stream.push(16);
        stream.extend_from_slice(&[b'x'; 48]);
        stream.extend_from_slice(b"\x01rest");
        let mut out = Vec::new();
        let mut offset = 0;
        for chunk in [&stream[..12], &stream[12..13], &stream[13..30], &stream[30..]] {
            let mut c = chunk.to_vec();
            substitute_credentials(&mut c, offset, &block, true);
            offset += c.len();
            out.extend(c);
        }
        assert_eq!(&out[..13], &stream[..13]);
        assert_eq!(&out[13..61], &block[..]);
        assert_eq!(&out[61..], b"\x01rest");
        assert_eq!(&out[13..17], b"user");
        assert_eq!(&out[37..41], b"pass");
    }

    #[test]
    fn leaves_non_aten_streams_alone() {
        let block = credential_block("user", "pass");
        let mut data = vec![7u8; 70];
        substitute_credentials(&mut data, 0, &block, false);
        assert!(data.iter().all(|&b| b == 7));
    }
}
