import { html, useState } from '../vendor/preact-htm.js';
import { invoke } from '../api.js';
import { t } from '../i18n.js';
import { Icon } from '../icons.js';
import { useStore, editSettings, flushSettings, openSheet, toast, setState, state } from '../store.js';
import { Avatar, Empty, NumberField, Row, Section, Segmented, Select, Switch, formatDuration } from '../components.js';

export function isWatching(status) {
  return !!status?.watch?.active;
}

async function startWatch(hostId) {
  await flushSettings();
  try {
    const r = await invoke('watch_start', { hostId });
    if (!r.ok) openSheet('problems', { title: t('watch.cannotStart'), problems: r.problems });
  } catch (e) {
    openSheet('problems', { title: t('watch.cannotStart'), problems: [String(e)] });
  }
}

async function stopWatch() {
  try { await invoke('watch_stop'); } catch (e) { toast(String(e), { kind: 'warn' }); }
}

function wantControl(want) {
  invoke('watch_control', { want }).catch((e) => toast(String(e), { kind: 'warn' }));
}

function liveFor(sinceMs, onDemand) {
  const minutes = Math.max(0, Math.floor((Date.now() - sinceMs) / 60000));
  const prefix = onDemand ? 'watch.online' : 'watch.live';
  if (minutes < 60) return t(prefix + 'Min', { n: minutes });
  return t(prefix + 'Hours', { h: Math.floor(minutes / 60), m: minutes % 60 });
}

