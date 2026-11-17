//! Proxmox VE: list a server's VMs and containers, and show their consoles.
//!
//! - **TLS:** Proxmox uses a self-signed certificate by default, so instead of a CA check the
//!   server's certificate is pinned on first contact (trust on first use): its SHA-256
//!   fingerprint is saved with the server, and any later certificate that doesn't match is
//!   refused. This keeps the (often root) password from being sent to an impostor.
//! - **Login:** username + password (`user@realm`) for a ticket, cached and renewed before its
//!   2-hour lifetime runs out. Accounts with two-factor authentication are not supported.
//! - **Consoles:** the console page connects to a local relay ([`crate::relay`]). Per connection
//!   the relay asks Proxmox for a VNC proxy ticket, opens Proxmox's `vncwebsocket`, does the VNC
//!   password step itself (the password is the ticket), and presents the page with a VNC
//!   connection that needs no password. The page never sees a ticket or a password.

use crate::relay::{self, PageSocket};
use des::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use futures_util::{SinkExt, StreamExt};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, SignatureScheme,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

/// A Proxmox VE server as configured by the user.
#[derive(Clone)]
pub struct Server {
    /// Origin, e.g. `https://pve.lan:8006`.
    pub base: String,
    pub username: String,
    pub password: String,
    /// Pinned certificate fingerprint, if one was saved already.
    pub fingerprint: Option<String>,
}

impl Server {
    fn user(&self) -> String {
        // "root" means "root@pam", as on the Proxmox login screen's default realm.
        if self.username.contains('@') { self.username.clone() } else { format!("{}@pam", self.username) }
    }
}

// ---------------------------------------------------------------- TLS pinning

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
struct Tls {
    config: Arc<ClientConfig>,
    verifier: Arc<PinVerifier>,
}

