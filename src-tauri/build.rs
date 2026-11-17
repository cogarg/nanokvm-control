// Declaring the app's commands here turns each into a permission (`allow-<command>`). Tauri then
// only lets a webview call them if a capability grants that permission to it (see
// capabilities/default.json, which grants them to the device-manager UI only). Without this, every
// page served from the app itself could call them, including the iKVM console page, which runs
// third-party code fed by the BMC.
const COMMANDS: &[&str] = &[
    "list_devices",
    "get_settings",
    "save_settings",
    "export_devices",
    "import_devices",
    "local_networks",
    "discover",
    "list_groups",
    "save_group",
    "delete_group",
    "save_device",
    "delete_device",
    "check_device",
    "list_guests",
    "open_tab",
    "show_tab",
    "close_tab",
    "open_window",
    "set_layout",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
