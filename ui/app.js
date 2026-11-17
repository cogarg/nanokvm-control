'use strict';

const { invoke } = window.__TAURI__.core;

const $ = (sel) => document.querySelector(sel);
const listEl = $('#devices');
const emptyEl = $('#empty');
const errorEl = $('#error');
const homeEl = $('#home');
const placeholderEl = $('#placeholder');
const tabsEl = $('#tabs');
const homeTab = $('#home-tab');
const dialog = $('#dialog');
const form = $('#form');
const fields = form.elements;
const formError = $('#form-error');
const passwordHint = $('#password-hint');
const cardTpl = $('#card');
const tabTpl = $('#tab');
const confirmDlg = $('#confirm');
const filterLabel = $('#filter');
const groupFilter = $('#group-filter');
const groupsEl = $('#groups');
const groupTpl = $('#group-section');
const groupDialog = $('#group-dialog');
const groupForm = $('#group-form');
const gfields = groupForm.elements;
const groupFormError = $('#group-form-error');
const pveEl = $('#pve');
const pveTpl = $('#pve-section');
const guestTpl = $('#guest-card');

const POLL_MS = 15000;
let devices = [];
const status = new Map(); // id -> 'online' | 'offline' | 'unknown'
let editingId = null;
// What the list shows: '' = everything, 'group:<id>' = one standard group, 'pve:<id>' = one Proxmox
// server's guests. Starts as everything on every launch.
let shownGroup = '';
let clearPassword = false;
let forgetFingerprint = false;
let groups = []; // standard groups [{id, name}]; Proxmox servers are groups too (devices of kind proxmox)
let editingGroup = null; // {type: 'standard'|'proxmox', item} while the group dialog edits one
// Proxmox guests per server id: { list: [guest], error: '' }. Guests aren't stored; they're
// fetched from each server when the list loads and every poll.
const guests = new Map();

// Console tabs, in display order. The native webviews live in Rust; this is the UI's view of them.
let tabs = []; // [id]
let active = null; // device id of the visible tab, or null for the device list

const deviceById = (id) => devices.find((d) => d.id === id);
const statusTitle = { online: 'Online', offline: 'Unreachable', unknown: 'Checking…', stopped: 'Stopped' };
const isServer = (d) => d.kind === 'proxmox';

// Guest console ids look like "pve:<server id>:<node>:<qemu|lxc>:<vmid>" (see main.rs).
function parseGuestId(id) {
  const [prefix, server, node, kind, vmid] = id.split(':');
  return prefix === 'pve' && vmid ? { server, node, kind, vmid } : null;
}
const guestById = (id) => {
  const p = parseGuestId(id);
  return p ? guests.get(p.server)?.list.find((g) => g.id === id) : undefined;
};
const guestLabel = (g) => `${g.kind === 'lxc' ? 'CT' : 'VM'} ${g.vmid}`;

// Name and tooltip for a console (device or Proxmox guest); null if it no longer exists.
function consoleInfo(id) {
  const d = deviceById(id);
  if (d) return { name: d.name, title: `${d.name} — ${d.url}` };
  const p = parseGuestId(id);
  const server = p && deviceById(p.server);
  if (!server) return null;
  const g = guestById(id);
  const label = guestLabel(p);
  return { name: g ? g.name : label, title: `${g ? `${g.name} — ` : ''}${label} on ${server.name} (node ${p.node})` };
}

// Dot status of a device, server or guest.
function statusOf(id) {
  const p = parseGuestId(id);
  if (p) {
    const g = guestById(id);
    if (!g) return 'unknown';
    return g.status === 'running' ? 'online' : 'stopped';
  }
  return status.get(id) || 'unknown';
}

function showError(el, msg) {
  el.textContent = msg || '';
  el.hidden = !msg;
}

// ---------------------------------------------------------------- device list

// The "Show" dropdown: everything, one standard group, or one Proxmox server. Hidden while there
// are no groups.
function renderGroupFilter() {
  const servers = devices.filter(isServer);
  const values = [...groups.map((g) => `group:${g.id}`), ...servers.map((d) => `pve:${d.id}`)];
  if (shownGroup && !values.includes(shownGroup)) shownGroup = ''; // that group is gone
  groupFilter.replaceChildren(
    new Option('All devices', ''),
    ...groups.map((g) => new Option(g.name, `group:${g.id}`)),
    ...servers.map((d) => new Option(`${d.name} (Proxmox)`, `pve:${d.id}`))
  );
  groupFilter.value = shownGroup;
  filterLabel.hidden = values.length === 0;
}

const KIND_LABEL = {
  nanokvm: 'NanoKVM',
  pikvm: 'PiKVM',
  'supermicro-ikvm': 'Supermicro iKVM',
  idrac: 'iDRAC',
  vnc: 'VNC',
};

