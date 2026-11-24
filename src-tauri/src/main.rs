#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backup;
mod console_tls;
mod crypto;
mod discovery;
mod idrac;
mod ikvm;
mod proxmox;
mod relay;
mod secrets;
mod store;
mod tls;
mod vnc;

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use store::{Device, DeviceInput, DeviceKind, DeviceView, Group, GroupInput, Store};
use tauri::{
    webview::WebviewBuilder, window::WindowBuilder, AppHandle, LogicalPosition, LogicalSize, Manager, Rect,
    State, Url, WebviewUrl, WebviewWindowBuilder,
};

const MAIN_WINDOW: &str = "main";
/// Overrides where the app keeps its data (device list + webview cookies/storage).
const DATA_DIR_ENV: &str = "NANOKVM_CONTROL_DATA_DIR";
const UI_WEBVIEW: &str = "ui";

/// Area (logical px, relative to the main window) where console tabs are drawn.
/// Reported by the UI, which owns the layout (everything below its tab bar).
#[derive(Clone, Copy, Deserialize)]
struct ConsoleBounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl ConsoleBounds {
    #[cfg(target_os = "linux")]
    fn gtk_rect(self) -> (i32, i32, i32, i32) {
        (self.x.round() as i32, self.y.round() as i32, self.width.round() as i32, self.height.round() as i32)
    }

    fn rect(self) -> Rect {
        Rect {
            position: LogicalPosition::new(self.x, self.y).into(),
            size: LogicalSize::new(self.width.max(1.0), self.height.max(1.0)).into(),
        }
    }
}

struct AppState {
    store: Mutex<Store>,
    http: reqwest::Client,
    /// For BMC web UIs (longer timeout, no redirects).
    bmc_http: reqwest::Client,
    /// Relays of open Supermicro / Proxmox consoles, by webview/window label.
    relays: Mutex<HashMap<String, relay::Relay>>,
    /// Proxmox login tickets.
    pve_sessions: Arc<proxmox::Sessions>,
    /// Names of Proxmox guests from the last listing, by console id (for window titles).
    guest_names: Mutex<HashMap<String, String>>,
    bounds: Mutex<ConsoleBounds>,
    /// Webview data directory (cookies, storage) when isolated; `None` uses the platform default.
    webview_data: Option<PathBuf>,
}

/// Where the app keeps its data. Release builds use the normal per-user app directories.
/// `NANOKVM_CONTROL_DATA_DIR` (any build) or a debug build (`cargo run`) instead keeps
/// everything under one separate folder, so development and testing never touch the device
/// list or console sessions of the installed app.
struct DataLocation {
    devices_file: PathBuf,
    webview_data: Option<PathBuf>,
    /// Set when the location is not the default, to make that visible in the window title.
    isolated_root: Option<PathBuf>,
}

fn data_location(app: &tauri::App) -> tauri::Result<DataLocation> {
    Ok(data_location_in(app.path().app_config_dir()?))
}

fn data_location_in(config_dir: PathBuf) -> DataLocation {
    let root = match std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None if cfg!(debug_assertions) => {
            let mut name = config_dir.file_name().unwrap_or_default().to_os_string();
            name.push(".dev");
            Some(config_dir.with_file_name(name))
        }
        None => None,
    };
    match root {
        Some(root) => DataLocation {
            devices_file: root.join("devices.json"),
            webview_data: Some(root.join("webview")),
            isolated_root: Some(root),
        },
        None => DataLocation {
            devices_file: config_dir.join("devices.json"),
            webview_data: None,
            isolated_root: None,
        },
    }
}

/// Whether WebKit runs without its DMA-BUF renderer in this process (Linux).
static COMPAT_RENDERER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Linux: applies the "compatibility renderer" setting. WebKit reads the variable when it starts,
/// so this has to run before Tauri creates anything; the settings file is found the same way
/// Tauri's `app_config_dir` does (`$XDG_CONFIG_HOME` or `~/.config`, plus the app identifier).
/// A variable already set by the user wins.
#[cfg(target_os = "linux")]
fn apply_renderer_setting() {
    const VAR: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";
    let active = match std::env::var_os(VAR) {
        // WebKit treats any value except "0" as set.
        Some(v) => v != "0",
        None => {
            let config_home = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
            let wanted = config_home.is_some_and(|dir| {
                let location = data_location_in(dir.join("io.github.cogarg.nanokvm-control"));
                store::load_settings(&location.devices_file).compat_renderer
            });
            if wanted {
                // Still single-threaded here.
                std::env::set_var(VAR, "1");
            }
            wanted
        }
    };
    COMPAT_RENDERER.store(active, std::sync::atomic::Ordering::Relaxed);
}

/// Where a device's console ended up after a connect request.
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum OpenedIn {
    Tab,
    Window,
}

