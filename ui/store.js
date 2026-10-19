import { useEffect, useState } from './vendor/preact-htm.js';
import { invoke, listen } from './api.js';
import { setLanguage, t } from './i18n.js';

const MAX_LOG_LINES = 1500;

export const state = {
  ready: false,
  init: null,          // static facts from app_init (platform, version, lists)
  settings: null,      // { ui, sender, receiver }
  access: null,        // who may connect and control: { rev, available, others, devices }
  autostart: null,     // starting at sign-in, as the system has it: { supported, on, other, disabled }
  status: null,        // the latest runtime snapshot (pushed a few times a second)
  group: null,
  tools: null,
  firewall: null,
  outputs: [],
  audioDevices: [],
  logs: [],
  page: 'share',
  sheet: null,         // { name, props }
  toasts: [],
};

const listeners = new Set();

export function setState(patch) {
  Object.assign(state, patch);
  for (const l of listeners) l();
}

export function useStore() {
  const [, force] = useState(0);
  useEffect(() => {
    const l = () => force((n) => n + 1);
    listeners.add(l);
    return () => listeners.delete(l);
  }, []);
  return state;
}

export function applyAppearance() {
  const ui = state.settings?.ui;
  const root = document.documentElement;
  if (ui?.theme === 'light' || ui?.theme === 'dark') root.dataset.theme = ui.theme;
  else delete root.dataset.theme;
  setLanguage(ui?.language || 'auto', state.init?.locale || navigator.language);
  root.lang = document.documentElement.dataset.lang || 'en';
  sendTrayTexts();
}

// The icon in the notification area is drawn by the system: it gets its words from here, in the same language.
let trayLang = null;
function sendTrayTexts() {
  const lang = document.documentElement.dataset.lang;
  if (lang === trayLang) return;
  trayLang = lang;
  const texts = { open: t('tray.open'), quit: t('tray.quit'), available: t('tray.available'), sharing: t('tray.sharing') };
  invoke('tray_texts', { texts }).catch(() => {});
}

export async function boot() {
  const init = await invoke('app_init');
  setState({
    init,
    settings: { ui: init.ui, sender: init.sender, receiver: init.receiver },
    access: init.access,
    autostart: init.autostart,
    group: init.group,
    tools: init.tools,
    firewall: init.firewall,
    outputs: init.outputs,
    audioDevices: init.audioDevices,
    status: init.status,
    logs: init.logs || [],
    ready: true,
  });
  applyAppearance();
  document.documentElement.classList.toggle('backdrop', !!init.backdrop);
  document.documentElement.classList.add(init.platform === 'macos' ? 'mac' : init.platform === 'windows' ? 'win' : 'linux');

  listen('status', (status) => {
    const patch = { status };
    if (status.group) patch.group = status.group;
    if (status.firewall) patch.firewall = status.firewall;
    setState(patch);
    // the rules changed behind the page's back (a request answered with "always"): fetch them again
    if (status.accessRev != null && state.access && status.accessRev !== state.access.rev && !accessTimer) {
      invoke('access_get').then((access) => { if (!accessTimer) setState({ access }); }).catch(() => {});
    }
  });
  listen('log', (payload) => {
    const logs = state.logs.concat(payload.lines);
    setState({ logs: logs.length > MAX_LOG_LINES ? logs.slice(logs.length - MAX_LOG_LINES) : logs });
  });
  listen('invite', (preview) => {
    if (!state.sheet) openSheet('invite-found', { preview });
  });
  // HdrCast was started again with an invite while it ran: the same as being started with it
  listen('join-invite', (preview) => openSheet('join', { preview }));
  if (init.startupInvite) openSheet('join', { preview: init.startupInvite });
}

// ---- settings: edit locally at once, save a moment later, take back what the backend normalised -----------------

let saveTimer = null;
let saveSeq = 0;

export function editSettings(section, patch) {
  const settings = { ...state.settings, [section]: { ...state.settings[section], ...patch } };
  setState({ settings });
  if (section === 'ui') applyAppearance();
  clearTimeout(saveTimer);
  const seq = ++saveSeq;
  saveTimer = setTimeout(async () => {
    try {
      const saved = await invoke('settings_save', { settings: state.settings });
      if (seq !== saveSeq) return;          // edited again meanwhile: keep what the user sees
      setState({ settings: { ui: saved.ui, sender: saved.sender, receiver: saved.receiver }, tools: saved.tools || state.tools });
    } catch (e) {
      toast(String(e), { kind: 'warn' });
    }
  }, 250);
}

/** Flushes a pending save, so a Start right after an edit uses the new values. */
export async function flushSettings() {
  if (!saveTimer) return;
  clearTimeout(saveTimer);
  saveTimer = null;
  const saved = await invoke('settings_save', { settings: state.settings });
  setState({ settings: { ui: saved.ui, sender: saved.sender, receiver: saved.receiver }, tools: saved.tools || state.tools });
}

// ---- access rules: the same, in their own file -------------------------------------------------------------------

let accessTimer = null;
let accessSeq = 0;

export function editAccess(patch) {
  setState({ access: { ...state.access, ...patch } });
  clearTimeout(accessTimer);
  const seq = ++accessSeq;
  accessTimer = setTimeout(async () => {
    try {
      const saved = await invoke('access_save', { access: state.access });
      if (seq === accessSeq) setState({ access: saved });
    } catch (e) {
      toast(String(e), { kind: 'warn' });
    }
    if (seq === accessSeq) accessTimer = null;
  }, 250);
}

/** Changes the rule of one device (by key), or removes it with `patch` = null. */
export function editDevice(key, patch) {
  const devices = patch === null
    ? state.access.devices.filter((d) => d.key !== key)
    : state.access.devices.map((d) => (d.key === key ? { ...d, ...patch } : d));
  editAccess({ devices });
}

// ---- sheets and toasts ---------------------------------------------------------------------------------------

export function openSheet(name, props) {
  setState({ sheet: { name, props: props || {} } });
}

export function closeSheet() {
  setState({ sheet: null });
}

let toastId = 0;

export function toast(text, opts) {
  const id = ++toastId;
  const t = { id, text, kind: opts?.kind || 'info', actions: opts?.actions || null };
  setState({ toasts: state.toasts.concat(t).slice(-3) });
  const ms = opts?.ms ?? (t.actions ? 0 : 3600);
  if (ms > 0) setTimeout(() => dismissToast(id), ms);
  return id;
}

export function dismissToast(id) {
  setState({ toasts: state.toasts.filter((t) => t.id !== id) });
}

export function go(page) {
  setState({ page });
}
