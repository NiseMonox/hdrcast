import { html, render, useEffect, useRef, useState } from './vendor/preact-htm.js';
import { invoke, win } from './api.js';
import { t } from './i18n.js';
import { Icon, Caption } from './icons.js';
import { useStore, boot, go, openSheet, closeSheet, toast, dismissToast, setState, state } from './store.js';
import { Avatar, Sheet } from './components.js';
import { SharePage, ShareActionBar, AddDeviceSheet, isSharing, answerKnock, copyInvite } from './pages/share.js';
import { WatchPage, isWatching } from './pages/watch.js';
import { SettingsPage, ActivityPage, pasteInvite, routeInvite } from './pages/settings.js';

function App() {
  const s = useStore();
  if (!s.ready) return null;
  return html`
    ${s.group.configured ? html`<${Shell} />` : html`<${Welcome} />`}
    ${s.sheet && html`<${Sheets} sheet=${s.sheet} />`}
    <${Toasts} toasts=${s.toasts} />`;
}

// ------------------------------------------------------------------------------------------------ shell

function WindowButtons() {
  const s = useStore();
  if (s.init.platform === 'macos') return null;      // the traffic lights are the system's
  return html`<div class="caption">
    <button title=${t('win.minimize')} onClick=${win.minimize}>${Caption.minimize}</button>
    <button title=${t('win.maximize')} onClick=${win.toggleMaximize}>${Caption.maximize}</button>
    <button class="close" title=${t('win.close')} onClick=${win.close}>${Caption.close}</button>
  </div>`;
}

function Shell() {
  const s = useStore();
  const [scrolled, setScrolled] = useState(false);
  const pageRef = useRef(null);
  const sharing = isSharing(s.status);
  const watching = isWatching(s.status);
  const liveCount = s.status.live.hosts.length;
  const asking = (s.status.share.knocks || []).length;
  const titles = { share: t('share.title'), watch: t('watch.title'), settings: t('settings.title'), activity: t('activity.title') };

  useEffect(() => {
    if (pageRef.current) pageRef.current.scrollTop = 0;
    setScrolled(false);
  }, [s.page]);

  const nav = (id, icon, label, extra) => html`
    <button class=${'nav-item' + (s.page === id ? ' active' : '')} onClick=${() => go(id)}>
      <${Icon} name=${icon} /><span class="grow">${label}</span>${extra}
    </button>`;

  return html`<div class="shell">
    <aside class="sidebar">
      <div class="sidebar-top" data-tauri-drag-region></div>
      <div class="brand" data-tauri-drag-region>
        <span class="brand-mark"><${Icon} name="play" /></span><span class="brand-name">HdrCast</span>
      </div>
      <nav class="nav">
        ${nav('share', 'broadcast', t('nav.share'), asking > 0
          ? html`<span class="nav-badge">${asking}</span>`
          : sharing && html`<span class="dot red pulse"></span>`)}
        ${nav('watch', 'play', t('nav.watch'), watching
          ? html`<span class="dot green pulse"></span>`
          : liveCount > 0 && html`<span class="nav-badge">${liveCount}</span>`)}
        <div class="nav-label">${t('nav.more')}</div>
        ${nav('settings', 'sliders', t('nav.settings'))}
        ${nav('activity', 'list', t('nav.activity'))}
      </nav>
      <div class="sidebar-spacer" data-tauri-drag-region></div>
      <div class="group-card">
        <div class="who">
          <${Avatar} name=${s.group.groupName} />
          <div class="names">
            <div class="gname">${s.group.groupName}</div>
            <div class="me">${t('group.as', { name: s.group.displayName })}</div>
          </div>
        </div>
        <div class="actions">
          <button class="btn small" onClick=${copyInvite}><${Icon} name="link" />${t('group.invite')}</button>
        </div>
      </div>
    </aside>
    <main class=${'main' + (scrolled ? ' scrolled' : '')}>
      <div class="titlebar" data-tauri-drag-region>
        <div class="titlebar-title" data-tauri-drag-region>${titles[s.page]}</div>
        <${WindowButtons} />
      </div>
      <div class="page" ref=${pageRef} onScroll=${(e) => setScrolled(e.currentTarget.scrollTop > 30)}>
        <div class="page-inner" key=${s.page}>
          ${s.page === 'share' && html`<${SharePage} />`}
          ${s.page === 'watch' && html`<${WatchPage} />`}
          ${s.page === 'settings' && html`<${SettingsPage} />`}
          ${s.page === 'activity' && html`<${ActivityPage} />`}
        </div>
      </div>
      ${s.page === 'share' && html`<${ShareActionBar} />`}
    </main>
  </div>`;
}

