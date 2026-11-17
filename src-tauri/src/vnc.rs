//! VNC (RFB) pieces shared by the console relays: logging in to a VNC server (no password or
//! classic VNC password), greeting the console page with a connection that needs no password,
//! and the relay for generic VNC devices.
//!
//! The relay always does the VNC login itself, so the console page never sees a password or ticket.

use crate::relay::{self, PageSocket};
use des::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::tungstenite::Message;

const TIMEOUT: Duration = Duration::from_secs(15);

/// Byte-oriented access to one side of a connection during the handshake.
pub(crate) trait Transport {
    async fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, String>;
    async fn write_all(&mut self, data: &[u8]) -> Result<(), String>;
}

/// Reads exact byte counts from a stream of WebSocket messages (binary ones carry the data).
pub(crate) struct Reader<S> {
    pub inner: S,
    /// Bytes received beyond what was read so far.
    pub buf: Vec<u8>,
}

impl<S, E> Reader<S>
where
    S: Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Display,
{
    pub fn new(inner: S) -> Self {
        Reader { inner, buf: Vec::new() }
    }

    pub async fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, String> {
        while self.buf.len() < n {
            match tokio::time::timeout(TIMEOUT, self.inner.next()).await {
                Err(_) => return Err("timed out".into()),
                Ok(None) => return Err("connection closed".into()),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(Message::Binary(b)))) => self.buf.extend_from_slice(&b),
                Ok(Some(Ok(Message::Close(frame)))) => {
                    return Err(frame
                        .map(|f| f.reason.to_string())
                        .filter(|r| !r.is_empty())
                        .unwrap_or("connection closed".into()))
                }
                Ok(Some(Ok(_))) => {}
            }
        }
        Ok(self.buf.drain(..n).collect())
    }
}

/// A WebSocket (split into its halves) as a [`Transport`].
pub(crate) struct WsIo<'a, Tx, Rx> {
    pub tx: &'a mut Tx,
    pub rx: &'a mut Reader<Rx>,
}

impl<Tx, Rx, E> Transport for WsIo<'_, Tx, Rx>
where
    Tx: Sink<Message> + Unpin,
    Tx::Error: std::fmt::Display,
    Rx: Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Display,
{
    async fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, String> {
        self.rx.read_exact(n).await
    }
    async fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        self.tx.send(Message::Binary(data.to_vec().into())).await.map_err(|e| e.to_string())
    }
}

/// A plain TCP connection as a [`Transport`].
pub(crate) struct TcpIo<'a>(pub &'a mut TcpStream);

impl Transport for TcpIo<'_> {
    async fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; n];
        match tokio::time::timeout(TIMEOUT, self.0.read_exact(&mut buf)).await {
            Err(_) => Err("timed out".into()),
            Ok(Err(e)) => Err(e.to_string()),
            Ok(Ok(_)) => Ok(buf),
        }
    }
    async fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        self.0.write_all(data).await.map_err(|e| e.to_string())
    }
}

async fn read_u32<T: Transport>(t: &mut T) -> Result<u32, String> {
    Ok(u32::from_be_bytes(t.read_exact(4).await?.try_into().unwrap()))
}