function deviceCard(d) {
  const node = cardTpl.content.firstElementChild.cloneNode(true);
  node.dataset.id = d.id;
  node.querySelector('.name').textContent = d.name;
  node.querySelector('.name').title = d.name;
  node.querySelector('.url').textContent = d.url;
  if (d.tlsFingerprint) node.querySelector('.url').title = `Pinned certificate SHA-256 ${d.tlsFingerprint}`;
  const meta = node.querySelector('.meta');
  const label = KIND_LABEL[d.kind] || d.kind;
  const warn = (text) => {
    meta.textContent = `${label}: ${text}`;
    meta.classList.add('warn');
  };
  if (d.kind === 'supermicro-ikvm' || d.kind === 'idrac') {
    if (d.username && d.hasPassword) meta.textContent = `${label} · as ${d.username}`;
    else warn(`the ${d.username ? 'password' : 'username'} is missing`);
  } else if (d.kind === 'vnc') {
    meta.textContent = `${label} · ${d.hasPassword ? 'password saved' : 'no password'}`;
  } else if (!d.autoLogin) meta.textContent = `${label} · manual login`;
  else if (d.username && d.hasPassword) meta.textContent = `${label} · auto-login as ${d.username}`;
  else warn(`auto-login is on, but the ${d.username ? 'password' : 'username'} is missing`);
  return node;
}

// Ungrouped devices first, then one section per standard group, then the Proxmox servers.
function render() {
  renderGroupFilter();
  emptyEl.hidden = devices.length > 0 || groups.length > 0;
  const hosts = devices.filter((d) => !isServer(d));
  const ungrouped = shownGroup ? [] : hosts.filter((d) => !d.group);
  listEl.replaceChildren(...ungrouped.map(deviceCard));
  listEl.hidden = ungrouped.length === 0;
  const shownGroups = groups.filter((g) => !shownGroup || shownGroup === `group:${g.id}`);
  groupsEl.replaceChildren(
    ...shownGroups.map((g) => {
      const sec = groupTpl.content.firstElementChild.cloneNode(true);
      sec.dataset.group = g.id;
      sec.querySelector('.name').textContent = g.name;
      const members = hosts.filter((d) => d.group === g.name);
      sec.querySelector('.pve-sub').textContent = `${members.length} device${members.length === 1 ? '' : 's'}`;
      sec.querySelector('.group-empty').hidden = members.length > 0;
      sec.querySelector('.grid').replaceChildren(...members.map(deviceCard));
      return sec;
    })
  );
  renderServers();
  renderStatus();
}

// One section per Proxmox server, with its VMs and containers.
function renderServers() {
  const servers = devices
    .filter(isServer)
    .filter((d) => !shownGroup || shownGroup === `pve:${d.id}`);
  pveEl.replaceChildren(
    ...servers.map((d) => {
      const sec = pveTpl.content.firstElementChild.cloneNode(true);
      sec.dataset.server = d.id;
      sec.querySelector('.name').textContent = d.name;
      const info = guests.get(d.id);
      const list = info?.list || [];
      const vms = list.filter((g) => g.kind !== 'lxc').length;
      const cts = list.length - vms;
      const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
      const counts = [vms || !cts ? plural(vms, 'VM') : '', cts ? plural(cts, 'container') : '']
        .filter(Boolean)
        .join(', ');
      sec.querySelector('.pve-sub').textContent = `${d.url}${info && !info.error ? ` · ${counts}` : ''}`;
      sec.querySelector('.pve-cert').textContent = d.tlsFingerprint
        ? `Certificate SHA-256 ${d.tlsFingerprint} (pinned)`
        : 'Certificate not seen yet';
      const missing = !d.username || !d.hasPassword;
      const err = missing ? 'Add the Proxmox username and password with Edit.' : info?.error || '';
      showError(sec.querySelector('.pve-error'), err);
      const dot = sec.querySelector('.dot');
      dot.dataset.status = !info ? 'unknown' : info.error || missing ? 'offline' : 'online';
      dot.title = !info ? 'Checking…' : info.error ? 'Not available' : 'Connected';
      sec.querySelector('.guests').replaceChildren(...list.map(guestCard));
      return sec;
    })
  );
}

function guestCard(g) {
  const node = guestTpl.content.firstElementChild.cloneNode(true);
  node.dataset.id = g.id;
  node.querySelector('.name').textContent = g.name;
  node.querySelector('.name').title = g.name;
  node.querySelector('.kind-tag').textContent = g.kind === 'lxc' ? 'CT' : 'VM';
  node.querySelector('.url').textContent = `${guestLabel(g)} · node ${g.node}`;
  const running = g.status === 'running';
  node.querySelector('.meta').textContent = running
    ? 'Running'
    : `${g.status ? g.status[0].toUpperCase() + g.status.slice(1) : 'Unknown state'}: start it in Proxmox to use its console`;
  for (const b of node.querySelectorAll('[data-action]')) {
    b.disabled = !running;
    if (!running) b.title = 'Only running guests have a console';
  }
  return node;
}

function renderStatus() {
  document.querySelectorAll('[data-id]').forEach((el) => {
    const dot = el.querySelector('.dot');
    if (!dot) return;
    const s = statusOf(el.dataset.id);
    dot.dataset.status = s;
    dot.title = statusTitle[s];
  });
}

// Fetches one Proxmox server's guests. Re-renders only when something changed.
async function loadGuests(id) {
  let next;
  try {
    next = { list: await invoke('list_guests', { id }), error: '' };
  } catch (e) {
    next = { list: guests.get(id)?.list || [], error: String(e) };
  }
  const before = JSON.stringify(guests.get(id) || null);
  guests.set(id, next);
  // The first successful contact pins the server's certificate; show it.
  const d = deviceById(id);
  if (d && !d.tlsFingerprint && !next.error) {
    try {
      devices = (await invoke('list_devices')).sort((a, b) => a.name.localeCompare(b.name));
    } catch {}
  }
  if (JSON.stringify(next) !== before || (d && !d.tlsFingerprint)) {
    render();
    renderTabs();
  }
}

