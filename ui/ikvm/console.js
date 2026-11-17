'use strict';
// Console page for Supermicro (ATEN) legacy iKVM, built on the vendored noVNC-iKVM core
// (../vendor/novnc-ikvm, see its NOTICE.md). The app injects window.__NKC_IKVM = {port, token, name}
// and runs a local WebSocket relay on that port; the relay logs in to the BMC and fills in the real
// iKVM credentials, so this page only sends placeholders and never sees a password.
(function () {
  const $ = (id) => document.getElementById(id);
  const cfg = window.__NKC_IKVM;
  const canvas = $('canvas');
  const screenEl = $('screen');
  const messageEl = $('message');
  const statusEl = $('status');
  const dot = $('dot');
  const wakeBtn = $('wake');
  const cadBtn = $('cad');
  const viewOnlyBox = $('viewonly');
  const reconnectBtn = $('reconnect');
  const cadDialog = $('cad-confirm');

  let rfb = null;
  let closeReason = ''; // from the relay (e.g. "The BMC rejected the saved username or password")
  let disconnectReason = ''; // from noVNC

  function showDisconnected() {
    const text = closeReason || disconnectReason;
    setStatus('disconnected', text);
    showMessage(text);
    reconnectBtn.hidden = false;
    updateControls();
  }

  function setStatus(state, text) {
    dot.dataset.state = state;
    statusEl.textContent = text;
  }

  function showMessage(text) {
    messageEl.textContent = text || '';
    messageEl.hidden = !text;
    canvas.hidden = !!text;
  }

  const connected = () => rfb && rfb._rfb_connection_state === 'connected';

  function updateControls() {
    const interactive = connected() && !viewOnlyBox.checked;
    wakeBtn.disabled = !interactive;
    cadBtn.disabled = !interactive;
  }

  // Send keyboard/mouse input only when allowed (and not while our own dialog has the keyboard).
  function applyInputMode() {
    if (!rfb) return;
    const on = !viewOnlyBox.checked && !cadDialog.open;
    rfb.set_view_only(!on);
    rfb.get_keyboard().set_focused(on);
    rfb.get_mouse().set_focused(on);
    updateControls();
  }

  // Scale the remote screen to fit the page, keeping its aspect ratio. The ATEN code path resizes
  // the framebuffer without always calling onFBResize, so updates also check for size changes.
  let fitted = '';
  function fit() {
    if (!connected() || !canvas.width || !canvas.height) return;
    const ratio = rfb.get_display().autoscale(screenEl.clientWidth, screenEl.clientHeight, false);
    rfb.get_mouse().set_scale(ratio);
    fitted = `${canvas.width}x${canvas.height}@${screenEl.clientWidth}x${screenEl.clientHeight}`;
  }
  function fitIfChanged() {
    if (`${canvas.width}x${canvas.height}@${screenEl.clientWidth}x${screenEl.clientHeight}` !== fitted) fit();
  }

  function connect() {
    closeReason = '';
    disconnectReason = '';
    reconnectBtn.hidden = true;
    setStatus('connecting', 'Connecting…');
    showMessage('Connecting…');
    rfb = new RFB({
      target: canvas,
      focusContainer: document,
      encrypt: false,
      shared: true, // don't kick out other viewers (e.g. IPMIView)
      local_cursor: false,
      view_only: false,
      wsProtocols: ['binary'],
      onUpdateState: (_rfb, state) => {
        if (state === 'connected') {
          setStatus('connected', 'Connected');
          showMessage('');
          applyInputMode();
          fit();
          canvas.focus();
        } else if (state === 'connecting') {
          setStatus('connecting', 'Connecting…');
        }
        updateControls();
      },
      onDisconnected: (_rfb, reason) => {
        disconnectReason = reason || 'Disconnected';
        showDisconnected();
      },
      onFBResize: () => fit(),
      onFBUComplete: () => fitIfChanged(),
      onDesktopName: (_rfb, name) => {
        if (name) document.title = `${cfg.name} — ${name}`;
      },
    });
    // ATEN login fields; the relay replaces them with the real one-time credentials.
    rfb.connect('127.0.0.1', cfg.port, 'nanokvm-control:placeholder', cfg.token);
  }

  // noVNC only logs the server's close reason; keep it to show the user (e.g. a rejected login).
  const openSocket = Websock.prototype.open;
  Websock.prototype.open = function (...args) {
    const result = openSocket.apply(this, args);
    // noVNC's own close handler runs first and may already have reported the disconnect.
    this._websocket?.addEventListener('close', (e) => {
      if (!e.reason) return;
      closeReason = e.reason;
      if (rfb && rfb._rfb_connection_state === 'disconnected') showDisconnected();
    });
    return result;
  };

  // Buttons shouldn't keep focus: Space/Enter belong to the remote machine.
  document.querySelectorAll('.bar button').forEach((b) => b.addEventListener('mouseup', () => b.blur()));

  wakeBtn.addEventListener('click', () => {
    if (connected()) rfb.sendKey(KeyTable.XK_Shift_L); // press + release
    canvas.focus();
  });

  cadBtn.addEventListener('click', () => {
    if (!connected()) return;
    cadDialog.returnValue = '';
    cadDialog.showModal();
    applyInputMode(); // the dialog needs the keyboard
  });
  cadDialog.addEventListener('close', () => {
    if (cadDialog.returnValue === 'ok' && connected()) rfb.sendCtrlAltDel();
    applyInputMode();
    canvas.focus();
  });

  viewOnlyBox.addEventListener('change', () => {
    applyInputMode();
    viewOnlyBox.blur();
  });

  reconnectBtn.addEventListener('click', () => {
    if (rfb && rfb._rfb_connection_state !== 'disconnected') return;
    connect();
  });

  new ResizeObserver(fit).observe(screenEl);

  if (!cfg) {
    setStatus('disconnected', 'Not started from NanoKVM Control');
    showMessage('This page is opened by NanoKVM Control for Supermicro iKVM devices.');
    updateControls();
    return;
  }
  document.title = `${cfg.name} — iKVM`;
  Util.init_logging('warn');
  connect();
})();