export function WatchPage() {
  const s = useStore();
  const { receiver } = s.settings;
  const status = s.status;
  const watching = isWatching(status);
  const live = status.live;
  const presets = s.init.latencyPresets;
  const set = (patch) => editSettings('receiver', patch);
  const [custom, setCustom] = useState(false);
  const presetValue = !custom && presets.some((p) => p.latencyMs === receiver.latencyMs) ? receiver.latencyMs : -1;

  const refresh = () => invoke('live_refresh').catch(() => {});

  return html`
    <div class="page-head">
      <div class="titles">
        <h1>${t('watch.title')}</h1>
        <div class="subtitle">${t('watch.subtitle', { group: s.group.groupName })}</div>
      </div>
    </div>

    ${watching && html`<${NowPlaying} watch=${status.watch} />`}

    <div class="section-title" style=${watching ? '' : 'margin-top:0'}>
      <span>${t('watch.liveNow')}</span><span class="spacer"></span>
      <button class="btn small plain" onClick=${refresh}><${Icon} name="refresh" />${t('common.refresh')}</button>
    </div>
    <div class="card rows">
      ${live.error
        ? html`<${Empty} icon="warning" title=${t('watch.cannotReach')} text=${live.error} />`
        : live.hosts.length === 0
          ? html`<${Empty} icon="play" title=${live.loaded ? t('watch.nobody') : t('watch.looking')}
              text=${live.loaded ? t('watch.nobodyText', { group: s.group.groupName }) : ''} />`
          : live.hosts.map((h) => {
              const mine = watching && status.watch.hostId === h.hostId;
              const full = h.maxViewers > 0 && h.viewers >= h.maxViewers && !mine;
              return html`<div class="person row" key=${h.hostId}>
                <${Avatar} name=${h.name} size="large" />
                <div class="who">
                  <div class="pname" style="font-size:14px">${h.name}${h.deviceCode && html` <span class="code">${h.deviceCode}</span>`}</div>
                  <div class="pmeta">
                    ${h.onDemand
                      ? html`<span class="badge">${t('watch.onlineBadge')}</span>`
                      : html`<span class="badge red"><span class="dot red pulse" style="width:6px;height:6px"></span>LIVE</span>`}
                    <span>${liveFor(h.sinceMs, h.onDemand)}</span>
                    ${(!h.onDemand || h.viewers > 0) && html`<span>· ${t('watch.viewers', { n: h.viewers, max: h.maxViewers })}</span>`}
                    ${h.control && html`<span class="badge"><${Icon} name="cursor" style="width:11px;height:11px" />${t('control.offered')}</span>`}
                  </div>
                </div>
                <div class="pactions">
                  ${mine
                    ? html`<span class="badge green">${t('watch.watchingNow')}</span>`
                    : html`<button class=${'btn' + (h.onDemand ? '' : ' primary')} disabled=${watching || full} onClick=${() => startWatch(h.hostId)}>
                        <${Icon} name=${h.onDemand ? 'link' : 'play'} />${full ? t('watch.full') : h.onDemand ? t('watch.connect') : t('watch.watch')}</button>`}
                </div>
              </div>`;
            })}
    </div>
    ${live.hosts.some((h) => h.onDemand) && html`<div class="section-note">${t('watch.onDemandNote')}</div>`}

    <${Section} title=${t('watch.connection')} note=${t('watch.latencyNote')}>
      <${Row} name=${t('watch.latency')} hint=${t('watch.latencyHint')} disabled=${watching}>
        <${Segmented} value=${presetValue} onChange=${(v) => { setCustom(v < 0); if (v > 0) set({ latencyMs: v }); }}
          options=${presets.map((p) => ({ value: p.latencyMs, label: t('preset.' + p.name, null, p.name) })).concat([{ value: -1, label: t('watch.custom') }])} />
      <//>
      ${presetValue === -1 && html`<${Row} name=${t('watch.latencyCustom')} disabled=${watching}>
        <${NumberField} value=${receiver.latencyMs} min=${20} max=${30000} onChange=${(v) => set({ latencyMs: v })} />
        <span class="unit">ms</span>
      <//>`}
    <//>

    <${Section} title=${t('watch.picture')}>
      <${Row} name=${t('watch.displayMode')} hint=${displayHint(receiver.displayMode, s.init.platform)} disabled=${watching}>
        <${Segmented} value=${receiver.displayMode} onChange=${(v) => set({ displayMode: v })}
          options=${[{ value: 0, label: t('watch.dmAuto') }, { value: 1, label: 'HDR' }, { value: 2, label: t('watch.dmSdr') }]} />
      <//>
      ${receiver.displayMode === 2 && html`<${Row} name=${t('watch.toneMapping')} disabled=${watching}>
        <${Select} value=${receiver.toneMapping} onChange=${(v) => set({ toneMapping: v })}
          options=${s.init.toneMappings.map((v) => ({ value: v, label: v }))} />
      <//>`}
      <${Row} name=${t('watch.fullscreen')} disabled=${watching}>
        <${Switch} on=${receiver.fullscreen} onChange=${(v) => set({ fullscreen: v })} />
      <//>
    <//>
    <div style="height:8px"></div>`;
}

function displayHint(mode, platform) {
  if (mode === 1) return platform === 'macos' ? t('watch.dmHdrHintMac') : t('watch.dmHdrHint');
  if (mode === 2) return t('watch.dmSdrHint');
  return t('watch.dmAutoHint');
}

// "IPv6 [2400:…]:51234, ping 38 ms" -> "IPv6 · ping 38 ms" (the full address is in the line above)
function shortPath(path) {
  if (!path) return ' ';
  const ping = /ping [\d.]+ ms/.exec(path);
  return path.split(' ')[0] + (ping ? ' · ' + ping[0] : '');
}

function pillOf(session, playing) {
  switch (session?.state) {
    case 'connected': return playing ? t('watch.pillPlaying') : t('watch.pillConnected');
    case 'waiting': return t('watch.pillWaiting');
    case 'approval': return t('watch.pillApproval');
    case 'refused': return t('watch.pillRefused');
    default: return t('watch.pillConnecting');
  }
}

/** What remote control is doing in this session, and the one thing the user can do about it. */
function ControlLine({ watch }) {
  const c = watch.control;
  if (!c || c === 'off') return null;
  const button = {
    offered: html`<button class="btn" onClick=${() => wantControl(true)}><${Icon} name="cursor" />${t('control.request.btn')}</button>`,
    allowed: html`<button class="btn" onClick=${() => wantControl(true)}><${Icon} name="cursor" />${t('control.take')}</button>`,
    requested: html`<button class="btn" onClick=${() => wantControl(false)}>${t('common.cancel')}</button>`,
    granted: html`<button class="btn" onClick=${() => wantControl(false)}>${t('control.release')}</button>`,
  }[c];
  return html`<div class="control-line">
    <span class="what"><${Icon} name="cursor" /> ${t('control.state.' + c, { host: watch.hostName })}</span>
    ${button}
  </div>`;
}

function NowPlaying({ watch }) {
  const session = watch.session;
  const player = watch.player;
  const playing = player?.state === 'playing';
  const refused = session?.state === 'refused';
  const cls = 'hero';
  const pill = pillOf(session, playing);
  return html`<div class=${cls}>
    <div class="hero-top">
      <${Avatar} name=${watch.hostName || '?'} size="large" />
      <div class="titles">
        <div class="hero-title">${watch.hostName || t('watch.pillConnecting')}${watch.hostDeviceCode && html` <span class="code">${watch.hostDeviceCode}</span>`}</div>
        <div class="hero-sub">${session?.state === 'approval' ? t('watch.approvalSub', { host: watch.hostName }) : session?.status || ''}</div>
      </div>
      <span class="live-pill"><span class=${'dot' + (playing ? ' pulse' : '')}></span>${pill}${playing && player ? ' ' + formatDuration(player.uptimeSec) : ''}</span>
      ${!refused && html`<button class="btn" disabled=${!player || player.state === 'idle'} onClick=${() => invoke('watch_fullscreen').catch(() => {})}>
        <${Icon} name="expand" />${t('watch.fullscreenBtn')}</button>`}
      <button class="btn solid" onClick=${stopWatch}>${refused ? t('common.close') : html`<${Icon} name="stop" />${t('watch.stop')}`}</button>
    </div>
    ${!refused && html`<div class="tiles">
      <div class="tile"><div class="k">${t('watch.tBuffer')}</div>
        <div class="v">${player?.cacheSeconds != null ? player.cacheSeconds.toFixed(1) : '—'}<small>${t('unit.s')}</small></div>
        <div class="s">${player?.text || ' '}</div></div>
      <div class="tile"><div class="k">${t('watch.tFps')}</div>
        <div class="v">${player?.fps != null ? player.fps.toFixed(1) : '—'}<small>fps</small></div>
        <div class="s">${player?.hwdec || ' '}</div></div>
      <div class="tile"><div class="k">${t('watch.tBitrate')}</div>
        <div class="v">${player?.bitrateMbps != null ? player.bitrateMbps.toFixed(1) : '—'}<small>Mbps</small></div>
        <div class="s">${shortPath(session?.path)}</div></div>
      <div class="tile"><div class="k">${t('watch.tDropped')}</div>
        <div class="v">${player ? player.drops + player.decoderDrops : '—'}</div>
        <div class="s">${player?.hdr ? t('watch.hdrActive') : (player?.output || ' ')}</div></div>
    </div>`}
    ${!refused && html`<${ControlLine} watch=${watch} />`}
    ${!refused && player?.source && html`<div class="hero-sub" style="margin-top:-6px">${player.source}</div>`}
  </div>`;
}
