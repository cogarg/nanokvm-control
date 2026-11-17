//! Persistent list of devices (NanoKVMs and Supermicro BMCs), stored as JSON in the app config
//! directory.

use serde::{Deserialize, Serialize};
use crate::secrets::Secrets;
use std::{fs, io, path::PathBuf, sync::Arc};
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
    /// Proxmox VE server: not a console itself, but a group whose VMs and containers are listed
    /// from its API (see proxmox.rs).
    Proxmox,
    /// PiKVM (and boards based on it): its own web UI is the console, like NanoKVM.
    Pikvm,
    /// Dell iDRAC 9: its HTML5 virtual console (see idrac.rs).
    Idrac,
    /// Any VNC server, shown with the bundled noVNC page (see vnc.rs).
    Vnc,
}

impl DeviceKind {
    /// Kinds whose console is the device's own web page (so its certificate matters there too).
    pub fn web_console(self) -> bool {
        matches!(self, DeviceKind::Nanokvm | DeviceKind::Pikvm | DeviceKind::Idrac)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: DeviceKind,
    /// Optional group name for filtering the device list; empty means no group.
    #[serde(default)]
    pub group: String,
    /// Normalized origin, e.g. `http://192.168.1.50` or `https://kvm.lan:8443`.
    pub url: String,
    #[serde(default)]
    pub username: String,
    /// In memory: always the real password. In `devices.json`: empty when `keychain` is set.
    #[serde(default)]
    pub password: String,
    /// The password lives in the system keychain (see secrets.rs), not in the file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keychain: bool,
    #[serde(default)]
    pub auto_login: bool,
    /// SHA-256 fingerprint of a self-signed HTTPS certificate, pinned on first contact (Proxmox,
    /// iDRAC, and HTTPS web consoles).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tls_fingerprint: String,
}

/// What the UI gets to see; the password never leaves the backend.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub group: String,
    pub url: String,
    pub username: String,
    pub has_password: bool,
    pub auto_login: bool,
    pub tls_fingerprint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInput {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub kind: DeviceKind,
    #[serde(default)]
    pub group: String,
    pub url: String,
    #[serde(default)]
    pub username: String,
    /// `None` or empty keeps the stored password when editing.
    pub password: Option<String>,
    #[serde(default)]
    pub clear_password: bool,
    #[serde(default)]
    pub auto_login: bool,
    /// Drop the pinned certificate (e.g. after the server got a new one).
    #[serde(default)]
    pub forget_fingerprint: bool,
}

impl From<&Device> for DeviceView {
    fn from(d: &Device) -> Self {
        DeviceView {
            id: d.id.clone(),
            name: d.name.clone(),
            kind: d.kind,
            group: d.group.clone(),
            url: d.url.clone(),
            username: d.username.clone(),
            has_password: !d.password.is_empty(),
            auto_login: d.auto_login,
            tls_fingerprint: d.tls_fingerprint.clone(),
        }
    }
}

/// Accepts `192.168.1.50`, `kvm.lan:8080`, `https://kvm.lan/` etc. and returns the origin
/// (for VNC: `vnc://host:port`).
/// Without a scheme, NanoKVMs default to HTTP; BMCs to HTTPS, so the BMC password isn't sent in
/// clear text (their self-signed certificate is accepted for these requests); Proxmox to HTTPS on
/// its port 8006.
pub fn normalize_url(input: &str, kind: DeviceKind) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Address is required".into());
    }
    if kind == DeviceKind::Vnc {
        return normalize_vnc(trimmed);
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else if matches!(kind, DeviceKind::SupermicroIkvm | DeviceKind::Pikvm | DeviceKind::Idrac) {
        // PiKVM and iDRAC serve HTTPS (self-signed) by default.
        format!("https://{trimmed}")
    } else if kind == DeviceKind::Proxmox {
        let host = trimmed.split('/').next().unwrap_or(trimmed);
        // No port given (an IPv6 address needs brackets to have one): use Proxmox's 8006.
        let has_port = if host.starts_with('[') { host.contains("]:") } else { host.contains(':') };
        if has_port { format!("https://{trimmed}") } else { format!("https://{host}:8006") }
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

/// A standard group: just a name devices can be put in. (Proxmox servers are groups too, but
/// they're stored as devices of kind `Proxmox`, since they have an address and a login.)
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInput {
    pub id: Option<String>,
    pub name: String,
}

/// App settings (`settings.json`, next to `devices.json`).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Reopen last session's console tabs at startup (off unless chosen).
    #[serde(default)]
    pub restore_tabs: bool,
    /// Open console tabs (ids, in order) and the visible one; only kept while `restore_tabs` is on.
    #[serde(default)]
    pub open_tabs: Vec<String>,
    #[serde(default)]
    pub active_tab: Option<String>,
}

pub fn load_settings(devices_file: &PathBuf) -> Settings {
    read_json(&devices_file.with_file_name("settings.json"))
}

pub fn save_settings(devices_file: &PathBuf, settings: &Settings) -> Result<(), String> {
    let mut settings = settings.clone();
    if !settings.restore_tabs {
        settings.open_tabs.clear();
        settings.active_tab = None;
    }
    write_json(&devices_file.with_file_name("settings.json"), &settings).map_err(|e| format!("Could not save settings: {e}"))
}

/// Trims and collapses whitespace, so "Rack  A " and "Rack A" are the same name.
fn clean_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `host`, `host:port`, `[v6]:port` or `vnc://host:port` -> `vnc://host:port` (default 5900).
fn normalize_vnc(input: &str) -> Result<String, String> {
    let rest = input.strip_prefix("vnc://").unwrap_or(input).trim_end_matches('/');
    if rest.contains("://") {
        return Err("A VNC address is a host and optional port, e.g. 192.168.1.20:5901".into());
    }
    let url = Url::parse(&format!("http://{rest}")).map_err(|e| format!("Invalid address: {e}"))?;
    let host = url.host_str().ok_or("Address has no host")?;
    if url.path() != "/" || url.query().is_some() || !url.username().is_empty() {
        return Err("A VNC address is a host and optional port, e.g. 192.168.1.20:5901".into());
    }
    // `Url` drops port 80 as the http default, so read an explicit ":80" back.
    let port = url.port().unwrap_or(if rest.ends_with(":80") { 80 } else { 5900 });
    Ok(format!("vnc://{host}:{port}"))
}

pub struct Store {
    path: PathBuf,
    devices: Vec<Device>,
    groups_path: PathBuf,
    groups: Vec<Group>,
    secrets: Arc<dyn Secrets>,
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &PathBuf) -> T {
    fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Atomic write (temp file + rename), readable by the owner only.
fn write_json<T: Serialize>(path: &PathBuf, value: &T) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path)
}