/// Linux/WebKitGTK only. Tauri packs every child webview of a window into the window's vertical
/// GtkBox, so they can't overlap and `set_bounds` is ignored. At startup the UI webview is moved
/// into a GtkOverlay; console tabs become overlay children that fill the area below the tab bar.
/// Overlay children don't count towards the window's minimum size, so it still resizes freely.
/// All of this runs on the GTK main thread, hence the thread-local.
#[cfg(target_os = "linux")]
mod gtk_overlay {
    use gtk::prelude::*;
    use std::cell::RefCell;

    thread_local! {
        static OVERLAY: RefCell<Option<gtk::Overlay>> = const { RefCell::new(None) };
    }

    pub fn install(ui: gtk::Widget) {
        let Some(vbox) = ui.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) else {
            return;
        };
        let overlay = gtk::Overlay::new();
        vbox.remove(&ui);
        overlay.add(&ui);
        vbox.pack_start(&overlay, true, true, 0);
        overlay.show();
        OVERLAY.with(|o| *o.borrow_mut() = Some(overlay));
    }

    pub fn adopt_console(console: gtk::Widget, rect: (i32, i32, i32, i32)) {
        OVERLAY.with(|o| {
            let Some(overlay) = &*o.borrow() else { return };
            if let Some(parent) = console.parent().and_then(|p| p.downcast::<gtk::Container>().ok()) {
                parent.remove(&console);
            }
            console.set_halign(gtk::Align::Fill);
            console.set_valign(gtk::Align::Fill);
            overlay.add_overlay(&console);
        });
        place(console, rect);
    }

    /// Positions a console at (x, y, width, height) within the overlay, via margins (the overlay
    /// stretches its children; margins are what positions them).
    pub fn place(console: gtk::Widget, (x, y, w, h): (i32, i32, i32, i32)) {
        OVERLAY.with(|o| {
            let Some(overlay) = &*o.borrow() else { return };
            let (ow, oh) = (overlay.allocated_width(), overlay.allocated_height());
            console.set_margin_start(x.max(0));
            console.set_margin_top(y.max(0));
            console.set_margin_end((ow - x - w).max(0));
            console.set_margin_bottom((oh - y - h).max(0));
        });
    }
}

fn tab_label(id: &str) -> String {
    format!("tab-{id}")
}

fn window_label(id: &str) -> String {
    format!("kvm-{id}")
}

fn device(state: &AppState, id: &str) -> Result<Device, String> {
    state.store.lock().unwrap().get(id).cloned().ok_or_else(|| "Unknown device".into())
}

/// Decides from the login reply `res` = {ok, status, j (JSON or null)}: null on success, else
/// the reason shown to the user.
const NANOKVM_JUDGE: &str = r#"function (res) {
      var code = res.j && typeof res.j.code === 'number' ? res.j.code : null;
      if (res.ok && (code === null || code === 0)) return null;
      var reasons = {
        '-2': 'the saved username or password is wrong',
        '-4': 'the NanoKVM has temporarily locked all logins after too many failed attempts',
        '-5': 'this account is temporarily locked after too many failed attempts'
      };
      return reasons[String(code)] || ('the NanoKVM answered: ' + ((res.j && res.j.msg) || ('HTTP ' + res.status)));
    }"#;

const PIKVM_JUDGE: &str = r#"function (res) {
      if (res.ok) return null;
      if (res.status === 403) return 'the saved username or password is wrong (two-factor codes are not supported)';
      return 'the PiKVM answered HTTP ' + res.status;
    }"#;