const loadAllGuests = () => Promise.all(devices.filter(isServer).map((d) => loadGuests(d.id)));

async function load() {
  try {
    [devices, groups] = await Promise.all([invoke('list_devices'), invoke('list_groups')]);
    devices.sort((a, b) => a.name.localeCompare(b.name));
    showError(errorEl, '');
  } catch (e) {
    showError(errorEl, String(e));
  }
  // Drop tabs for consoles that no longer exist (deleted device or Proxmox server).
  for (const id of guests.keys()) if (!deviceById(id)) guests.delete(id);
  tabs = tabs.filter((id) => consoleInfo(id));
  if (active && !consoleInfo(active)) active = null;
  tiles = tiles.filter((id) => tabs.includes(id));
  if (tileMode && tiles.length === 0) tileMode = false;
  render();
  renderTabs();
  checkAll();
  loadAllGuests();
}

async function check(id) {
  try {
    status.set(id, (await invoke('check_device', { id })) ? 'online' : 'offline');
  } catch {
    status.set(id, 'offline');
  }
  renderStatus();
}

function checkAll() {
  // Proxmox servers show the result of their guest listing instead.
  return Promise.all(devices.filter((d) => !isServer(d)).map((d) => check(d.id)));
}

// ---------------------------------------------------------------- tabs

function renderTabs() {
  tabsEl.replaceChildren(
    ...tabs.map((id) => {
      const info = consoleInfo(id);
      const node = tabTpl.content.firstElementChild.cloneNode(true);
      node.dataset.id = id;
      node.title = info.title;
      node.querySelector('.tab-name').textContent = info.name;
      node.setAttribute('aria-selected', String(tileMode ? tiles.includes(id) : active === id));
      return node;
    })
  );
  homeTab.setAttribute('aria-selected', String(!tileMode && active === null));
  homeEl.hidden = tileMode || active !== null;
  placeholderEl.hidden = tileMode || active === null;
  renderTiles();
  renderStatus();
  tabsEl.querySelector('[aria-selected="true"]')?.scrollIntoView({ inline: 'nearest' });
  saveSession();
  applyLayout();
}

// ---------------------------------------------------------------- tiled view

const stageEl = $('#stage');
const tilesEl = $('#tiles');
const tileTpl = $('#tile-tpl');
const tileBtn = $('#tile-btn');
let tileMode = false;
let tiles = []; // ids of the tiled consoles (max 4)
let recent = []; // tab ids, most recently shown first
const MAX_TILES = 4;

function touchRecent(id) {
  if (id) recent = [id, ...recent.filter((x) => x !== id)];
}

function renderTiles() {
  tileBtn.disabled = !tileMode && tabs.length < 2;
  tileBtn.setAttribute('aria-pressed', String(tileMode));
  tilesEl.hidden = !tileMode;
  if (!tileMode) {
    tilesEl.replaceChildren();
    return;
  }
  tilesEl.dataset.count = String(tiles.length);
  tilesEl.replaceChildren(
    ...tiles.map((id) => {
      const node = tileTpl.content.firstElementChild.cloneNode(true);
      const info = consoleInfo(id);
      node.dataset.id = id;
      node.querySelector('.tile-name').textContent = info ? info.name : id;
      node.querySelector('.tile-name').title = info ? info.title : '';
      return node;
    })
  );
}

const rectOf = (el) => {
  const r = el.getBoundingClientRect();
  return { x: r.left, y: r.top, width: r.width, height: r.height };
};

// Tells the app where each visible console goes (the webviews are native, drawn over the page).
let layoutFrame = 0;
function applyLayout(focus) {
  cancelAnimationFrame(layoutFrame);
  layoutFrame = requestAnimationFrame(() => {
    const area = rectOf(stageEl);
    let list = [];
    if (tileMode) {
      list = [...tilesEl.querySelectorAll('.tile')].map((t) => ({ id: t.dataset.id, ...rectOf(t.querySelector('.tile-body')) }));
    } else if (active) {
      list = [{ id: active, ...area }];
      focus = active;
    }
    invoke('set_layout', { area, tiles: list, focus: focus ?? null }).catch(() => {});
  });
}
new ResizeObserver(() => applyLayout()).observe(stageEl);
window.addEventListener('resize', () => applyLayout());

function enterTiles() {
  tiles = [...new Set([active, ...recent, ...tabs])].filter((id) => id && tabs.includes(id)).slice(0, MAX_TILES);
  if (tiles.length === 0) return;
  tileMode = true;
  renderTabs();
}

function exitTiles(show) {
  tileMode = false;
  tiles = [];
  active = show === undefined ? active : show;
  touchRecent(active);
  renderTabs();
}

// Adds a console to the tiled view (replacing the one shown least recently when full).
function addTile(id) {
  if (tiles.includes(id)) return;
  if (tiles.length >= MAX_TILES) {
    const oldest = [...tiles].sort((a, b) => recent.indexOf(b) - recent.indexOf(a))[0];
    tiles = tiles.filter((t) => t !== oldest);
  }
  tiles.push(id);
  touchRecent(id);
  renderTabs();
  applyLayout(id);
}

