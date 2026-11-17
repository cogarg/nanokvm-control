//! Device passwords in the operating system's keychain: Keychain on macOS, Credential Manager on
//! Windows, the Secret Service (GNOME Keyring, KWallet, …) on Linux. One entry per device, under
//! a service name that includes the data folder when the app runs with a separate one (debug
//! builds, `NANOKVM_CONTROL_DATA_DIR`), so test runs never touch the installed app's entries.
//!
//! Without a usable keychain, passwords stay in `devices.json` as before; the UI says so.

#[cfg(test)]
use std::{collections::HashMap, sync::Mutex};

pub trait Secrets: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<String>, String>;
    fn set(&self, id: &str, password: &str) -> Result<(), String>;
    fn delete(&self, id: &str) -> Result<(), String>;
    /// Whether the store works at all (checked once, at startup).
    fn available(&self) -> bool;
}

pub struct Keychain {
    service: String,
    available: bool,
}

impl Keychain {
    /// `namespace`: `None` for the installed app's normal data, else something identifying the
    /// separate data folder.
    pub fn new(namespace: Option<&str>) -> Self {
        let service = match namespace {
            None => "io.github.cogarg.nanokvm-control".to_string(),
            Some(ns) => format!("io.github.cogarg.nanokvm-control (data: {ns})"),
        };
        let available = match keyring::Entry::store_status() {
            Ok(()) => {
                // A store can exist yet be unusable (e.g. no Secret Service running): probe it.
                match keyring::Entry::new(&service, "__probe__").map(|e| e.get_password()) {
                    Ok(Ok(_)) | Ok(Err(keyring::Error::NoEntry)) => true,
                    Ok(Err(e)) => {
                        eprintln!("[secrets] keychain not usable: {e}");
                        false
                    }
                    Err(e) => {
                        eprintln!("[secrets] keychain not usable: {e}");
                        false
                    }
                }
            }
            Err(e) => {
                eprintln!("[secrets] no keychain: {e}");
                false
            }
        };
        Keychain { service, available }
    }

    fn entry(&self, id: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&self.service, id).map_err(|e| e.to_string())
    }
}

impl Secrets for Keychain {
    fn get(&self, id: &str) -> Result<Option<String>, String> {
        match self.entry(id)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    fn set(&self, id: &str, password: &str) -> Result<(), String> {
        self.entry(id)?.set_password(password).map_err(|e| e.to_string())
    }
    fn delete(&self, id: &str) -> Result<(), String> {
        match self.entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
    fn available(&self) -> bool {
        self.available
    }
}

/// In-memory store, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct Memory {
    map: Mutex<HashMap<String, String>>,
    pub usable: bool,
}

#[cfg(test)]
impl Memory {
    pub fn new(usable: bool) -> Self {
        Memory { map: Mutex::default(), usable }
    }
}

#[cfg(test)]
impl Secrets for Memory {
    fn get(&self, id: &str) -> Result<Option<String>, String> {
        Ok(self.map.lock().unwrap().get(id).cloned())
    }
    fn set(&self, id: &str, password: &str) -> Result<(), String> {
        self.map.lock().unwrap().insert(id.to_string(), password.to_string());
        Ok(())
    }
    fn delete(&self, id: &str) -> Result<(), String> {
        self.map.lock().unwrap().remove(id);
        Ok(())
    }
    fn available(&self) -> bool {
        self.usable
    }
}
