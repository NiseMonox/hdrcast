import { html, useState } from '../vendor/preact-htm.js';
import { invoke } from '../api.js';
import { t } from '../i18n.js';
import { Icon } from '../icons.js';
import { useStore, editSettings, flushSettings, openSheet, closeSheet, toast, setState, state } from '../store.js';
import { Avatar, Empty, NumberField, Row, Section, Segmented, Select, Sheet, Switch, formatDuration } from '../components.js';
import { bitrateChoices, bitrateNote, ceilingOf, mbpsText } from '../bitrate.js';

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

/** The resend headroom the viewer asks for when it is not set: by the delay, as the app does. */
function autoOverhead(latencyMs) {
  return latencyMs >= 3000 ? 50 : 25;
}

/** Seconds behind live: the delay, the player's buffer and about 0.8 s of capturing, encoding, decoding and showing
 * (measured against a clock on the screen: 1.19 s for 90 ms and 0.3 s with the host's Balanced quality, 0.84 s with
 * Speed; docs/network.md §3). */
function behindLive(latencyMs, bufferSeconds) {
  return (latencyMs / 1000 + bufferSeconds + 0.8).toFixed(1);
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
  const probe = status.probe || { run: null, results: {} };
  const overheads = [0, 25, 50, 100].concat([25, 50, 100].includes(receiver.overheadPercent) || !receiver.overheadPercent ? [] : [receiver.overheadPercent]);

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
                    ${h.lowLatency && html`<span class="badge">${t('watch.lowLatencyBadge')}</span>`}
                  </div>
                </div>
                <div class="pactions">
                  ${!receiver.controlMode && html`<${ProbeButton} host=${h} probe=${probe} blocked=${watching || full} />`}
                  ${mine
                    ? html`<span class="badge green">${t('watch.watchingNow')}</span>`
                    : receiver.controlMode && h.lowLatency
                      ? html`<button class="btn primary" disabled=${watching || full} onClick=${() => startWatch(h.hostId)}>
                          <${Icon} name="cursor" />${full ? t('watch.full') : t('watch.controlBtn')}</button>`
                      : html`<button class=${'btn' + (h.onDemand ? '' : ' primary')} disabled=${watching || full} onClick=${() => startWatch(h.hostId)}>
                          <${Icon} name=${h.onDemand ? 'link' : 'play'} />${full ? t('watch.full') : h.onDemand ? t('watch.connect') : t('watch.watch')}</button>`}
                </div>
              </div>`;
            })}
    </div>
    ${live.hosts.some((h) => h.onDemand) && html`<div class="section-note">${t('watch.onDemandNote')}</div>`}

    <${Section} title=${t('watch.connection')}
      note=${receiver.controlMode ? t('watch.controlNote') : watching ? t('watch.latencyNoteWatching') : t('watch.latencyNote')}>
      <${Row} name=${t('watch.mode')} hint=${receiver.controlMode ? t('watch.modeControlHint') : t('watch.modeWatchHint')} disabled=${watching}>
        <${Segmented} value=${!!receiver.controlMode} onChange=${(v) => set({ controlMode: v })}
          options=${[{ value: false, label: t('watch.modeWatch') }, { value: true, label: t('watch.modeControl') }]} />
      <//>
      ${!receiver.controlMode && html`<${Row} name=${t('watch.latency')} hint=${t('watch.latencyHint')} disabled=${watching}>
        <${Segmented} value=${presetValue} onChange=${(v) => {
            setCustom(v < 0);
            const preset = presets.find((p) => p.latencyMs === v);
            if (preset) set({ latencyMs: preset.latencyMs, bufferSeconds: preset.bufferSeconds });
          }}
          options=${presets.map((p) => ({ value: p.latencyMs, label: t('preset.' + p.name, null, p.name) })).concat([{ value: -1, label: t('watch.custom') }])} />
      <//>`}
      ${!receiver.controlMode && presetValue === -1 && html`<${Row} name=${t('watch.latencyCustom')} disabled=${watching}>
        <${NumberField} value=${receiver.latencyMs} min=${20} max=${30000} onChange=${(v) => set({ latencyMs: v })} />
        <span class="unit">ms</span>
      <//>`}
      ${!receiver.controlMode && html`<${Row} name=${t('watch.buffer')} hint=${t('watch.bufferHint')} disabled=${watching}>
        <${NumberField} value=${receiver.bufferSeconds} min=${0.1} max=${30} step=${0.1} onChange=${(v) => set({ bufferSeconds: v })} />
        <span class="unit">${t('unit.s')}</span>
      <//>`}
      ${!receiver.controlMode && html`<${Row} name=${t('watch.overhead')} hint=${t('watch.overheadHint')} disabled=${watching}>
        <${Segmented} value=${receiver.overheadPercent || 0} onChange=${(v) => set({ overheadPercent: v })}
          options=${overheads.map((n) => ({ value: n, label: n ? n + ' %' : t('watch.overheadAuto', { n: autoOverhead(receiver.latencyMs) }) }))} />
      <//>`}
      ${!receiver.controlMode && html`<${Row} name=${t('watch.behind')} hint=${t('watch.behindHint')}>
        <span class="value-text">${t('watch.behindValue', { s: behindLive(receiver.latencyMs, receiver.bufferSeconds) })}</span>
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
function shortPath(path, net) {
  if (!path) return ' ';
  const ping = /ping [\d.]+ ms/.exec(path);
  const loss = net?.lossPercent >= 0.05 ? ' · ' + t('watch.loss', { p: net.lossPercent.toFixed(1) }) : '';
  return path.split(' ')[0] + (ping ? ' · ' + ping[0] : '') + loss;
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

/** The most this viewer asks the host's stream to carry (docs/design.md §8.13), and what the host answered. Kept for
 *  this host: every later join asks for it too. A host before 0.3.10 takes no such requests. */
function BitrateLine({ watch }) {
  const b = watch.bitrate;
  if (!b || watch.session?.state !== 'connected') return null;
  if (!b.can) return html`<div class="control-line"><span class="what"><${Icon} name="gauge" /> ${t('watch.brOld')}</span></div>`;
  const ceiling = ceilingOf(b);
  const wanted = b.link?.wantedKbps || 0;
  const choose = (kbps) => {
    if (kbps === wanted) return;
    // shown at once (the next status says the same)
    const w = state.status.watch;
    setState({ status: { ...state.status, watch: { ...w, bitrate: { ...w.bitrate, link: { ...w.bitrate.link, wantedKbps: kbps, waiting: true } } } } });
    invoke('watch_set_bitrate', { kbps }).catch((e) => toast(String(e), { kind: 'warn' }));
  };
  return html`<div class="control-line">
    <span class="what" title=${t('watch.brHint')}><${Icon} name="gauge" /> ${bitrateNote(b)}</span>
    <span class="unit">${t('watch.bitrateLimit')}</span>
    <${Select} value=${wanted} onChange=${choose}
      options=${bitrateChoices(ceiling, wanted).map((kbps) => ({
        value: kbps,
        label: kbps ? `${mbpsText(kbps)} Mbps` : t('watch.brHost', { mbps: ceiling ? mbpsText(ceiling) : '—' }),
      }))} />
  </div>`;
}

/** What the guard did with pictures damaged on the way (null when mpv receives the stream itself). */
function GuardLine({ guard }) {
  if (!guard) return null;
  if (guard.frozen) return html`<div class="hero-sub" style="margin-top:-6px">${t('watch.guardFrozen')}</div>`;
  if (!guard.losses && !guard.heldBack) return null;
  return html`<div class="hero-sub" style="margin-top:-6px">
    ${t('watch.guardSummary', { losses: guard.losses, held: guard.heldBack, freezes: guard.freezes })}</div>`;
}

/** Control mode (docs/design.md §11): what the host and the path add to the picture, and what was lost on the way. */
function ControlModeTiles({ watch }) {
  const ll = watch.ll;
  const player = watch.player;
  const session = watch.session;
  const delay = ll && ll.rttMs != null ? ll.hostMs + ll.rttMs / 2 : null;
  return html`<div class="tiles">
      <div class="tile"><div class="k">${t('watch.tDelay')}</div>
        <div class="v">${delay != null ? delay.toFixed(0) : '—'}<small>ms</small></div>
        <div class="s">${ll ? t('watch.llDelaySub', { host: ll.hostMs.toFixed(1), rtt: ll.rttMs != null ? ll.rttMs.toFixed(1) : '—' }) : ' '}</div></div>
      <div class="tile"><div class="k">${t('watch.tFps')}</div>
        <div class="v">${ll ? ll.fps.toFixed(0) : '—'}<small>fps</small></div>
        <div class="s">${player?.hwdec || ' '}</div></div>
      <div class="tile"><div class="k">${t('watch.tBitrate')}</div>
        <div class="v">${ll ? ll.mbps.toFixed(1) : '—'}<small>Mbps</small></div>
        <div class="s">${shortPath(session?.path, null)}</div></div>
      <div class="tile"><div class="k">${t('watch.tLost')}</div>
        <div class="v">${ll ? ll.lost : '—'}</div>
        <div class="s">${ll ? t('watch.llRestored', { n: ll.restored }) : ' '}</div></div>
    </div>
    <div class="hero-sub" style="margin-top:-6px">${ll?.waiting && ll.shown > 0 ? t('watch.llWaiting') : t('watch.llMode')}</div>`;
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
      ${!refused && html`<button class="btn" title=${t('watch.overlayHint')} aria-pressed=${watch.overlay ? 'true' : 'false'}
        onClick=${() => invoke('overlay_toggle').catch(() => {})}><${Icon} name="sliders" />${t('watch.overlay')}</button>`}
      ${!refused && html`<button class="btn" disabled=${!player || player.state === 'idle'} onClick=${() => invoke('watch_fullscreen').catch(() => {})}>
        <${Icon} name="expand" />${t('watch.fullscreenBtn')}</button>`}
      <button class="btn solid" onClick=${stopWatch}>${refused ? t('common.close') : html`<${Icon} name="stop" />${t('watch.stop')}`}</button>
    </div>
    ${!refused && watch.lowLatency && html`<${ControlModeTiles} watch=${watch} />`}
    ${!refused && !watch.lowLatency && html`<div class="tiles">
      <div class="tile"><div class="k">${t('watch.tBuffer')}</div>
        <div class="v">${player?.cacheSeconds != null ? player.cacheSeconds.toFixed(1) : '—'}<small>${t('unit.s')}</small></div>
        <div class="s">${player?.text || ' '}</div></div>
      <div class="tile"><div class="k">${t('watch.tFps')}</div>
        <div class="v">${player?.fps != null ? player.fps.toFixed(1) : '—'}<small>fps</small></div>
        <div class="s">${player?.hwdec || ' '}</div></div>
      <div class="tile"><div class="k">${t('watch.tBitrate')}</div>
        <div class="v">${player?.bitrateMbps != null ? player.bitrateMbps.toFixed(1) : '—'}<small>Mbps</small></div>
        <div class="s">${shortPath(session?.path, session?.net)}</div></div>
      <div class="tile"><div class="k">${t('watch.tDropped')}</div>
        <div class="v">${player ? player.drops + player.decoderDrops : '—'}</div>
        <div class="s">${player?.hdr ? t('watch.hdrActive') : (player?.output || ' ')}</div></div>
    </div>`}
    ${!refused && html`<${ControlLine} watch=${watch} />`}
    ${!refused && html`<${BitrateLine} watch=${watch} />`}
    ${!refused && html`<${GuardLine} guard=${player?.guard} />`}
    ${!refused && player?.source && html`<div class="hero-sub" style="margin-top:-6px">${player.source}</div>`}
  </div>`;
}

// ------------------------------------------------------------------------------------------------ connection test

/** The test's button on a host's row: what it found, or how far it is. Blocked: watching, or the host is full (the
 * test takes a viewer's place); a result can still be looked at. */
function ProbeButton({ host, probe, blocked }) {
  const run = probe.run;
  const mine = run && run.hostId === host.hostId;
  const result = probe.results?.[host.hostId];
  const label = mine && run.active
    ? (run.secondsLeft != null ? t('probe.btnLeft', { s: Math.ceil(run.secondsLeft) }) : t('probe.btnRunning'))
    : result ? t('probe.btnResult', { ms: result.latencyMs }) : t('probe.btn');
  return html`<button class="btn plain" title=${t('probe.btnHint')} disabled=${(run?.active && !mine) || (blocked && !result && !mine)}
    onClick=${() => openSheet('probe', { hostId: host.hostId, hostName: host.name })}>
    <${Icon} name="gauge" />${label}</button>`;
}

function fmt(n, digits) {
  return n == null ? '—' : Number(n).toFixed(digits);
}

export function ProbeSheet({ hostId, hostName }) {
  const s = useStore();
  const probe = s.status.probe || { run: null, results: {} };
  const run = probe.run && probe.run.hostId === hostId ? probe.run : null;
  const result = probe.results?.[hostId];
  const watching = isWatching(s.status);
  const elsewhere = probe.run?.active && probe.run.hostId !== hostId;
  const { receiver } = s.settings;
  const name = result?.hostName || run?.hostName || hostName;
  const start = async () => {
    await flushSettings();
    try {
      const r = await invoke('probe_start', { hostId });
      if (!r.ok) toast(r.problems.join('\n'), { kind: 'warn' });
    } catch (e) {
      toast(String(e), { kind: 'warn' });
    }
  };
  const stop = () => invoke('probe_stop').catch(() => {});
  const apply = () => {
    const overheadPercent = result.overheadPercent <= autoOverhead(result.latencyMs) ? 0 : result.overheadPercent;
    editSettings('receiver', { latencyMs: result.latencyMs, bufferSeconds: result.bufferSeconds, overheadPercent });
    toast(t('probe.applied', { ms: result.latencyMs, s: result.bufferSeconds }), { kind: 'ok' });
    closeSheet();
  };

  let body;
  if (run?.active) {
    const phase = run.phase === 'joining' && run.approval ? 'approval' : run.phase;
    body = html`
      <p class="stext">${t('probe.phase.' + phase, { host: name, s: Math.ceil(run.secondsLeft || 0) })}</p>
      <div class="progress"><div class="bar" style=${`width:${Math.round(run.progress * 100)}%`}></div></div>
      ${run.phase === 'joining' && run.status && html`<p class="stext small">${run.status}</p>`}
      ${run.now && html`<p class="stext small">${t('probe.nowLine', { rtt: fmt(run.now.rttMs, 1), loss: fmt(run.now.lossPercent, 2), late: fmt(run.now.latestMs, 0) })}</p>`}
      <div class="sheet-actions">
        <button class="btn" onClick=${closeSheet}>${t('probe.hide')}</button>
        <button class="btn primary" onClick=${stop}><${Icon} name="stop" />${t('probe.stop')}</button>
      </div>`;
  } else if (result && !(run?.phase === 'failed' && run.error)) {
    body = html`<${ProbeResult} result=${result} receiver=${receiver} />
      <div class="sheet-actions">
        <button class="btn" disabled=${watching || elsewhere} onClick=${start}>${t('probe.again')}</button>
        <span class="spacer"></span>
        <button class="btn" onClick=${closeSheet}>${t('common.close')}</button>
        <button class="btn primary" disabled=${watching} onClick=${apply}>${t('probe.apply')}</button>
      </div>`;
  } else {
    body = html`
      ${run?.phase === 'failed' && run.error
        ? html`<p class="stext">${t('probe.failed', { error: run.error })}</p>`
        : html`<p class="stext left">${t('probe.intro', { host: name })}</p>
          <p class="stext left small">${t('probe.tips', { host: name })}</p>`}
      ${watching && html`<p class="stext small">${t('probe.notWhileWatching')}</p>`}
      ${elsewhere && html`<p class="stext small">${t('probe.busy', { host: probe.run.hostName })}</p>`}
      <div class="sheet-actions">
        <button class="btn" onClick=${closeSheet}>${t('common.cancel')}</button>
        <button class="btn primary" disabled=${watching || elsewhere} onClick=${start}><${Icon} name="gauge" />${t('probe.start')}</button>
      </div>`;
  }
  return html`<${Sheet} wide onClose=${closeSheet}>
    <div class="sicon"><${Icon} name="gauge" /></div>
    <h2>${t('probe.title', { host: name })}</h2>
    ${body}
  <//>`;
}

function ProbeResult({ result: r, receiver }) {
  const covers = t('probe.coversValue', { first: fmt(r.firstTryMs, 0), resend: fmt(r.resendMs, 0) })
    + (r.rounds > 1 ? t('probe.coversMore', { n: r.rounds - 1, extra: fmt(r.extraRoundMs, 0) }) : '');
  return html`
    <p class="stext">${t('probe.verdict.' + r.verdict)}</p>
    <div class="probe-figs">
      <div class="fig"><div class="k">${t('watch.latency')}</div><div class="v">${r.latencyMs}<small>ms</small></div></div>
      <div class="fig"><div class="k">${t('watch.buffer')}</div><div class="v">${r.bufferSeconds}<small>${t('unit.s')}</small></div></div>
      <div class="fig"><div class="k">${t('watch.overhead')}</div><div class="v">${r.overheadPercent}<small>%</small></div></div>
      <div class="fig"><div class="k">${t('watch.behind')}</div><div class="v">${behindLive(r.latencyMs, r.bufferSeconds)}<small>${t('unit.s')}</small></div></div>
    </div>
    <p class="stext small">${t('probe.now', { ms: receiver.latencyMs, s: receiver.bufferSeconds, behind: behindLive(receiver.latencyMs, receiver.bufferSeconds) })}
      ${' · '}${t('probe.by.' + r.decidedBy)}</p>
    <div class="kv probe-kv">
      <span class="k">${t('probe.rtt')}</span><span class="v">${t('probe.rttValue', { rtt: fmt(r.rttMs, 1), v: fmt(r.rttVarMs, 1) })}</span>
      <span class="k">${t('probe.loss')}</span><span class="v">${t('probe.lossValue', { p: fmt(r.lossPercent, 2), rec: r.recovered, un: r.unrecovered })}</span>
      <span class="k">${t('probe.latest')}</span><span class="v">${t('probe.latestValue', { ms: fmt(r.latestMs, 0), first: fmt(r.firstTryMs, 0), rec: fmt(r.recoveredMaxMs, 0) })}</span>
      <span class="k">${t('probe.covers')}</span><span class="v">${covers}</span>
      <span class="k">${t('probe.stream')}</span><span class="v">${t('probe.streamValue', { mbps: fmt(r.mbps, 1), n: r.packets, s: fmt(r.seconds, 0) })}</span>
      ${r.path && html`<span class="k">${t('probe.path')}</span><span class="v">${r.path}</span>`}
    </div>
    ${r.notes.length > 0 && html`<ul class="problems">${r.notes.map((n) => html`<li>${t('probe.note.' + n.kind, { n: n.n })}</li>`)}</ul>`}`;
}
