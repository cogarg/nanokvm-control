# noVNC-iKVM (vendored)

The files in this directory are the browser-side VNC client used for **Supermicro (ATEN) legacy
iKVM** consoles. They are third-party code, not covered by NanoKVM Control's MIT license.

## Origin

- **noVNC**, the HTML5 VNC client by Joel Martin and the noVNC authors:
  <https://github.com/novnc/noVNC>
- **ATEN iKVM support for noVNC** (ATEN login, keyboard/mouse messages and the AST2100 video
  decoder in `core/ast2100/`) by **Kevin Kelley**, from the `bmc-support` branch of
  <https://github.com/kelleyk/noVNC>, commit `eb386b7672b1cef4ca02c9da769d5d08beefd0ab`
  (2017-01-12). Its ATEN handling credits
  [chicken-aten-ikvm](https://github.com/thefloweringash/chicken-aten-ikvm) by thefloweringash,
  a reverse-engineered ATEN iKVM client (see the comments in `core/rfb.js`).

Only the `core/` directory is included (the files the console page loads), plus the license texts.
`core/inflator.mod.js` (a module build not used here) was left out.

## Licenses

See `LICENSE.txt` (from the upstream repository) for the full picture. In short:

- noVNC core files: **Mozilla Public License 2.0** (`docs/LICENSE.MPL-2.0`).
- `core/inflator.js`: contains **pako** (Copyright (C) 2014 Vitaly Puzrin), **MIT** license
  (`docs/LICENSE.pako`).
- `core/des.js`: permissive notices from AT&T Laboratories Cambridge and Widget Workshop, kept in the
  file itself.
- `core/ast2100/*`: carry Kevin Kelley's copyright notice but no license line of their own. They
  are published in that repository, whose default license is MPL-2.0 (see `LICENSE.txt`), and are
  used here on that basis.

## Changes made for NanoKVM Control

One change, in `core/rfb.js` (`_negotiate_aten_auth`), also marked in the file:

```diff
-            this._sock.rQskipBytes(16);
+            this._sock.rQskipBytes(this._sock.rQlen() >= 20 ? 20 : 16);
```

Supermicro X10 BMCs (Aspeed AST2400, firmware 3.x) send 24 bytes after the client picks security
type 16, where this code expected 20, so the login result was read from the wrong place
("Unknown SecurityResult"). Tested against an X10 board with BMC firmware 3.24.

As MPL-2.0 requires, the source of these files (including the change) is this directory.
