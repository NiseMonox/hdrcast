import { html, useState } from '../vendor/preact-htm.js';
import { invoke } from '../api.js';
import { t } from '../i18n.js';
import { Icon } from '../icons.js';
import { useStore, editSettings, flushSettings, openSheet, toast, setState, state } from '../store.js';
import {
  Avatar, Disclosure, Empty, NumberField, Row, Section, Segmented, Select, Slider, Stepper, Switch, TextField, formatDuration,
} from '../components.js';

export function isSharing(status) {
  const s = status?.share?.state;
  return s === 'starting' || s === 'streaming' || s === 'stopping';
}

export async function startShare() {
  await flushSettings();
  const fw = state.firewall;
  if (fw?.applicable && fw.allowed === false && fw.canFix && !state.firewallOffered) {
    setState({ firewallOffered: true });
    openSheet('firewall', { why: t('firewall.whyHost'), then: reallyStart });
    return;
  }
  await reallyStart();
}

async function reallyStart() {
  try {
    const r = await invoke('share_start');
    if (!r.ok) openSheet('problems', { title: t('share.cannotStart'), problems: r.problems });
  } catch (e) {
    openSheet('problems', { title: t('share.cannotStart'), problems: [String(e)] });
  }
}

export async function stopShare() {
  try { await invoke('share_stop'); } catch (e) { toast(String(e), { kind: 'warn' }); }
}

function planLine(plan, sender) {
  if (!plan || !plan.width) return t('share.noDisplay');
  return `${plan.hdr ? 'HDR10' : 'SDR'} · ${plan.width}×${plan.height} · ${plan.fps} fps · ${sender.bitrateMbps} Mbps`;
}

