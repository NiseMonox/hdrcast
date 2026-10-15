import { html, useEffect, useRef, useState } from '../vendor/preact-htm.js';
import { invoke } from '../api.js';
import { t } from '../i18n.js';
import { Icon } from '../icons.js';
import { useStore, editSettings, openSheet, toast, setState } from '../store.js';
import { Avatar, NumberField, Row, Section, Segmented, Select, Switch, TextField } from '../components.js';
import { FirewallRow, copyInvite } from './share.js';
import { isSharing } from './share.js';
import { isWatching } from './watch.js';

export function pasteInvite() {
  return invoke('invite_preview', { text: null })
    .then((preview) => {
      if (preview.ok) routeInvite(preview);
      else openSheet('paste', {});
    })
    .catch(() => openSheet('paste', {}));
}

export function routeInvite(preview) {
  if (preview.alreadyMember) toast(t('group.alreadyIn', { group: preview.groupName }), { kind: 'info' });
  else openSheet('join', { preview });
}

function ToolRow({ kind, name, tool, value, onChange, hint }) {
  const browse = async () => {
    try {
      const path = await invoke('tool_browse', { kind });
      if (path) onChange(path);
    } catch (e) { toast(String(e), { kind: 'warn' }); }
  };
  return html`<${Row} name=${name} hint=${tool?.path ? tool.path : hint}>
    ${tool?.path
      ? html`<span class="badge green"><${Icon} name="check" style="width:11px;height:11px" />${t('settings.found')}</span>`
      : html`<span class="badge red">${t('settings.missing')}</span>`}
    <button class="btn" onClick=${browse}><${Icon} name="folder" />${t('settings.browse')}</button>
    ${value && html`<button class="btn" onClick=${() => onChange('')}>${t('settings.auto')}</button>`}
  <//>`;
}

