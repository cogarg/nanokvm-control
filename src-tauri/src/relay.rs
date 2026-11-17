//! Local WebSocket relay for console pages that can't reach their server directly (Supermicro
//! iKVM, Proxmox VE). Listens on 127.0.0.1 with a random port and a random path; each accepted
//! connection is handed to a per-console handler. Dropping the [`Relay`] stops it and closes its
//! connections.

use std::future::Future;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tokio_tungstenite::{
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::{HeaderValue, StatusCode},
        protocol::{frame::coding::CloseCode, CloseFrame},
    },
    WebSocketStream,
};

pub type PageSocket = WebSocketStream<TcpStream>;

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

/// Starts a relay; `handler` runs for every page connection until it returns or the relay stops.
pub async fn start<F, Fut>(name: String, handler: F) -> Result<Relay, String>
where
    F: Fn(PageSocket) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
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
                    let (handler, path, name, mut stop) = (handler.clone(), path.clone(), name.clone(), stop.clone());
                    tauri::async_runtime::spawn(async move {
                        let Some(ws) = accept_page(stream, &path, &name).await else { return };
                        tokio::select! {
                            _ = handler(ws) => {}
                            _ = stop.changed() => {}
                        }
                    });
                }
            }
        }
    });
    Ok(Relay { port, token, shutdown })
}

async fn accept_page(stream: TcpStream, path: &str, name: &str) -> Option<PageSocket> {
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
    match tokio_tungstenite::accept_hdr_async(stream, check).await {
        Ok(ws) => Some(ws),
        Err(e) => {
            eprintln!("[relay {name}] rejected a connection: {e}");
            None
        }
    }
}

/// Close frame telling the page why the console can't be shown (logged as well).
pub fn fail(name: &str, reason: impl Into<String>) -> CloseFrame {
    let reason = reason.into();
    eprintln!("[relay {name}] {reason}");
    // Close reasons are limited to 123 bytes.
    let mut short = String::new();
    for c in reason.chars() {
        if short.len() + c.len_utf8() > 120 {
            break;
        }
        short.push(c);
    }
    CloseFrame { code: CloseCode::Error, reason: short.into() }
}