// ------------------------------------------------------------------------------------------------ first run

function Welcome() {
  return html`<div class="shell" style="grid-template-columns:1fr">
    <main class="main">
      <div class="titlebar" data-tauri-drag-region><div class="titlebar-title"></div><${WindowButtons} /></div>
      <div class="welcome">
        <div class="welcome-inner">
          <div class="appicon"><${Icon} name="play" /></div>
          <h1>${t('welcome.title')}</h1>
          <p>${t('welcome.text')}</p>
          <div class="choices">
            <button class="choice" onClick=${pasteInvite}>
              <span class="cicon"><${Icon} name="clipboard" /></span>
              <span class="ctitle">${t('welcome.joinTitle')}</span>
              <span class="ctext">${t('welcome.joinText')}</span>
            </button>
            <button class="choice" onClick=${() => openSheet('create', {})}>
              <span class="cicon"><${Icon} name="people" /></span>
              <span class="ctitle">${t('welcome.createTitle')}</span>
              <span class="ctext">${t('welcome.createText')}</span>
            </button>
          </div>
        </div>
      </div>
    </main>
  </div>`;
}

// ------------------------------------------------------------------------------------------------ sheets

function Sheets({ sheet }) {
  const { name, props } = sheet;
  if (name === 'create') return html`<${CreateSheet} ...${props} />`;
  if (name === 'paste') return html`<${PasteSheet} ...${props} />`;
  if (name === 'join') return html`<${JoinSheet} ...${props} />`;
  if (name === 'invite-found') return html`<${InviteFoundSheet} ...${props} />`;
  if (name === 'problems') return html`<${ProblemsSheet} ...${props} />`;
  if (name === 'firewall') return html`<${FirewallSheet} ...${props} />`;
  if (name === 'add-device') return html`<${AddDeviceSheet} ...${props} />`;
  return null;
}

function useAutoFocus() {
  const ref = useRef(null);
  useEffect(() => {
    if (ref.current) {
      ref.current.focus();
      if (ref.current.select) ref.current.select();
    }
  }, []);
  return ref;
}

function CreateSheet() {
  const s = useStore();
  const [groupName, setGroupName] = useState(s.group.configured ? s.group.groupName : t('group.defaultName'));
  const [me, setMe] = useState(s.group.displayName || s.init.defaultName || '');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const ref = useAutoFocus();
  const submit = async (e) => {
    e.preventDefault();
    if (!groupName.trim() || !me.trim()) return;
    setBusy(true);
    try {
      const group = await invoke('group_create', { groupName: groupName.trim(), displayName: me.trim() });
      setState({ group });
      closeSheet();
      await copyInvite();
    } catch (err) {
      setError(String(err));
    }
    setBusy(false);
  };
  return html`<${Sheet} onClose=${closeSheet}>
    <form onSubmit=${submit}>
      <div class="sicon"><${Icon} name="people" /></div>
      <h2>${t('create.title')}</h2>
      <p class="stext">${s.group.configured ? t('create.replace', { group: s.group.groupName }) : t('create.text')}</p>
      <div class="form">
        <label>${t('create.groupName')}
          <input class="field" ref=${ref} maxlength="40" value=${groupName} onInput=${(e) => setGroupName(e.target.value)} /></label>
        <label>${t('create.myName')}
          <input class="field" maxlength="24" value=${me} onInput=${(e) => setMe(e.target.value)} /></label>
        ${error && html`<div class="error">${error}</div>`}
      </div>
      <div class="sheet-actions">
        <button type="button" class="btn" onClick=${closeSheet}>${t('common.cancel')}</button>
        <button type="submit" class="btn primary" disabled=${busy || !groupName.trim() || !me.trim()}>${t('create.submit')}</button>
      </div>
    </form>
  <//>`;
}

