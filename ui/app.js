'use strict';

const { invoke } = window.__TAURI__.core;

const $ = (sel) => document.querySelector(sel);
const listEl = $('#devices');
const emptyEl = $('#empty');
const errorEl = $('#error');
const dialog = $('#dialog');
const form = $('#form');
const fields = form.elements;
const formError = $('#form-error');
const passwordHint = $('#password-hint');
const cardTpl = $('#card');
const confirmDlg = $('#confirm');

const POLL_MS = 15000;
let devices = [];
const status = new Map(); // id -> 'online' | 'offline' | 'unknown'
let editingId = null;
let clearPassword = false;

function showError(el, msg) {
  el.textContent = msg || '';
  el.hidden = !msg;
}

function render() {
  emptyEl.hidden = devices.length > 0;
  listEl.replaceChildren(
    ...devices.map((d) => {
      const node = cardTpl.content.firstElementChild.cloneNode(true);
      node.dataset.id = d.id;
      node.querySelector('.name').textContent = d.name;
      node.querySelector('.name').title = d.name;
      node.querySelector('.url').textContent = d.url;
      node.querySelector('.meta').textContent =
        d.autoLogin && d.username && d.hasPassword ? `Auto-login as ${d.username}` : 'Manual login';
      setDot(node, status.get(d.id) || 'unknown');
      return node;
    })
  );
}

function setDot(card, s) {
  const dot = card.querySelector('.dot');
  dot.dataset.status = s;
  dot.title = { online: 'Online', offline: 'Unreachable', unknown: 'Checking…' }[s];
}

async function load() {
  try {
    devices = await invoke('list_devices');
    devices.sort((a, b) => a.name.localeCompare(b.name));
    showError(errorEl, '');
  } catch (e) {
    showError(errorEl, String(e));
  }
  render();
  checkAll();
}

async function check(id) {
  try {
    status.set(id, (await invoke('check_device', { id })) ? 'online' : 'offline');
  } catch {
    status.set(id, 'offline');
  }
  const card = listEl.querySelector(`[data-id="${CSS.escape(id)}"]`);
  if (card) setDot(card, status.get(id));
}

function checkAll() {
  return Promise.all(devices.map((d) => check(d.id)));
}

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

function openDialog(device) {
  editingId = device ? device.id : null;
  clearPassword = false;
  form.reset();
  showError(formError, '');
  $('#dialog-title').textContent = device ? 'Edit device' : 'Add device';
  if (device) {
    fields.name.value = device.name;
    fields.url.value = device.url;
    fields.username.value = device.username;
    fields.autoLogin.checked = device.autoLogin;
  } else {
    fields.autoLogin.checked = true;
  }
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
  ev.currentTarget.disabled = true;
  devices.forEach((d) => status.set(d.id, 'unknown'));
  render();
  await checkAll();
  ev.currentTarget.disabled = false;
});

document.addEventListener('click', async (ev) => {
  const btn = ev.target.closest('button[data-action]');
  if (!btn) return;
  const action = btn.dataset.action;
  if (action === 'add') return openDialog(null);

  const id = btn.closest('.card')?.dataset.id;
  const device = devices.find((d) => d.id === id);
  if (!device) return;

  if (action === 'edit') return openDialog(device);

  if (action === 'delete') {
    const ok = await confirmDialog(
      `Delete "${device.name}"?`,
      'Its address and saved login will be removed. An open console window for it will close.'
    );
    if (!ok) return;
    try {
      await invoke('delete_device', { id });
      status.delete(id);
      await load();
    } catch (e) {
      showError(errorEl, String(e));
    }
    return;
  }

  if (action === 'connect') {
    btn.disabled = true;
    try {
      await invoke('connect_device', { id });
      showError(errorEl, '');
    } catch (e) {
      showError(errorEl, `${device.name}: ${e}`);
    } finally {
      btn.disabled = false;
    }
  }
});

setInterval(checkAll, POLL_MS);
load();
