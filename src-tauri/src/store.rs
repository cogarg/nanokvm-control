//! Persistent list of devices (NanoKVMs and Supermicro BMCs), stored as JSON in the app config
//! directory.

use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf};
use tauri::Url;

/// What kind of device this is, which decides how its console is opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceKind {
    /// NanoKVM: its own web UI is the console.
    #[default]
    Nanokvm,
    /// Supermicro (or other ATEN-based) BMC whose console is the legacy Java iKVM; shown with the
    /// bundled noVNC-iKVM client (see ikvm.rs).
    SupermicroIkvm,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: DeviceKind,
    /// Normalized origin, e.g. `http://192.168.1.50` or `https://kvm.lan:8443`.
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub auto_login: bool,
}

/// What the UI gets to see; the password never leaves the backend.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub url: String,
    pub username: String,
    pub has_password: bool,
    pub auto_login: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInput {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub kind: DeviceKind,
    pub url: String,
    #[serde(default)]
    pub username: String,
    /// `None` or empty keeps the stored password when editing.
    pub password: Option<String>,
    #[serde(default)]
    pub clear_password: bool,
    #[serde(default)]
    pub auto_login: bool,
}

impl From<&Device> for DeviceView {
    fn from(d: &Device) -> Self {
        DeviceView {
            id: d.id.clone(),
            name: d.name.clone(),
            kind: d.kind,
            url: d.url.clone(),
            username: d.username.clone(),
            has_password: !d.password.is_empty(),
            auto_login: d.auto_login,
        }
    }
}

/// Accepts `192.168.1.50`, `kvm.lan:8080`, `https://kvm.lan/` etc. and returns the origin.
/// Without a scheme, NanoKVMs default to HTTP; BMCs to HTTPS, so the BMC password isn't sent in
/// clear text (their self-signed certificate is accepted for these requests).
pub fn normalize_url(input: &str, kind: DeviceKind) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Address is required".into());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else if kind == DeviceKind::SupermicroIkvm {
        format!("https://{trimmed}")
    } else {
        format!("http://{trimmed}")
    };
    let url = Url::parse(&with_scheme).map_err(|e| format!("Invalid address: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Address must use http or https".into());
    }
    if url.host_str().is_none() {
        return Err("Address has no host".into());
    }
    Ok(url.origin().ascii_serialization())
}

pub struct Store {
    path: PathBuf,
    devices: Vec<Device>,
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let devices = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Store { path, devices }
    }

    fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.devices)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, &self.path)
    }

    pub fn views(&self) -> Vec<DeviceView> {
        self.devices.iter().map(DeviceView::from).collect()
    }

    pub fn get(&self, id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn upsert(&mut self, input: DeviceInput) -> Result<DeviceView, String> {
        let name = input.name.trim().to_string();
        if name.is_empty() {
            return Err("Name is required".into());
        }
        let url = normalize_url(&input.url, input.kind)?;
        let new_password = input.password.filter(|p| !p.is_empty());

        let view = match input.id.as_deref().and_then(|id| self.devices.iter_mut().find(|d| d.id == id)) {
            Some(d) => {
                d.name = name;
                d.kind = input.kind;
                d.url = url;
                d.username = input.username.trim().to_string();
                if input.clear_password {
                    d.password.clear();
                }
                if let Some(p) = new_password {
                    d.password = p;
                }
                d.auto_login = input.auto_login;
                DeviceView::from(&*d)
            }
            None => {
                let d = Device {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    kind: input.kind,
                    url,
                    username: input.username.trim().to_string(),
                    password: new_password.unwrap_or_default(),
                    auto_login: input.auto_login,
                };
                let v = DeviceView::from(&d);
                self.devices.push(d);
                v
            }
        };
        self.save().map_err(|e| format!("Could not save devices: {e}"))?;
        Ok(view)
    }

    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        self.devices.retain(|d| d.id != id);
        self.save().map_err(|e| format!("Could not save devices: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_url, Device, DeviceKind};

    #[test]
    fn normalizes_addresses() {
        let n = DeviceKind::Nanokvm;
        assert_eq!(normalize_url("192.168.1.50", n).unwrap(), "http://192.168.1.50");
        assert_eq!(normalize_url(" kvm.lan:8080/foo ", n).unwrap(), "http://kvm.lan:8080");
        assert_eq!(normalize_url("https://kvm.lan/", n).unwrap(), "https://kvm.lan");
        assert!(normalize_url("ftp://kvm.lan", n).is_err());
        assert!(normalize_url("", n).is_err());
        let s = DeviceKind::SupermicroIkvm;
        assert_eq!(normalize_url("192.168.2.240", s).unwrap(), "https://192.168.2.240");
        assert_eq!(normalize_url("http://192.168.2.240", s).unwrap(), "http://192.168.2.240");
    }

    #[test]
    fn old_device_files_load_as_nanokvm() {
        let d: Device = serde_json::from_str(r#"{"id":"a","name":"x","url":"http://1.2.3.4"}"#).unwrap();
        assert_eq!(d.kind, DeviceKind::Nanokvm);
        let d: Device =
            serde_json::from_str(r#"{"id":"a","name":"x","kind":"supermicro-ikvm","url":"https://1.2.3.4"}"#).unwrap();
        assert_eq!(d.kind, DeviceKind::SupermicroIkvm);
    }
}