impl Store {
    /// Loads `devices.json` and the `groups.json` next to it; passwords come from `secrets`.
    pub fn load(path: PathBuf, secrets: Arc<dyn Secrets>) -> Self {
        let groups_path = path.with_file_name("groups.json");
        let mut store =
            Store { devices: read_json(&path), groups: read_json(&groups_path), path, groups_path, secrets };
        // Passwords: read the ones in the keychain; move plain-text ones there if it's usable.
        let mut migrated = false;
        for d in store.devices.iter_mut() {
            if d.keychain {
                match store.secrets.get(&d.id) {
                    Ok(p) => d.password = p.unwrap_or_default(),
                    Err(e) => eprintln!("[secrets] could not read the password of {}: {e}", d.name),
                }
            } else if !d.password.is_empty() && store.secrets.available() {
                match store.secrets.set(&d.id, &d.password) {
                    Ok(()) => {
                        d.keychain = true;
                        migrated = true;
                    }
                    Err(e) => eprintln!("[secrets] could not move the password of {} to the keychain: {e}", d.name),
                }
            }
        }
        if migrated {
            let _ = store.save();
        }
        // Version 0.5.0 had free-text groups on devices only; turn those into groups.
        let mut added = false;
        for name in store.devices.iter().filter(|d| d.kind != DeviceKind::Proxmox).map(|d| d.group.clone()) {
            if !name.is_empty() && !store.groups.iter().any(|g| g.name == name) {
                store.groups.push(Group { id: uuid::Uuid::new_v4().to_string(), name });
                added = true;
            }
        }
        if added {
            let _ = store.save_groups();
        }
        store
    }

    /// Writes `devices.json`, leaving out passwords that are in the keychain.
    fn save(&self) -> io::Result<()> {
        let on_disk: Vec<Device> = self
            .devices
            .iter()
            .map(|d| {
                let mut d = d.clone();
                if d.keychain {
                    d.password.clear();
                }
                d
            })
            .collect();
        write_json(&self.path, &on_disk)
    }