async fn read_reason<T: Transport>(t: &mut T) -> String {
    match read_u32(t).await {
        Ok(len) if len < 4096 => t.read_exact(len as usize).await.map(|r| String::from_utf8_lossy(&r).to_string()).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Logs in to a VNC server (RFB 3.3 / 3.7 / 3.8) with no password or a classic VNC password.
/// Afterwards the connection continues with ClientInit, which the page sends.
pub(crate) async fn login<T: Transport>(t: &mut T, password: Option<&str>) -> Result<(), String> {
    let version = t.read_exact(12).await?;
    let text = String::from_utf8_lossy(&version);
    let minor: u32 = match text.strip_prefix("RFB 003.").and_then(|r| r.trim_end().parse().ok()) {
        Some(m) => m,
        None if text.starts_with("RFB ") => 8, // e.g. "RFB 004.001" (RealVNC): speak 3.8
        None => return Err("This doesn't look like a VNC server".into()),
    };
    // 3.4–3.6 are treated as 3.3 by the specification; anything newer as 3.8.
    let minor = match minor {
        7 => 7,
        m if m >= 8 => 8,
        _ => 3,
    };
    t.write_all(format!("RFB 003.{minor:03}\n").as_bytes()).await?;

    let chosen = if minor >= 7 {
        let count = t.read_exact(1).await?[0] as usize;
        if count == 0 {
            return Err(format!("The VNC server refused the connection: {}", read_reason(t).await));
        }
        let types = t.read_exact(count).await?;
        let pick = if types.contains(&2) && password.is_some() {
            2
        } else if types.contains(&1) {
            1
        } else if types.contains(&2) {
            return Err("The VNC server asks for a password, but none is saved (Edit)".into());
        } else {
            return Err(format!(
                "The VNC server only offers login methods the app doesn't support (types {types:?}); \
                 classic VNC password or no password are supported"
            ));
        };
        t.write_all(&[pick]).await?;
        pick
    } else {
        match read_u32(t).await? {
            0 => return Err(format!("The VNC server refused the connection: {}", read_reason(t).await)),
            1 => 1,
            2 if password.is_some() => 2,
            2 => return Err("The VNC server asks for a password, but none is saved (Edit)".into()),
            other => return Err(format!("Unsupported VNC login method {other}")),
        }
    };
    if chosen == 2 {
        let challenge = t.read_exact(16).await?;
        t.write_all(&auth_response(password.unwrap_or_default(), &challenge)).await?;
    }
    // A security result follows the password step, and in 3.8 also "no password".
    if chosen == 2 || minor >= 8 {
        if read_u32(t).await? != 0 {
            let reason = if minor >= 8 { read_reason(t).await } else { String::new() };
            let reason = if reason.trim().is_empty() { String::new() } else { format!(" ({})", reason.trim()) };
            return Err(if chosen == 2 {
                format!("The VNC server rejected the password{reason}")
            } else {
                format!("The VNC server refused the connection{reason}")
            });
        }
    }
    Ok(())
}

/// Classic VNC authentication: DES-encrypt the 16-byte challenge with the password's first 8
/// bytes (zero-padded, each byte's bits mirrored) as the key.
pub(crate) fn auth_response(password: &str, challenge: &[u8]) -> [u8; 16] {
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

/// Offers the console page an RFB 3.8 connection that needs no password (security type 1).
pub(crate) async fn greet_page<T: Transport>(page: &mut T) -> Result<(), String> {
    page.write_all(b"RFB 003.008\n").await?;
    page.read_exact(12).await?; // the page's protocol version
    page.write_all(&[1, 1]).await?; // one security type: None
    page.read_exact(1).await?; // the page picks it
    page.write_all(&[0, 0, 0, 0]).await // security result: OK
}

// ---------------------------------------------------------------- generic VNC devices

/// Starts the relay for a generic VNC server.
pub async fn start_relay(host: String, port: u16, password: Option<String>) -> Result<relay::Relay, String> {
    let name = format!("vnc://{host}:{port}");
    relay::start(name.clone(), move |page| {
        let (host, password, name) = (host.clone(), password.clone(), name.clone());
        async move { serve(page, &host, port, password.as_deref(), &name).await }
    })
    .await
}

async fn serve(page: PageSocket, host: &str, port: u16, password: Option<&str>, name: &str) {
    let (mut page_tx, page_rx) = page.split();
    let mut page_rx = Reader::new(page_rx);
    let connected = async {
        let mut tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host, port)))
            .await
            .map_err(|_| "Timed out connecting to the VNC server".to_string())?
            .map_err(|e| format!("Could not connect to the VNC server ({e})"))?;
        let _ = tcp.set_nodelay(true);
        login(&mut TcpIo(&mut tcp), password).await?;
        greet_page(&mut WsIo { tx: &mut page_tx, rx: &mut page_rx })
            .await
            .map_err(|e| format!("Console handshake failed ({e})"))?;
        Ok::<_, String>(tcp)
    }
    .await;
    let tcp = match connected {
        Ok(t) => t,
        Err(e) => {
            let _ = page_tx.send(Message::Close(Some(relay::fail(name, e)))).await;
            return;
        }
    };
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();
    // Anything the page already sent past the handshake (normally its ClientInit) goes first.
    let leftover = std::mem::take(&mut page_rx.buf);
    if !leftover.is_empty() && tcp_tx.write_all(&leftover).await.is_err() {
        return;
    }
    let mut page_rx = page_rx.inner;
    let up = async {
        while let Some(Ok(msg)) = page_rx.next().await {
            match msg {
                Message::Binary(b) => {
                    if tcp_tx.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    let down = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match tcp_rx.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if page_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = page_tx.send(Message::Close(Some(relay::fail(name, "The VNC server closed the connection")))).await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_response_matches_openssl() {
        // OpenSSL DES-ECB of the challenge 0x00..0x0f with the key "password" with each byte's
        // bits mirrored (0e86ceceeef64e26), which is what VNC authentication specifies.
        let challenge: Vec<u8> = (0u8..16).collect();
        let got: String = auth_response("password", &challenge).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, "b866924125c8eebb9debc1db61c538e2");
    }

    /// Scripted server side of a handshake, for testing `login`.
    struct Script {
        from_server: Vec<u8>,
        to_server: Vec<u8>,
    }
    impl Transport for Script {
        async fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, String> {
            if self.from_server.len() < n {
                return Err("connection closed".into());
            }
            Ok(self.from_server.drain(..n).collect())
        }
        async fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
            self.to_server.extend_from_slice(data);
            Ok(())
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    #[test]
    fn logs_in_with_vnc_password_38() {
        let mut s = Script { from_server: b"RFB 003.008\n".to_vec(), to_server: vec![] };
        s.from_server.extend([1, 2]); // one type: VNC auth
        s.from_server.extend(0u8..16); // challenge
        s.from_server.extend([0, 0, 0, 0]); // OK
        run(login(&mut s, Some("password"))).unwrap();
        assert_eq!(&s.to_server[..12], b"RFB 003.008\n");
        assert_eq!(s.to_server[12], 2);
        assert_eq!(&s.to_server[13..], &auth_response("password", &(0u8..16).collect::<Vec<_>>()));
    }

    #[test]
    fn prefers_no_password_when_none_saved_and_handles_33() {
        let mut s = Script { from_server: b"RFB 003.008\n".to_vec(), to_server: vec![] };
        s.from_server.extend([2, 2, 1, 0, 0, 0, 0]);
        run(login(&mut s, None)).unwrap();
        assert_eq!(s.to_server[12], 1);

        let mut s = Script { from_server: b"RFB 003.003\n".to_vec(), to_server: vec![] };
        s.from_server.extend([0, 0, 0, 1]); // 3.3: server decides, no password
        run(login(&mut s, None)).unwrap();
        assert_eq!(&s.to_server, b"RFB 003.003\n");
    }

    #[test]
    fn reports_rejected_password_and_missing_password() {
        let mut s = Script { from_server: b"RFB 003.008\n".to_vec(), to_server: vec![] };
        s.from_server.extend([1, 2]);
        s.from_server.extend(0u8..16);
        s.from_server.extend([0, 0, 0, 1, 0, 0, 0, 5]);
        s.from_server.extend(b"nope!");
        let err = run(login(&mut s, Some("x"))).unwrap_err();
        assert!(err.contains("rejected the password") && err.contains("nope!"), "{err}");

        let mut s = Script { from_server: b"RFB 003.008\n".to_vec(), to_server: vec![] };
        s.from_server.extend([1, 2]);
        assert!(run(login(&mut s, None)).unwrap_err().contains("none is saved"));
    }
}
