// A stand-in backend for working on the UI in a plain browser (never loaded inside the app).
// URL switches: ?fresh (no group yet), ?live (already sharing), ?watching, ?knock (somebody asks to watch),
// ?off (not available to connect), ?mac, ?lang=zh|ja|en, ?theme=dark|light,
// ?nohdr (the system refuses HDR capture: an HDR display is sent as SDR, as on a Mac without it),
// ?autostart=on|disabled|other (the sign-in entry). overlay.html?watching is the button over the player
// (overlay.html?watching&panel the panel it opens into).

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
const LATENCY_PRESETS = [{ name: 'Fast', latencyMs: 300 }, { name: 'Japan', latencyMs: 1500 }, { name: 'Australia', latencyMs: 2000 }, { name: 'China', latencyMs: 3500 }];
const HWDEC_MODES = [{ id: 0, name: 'auto-safe' }, { id: 1, name: mac ? 'videotoolbox' : 'd3d11va' }, { id: 2, name: mac ? 'videotoolbox-copy' : 'd3d11va-copy' }, { id: 3, name: 'no (software)' }];
const DEVICE = '7KQ2-M9XD-4TPA';

const db = {
  ui: { language: q.get('lang') || 'auto', theme: q.get('theme') || 'auto' },
  sender: {
    outputIndex: 0, outputDeviceName: '\\\\.\\DISPLAY1', outputFriendlyName: 'AW3225QF', fps: 60, scale: 0, hdrMode: 0, cursor: true,
    sdrWhite: 80, encoder: 0, quality: 1, bitrateMbps: 40, gopSeconds: 1, bufferSeconds: 0.25, bFrames: true, lookahead: true, keyframeOnRequest: false,
    audio: true, audioDevice: 'default', audioCodec: 1, audioBitrateKbps: 192, audioExcludeApp: '', maxViewers: 4, uploadMbps: 0,
    ffmpegPath: '', settingsVersion: 1,
  },
  receiver: {
    latencyMs: 1500, expectedBitrateMbps: 100, displayMode: 0, toneMapping: 'bt.2390', hwDec: 1, bufferSeconds: 1.5,
    fullscreen: false, fullscreenScreen: 0, frontend: 0, mpvPath: '', ffmpegPath: '', autoCatchUp: true, targetPeakNits: 0, lastHost: '',
  },
  access: {
    rev: 0,
    available: !q.has('off'),
    others: { connect: 'ask', control: 'off' },
    devices: [
      { key: 'k-laptop', code: 'Q4ZD-81HM-X0RT', name: 'NiseMono laptop', connect: 'allow', control: 'allow', lastSeenMs: now() - 3 * 3600000 },
    ],
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
  share: { state: 'idle', live: false, detail: 'Stopped', startedMs: 0 },
  watch: { active: false, hostId: '', hostName: '', startedMs: 0, want: null },
  viewers: [],
  knocks: [],
  seen: [{ key: 'k-jiuyi', code: 'B7WP-3N6S-KCE1', name: 'Hana', source: 'asked' }],
  autostart: {
    supported: true,
    on: q.get('autostart') === 'on',
    other: q.get('autostart') === 'other' ? (mac ? '/Users/me/Downloads/HdrCast.app/Contents/MacOS/HdrCast' : 'D:\\HdrCast-0.3.0\\HdrCast.exe') : null,
    disabled: q.get('autostart') === 'disabled',
  },
};

const hosts = [
  { hostId: 'h1', name: 'Hana', sinceMs: now() - 23 * 60000, mbps: 40, viewers: 1, maxViewers: 4, codec: 'hevc', ipv6: false, openIpv4: true, control: true, onDemand: false, deviceCode: 'B7WP-3N6S-KCE1' },
  { hostId: 'h2', name: 'Leo', sinceMs: now() - 95 * 60000, mbps: 25, viewers: 3, maxViewers: 3, codec: 'hevc', ipv6: true, openIpv4: null, control: false, onDemand: false, deviceCode: '' },
  { hostId: 'h3', name: 'NiseMono desktop', sinceMs: now() - 300 * 60000, mbps: 60, viewers: 0, maxViewers: 4, codec: 'hevc', ipv6: true, openIpv4: true, control: true, onDemand: true, deviceCode: 'H2C8-5RVJ-9MZ4' },
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

function mode() {
  if (db.share.live) return 'live';
  return db.access.available ? 'available' : 'off';
}

function status() {
  const s = db.share;
  const up = s.startedMs ? (now() - s.startedMs) / 1000 : 0;
  const streaming = s.state === 'streaming';
  const wobble = Math.sin(now() / 900);
  const w = db.watch;
  const wup = w.startedMs ? (now() - w.startedMs) / 1000 : 0;
  const host = hosts.find((h) => h.hostId === w.hostId);
  const asks = host?.onDemand && wup < 3.5;
  const connected = w.active && wup > (host?.onDemand ? 5 : 2.5);
  const playing = w.active && wup > (host?.onDemand ? 7 : 4.5);
  let control = 'off';
  if (connected && host?.control) {
    if (host.onDemand) control = w.want === false ? 'allowed' : 'granted';
    else control = w.want === true ? (wup - w.wantAt > 2 ? 'granted' : 'requested') : 'offered';
  }
  const p = plan();
  const sentHdr = p.hdr && !q.has('nohdr');
  return {
    share: {
      mode: mode(), state: s.state, detail: s.detail, attempt: 1, uptimeSec: up,
      capture: streaming ? { fps: db.sender.fps - 0.02 + wobble * 0.02, duplicated: Math.floor(up * 12), dropped: 0, gpuMs: 0.61 + wobble * 0.05, audioUnderrun: 0 } : null,
      encode: streaming ? { fps: db.sender.fps, frame: Math.floor(up * db.sender.fps), bitrateKbps: db.sender.bitrateMbps * 1000 * (0.97 + wobble * 0.03), speed: 1.0, dropFrames: 0 } : null,
      video: streaming ? {
        hdr: sentHdr, width: p.width, height: p.height, hdrUnavailable: p.hdr && !sentHdr,
        format: (sentHdr ? 'p010 / HDR10 ' : 'nv12 / SDR ') + p.width + 'x' + p.height,
      } : null,
      p2p: mode() === 'off' ? null : {
        status: `${db.share.live ? 'Live' : 'Available'} in "${db.group.groupName}" - ${db.viewers.length} viewer${db.viewers.length === 1 ? '' : 's'}`,
        hasIpv6: true, ipv4Open: db.firewall.allowed, uploadMbps: db.viewers.length * db.sender.bitrateMbps * 1.25,
        viewers: db.viewers.map((v) => ({ ...v, state: now() - v.joinedMs > 2500 ? 'watching' : 'connecting' })),
      },
      knocks: db.knocks.map((k) => ({ ...k, waitingSec: Math.floor((now() - k.sinceMs) / 1000), limitSec: 50 })),
      problem: null,
      device: DEVICE,
    },
    plan: p,
    watch: {
      active: w.active, hostId: w.hostId, hostName: w.hostName,
      hostDeviceCode: host?.deviceCode || '', hostOnDemand: !!host?.onDemand,
      latencyMs: w.latencyMs || db.receiver.latencyMs, overlay: !!w.overlay, muted: !!w.muted,
      session: w.active ? {
        state: connected ? 'connected' : asks ? 'approval' : wup > 1 ? 'punching' : 'joining',
        status: connected ? `Connected to ${w.hostName} (IPv6 [2400:aaaa::1]:51234, ping 38 ms)` : asks ? `Waiting for ${w.hostName} to answer` : `Connecting to ${w.hostName}`,
        path: connected ? 'IPv6 · ping 38 ms' : '', couldHelp: false,
        net: connected ? { lossPercent: 2.1, lostForGood: 3, mbps: 39.2, rttMs: 38 } : null,
      } : null,
      player: w.active && connected ? {
        state: playing ? 'playing' : 'waiting', text: playing ? 'Playing' : 'Connecting…', attempt: 1, uptimeSec: Math.max(0, wup - 7),
        cacheSeconds: playing ? 1.5 + wobble * 0.2 : null, drops: 0, decoderDrops: 0, fps: playing ? 60 : null,
        bitrateMbps: playing ? 39.2 + wobble : null, hwdec: playing ? HWDEC_MODES[db.receiver.hwDec].name.split(' ')[0] : '',
        width: playing ? 3840 : null, height: playing ? 2160 : null, fullscreen: false,
        source: playing ? '3840x2160 p010 bt.2020 pq' : '', output: playing ? 'SDR' : '', hdr: playing,
        guard: playing ? { frozen: wobble > 0.9, losses: 3, freezes: 2, heldBack: 95 } : null,
      } : null,
      control,
    },
    live: { hosts, error: null, loaded: true },
    group: db.group,
    firewall: db.firewall,
    accessRev: db.access.rev,
    deviceCandidates: db.seen.concat([{ key: 'k-desk', code: 'H2C8-5RVJ-9MZ4', name: 'NiseMono desktop', source: 'online' }])
      .filter((c) => !db.access.devices.some((d) => d.key === c.key)),
  };
}

function scheduleViewers() {
  const people = [
    { sid: 's1', name: 'Hana', deviceCode: 'B7WP-3N6S-KCE1', verified: true, ipv6: false, path: '198.51.100.13:62909', rttMs: 74, latencyMs: 3500, overheadPercent: 50, control: 'none',
      net: { lossPercent: 3.4, lostForGood: 12, mbps: 43.6, rttMs: 76 } },
    { sid: 's2', name: 'Leo', deviceCode: '', verified: false, ipv6: false, path: '198.51.100.3:20834', rttMs: 81, latencyMs: 3500, overheadPercent: 50, control: 'requested',
      net: { lossPercent: 0.4, lostForGood: 0, mbps: 43.6, rttMs: 83 } },
    { sid: 's3', name: 'Mika', deviceCode: 'P0VD-2KQN-71XA', verified: true, ipv6: true, path: '[2400:aaaa::7]:50112', rttMs: 12, latencyMs: 1500, overheadPercent: 25, control: 'none',
      net: { lossPercent: 0, lostForGood: 0, mbps: 43.6, rttMs: 12 } },
  ];
  people.forEach((p, i) => setTimeout(() => {
    if (db.share.state === 'streaming' || db.share.state === 'starting') {
      db.viewers.push({ ...p, joinedMs: now() });
      emit('log', { lines: [`[p2p] ${p.name} is joining (latency ${p.latencyMs} ms)`] });
    }
  }, 2500 + i * 2600));
}

function knock() {
  db.knocks.push({ sid: 'k' + now(), name: 'Mika', deviceCode: 'P0VD-2KQN-71XA', verified: true, sinceMs: now() });
  db.seen.unshift({ key: 'k-mika', code: 'P0VD-2KQN-71XA', name: 'Mika', source: 'asked' });
  emit('log', { lines: ['[p2p] Mika asks to join from device P0VD-2KQN-71XA', '[app] Mika asks to watch this computer: waiting for your answer'] });
}

function startStream(live) {
  db.share = { state: 'starting', live, detail: 'Waiting for the capture engine…', startedMs: now() };
  setTimeout(() => { if (db.share.state === 'starting') { db.share.state = 'streaming'; db.share.detail = 'Streaming'; } }, 1400);
}

const api = {
  app_init() {
    return {
      version: '0.3.1', platform: mac ? 'macos' : 'windows', locale: navigator.language, backdrop: false, defaultName: 'NiseMono',
      ui: db.ui, sender: db.sender, receiver: db.receiver, group: db.group, outputs: db.outputs, access: db.access, device: DEVICE,
      audioDevices: [{ id: '{0.0.0.00000000}.{a}', name: 'Speakers (Realtek USB Audio)' }, { id: '{0.0.0.00000000}.{b}', name: 'AW3225QF (NVIDIA High Definition Audio)' }],
      encoders: mac
        ? [{ id: 5, name: 'Apple VideoToolbox (HEVC)' }, { id: 4, name: 'x265 (software)' }]
        : [{ id: 0, name: 'NVIDIA NVENC (HEVC)' }, { id: 2, name: 'AMD AMF (HEVC)' }, { id: 3, name: 'Intel Quick Sync (HEVC)' }, { id: 4, name: 'x265 (software)' }],
      audioCodecs: [{ id: 1, name: 'AAC' }, { id: 2, name: 'Opus' }],
      latencyPresets: LATENCY_PRESETS,
      toneMappings: ['bt.2390', 'auto', 'spline', 'bt.2446a', 'st2094-40', 'st2094-10', 'hable', 'mobius', 'reinhard', 'clip'],
      hwDecModes: HWDEC_MODES,
      tools: { ffmpeg: { path: mac ? '/opt/homebrew/opt/ffmpeg-full/bin/ffmpeg' : 'D:\\HdrCast\\tools\\ffmpeg.exe' }, mpv: { path: mac ? null : 'D:\\HdrCast\\tools\\mpv.exe' } },
      firewall: db.firewall,
      paths: { config: mac ? '~/Library/Application Support/HdrCast' : 'C:\\Users\\me\\AppData\\Roaming\\HdrCast', logs: '' },
      caps: { audioExclude: !mac, clipboardWatch: !mac },
      status: status(),
      startupInvite: null,
      startHidden: false,
      autostart: db.autostart,
    };
  },
  autostart_get: () => db.autostart,
  autostart_set({ on }) {
    db.autostart = { supported: true, on, other: null, disabled: false };
    return db.autostart;
  },
  tray_texts: () => null,
  settings_save({ settings }) {
    db.ui = settings.ui; db.sender = settings.sender; db.receiver = settings.receiver;
    return { ...settings, tools: null };
  },
  access_get: () => db.access,
  access_save({ access }) {
    db.access = { ...access, rev: db.access.rev, devices: access.devices.map((d) => ({ ...d, code: d.code || '????-????-????' })) };
    return db.access;
  },
  knock_answer({ sid, allow, remember }) {
    const k = db.knocks.find((x) => x.sid === sid);
    db.knocks = db.knocks.filter((x) => x.sid !== sid);
    if (k && remember) {
      db.access = { ...db.access, rev: db.access.rev + 1, devices: [{ key: 'k-mika', code: k.deviceCode, name: k.name, connect: allow ? 'allow' : 'block', control: db.access.others.control, lastSeenMs: now() }].concat(db.access.devices) };
    }
    if (k && allow) {
      if (db.share.state === 'idle') startStream(false);
      setTimeout(() => db.viewers.push({ sid, name: k.name, deviceCode: k.deviceCode, verified: k.verified, ipv6: true, path: '[2400:aaaa::7]:50112', rttMs: 12, latencyMs: 1500, overheadPercent: 25, control: 'none', joinedMs: now() }), 1500);
    }
    return db.access;
  },
  share_kick({ sid }) { db.viewers = db.viewers.filter((v) => v.sid !== sid); },
  outputs_refresh: () => db.outputs,
  audio_apps: () => ['Discord.exe', 'chrome.exe', 'Spotify.exe'],
  async encoders_probe() {
    await new Promise((r) => setTimeout(r, 1800));
    return { recommended: mac ? 5 : 0, results: [] };
  },
  share_start() {
    startStream(true);
    db.viewers = [];
    emit('log', { lines: ['[app] NVIDIA NVENC (HEVC) 40 Mbps 3840x2160@60 HDR10', '[p2p] candidates: [2400:aaaa:8ae2::1c]:51234, 192.168.1.3:51235', '[run] HdrCast.exe capture --output 0 --fps 60 --format p010 …'] });
    scheduleViewers();
    return { ok: true };
  },
  share_stop() {
    db.share = { state: 'idle', live: false, detail: 'Stopped', startedMs: 0 };
    db.viewers = [];
    db.knocks = [];
    emit('log', { lines: ['[app] stopped sharing; still available to the group'] });
  },
  control_set({ sid, allow }) {
    const v = db.viewers.find((x) => x.sid === sid);
    if (v) v.control = allow ? 'granted' : 'none';
  },
  live_refresh: () => null,
  watch_start({ hostId }) {
    const h = hosts.find((x) => x.hostId === hostId);
    db.watch = { active: true, hostId, hostName: h.name, startedMs: now(), want: null, wantAt: 0, latencyMs: db.receiver.latencyMs, overlay: false };
    emit('log', { lines: [`[app] watching ${h.name} (latency ${db.receiver.latencyMs} ms)`] });
    return { ok: true };
  },
  watch_stop() { db.watch = { active: false, hostId: '', hostName: '', startedMs: 0, want: null }; },
  watch_control({ want }) { db.watch.want = want; db.watch.wantAt = (now() - db.watch.startedMs) / 1000; },
  watch_fullscreen: () => null,
  watch_set_hwdec({ hwDec }) {
    db.receiver = { ...db.receiver, hwDec };
    emit('receiver', db.receiver);
    return db.receiver;
  },
  async watch_set_latency({ latencyMs }) {
    db.receiver = { ...db.receiver, latencyMs };
    emit('receiver', db.receiver);
    const w = db.watch;
    db.watch = { ...w, active: false };
    await new Promise((r) => setTimeout(r, 1500));
    db.watch = { ...w, latencyMs, startedMs: now() };
    return { ok: true };
  },
  watch_set_mute({ muted }) { db.watch = { ...db.watch, muted }; },
  overlay_init() {
    return {
      platform: mac ? 'macos' : 'windows', ui: db.ui, receiver: db.receiver, latencyPresets: LATENCY_PRESETS, hwDecModes: HWDEC_MODES,
      expanded: q.has('panel') || !!db.watch.overlay, status: status(),
    };
  },
  overlay_toggle() {
    db.watch = { ...db.watch, overlay: !db.watch.overlay };
    emit('overlay', { expanded: db.watch.overlay });
  },
  overlay_expand({ expanded }) { db.watch = { ...db.watch, overlay: expanded }; },
  overlay_fit: () => null,
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
setTimeout(() => emit('log', { lines: ['[app] settings: C:\\Users\\me\\AppData\\Roaming\\HdrCast', '[app] group: "Friends" as NiseMono', `[app] this computer's device code: ${DEVICE}`] }), 300);
if (q.has('live')) setTimeout(() => api.share_start(), 200);
if (q.has('watching')) setTimeout(() => api.watch_start({ hostId: 'h3' }), 200);
if (q.has('knock')) setTimeout(knock, 1500);