tileBtn.addEventListener('click', () => (tileMode ? exitTiles(tiles[0] ?? active) : enterTiles()));
tilesEl.addEventListener('click', (ev) => {
  const btn = ev.target.closest('[data-tile-action]');
  if (!btn) return;
  const id = btn.closest('.tile').dataset.id;
  if (btn.dataset.tileAction === 'solo') return exitTiles(id);
  tiles = tiles.filter((t) => t !== id);
  if (tiles.length === 0) return exitTiles(null);
  renderTabs();
});

// Shows a console tab (or the device list for null). In the tiled view, a console is added to
// the tiles instead, and the device list leaves the tiled view.
async function activate(id) {
  if (tileMode) {
    if (id === null) return exitTiles(null);
    return addTile(id);
  }
  active = id;
  touchRecent(id);
  renderTabs();
}

async function connect(id) {
  const d = consoleInfo(id);
  try {
    const where = await invoke('open_tab', { id });
    if (where === 'window') return; // already popped out: its window was focused
    if (!tabs.includes(id)) tabs.push(id);
    showError(errorEl, '');
    if (tileMode) return addTile(id);
    active = id;
    touchRecent(id);
    renderTabs();
  } catch (e) {
    showError(errorEl, `${d.name}: ${e}`);
  }
}

function forgetTab(id) {
  const idx = tabs.indexOf(id);
  if (idx === -1) return false;
  tabs.splice(idx, 1);
  if (active === id) return tabs[Math.min(idx, tabs.length - 1)] ?? null; // neighbour to show next
  return active;
}

async function closeTab(id) {
  if (!tabs.includes(id)) return;
  const next = forgetTab(id);
  await invoke('close_tab', { id });
  if (tileMode) {
    tiles = tiles.filter((t) => t !== id);
    if (tiles.length === 0) return exitTiles(null);
    return renderTabs();
  }
  await activate(next);
}

async function openWindow(id) {
  const d = consoleInfo(id);
  const hadTab = tabs.includes(id);
  const next = hadTab ? forgetTab(id) : active;
  try {
    await invoke('open_window', { id });
  } catch (e) {
    showError(errorEl, `${d.name}: ${e}`);
  }
  if (tileMode && tiles.includes(id)) {
    tiles = tiles.filter((t) => t !== id);
    if (tiles.length === 0) return exitTiles(null);
    return renderTabs();
  }
  if (hadTab) await activate(next);
}

homeTab.addEventListener('click', () => activate(null));

groupFilter.addEventListener('change', () => {
  shownGroup = groupFilter.value;
  render();
});

tabsEl.addEventListener('click', (ev) => {
  const tab = ev.target.closest('.tab');
  if (!tab) return;
  const id = tab.dataset.id;
  const action = ev.target.closest('[data-tab-action]')?.dataset.tabAction;
  if (action === 'close') return closeTab(id);
  if (action === 'popout') return openWindow(id);
  if (tileMode || active !== id) activate(id);
});
// Middle-click closes a tab, like a browser.
tabsEl.addEventListener('auxclick', (ev) => {
  const tab = ev.target.closest('.tab');
  if (tab && ev.button === 1) closeTab(tab.dataset.id);
});

// Console webviews are drawn natively below the tab bar; keep Rust in sync with that area.
// Measured from the tab bar and the viewport, so it is right whichever view is showing.

// ---------------------------------------------------------------- dialogs

// In-app replacement for window.confirm(), which native webviews don't all implement.
function confirmDialog(title, message) {
  $('#confirm-title').textContent = title;
  $('#confirm-msg').textContent = message;
  confirmDlg.returnValue = '';
  confirmDlg.showModal();
  $('#confirm-cancel').focus();
  return new Promise((resolve) => {
    confirmDlg.addEventListener('close', () => resolve(confirmDlg.returnValue === 'ok'), { once: true });
  });
}

// The login fields mean slightly different things per device type.
const KIND_UI = {
  '': { legend: 'Login', placeholder: 'e.g. 192.168.1.50' },
  nanokvm: { legend: 'Login (optional)', autoLogin: true, placeholder: 'e.g. 192.168.1.50 or https://kvm.lan' },
  pikvm: {
    legend: 'Login (optional)',
    autoLogin: true,
    placeholder: 'e.g. 192.168.1.40 or https://pikvm.lan',
    hint: 'Auto-login uses the PiKVM API; accounts with two-factor codes have to log in by hand.',
  },
  idrac: {
    legend: 'iDRAC login (required)',
    required: true,
    placeholder: 'e.g. 192.168.1.80 (iDRAC address)',
    hint:
      'For iDRAC 9. Each time you connect, the app logs in, opens the HTML5 virtual console and logs out ' +
      'again. The account needs the virtual console privilege.',
  },
  vnc: {
    legend: 'VNC password',
    noUsername: true,
    placeholder: 'e.g. 192.168.1.20 or 192.168.1.20:5901',
    hint:
      'Leave empty for servers without a password. The classic VNC password is supported, not RealVNC or ' +
      'Apple account logins. The connection itself is not encrypted.',
  },
  'supermicro-ikvm': {
    legend: 'BMC login (required)',
    required: true,
    placeholder: 'e.g. 192.168.1.60 (BMC address)',
    hint: "The app logs in to the BMC's web interface with this account each time you connect, to start the iKVM console.",
  },
};

