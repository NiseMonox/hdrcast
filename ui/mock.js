// A stand-in backend for working on the UI in a plain browser (never loaded inside the app).
// URL switches: ?fresh (no group yet), ?live (already sharing), ?watching, ?mac, ?lang=zh|ja|en, ?theme=dark|light

const q = new URLSearchParams(location.search);
const handlers = new Map();
const emit = (event, payload) => (handlers.get(event) || []).forEach((h) => h(payload));

export function listen(event, handler) {
  if (!handlers.has(event)) handlers.set(event, []);
  handlers.get(event).push(handler);
  return () => handlers.set(event, handlers.get(event).filter((h) => h !== handler));
}

const mac = q.has('mac');
const now = () => Date.now();

const db = {
  ui: { language: q.get('lang') || 'auto', theme: q.get('theme') || 'auto', remoteControl: 1, requestControl: false },
  sender: {
    outputIndex: 0, outputDeviceName: '\\\\.\\DISPLAY1', outputFriendlyName: 'AW3225QF', fps: 60, scale: 0, hdrMode: 0, cursor: true,
    sdrWhite: 80, encoder: 0, quality: 1, bitrateMbps: 40, gopSeconds: 2, bufferSeconds: 2, bFrames: true, lookahead: true,
    audio: true, audioDevice: 'default', audioCodec: 1, audioBitrateKbps: 192, audioExcludeApp: '', maxViewers: 4, uploadMbps: 0,
    ffmpegPath: '',
  },
  receiver: {
    latencyMs: 1500, expectedBitrateMbps: 100, displayMode: 0, toneMapping: 'bt.2390', hwDec: 1, bufferSeconds: 1.5,
    fullscreen: false, fullscreenScreen: 0, frontend: 0, mpvPath: '', ffmpegPath: '', autoCatchUp: true, targetPeakNits: 0, lastHost: '',
  },
  group: q.has('fresh')
    ? { configured: false, groupName: '', displayName: '', peerId: '', detectClipboard: true }
    : { configured: true, groupName: 'Friends', displayName: 'NiseMono', peerId: 'me', detectClipboard: true },
  outputs: [
    { index: 0, deviceName: '\\\\.\\DISPLAY1', friendlyName: 'AW3225QF', width: 3840, height: 2160, refreshHz: 240, hdr: true, maxLuminance: 1000 },
    { index: 1, deviceName: '\\\\.\\DISPLAY2', friendlyName: 'DELL U2419H', width: 1920, height: 1080, refreshHz: 60, hdr: false, maxLuminance: 0 },
  ],
  firewall: mac ? { applicable: false, allowed: null, text: '' }
    : { applicable: true, allowed: false, canFix: true, text: 'Only replies get in: friends whose routers change ports cannot connect.' },
  share: { state: 'idle', detail: 'Stopped', startedMs: 0 },
  watch: { active: false, hostId: '', hostName: '', startedMs: 0 },
  viewers: [],
};

const hosts = [
  { hostId: 'h1', name: 'Hana', sinceMs: now() - 23 * 60000, mbps: 40, viewers: 1, maxViewers: 4, codec: 'hevc', ipv6: false, openIpv4: true, control: true },
  { hostId: 'h2', name: 'Leo', sinceMs: now() - 95 * 60000, mbps: 25, viewers: 3, maxViewers: 3, codec: 'hevc', ipv6: true, openIpv4: null, control: false },
];

function plan() {
  const o = db.outputs.find((x) => x.index === db.sender.outputIndex) || db.outputs[0];
  const heights = [0, 1440, 1080, 720];
  const target = heights[db.sender.scale];
  let w = o.width, h = o.height;
  if (target && target < o.height) { w = Math.round((o.width * target) / o.height) & ~1; h = target; }
  const hdr = o.hdr && db.sender.hdrMode === 0;
  const need = db.sender.bitrateMbps * db.sender.maxViewers * 1.25;
  let note = '';
  if (o.hdr && db.sender.hdrMode === 1) note = 'The monitor is in HDR mode, streaming SDR anyway.';
  if (!o.hdr && db.sender.hdrMode === 0) note = 'Turn on HDR in Windows for an HDR stream.';
  return {
    hdr, width: w, height: h, fps: db.sender.fps, format: (hdr ? 'p010 / HDR10 ' : 'nv12 / SDR ') + w + 'x' + h, note,
    uploadNeedMbps: need, uploadTooMuch: db.sender.uploadMbps > 0 && need > db.sender.uploadMbps * 0.8,
  };
}

