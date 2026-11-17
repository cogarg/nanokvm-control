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
const groupList = $('#group-list');
const pveEl = $('#pve');
const pveTpl = $('#pve-section');
const guestTpl = $('#guest-card');

const POLL_MS = 15000;
let devices = [];
const status = new Map(); // id -> 'online' | 'offline' | 'unknown'
let editingId = null;
// What the list shows: '' = everything, 'group:<name>' = one group, 'pve:<id>' = one Proxmox
// server's guests. Starts as everything on every launch.
let shownGroup = '';
let clearPassword = false;
let forgetFingerprint = false;
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

const groupNames = () =>
  [...new Set(devices.filter((d) => !isServer(d)).map((d) => d.group).filter(Boolean))].sort((a, b) =>
    a.localeCompare(b)
  );

// The "Show" dropdown: everything, one group, or one Proxmox server. Hidden while there is nothing
// to choose from.
function renderGroupFilter() {
  const groups = groupNames();
  const servers = devices.filter(isServer);
  const values = [...groups.map((g) => `group:${g}`), ...servers.map((d) => `pve:${d.id}`)];
  if (shownGroup && !values.includes(shownGroup)) shownGroup = ''; // that group/server is gone
  groupFilter.replaceChildren(
    new Option('All devices', ''),
    ...groups.map((g) => new Option(g, `group:${g}`)),
    ...servers.map((d) => new Option(`${d.name} (Proxmox)`, `pve:${d.id}`))
  );
  groupFilter.value = shownGroup;
  filterLabel.hidden = values.length === 0;
  groupList.replaceChildren(...groups.map((g) => new Option(g)));
}

function render() {
  renderGroupFilter();
  emptyEl.hidden = devices.length > 0;
  const plain = devices.filter((d) => !isServer(d));
  const shown = !shownGroup
    ? plain
    : shownGroup.startsWith('group:')
      ? plain.filter((d) => d.group === shownGroup.slice(6))
      : [];
  listEl.replaceChildren(
    ...shown.map((d) => {
      const node = cardTpl.content.firstElementChild.cloneNode(true);
      node.dataset.id = d.id;
      node.querySelector('.name').textContent = d.name;
      node.querySelector('.name').title = d.name;
      const tag = node.querySelector('.group-tag');
      tag.textContent = d.group;
      tag.title = `Group: ${d.group}`;
      tag.hidden = !d.group;
      node.querySelector('.url').textContent = d.url;
      const meta = node.querySelector('.meta');
      if (d.kind === 'supermicro-ikvm') {
        if (d.username && d.hasPassword) meta.textContent = `Supermicro iKVM · as ${d.username}`;
        else {
          meta.textContent = `Supermicro iKVM: the BMC ${d.username ? 'password' : 'username'} is missing`;
          meta.classList.add('warn');
        }
      } else if (!d.autoLogin) meta.textContent = 'Manual login';
      else if (d.username && d.hasPassword) meta.textContent = `Auto-login as ${d.username}`;
      else {
        meta.textContent = `Auto-login is on, but the ${d.username ? 'password' : 'username'} is missing`;
        meta.classList.add('warn');
      }
      return node;
    })
  );
  listEl.hidden = shown.length === 0;
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
    devices = await invoke('list_devices');
    devices.sort((a, b) => a.name.localeCompare(b.name));
    showError(errorEl, '');
  } catch (e) {
    showError(errorEl, String(e));
  }
  // Drop tabs for consoles that no longer exist (deleted device or Proxmox server).
  for (const id of guests.keys()) if (!deviceById(id)) guests.delete(id);
  tabs = tabs.filter((id) => consoleInfo(id));
  if (active && !consoleInfo(active)) active = null;
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
  homeTab.setAttribute('aria-selected', String(active === null));
  tabsEl.replaceChildren(
    ...tabs.map((id) => {
      const info = consoleInfo(id);
      const node = tabTpl.content.firstElementChild.cloneNode(true);
      node.dataset.id = id;
      node.title = info.title;
      node.querySelector('.tab-name').textContent = info.name;
      node.setAttribute('aria-selected', String(active === id));
      return node;
    })
  );
  homeEl.hidden = active !== null;
  placeholderEl.hidden = active === null;
  renderStatus();
  tabsEl.querySelector('[aria-selected="true"]')?.scrollIntoView({ inline: 'nearest' });
}

async function activate(id) {
  active = id;
  renderTabs();
  try {
    await invoke('show_tab', { id });
  } catch (e) {
    showError(errorEl, String(e));
  }
}