function applyKind() {
  const k = KIND_UI[fields.kind.value] || KIND_UI[''];
  $('#login-legend').textContent = k.legend;
  $('#autologin-row').hidden = !k.autoLogin;
  $('#login-hint').textContent = k.hint || '';
  $('#login-hint').hidden = !k.hint;
  $('#username-row').hidden = !!k.noUsername;
  fields.url.placeholder = k.placeholder;
  fields.username.required = !!k.required;
}

fields.kind.addEventListener('change', applyKind);

function openDialog(device, prefill) {
  editingId = device ? device.id : null;
  clearPassword = false;
  forgetFingerprint = false;
  form.reset();
  showError(formError, '');
  $('#dialog-title').textContent = device ? 'Edit device' : 'Add device';
  // Group: none, or one of the standard groups (Proxmox groups only hold their own VMs).
  fields.group.replaceChildren(new Option('None', ''), ...groups.map((g) => new Option(g.name, g.name)));
  if (device) {
    fields.kind.value = device.kind || 'nanokvm';
    fields.name.value = device.name;
    fields.group.value = device.group || '';
    fields.url.value = device.url;
    fields.username.value = device.username;
    fields.autoLogin.checked = device.autoLogin;
  } else {
    // New devices start with an empty form: no type, group, account or auto-login preselected
    // (unless they come from "Find devices", which fills in what it found).
    fields.kind.value = prefill?.kind || '';
    fields.name.value = prefill?.name || '';
    fields.url.value = prefill?.url || '';
    fields.autoLogin.checked = false;
  }
  applyKind();
  passwordHint.hidden = !(device && device.hasPassword);
  $('#device-cert-row').hidden = !device?.tlsFingerprint;
  $('#device-cert-fp').textContent = device?.tlsFingerprint || '';
  dialog.showModal();
  (device ? fields.name : fields.kind).focus();
}

form.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const submit = form.querySelector('[type="submit"]');
  submit.disabled = true;
  try {
    const saved = await invoke('save_device', {
      input: {
        id: editingId,
        kind: fields.kind.value,
        name: fields.name.value,
        group: fields.group.value,
        url: fields.url.value,
        username: fields.username.value,
        password: fields.password.value || null,
        clearPassword,
        autoLogin: !!KIND_UI[fields.kind.value]?.autoLogin && fields.autoLogin.checked,
        forgetFingerprint,
      },
    });
    dialog.close();
    status.delete(saved.id);
    await load();
    if (findDialog.open) renderFound();
  } catch (e) {
    showError(formError, String(e));
  } finally {
    submit.disabled = false;
  }
});

$('#clear-password').addEventListener('click', () => {
  clearPassword = true;
  fields.password.value = '';
  passwordHint.hidden = true;
});
$('#device-forget-cert').addEventListener('click', () => {
  forgetFingerprint = true;
  $('#device-cert-row').hidden = true;
});
$('#cancel').addEventListener('click', () => dialog.close());
$('#add').addEventListener('click', () => openDialog(null));

// ---------------------------------------------------------------- group dialog

// Standard group: a name. Proxmox VE server: a name plus address and login; its VMs and
// containers are listed in the group.
function applyGroupType() {
  const pve = gfields.type.value === 'proxmox';
  $('#pve-fields').hidden = !pve;
  gfields.url.required = pve;
  gfields.username.required = pve;
  // A new Proxmox server needs a password; when editing, blank keeps the saved one.
  gfields.password.required = pve && !(editingGroup?.item.hasPassword);
}
gfields.type.addEventListener('change', applyGroupType);

// `edit`: {type: 'standard', item: group} or {type: 'proxmox', item: device}, or null to add.
function openGroupDialog(edit, prefill) {
  editingGroup = edit;
  forgetFingerprint = false;
  groupForm.reset();
  showError(groupFormError, '');
  $('#group-dialog-title').textContent = edit ? (edit.type === 'proxmox' ? 'Edit Proxmox server' : 'Rename group') : 'Add group';
  gfields.type.value = edit ? edit.type : prefill?.type || 'standard'; // most groups are standard ones
  gfields.type.disabled = !!edit; // a group can't change type
  if (edit) gfields.name.value = edit.item.name;
  if (prefill) {
    gfields.name.value = prefill.name || '';
    gfields.url.value = prefill.url || '';
  }
  const server = edit?.type === 'proxmox' ? edit.item : null;
  if (server) {
    gfields.url.value = server.url;
    gfields.username.value = server.username;
  }
  $('#pve-password-hint').hidden = !server?.hasPassword;
  $('#cert-row').hidden = !server?.tlsFingerprint;
  $('#cert-fp').textContent = server?.tlsFingerprint || '';
  applyGroupType();
  groupDialog.showModal();
  gfields.name.focus();
}

groupForm.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const submit = groupForm.querySelector('[type="submit"]');
  submit.disabled = true;
  try {
    if (gfields.type.value === 'proxmox') {
      const saved = await invoke('save_device', {
        input: {
          id: editingGroup?.item.id ?? null,
          kind: 'proxmox',
          name: gfields.name.value,
          group: '',
          url: gfields.url.value,
          username: gfields.username.value,
          password: gfields.password.value || null,
          clearPassword: false,
          autoLogin: false,
          forgetFingerprint,
        },
      });
      guests.delete(saved.id); // address or login may have changed
    } else {
      await invoke('save_group', { input: { id: editingGroup?.item.id ?? null, name: gfields.name.value } });
    }
    groupDialog.close();
    await load();
    if (findDialog.open) renderFound();
  } catch (e) {
    showError(groupFormError, String(e));
  } finally {
    submit.disabled = false;
  }
});