/// Script injected into a console webview: logs in once per webview, from the device's own origin,
/// so the session cookie lands in the webview's cookie jar exactly as with a manual login.
fn autologin_script(d: &Device) -> Option<String> {
    if !d.auto_login || d.username.is_empty() || d.password.is_empty() {
        return None;
    }
    let origin = serde_json::to_string(&d.url).ok()?;
    let (content_type, body, judge, after, product) = match d.kind {
        // NanoKVM: JSON body, password encrypted the way its web UI does it.
        DeviceKind::Nanokvm => (
            "application/json",
            serde_json::to_string(&serde_json::json!({
                "username": d.username,
                "password": crypto::encode_password(&d.password),
            }))
            .ok()?,
            NANOKVM_JUDGE,
            "/",
            "NanoKVM",
        ),
        // PiKVM: form body (https://docs.pikvm.org/api/#authentication), then its KVM page.
        DeviceKind::Pikvm => (
            "application/x-www-form-urlencoded",
            format!("user={}&passwd={}", urlencoding::encode(&d.username), urlencoding::encode(&d.password)),
            PIKVM_JUDGE,
            "/kvm/",
            "PiKVM",
        ),
        _ => return None,
    };
    let body = serde_json::to_string(&body).ok()?; // embed as a JS string literal
    let after = serde_json::to_string(after).ok()?;
    Some(format!(
        r#"(function () {{
  if (window.top !== window || location.origin !== {origin}) return;
  var FLAG = '__nanokvm_control_autologin';
  try {{ if (sessionStorage.getItem(FLAG)) return; sessionStorage.setItem(FLAG, '1'); }} catch (e) {{ return; }}
  var judge = {judge};
  fetch('/api/auth/login', {{
    method: 'POST',
    credentials: 'same-origin',
    headers: {{ 'Content-Type': '{content_type}' }},
    body: {body}
  }})
    .then(function (r) {{
      return r.json().catch(function () {{ return null; }})
        .then(function (j) {{ return {{ ok: r.ok, status: r.status, j: j }}; }});
    }})
    .then(function (res) {{
      var reason = judge(res);
      if (reason === null) {{ location.replace(location.origin + {after}); return; }}
      fail(reason);
    }})
    .catch(function (e) {{ fail('the login request failed (' + e + ')'); }});

  // Tell the user instead of failing silently; the banner sits above the {product} page.
  function fail(reason) {{
    console.warn('[NanoKVM Control] auto-login failed: ' + reason);
    function show() {{
      var bar = document.createElement('div');
      bar.setAttribute('role', 'alert');
      bar.style.cssText = 'position:fixed;top:0;left:0;right:0;z-index:2147483647;display:flex;gap:12px;' +
        'align-items:center;padding:10px 16px;background:#7a271a;color:#fff;font:14px/1.4 system-ui,sans-serif;' +
        'box-shadow:0 2px 8px rgba(0,0,0,.35)';
      var text = document.createElement('span');
      text.style.flex = '1';
      text.textContent = 'NanoKVM Control could not log in automatically: ' + reason +
        '. Log in below, or fix the saved credentials with Edit on the device card.';
      var close = document.createElement('button');
      close.textContent = 'Dismiss';
      close.style.cssText = 'font:inherit;color:#fff;background:transparent;border:1px solid rgba(255,255,255,.6);' +
        'border-radius:6px;padding:4px 10px;cursor:pointer';
      close.onclick = function () {{ bar.remove(); }};
      bar.appendChild(text);
      bar.appendChild(close);
      document.body.appendChild(bar);
    }}
    if (document.body) show(); else document.addEventListener('DOMContentLoaded', show);
  }}
}})();"#
    ))
}

/// Script injected into every console webview: restarts the MJPEG stream if no picture arrives
/// (see stream_watchdog.js for the WebKit behaviour it works around).
fn stream_watchdog_script(d: &Device) -> Option<String> {
    let origin = serde_json::to_string(&d.url).ok()?;
    Some(include_str!("stream_watchdog.js").replace("__ORIGIN__", &origin))
}

/// Linux/WebKitGTK: turn off the disk cache of a webview's web context. With it on, WebKit
/// learns that the console page loads `/api/stream/mjpeg` and later fetches it speculatively;
/// the never-ending stream then starves the page's own request and the console stays black.
/// The UI and all consoles share one context (same data directory), so calling this on the UI
/// webview at startup covers every console before it loads.
#[cfg(target_os = "linux")]
fn disable_disk_cache(webview: &webkit2gtk::WebView) {
    use webkit2gtk::{CacheModel, WebContextExt, WebViewExt};
    if let Some(ctx) = webview.context() {
        if ctx.cache_model() != CacheModel::DocumentViewer {
            ctx.set_cache_model(CacheModel::DocumentViewer);
        }
    }
}

fn console_url(d: &Device) -> Result<WebviewUrl, String> {
    let path = if d.kind == DeviceKind::Pikvm { "/kvm/" } else { "/" };
    Ok(WebviewUrl::External(Url::parse(&format!("{}{path}", d.url)).map_err(|e| e.to_string())?))
}

/// What a console webview loads and which scripts it gets.
struct ConsoleSpec {
    url: WebviewUrl,
    scripts: Vec<String>,
    /// Window title for pop-out windows.
    title: String,
    /// HTTPS web console: the device whose pinned certificate the webview must accept (see
    /// console_tls). The webview then starts on about:blank and is navigated once the check is in.
    pin: Option<String>,
}

impl ConsoleSpec {
    /// The URL to create the webview with.
    fn initial_url(&self) -> WebviewUrl {
        if self.pin.is_some() {
            WebviewUrl::External(Url::parse("about:blank").unwrap())
        } else {
            self.url.clone()
        }
    }

    /// The URL to navigate to after the certificate check is installed (when there is one).
    fn deferred_url(&self) -> Option<Url> {
        match (&self.pin, &self.url) {
            (Some(_), WebviewUrl::External(u)) => Some(u.clone()),
            _ => None,
        }
    }
}

/// Certificate check for a device's HTTPS console: pins the first certificate, then insists on it.
fn pin_check(app: &AppHandle, id: &str) -> console_tls::Check {
    let (app, id) = (app.clone(), id.to_string());
    Arc::new(move |fp: &str| {
        let state = app.state::<AppState>();
        let mut store = state.store.lock().unwrap();
        let Some(d) = store.get(&id).cloned() else {
            return console_tls::Verdict::Refuse("This device no longer exists.".into());
        };
        if d.tls_fingerprint.is_empty() {
            store.pin_fingerprint(&id, fp);
            console_tls::Verdict::Allow
        } else if d.tls_fingerprint == fp {
            console_tls::Verdict::Allow
        } else {
            console_tls::Verdict::Refuse(format!(
                "{} presented a different certificate than the one saved for it, so its console was not loaded \
                 (it could be someone impersonating it). New SHA-256 fingerprint: {fp}. If you expected this, \
                 for example after a certificate renewal, use Edit > Forget certificate on the device and connect again.",
                d.name
            ))
        }
    })
}