async function connect(id) {
  const d = consoleInfo(id);
  try {
    const where = await invoke('open_tab', { id });
    if (where === 'window') return; // already popped out: its window was focused
    if (!tabs.includes(id)) tabs.push(id);
    showError(errorEl, '');
    active = id;
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
  if (active !== id) activate(id);
});
// Middle-click closes a tab, like a browser.
tabsEl.addEventListener('auxclick', (ev) => {
  const tab = ev.target.closest('.tab');
  if (tab && ev.button === 1) closeTab(tab.dataset.id);
});

// Console webviews are drawn natively below the tab bar; keep Rust in sync with that area.
// Measured from the tab bar and the viewport, so it is right whichever view is showing.
const tabbarEl = $('.tabbar');
function reportBounds() {
  const top = tabbarEl.getBoundingClientRect().bottom;
  invoke('set_console_bounds', {
    bounds: { x: 0, y: top, width: window.innerWidth, height: Math.max(0, window.innerHeight - top) },
  }).catch(() => {});
}
new ResizeObserver(reportBounds).observe(tabbarEl);
window.addEventListener('resize', reportBounds);

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
  'supermicro-ikvm': {
    legend: 'BMC login (required)',
    required: true,
    placeholder: 'e.g. 192.168.1.60 (BMC address)',
    hint: "The app logs in to the BMC's web interface with this account each time you connect, to start the iKVM console.",
  },
  proxmox: {
    legend: 'Proxmox login (required)',
    required: true,
    noGroup: true,
    placeholder: 'e.g. 192.168.1.10 (port 8006 is used unless you give one)',
    hint:
      'Username as user@realm, e.g. root@pam ("root" alone means root@pam). Accounts with two-factor ' +
      'authentication are not supported. The app only lists VMs and containers and opens their consoles.',
  },
};

function applyKind() {
  const k = KIND_UI[fields.kind.value] || KIND_UI[''];
  $('#login-legend').textContent = k.legend;
  $('#autologin-row').hidden = !k.autoLogin;
  $('#login-hint').textContent = k.hint || '';
  $('#login-hint').hidden = !k.hint;
  $('#group-row').hidden = !!k.noGroup;
  fields.url.placeholder = k.placeholder;
  fields.username.required = !!k.required;
}

fields.kind.addEventListener('change', applyKind);

function openDialog(device) {
  editingId = device ? device.id : null;
  clearPassword = false;
  forgetFingerprint = false;
  form.reset();
  showError(formError, '');
  $('#dialog-title').textContent = device ? 'Edit device' : 'Add device';
  if (device) {
    fields.kind.value = device.kind || 'nanokvm';
    fields.name.value = device.name;
    fields.group.value = device.group || '';
    fields.url.value = device.url;
    fields.username.value = device.username;
    fields.autoLogin.checked = device.autoLogin;
  } else {
    // New devices start with an empty form: no type, group, account or auto-login preselected.
    fields.kind.value = '';
    fields.autoLogin.checked = false;
  }
  applyKind();
  passwordHint.hidden = !(device && device.hasPassword);
  $('#cert-row').hidden = !(device && device.tlsFingerprint);
  $('#cert-fp').textContent = device?.tlsFingerprint || '';
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
        group: fields.kind.value === 'proxmox' ? '' : fields.group.value,
        url: fields.url.value,
        username: fields.username.value,
        password: fields.password.value || null,
        clearPassword,
        autoLogin: fields.kind.value === 'nanokvm' && fields.autoLogin.checked,
        forgetFingerprint,
      },
    });
    dialog.close();
    status.delete(saved.id);
    guests.delete(saved.id); // address or login may have changed
    await load();
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
$('#forget-cert').addEventListener('click', () => {
  forgetFingerprint = true;
  $('#cert-row').hidden = true;
});
$('#cancel').addEventListener('click', () => dialog.close());
$('#add').addEventListener('click', () => openDialog(null));
$('#refresh').addEventListener('click', async (ev) => {
  const btn = ev.currentTarget;
  btn.disabled = true;
  devices.forEach((d) => status.set(d.id, 'unknown'));
  renderStatus();
  await Promise.all([checkAll(), loadAllGuests()]);
  btn.disabled = false;
});

// ---------------------------------------------------------------- card actions

listEl.addEventListener('click', async (ev) => {
  const btn = ev.target.closest('button[data-action]');
  if (!btn) return;
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
      openDialog(server);
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

setInterval(() => {
  checkAll();
  loadAllGuests();
}, POLL_MS);
reportBounds();
load();