export function SharePage() {
  const s = useStore();
  const { sender, ui } = s.settings;
  const status = s.status;
  const share = status.share;
  const plan = status.plan;
  const live = isSharing(status);
  const locked = live;
  const [advanced, setAdvanced] = useState(false);
  const [probing, setProbing] = useState(false);
  const [audioApps, setAudioApps] = useState([]);
  const set = (patch) => editSettings('sender', patch);

  const refreshOutputs = async () => {
    try {
      setState({ outputs: await invoke('outputs_refresh') });
    } catch (e) { toast(String(e), { kind: 'warn' }); }
  };

  const probe = async () => {
    setProbing(true);
    try {
      const r = await invoke('encoders_probe');
      if (r.recommended != null) {
        set({ encoder: r.recommended });
        const name = s.init.encoders.find((e) => e.id === r.recommended)?.name || '';
        toast(t('share.probeBest', { name }), { kind: 'ok' });
      } else if (r.problems?.length) {
        openSheet('problems', { title: t('share.probeFailed'), problems: r.problems });
      } else {
        toast(t('share.probeNone'), { kind: 'warn' });
      }
    } catch (e) {
      toast(String(e), { kind: 'warn' });
    }
    setProbing(false);
  };

  const selected = s.outputs.find((o) => o.index === sender.outputIndex) || s.outputs[0];

  return html`
    <div class="page-head">
      <div class="titles">
        <h1>${t('share.title')}</h1>
        <div class="subtitle">${live ? t('share.subtitleLive', { group: s.group.groupName }) : t('share.subtitle', { group: s.group.groupName })}</div>
      </div>
    </div>

    ${(live || share.state === 'failed') && html`<${LivePanel} status=${status} group=${s.group} sender=${sender} />`}
    ${live && html`<${Viewers} share=${share} policy=${ui.remoteControl} />`}

    <div class="section-title" style=${live || share.state === 'failed' ? '' : 'margin-top:0'}>
      <span>${t('share.display')}</span><span class="spacer"></span>
      <button class="btn small plain" disabled=${locked} onClick=${refreshOutputs}><${Icon} name="refresh" />${t('common.refresh')}</button>
    </div>
    <div class="card">
      ${s.outputs.length === 0
        ? html`<${Empty} icon="display" title=${t('share.noDisplay')} text=${t('share.noDisplayText')} />`
        : html`<div class="displays">
            ${s.outputs.map((o, i) => html`
              <button class=${'display' + (o.hdr ? ' hdr' : '') + (selected && o.index === selected.index ? ' selected' : '')}
                disabled=${locked}
                onClick=${() => set({ outputIndex: o.index, outputDeviceName: o.deviceName, outputFriendlyName: o.friendlyName })}>
                <div class="screen"><span class="num">${i + 1}</span>${o.hdr && html`<span class="badge hdr">HDR</span>`}</div>
                <div class="dname">${o.friendlyName || o.deviceName}</div>
                <div class="dmeta">${o.width}×${o.height}${o.refreshHz ? ` · ${Math.round(o.refreshHz)} Hz` : ''}${o.maxLuminance > 0 ? ` · ${Math.round(o.maxLuminance)} nit` : ''}</div>
              </button>`)}
          </div>`}
    </div>
    ${plan?.note && html`<div class="section-note">${plan.note}</div>`}

    <${Section} title=${t('share.picture')}>
      <${Row} name=${t('share.bitrate')} hint=${t('share.bitrateHint')} wide disabled=${locked}>
        <div class="slider-wrap">
          <${Slider} value=${Math.min(150, sender.bitrateMbps)} min=${5} max=${150} step=${1} onChange=${(v) => set({ bitrateMbps: v })} />
          <${NumberField} value=${sender.bitrateMbps} min=${1} max=${500} onChange=${(v) => set({ bitrateMbps: v })} width=${56} />
          <span class="unit">Mbps</span>
        </div>
      <//>
      <${Row} name=${t('share.fps')} disabled=${locked}>
        <${Segmented} value=${sender.fps} onChange=${(v) => set({ fps: v })}
          options=${[30, 48, 60, 90, 120].map((v) => ({ value: v, label: String(v) }))} />
      <//>
      <${Row} name=${t('share.resolution')} disabled=${locked}>
        <${Segmented} value=${sender.scale} onChange=${(v) => set({ scale: v })}
          options=${[{ value: 0, label: t('share.native') }, { value: 1, label: '1440p' }, { value: 2, label: '1080p' }, { value: 3, label: '720p' }]} />
      <//>
      <${Row} name="HDR" hint=${t('share.hdrHint')} disabled=${locked}>
        <${Segmented} value=${sender.hdrMode} onChange=${(v) => set({ hdrMode: v })}
          options=${[{ value: 0, label: t('share.hdrAuto') }, { value: 1, label: t('share.hdrOff') }]} />
      <//>
      <${Row} name=${t('share.encoder')} disabled=${locked}>
        <${Select} value=${sender.encoder} onChange=${(v) => set({ encoder: v })}
          options=${s.init.encoders.map((e) => ({ value: e.id, label: e.name }))} />
        <button class="btn" disabled=${probing} onClick=${probe}>
          ${probing ? html`<span class="spinner"></span>` : html`<${Icon} name="sparkles" />`}${t('share.probe')}
        </button>
      <//>
      ${advanced && html`
        <${Row} name=${t('share.quality')} hint=${t('share.qualityHint')} disabled=${locked}>
          <${Segmented} value=${sender.quality} onChange=${(v) => set({ quality: v })}
            options=${[{ value: 0, label: t('share.qSpeed') }, { value: 1, label: t('share.qBalanced') }, { value: 2, label: t('share.qQuality') }]} />
        <//>
        <${Row} name=${t('share.gop')} disabled=${locked}>
          <${NumberField} value=${sender.gopSeconds} min=${0.5} max=${10} step=${0.5} onChange=${(v) => set({ gopSeconds: v })} />
          <span class="unit">${t('unit.s')}</span>
        <//>
        <${Row} name=${t('share.bframes')} disabled=${locked}><${Switch} on=${sender.bFrames} onChange=${(v) => set({ bFrames: v })} /><//>
        <${Row} name=${t('share.lookahead')} disabled=${locked}><${Switch} on=${sender.lookahead} onChange=${(v) => set({ lookahead: v })} /><//>
        <${Row} name=${t('share.cursor')} disabled=${locked}><${Switch} on=${sender.cursor} onChange=${(v) => set({ cursor: v })} /><//>`}
      <${Disclosure} label=${advanced ? t('common.less') : t('share.advanced')} open=${advanced} onToggle=${() => setAdvanced(!advanced)} />
    <//>

    <${Section} title=${t('share.sound')}>
      <${Row} name=${t('share.audio')} hint=${t('share.audioHint')} disabled=${locked}>
        <${Switch} on=${sender.audio} onChange=${(v) => set({ audio: v })} />
      <//>
      ${sender.audio && html`
        ${s.audioDevices.length > 0 && html`<${Row} name=${t('share.audioDevice')} disabled=${locked}>
          <${Select} value=${sender.audioDevice} onChange=${(v) => set({ audioDevice: v })} style="max-width:300px"
            options=${[{ value: 'default', label: t('share.audioDefault') }].concat(s.audioDevices.map((d) => ({ value: d.id, label: d.name })))} />
        <//>`}
        <${Row} name=${t('share.audioCodec')} disabled=${locked}>
          <${Segmented} value=${sender.audioCodec} onChange=${(v) => set({ audioCodec: v })}
            options=${s.init.audioCodecs.map((c) => ({ value: c.id, label: c.name }))} />
          <${NumberField} value=${sender.audioBitrateKbps} min=${32} max=${512} onChange=${(v) => set({ audioBitrateKbps: v })} width=${56} />
          <span class="unit">kbps</span>
        <//>
        ${s.init.caps.audioExclude && html`<${Row} name=${t('share.leaveOut')} hint=${t('share.leaveOutHint')} disabled=${locked}>
          <input class="field" style="width:190px" list="audio-apps" placeholder=${t('share.leaveOutNone')} spellcheck=${false}
            value=${sender.audioExcludeApp}
            onFocus=${async () => { try { setAudioApps(await invoke('audio_apps')); } catch (e) { /* the list is a convenience */ } }}
            onChange=${(e) => set({ audioExcludeApp: e.target.value.trim() })} />
          <datalist id="audio-apps">${audioApps.map((a) => html`<option value=${a}></option>`)}</datalist>
        <//>`}`}
    <//>

    <${Section} title=${t('share.viewers')} note=${plan ? uploadNote(plan, sender) : ''} noteWarn=${plan?.uploadTooMuch}>
      <${Row} name=${t('share.maxViewers')} hint=${t('share.maxViewersHint')} disabled=${locked}>
        <${Stepper} value=${sender.maxViewers} min=${1} max=${16} onChange=${(v) => set({ maxViewers: v })} />
      <//>
      <${Row} name=${t('share.upload')} hint=${t('share.uploadHint')}>
        <${NumberField} value=${sender.uploadMbps} min=${0} max=${100000} onChange=${(v) => set({ uploadMbps: v })} />
        <span class="unit">Mbps</span>
      <//>
      <${Row} name=${t('control.title')} hint=${t('control.hostHint')}>
        <${Segmented} value=${ui.remoteControl} onChange=${(v) => editSettings('ui', { remoteControl: v })}
          options=${[{ value: 0, label: t('control.off') }, { value: 1, label: t('control.ask') }, { value: 2, label: t('control.allow') }]} />
      <//>
      ${s.firewall?.applicable && html`<${FirewallRow} firewall=${s.firewall} />`}
    <//>
    <div style="height:8px"></div>`;
}

function uploadNote(plan, sender) {
  const base = t(sender.maxViewers === 1 ? 'share.uploadNeed1' : 'share.uploadNeed', {
    n: sender.maxViewers, mbps: sender.bitrateMbps, need: Math.ceil(plan.uploadNeedMbps),
  });
  return plan.uploadTooMuch ? base + ' ' + t('share.uploadTooMuch') : base;
}

export function FirewallRow({ firewall }) {
  const [busy, setBusy] = useState(false);
  const allow = async () => {
    setBusy(true);
    try { setState({ firewall: await invoke('firewall_allow') }); } catch (e) { toast(String(e), { kind: 'warn' }); }
    setBusy(false);
  };
  return html`<${Row} name=${t('firewall.title')} hint=${firewall.text}>
    ${firewall.allowed === true
      ? html`<span class="badge green"><${Icon} name="check" style="width:11px;height:11px" />${t('firewall.open')}</span>`
      : firewall.canFix
        ? html`<button class="btn tinted" disabled=${busy} onClick=${allow}><${Icon} name="shield" />${t('firewall.allow')}</button>`
        : null}
  <//>`;
}

export function ShareActionBar() {
  const s = useStore();
  if (isSharing(s.status)) return null;
  const { sender } = s.settings;
  const plan = s.status.plan;
  return html`<div class="actionbar">
    <div class="summary">
      <div class="line1">${planLine(plan, sender)}</div>
      <div class=${'line2' + (plan?.uploadTooMuch ? ' warn' : '')}>${plan ? uploadNote(plan, sender) : ''}</div>
    </div>
    <button class="btn primary large" onClick=${startShare}><${Icon} name="broadcast" />${t('share.goLive')}</button>
  </div>`;
}

// The engine's status line starts with the group's name, which the panel's title already says.
function reach(status) {
  const cut = status.startsWith('Live in "') ? status.indexOf('" - ') : -1;
  return cut < 0 ? status : status.slice(cut + 4);
}

function LivePanel({ status, group, sender }) {
  const share = status.share;
  const failed = share.state === 'failed';
  const starting = share.state === 'starting';
  const cap = share.capture;
  const enc = share.encode;
  const p2p = share.p2p;
  const cls = 'hero';
  const dropped = (cap?.dropped || 0) + (enc?.dropFrames || 0);
  return html`<div class=${cls}>
    <div class="hero-top">
      <div class="titles">
        <div class="hero-title">${failed ? t('share.failed') : starting ? t('share.starting') : t('share.liveIn', { group: group.groupName })}</div>
        <div class="hero-sub">${failed ? share.detail : (p2p?.status && share.state === 'streaming' ? reach(p2p.status) : share.detail)}</div>
      </div>
      ${!failed && html`<span class=${'live-pill' + (share.state === 'streaming' ? ' onair' : '')}><span class=${'dot' + (share.state === 'streaming' ? ' pulse' : '')}></span>
        ${share.state === 'streaming' ? 'LIVE' : '···'} ${share.state === 'streaming' ? formatDuration(share.uptimeSec) : ''}</span>`}
      ${failed
        ? html`<button class="btn solid" onClick=${startShare}>${t('common.retry')}</button>`
        : html`<button class="btn solid" disabled=${share.state === 'stopping'} onClick=${stopShare}><${Icon} name="stop" />${t('share.stop')}</button>`}
    </div>
    ${!failed && html`<div class="tiles">
      <div class="tile"><div class="k">${t('share.tBitrate')}</div>
        <div class="v">${enc && enc.bitrateKbps >= 0 ? (enc.bitrateKbps / 1000).toFixed(1) : '—'}<small>Mbps</small></div>
        <div class="s">${t('share.tTarget', { mbps: sender.bitrateMbps })}</div></div>
      <div class="tile"><div class="k">${t('share.tEncode')}</div>
        <div class="v">${enc ? enc.fps.toFixed(1) : '—'}<small>fps</small></div>
        <div class="s">${enc && enc.speed >= 0 ? t('share.tSpeed', { x: enc.speed.toFixed(2) }) : ' '}</div></div>
      <div class="tile"><div class="k">${t('share.tCapture')}</div>
        <div class="v">${cap ? cap.fps.toFixed(1) : '—'}<small>fps</small></div>
        <div class="s">${cap ? `${cap.gpuMs.toFixed(2)} ms GPU` : ' '}</div></div>
      <div class="tile"><div class="k">${t('share.tDropped')}</div>
        <div class="v">${cap || enc ? dropped : '—'}</div>
        <div class="s">${status.plan?.format || ''}</div></div>
    </div>`}
  </div>`;
}

function Viewers({ share, policy }) {
  const p2p = share.p2p;
  const viewers = p2p?.viewers || [];
  const control = (sid, allow) => invoke('control_set', { sid, allow }).catch((e) => toast(String(e), { kind: 'warn' }));
  return html`
    <div class="section-title">
      <span>${t('share.watching', { n: viewers.length })}</span><span class="spacer"></span>
      ${viewers.length > 0 && p2p && html`<span class="value">${t('share.uploadNow', { mbps: Math.ceil(p2p.uploadMbps) })}</span>`}
    </div>
    <div class="card rows">
      ${viewers.length === 0
        ? html`<${Empty} icon="people" title=${t('share.nobody')} text=${t('share.nobodyText')}>
            <button class="btn tinted" onClick=${copyInvite}><${Icon} name="copy" />${t('group.copyInvite')}</button>
          <//>`
        : viewers.map((v) => html`<div class="person row" key=${v.sid}>
            <${Avatar} name=${v.name} />
            <div class="who">
              <div class="pname">${v.name}</div>
              <div class="pmeta">
                ${v.state === 'watching'
                  ? html`<span class=${'badge ' + (v.ipv6 ? 'blue' : '')}>${v.ipv6 ? 'IPv6' : 'IPv4'}</span>
                      ${v.rttMs >= 0 && html`<span>${t('share.ping', { ms: Math.round(v.rttMs) })}</span>`}
                      <span>· ${t('share.srtLatency', { s: (v.latencyMs / 1000).toFixed(1).replace(/\.0$/, '') })}</span>`
                  : html`<span class="spinner"></span><span>${t('share.connecting')}</span>`}
                ${v.control === 'granted' && html`<span class="badge orange"><${Icon} name="cursor" style="width:11px;height:11px" />${t('control.active')}</span>`}
              </div>
            </div>
            <div class="pactions">
              ${v.control === 'requested' && html`
                <span class="value">${t('control.wants')}</span>
                <button class="btn small" onClick=${() => control(v.sid, false)}>${t('control.deny')}</button>
                <button class="btn small primary" onClick=${() => control(v.sid, true)}>${t('control.grant')}</button>`}
              ${v.control === 'granted' && html`<button class="btn small danger" onClick=${() => control(v.sid, false)}>${t('control.revoke')}</button>`}
            </div>
          </div>`)}
    </div>
    ${policy === 0 ? null : html`<div class="section-note">${policy === 2 ? t('control.noteAllow') : t('control.noteAsk')}</div>`}`;
}

export async function copyInvite() {
  try {
    await invoke('invite_copy');
    toast(t('group.inviteCopied'), { kind: 'ok', ms: 5000 });
  } catch (e) {
    toast(String(e), { kind: 'warn' });
  }
}
