// The panel over the player (docs/design.md §3.3). Folded, it is a small round button on the picture; opened, it
// shows what the stream is doing and the three things a viewer can change while watching: the decoder and the sound
// at once, the delay by joining the host again (the delay is agreed on when joining). Resolution, frame rate and
// bitrate are the host's to set. It is a small window of its own: the backend keeps it on the player's window and
// sizes it (to the button, or to the height this page measures), and both can be dragged.

import { html, render, useEffect, useRef, useState } from './vendor/preact-htm.js';
import { invoke, listen, win } from './api.js';
import { setLanguage, t } from './i18n.js';
import { Icon } from './icons.js';
import { NumberField, formatDuration } from './components.js';

const view = {
  init: null,          // the lists from overlay_init: latencyPresets, hwDecModes
  expanded: false,     // the panel is open (the backend decides: the button, Ctrl+Alt+O, the Watch page)
  status: null,        // the latest status snapshot (the same one the main window gets)
  receiver: null,      // the viewer's settings as saved
  rejoining: false,    // a new delay is being taken: the session is gone for a moment
};
const listeners = new Set();

function update(patch) {
  Object.assign(view, patch);
  for (const l of listeners) l();
}

function useView() {
  const [, force] = useState(0);
  useEffect(() => {
    const l = () => force((n) => n + 1);
    listeners.add(l);
    return () => listeners.delete(l);
  }, []);
  return view;
}

/** Opens the panel, or folds it (and the keyboard goes back to the player). */
const expand = (expanded) => {
  update({ expanded });
  invoke('overlay_expand', { expanded }).catch(() => {});
};

const fixed = (v, digits) => (v != null ? v.toFixed(digits) : '—');

function pillOf(session) {
  switch (session?.state) {
    case 'connected': return t('watch.pillConnected');
    case 'waiting': return t('watch.pillWaiting');
    case 'approval': return t('watch.pillApproval');
    case 'refused': return t('watch.pillRefused');
    default: return t('watch.pillConnecting');
  }
}

/** "3840×2160 · 60 fps · HDR": what arrives from the host. */
function sourceLine(player, session) {
  if (player?.state !== 'playing') return session?.status || ' ';
  const parts = [];
  if (player.width && player.height) parts.push(`${player.width}×${player.height}`);
  if (player.fps != null) parts.push(`${Math.round(player.fps)} fps`);
  parts.push(player.hdr ? 'HDR' : 'SDR');
  return parts.join(' · ');
}

/** Folded: a click opens the panel, a drag moves the button (the system moves the window). */
function Bubble() {
  const press = useRef(null);
  const down = (e) => {
    if (e.button === 0) press.current = { x: e.screenX, y: e.screenY };
  };
  const move = (e) => {
    const p = press.current;
    if (p && (e.buttons & 1) && Math.hypot(e.screenX - p.x, e.screenY - p.y) > 3) {
      press.current = null;
      win.startDragging();
    }
  };
  const up = () => {
    if (press.current) {
      press.current = null;
      expand(true);
    }
  };
  return html`<button class="ov-bubble" title=${t('overlay.bubbleHint')}
    onPointerDown=${down} onPointerMove=${move} onPointerUp=${up} onPointerLeave=${() => { press.current = null; }}>
    <${Icon} name="sliders" />
  </button>`;
}

function Stat({ k, v, unit }) {
  return html`<div class="ov-stat"><div class="k">${k}</div><div class="v">${v}${unit && html`<small>${unit}</small>`}</div></div>`;
}

function Head({ title, time }) {
  return html`<div class="ov-head" data-tauri-drag-region>
    <span class="ov-host" data-tauri-drag-region>${title}</span>
    <span class="ov-time" data-tauri-drag-region>${time}</span>
    <button class="ov-x" title=${t('overlay.collapse')} onClick=${() => expand(false)}><${Icon} name="minus" /></button>
  </div>`;
}