function status() {
  const s = db.share;
  const up = s.startedMs ? (now() - s.startedMs) / 1000 : 0;
  const streaming = s.state === 'streaming';
  const wobble = Math.sin(now() / 900);
  const w = db.watch;
  const wup = w.startedMs ? (now() - w.startedMs) / 1000 : 0;
  const connected = w.active && wup > 2.5;
  const playing = w.active && wup > 4.5;
  return {
    share: {
      state: s.state, detail: s.detail, attempt: 1, uptimeSec: up, summary: '',
      capture: streaming ? { fps: db.sender.fps - 0.02 + wobble * 0.02, duplicated: Math.floor(up * 12), dropped: 0, gpuMs: 0.61 + wobble * 0.05, audioUnderrun: 0 } : null,
      encode: streaming ? { fps: db.sender.fps, frame: Math.floor(up * db.sender.fps), bitrateKbps: db.sender.bitrateMbps * 1000 * (0.97 + wobble * 0.03), speed: 1.0, dropFrames: 0 } : null,
      p2p: s.state === 'idle' || s.state === 'failed' ? null : {
        status: `Live in "${db.group.groupName}" - ${db.viewers.length} viewer${db.viewers.length === 1 ? '' : 's'}`,
        hasIpv6: true, ipv4Open: db.firewall.allowed, uploadMbps: db.viewers.length * db.sender.bitrateMbps * 1.25,
        viewers: db.viewers.map((v) => ({ ...v, state: now() - v.joinedMs > 2500 ? 'watching' : 'connecting', joinedSec: (now() - v.joinedMs) / 1000 })),
      },
    },
    plan: plan(),
    watch: {
      active: w.active, hostId: w.hostId, hostName: w.hostName,
      session: w.active ? {
        state: connected ? 'connected' : wup > 1 ? 'punching' : 'joining',
        status: connected ? `Connected to ${w.hostName} (IPv6 [2400:aaaa::1]:51234, ping 38 ms)` : `Connecting to ${w.hostName}`,
        path: connected ? 'IPv6 · ping 38 ms' : '', couldHelp: false,
      } : null,
      player: w.active && connected ? {
        state: playing ? 'playing' : 'waiting', text: playing ? 'Playing' : 'Connecting…', attempt: 1, uptimeSec: Math.max(0, wup - 4.5),
        cacheSeconds: playing ? 1.5 + wobble * 0.2 : null, drops: 0, decoderDrops: 0, fps: playing ? 60 : null,
        bitrateMbps: playing ? 39.2 + wobble : null, hwdec: playing ? (mac ? 'videotoolbox' : 'd3d11va') : '',
        source: playing ? '3840x2160 p010 bt.2020 pq' : '', output: playing ? 'SDR' : '', hdr: playing,
      } : null,
      control: !w.active || !db.ui.requestControl ? 'off' : wup > 7 ? 'granted' : 'requested',
    },
    live: { hosts, error: null, loaded: true },
    group: db.group,
    firewall: db.firewall,
  };
}

function scheduleViewers() {
  const people = [
    { sid: 's1', name: 'Hana', ipv6: false, path: '198.51.100.13:62909', rttMs: 74, latencyMs: 3500, overheadPercent: 50, control: 'none' },
    { sid: 's2', name: 'Leo', ipv6: false, path: '198.51.100.3:20834', rttMs: 81, latencyMs: 3500, overheadPercent: 50, control: 'requested' },
    { sid: 's3', name: 'Mika', ipv6: true, path: '[2400:aaaa::7]:50112', rttMs: 12, latencyMs: 1500, overheadPercent: 25, control: 'none' },
  ];
  people.forEach((p, i) => setTimeout(() => {
    if (db.share.state === 'streaming' || db.share.state === 'starting') {
      db.viewers.push({ ...p, joinedMs: now() });
      emit('log', { lines: [`[p2p] ${p.name} is joining (latency ${p.latencyMs} ms)`] });
    }
  }, 2500 + i * 2600));
}