impl Tls {
    fn new(expected: Option<String>) -> Result<Self, String> {
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

    fn seen(&self) -> Option<String> {
        self.verifier.seen.lock().unwrap().clone()
    }

    /// Turns a failed request into a clear message when the certificate is the reason.
    fn explain(&self, err: String) -> String {
        match (&self.verifier.expected, self.seen()) {
            (Some(expected), Some(seen)) if *expected != seen => format!(
                "The server's certificate has changed, so the app refused to connect. New fingerprint: {seen}. \
                 If you expected this (e.g. a renewed certificate), use Edit > Forget certificate."
            ),
            _ => err,
        }
    }

    fn http(&self) -> Result<reqwest::Client, String> {
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

// ---------------------------------------------------------------- API

#[derive(Clone)]
struct Session {
    ticket: String,
    csrf: String,
    created: Instant,
}

/// Login tickets per server (Proxmox tickets are valid for 2 hours).
#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, Session>>);

impl Sessions {
    fn key(server: &Server) -> String {
        format!("{} {}", server.base, server.user())
    }
    fn get(&self, server: &Server) -> Option<Session> {
        let map = self.0.lock().unwrap();
        map.get(&Self::key(server)).filter(|s| s.created.elapsed() < Duration::from_secs(90 * 60)).cloned()
    }
    fn put(&self, server: &Server, s: Session) {
        self.0.lock().unwrap().insert(Self::key(server), s);
    }
    pub fn forget(&self, server: &Server) {
        self.0.lock().unwrap().remove(&Self::key(server));
    }
}

#[derive(Deserialize)]
struct ApiReply<T> {
    data: Option<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TicketData {
    ticket: Option<String>,
    #[serde(rename = "CSRFPreventionToken")]
    csrf: Option<String>,
    #[serde(rename = "NeedTFA")]
    need_tfa: Option<serde_json::Value>,
}

async fn login(http: &reqwest::Client, server: &Server) -> Result<Session, String> {
    let resp = http
        .post(format!("{}/api2/json/access/ticket", server.base))
        .form(&[("username", server.user().as_str()), ("password", server.password.as_str())])
        .send()
        .await
        .map_err(|e| format!("Could not reach the Proxmox server ({e})"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("Proxmox rejected the username or password (use user@realm, e.g. root@pam)".into());
    }
    if !resp.status().is_success() {
        return Err(format!("Proxmox login failed (HTTP {})", resp.status()));
    }
    let data = resp
        .json::<ApiReply<TicketData>>()
        .await
        .map_err(|e| format!("Unexpected login reply from Proxmox ({e})"))?
        .data
        .ok_or("Unexpected login reply from Proxmox")?;
    if data.need_tfa.as_ref().is_some_and(|v| !v.is_null() && v != &serde_json::json!(0)) {
        return Err("This account uses two-factor authentication, which the app doesn't support".into());
    }
    Ok(Session {
        ticket: data.ticket.ok_or("Proxmox didn't return a login ticket")?,
        csrf: data.csrf.unwrap_or_default(),
        created: Instant::now(),
    })
}

fn cookie(session: &Session) -> String {
    format!("PVEAuthCookie={}", urlencoding::encode(&session.ticket))
}

/// Runs `call` with a valid session, logging in (again) when needed. A 401 from the call means
/// the cached ticket expired; that is retried once with a fresh login.
async fn with_session<T, F, Fut>(http: &reqwest::Client, server: &Server, sessions: &Sessions, call: F) -> Result<T, String>
where
    F: Fn(Session) -> Fut,
    Fut: std::future::Future<Output = Result<Result<T, String>, reqwest::StatusCode>>,
{
    let cached = sessions.get(server);
    let had_cache = cached.is_some();
    let session = match cached {
        Some(s) => s,
        None => {
            let s = login(http, server).await?;
            sessions.put(server, s.clone());
            s
        }
    };
    match call(session).await {
        Err(status) if status == reqwest::StatusCode::UNAUTHORIZED && had_cache => {
            sessions.forget(server);
            let s = login(http, server).await?;
            sessions.put(server, s.clone());
            call(s).await.map_err(|status| format!("Proxmox answered HTTP {status}"))?
        }
        Err(status) => Err(format!("Proxmox answered HTTP {status}")),
        Ok(result) => result,
    }
}

/// A VM or container, as the UI shows it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Guest {
    pub node: String,
    pub vmid: u32,
    pub name: String,
    /// `qemu` (VM) or `lxc` (container).
    pub kind: String,
    /// `running`, `stopped`, `paused`, …
    pub status: String,
}

#[derive(Deserialize)]
struct Resource {
    #[serde(rename = "type")]
    kind: String,
    vmid: Option<u32>,
    node: Option<String>,
    name: Option<String>,
    status: Option<String>,
    #[serde(default)]
    template: serde_json::Value,
}

pub async fn list_guests(server: &Server, sessions: &Sessions) -> Reply<Vec<Guest>> {
    let tls = match Tls::new(server.fingerprint.clone()) {
        Ok(t) => t,
        Err(e) => return Reply { result: Err(e), fingerprint: None },
    };
    let result = async {
        let http = tls.http()?;
        let resources: Vec<Resource> = with_session(&http, server, sessions, |s| {
            let req = http
                .get(format!("{}/api2/json/cluster/resources?type=vm", server.base))
                .header(reqwest::header::COOKIE, cookie(&s));
            async move {
                let resp = req.send().await.map_err(|_| reqwest::StatusCode::BAD_GATEWAY)?;
                if !resp.status().is_success() {
                    return Err(resp.status());
                }
                Ok(resp
                    .json::<ApiReply<Vec<Resource>>>()
                    .await
                    .map_err(|e| format!("Unexpected guest list from Proxmox ({e})"))
                    .map(|r| r.data.unwrap_or_default()))
            }
        })
        .await?;
        let mut guests: Vec<Guest> = resources
            .into_iter()
            .filter(|r| r.kind == "qemu" || r.kind == "lxc")
            .filter(|r| !matches!(&r.template, serde_json::Value::Number(n) if n.as_u64() == Some(1)))
            .filter_map(|r| {
                let vmid = r.vmid?;
                Some(Guest {
                    node: r.node?,
                    vmid,
                    name: r.name.unwrap_or_else(|| format!("{} {vmid}", if r.kind == "lxc" { "CT" } else { "VM" })),
                    kind: r.kind,
                    status: r.status.unwrap_or_default(),
                })
            })
            .collect();
        guests.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.vmid.cmp(&b.vmid)));
        Ok(guests)
    }
    .await
    .map_err(|e: String| tls.explain(e));
    Reply { result, fingerprint: tls.seen() }
}

/// Is the server reachable with the expected certificate?
pub async fn reachable(server: &Server) -> bool {
    let Ok(tls) = Tls::new(server.fingerprint.clone()) else { return false };
    let Ok(http) = tls.http() else { return false };
    http.get(format!("{}/api2/json/version", server.base)).send().await.is_ok()
}

#[derive(Deserialize)]
struct VncProxy {
    port: serde_json::Value,
    ticket: String,
}

// ---------------------------------------------------------------- console relay

/// Identifies one guest's console.
#[derive(Clone)]
pub struct Target {
    pub node: String,
    pub kind: String,
    pub vmid: u32,
}

/// Starts a relay for one guest console. `on_fingerprint` is called with the certificate seen,
/// so a server's first contact can pin it.
pub async fn start_console_relay(
    server: Server,
    sessions: Arc<Sessions>,
    target: Target,
    on_fingerprint: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<relay::Relay, String> {
    let name = format!("{} {}/{}", server.base, target.node, target.vmid);
    relay::start(name.clone(), move |page| {
        let (server, sessions, target, on_fp, name) =
            (server.clone(), sessions.clone(), target.clone(), on_fingerprint.clone(), name.clone());
        async move { serve(page, &server, &sessions, &target, &*on_fp, &name).await }
    })
    .await
}

async fn serve(
    page: PageSocket,
    server: &Server,
    sessions: &Sessions,
    target: &Target,
    on_fingerprint: &(dyn Fn(String) + Send + Sync),
    name: &str,
) {
    let (mut page_tx, page_rx) = page.split();
    let tls = match Tls::new(server.fingerprint.clone()) {
        Ok(t) => t,
        Err(e) => {
            let _ = page_tx.send(Message::Close(Some(relay::fail(name, e)))).await;
            return;
        }
    };
    let connected = connect(server, sessions, target, &tls).await;
    if let Some(fp) = tls.seen() {
        on_fingerprint(fp);
    }
    let (pve, leftover_from_pve) = match connected {
        Ok(c) => c,
        Err(e) => {
            let _ = page_tx.send(Message::Close(Some(relay::fail(name, tls.explain(e))))).await;
            return;
        }
    };
    let (mut pve_tx, pve_rx) = pve.split();

    // Present the page with a password-less VNC connection (security type 1, "None").
    let mut page_rx = Reader::new(page_rx);
    let handshake = async {
        page_tx.send(Message::Binary(b"RFB 003.008\n".to_vec().into())).await.map_err(|e| e.to_string())?;
        page_rx.read_exact(12).await?; // the page's protocol version
        page_tx.send(Message::Binary(vec![1u8, 1].into())).await.map_err(|e| e.to_string())?;
        page_rx.read_exact(1).await?; // the page picks type 1
        page_tx.send(Message::Binary(vec![0u8, 0, 0, 0].into())).await.map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    };
    if let Err(e) = handshake.await {
        let _ = page_tx.send(Message::Close(Some(relay::fail(name, format!("Console handshake failed ({e})"))))).await;
        return;
    }
    // Anything either side already sent beyond the handshake goes through first.
    if !leftover_from_pve.is_empty() && page_tx.send(Message::Binary(leftover_from_pve.into())).await.is_err() {
        return;
    }
    let leftover_from_page = std::mem::take(&mut page_rx.buf);
    if !leftover_from_page.is_empty() && pve_tx.send(Message::Binary(leftover_from_page.into())).await.is_err() {
        return;
    }

    let mut page_rx = page_rx.inner;
    let mut pve_rx = pve_rx;
    let up = async {
        while let Some(Ok(msg)) = page_rx.next().await {
            match msg {
                Message::Binary(b) => {
                    if pve_tx.send(Message::Binary(b)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = pve_tx.send(Message::Close(None)).await;
    };
    let down = async {
        while let Some(Ok(msg)) = pve_rx.next().await {
            match msg {
                Message::Binary(b) => {
                    if page_tx.send(Message::Binary(b)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = page_tx.send(Message::Close(Some(relay::fail(name, "The console connection was closed by Proxmox")))).await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

type PveSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Gets a console ticket, opens Proxmox's vncwebsocket and completes the VNC login. Returns the
/// socket and any bytes Proxmox already sent past the login (normally none).
async fn connect(server: &Server, sessions: &Sessions, target: &Target, tls: &Tls) -> Result<(PveSocket, Vec<u8>), String> {
    let http = tls.http()?;
    let path = format!("/api2/json/nodes/{}/{}/{}", target.node, target.kind, target.vmid);
    // The session used for vncproxy must also authenticate the websocket.
    let (session, proxy) = {
        let session_used = Mutex::new(None);
        let proxy: VncProxy = with_session(&http, server, sessions, |s| {
            *session_used.lock().unwrap() = Some(s.clone());
            let req = http
                .post(format!("{}{path}/vncproxy", server.base))
                .header(reqwest::header::COOKIE, cookie(&s))
                .header("CSRFPreventionToken", s.csrf.clone())
                .form(&[("websocket", "1")]);
            async move {
                let resp = req.send().await.map_err(|_| reqwest::StatusCode::BAD_GATEWAY)?;
                if !resp.status().is_success() {
                    return Err(resp.status());
                }
                Ok(resp
                    .json::<ApiReply<VncProxy>>()
                    .await
                    .map_err(|e| format!("Unexpected console reply from Proxmox ({e})"))
                    .and_then(|r| r.data.ok_or_else(|| "Proxmox didn't start a console".to_string())))
            }
        })
        .await
        .map_err(|e| {
            if e.contains("HTTP 403") {
                "This Proxmox account isn't allowed to open the console (needs VM.Console)".to_string()
            } else {
                e
            }
        })?;
        let used = session_used.lock().unwrap().take();
        (used.ok_or("No Proxmox session")?, proxy)
    };
    let port = match &proxy.port {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let ws_base = server.base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let url = format!(
        "{ws_base}{path}/vncwebsocket?port={}&vncticket={}",
        urlencoding::encode(&port),
        urlencoding::encode(&proxy.ticket)
    );
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    let headers = request.headers_mut();
    headers.insert("cookie", HeaderValue::from_str(&cookie(&session)).map_err(|e| e.to_string())?);
    headers.insert("sec-websocket-protocol", HeaderValue::from_static("binary"));
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(tls.config.clone())),
        ),
    )
    .await
    .map_err(|_| "Timed out opening the Proxmox console".to_string())?
    .map_err(|e| format!("Could not open the Proxmox console ({e})"))?;

    let (mut tx, rx) = ws.split();
    let mut rx = Reader::new(rx);
    let version = rx.read_exact(12).await?;
    if !version.starts_with(b"RFB 003.") {
        return Err("Proxmox didn't answer with a VNC console".into());
    }
    tx.send(Message::Binary(b"RFB 003.008\n".to_vec().into())).await.map_err(|e| e.to_string())?;
    let count = rx.read_exact(1).await?[0] as usize;
    if count == 0 {
        let len = u32::from_be_bytes(rx.read_exact(4).await?.try_into().unwrap()) as usize;
        let reason = String::from_utf8_lossy(&rx.read_exact(len).await?).to_string();
        return Err(format!("Proxmox refused the console: {reason}"));
    }
    let types = rx.read_exact(count).await?;
    if types.contains(&2) {
        tx.send(Message::Binary(vec![2u8].into())).await.map_err(|e| e.to_string())?;
        let challenge = rx.read_exact(16).await?;
        let response = vnc_auth_response(&proxy.ticket, &challenge);
        tx.send(Message::Binary(response.to_vec().into())).await.map_err(|e| e.to_string())?;
    } else if types.contains(&1) {
        tx.send(Message::Binary(vec![1u8].into())).await.map_err(|e| e.to_string())?;
    } else {
        return Err(format!("Unsupported console security types {types:?}"));
    }
    let result = u32::from_be_bytes(rx.read_exact(4).await?.try_into().unwrap());
    if result != 0 {
        let reason = match rx.read_exact(4).await {
            Ok(len) => {
                let len = u32::from_be_bytes(len.try_into().unwrap()) as usize;
                rx.read_exact(len).await.map(|r| String::from_utf8_lossy(&r).to_string()).unwrap_or_default()
            }
            Err(_) => String::new(),
        };
        return Err(format!("Proxmox refused the console login {reason}").trim().to_string());
    }
    let leftover = std::mem::take(&mut rx.buf);
    let ws = tx.reunite(rx.inner).map_err(|_| "internal error".to_string())?;
    Ok((ws, leftover))
}

/// Classic VNC authentication: DES-encrypt the 16-byte challenge with the password's first 8
/// bytes (zero-padded, each byte's bits mirrored) as the key.
fn vnc_auth_response(password: &str, challenge: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let cipher = des::Des::new(GenericArray::from_slice(&key));
    let mut out = [0u8; 16];
    out.copy_from_slice(&challenge[..16]);
    for block in out.chunks_mut(8) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    out
}

/// Reads exact byte counts from a stream of WebSocket binary messages.
struct Reader<S> {
    inner: S,
    buf: Vec<u8>,
}

impl<S, E> Reader<S>
where
    S: futures_util::Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Display,
{
    fn new(inner: S) -> Self {
        Reader { inner, buf: Vec::new() }
    }

    async fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, String> {
        while self.buf.len() < n {
            match tokio::time::timeout(Duration::from_secs(15), self.inner.next()).await {
                Err(_) => return Err("timed out".into()),
                Ok(None) => return Err("connection closed".into()),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(Message::Binary(b)))) => self.buf.extend_from_slice(&b),
                Ok(Some(Ok(Message::Close(frame)))) => {
                    return Err(frame.map(|f| f.reason.to_string()).filter(|r| !r.is_empty()).unwrap_or("connection closed".into()))
                }
                Ok(Some(Ok(_))) => {}
            }
        }
        Ok(self.buf.drain(..n).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vnc_auth_matches_reference() {
        // Reference: OpenSSL DES-ECB of the challenge 0x00..0x0f with the key "password" with each
        // byte's bits mirrored (0e86ceceeef64e26), which is what VNC authentication specifies.
        let challenge: Vec<u8> = (0u8..16).collect();
        let got = vnc_auth_response("password", &challenge);
        assert_eq!(hex(&got), REFERENCE);
    }

    const REFERENCE: &str = "b866924125c8eebb9debc1db61c538e2"; // openssl enc -des-ecb, mirrored key

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn formats_fingerprints_like_proxmox() {
        let fp = fingerprint(b"abc");
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(fp.starts_with("BA:78:16:BF:"));
    }

    #[test]
    fn realm_defaults_to_pam() {
        let s = |u: &str| Server { base: String::new(), username: u.into(), password: String::new(), fingerprint: None };
        assert_eq!(s("root").user(), "root@pam");
        assert_eq!(s("me@pve").user(), "me@pve");
    }
}