$('#forget-cert').addEventListener('click', () => {
  forgetFingerprint = true;
  $('#cert-row').hidden = true;
});
$('#group-cancel').addEventListener('click', () => groupDialog.close());
$('#add-group').addEventListener('click', () => openGroupDialog(null));

// Standard group sections: rename / delete.
groupsEl.addEventListener('click', async (ev) => {
  const btn = ev.target.closest('.pve-head button[data-action]');
  if (!btn) return;
  const group = groups.find((g) => g.id === btn.closest('.group-sec')?.dataset.group);
  if (!group) return;
  if (btn.dataset.action === 'edit') return openGroupDialog({ type: 'standard', item: group });
  if (btn.dataset.action === 'delete') {
    const ok = await confirmDialog(
      `Delete the group "${group.name}"?`,
      'Its devices are kept; they just won\'t be in a group any more.'
    );
    if (!ok) return;
    try {
      await invoke('delete_group', { id: group.id });
      await load();
    } catch (e) {
      showError(errorEl, String(e));
    }
  }
});
$('#refresh').addEventListener('click', async (ev) => {
  const btn = ev.currentTarget;
  btn.disabled = true;
  devices.forEach((d) => status.set(d.id, 'unknown'));
  renderStatus();
  await Promise.all([checkAll(), loadAllGuests()]);
  btn.disabled = false;
});

// ---------------------------------------------------------------- card actions

// Device cards (ungrouped and in group sections). Guest cards and section headers have their own
// handlers below.
homeEl.addEventListener('click', async (ev) => {
  const btn = ev.target.closest('.card button[data-action]');
  if (!btn || btn.closest('.guest')) return;
  const id = btn.closest('.card')?.dataset.id;
  const device = deviceById(id);
  if (!device) return;

  switch (btn.dataset.action) {
    case 'connect':
      btn.disabled = true;
      await connect(id);
      btn.disabled = false;
      break;
    case 'window':
      btn.disabled = true;
      await openWindow(id);
      btn.disabled = false;
      break;
    case 'edit':
      openDialog(device);
      break;
    case 'delete': {
      const ok = await confirmDialog(
        `Delete "${device.name}"?`,
        'Its address and saved login will be removed. Its open console tab or window will close.'
      );
      if (!ok) return;
      try {
        await invoke('delete_device', { id });
        status.delete(id);
        await load();
      } catch (e) {
        showError(errorEl, String(e));
      }
      break;
    }
  }
});
emptyEl.addEventListener('click', (ev) => {
  if (ev.target.closest('[data-action="add"]')) openDialog(null);
  if (ev.target.closest('[data-action="find"]')) $('#find-btn').click();
});

// Proxmox sections: server buttons, and Connect / new window on guest cards.
pveEl.addEventListener('click', async (ev) => {
  const btn = ev.target.closest('button[data-action]');
  if (!btn) return;
  const guestId = btn.closest('.guest')?.dataset.id;
  if (guestId) {
    btn.disabled = true;
    await (btn.dataset.action === 'window' ? openWindow(guestId) : connect(guestId));
    btn.disabled = false;
    return;
  }
  const server = deviceById(btn.closest('.pve')?.dataset.server);
  if (!server) return;
  switch (btn.dataset.action) {
    case 'refresh':
      btn.disabled = true;
      await loadGuests(server.id);
      btn.disabled = false;
      break;
    case 'edit':
      openGroupDialog({ type: 'proxmox', item: server });
      break;
    case 'delete': {
      const ok = await confirmDialog(
        `Delete "${server.name}"?`,
        'Its address, saved login and pinned certificate will be removed. Open consoles of its VMs and containers will close.'
      );
      if (!ok) return;
      try {
        await invoke('delete_device', { id: server.id });
        guests.delete(server.id);
        await load();
      } catch (e) {
        showError(errorEl, String(e));
      }
      break;
    }
  }
});

// ---------------------------------------------------------------- settings & session

let settings = { restoreTabs: false, passwordStorage: 'keychain' };
let restoring = false; // don't record the session while it is being restored

async function loadSettings() {
  try {
    settings = await invoke('get_settings');
  } catch (e) {
    showError(errorEl, String(e));
  }
}

// Remembers the open tabs while "reopen tabs" is on (debounced).
let sessionTimer = null;
function saveSession() {
  if (!settings.restoreTabs || restoring) return;
  clearTimeout(sessionTimer);
  sessionTimer = setTimeout(() => {
    invoke('save_settings', { settings: { restoreTabs: true, openTabs: tabs, activeTab: active } }).catch(() => {});
  }, 300);
}

async function restoreSession() {
  const ids = (settings.openTabs || []).filter((id) => consoleInfo(id));
  if (!settings.restoreTabs || ids.length === 0) return;
  restoring = true;
  for (const id of ids) await connect(id);
  restoring = false;
  const wanted = settings.activeTab && tabs.includes(settings.activeTab) ? settings.activeTab : null;
  await activate(wanted);
}