/// Installs the certificate check in a freshly created console webview (Linux and Windows).
fn install_pin_check(p: &tauri::webview::PlatformWebview, check: console_tls::Check) {
    #[cfg(target_os = "linux")]
    console_tls::install(&p.inner(), check);
    #[cfg(windows)]
    if let Err(e) = console_tls::install(p, check) {
        eprintln!("[console] could not install the certificate check: {e}");
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = (p, check);
}

/// HTTPS web consoles pin the device's certificate in the webview.
fn web_console_pin(d: &Device) -> Option<String> {
    (d.kind.web_console() && d.url.starts_with("https://")).then(|| d.id.clone())
}

/// Console ids are device ids, or `pve:<server id>:<node>:<qemu|lxc>:<vmid>` for Proxmox guests.
const GUEST_PREFIX: &str = "pve:";

fn guest_id(server: &str, g: &proxmox::Guest) -> String {
    format!("{GUEST_PREFIX}{server}:{}:{}:{}", g.node, g.kind, g.vmid)
}

fn parse_guest_id(id: &str) -> Option<(String, proxmox::Target)> {
    let mut parts = id.strip_prefix(GUEST_PREFIX)?.split(':');
    let (server, node, kind, vmid) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !matches!(kind, "qemu" | "lxc") {
        return None;
    }
    Some((server.to_string(), proxmox::Target { node: node.into(), kind: kind.into(), vmid: vmid.parse().ok()? }))
}

fn pve_server(d: &Device) -> proxmox::Server {
    proxmox::Server {
        base: d.url.clone(),
        username: d.username.clone(),
        password: d.password.clone(),
        fingerprint: Some(d.tls_fingerprint.clone()).filter(|f| !f.is_empty()),
    }
}

/// Pins a Proxmox server's certificate the first time it is seen.
fn pin_callback(app: &AppHandle, server_id: &str) -> Arc<dyn Fn(String) + Send + Sync> {
    let (app, id) = (app.clone(), server_id.to_string());
    Arc::new(move |fp| app.state::<AppState>().store.lock().unwrap().pin_fingerprint(&id, &fp))
}

fn vnc_page(relay: &relay::Relay, name: &str) -> String {
    let config = serde_json::json!({ "port": relay.port, "token": relay.token, "name": name });
    format!("window.__NKC_VNC = {config};")
}

/// NanoKVM: its own web UI, plus auto-login and the stream watchdog. Supermicro and Proxmox: the
/// bundled noVNC page (ui/vnc), talking to a relay started for this console (`label`).
async fn console_spec(app: &AppHandle, state: &AppState, id: &str, label: &str) -> Result<ConsoleSpec, String> {
    if let Some((server_id, target)) = parse_guest_id(id) {
        let server = device(state, &server_id)?;
        if server.username.is_empty() || server.password.is_empty() {
            return Err("Save the Proxmox username and password first (Edit)".into());
        }
        let name = state
            .guest_names
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_else(|| format!("{} {}", if target.kind == "lxc" { "CT" } else { "VM" }, target.vmid));
        let relay = proxmox::start_console_relay(
            pve_server(&server),
            state.pve_sessions.clone(),
            target,
            pin_callback(app, &server_id),
        )
        .await?;
        let script = vnc_page(&relay, &name);
        state.relays.lock().unwrap().insert(label.to_string(), relay);
        return Ok(ConsoleSpec {
            url: WebviewUrl::App("vnc/index.html".into()),
            scripts: vec![script],
            title: format!("{name} — {}", server.name),
            pin: None,
        });
    }
    let d = device(state, id)?;
    match d.kind {
        DeviceKind::Nanokvm => Ok(ConsoleSpec {
            url: console_url(&d)?,
            scripts: [autologin_script(&d), stream_watchdog_script(&d)].into_iter().flatten().collect(),
            title: format!("{} — NanoKVM", d.name),
            pin: web_console_pin(&d),
        }),
        DeviceKind::Pikvm => Ok(ConsoleSpec {
            url: console_url(&d)?,
            scripts: autologin_script(&d).into_iter().collect(),
            title: format!("{} — PiKVM", d.name),
            pin: web_console_pin(&d),
        }),
        DeviceKind::Idrac => {
            if d.username.is_empty() || d.password.is_empty() {
                return Err("Save the iDRAC username and password first (Edit)".into());
            }
            let reply = idrac::console_url(&idrac::Idrac {
                base: d.url.clone(),
                username: d.username.clone(),
                password: d.password.clone(),
                fingerprint: Some(d.tls_fingerprint.clone()).filter(|f| !f.is_empty()),
            })
            .await;
            if let Some(fp) = reply.fingerprint {
                state.store.lock().unwrap().pin_fingerprint(&d.id, &fp);
            }
            let location = Url::parse(&reply.result?).map_err(|e| e.to_string())?;
            Ok(ConsoleSpec {
                url: WebviewUrl::External(location),
                scripts: vec![],
                title: format!("{} — iDRAC", d.name),
                pin: Some(d.id.clone()),
            })
        }
        DeviceKind::Vnc => {
            let url = Url::parse(&d.url).map_err(|e| e.to_string())?;
            let host = url.host_str().ok_or("The address has no host")?.trim_matches(['[', ']']).to_string();
            let port = url.port().unwrap_or(5900);
            let password = Some(d.password.clone()).filter(|p| !p.is_empty());
            let relay = vnc::start_relay(host, port, password).await?;
            let script = vnc_page(&relay, &d.name);
            state.relays.lock().unwrap().insert(label.to_string(), relay);
            Ok(ConsoleSpec {
                url: WebviewUrl::App("vnc/index.html".into()),
                scripts: vec![script],
                title: format!("{} — VNC", d.name),
                pin: None,
            })
        }
        DeviceKind::SupermicroIkvm => {
            if d.username.is_empty() || d.password.is_empty() {
                return Err("Save the BMC username and password first (Edit)".into());
            }
            let url = Url::parse(&d.url).map_err(|e| e.to_string())?;
            let host = url.host_str().ok_or("The address has no host")?.to_string();
            let bmc = ikvm::Bmc { base: d.url.clone(), username: d.username.clone(), password: d.password.clone() };
            let relay = ikvm::start_relay(state.bmc_http.clone(), bmc, host).await?;
            let script = vnc_page(&relay, &d.name);
            state.relays.lock().unwrap().insert(label.to_string(), relay);
            Ok(ConsoleSpec {
                url: WebviewUrl::App("vnc/index.html".into()),
                scripts: vec![script],
                title: format!("{} — iKVM", d.name),
                pin: None,
            })
        }
        DeviceKind::Proxmox => Err("A Proxmox server has no console of its own; connect to one of its VMs".into()),
    }
}

/// Stops the relay of a console, if it had one.
fn stop_relay(app: &AppHandle, label: &str) {
    app.state::<AppState>().relays.lock().unwrap().remove(label);
}

fn focus_window(app: &AppHandle, label: &str) -> bool {
    match app.get_webview_window(label) {
        Some(w) => {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
            true
        }
        None => false,
    }
}

/// Shows only the given console tab (or none, for the device list) and focuses it.
fn show_only(app: &AppHandle, id: Option<&str>) {
    let wanted = id.map(tab_label);
    for (label, webview) in app.webviews() {
        if !label.starts_with("tab-") {
            continue;
        }
        if Some(&label) == wanted.as_ref() {
            let _ = webview.show();
            let _ = webview.set_focus();
        } else {
            let _ = webview.hide();
        }
    }
    if id.is_none() {
        if let Some(ui) = app.get_webview(UI_WEBVIEW) {
            let _ = ui.set_focus();
        }
    }
}

fn close_tab_webview(app: &AppHandle, id: &str) {
    if let Some(w) = app.get_webview(&tab_label(id)) {
        let _ = w.close();
    }
    stop_relay(app, &tab_label(id));
}

// ---------------------------------------------------------------- device store

#[tauri::command]
fn list_devices(state: State<AppState>) -> Vec<DeviceView> {
    state.store.lock().unwrap().views()
}

/// Settings plus where passwords are kept ("keychain" or "file").
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    #[serde(flatten)]
    settings: store::Settings,
    password_storage: &'static str,
    /// The renderer option exists on this platform (Linux).
    renderer_option: bool,
    /// The compatibility renderer is in use right now (it changes only on restart).
    compat_renderer_active: bool,
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> SettingsView {
    let store = state.store.lock().unwrap();
    SettingsView {
        settings: store::load_settings(store.path()),
        password_storage: store.password_storage(),
        renderer_option: cfg!(target_os = "linux"),
        compat_renderer_active: COMPAT_RENDERER.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Saves the settings; open tabs are only remembered while "reopen tabs" is on.
#[tauri::command]
fn save_settings(state: State<AppState>, settings: store::Settings) -> Result<(), String> {
    let store = state.store.lock().unwrap();
    store::save_settings(store.path(), &settings)
}

/// Writes an encrypted export of all devices (with passwords) and groups to a file the user picks.
/// Returns the file's path, or `None` if the user cancelled.
#[tauri::command]
async fn export_devices(app: AppHandle, state: State<'_, AppState>, passphrase: String) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    if passphrase.chars().count() < backup::MIN_PASSPHRASE {
        return Err(format!("Use a passphrase of at least {} characters", backup::MIN_PASSPHRASE));
    }
    let contents = state.store.lock().unwrap().export_contents();
    let picker = app
        .dialog()
        .file()
        .set_title("Export devices")
        .set_file_name(format!("nanokvm-control-devices.{}", backup::EXTENSION))
        .add_filter("NanoKVM Control backup", &[backup::EXTENSION]);
    let path = tauri::async_runtime::spawn_blocking(move || picker.blocking_save_file())
        .await
        .map_err(|e| e.to_string())?;
    let Some(path) = path.and_then(|p| p.into_path().ok()) else { return Ok(None) };
    // Argon2 with 64 MiB takes a moment; keep it off the async workers.
    let text = tauri::async_runtime::spawn_blocking(move || backup::encrypt(&contents, &passphrase))
        .await
        .map_err(|e| e.to_string())??;
    std::fs::write(&path, text).map_err(|e| format!("Could not write the file: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(Some(path.display().to_string()))
}

/// Reads an export the user picks and merges it into the device list. Returns a summary, or
/// `None` if the user cancelled.
#[tauri::command]
async fn import_devices(app: AppHandle, state: State<'_, AppState>, passphrase: String) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let picker = app
        .dialog()
        .file()
        .set_title("Import devices")
        .add_filter("NanoKVM Control backup", &[backup::EXTENSION]);
    let path = tauri::async_runtime::spawn_blocking(move || picker.blocking_pick_file())
        .await
        .map_err(|e| e.to_string())?;
    let Some(path) = path.and_then(|p| p.into_path().ok()) else { return Ok(None) };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("Could not read the file: {e}"))?;
    let contents = tauri::async_runtime::spawn_blocking(move || backup::decrypt(&text, &passphrase))
        .await
        .map_err(|e| e.to_string())??;
    let (added, updated, groups) = state.store.lock().unwrap().import(contents)?;
    Ok(Some(format!(
        "Imported: {added} new device{}, {updated} updated, {groups} new group{}.",
        if added == 1 { "" } else { "s" },
        if groups == 1 { "" } else { "s" }
    )))
}

/// This computer's networks, offered as ranges for "Find devices".
#[tauri::command]
fn local_networks() -> Vec<String> {
    discovery::local_networks()
}

/// A device found by a scan, plus the name it already has in the list (if any).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FoundView {
    #[serde(flatten)]
    found: discovery::Found,
    existing: Option<String>,
}

/// Scans the address range the user entered for supported devices.
#[tauri::command]
async fn discover(state: State<'_, AppState>, range: String) -> Result<Vec<FoundView>, String> {
    let addresses = discovery::parse_ranges(&range)?;
    let found = discovery::scan(addresses).await;
    let devices = state.store.lock().unwrap().views();
    let host_of = |url: &str| {
        let with_scheme = if url.contains("://") { url.to_string() } else { format!("x://{url}") };
        Url::parse(&with_scheme).ok().and_then(|u| u.host_str().map(str::to_string))
    };
    Ok(found
        .into_iter()
        .map(|f| {
            let existing = devices
                .iter()
                .find(|d| host_of(&d.url).as_deref() == Some(f.address.as_str()))
                .map(|d| d.name.clone());
            FoundView { found: f, existing }
        })
        .collect())
}

#[tauri::command]
fn list_groups(state: State<AppState>) -> Vec<Group> {
    state.store.lock().unwrap().groups()
}

#[tauri::command]
fn save_group(state: State<AppState>, input: GroupInput) -> Result<Group, String> {
    state.store.lock().unwrap().upsert_group(input)
}

#[tauri::command]
fn delete_group(state: State<AppState>, id: String) -> Result<(), String> {
    state.store.lock().unwrap().delete_group(&id)
}

#[tauri::command]
fn save_device(state: State<AppState>, input: DeviceInput) -> Result<DeviceView, String> {
    state.store.lock().unwrap().upsert(input)
}

#[tauri::command]
fn delete_device(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    // Its own console, and (for a Proxmox server) its guests' consoles.
    let guests = format!("{GUEST_PREFIX}{id}:");
    let mut ids = vec![id.clone()];
    for label in app.webviews().into_keys() {
        let console = label.strip_prefix("tab-").or_else(|| label.strip_prefix("kvm-"));
        if let Some(c) = console.filter(|c| c.starts_with(&guests)) {
            ids.push(c.to_string());
        }
    }
    for c in ids {
        close_tab_webview(&app, &c);
        if let Some(w) = app.get_webview_window(&window_label(&c)) {
            let _ = w.close();
        }
        stop_relay(&app, &window_label(&c));
    }
    state.store.lock().unwrap().delete(&id)
}

/// A Proxmox guest as the UI shows it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GuestView {
    id: String,
    #[serde(flatten)]
    guest: proxmox::Guest,
}

/// Lists a Proxmox server's VMs and containers (templates left out).
#[tauri::command]
async fn list_guests(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<Vec<GuestView>, String> {
    let server = device(&state, &id)?;
    if server.kind != DeviceKind::Proxmox {
        return Err("Not a Proxmox server".into());
    }
    if server.username.is_empty() || server.password.is_empty() {
        return Err("Save the Proxmox username and password first (Edit)".into());
    }
    let reply = proxmox::list_guests(&pve_server(&server), &state.pve_sessions).await;
    if let Some(fp) = reply.fingerprint {
        pin_callback(&app, &id)(fp);
    }
    let guests: Vec<GuestView> =
        reply.result?.into_iter().map(|g| GuestView { id: guest_id(&id, &g), guest: g }).collect();
    let mut names = state.guest_names.lock().unwrap();
    names.retain(|k, _| !k.starts_with(&format!("{GUEST_PREFIX}{id}:")));
    names.extend(guests.iter().map(|g| (g.id.clone(), g.guest.name.clone())));
    Ok(guests)
}

/// Reachability check: any HTTP response from the device counts as online.
#[tauri::command]
async fn check_device(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let d = device(&state, &id)?;
    let http = match d.kind {
        DeviceKind::Nanokvm => &state.http,
        // Older BMCs need the system TLS library (see ikvm::http_client); keep the short timeout.
        DeviceKind::SupermicroIkvm => &state.bmc_http,
        // Checked with the pinned certificate.
        DeviceKind::Proxmox => {
            let server = pve_server(&d);
            return Ok(tokio::time::timeout(Duration::from_secs(4), proxmox::reachable(&server)).await.unwrap_or(false));
        }
        // Self-signed certificates are fine for a reachability check.
        DeviceKind::Pikvm | DeviceKind::Idrac => &state.http,
        // Reachable if its port accepts a connection.
        DeviceKind::Vnc => {
            let url = Url::parse(&d.url).map_err(|e| e.to_string())?;
            let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']).to_string();
            let port = url.port().unwrap_or(5900);
            let connect = tokio::net::TcpStream::connect((host.as_str(), port));
            return Ok(matches!(tokio::time::timeout(Duration::from_secs(4), connect).await, Ok(Ok(_))));
        }
    };
    let check = http.get(&d.url).send();
    Ok(matches!(tokio::time::timeout(Duration::from_secs(4), check).await, Ok(Ok(_))))
}

// ---------------------------------------------------------------- consoles

/// Opens the device's console as a tab in the main window and shows it. If the console is
/// already popped out into its own window, that window is focused instead.
#[tauri::command]
async fn open_tab(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<OpenedIn, String> {
    if focus_window(&app, &window_label(&id)) {
        return Ok(OpenedIn::Window);
    }
    if app.get_webview(&tab_label(&id)).is_none() {
        let window = app.get_window(MAIN_WINDOW).ok_or("Main window is gone")?;
        let bounds = *state.bounds.lock().unwrap();
        let label = tab_label(&id);
        let spec = console_spec(&app, &state, &id, &label).await?;
        let mut builder = WebviewBuilder::new(&label, spec.initial_url());
        if let Some(dir) = &state.webview_data {
            builder = builder.data_directory(dir.clone());
        }
        for script in spec.scripts.iter().cloned() {
            builder = builder.initialization_script(script);
        }
        let rect = bounds.rect();
        let webview = match window.add_child(builder, rect.position, rect.size) {
            Ok(w) => w,
            Err(e) => {
                stop_relay(&app, &label);
                return Err(format!("Could not open console: {e}"));
            }
        };
        let check = spec.pin.as_deref().map(|id| pin_check(&app, id));
        let _ = webview.with_webview(move |p| {
            if let Some(check) = check {
                install_pin_check(&p, check);
            }
            #[cfg(target_os = "linux")]
            {
                use gtk::prelude::Cast;
                disable_disk_cache(&p.inner());
                gtk_overlay::adopt_console(p.inner().upcast(), bounds.gtk_rect());
            }
        });
        // Runs after the closure above (both go through the main thread's queue, in order).
        if let Some(url) = spec.deferred_url() {
            let _ = webview.navigate(url);
        }
    }
    show_only(&app, Some(&id));
    Ok(OpenedIn::Tab)
}

/// Switches the visible tab; `None` shows the device list.
#[tauri::command]
fn show_tab(app: AppHandle, id: Option<String>) {
    show_only(&app, id.as_deref());
}

#[tauri::command]
fn close_tab(app: AppHandle, id: String) {
    close_tab_webview(&app, &id);
}

/// Opens (or focuses) the device's console in its own window, closing its tab if it had one.
#[tauri::command]
async fn open_window(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    close_tab_webview(&app, &id);
    let label = window_label(&id);
    if focus_window(&app, &label) {
        return Ok(());
    }
    let spec = console_spec(&app, &state, &id, &label).await?;
    let mut builder = WebviewWindowBuilder::new(&app, &label, spec.initial_url())
        .title(&spec.title)
        .inner_size(1280.0, 800.0)
        .min_inner_size(640.0, 400.0);
    if let Some(dir) = &state.webview_data {
        builder = builder.data_directory(dir.clone());
    }
    for script in spec.scripts.iter().cloned() {
        builder = builder.initialization_script(script);
    }
    let window = match builder.build() {
        Ok(w) => w,
        Err(e) => {
            stop_relay(&app, &label);
            return Err(format!("Could not open console window: {e}"));
        }
    };
    let (handle, closed_label) = (app.clone(), label.clone());
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            stop_relay(&handle, &closed_label);
        }
    });
    let check = spec.pin.as_deref().map(|id| pin_check(&app, id));
    let _ = window.with_webview(move |p| {
        if let Some(check) = check {
            install_pin_check(&p, check);
        }
        #[cfg(target_os = "linux")]
        disable_disk_cache(&p.inner());
    });
    if let Some(url) = spec.deferred_url() {
        let _ = window.navigate(url);
    }
    Ok(())
}

