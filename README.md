# nanokvm-control

Control center for [Sipeed NanoKVM](https://github.com/sipeed/NanoKVM) devices, older
**Supermicro BMCs** (the ones whose remote console is the Java iKVM viewer) and **Proxmox VE**
virtual machines: a small Tauri v2 desktop app that keeps a list of your devices, shows whether each
is reachable, and opens their consoles in tabs or separate windows, logging you in automatically.

## Features

- Add / edit / delete devices (type, name, group, address, credentials). Forms start empty: no
  preselected type and no default usernames or passwords
- **Groups** (**Add group**), of two kinds:
  - **Standard group:** a name. Put devices in it with the device's **Group** menu (*None* or one
    of your standard groups). Rename or delete it from its section; deleting keeps its devices.
  - **Proxmox VE server:** address and login; the group lists that server's VMs and containers
    (see [Proxmox VE](#proxmox-ve)). Devices can't be added to it.
- The list shows ungrouped devices first, then a section per group. **Show** narrows it to one
  group; it always opens on *All devices*
- Device types:
  - **NanoKVM** and **PiKVM**: their own web UI is the console, with optional auto-login
  - **Supermicro BMC (legacy Java iKVM)**: a built-in browser console replaces the Java viewer, no
    Java needed (see [Supermicro BMCs](#supermicro-bmcs-legacy-java-ikvm))
  - **Dell iDRAC 9**: opens its HTML5 virtual console directly (see [Dell iDRAC 9](#dell-idrac-9))
  - **VNC server**: any VNC server, with or without a password (see [VNC servers](#vnc-servers))
- **Self-signed HTTPS certificates** are accepted on first contact and then pinned (see
  [Certificates](#certificates))
- **Find devices:** scans an address range you enter and recognises the supported device types
  (see [Finding devices](#finding-devices))
- **Passwords in the system keychain** (Keychain, Credential Manager, Secret Service), with an
  **encrypted export/import** of the whole list (see [Where data lives](#where-data-lives))
- **Quick connect:** **Ctrl+K** (or the search button in the tab bar) finds a device or VM by name,
  address or group; Enter opens it in a tab, Shift+Enter in a window. It works while the device
  list or tab bar has focus; inside a console all keys go to the remote machine
- **Tiled view:** the tile button in the tab bar shows up to 4 open consoles side by side; clicking
  another tab adds it, each tile can be shown alone or removed
- **Reopen tabs** from the last session at startup (off unless turned on in **Settings**)
- **Compatibility renderer** (Linux, in **Settings**) for graphics drivers that misbehave with WebKit
- Online/offline status, polled every 15 s
- **Connect** opens the device's console in a **tab** of the main window; switch with the tab bar,
  close with × or middle-click
- **Open in new window** (the ↗ button on a device card or tab) pops a console out into its own
  window; Connect on a popped-out device focuses that window instead of opening a duplicate tab
- Auto-login: the app performs the NanoKVM login from inside the console itself, so the session
  cookie is set exactly as if you had logged in by hand
- Consoles (tabs and windows) have **no** access to the app's commands: only the device-manager UI
  may call them (enforced with Tauri capabilities; see `src-tauri/build.rs`)

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
  `sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libxdo-dev libssl-dev build-essential`
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

## Supermicro BMCs (legacy Java iKVM)

Older Supermicro boards (and other boards with an ATEN-made BMC) only offer their remote console
through a Java Web Start viewer. NanoKVM Control shows that console in a tab instead, using
[noVNC-iKVM](ui/vendor/novnc-ikvm/NOTICE.md): noVNC with ATEN iKVM support by Kevin Kelley.

Add the BMC with type **Supermicro BMC (legacy Java iKVM)**, its address and a BMC account
(Supermicro's factory account is `ADMIN`; the form doesn't fill it in). An address without
`http(s)://` is treated as HTTPS.

How a connection works (`src-tauri/src/ikvm.rs`):

1. The console tab loads the bundled page `ui/vnc/` and connects to a WebSocket relay that the app
   runs on `127.0.0.1` for that tab (random port and path), because pages can't open raw TCP
   connections.
2. For each connection the relay logs in to the BMC's web interface, fetches the Java viewer's
   launch file to get a one-time iKVM username and password, and connects to the BMC's iKVM port
   (5900).
3. The relay writes those one-time credentials into the iKVM login itself: the page only sends
   placeholders and never sees any credentials.
4. A few seconds later the relay logs the web session out again (BMCs allow only a few). The
   console keeps running; **Reconnect** simply repeats the whole sequence.

The console has **Wake screen** (presses Shift, for consoles that only redraw after input),
**Ctrl+Alt+Del** (asks first: on many servers it reboots immediately) and **View only**.

Notes:

- **Tested on:** an X10-generation board (Xeon E5 v3, Aspeed AST2400) with BMC firmware 3.24.
  Other ATEN BMCs may or may not work; newer boards (X11 and later) have an HTML5 console and don't
  need this.
- **Encryption:** the BMC web login uses HTTPS and accepts the BMC's self-signed certificate.
  Older firmware only offers TLS 1.2 with CBC ciphers, so these requests use the system TLS
  library (OpenSSL on Linux) rather than rustls. The iKVM video/keyboard stream itself (port 5900)
  is **not encrypted**, same as with the Java viewer, so use it on a trusted network.
- **Not supported:** virtual media (mounting ISOs), and the AST2100 decoder's picture is not quite
  as sharp as Java's at low quality settings (a known limitation of the decoder).
- If the account is wrong, the tab says so and nothing is retried: BMCs lock accounts after
  repeated failed logins.

## Dell iDRAC 9

Add the iDRAC with type **Dell iDRAC 9**, its address (HTTPS) and an iDRAC account with the virtual
console privilege. On each **Connect** the app (`src-tauri/src/idrac.rs`) logs in to the iDRAC web
interface, asks it for the HTML5 virtual console address (which carries one-time console keys),
logs out again right away, and opens that address in the tab. The console is the iDRAC's own viewer,
with its Power, Boot, Keyboard and Virtual Media menus. Failed logins are never retried, since
iDRACs block an address after repeated failures. Tested with iDRAC 9 firmware 7.00; iDRAC 8 and
older use a different interface and aren't supported.

## PiKVM

Type **PiKVM** works like NanoKVM: the console is PiKVM's own web page (`/kvm/`), and auto-login
uses its API (`POST /api/auth/login`). Accounts with two-factor codes have to log in by hand.
*Not yet tested against a real PiKVM.*

## VNC servers

Type **VNC server** takes `host` or `host:port` (port 5900 by default) and an optional password.
The console uses the same noVNC page as the Supermicro and Proxmox consoles; the app's relay
(`src-tauri/src/vnc.rs`) connects to the server, answers the VNC password challenge itself, and
gives the page a connection without a password, so the page never sees it. Supported: no password
and the classic VNC password (RFB 3.3, 3.7, 3.8). Not supported: RealVNC account logins, Apple
Remote Desktop logins, VeNCrypt/TLS. VNC itself is **not encrypted**; use it on trusted networks or
through a tunnel.

## Finding devices

**Find devices** scans only the addresses you enter: a network (`192.168.1.0/24`, at most a /20 =
4096 addresses), a range (`192.168.1.10-192.168.1.60` or `192.168.1.10-60`) or single addresses,
separated by commas. This computer's networks are offered as one-click suggestions; nothing is
scanned until you press **Scan**.

Each address gets a short TCP check on ports 80, 443, 8006, 5900 and 5901 (128 addresses at a
time). Addresses that answer are identified without logging in: the Proxmox page on 8006, the
Redfish service root (Dell iDRAC), the page title (NanoKVM), PiKVM's page, the ATEN login page
(older Supermicro BMCs) and the VNC greeting. Each result has **Add…**, which opens the Add dialog
filled in with what was found (credentials stay empty). Devices already in the list are marked.

## Certificates

Devices with HTTPS often use self-signed certificates (Proxmox, iDRAC, PiKVM, NanoKVM in HTTPS
mode). The app trusts the certificate a device presents on first contact and saves its SHA-256
fingerprint with the device ("trust on first use"). From then on a different certificate is
refused, with a message showing the new fingerprint, both for the app's own requests (logins,
Proxmox API) and inside console tabs. The pinned fingerprint is shown in the device's **Edit** dialog
(and on Proxmox sections); compare it with the one the device shows. After a legitimate certificate
change, use **Edit > Forget certificate**.

In console tabs this uses each web engine's certificate-error hook: WebKitGTK on Linux, WebView2 on
Windows. macOS's WKWebView offers no such hook through Tauri, so on macOS consoles with self-signed
certificates still fail to load (the app's own requests are pinned there too).

## Proxmox VE

Use **Add group** with type **Proxmox VE server**, its address (port 8006 is used unless you give one)
and a Proxmox account as `user@realm`, e.g. `root@pam` (`root` alone means `root@pam`). The list
then shows a section for the server with its VMs and containers (templates are left out), refreshed
every 15 s, and the **Show** menu can narrow the list to that server. **Connect** on a running guest
opens its console in a tab; ↗ opens it in a window.

How it works (`src-tauri/src/proxmox.rs`):

- **Certificate pinning.** Proxmox normally uses a self-signed certificate, so the app trusts the
  certificate it sees on first contact and saves its SHA-256 fingerprint (shown under the server's
  name; compare it with *Node > System > Certificates* in Proxmox). Later connections with a
  different certificate are refused before the password is sent. After a legitimate certificate
  change, use *Edit > Forget certificate*.
- **Login:** username + password for a Proxmox ticket, kept in memory and renewed before it expires.
  Accounts with two-factor authentication aren't supported.
- **Consoles:** the console page (the same noVNC page as for Supermicro) connects to a local relay.
  For each connection the relay requests a console ticket (`vncproxy`), opens Proxmox's
  `vncwebsocket`, answers the VNC password challenge with that ticket, and gives the page a
  connection that needs no password. The page never sees the password or any ticket.
- **Read-only:** the app only lists guests and opens consoles. It never starts, stops or changes
  anything; start a stopped guest in Proxmox to use its console. An account (or custom role) with
  just the `VM.Audit` and `VM.Console` privileges is enough.

## Where data lives

`devices.json` (devices and Proxmox servers), `groups.json` (standard groups) and `settings.json`
in the app config directory:

- Linux: `~/.config/io.github.cogarg.nanokvm-control/` (file mode `0600`)
- macOS: `~/Library/Application Support/io.github.cogarg.nanokvm-control/` (file mode `0600`)
- Windows: `%APPDATA%\io.github.cogarg.nanokvm-control\` (protected by your user profile's permissions)

**Passwords** are kept in the operating system's keychain: Keychain on macOS, Credential Manager
on Windows, the Secret Service (GNOME Keyring, KWallet, …) on Linux, one entry per device under
`io.github.cogarg.nanokvm-control`. Passwords from older versions are moved there automatically on
the first start. If no keychain is available (e.g. a Linux system without a Secret Service), they
stay in `devices.json` as before, readable only by your user, and **Settings** says so. Separate data
folders (below) use separate keychain entries.

**Export / import** (in **Settings**): exports all devices, groups and passwords to a `.nkcbackup`
file encrypted with a passphrase (Argon2id key derivation, AES-256-GCM); importing merges such a
file into the list (devices with the same id are updated, others added).

### Separate data for development and testing

Development builds never touch the installed app's data:

- **Debug builds** (`cargo run`, `cargo build`) keep the device list *and* webview data (cookies,
  sessions) in a separate folder next to the normal one, e.g.
  `~/.config/io.github.cogarg.nanokvm-control.dev/`.
- **`NANOKVM_CONTROL_DATA_DIR=/some/dir`** puts all of it under that folder instead, in any build,
  which is handy for throwaway test runs:
  `NANOKVM_CONTROL_DATA_DIR=$(mktemp -d) cargo run`

Whenever a non-default location is in use, the window title shows it. (On macOS only the device
list is relocated; WebKit there keeps one shared website data store.)

## Layout

```
ui/                    device-manager frontend (index.html, style.css, app.js)
src-tauri/src/main.rs  Tauri commands, console tabs (multi-webview) and windows, auto-login script
src-tauri/src/stream_watchdog.js  injected into consoles: restarts the MJPEG stream if no picture arrives
src-tauri/src/store.rs device model + JSON persistence + address normalization
src-tauri/src/ikvm.rs  Supermicro iKVM: BMC web login, launch-file credentials, WebSocket relay
src-tauri/build.rs     declares the app's commands, so capabilities can limit them to the UI
src-tauri/src/proxmox.rs  Proxmox VE: certificate pinning, login, guest list, console relay
src-tauri/src/relay.rs    local WebSocket relay shared by the Supermicro and Proxmox consoles
src-tauri/src/secrets.rs  passwords in the system keychain
src-tauri/src/backup.rs   encrypted export/import file format
src-tauri/src/discovery.rs  "Find devices": address range parsing, scanning, recognising devices
src-tauri/src/vnc.rs      VNC login (RFB 3.3/3.7/3.8), page greeting, relay for generic VNC servers
src-tauri/src/idrac.rs    Dell iDRAC 9: login, virtual console address, logout
src-tauri/src/tls.rs      certificate pinning ("trust on first use") for the app's own HTTPS requests
src-tauri/src/console_tls.rs  certificate pinning inside console webviews (WebKitGTK, WebView2)
ui/vnc/                console page for Supermicro iKVM, Proxmox guests and VNC servers (uses ui/vendor/novnc-ikvm)
ui/vendor/novnc-ikvm/  third-party noVNC-iKVM client (MPL-2.0), see its NOTICE.md
src-tauri/src/crypto.rs NanoKVM password encoding (CryptoJS-compatible AES, as the web UI does)
src-tauri/Info.plist   macOS: allow plain-HTTP consoles (ATS) + Local Network prompt text
```

## Known limitations

- **Self-signed HTTPS in console tabs on macOS** isn't supported (see [Certificates](#certificates)).
  On Linux and Windows the certificate is pinned on first contact. Within one app session, a
  certificate accepted for a host applies to every console of that host.
- **Video (Linux):** the console uses the system WebKitGTK. If H.264/WebRTC mode doesn't play, switch the
  NanoKVM to MJPEG mode in its settings. If the app crashes after running for a while, or windows
  stay blank or flicker (seen with some NVIDIA and Mesa drivers), turn on **Settings → Use the
  compatibility renderer** and restart the app. It starts WebKit with
  `WEBKIT_DISABLE_DMABUF_RENDERER=1`; setting that variable yourself works too and takes precedence.
- **Black console, working mouse (WebKit, MJPEG mode):** WebKit learns that the console page loads
  the never-ending `/api/stream/mjpeg` and starts fetching it speculatively on later visits; the
  page's own request then waits forever, and reloading doesn't help. The app turns off WebKitGTK's
  disk cache (Linux), and `stream_watchdog.js` re-requests the stream under a fresh URL if no
  picture shows within 4 s (all platforms). The same symptom in a regular browser has other causes.
- **macOS shortcuts:** Cmd+Q / Cmd+W go to the app (quit / close the console window), not the remote machine.
- **Console tabs use Tauri's `unstable` multi-webview API.** On Windows/macOS each tab is a native
  webview positioned under the tab bar; on Linux, where WebKitGTK can't position child webviews,
  the window's webviews are rearranged into a `GtkOverlay` (see `gtk_overlay` in `main.rs`).
  `Cargo.lock` pins Tauri; re-test tabs after upgrading it, as this API may change.

## Credits & licenses

NanoKVM Control is MIT-licensed (see `LICENSE`). It builds on the work of others:

**Bundled third-party code**

- **[noVNC](https://github.com/novnc/noVNC)** by Joel Martin and the noVNC authors:
  Mozilla Public License 2.0.
- **ATEN iKVM support for noVNC** (ATEN login, keyboard/mouse messages, AST2100 video decoder) by
  **Kevin Kelley**, from the `bmc-support` branch of
  [kelleyk/noVNC](https://github.com/kelleyk/noVNC). Its ATEN handling credits
  [chicken-aten-ikvm](https://github.com/thefloweringash/chicken-aten-ikvm) by thefloweringash.
- **[pako](https://github.com/nodeca/pako)** by Vitaly Puzrin (inside noVNC's `inflator.js`): MIT.
- noVNC's `des.js` carries notices from AT&T Laboratories Cambridge and Widget Workshop.

These live in `ui/vendor/novnc-ikvm/` with their license texts and a
[NOTICE.md](ui/vendor/novnc-ikvm/NOTICE.md) describing the exact source, the one change made for
this app, and a note on the AST2100 decoder files (they carry a copyright notice but no license
line of their own and are used under the repository's default MPL-2.0).

**Libraries** (compiled in, not modified)

| Library | License |
|---|---|
| [Tauri](https://github.com/tauri-apps/tauri), wry, tao | Apache-2.0 / MIT |
| [webkit2gtk-rs](https://github.com/tauri-apps/webkit2gtk-rs), [gtk-rs](https://github.com/gtk-rs/gtk3-rs) (Linux) | MIT |
| [reqwest](https://github.com/seanmonstar/reqwest), [hyper](https://github.com/hyperium/hyper) | MIT / Apache-2.0, MIT |
| [rustls](https://github.com/rustls/rustls), [ring](https://github.com/briansmith/ring), [rustls-native-certs](https://github.com/rustls/rustls-native-certs) | Apache-2.0 / ISC / MIT; ring: Apache-2.0 AND ISC |
| [rust-native-tls](https://github.com/sfackler/rust-native-tls), [rust-openssl](https://github.com/sfackler/rust-openssl) (with the system's [OpenSSL](https://www.openssl.org/) on Linux) | MIT / Apache-2.0, Apache-2.0 |
| [tokio](https://github.com/tokio-rs/tokio), [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite) | MIT |
| [webview2-com](https://github.com/wravery/webview2-rs) (Windows) | MIT |
| [windows-rs](https://github.com/microsoft/windows-rs) `windows-core` (Windows) | MIT / Apache-2.0 |
| [futures-rs](https://github.com/rust-lang/futures-rs) | MIT / Apache-2.0 |
| [serde](https://github.com/serde-rs/serde), [serde_json](https://github.com/serde-rs/json) | MIT / Apache-2.0 |
| [RustCrypto](https://github.com/RustCrypto) `aes`, `cbc`, `md-5`, `des`, `sha2` | MIT / Apache-2.0 |
| [rand](https://github.com/rust-random/rand), [uuid](https://github.com/uuid-rs/uuid), [base64](https://github.com/marshallpierce/rust-base64) | MIT / Apache-2.0 |
| [urlencoding](https://github.com/kornelski/rust_urlencoding) | MIT |
| [keyring](https://github.com/open-source-cooperative/keyring-rs) | MIT / Apache-2.0 |
| [RustCrypto](https://github.com/RustCrypto) `argon2`, `aes-gcm` | MIT / Apache-2.0 |
| [tauri-plugin-dialog](https://github.com/tauri-apps/plugins-workspace) | MIT / Apache-2.0 |
| [if-addrs](https://github.com/messense/if-addrs) | MIT / BSD-3-Clause |

The full dependency tree and each crate's license can be listed with
`cargo metadata` or [`cargo-about`](https://github.com/EmbarkStudios/cargo-about).

**Protocols and devices**

- [Sipeed NanoKVM](https://github.com/sipeed/NanoKVM): the login protocol (including its
  CryptoJS-style password encryption) follows the NanoKVM web UI's own code.
- Supermicro/ATEN iKVM: reverse-engineered by the projects above; Supermicro and ATEN aren't
  affiliated with this app.
- [PiKVM](https://pikvm.org/): auto-login uses its documented
  [API](https://docs.pikvm.org/api/).
- Dell iDRAC: the virtual console is Dell's own HTML5 viewer, opened the way the iDRAC web interface
  opens it. Dell isn't affiliated with this app; iDRAC is a Dell trademark.
- [Proxmox VE](https://www.proxmox.com/en/proxmox-virtual-environment): uses its documented
  [HTTP API](https://pve.proxmox.com/wiki/Proxmox_VE_API). Proxmox Server Solutions GmbH isn't
  affiliated with this app; Proxmox is their trademark.