const settingsDialog = $('#settings-dialog');
const settingsForm = $('#settings-form');
$('#settings-btn').addEventListener('click', async () => {
  await loadSettings();
  showError($('#settings-error'), '');
  showError($('#backup-result'), '');
  settingsForm.elements.restoreTabs.checked = !!settings.restoreTabs;
  $('#password-storage').textContent =
    settings.passwordStorage === 'keychain'
      ? 'Passwords are stored in the system keychain.'
      : 'No system keychain is available, so passwords are stored in the device file (readable only by your user).';
  settingsDialog.showModal();
});
$('#settings-cancel').addEventListener('click', () => settingsDialog.close());
settingsForm.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const restoreTabs = settingsForm.elements.restoreTabs.checked;
  try {
    await invoke('save_settings', { settings: { restoreTabs, openTabs: restoreTabs ? tabs : [], activeTab: restoreTabs ? active : null } });
    settings.restoreTabs = restoreTabs;
    settingsDialog.close();
  } catch (e) {
    showError($('#settings-error'), String(e));
  }
});

// ---------------------------------------------------------------- find devices

const findDialog = $('#find-dialog');
const findForm = $('#find-form');
const findResults = $('#find-results');
let lastFound = []; // results of the last scan, until the dialog is closed

// Host part of a saved device address (http(s)://host[:port], vnc://host:port).
function hostOf(url) {
  try {
    return new URL(url).hostname.replace(/^\[|\]$/g, '');
  } catch {
    return '';
  }
}

// Shows the scan results that aren't in the device list yet (it may have changed since the scan).
function renderFound() {
  const known = new Set(devices.map((d) => hostOf(d.url)));
  const fresh = lastFound.filter((f) => !known.has(f.address));
  const skipped = lastFound.length - fresh.length;
  const plural = (n) => `${n} device${n === 1 ? '' : 's'}`;
  let text;
  if (lastFound.length === 0) text = 'No supported devices found in that range.';
  else if (fresh.length === 0)
    text = lastFound.length === 1 ? 'The device found is already in your list.' : `All ${plural(lastFound.length)} found are already in your list.`;
  else text = `Found ${fresh.length} new ${fresh.length === 1 ? 'device' : 'devices'}${skipped ? ` (${skipped} more already in your list, not shown)` : ''}.`;
  showError($('#find-status'), text);
  findResults.replaceChildren(...fresh.map(foundRow));
}

$('#find-btn').addEventListener('click', async () => {
  showError($('#find-error'), '');
  showError($('#find-status'), '');
  lastFound = [];
  findResults.replaceChildren();
  findDialog.showModal();
  findForm.elements.range.focus();
  // This computer's networks as one-click suggestions (the field itself stays as the user left it).
  let nets = [];
  try {
    nets = await invoke('local_networks');
  } catch {}
  const chips = $('#find-nets');
  chips.replaceChildren(
    ...(nets.length ? [document.createTextNode('Your networks:')] : []),
    ...nets.map((n) => {
      const b = document.createElement('button');
      b.type = 'button';
      b.className = 'ghost';
      b.textContent = n;
      b.addEventListener('click', () => {
        findForm.elements.range.value = n;
        findForm.elements.range.focus();
      });
      return b;
    })
  );
  chips.hidden = nets.length === 0;
});
$('#find-close').addEventListener('click', () => findDialog.close());

findForm.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const scanBtn = $('#find-scan');
  scanBtn.disabled = true;
  showError($('#find-error'), '');
  showError($('#find-status'), 'Scanning… this takes a few seconds up to a minute, depending on the range.');
  findResults.replaceChildren();
  try {
    lastFound = await invoke('discover', { range: findForm.elements.range.value });
    renderFound();
  } catch (e) {
    showError($('#find-status'), '');
    showError($('#find-error'), String(e));
  } finally {
    scanBtn.disabled = false;
  }
});

const FOUND_LABEL = { ...KIND_LABEL, proxmox: 'Proxmox VE' };

function foundRow(f) {
  const li = document.createElement('li');
  const main = document.createElement('div');
  main.className = 'f-main';
  const title = document.createElement('div');
  title.className = 'f-title';
  title.textContent = `${FOUND_LABEL[f.kind] || f.kind} · ${f.address}`;
  const detail = document.createElement('div');
  detail.className = 'f-detail';
  detail.textContent = f.detail;
  main.append(title, detail);
  const add = document.createElement('button');
  add.type = 'button';
  add.className = 'primary';
  add.textContent = 'Add…';
  // The Add dialog opens on top; the scan results stay until this dialog is closed.
  add.addEventListener('click', () => {
    const name = `${FOUND_LABEL[f.kind] || f.kind} ${f.address}`;
    if (f.kind === 'proxmox') openGroupDialog(null, { type: 'proxmox', name, url: f.url });
    else openDialog(null, { kind: f.kind, name, url: f.url });
  });
  li.append(main, add);
  return li;
}

// ---------------------------------------------------------------- export / import

const passDialog = $('#pass-dialog');
const passForm = $('#pass-form');
let passMode = null; // 'export' | 'import'

