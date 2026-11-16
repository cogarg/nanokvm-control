# nanokvm-control

Control center for [Sipeed NanoKVM](https://github.com/sipeed/NanoKVM) devices: a small Tauri v2
desktop app that keeps a list of your NanoKVMs, shows whether each is reachable, and opens each
device's console in its own native window, optionally logging you in automatically.

## Features

- Add / edit / delete devices (name, address, optional credentials)
- Online/offline status, polled every 15 s
- **Connect** opens the device's web console in a dedicated window (one per device; reconnecting focuses it)
- Auto-login: the app performs the NanoKVM login from inside the console window, so the session cookie is set exactly as if you had logged in by hand
- Console windows run the remote page with **no** access to the app's IPC

## Platforms

Runs on **Linux, macOS and Windows**. Each OS uses its own web engine for the console windows:

| OS      | Web engine            | Notes |
|---------|-----------------------|-------|
| Windows | WebView2 (Chromium)   | Best H.264/WebRTC support; WebView2 ships with Windows 10/11 (installer bootstraps it otherwise) |
| macOS   | WKWebView (Safari)    | Good H.264 support; macOS asks for **Local Network** access on first use, and devices show offline until you allow it |
| Linux   | WebKitGTK 4.1         | If H.264/WebRTC mode doesn't play, use MJPEG mode on the NanoKVM |

## Build requirements

- **All:** Rust (stable). No Node.js code is involved: the UI in `ui/` is plain HTML/CSS/JS.
- **Linux (Arch/CachyOS):**
  `sudo pacman -S --needed webkit2gtk-4.1 base-devel curl wget file openssl appmenu-gtk-module libappindicator-gtk3 librsvg`
- **Linux (Debian/Ubuntu):**
  `sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libxdo-dev build-essential`
- **macOS:** Xcode Command Line Tools (`xcode-select --install`).
- **Windows:** Visual Studio Build Tools with the "Desktop development with C++" workload.

## Run / build

```bash
cd src-tauri
cargo run              # development
cargo test             # unit tests
cargo build --release  # bare binary in src-tauri/target/release/
```

Installers are built with the Tauri CLI (on the OS you are targeting; Tauri doesn't cross-compile):

```bash
npx @tauri-apps/cli@^2 build          # or: cargo install tauri-cli --version "^2" && cargo tauri build
# macOS universal (Apple Silicon + Intel):
rustup target add aarch64-apple-darwin x86_64-apple-darwin
npx @tauri-apps/cli@^2 build --target universal-apple-darwin
```

Output goes to `src-tauri/target/[<target>/]release/bundle/`: `.deb`/`.rpm`/`.AppImage` on Linux,
`.dmg`/`.app` on macOS, `.msi` and NSIS `-setup.exe` on Windows.

### CI

`.github/workflows/build.yml` runs the tests and builds installers for all three OSes on every push
and pull request (download them from the run's *Artifacts*). Pushing a tag like `v0.1.0` also
creates a **draft GitHub release** with all installers attached:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

Builds are **unsigned**: macOS Gatekeeper will need right-click -> Open the first time (or
`xattr -dr com.apple.quarantine "/Applications/NanoKVM Control.app"`), and Windows SmartScreen
will show "unknown publisher". Signing needs an Apple Developer ID / Windows code-signing certificate.

## Where data lives

`devices.json` in the app config directory:

- Linux: `~/.config/io.github.cogarg.nanokvm-control/` (file mode `0600`)
- macOS: `~/Library/Application Support/io.github.cogarg.nanokvm-control/` (file mode `0600`)
- Windows: `%APPDATA%\io.github.cogarg.nanokvm-control\` (protected by your user profile's permissions)

**Passwords are stored in plain text in that file**, so treat it like an SSH config.

## Layout

```
ui/                    device-manager frontend (index.html, style.css, app.js)
src-tauri/src/main.rs  Tauri commands: list/save/delete/check/connect, console windows, auto-login script
src-tauri/src/store.rs device model + JSON persistence + address normalization
src-tauri/src/crypto.rs NanoKVM password encoding (CryptoJS-compatible AES, as the web UI does)
src-tauri/Info.plist   macOS: allow plain-HTTP consoles (ATS) + Local Network prompt text
```

## Known limitations

- **HTTPS with self-signed certs:** status checks accept them, but the console window refuses the
  certificate on every OS. Use HTTP on a trusted LAN or give the device a trusted certificate.
- **Video (Linux):** the console uses the system WebKitGTK. If H.264/WebRTC mode doesn't play, switch the
  NanoKVM to MJPEG mode in its settings. On NVIDIA, a blank window may need
  `WEBKIT_DISABLE_DMABUF_RENDERER=1`.
- **macOS shortcuts:** Cmd+Q / Cmd+W go to the app (quit / close the console window), not the remote machine.