export function SettingsPage() {
  const s = useStore();
  const { ui, sender, receiver } = s.settings;
  const busy = isSharing(s.status) || isWatching(s.status);
  const watching = isWatching(s.status);
  const mac = s.init.platform === 'macos';
  const setR = (patch) => editSettings('receiver', patch);

  return html`
    <div class="page-head"><div class="titles"><h1>${t('settings.title')}</h1></div></div>

    <${Section} title=${t('settings.general')}>
      <${Row} name=${t('settings.language')} tile="globe">
        <${Select} value=${ui.language} onChange=${(v) => editSettings('ui', { language: v })}
          options=${[{ value: 'auto', label: t('settings.langAuto') }, { value: 'en', label: 'English' }, { value: 'zh', label: '简体中文' }, { value: 'ja', label: '日本語' }]} />
      <//>
      <${Row} name=${t('settings.appearance')} tile="sparkles">
        <${Segmented} value=${ui.theme} onChange=${(v) => editSettings('ui', { theme: v })}
          options=${[{ value: 'auto', label: t('settings.themeAuto') }, { value: 'light', label: t('settings.themeLight') }, { value: 'dark', label: t('settings.themeDark') }]} />
      <//>
    <//>

    <${Section} title=${t('settings.group')} note=${t('settings.groupNote')}>
      <div class="person row">
        <${Avatar} name=${s.group.groupName} size="large" />
        <div class="who">
          <div class="pname" style="font-size:14px">${s.group.groupName}</div>
          <div class="pmeta">${t('group.as', { name: s.group.displayName })}</div>
        </div>
        <div class="pactions">
          <button class="btn tinted" onClick=${copyInvite}><${Icon} name="copy" />${t('group.copyInvite')}</button>
        </div>
      </div>
      <${Row} name=${t('settings.myName')}>
        <${TextField} value=${s.group.displayName} maxLength=${24} disabled=${busy} className=""
          onChange=${async (v) => {
            if (!v) return;
            try { setState({ group: await invoke('group_rename', { displayName: v }) }); } catch (e) { toast(String(e), { kind: 'warn' }); }
          }} />
      <//>
      <${Row} name=${t('settings.otherGroup')} hint=${t('settings.otherGroupHint')} disabled=${busy}>
        <button class="btn" onClick=${pasteInvite}><${Icon} name="clipboard" />${t('group.pasteInvite')}</button>
        <button class="btn" onClick=${() => openSheet('create', {})}><${Icon} name="plus" />${t('group.create')}</button>
      <//>
      ${s.init.caps.clipboardWatch && html`<${Row} name=${t('settings.detectClipboard')} hint=${t('settings.detectClipboardHint')}>
        <${Switch} on=${s.group.detectClipboard}
          onChange=${async (v) => { try { setState({ group: await invoke('group_detect_clipboard', { on: v }) }); } catch (e) { toast(String(e), { kind: 'warn' }); } }} />
      <//>`}
    <//>

    <${Section} title=${t('settings.tools')} note=${mac ? t('settings.toolsNoteMac') : t('settings.toolsNote')}>
      <${ToolRow} kind="ffmpeg" name="ffmpeg" tool=${s.tools.ffmpeg} value=${sender.ffmpegPath} hint=${t('settings.ffmpegHint')}
        onChange=${(v) => { editSettings('sender', { ffmpegPath: v }); editSettings('receiver', { ffmpegPath: v }); }} />
      <${ToolRow} kind="mpv" name="mpv" tool=${s.tools.mpv} value=${receiver.mpvPath} hint=${t('settings.mpvHint')}
        onChange=${(v) => setR({ mpvPath: v })} />
    <//>

    <${Section} title=${t('settings.playback')}>
      <${Row} name=${t('settings.hwdec')} disabled=${watching}>
        <${Select} value=${receiver.hwDec} onChange=${(v) => setR({ hwDec: v })}
          options=${s.init.hwDecModes.map((m) => ({ value: m.id, label: m.name }))} />
      <//>
      <${Row} name=${t('settings.buffer')} hint=${t('settings.bufferHint')} disabled=${watching}>
        <${NumberField} value=${receiver.bufferSeconds} min=${0.2} max=${30} step=${0.1} onChange=${(v) => setR({ bufferSeconds: v })} />
        <span class="unit">${t('unit.s')}</span>
      <//>
      <${Row} name=${t('settings.expectedBitrate')} hint=${t('settings.expectedBitrateHint')} disabled=${watching}>
        <${NumberField} value=${receiver.expectedBitrateMbps} min=${1} max=${1000} onChange=${(v) => setR({ expectedBitrateMbps: v })} />
        <span class="unit">Mbps</span>
      <//>
      <${Row} name=${t('settings.targetPeak')} hint=${t('settings.targetPeakHint')} disabled=${watching}>
        <${NumberField} value=${receiver.targetPeakNits} min=${0} max=${10000} onChange=${(v) => setR({ targetPeakNits: v })} />
        <span class="unit">nit</span>
      <//>
      <${Row} name=${t('settings.screen')} hint=${t('settings.screenHint')} disabled=${watching}>
        <${NumberField} value=${receiver.fullscreenScreen} min=${0} max=${32} onChange=${(v) => setR({ fullscreenScreen: v })} />
      <//>
      <${Row} name=${t('settings.catchUp')} hint=${t('settings.catchUpHint')}>
        <${Switch} on=${receiver.autoCatchUp} onChange=${(v) => setR({ autoCatchUp: v })} />
      <//>
      <${Row} name=${t('settings.frontend')} hint=${t('settings.frontendHint')} disabled=${watching}>
        <${Select} value=${receiver.frontend} onChange=${(v) => setR({ frontend: v })}
          options=${[{ value: 0, label: t('settings.frontendMpv') }, { value: 1, label: t('settings.frontendPipe') }]} />
      <//>
    <//>

    ${s.firewall?.applicable && html`<${Section} title=${t('settings.network')} note=${t('firewall.note')}>
      <${FirewallRow} firewall=${s.firewall} />
    <//>`}

    <${Section} title=${t('settings.about')}>
      <${Row} name="HdrCast" hint=${t('settings.aboutHint')}>
        <span class="value">${s.init.version}</span>
      <//>
      <${Row} name=${t('settings.files')} hint=${s.init.paths.config}>
        <button class="btn" onClick=${() => invoke('logs_open').catch((e) => toast(String(e), { kind: 'warn' }))}>
          <${Icon} name="folder" />${t('activity.openFolder')}</button>
      <//>
    <//>
    <div style="height:8px"></div>`;
}

const tagRe = /^\[([a-z0-9 _.-]+)\]/i;

export function ActivityPage() {
  const s = useStore();
  const box = useRef(null);
  const stick = useRef(true);
  const [filter, setFilter] = useState('all');
  const lines = filter === 'all' ? s.logs : s.logs.filter((l) => matches(l, filter));

  useEffect(() => {
    const el = box.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [s.logs.length, filter]);

  const copy = async () => {
    try {
      await invoke('clipboard_write', { text: lines.join('\n') });
      toast(t('activity.copied'), { kind: 'ok' });
    } catch (e) { toast(String(e), { kind: 'warn' }); }
  };

  const filters = [['all', t('activity.all')], ['app', 'App'], ['p2p', t('activity.network')], ['video', t('activity.video')], ['player', t('activity.player')]];

  return html`
    <div class="page-head">
      <div class="titles"><h1>${t('activity.title')}</h1><div class="subtitle">${t('activity.subtitle')}</div></div>
      <div class="toolbar">
        <button class="btn" onClick=${copy}><${Icon} name="copy" />${t('activity.copy')}</button>
        <button class="btn" onClick=${() => invoke('logs_open').catch((e) => toast(String(e), { kind: 'warn' }))}>
          <${Icon} name="folder" />${t('activity.openFolder')}</button>
      </div>
    </div>
    <div class="chips" style="padding:0 4px 10px">
      ${filters.map(([id, label]) => html`<button class=${'chip' + (filter === id ? ' on' : '')} onClick=${() => setFilter(id)}>${label}</button>`)}
    </div>
    <div class="card">
      <div class="logbox" ref=${box}
        onScroll=${(e) => { const el = e.currentTarget; stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24; }}>
        ${lines.length === 0
          ? html`<div class="ln">${t('activity.empty')}</div>`
          : lines.map((l) => {
              const m = tagRe.exec(l);
              const tag = m ? m[1] : '';
              const cls = 'ln' + (tag === 'p2p' || tag.startsWith('remux') ? ' p2p' : '') + (/error|fail|cannot|could not/i.test(l) ? ' warn' : '');
              return m
                ? html`<div class=${cls}><span class="tag">[${tag}]</span>${l.slice(m[0].length)}</div>`
                : html`<div class=${cls}>${l}</div>`;
            })}
      </div>
    </div>`;
}

function matches(line, filter) {
  const m = tagRe.exec(line);
  const tag = m ? m[1].toLowerCase() : '';
  switch (filter) {
    case 'app': return tag === 'app' || tag === 'state' || tag === 'probe' || tag === 'control';
    case 'p2p': return tag === 'p2p' || tag.startsWith('remux');
    case 'video': return tag === 'run' || tag === 'capture' || tag === 'ffmpeg';
    case 'player': return tag === 'mpv' || tag === 'ipc' || tag === 'pipe';
    default: return true;
  }
}