function PasteSheet() {
  const [text, setText] = useState('');
  const [error, setError] = useState('');
  const ref = useAutoFocus();
  const submit = async (e) => {
    e.preventDefault();
    try {
      const preview = await invoke('invite_preview', { text });
      if (!preview.ok) setError(preview.error);
      else {
        closeSheet();
        routeInvite(preview);
      }
    } catch (err) {
      setError(String(err));
    }
  };
  return html`<${Sheet} wide onClose=${closeSheet}>
    <form onSubmit=${submit}>
      <div class="sicon"><${Icon} name="clipboard" /></div>
      <h2>${t('paste.title')}</h2>
      <p class="stext">${t('paste.text')}</p>
      <div class="form">
        <textarea class="field mono" rows="5" ref=${ref} placeholder="hdrcast:…" spellcheck=${false} value=${text}
          onInput=${(e) => { setText(e.target.value); setError(''); }}></textarea>
        ${error && html`<div class="error">${error}</div>`}
      </div>
      <div class="sheet-actions">
        <button type="button" class="btn" onClick=${closeSheet}>${t('common.cancel')}</button>
        <button type="submit" class="btn primary" disabled=${!text.trim()}>${t('common.continue')}</button>
      </div>
    </form>
  <//>`;
}

function JoinSheet({ preview }) {
  const s = useStore();
  const [me, setMe] = useState(preview.defaultName || s.group.displayName || s.init.defaultName || '');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const ref = useAutoFocus();
  const submit = async (e) => {
    e.preventDefault();
    if (!me.trim()) return;
    setBusy(true);
    try {
      const group = await invoke('invite_join', { text: preview.text, displayName: me.trim() });
      setState({ group, page: 'watch' });
      closeSheet();
      toast(t('join.done', { group: group.groupName }), { kind: 'ok' });
    } catch (err) {
      setError(String(err));
    }
    setBusy(false);
  };
  return html`<${Sheet} onClose=${closeSheet}>
    <form onSubmit=${submit}>
      <div class="sicon"><${Icon} name="people" /></div>
      <h2>${t('join.title', { group: preview.groupName })}</h2>
      <p class="stext">${preview.currentGroup ? t('join.switch', { old: preview.currentGroup, group: preview.groupName }) : t('join.text')}</p>
      <div class="form">
        <label>${t('create.myName')}
          <input class="field" ref=${ref} maxlength="24" value=${me} onInput=${(e) => setMe(e.target.value)} /></label>
        ${error && html`<div class="error">${error}</div>`}
      </div>
      <div class="sheet-actions">
        <button type="button" class="btn" onClick=${closeSheet}>${t('common.cancel')}</button>
        <button type="submit" class="btn primary" disabled=${busy || !me.trim()}>${t('join.submit')}</button>
      </div>
    </form>
  <//>`;
}

function InviteFoundSheet({ preview }) {
  const dismiss = () => {
    invoke('invite_dismiss', { fingerprint: preview.fingerprint }).catch(() => {});
    closeSheet();
  };
  return html`<${Sheet} onClose=${dismiss}>
    <div class="sicon"><${Icon} name="clipboard" /></div>
    <h2>${t('found.title')}</h2>
    <p class="stext">${t('found.text', { group: preview.groupName })}</p>
    <div class="sheet-actions">
      <button class="btn" onClick=${dismiss}>${t('found.no')}</button>
      <button class="btn primary" onClick=${() => openSheet('join', { preview })}>${t('found.yes')}</button>
    </div>
  <//>`;
}

function ProblemsSheet({ title, problems }) {
  return html`<${Sheet} wide onClose=${closeSheet}>
    <div class="sicon"><${Icon} name="warning" /></div>
    <h2>${title}</h2>
    ${problems.length === 1
      ? html`<p class="stext" style="white-space:pre-line">${problems[0]}</p>`
      : html`<ul class="problems">${problems.map((p) => html`<li style="white-space:pre-line">${p}</li>`)}</ul>`}
    <div class="sheet-actions"><button class="btn primary" onClick=${closeSheet}>${t('common.ok')}</button></div>
  <//>`;
}

function FirewallSheet({ why, then }) {
  const [busy, setBusy] = useState(false);
  const finish = async (allow) => {
    if (allow) {
      setBusy(true);
      try { setState({ firewall: await invoke('firewall_allow') }); } catch (e) { toast(String(e), { kind: 'warn' }); }
    }
    closeSheet();
    if (then) then();
  };
  return html`<${Sheet} onClose=${() => finish(false)}>
    <div class="sicon"><${Icon} name="shield" /></div>
    <h2>${t('firewall.sheetTitle')}</h2>
    <p class="stext">${t('firewall.sheetText', { why })}</p>
    <div class="sheet-actions">
      <button class="btn" disabled=${busy} onClick=${() => finish(false)}>${t('firewall.notNow')}</button>
      <button class="btn primary" disabled=${busy} onClick=${() => finish(true)}>${t('firewall.allow')}</button>
    </div>
  <//>`;
}

