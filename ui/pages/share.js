import { html, useState } from '../vendor/preact-htm.js';
import { invoke } from '../api.js';
import { t } from '../i18n.js';
import { Icon } from '../icons.js';
import { useStore, editSettings, editAccess, editDevice, flushSettings, openSheet, closeSheet, toast, setState, state } from '../store.js';
import {
  Avatar, Disclosure, Empty, NumberField, Row, Section, Segmented, Select, Sheet, Slider, Stepper, Switch, formatDuration,
} from '../components.js';

/** The screen is being captured (live, or on request for somebody watching). */
export function isSharing(status) {
  const s = status?.share?.state;
  return s === 'starting' || s === 'streaming' || s === 'stopping';
}

export function isLive(status) {
  return status?.share?.mode === 'live';
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

export function answerKnock(sid, allow, remember) {
  return invoke('knock_answer', { sid, allow, remember: !!remember })
    .then((access) => setState({ access }))
    .catch((e) => toast(String(e), { kind: 'warn' }));
}

function planLine(plan, sender) {
  if (!plan || !plan.width) return t('share.noDisplay');
  return `${plan.hdr ? 'HDR10' : 'SDR'} · ${plan.width}×${plan.height} · ${plan.fps} fps · ${sender.bitrateMbps} Mbps`;
}

export function SharePage() {
  const s = useStore();
  const { sender } = s.settings;
  const status = s.status;
  const share = status.share;
  const plan = status.plan;
  const sharing = isSharing(status);
  const viewers = share.p2p?.viewers || [];
  const knocks = share.knocks || [];
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
  const subtitle = share.mode === 'live'
    ? t('share.subtitleLive', { group: s.group.groupName })
    : share.mode === 'available'
      ? t('share.subtitleAvailable', { group: s.group.groupName })
      : t('share.subtitle', { group: s.group.groupName });
  const panel = sharing || share.state === 'failed' || (share.problem && share.mode !== 'off');
  // Reachable, but friends behind routers that change ports cannot get through: say so at the top, not only in
  // the Firewall row at the bottom of the page.
  const fw = s.firewall;
  const firewallCard = share.mode !== 'off' && fw?.applicable && fw.allowed === false && fw.canFix;

  return html`
    <div class="page-head">
      <div class="titles">
        <h1>${t('share.title')}</h1>
        <div class="subtitle">${subtitle}</div>
      </div>
    </div>

    ${panel
      ? html`<${LivePanel} status=${status} group=${s.group} sender=${sender} />`
      : html`<${AvailabilityCard} share=${share} access=${s.access} />`}
    ${firewallCard && html`<${FirewallCard} />`}
    ${knocks.length > 0 && html`<${Knocks} knocks=${knocks} />`}
    ${(viewers.length > 0 || sharing) && html`<${Viewers} share=${share} />`}

    <div class="section-title">
      <span>${t('share.display')}</span><span class="spacer"></span>
      <button class="btn small plain" onClick=${refreshOutputs}><${Icon} name="refresh" />${t('common.refresh')}</button>
    </div>
    <div class="card">
      ${s.outputs.length === 0
        ? html`<${Empty} icon="display" title=${t('share.noDisplay')} text=${t('share.noDisplayText')} />`
        : html`<div class="displays">
            ${s.outputs.map((o, i) => html`
              <button class=${'display' + (o.hdr ? ' hdr' : '') + (selected && o.index === selected.index ? ' selected' : '')}
               
                onClick=${() => set({ outputIndex: o.index, outputDeviceName: o.deviceName, outputFriendlyName: o.friendlyName })}>
                <div class="screen"><span class="num">${i + 1}</span>${o.hdr && html`<span class="badge hdr">HDR</span>`}</div>
                <div class="dname">${o.friendlyName || o.deviceName}</div>
                <div class="dmeta">${o.width}×${o.height}${o.refreshHz ? ` · ${Math.round(o.refreshHz)} Hz` : ''}${o.maxLuminance > 0 ? ` · ${Math.round(o.maxLuminance)} nit` : ''}</div>
              </button>`)}
          </div>`}
    </div>
    ${plan?.note && html`<div class="section-note">${plan.note}</div>`}

    <${Section} title=${t('share.picture')} note=${sharing ? t('share.liveChanges') : null}>
      <${Row} name=${t('share.bitrate')} hint=${t('share.bitrateHint')} wide>
        <div class="slider-wrap">
          <${Slider} value=${Math.min(150, sender.bitrateMbps)} min=${5} max=${150} step=${1} onChange=${(v) => set({ bitrateMbps: v })} />
          <${NumberField} value=${sender.bitrateMbps} min=${1} max=${500} onChange=${(v) => set({ bitrateMbps: v })} width=${56} />
          <span class="unit">Mbps</span>
        </div>
      <//>
      <${Row} name=${t('share.fps')}>
        <${Segmented} value=${sender.fps} onChange=${(v) => set({ fps: v })}
          options=${[30, 48, 60, 90, 120].map((v) => ({ value: v, label: String(v) }))} />
      <//>
      <${Row} name=${t('share.resolution')}>
        <${Segmented} value=${sender.scale} onChange=${(v) => set({ scale: v })}
          options=${[{ value: 0, label: t('share.native') }, { value: 1, label: '1440p' }, { value: 2, label: '1080p' }, { value: 3, label: '720p' }]} />
      <//>
      <${Row} name="HDR" hint=${t('share.hdrHint')}>
        <${Segmented} value=${sender.hdrMode} onChange=${(v) => set({ hdrMode: v })}
          options=${[{ value: 0, label: t('share.hdrAuto') }, { value: 1, label: t('share.hdrOff') }]} />
      <//>
      <${Row} name=${t('share.encoder')}>
        <${Select} value=${sender.encoder} onChange=${(v) => set({ encoder: v })}
          options=${s.init.encoders.map((e) => ({ value: e.id, label: e.name }))} />
        <button class="btn" disabled=${probing} onClick=${probe}>
          ${probing ? html`<span class="spinner"></span>` : html`<${Icon} name="sparkles" />`}${t('share.probe')}
        </button>
      <//>
      ${advanced && html`
        <${Row} name=${t('share.quality')} hint=${t('share.qualityHint')}>
          <${Segmented} value=${sender.quality} onChange=${(v) => set({ quality: v })}
            options=${[{ value: 0, label: t('share.qSpeed') }, { value: 1, label: t('share.qBalanced') }, { value: 2, label: t('share.qQuality') }]} />
        <//>
        <${Row} name=${t('share.gop')}>
          <${NumberField} value=${sender.gopSeconds} min=${0.5} max=${10} step=${0.5} onChange=${(v) => set({ gopSeconds: v })} />
          <span class="unit">${t('unit.s')}</span>
        <//>
        <${Row} name=${t('share.bframes')}><${Switch} on=${sender.bFrames} onChange=${(v) => set({ bFrames: v })} /><//>
        ${sender.encoder === 0 && html`<${Row} name=${t('share.keyframeOnRequest')} hint=${t('share.keyframeOnRequestHint')}>
          <${Switch} on=${sender.keyframeOnRequest} onChange=${(v) => set({ keyframeOnRequest: v })} />
        <//>`}
        <${Row} name=${t('share.lookahead')} disabled=${sender.encoder === 0 && sender.keyframeOnRequest}><${Switch} on=${sender.lookahead} onChange=${(v) => set({ lookahead: v })} /><//>
        <${Row} name=${t('share.cursor')}><${Switch} on=${sender.cursor} onChange=${(v) => set({ cursor: v })} /><//>`}
      <${Disclosure} label=${advanced ? t('common.less') : t('share.advanced')} open=${advanced} onToggle=${() => setAdvanced(!advanced)} />
    <//>

    <${Section} title=${t('share.controlMode')}
      note=${sender.controlMode && share.controlMode && !share.controlMode.offered ? t('share.controlNoNvidia') : t('share.controlModeNote')}
      noteWarn=${sender.controlMode && share.controlMode && !share.controlMode.offered}>
      <${Row} name=${t('share.controlOffer')} hint=${t('share.controlOfferHint')}>
        <${Switch} on=${sender.controlMode} onChange=${(v) => set({ controlMode: v })} />
      <//>
      ${sender.controlMode && html`
        <${Row} name=${t('share.controlBitrate')} hint=${t('share.controlBitrateHint')} wide>
          <div class="slider-wrap">
            <${Slider} value=${Math.min(100, sender.controlBitrateMbps)} min=${5} max=${100} step=${1} onChange=${(v) => set({ controlBitrateMbps: v })} />
            <${NumberField} value=${sender.controlBitrateMbps} min=${1} max=${500} onChange=${(v) => set({ controlBitrateMbps: v })} width=${56} />
            <span class="unit">Mbps</span>
          </div>
        <//>
        <${Row} name=${t('share.controlFps')} hint=${t('share.controlFpsHint')}>
          <${Segmented} value=${sender.controlFps} onChange=${(v) => set({ controlFps: v })}
            options=${[30, 60, 90, 120, 144, 240].map((v) => ({ value: v, label: String(v) }))} />
        <//>`}
    <//>

    <${Section} title=${t('share.sound')}>
      <${Row} name=${t('share.audio')} hint=${t('share.audioHint')}>
        <${Switch} on=${sender.audio} onChange=${(v) => set({ audio: v })} />
      <//>
      ${sender.audio && html`
        ${s.audioDevices.length > 0 && html`<${Row} name=${t('share.audioDevice')}>
          <${Select} value=${sender.audioDevice} onChange=${(v) => set({ audioDevice: v })} style="max-width:300px"
            options=${[{ value: 'default', label: t('share.audioDefault') }].concat(s.audioDevices.map((d) => ({ value: d.id, label: d.name })))} />
        <//>`}
        <${Row} name=${t('share.audioCodec')}>
          <${Segmented} value=${sender.audioCodec} onChange=${(v) => set({ audioCodec: v })}
            options=${s.init.audioCodecs.map((c) => ({ value: c.id, label: c.name }))} />
          <${NumberField} value=${sender.audioBitrateKbps} min=${32} max=${512} onChange=${(v) => set({ audioBitrateKbps: v })} width=${56} />
          <span class="unit">kbps</span>
        <//>
        ${s.init.caps.audioExclude && html`<${Row} name=${t('share.leaveOut')} hint=${t('share.leaveOutHint')}>
          <input class="field" style="width:190px" list="audio-apps" placeholder=${t('share.leaveOutNone')} spellcheck=${false}
            value=${sender.audioExcludeApp}
            onFocus=${async () => { try { setAudioApps(await invoke('audio_apps')); } catch (e) { /* the list is a convenience */ } }}
            onChange=${(e) => set({ audioExcludeApp: e.target.value.trim() })} />
          <datalist id="audio-apps">${audioApps.map((a) => html`<option value=${a}></option>`)}</datalist>
        <//>`}`}
    <//>

    <${AccessSection} access=${s.access} group=${s.group} device=${share.device} candidates=${status.deviceCandidates || []} />

    <${Section} title=${t('share.viewers')} note=${plan ? uploadNote(plan, sender) : ''} noteWarn=${plan?.uploadTooMuch}>
      <${Row} name=${t('share.maxViewers')} hint=${t('share.maxViewersHint')}>
        <${Stepper} value=${sender.maxViewers} min=${1} max=${16} onChange=${(v) => set({ maxViewers: v })} />
      <//>
      <${Row} name=${t('share.upload')} hint=${t('share.uploadHint')}>
        <${NumberField} value=${sender.uploadMbps} min=${0} max=${100000} onChange=${(v) => set({ uploadMbps: v })} />
        <span class="unit">Mbps</span>
      <//>
      <${Row} name=${t('share.endWhenLeft')} hint=${t('share.endWhenLeftHint')}>
        <${Switch} on=${sender.endWhenViewersLeave} onChange=${(v) => set({ endWhenViewersLeave: v })} />
      <//>
      ${fw?.applicable && !firewallCard && html`<${FirewallRow} firewall=${fw} />`}
    <//>
    <div style="height:8px"></div>`;
}

function uploadNote(plan, sender) {
  const base = t(sender.maxViewers === 1 ? 'share.uploadNeed1' : 'share.uploadNeed', {
    n: sender.maxViewers, mbps: sender.bitrateMbps, need: Math.ceil(plan.uploadNeedMbps),
  });
  return plan.uploadTooMuch ? base + ' ' + t('share.uploadTooMuch') : base;
}

/** Runs the elevated Allow (Windows shows its UAC prompt) with `setBusy` true meanwhile. */
async function allowFirewall(setBusy) {
  setBusy(true);
  try { setState({ firewall: await invoke('firewall_allow') }); } catch (e) { toast(String(e), { kind: 'warn' }); }
  setBusy(false);
}

/** Near the top of the page while the firewall keeps friends out: the same fix as the Firewall row. */
function FirewallCard() {
  const [busy, setBusy] = useState(false);
  return html`<div class="card avail" style="margin-top:12px">
    <span class="avail-mark"><${Icon} name="shield" /></span>
    <div class="who">
      <div class="pname">${t('share.firewallTitle')}</div>
      <div class="pmeta">${t('share.firewallSub')}</div>
    </div>
    <div class="pactions">
      <button class="btn tinted" disabled=${busy} onClick=${() => allowFirewall(setBusy)}><${Icon} name="shield" />${t('firewall.allow')}</button>
    </div>
  </div>`;
}

export function FirewallRow({ firewall }) {
  const [busy, setBusy] = useState(false);
  const allow = () => allowFirewall(setBusy);
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
  if (isLive(s.status)) return null;
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
  const m = /^(Live|Available) in ".*?" - /.exec(status);
  return m ? status.slice(m[0].length) : status;
}

/** Not capturing: says whether the group can connect, and as which computer. */
function AvailabilityCard({ share, access }) {
  const on = share.mode === 'available';
  return html`<div class="card avail">
    <span class=${'avail-mark' + (on ? ' on' : '')}><${Icon} name=${on ? 'people' : 'display'} /></span>
    <div class="who">
      <div class="pname">${on ? t('share.availableTitle') : t('share.notAvailableTitle')}</div>
      <div class="pmeta">${on ? t('share.availableSub') : t('share.notAvailableSub')}</div>
    </div>
    <div class="pactions">
      ${share.device && html`<span class="code" title=${t('access.thisDevice')}>${share.device}</span>`}
      ${!on && access && html`<button class="btn" onClick=${() => editAccess({ available: true })}>${t('share.becomeAvailable')}</button>`}
    </div>
  </div>`;
}

function LivePanel({ status, group, sender }) {
  const share = status.share;
  const live = share.mode === 'live';
  const failed = share.state === 'failed' || (!!share.problem && share.state === 'idle');
  const starting = share.state === 'starting';
  const cap = share.capture;
  const enc = share.encode;
  const p2p = share.p2p;
  const dropped = (cap?.dropped || 0) + (enc?.dropFrames || 0);
  const title = failed
    ? t('share.failed')
    : starting
      ? t('share.starting')
      : live ? t('share.liveIn', { group: group.groupName }) : t('share.onDemand');
  const sub = failed
    ? (share.problem || share.detail)
    : (p2p?.status && share.state === 'streaming' ? reach(p2p.status) : share.detail);
  return html`<div class="hero">
    <div class="hero-top">
      <div class="titles">
        <div class="hero-title">${title}</div>
        <div class="hero-sub" style="white-space:pre-line">${sub}</div>
      </div>
      ${!failed && html`<span class=${'live-pill' + (share.state === 'streaming' ? ' onair' : '')}><span class=${'dot' + (share.state === 'streaming' ? ' pulse' : '')}></span>
        ${share.state === 'streaming' ? (live ? 'LIVE' : t('share.pillOnDemand')) : '···'} ${share.state === 'streaming' ? formatDuration(share.uptimeSec) : ''}</span>`}
      ${failed
        ? html`<button class="btn solid" onClick=${live ? startShare : stopShare}>${live ? t('common.retry') : t('common.ok')}</button>`
        : html`<button class="btn solid" disabled=${share.state === 'stopping'} onClick=${stopShare}>
            <${Icon} name="stop" />${live ? t('share.stop') : t('share.disconnectAll')}</button>`}
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
        <div class="s">${share.video?.format || status.plan?.format || ''}</div></div>
    </div>`}
    ${!failed && share.video?.hdrUnavailable && html`<div class="hero-note">${t('share.hdrUnavailable')}</div>`}
  </div>`;
}

/** Who asks to watch and waits for an answer. */
function Knocks({ knocks }) {
  return html`
    <div class="section-title"><span>${t('knock.title', { n: knocks.length })}</span></div>
    <div class="card rows">
      ${knocks.map((k) => html`<div class="person row" key=${k.sid}>
        <${Avatar} name=${k.name} />
        <div class="who">
          <div class="pname">${t('knock.wants', { name: k.name })}</div>
          <div class="pmeta">
            ${k.verified ? html`<span class="code">${k.deviceCode}</span>` : html`<span>${t('knock.unverified')}</span>`}
            <span>· ${t('knock.left', { s: Math.max(0, k.limitSec - k.waitingSec) })}</span>
          </div>
        </div>
        <div class="pactions">
          <button class="btn small" onClick=${() => answerKnock(k.sid, false)}>${t('knock.deny')}</button>
          ${k.verified && html`<button class="btn small" title=${t('knock.alwaysHint')} onClick=${() => answerKnock(k.sid, true, true)}>${t('knock.always')}</button>`}
          <button class="btn small primary" onClick=${() => answerKnock(k.sid, true)}>${t('knock.allow')}</button>
        </div>
      </div>`)}
    </div>`;
}

function Viewers({ share }) {
  const p2p = share.p2p;
  const viewers = p2p?.viewers || [];
  const control = (sid, allow) => invoke('control_set', { sid, allow }).catch((e) => toast(String(e), { kind: 'warn' }));
  const kick = (sid) => invoke('share_kick', { sid }).catch((e) => toast(String(e), { kind: 'warn' }));
  // live, and everyone who watched has left: the broadcast ends when this runs out (the switch under Viewers)
  const ending = share.mode === 'live' && share.endsInSec != null;
  return html`
    <div class="section-title">
      <span>${t('share.watching', { n: viewers.length })}</span><span class="spacer"></span>
      ${viewers.length > 0 && p2p && html`<span class="value">${t('share.uploadNow', { mbps: Math.ceil(p2p.uploadMbps) })}</span>`}
    </div>
    <div class="card rows">
      ${viewers.length === 0 && ending
        ? html`<${Empty} icon="people" title=${t('share.allLeft')} text=${t('share.endsIn', { s: share.endsInSec })} />`
        : viewers.length === 0
        ? html`<${Empty} icon="people" title=${t('share.nobody')} text=${t(share.mode === 'live' ? 'share.nobodyText' : 'share.nobodyOnDemand')}>
            ${share.mode === 'live' && html`<button class="btn tinted" onClick=${copyInvite}><${Icon} name="copy" />${t('group.copyInvite')}</button>`}
          <//>`
        : viewers.map((v) => html`<div class="person row" key=${v.sid}>
            <${Avatar} name=${v.name} />
            <div class="who">
              <div class="pname">${v.name}${v.deviceCode && html` <span class="code">${v.deviceCode}</span>`}</div>
              <div class="pmeta">
                ${v.state === 'watching' && v.mode === 'control'
                  ? html`<span class=${'badge ' + (v.ipv6 ? 'blue' : '')}>${v.ipv6 ? 'IPv6' : 'IPv4'}</span>
                      <span class="badge">${t('share.inControlMode')}</span>
                      ${(v.ll?.rttMs ?? v.rttMs) >= 0 && html`<span>${t('share.ping', { ms: Math.round(v.ll?.rttMs ?? v.rttMs) })}</span>`}
                      ${v.ll && html`<span>· ${v.ll.mbps.toFixed(1)} Mbps</span>`}
                      ${v.ll?.lost > 0 && html`<span>· ${t('share.llLost', { n: v.ll.lost })}</span>`}
                      ${v.ll?.restored > 0 && html`<span>· ${t('share.llRestored', { n: v.ll.restored })}</span>`}`
                  : v.state === 'watching'
                  ? html`<span class=${'badge ' + (v.ipv6 ? 'blue' : '')}>${v.ipv6 ? 'IPv6' : 'IPv4'}</span>
                      ${v.rttMs >= 0 && html`<span>${t('share.ping', { ms: Math.round(v.rttMs) })}</span>`}
                      <span>· ${t('share.srtLatency', { s: (v.latencyMs / 1000).toFixed(1).replace(/\.0$/, '') })}</span>
                      ${v.net?.lossPercent >= 0.05 && html`<span>· ${t('share.loss', { p: v.net.lossPercent.toFixed(1) })}</span>`}
                      ${v.net?.lostForGood > 0 && html`<span>· ${t('share.lostForGood', { n: v.net.lostForGood })}</span>`}`
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
              <button class="btn small plain" title=${t('share.disconnect')} onClick=${() => kick(v.sid)}>${t('share.disconnect')}</button>
            </div>
          </div>`)}
    </div>`;
}

// ------------------------------------------------------------------------------------------- who can connect

const connectOptions = () => [
  { value: 'allow', label: t('access.connectAllow') },
  { value: 'ask', label: t('access.connectAsk') },
  { value: 'block', label: t('access.connectBlock') },
];
const controlOptions = () => [
  { value: 'off', label: t('access.controlOff') },
  { value: 'ask', label: t('access.controlAsk') },
  { value: 'allow', label: t('access.controlAllow') },
];

function RuleControls({ rule, onChange }) {
  return html`<div class="rule">
    <label class="rule-part"><span>${t('access.connect')}</span>
      <${Select} value=${rule.connect} onChange=${(v) => onChange({ connect: v })} options=${connectOptions()} /></label>
    <label class="rule-part"><span>${t('access.control')}</span>
      <${Select} value=${rule.control} onChange=${(v) => onChange({ control: v })} options=${controlOptions()} /></label>
  </div>`;
}

function seenAgo(ms) {
  if (!ms) return '';
  const minutes = Math.max(0, Math.floor((Date.now() - ms) / 60000));
  if (minutes < 2) return t('access.seenNow');
  if (minutes < 60) return t('access.seenMin', { n: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return t('access.seenHours', { n: hours });
  return t('access.seenDays', { n: Math.floor(hours / 24) });
}

function AccessSection({ access, group, device, candidates }) {
  if (!access) return null;
  return html`
    <div class="section-title"><span>${t('access.title')}</span></div>
    <div class="card rows">
      <${Row} name=${t('access.available')} hint=${t(state.init?.platform === 'macos' ? 'access.availableHintMac' : 'access.availableHint')}>
        <${Switch} on=${access.available} onChange=${(v) => editAccess({ available: v })} />
      <//>
      <div class="row access-row">
        <span class="avail-mark small"><${Icon} name="people" /></span>
        <div class="label">
          <div class="name">${t('access.others', { group: group.groupName })}</div>
          <div class="hint">${t('access.othersHint')}</div>
        </div>
        <${RuleControls} rule=${access.others} onChange=${(patch) => editAccess({ others: { ...access.others, ...patch } })} />
      </div>
      ${access.devices.map((d) => html`<div class="row access-row" key=${d.key}>
        <${Avatar} name=${d.name} size="small" />
        <div class="label">
          <div class="name">${d.name} <span class="code">${d.code}</span></div>
          <div class="hint">${seenAgo(d.lastSeenMs)}</div>
        </div>
        <${RuleControls} rule=${d} onChange=${(patch) => editDevice(d.key, patch)} />
        <button class="btn icon small plain" title=${t('access.remove')} onClick=${() => editDevice(d.key, null)}><${Icon} name="x" /></button>
      </div>`)}
      <div class="row">
        <div class="label"><div class="hint">${access.devices.length === 0 ? t('access.devicesHint') : t('access.devicesNote')}</div></div>
        <div class="control">
          <button class="btn" onClick=${() => openSheet('add-device', {})}><${Icon} name="plus" />${t('access.add')}</button>
        </div>
      </div>
    </div>
    <div class="section-note">${t('access.note', { code: device || '—' })}</div>`;
}

/** Picks a computer from the group (online now, or that asked lately) and gives it a rule. */
export function AddDeviceSheet() {
  const s = useStore();
  const candidates = s.status.deviceCandidates || [];
  const add = (c, mine) => {
    const rule = mine ? { connect: 'allow', control: 'allow' } : { connect: 'allow', control: 'ask' };
    editAccess({ devices: [{ key: c.key, name: c.name, code: c.code, lastSeenMs: Date.now(), ...rule }].concat(s.access.devices) });
    closeSheet();
    toast(t('adddev.added', { name: c.name }), { kind: 'ok' });
  };
  return html`<${Sheet} wide onClose=${closeSheet}>
    <div class="sicon"><${Icon} name="display" /></div>
    <h2>${t('adddev.title')}</h2>
    <p class="stext">${t('adddev.text', { code: s.status.share.device || '' })}</p>
    <div class="card rows" style="margin-top:14px;text-align:left">
      ${candidates.length === 0
        ? html`<${Empty} icon="people" title=${t('adddev.noneTitle')} text=${t('adddev.none')} />`
        : candidates.map((c) => html`<div class="person row" key=${c.key}>
            <${Avatar} name=${c.name} />
            <div class="who">
              <div class="pname">${c.name}</div>
              <div class="pmeta"><span class="code">${c.code}</span><span>· ${t(c.source === 'online' ? 'adddev.online' : 'adddev.asked')}</span></div>
            </div>
            <div class="pactions">
              <button class="btn small" title=${t('adddev.friendHint')} onClick=${() => add(c, false)}>${t('adddev.friend')}</button>
              <button class="btn small primary" title=${t('adddev.mineHint')} onClick=${() => add(c, true)}>${t('adddev.mine')}</button>
            </div>
          </div>`)}
    </div>
    <div class="sheet-actions"><button class="btn" onClick=${closeSheet}>${t('common.close')}</button></div>
  <//>`;
}

export async function copyInvite() {
  try {
    await invoke('invite_copy');
    toast(t('group.inviteCopied'), { kind: 'ok', ms: 5000 });
  } catch (e) {
    toast(String(e), { kind: 'warn' });
  }
}
