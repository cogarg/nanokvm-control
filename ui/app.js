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

const POLL_MS = 15000;
let devices = [];
const status = new Map(); // id -> 'online' | 'offline' | 'unknown'
let editingId = null;
let clearPassword = false;

// Console tabs, in display order. The native webviews live in Rust; this is the UI's view of them.
let tabs = []; // [id]
let active = null; // device id of the visible tab, or null for the device list

const deviceById = (id) => devices.find((d) => d.id === id);
const statusTitle = { online: 'Online', offline: 'Unreachable', unknown: 'Checking…' };

function showError(el, msg) {
  el.textContent = msg || '';
  el.hidden = !msg;
}

// ---------------------------------------------------------------- device list

function render() {
  emptyEl.hidden = devices.length > 0;
  listEl.replaceChildren(
    ...devices.map((d) => {
      const node = cardTpl.content.firstElementChild.cloneNode(true);
      node.dataset.id = d.id;
      node.querySelector('.name').textContent = d.name;
      node.querySelector('.name').title = d.name;
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
  renderStatus();
}

function renderStatus() {
  document.querySelectorAll('[data-id]').forEach((el) => {
    const dot = el.querySelector('.dot');
    if (!dot) return;
    const s = status.get(el.dataset.id) || 'unknown';
    dot.dataset.status = s;
    dot.title = statusTitle[s];
  });
}

async function load() {
  try {
    devices = await invoke('list_devices');
    devices.sort((a, b) => a.name.localeCompare(b.name));
    showError(errorEl, '');
  } catch (e) {
    showError(errorEl, String(e));
  }
  // Drop tabs for devices that no longer exist; refresh names of the rest.
  tabs = tabs.filter((id) => deviceById(id));
  if (active && !deviceById(active)) active = null;
  render();
  renderTabs();
  checkAll();
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
  return Promise.all(devices.map((d) => check(d.id)));
}

// ---------------------------------------------------------------- tabs

function renderTabs() {
  homeTab.setAttribute('aria-selected', String(active === null));
  tabsEl.replaceChildren(
    ...tabs.map((id) => {
      const d = deviceById(id);
      const node = tabTpl.content.firstElementChild.cloneNode(true);
      node.dataset.id = id;
      node.title = `${d.name} — ${d.url}`;
      node.querySelector('.tab-name').textContent = d.name;
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
  const d = deviceById(id);
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
  const d = deviceById(id);
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

// Default accounts: NanoKVM "admin", Supermicro BMC "ADMIN".
const DEFAULT_USER = { nanokvm: 'admin', 'supermicro-ikvm': 'ADMIN' };

// The login fields mean slightly different things per device type.
function applyKind() {
  const ikvm = fields.kind.value === 'supermicro-ikvm';
  $('#login-legend').textContent = ikvm ? 'BMC login (required)' : 'Login (optional)';
  $('#autologin-row').hidden = ikvm;
  $('#ikvm-hint').hidden = !ikvm;
  fields.url.placeholder = ikvm ? 'BMC address, e.g. 192.168.1.60' : '192.168.1.50 or https://kvm.lan';
  fields.username.required = ikvm;
}

fields.kind.addEventListener('change', () => {
  // Swap the suggested username if it's still the other type's default.
  if (!editingId && Object.values(DEFAULT_USER).includes(fields.username.value)) {
    fields.username.value = DEFAULT_USER[fields.kind.value];
  }
  applyKind();
});

function openDialog(device) {
  editingId = device ? device.id : null;
  clearPassword = false;
  form.reset();
  showError(formError, '');
  $('#dialog-title').textContent = device ? 'Edit device' : 'Add device';
  if (device) {
    fields.kind.value = device.kind || 'nanokvm';
    fields.name.value = device.name;
    fields.url.value = device.url;
    fields.username.value = device.username;
    fields.autoLogin.checked = device.autoLogin;
  } else {
    fields.kind.value = 'nanokvm';
    fields.username.value = DEFAULT_USER.nanokvm;
    fields.autoLogin.checked = true;
  }
  applyKind();
  passwordHint.hidden = !(device && device.hasPassword);
  dialog.showModal();
  fields.name.focus();
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
        url: fields.url.value,
        username: fields.username.value,
        password: fields.password.value || null,
        clearPassword,
        autoLogin: fields.autoLogin.checked,
      },
    });
    dialog.close();
    status.delete(saved.id);
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
$('#cancel').addEventListener('click', () => dialog.close());
$('#add').addEventListener('click', () => openDialog(null));
$('#refresh').addEventListener('click', async (ev) => {
  const btn = ev.currentTarget;
  btn.disabled = true;
  devices.forEach((d) => status.set(d.id, 'unknown'));
  renderStatus();
  await checkAll();
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

setInterval(checkAll, POLL_MS);
reportBounds();
load();
