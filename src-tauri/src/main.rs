#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod crypto;
mod ikvm;
mod store;

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Mutex, time::Duration};
use store::{Device, DeviceInput, DeviceKind, DeviceView, Store};
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
    /// iKVM relays of open Supermicro consoles, by webview/window label.
    relays: Mutex<HashMap<String, ikvm::Relay>>,
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
    let config_dir = app.path().app_config_dir()?;
    let root = match std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None if cfg!(debug_assertions) => {
            let mut name = config_dir.file_name().unwrap_or_default().to_os_string();
            name.push(".dev");
            Some(config_dir.with_file_name(name))
        }
        None => None,
    };
    Ok(match root {
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
    })
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

    pub fn adopt_console(console: gtk::Widget, top: i32) {
        OVERLAY.with(|o| {
            let Some(overlay) = &*o.borrow() else { return };
            if let Some(parent) = console.parent().and_then(|p| p.downcast::<gtk::Container>().ok()) {
                parent.remove(&console);
            }
            console.set_halign(gtk::Align::Fill);
            console.set_valign(gtk::Align::Fill);
            console.set_margin_top(top);
            overlay.add_overlay(&console);
        });
    }

    pub fn set_top(top: i32) {
        OVERLAY.with(|o| {
            let Some(overlay) = &*o.borrow() else { return };
            let ui = overlay.child();
            for child in overlay.children() {
                if Some(&child) != ui.as_ref() {
                    child.set_margin_top(top);
                }
            }
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

/// Script injected into a console webview: logs in once per webview, from the NanoKVM origin
/// itself, so the server's HttpOnly `nano-kvm-token` cookie lands in the webview's cookie jar.
fn autologin_script(d: &Device) -> Option<String> {
    if !d.auto_login || d.username.is_empty() || d.password.is_empty() {
        return None;
    }
    let origin = serde_json::to_string(&d.url).ok()?;
    let body = serde_json::to_string(&serde_json::json!({
        "username": d.username,
        "password": crypto::encode_password(&d.password),
    }))
    .ok()?;
    let body = serde_json::to_string(&body).ok()?; // embed as a JS string literal
    Some(format!(
        r#"(function () {{
  if (window.top !== window || location.origin !== {origin}) return;
  var FLAG = '__nanokvm_control_autologin';
  try {{ if (sessionStorage.getItem(FLAG)) return; sessionStorage.setItem(FLAG, '1'); }} catch (e) {{ return; }}
  fetch('/api/auth/login', {{
    method: 'POST',
    credentials: 'same-origin',
    headers: {{ 'Content-Type': 'application/json' }},
    body: {body}
  }})
    .then(function (r) {{
      return r.json().catch(function () {{ return null; }})
        .then(function (j) {{ return {{ ok: r.ok, status: r.status, j: j }}; }});
    }})
    .then(function (res) {{
      var code = res.j && typeof res.j.code === 'number' ? res.j.code : null;
      if (res.ok && (code === null || code === 0)) {{ location.replace(location.origin + '/'); return; }}
      var reasons = {{
        '-2': 'the saved username or password is wrong',
        '-4': 'the NanoKVM has temporarily locked all logins after too many failed attempts',
        '-5': 'this account is temporarily locked after too many failed attempts'
      }};
      fail(reasons[String(code)] || ('the NanoKVM answered: ' + ((res.j && res.j.msg) || ('HTTP ' + res.status))));
    }})
    .catch(function (e) {{ fail('the login request failed (' + e + ')'); }});

  // Tell the user instead of failing silently; the banner sits above the NanoKVM page.
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
    Ok(WebviewUrl::External(Url::parse(&d.url).map_err(|e| e.to_string())?))
}

/// What a console webview loads and which scripts it gets, per device kind.
struct ConsoleSpec {
    url: WebviewUrl,
    scripts: Vec<String>,
}

/// NanoKVM: its own web UI, plus auto-login and the stream watchdog. Supermicro: the bundled
/// noVNC-iKVM page (ui/ikvm), talking to a relay started for this console (`label`).
async fn console_spec(state: &AppState, d: &Device, label: &str) -> Result<ConsoleSpec, String> {
    match d.kind {
        DeviceKind::Nanokvm => Ok(ConsoleSpec {
            url: console_url(d)?,
            scripts: [autologin_script(d), stream_watchdog_script(d)].into_iter().flatten().collect(),
        }),
        DeviceKind::SupermicroIkvm => {
            if d.username.is_empty() || d.password.is_empty() {
                return Err("Save the BMC username and password first (Edit)".into());
            }
            let url = Url::parse(&d.url).map_err(|e| e.to_string())?;
            let host = url.host_str().ok_or("The address has no host")?.to_string();
            let bmc = ikvm::Bmc { base: d.url.clone(), username: d.username.clone(), password: d.password.clone() };
            let relay = ikvm::start_relay(state.bmc_http.clone(), bmc, host).await?;
            let config = serde_json::json!({ "port": relay.port, "token": relay.token, "name": d.name });
            state.relays.lock().unwrap().insert(label.to_string(), relay);
            Ok(ConsoleSpec {
                url: WebviewUrl::App("ikvm/index.html".into()),
                scripts: vec![format!("window.__NKC_IKVM = {config};")],
            })
        }
    }
}

/// Stops the iKVM relay of a console, if it had one.
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

#[tauri::command]
fn save_device(state: State<AppState>, input: DeviceInput) -> Result<DeviceView, String> {
    state.store.lock().unwrap().upsert(input)
}

#[tauri::command]
fn delete_device(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> {
    close_tab_webview(&app, &id);
    if let Some(w) = app.get_webview_window(&window_label(&id)) {
        let _ = w.close();
    }
    stop_relay(&app, &window_label(&id));
    state.store.lock().unwrap().delete(&id)
}

/// Reachability check: any HTTP response from the device counts as online.
#[tauri::command]
async fn check_device(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let d = device(&state, &id)?;
    let http = match d.kind {
        DeviceKind::Nanokvm => &state.http,
        // Older BMCs need the system TLS library (see ikvm::http_client); keep the short timeout.
        DeviceKind::SupermicroIkvm => &state.bmc_http,
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
        let d = device(&state, &id)?;
        let window = app.get_window(MAIN_WINDOW).ok_or("Main window is gone")?;
        let bounds = *state.bounds.lock().unwrap();
        let label = tab_label(&id);
        let spec = console_spec(&state, &d, &label).await?;
        let mut builder = WebviewBuilder::new(&label, spec.url);
        if let Some(dir) = &state.webview_data {
            builder = builder.data_directory(dir.clone());
        }
        for script in spec.scripts {
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
        #[cfg(target_os = "linux")]
        {
            let top = bounds.y.round() as i32;
            let _ = webview.with_webview(move |p| {
                use gtk::prelude::Cast;
                disable_disk_cache(&p.inner());
                gtk_overlay::adopt_console(p.inner().upcast(), top);
            });
        }
        #[cfg(not(target_os = "linux"))]
        let _ = webview;
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
    let d = device(&state, &id)?;
    let spec = console_spec(&state, &d, &label).await?;
    let suffix = match d.kind {
        DeviceKind::Nanokvm => "NanoKVM",
        DeviceKind::SupermicroIkvm => "iKVM",
    };
    let mut builder = WebviewWindowBuilder::new(&app, &label, spec.url)
        .title(format!("{} — {suffix}", d.name))
        .inner_size(1280.0, 800.0)
        .min_inner_size(640.0, 400.0);
    if let Some(dir) = &state.webview_data {
        builder = builder.data_directory(dir.clone());
    }
    for script in spec.scripts {
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
    #[cfg(target_os = "linux")]
    let _ = window.with_webview(|p| disable_disk_cache(&p.inner()));
    #[cfg(not(target_os = "linux"))]
    let _ = window;
    Ok(())
}

/// Called by the UI whenever the area below its tab bar moves or resizes.
#[tauri::command]
fn set_console_bounds(app: AppHandle, state: State<AppState>, bounds: ConsoleBounds) {
    *state.bounds.lock().unwrap() = bounds;
    for (label, webview) in app.webviews() {
        if label.starts_with("tab-") {
            let _ = webview.set_bounds(bounds.rect());
        }
    }
    #[cfg(target_os = "linux")]
    {
        let top = bounds.y.round() as i32;
        let _ = app.run_on_main_thread(move || gtk_overlay::set_top(top));
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let location = data_location(app)?;
            let http = reqwest::Client::builder()
                .use_rustls_tls()
                .timeout(Duration::from_secs(3))
                // NanoKVM's optional HTTPS mode uses a self-signed certificate.
                .danger_accept_invalid_certs(true)
                .build()?;
            app.manage(AppState {
                store: Mutex::new(Store::load(location.devices_file)),
                http,
                bmc_http: ikvm::http_client()?,
                relays: Mutex::new(HashMap::new()),
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
            save_device,
            delete_device,
            check_device,
            open_tab,
            show_tab,
            close_tab,
            open_window,
            set_console_bounds
        ])
        .run(tauri::generate_context!())
        .expect("error while running NanoKVM Control");
}