const api = {
  app_init() {
    return {
      version: '0.3.0', platform: mac ? 'macos' : 'windows', locale: navigator.language, backdrop: false, defaultName: 'NiseMono',
      ui: db.ui, sender: db.sender, receiver: db.receiver, group: db.group, outputs: db.outputs,
      audioDevices: [{ id: '{0.0.0.00000000}.{a}', name: 'Speakers (Realtek USB Audio)' }, { id: '{0.0.0.00000000}.{b}', name: 'AW3225QF (NVIDIA High Definition Audio)' }],
      encoders: mac
        ? [{ id: 5, name: 'Apple VideoToolbox (HEVC)' }, { id: 4, name: 'x265 (software)' }]
        : [{ id: 0, name: 'NVIDIA NVENC (HEVC)' }, { id: 2, name: 'AMD AMF (HEVC)' }, { id: 3, name: 'Intel Quick Sync (HEVC)' }, { id: 4, name: 'x265 (software)' }],
      audioCodecs: [{ id: 1, name: 'AAC' }, { id: 2, name: 'Opus' }],
      latencyPresets: [{ name: 'Fast', latencyMs: 300 }, { name: 'Japan', latencyMs: 1500 }, { name: 'Australia', latencyMs: 2000 }, { name: 'China', latencyMs: 3500 }],
      toneMappings: ['bt.2390', 'auto', 'spline', 'bt.2446a', 'st2094-40', 'st2094-10', 'hable', 'mobius', 'reinhard', 'clip'],
      hwDecModes: [{ id: 0, name: 'auto-safe' }, { id: 1, name: mac ? 'videotoolbox' : 'd3d11va' }, { id: 2, name: mac ? 'videotoolbox-copy' : 'd3d11va-copy' }, { id: 3, name: 'no (software)' }],
      tools: { ffmpeg: { path: mac ? '/opt/homebrew/bin/ffmpeg' : 'D:\\HdrCast\\tools\\ffmpeg.exe' }, mpv: { path: mac ? null : 'D:\\HdrCast\\tools\\mpv.exe' } },
      firewall: db.firewall,
      paths: { config: mac ? '~/Library/Application Support/HdrCast' : 'C:\\Users\\me\\AppData\\Roaming\\HdrCast', logs: '' },
      caps: { audioExclude: !mac, clipboardWatch: !mac },
      status: status(),
      startupInvite: null,
    };
  },
  settings_save({ settings }) {
    db.ui = settings.ui; db.sender = settings.sender; db.receiver = settings.receiver;
    return { ...settings, tools: null };
  },
  outputs_refresh: () => db.outputs,
  audio_apps: () => ['Discord.exe', 'chrome.exe', 'Spotify.exe'],
  async encoders_probe() {
    await new Promise((r) => setTimeout(r, 1800));
    return { recommended: mac ? 5 : 0, results: [] };
  },
  share_start() {
    db.share = { state: 'starting', detail: 'Waiting for the capture engine…', startedMs: now() };
    db.viewers = [];
    emit('log', { lines: ['[app] NVIDIA NVENC (HEVC) 40 Mbps 3840x2160@60 HDR10', '[p2p] candidates: [2400:aaaa:8ae2::1c]:51234, 192.168.1.3:51235', '[run] HdrCast.exe capture --output 0 --fps 60 --format p010 …'] });
    setTimeout(() => { if (db.share.state === 'starting') { db.share.state = 'streaming'; db.share.detail = 'Streaming'; } }, 1400);
    scheduleViewers();
    return { ok: true };
  },
  share_stop() {
    db.share = { state: 'idle', detail: 'Stopped', startedMs: 0 };
    db.viewers = [];
    emit('log', { lines: ['[app] stopped by user'] });
  },
  control_set({ sid, allow }) {
    const v = db.viewers.find((x) => x.sid === sid);
    if (v) v.control = allow ? 'granted' : 'none';
  },
  live_refresh: () => null,
  watch_start({ hostId }) {
    const h = hosts.find((x) => x.hostId === hostId);
    db.watch = { active: true, hostId, hostName: h.name, startedMs: now() };
    emit('log', { lines: [`[app] watching ${h.name} (latency ${db.receiver.latencyMs} ms)`] });
    return { ok: true };
  },
  watch_stop() { db.watch = { active: false, hostId: '', hostName: '', startedMs: 0 }; },
  watch_fullscreen: () => null,
  group_create({ groupName, displayName }) {
    db.group = { configured: true, groupName, displayName, peerId: 'me', detectClipboard: true };
    return db.group;
  },
  group_rename({ displayName }) { db.group = { ...db.group, displayName }; return db.group; },
  group_detect_clipboard({ on }) { db.group = { ...db.group, detectClipboard: on }; return db.group; },
  invite_preview({ text }) {
    if (text == null) return { ok: false, error: 'The clipboard holds no invite.' };
    if (!/hdrcast:/i.test(text)) return { ok: false, error: 'This does not look like an HdrCast invite. Copy the whole message, including the line that starts with hdrcast:' };
    return { ok: true, groupName: 'Weekend Movies', alreadyMember: false, currentGroup: db.group.configured ? db.group.groupName : null, defaultName: db.group.displayName || 'NiseMono', fingerprint: 'abcd1234', text };
  },
  invite_join({ displayName }) {
    db.group = { configured: true, groupName: 'Weekend Movies', displayName, peerId: 'me', detectClipboard: true };
    return db.group;
  },
  invite_copy: () => null,
  invite_dismiss: () => null,
  firewall_allow() {
    db.firewall = { applicable: true, allowed: true, canFix: false, text: 'Friends can reach this PC through Windows Firewall.' };
    return db.firewall;
  },
  tool_browse: () => null,
  logs_open: () => null,
  clipboard_write: ({ text }) => navigator.clipboard?.writeText(text),
  app_focused: () => null,
};

export async function invoke(cmd, args) {
  if (!api[cmd]) throw new Error('mock: no command ' + cmd);
  return api[cmd](args || {});
}

setInterval(() => emit('status', status()), 400);
setTimeout(() => emit('log', { lines: ['[app] settings: C:\\Users\\me\\AppData\\Roaming\\HdrCast', '[app] group: "Friends" as NiseMono'] }), 300);
if (q.has('live')) setTimeout(() => api.share_start(), 200);
if (q.has('watching')) setTimeout(() => api.watch_start({ hostId: 'h1' }), 200);