/// One console placed by the UI: its id and where it goes (logical px, window-relative).
#[derive(Deserialize)]
struct Tile {
    id: String,
    #[serde(flatten)]
    bounds: ConsoleBounds,
}

/// Lays out the console tabs: the given ones are shown at their rectangles, all others hidden.
/// One tile is the normal single-tab view, several are the tiled view, none is the device list.
/// `area` (everything below the tab bar) is where new tabs start out.
#[tauri::command]
fn set_layout(app: AppHandle, state: State<AppState>, area: ConsoleBounds, tiles: Vec<Tile>, focus: Option<String>) {
    *state.bounds.lock().unwrap() = area;
    let placed: HashMap<String, ConsoleBounds> = tiles.into_iter().map(|t| (tab_label(&t.id), t.bounds)).collect();
    for (label, webview) in app.webviews() {
        if !label.starts_with("tab-") {
            continue;
        }
        match placed.get(&label) {
            Some(&bounds) => {
                let _ = webview.set_bounds(bounds.rect());
                #[cfg(target_os = "linux")]
                let _ = webview.with_webview(move |p| {
                    use gtk::prelude::Cast;
                    gtk_overlay::place(p.inner().upcast(), bounds.gtk_rect());
                });
                let _ = webview.show();
            }
            None => {
                let _ = webview.hide();
            }
        }
    }
    match focus.and_then(|id| app.get_webview(&tab_label(&id))) {
        Some(w) => {
            let _ = w.set_focus();
        }
        None if placed.is_empty() => {
            if let Some(ui) = app.get_webview(UI_WEBVIEW) {
                let _ = ui.set_focus();
            }
        }
        None => {}
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    apply_renderer_setting();
    tauri::Builder::default()
        // File pickers for export/import, used from Rust only (the UI gets no dialog permissions).
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let location = data_location(app)?;
            let http = reqwest::Client::builder()
                .use_rustls_tls()
                .timeout(Duration::from_secs(3))
                // NanoKVM's optional HTTPS mode uses a self-signed certificate.
                .danger_accept_invalid_certs(true)
                .build()?;
            app.manage(AppState {
                store: Mutex::new(Store::load(location.devices_file, {
                    // Separate data folders get separate keychain entries (see secrets.rs).
                    let namespace = location.isolated_root.as_ref().map(|p| p.display().to_string());
                    Arc::new(secrets::Keychain::new(namespace.as_deref()))
                })),
                http,
                bmc_http: ikvm::http_client()?,
                relays: Mutex::new(HashMap::new()),
                pve_sessions: Arc::new(proxmox::Sessions::default()),
                guest_names: Mutex::new(HashMap::new()),
                bounds: Mutex::new(ConsoleBounds { x: 0.0, y: 44.0, width: 1280.0, height: 776.0 }),
                webview_data: location.webview_data.clone(),
            });
            let title = match &location.isolated_root {
                Some(root) => format!("NanoKVM Control — data: {}", root.display()),
                None => "NanoKVM Control".to_string(),
            };

            // A plain window hosting several webviews: the UI fills it, console tabs sit on top
            // of the area below the UI's tab bar.
            let window = WindowBuilder::new(app, MAIN_WINDOW)
                .title(title)
                .inner_size(1280.0, 820.0)
                .min_inner_size(640.0, 420.0)
                .build()?;
            let size = window.inner_size()?.to_logical::<f64>(window.scale_factor()?);
            let mut ui_builder = WebviewBuilder::new(UI_WEBVIEW, WebviewUrl::App("index.html".into())).auto_resize();
            if let Some(dir) = location.webview_data {
                ui_builder = ui_builder.data_directory(dir);
            }
            let ui = window.add_child(ui_builder, LogicalPosition::new(0.0, 0.0), size)?;
            #[cfg(target_os = "linux")]
            ui.with_webview(|p| {
                use gtk::prelude::Cast;
                disable_disk_cache(&p.inner());
                gtk_overlay::install(p.inner().upcast());
            })?;
            #[cfg(not(target_os = "linux"))]
            let _ = ui;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_devices,
            get_settings,
            save_settings,
            export_devices,
            import_devices,
            local_networks,
            discover,
            list_groups,
            save_group,
            delete_group,
            save_device,
            delete_device,
            check_device,
            list_guests,
            open_tab,
            show_tab,
            close_tab,
            open_window,
            set_layout
        ])
        .run(tauri::generate_context!())
        .expect("error while running NanoKVM Control");
}