// ------------------------------------------------------------------------------------------------ toasts

function Toasts({ toasts }) {
  const icon = { ok: 'check', warn: 'warning', info: 'info' };
  return html`<div class="toasts" style=${state.group?.configured ? '' : 'left:0'}>
    ${toasts.map((x) => html`<div class="toast" key=${x.id} onClick=${() => { if (!x.actions) dismissToast(x.id); }}>
      <${Icon} name=${icon[x.kind] || 'info'} />
      <span>${x.text}</span>
      ${x.actions && x.actions.map((a) => html`<button class=${'btn small' + (a.primary ? ' primary' : '')}
        onClick=${() => { dismissToast(x.id); a.run(); }}>${a.label}</button>`)}
    </div>`)}
  </div>`;
}

// Things in the status snapshot that want the user's attention once, wherever they are in the app.
const announced = new Map();      // session id -> the toast asking about it
const knocked = new Map();        // session id of a request to watch -> its toast
function watchStatus() {
  // somebody asks to watch this computer
  const knocks = state.status?.share?.knocks || [];
  for (const k of knocks) {
    if (knocked.has(k.sid)) continue;
    knocked.set(k.sid, toast(t('knock.toast', { name: k.name }) + (k.verified ? ` (${k.deviceCode})` : ''), {
      kind: 'info',
      actions: [{ label: t('knock.deny'), run: () => answerKnock(k.sid, false) }, { label: t('knock.allow'), primary: true, run: () => answerKnock(k.sid, true) }],
    }));
  }
  for (const [sid, id] of knocked) {
    if (!knocks.some((k) => k.sid === sid)) {
      dismissToast(id);
      knocked.delete(sid);
    }
  }

  const viewers = state.status?.share?.p2p?.viewers || [];
  for (const v of viewers) {
    if (v.control !== 'requested' || announced.has(v.sid)) continue;
    const answer = (allow) => invoke('control_set', { sid: v.sid, allow }).catch((e) => toast(String(e), { kind: 'warn' }));
    announced.set(v.sid, toast(t('control.request', { name: v.name }), {
      kind: 'info',
      actions: [{ label: t('control.deny'), run: () => answer(false) }, { label: t('control.grant'), primary: true, run: () => answer(true) }],
    }));
  }
  // answered elsewhere (the viewer list), withdrawn, or the viewer left: the question goes away, and may be asked again
  for (const [sid, id] of announced) {
    if (!viewers.some((v) => v.sid === sid && v.control === 'requested')) {
      dismissToast(id);
      announced.delete(sid);
    }
  }
  // Watching failed on IPv4 alone and the host's side is not open: a firewall rule on this side can help.
  const fw = state.firewall;
  if (state.status?.watch?.session?.couldHelp && fw?.applicable && fw.allowed === false && fw.canFix && !state.firewallOfferedWatching && !state.sheet) {
    setState({ firewallOfferedWatching: true });
    openSheet('firewall', { why: t('firewall.whyViewer') });
  }
  // Others can connect to this computer (available, or live) but Windows Firewall only lets replies in. Going live
  // used to be the moment to ask; now nobody has to go live, so ask once per run as soon as the computer is
  // reachable. A copy in another folder is a new program to the firewall, so this comes back when an upgrade is not
  // unzipped over the old folder (the zip's folder is plain HdrCast for that reason).
  const mode = state.status?.share?.mode;
  if ((mode === 'available' || mode === 'live') && fw?.applicable && fw.allowed === false && fw.canFix && !state.firewallOffered && !state.sheet) {
    setState({ firewallOffered: true });
    openSheet('firewall', { why: t('firewall.whyHost') });
  }
}

window.addEventListener('contextmenu', (e) => {
  const el = e.target;
  const editable = el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || (el.closest && el.closest('.logbox, .stext, .hero-sub'));
  if (!editable) e.preventDefault();
});

// Windows: when the window comes forward, the backend looks at the clipboard for an invite (and says so by event).
window.addEventListener('focus', () => {
  if (state.ready) invoke('app_focused').catch(() => {});
});

render(html`<${App} />`, document.getElementById('app'));
boot()
  .then(() => {
    setInterval(watchStatus, 500);
  })
  .catch((e) => {
    document.getElementById('app').textContent = 'HdrCast could not start: ' + e;
  })
  .finally(() => {
    // started at sign-in: the window stays hidden until it is opened from the notification area
    if (!state.init?.startHidden) requestAnimationFrame(() => win.show());
  });