    /// Where passwords are kept: "keychain", or "file" when no keychain is usable.
    pub fn password_storage(&self) -> &'static str {
        if self.secrets.available() { "keychain" } else { "file" }
    }

    /// Puts a device's (changed) password in the keychain, or removes it from there.
    fn store_password(secrets: &dyn Secrets, d: &mut Device) {
        if !secrets.available() {
            d.keychain = false;
            return;
        }
        let result = if d.password.is_empty() { secrets.delete(&d.id) } else { secrets.set(&d.id, &d.password) };
        match result {
            Ok(()) => d.keychain = !d.password.is_empty(),
            Err(e) => {
                // Keep it usable: fall back to the file for this device.
                eprintln!("[secrets] keychain write failed for {}: {e}", d.name);
                d.keychain = false;
            }
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    fn save_groups(&self) -> io::Result<()> {
        write_json(&self.groups_path, &self.groups)
    }

    /// Group names (standard and Proxmox) must be unique, ignoring case.
    fn name_taken(&self, name: &str, except_id: Option<&str>) -> bool {
        let same = |other: &str| other.to_lowercase() == name.to_lowercase();
        self.groups.iter().any(|g| Some(g.id.as_str()) != except_id && same(&g.name))
            || self
                .devices
                .iter()
                .any(|d| d.kind == DeviceKind::Proxmox && Some(d.id.as_str()) != except_id && same(&d.name))
    }

    pub fn groups(&self) -> Vec<Group> {
        let mut groups = self.groups.clone();
        groups.sort_by_key(|g| g.name.to_lowercase());
        groups
    }

    /// Adds a group, or renames one (its devices follow).
    pub fn upsert_group(&mut self, input: GroupInput) -> Result<Group, String> {
        let name = clean_name(&input.name);
        if name.is_empty() {
            return Err("Name is required".into());
        }
        if self.name_taken(&name, input.id.as_deref()) {
            return Err(format!("There is already a group called \"{name}\""));
        }
        let group = match input.id.as_deref().and_then(|id| self.groups.iter_mut().find(|g| g.id == id)) {
            Some(g) => {
                let old = std::mem::replace(&mut g.name, name.clone());
                let group = g.clone();
                if old != name {
                    for d in self.devices.iter_mut().filter(|d| d.group == old) {
                        d.group = name.clone();
                    }
                    self.save().map_err(|e| format!("Could not save devices: {e}"))?;
                }
                group
            }
            None => {
                let g = Group { id: uuid::Uuid::new_v4().to_string(), name };
                self.groups.push(g.clone());
                g
            }
        };
        self.save_groups().map_err(|e| format!("Could not save groups: {e}"))?;
        Ok(group)
    }

    /// Deletes a group; its devices are kept, without a group.
    pub fn delete_group(&mut self, id: &str) -> Result<(), String> {
        let Some(pos) = self.groups.iter().position(|g| g.id == id) else { return Ok(()) };
        let group = self.groups.remove(pos);
        let mut changed = false;
        for d in self.devices.iter_mut().filter(|d| d.group == group.name) {
            d.group.clear();
            changed = true;
        }
        if changed {
            self.save().map_err(|e| format!("Could not save devices: {e}"))?;
        }
        self.save_groups().map_err(|e| format!("Could not save groups: {e}"))
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
        // Devices go in an existing standard group (or none); Proxmox servers are groups themselves.
        let group = clean_name(&input.group);
        if input.kind == DeviceKind::Proxmox {
            if !group.is_empty() {
                return Err("A Proxmox server can't be put in a group".into());
            }
            if self.name_taken(&name, input.id.as_deref()) {
                return Err(format!("There is already a group called \"{name}\""));
            }
        } else if !group.is_empty() && !self.groups.iter().any(|g| g.name == group) {
            return Err(format!("There is no group called \"{group}\""));
        }
        let new_password = input.password.filter(|p| !p.is_empty());

        let view = match input.id.as_deref().and_then(|id| self.devices.iter_mut().find(|d| d.id == id)) {
            Some(d) => {
                // A different server (or protocol) means a different certificate.
                if d.url != url || input.forget_fingerprint {
                    d.tls_fingerprint.clear();
                }
                d.name = name;
                d.kind = input.kind;
                d.group = group;
                d.url = url;
                d.username = input.username.trim().to_string();
                let password_changed = input.clear_password || new_password.is_some();
                if input.clear_password {
                    d.password.clear();
                }
                if let Some(p) = new_password {
                    d.password = p;
                }
                if password_changed {
                    Self::store_password(self.secrets.as_ref(), d);
                }
                d.auto_login = input.auto_login;
                DeviceView::from(&*d)
            }
            None => {
                let mut d = Device {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    kind: input.kind,
                    group,
                    url,
                    username: input.username.trim().to_string(),
                    password: new_password.unwrap_or_default(),
                    keychain: false,
                    auto_login: input.auto_login,
                    tls_fingerprint: String::new(),
                };
                if !d.password.is_empty() {
                    Self::store_password(self.secrets.as_ref(), &mut d);
                }
                let v = DeviceView::from(&d);
                self.devices.push(d);
                v
            }
        };
        self.save().map_err(|e| format!("Could not save devices: {e}"))?;
        Ok(view)
    }

    /// Everything for an export: devices with their passwords, and the standard groups.
    pub fn export_contents(&self) -> crate::backup::Contents {
        let devices = self
            .devices
            .iter()
            .map(|d| Device { keychain: false, ..d.clone() })
            .collect();
        crate::backup::Contents { devices, groups: self.groups.clone() }
    }

    /// Merges an export into the list: devices with the same id are replaced, others added;
    /// groups are added unless one with the same name exists. Returns (added, updated, groups added).
    pub fn import(&mut self, contents: crate::backup::Contents) -> Result<(usize, usize, usize), String> {
        let mut groups_added = 0;
        for g in contents.groups {
            let name = clean_name(&g.name);
            if name.is_empty() || self.groups.iter().any(|x| x.name.to_lowercase() == name.to_lowercase()) {
                continue;
            }
            let id = if self.groups.iter().any(|x| x.id == g.id) { uuid::Uuid::new_v4().to_string() } else { g.id };
            self.groups.push(Group { id, name });
            groups_added += 1;
        }
        let (mut added, mut updated) = (0, 0);
        for mut d in contents.devices {
            if d.id.is_empty() || normalize_url(&d.url, d.kind).is_err() {
                continue;
            }
            d.keychain = false;
            // Keep group references valid: a standard group by name (any case), or none.
            if d.kind == DeviceKind::Proxmox {
                d.group.clear();
                if self.name_taken(&d.name, Some(&d.id)) {
                    d.name = format!("{} (imported)", d.name);
                }
            } else {
                d.group = self
                    .groups
                    .iter()
                    .find(|g| g.name.to_lowercase() == d.group.to_lowercase())
                    .map(|g| g.name.clone())
                    .unwrap_or_default();
            }
            Self::store_password(self.secrets.as_ref(), &mut d);
            match self.devices.iter().position(|x| x.id == d.id) {
                Some(i) => {
                    self.devices[i] = d;
                    updated += 1;
                }
                None => {
                    self.devices.push(d);
                    added += 1;
                }
            }
        }
        self.save().map_err(|e| format!("Could not save devices: {e}"))?;
        self.save_groups().map_err(|e| format!("Could not save groups: {e}"))?;
        Ok((added, updated, groups_added))
    }

    /// Pins a server certificate on first contact (no-op if one is pinned already).
    pub fn pin_fingerprint(&mut self, id: &str, fingerprint: &str) {
        if let Some(d) = self.devices.iter_mut().find(|d| d.id == id) {
            if d.tls_fingerprint.is_empty() {
                d.tls_fingerprint = fingerprint.to_string();
                let _ = self.save();
            }
        }
    }

    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        if self.devices.iter().any(|d| d.id == id && d.keychain) {
            if let Err(e) = self.secrets.delete(id) {
                eprintln!("[secrets] could not remove a deleted device's password from the keychain: {e}");
            }
        }
        self.devices.retain(|d| d.id != id);
        self.save().map_err(|e| format!("Could not save devices: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_url, Device, DeviceInput, DeviceKind, GroupInput, Store};
    use crate::secrets::{Memory, Secrets};
    use std::sync::Arc;

    fn mem() -> Arc<dyn Secrets> {
        Arc::new(Memory::new(true))
    }

    #[test]
    fn normalizes_addresses() {
        let n = DeviceKind::Nanokvm;
        assert_eq!(normalize_url("192.168.1.50", n).unwrap(), "http://192.168.1.50");
        assert_eq!(normalize_url(" kvm.lan:8080/foo ", n).unwrap(), "http://kvm.lan:8080");
        assert_eq!(normalize_url("https://kvm.lan/", n).unwrap(), "https://kvm.lan");
        assert!(normalize_url("ftp://kvm.lan", n).is_err());
        assert!(normalize_url("", n).is_err());
        let s = DeviceKind::SupermicroIkvm;
        assert_eq!(normalize_url("192.0.2.240", s).unwrap(), "https://192.0.2.240");
        assert_eq!(normalize_url("http://192.0.2.240", s).unwrap(), "http://192.0.2.240");
        let p = DeviceKind::Proxmox;
        assert_eq!(normalize_url("192.0.2.5", p).unwrap(), "https://192.0.2.5:8006");
        assert_eq!(normalize_url("pve.example:8443", p).unwrap(), "https://pve.example:8443");
        assert_eq!(normalize_url("https://pve.example", p).unwrap(), "https://pve.example");
        assert_eq!(normalize_url("[2001:db8::1]", p).unwrap(), "https://[2001:db8::1]:8006");
        let v = DeviceKind::Vnc;
        assert_eq!(normalize_url("192.0.2.7", v).unwrap(), "vnc://192.0.2.7:5900");
        assert_eq!(normalize_url("vnc://pc.example:5901/", v).unwrap(), "vnc://pc.example:5901");
        assert_eq!(normalize_url("[2001:db8::2]:5902", v).unwrap(), "vnc://[2001:db8::2]:5902");
        assert_eq!(normalize_url("192.0.2.7:80", v).unwrap(), "vnc://192.0.2.7:80");
        assert!(normalize_url("http://192.0.2.7", v).is_err());
        assert_eq!(normalize_url("192.0.2.8", DeviceKind::Idrac).unwrap(), "https://192.0.2.8");
        assert_eq!(normalize_url("pikvm.example", DeviceKind::Pikvm).unwrap(), "https://pikvm.example");
    }

    #[test]
    fn old_device_files_load_as_nanokvm() {
        let d: Device = serde_json::from_str(r#"{"id":"a","name":"x","url":"http://1.2.3.4"}"#).unwrap();
        assert_eq!(d.kind, DeviceKind::Nanokvm);
        assert_eq!(d.group, "");
        let d: Device =
            serde_json::from_str(r#"{"id":"a","name":"x","kind":"supermicro-ikvm","url":"https://1.2.3.4"}"#).unwrap();
        assert_eq!(d.kind, DeviceKind::SupermicroIkvm);
    }

    #[test]
    fn saves_and_reloads_groups() {
        let dir = std::env::temp_dir().join(format!("nkc-store-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("devices.json");
        let secrets = mem();
        let mut store = Store::load(path.clone(), secrets.clone());
        let input = |group: &str| DeviceInput {
            id: None,
            name: "x".into(),
            kind: DeviceKind::Nanokvm,
            group: group.into(),
            url: "1.2.3.4".into(),
            username: String::new(),
            password: None,
            clear_password: false,
            auto_login: false,
            forget_fingerprint: false,
        };
        assert!(store.upsert(input("Rack A")).is_err(), "unknown groups are refused");
        let rack = store.upsert_group(GroupInput { id: None, name: "  Rack   A ".into() }).unwrap();
        assert_eq!(rack.name, "Rack A");
        assert!(store.upsert_group(GroupInput { id: None, name: "rack a".into() }).is_err(), "names are unique");
        assert_eq!(store.upsert(input("Rack A")).unwrap().group, "Rack A");
        assert_eq!(store.upsert(input("")).unwrap().group, "");
        let groups: Vec<_> = Store::load(path.clone(), secrets.clone()).views().into_iter().map(|v| v.group).collect();
        assert_eq!(groups, ["Rack A", ""]);

        // Renaming moves the devices along; deleting leaves them ungrouped.
        store.upsert_group(GroupInput { id: Some(rack.id.clone()), name: "Rack B".into() }).unwrap();
        assert_eq!(Store::load(path.clone(), secrets.clone()).views()[0].group, "Rack B");
        store.delete_group(&rack.id).unwrap();
        let reloaded = Store::load(path.clone(), secrets.clone());
        assert_eq!(reloaded.views()[0].group, "");
        assert!(reloaded.groups().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn migrates_free_text_groups() {
        let dir = std::env::temp_dir().join(format!("nkc-store-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.json");
        std::fs::write(
            &path,
            r#"[{"id":"a","name":"x","url":"http://192.0.2.1","group":"Office"},
                {"id":"b","name":"PVE","kind":"proxmox","url":"https://192.0.2.2:8006"}]"#,
        )
        .unwrap();
        let secrets = mem();
        let names: Vec<_> = Store::load(path.clone(), secrets.clone()).groups().into_iter().map(|g| g.name).collect();
        assert_eq!(names, ["Office"]);
        assert!(dir.join("groups.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn temp_store_file(json: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("nkc-store-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.json");
        std::fs::write(&path, json).unwrap();
        (dir, path)
    }

    #[test]
    fn moves_plain_text_passwords_to_the_keychain() {
        let (dir, path) = temp_store_file(r#"[{"id":"a","name":"x","url":"http://192.0.2.1","password":"s3cret"}]"#);
        let secrets = mem();
        let store = Store::load(path.clone(), secrets.clone());
        assert_eq!(store.get("a").unwrap().password, "s3cret");
        assert_eq!(secrets.get("a").unwrap().as_deref(), Some("s3cret"));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("s3cret"), "{on_disk}");
        assert!(on_disk.contains("\"keychain\": true"), "{on_disk}");
        // Read back from the keychain on the next start.
        assert_eq!(Store::load(path, secrets).get("a").unwrap().password, "s3cret");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keeps_passwords_in_the_file_without_a_keychain() {
        let (dir, path) = temp_store_file(r#"[{"id":"a","name":"x","url":"http://192.0.2.1","password":"s3cret"}]"#);
        let none: Arc<dyn Secrets> = Arc::new(Memory::new(false));
        let mut store = Store::load(path.clone(), none);
        assert_eq!(store.password_storage(), "file");
        assert!(std::fs::read_to_string(&path).unwrap().contains("s3cret"));
        // New and changed passwords stay in the file as well.
        let input = DeviceInput {
            id: Some("a".into()),
            name: "x".into(),
            kind: DeviceKind::Nanokvm,
            group: String::new(),
            url: "192.0.2.1".into(),
            username: String::new(),
            password: Some("other".into()),
            clear_password: false,
            auto_login: false,
            forget_fingerprint: false,
        };
        store.upsert(input).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("other"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn changing_and_deleting_updates_the_keychain() {
        let (dir, path) = temp_store_file("[]");
        let secrets = mem();
        let mut store = Store::load(path.clone(), secrets.clone());
        let input = |id: Option<String>, password: Option<&str>, clear: bool| DeviceInput {
            id,
            name: "x".into(),
            kind: DeviceKind::Nanokvm,
            group: String::new(),
            url: "192.0.2.1".into(),
            username: "u".into(),
            password: password.map(String::from),
            clear_password: clear,
            auto_login: false,
            forget_fingerprint: false,
        };
        let id = store.upsert(input(None, Some("one"), false)).unwrap().id;
        assert_eq!(secrets.get(&id).unwrap().as_deref(), Some("one"));
        store.upsert(input(Some(id.clone()), Some("two"), false)).unwrap();
        assert_eq!(secrets.get(&id).unwrap().as_deref(), Some("two"));
        store.upsert(input(Some(id.clone()), None, true)).unwrap();
        assert_eq!(secrets.get(&id).unwrap(), None);
        store.upsert(input(Some(id.clone()), Some("three"), false)).unwrap();
        store.delete(&id).unwrap();
        assert_eq!(secrets.get(&id).unwrap(), None);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("three"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exports_and_imports_with_passwords() {
        let (dir, path) = temp_store_file(r#"[{"id":"a","name":"x","url":"http://192.0.2.1","password":"s3cret","group":"Office"}]"#);
        let secrets = mem();
        let store = Store::load(path.clone(), secrets.clone());
        let contents = store.export_contents();
        assert_eq!(contents.devices[0].password, "s3cret");
        assert!(!contents.devices[0].keychain);

        // Import into an empty list elsewhere.
        let (dir2, path2) = temp_store_file("[]");
        let secrets2 = mem();
        let mut other = Store::load(path2.clone(), secrets2.clone());
        assert_eq!(other.import(contents).unwrap(), (1, 0, 1));
        assert_eq!(other.get("a").unwrap().group, "Office");
        assert_eq!(secrets2.get("a").unwrap().as_deref(), Some("s3cret"));
        assert!(!std::fs::read_to_string(&path2).unwrap().contains("s3cret"));
        // Importing the same again updates instead of duplicating.
        assert_eq!(other.import(store.export_contents()).unwrap(), (0, 1, 0));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }
}