function Panel() {
  const v = useView();
  const root = useRef(null);
  // the window takes the panel's height
  useEffect(() => {
    const el = root.current;
    if (!el) return undefined;
    let last = 0;
    const ro = new ResizeObserver(() => {
      const height = Math.ceil(el.getBoundingClientRect().height);
      if (height !== last) {
        last = height;
        invoke('overlay_fit', { height }).catch(() => {});
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const w = v.status?.watch;
  let body;
  if (w?.active) body = html`<${Watching} w=${w} />`;
  else if (v.rejoining) body = html`<${Head} title=${t('overlay.rejoining')} time="" /><div class="ov-source">${t('overlay.rejoiningText')}</div>`;
  else body = html`<${Head} title="HdrCast" time="" /><div class="ov-source">${t('overlay.notWatching')}</div>`;
  return html`<div class="ov" ref=${root}>${body}</div>`;
}

function Watching({ w }) {
  const session = w.session;
  const player = w.player;
  const playing = player?.state === 'playing';
  const net = session?.net;
  const decoding = player?.hwdec === 'no' ? t('overlay.software') : player?.hwdec || '—';
  const toggleMute = () => {
    update({ status: { ...view.status, watch: { ...w, muted: !w.muted } } });
    invoke('watch_set_mute', { muted: !w.muted }).catch(() => {});
  };
  return html`
    <${Head} title=${html`<span class=${'dot' + (playing ? ' red pulse' : '')}></span>${w.hostName || t('watch.pillConnecting')}`}
      time=${playing ? formatDuration(player.uptimeSec) : pillOf(session)} />
    <div class="ov-source">${sourceLine(player, session)}</div>
    ${w.lowLatency
      ? html`<div class="ov-stats">
          <${Stat} k=${t('watch.tBitrate')} v=${fixed(w.ll?.mbps, 1)} unit="Mbps" />
          <${Stat} k=${t('overlay.host')} v=${fixed(w.ll?.hostMs, 1)} unit="ms" />
          <${Stat} k=${t('overlay.ping')} v=${fixed(w.ll?.rttMs, 1)} unit="ms" />
          <${Stat} k=${t('overlay.lost')} v=${w.ll ? w.ll.lost : '—'} />
          <${Stat} k=${t('overlay.restored')} v=${w.ll ? w.ll.restored : '—'} />
          <${Stat} k=${t('overlay.decoding')} v=${decoding} />
        </div>
        ${w.ll?.waiting && w.ll.shown > 0 && html`<div class="ov-guard">${t('watch.llWaiting')}</div>`}
        <div class="ov-note">${t('overlay.controlMode')}</div>`
      : html`<div class="ov-stats">
          <${Stat} k=${t('watch.tBitrate')} v=${fixed(player?.bitrateMbps, 1)} unit="Mbps" />
          <${Stat} k=${t('watch.tBuffer')} v=${fixed(player?.cacheSeconds, 1)} unit=${t('unit.s')} />
          <${Stat} k=${t('overlay.ping')} v=${net ? Math.round(net.rttMs) : '—'} unit="ms" />
          <${Stat} k=${t('overlay.loss')} v=${net ? net.lossPercent.toFixed(1) : '—'} unit="%" />
          <${Stat} k=${t('watch.tDropped')} v=${player ? player.drops + player.decoderDrops : '—'} />
          <${Stat} k=${t('overlay.decoding')} v=${decoding} />
        </div>
        ${player?.guard?.frozen
          ? html`<div class="ov-guard">${t('watch.guardFrozen')}</div>`
          : player?.guard?.losses > 0 && html`<div class="ov-guard">${t('watch.guardSummary', { losses: player.guard.losses, held: player.guard.heldBack, freezes: player.guard.freezes })}</div>`}
        <${Delay} w=${w} />`}
    <${Decoder} />
    <div class="ov-foot">
      <button class="btn small" disabled=${!player || player.state === 'idle'} onClick=${() => invoke('watch_fullscreen').catch(() => {})}>
        <${Icon} name="expand" />${player?.fullscreen ? t('overlay.exitFullscreen') : t('watch.fullscreenBtn')}</button>
      <button class="btn small" aria-pressed=${w.muted ? 'true' : 'false'} onClick=${toggleMute}>
        <${Icon} name=${w.muted ? 'muted' : 'speaker'} />${w.muted ? t('overlay.unmute') : t('overlay.mute')}</button>
    </div>`;
}

/** The delay: chosen here, taken by joining the host again (Apply). */
function Delay({ w }) {
  const v = useView();
  const inUse = w.latencyMs;
  const [want, setWant] = useState(null);
  const [problem, setProblem] = useState(null);
  const value = want ?? inUse;
  const busy = v.rejoining;
  const apply = async () => {
    setProblem(null);
    update({ rejoining: true });
    try {
      const r = await invoke('watch_set_latency', { latencyMs: value });
      if (r.ok) setWant(null);
      else setProblem((r.problems || []).join(' '));
    } catch (e) {
      setProblem(String(e));
    }
    update({ rejoining: false });
  };
  return html`<div class="ov-section">
    <div class="ov-label"><span>${t('watch.latency')}</span><span class="spacer"></span>
      <span class="ov-now">${t('overlay.now', { v: inUse + ' ms' })}</span></div>
    <div class="ov-chips four">
      ${v.init.latencyPresets.map((p) => html`<button class=${'ov-chip' + (p.latencyMs === value ? ' on' : '')} disabled=${busy}
        onClick=${() => setWant(p.latencyMs)}>${t('preset.' + p.name, null, p.name)}<small>${p.latencyMs}</small></button>`)}
    </div>
    <div class="ov-row">
      <${NumberField} value=${value} min=${20} max=${30000} width=${80} disabled=${busy} onChange=${setWant} />
      <span class="unit">ms</span>
      <span class="spacer"></span>
      <button class="btn small primary" disabled=${busy || value === inUse} onClick=${apply}>
        ${busy ? t('overlay.rejoining') : t('overlay.apply')}</button>
    </div>
    ${(problem || value !== inUse) && html`<div class=${'ov-note' + (problem ? ' warn' : '')}>${problem || t('overlay.delayNote')}</div>`}
  </div>`;
}

/** The decoder: switched in the running player at once. */
function Decoder() {
  const v = useView();
  const current = v.receiver.hwDec;
  const choose = async (id) => {
    if (id === current) return;
    const before = v.receiver;
    update({ receiver: { ...before, hwDec: id } });
    try {
      update({ receiver: await invoke('watch_set_hwdec', { hwDec: id }) });
    } catch (e) {
      update({ receiver: before });
    }
  };
  return html`<div class="ov-section">
    <div class="ov-label"><span>${t('overlay.decoder')}</span><span class="spacer"></span>
      <span class="ov-now">${t('overlay.decoderNote')}</span></div>
    <div class="ov-chips two">
      ${v.init.hwDecModes.map((m) => html`<button class=${'ov-chip' + (m.id === current ? ' on' : '')} onClick=${() => choose(m.id)}>${m.name}</button>`)}
    </div>
  </div>`;
}

function Root() {
  const v = useView();
  return v.expanded ? html`<${Panel} />` : html`<${Bubble} />`;
}

window.addEventListener('keydown', (e) => {
  if (e.ctrlKey && e.altKey && e.key.toLowerCase() === 'o') {
    e.preventDefault();
    expand(!view.expanded);
  } else if (e.key === 'Escape' && view.expanded) {
    e.preventDefault();
    expand(false);
  }
});

window.addEventListener('contextmenu', (e) => {
  if (!(e.target instanceof HTMLInputElement)) e.preventDefault();
});

async function boot() {
  const init = await invoke('overlay_init');
  setLanguage(init.ui?.language || 'auto', navigator.language);
  const root = document.documentElement;
  root.lang = root.dataset.lang || 'en';
  root.classList.add(init.platform === 'macos' ? 'mac' : init.platform === 'windows' ? 'win' : 'linux');
  update({ init, expanded: !!init.expanded, status: init.status, receiver: init.receiver });
  listen('status', (status) => update({ status }));
  listen('receiver', (receiver) => update({ receiver }));
  listen('overlay', (payload) => update({ expanded: !!payload.expanded }));
  render(html`<${Root} />`, document.getElementById('overlay'));
}

boot().catch((e) => {
  document.getElementById('overlay').textContent = 'HdrCast: ' + e;
});