function openPassDialog(mode) {
  passMode = mode;
  passForm.reset();
  showError($('#pass-error'), '');
  const exporting = mode === 'export';
  $('#pass-title').textContent = exporting ? 'Export devices' : 'Import devices';
  $('#pass-msg').textContent = exporting
    ? 'Choose a passphrase of at least 8 characters. It is needed to import the file and cannot be recovered. You pick where to save the file next.'
    : 'Enter the passphrase the file was exported with. You pick the file next.';
  $('#pass-confirm-row').hidden = !exporting;
  passForm.elements.confirm.required = exporting;
  $('#pass-ok').textContent = exporting ? 'Choose file and export' : 'Choose file and import';
  passDialog.showModal();
  passForm.elements.passphrase.focus();
}

passForm.addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const passphrase = passForm.elements.passphrase.value;
  if (passMode === 'export' && passphrase !== passForm.elements.confirm.value) {
    return showError($('#pass-error'), 'The passphrases differ.');
  }
  const ok = $('#pass-ok');
  ok.disabled = true;
  try {
    const result = await invoke(passMode === 'export' ? 'export_devices' : 'import_devices', { passphrase });
    passDialog.close();
    if (result === null) return; // file picker cancelled
    const msg = passMode === 'export' ? `Exported to ${result}` : result;
    showError($('#backup-result'), msg);
    if (passMode === 'import') await load();
  } catch (e) {
    showError($('#pass-error'), String(e));
  } finally {
    ok.disabled = false;
  }
});
$('#pass-cancel').addEventListener('click', () => passDialog.close());
$('#export-btn').addEventListener('click', () => openPassDialog('export'));
$('#import-btn').addEventListener('click', () => openPassDialog('import'));

// ---------------------------------------------------------------- quick connect (Ctrl+K)
// Only works while the device list or tab bar has focus; inside a console, keys go to the device.

const quick = $('#quick');
const quickInput = $('#quick-input');
const quickList = $('#quick-list');
let quickItems = [];
let quickSel = -1;

function consoleChoices() {
  const items = devices
    .filter((d) => !isServer(d))
    .map((d) => ({
      id: d.id,
      name: d.name,
      detail: [KIND_LABEL[d.kind] || d.kind, d.url, d.group].filter(Boolean).join(' · '),
      enabled: true,
    }));
  for (const [serverId, info] of guests) {
    const server = deviceById(serverId);
    for (const g of info.list || []) {
      items.push({
        id: g.id,
        name: g.name,
        detail: `${guestLabel(g)} · ${server ? server.name : 'Proxmox'} · ${g.status}`,
        enabled: g.status === 'running',
      });
    }
  }
  return items;
}

function renderQuick() {
  const q = quickInput.value.trim().toLowerCase();
  const rank = (i) => (i.name.toLowerCase().startsWith(q) ? 0 : 1);
  quickItems = consoleChoices()
    .filter((i) => !q || `${i.name} ${i.detail}`.toLowerCase().includes(q))
    .sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
  quickSel = quickItems.findIndex((i) => i.enabled);
  drawQuick();
}

function drawQuick() {
  if (quickItems.length === 0) {
    const li = document.createElement('li');
    li.className = 'q-empty';
    li.textContent = 'Nothing matches';
    quickList.replaceChildren(li);
    return;
  }
  quickList.replaceChildren(
    ...quickItems.map((item, idx) => {
      const li = document.createElement('li');
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(idx === quickSel));
      if (!item.enabled) li.className = 'off';
      const name = document.createElement('span');
      name.className = 'q-name';
      name.textContent = item.name;
      const detail = document.createElement('span');
      detail.className = 'q-detail';
      detail.textContent = item.detail;
      li.append(name, detail);
      li.addEventListener('click', (ev) => pickQuick(idx, ev.shiftKey));
      return li;
    })
  );
  quickList.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
}

function moveQuick(step) {
  if (quickItems.length === 0) return;
  let i = quickSel;
  for (let n = 0; n < quickItems.length; n++) {
    i = (i + step + quickItems.length) % quickItems.length;
    if (quickItems[i].enabled) break;
  }
  quickSel = i;
  drawQuick();
}

async function pickQuick(idx, newWindow) {
  const item = quickItems[idx];
  if (!item || !item.enabled) return;
  quick.close();
  await (newWindow ? openWindow(item.id) : connect(item.id));
}

function openQuick() {
  if (quick.open) return;
  quickInput.value = '';
  renderQuick();
  quick.showModal();
  quickInput.focus();
}

quickInput.addEventListener('input', renderQuick);
quickInput.addEventListener('keydown', (ev) => {
  if (ev.key === 'ArrowDown') { ev.preventDefault(); moveQuick(1); }
  else if (ev.key === 'ArrowUp') { ev.preventDefault(); moveQuick(-1); }
  else if (ev.key === 'Enter') { ev.preventDefault(); pickQuick(quickSel, ev.shiftKey); }
});
quick.addEventListener('click', (ev) => {
  if (ev.target === quick) quick.close(); // click on the backdrop
});
$('#quick-btn').addEventListener('click', openQuick);
document.addEventListener('keydown', (ev) => {
  if ((ev.ctrlKey || ev.metaKey) && !ev.altKey && ev.key.toLowerCase() === 'k') {
    ev.preventDefault();
    openQuick();
  }
});

setInterval(() => {
  checkAll();
  loadAllGuests();
}, POLL_MS);
applyLayout();
(async () => {
  await loadSettings();
  await load();
  await restoreSession();
})();
