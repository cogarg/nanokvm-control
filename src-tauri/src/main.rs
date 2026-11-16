#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod crypto;
mod store;

use std::{sync::Mutex, time::Duration};
use store::{Device, DeviceInput, DeviceView, Store};
use tauri::{AppHandle, Manager, State, Url, WebviewUrl, WebviewWindowBuilder};

struct AppState {
    store: Mutex<Store>,
    http: reqwest::Client,
}

fn window_label(id: &str) -> String {
    format!("kvm-{id}")
}

/// Script injected into a console window: logs in once per window, from the NanoKVM origin
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
    .then(function (r) {{ return r.json().then(function (j) {{ return {{ ok: r.ok, j: j }}; }}); }})
    .then(function (res) {{
      var failed = !res.ok || (res.j && typeof res.j.code === 'number' && res.j.code !== 0);
      if (failed) {{ console.warn('[NanoKVM Control] auto-login failed', res.j); return; }}
      location.replace(location.origin + '/');
    }})
    .catch(function (e) {{ console.warn('[NanoKVM Control] auto-login error', e); }});
}})();"#
    ))
}

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
    if let Some(w) = app.get_webview_window(&window_label(&id)) {
        let _ = w.close();
    }
    state.store.lock().unwrap().delete(&id)
}

/// Reachability check: any HTTP response from the device counts as online.
#[tauri::command]
async fn check_device(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let url = state
        .store
        .lock()
        .unwrap()
        .get(&id)
        .map(|d| d.url.clone())
        .ok_or("Unknown device")?;
    Ok(state.http.get(url).send().await.is_ok())
}

/// Opens (or focuses) the device's web console in its own window.
#[tauri::command]
async fn connect_device(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    let device = state
        .store
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or("Unknown device")?;

    let label = window_label(&device.id);
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }

    let url = Url::parse(&device.url).map_err(|e| e.to_string())?;
    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(url))
        .title(format!("{} — NanoKVM", device.name))
        .inner_size(1280.0, 800.0)
        .min_inner_size(640.0, 400.0);
    if let Some(script) = autologin_script(&device) {
        builder = builder.initialization_script(script.as_str());
    }
    builder.build().map_err(|e| format!("Could not open console window: {e}"))?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let path = app.path().app_config_dir()?.join("devices.json");
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                // NanoKVM's optional HTTPS mode uses a self-signed certificate.
                .danger_accept_invalid_certs(true)
                .build()?;
            app.manage(AppState { store: Mutex::new(Store::load(path)), http });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_devices,
            save_device,
            delete_device,
            check_device,
            connect_device
        ])
        .run(tauri::generate_context!())
        .expect("error while running NanoKVM Control");
}
