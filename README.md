# HdrCast

**English** | [简体中文](README.zh-CN.md)

Share your desktop, in HDR10 and with the computer's sound, with friends far away (it was made for friends in China, Japan
and Australia) at 10-100 Mbps and 1.5-5 s behind live, with a picture that stays sharp and smooth. It is made for a private
group and works **peer-to-peer**: anyone in the group can share, and the others receive the stream straight from the
sharer's computer. No relay server, no public IPv4 address, no port forwarding to set up. Runs on Windows and macOS.

- **One app.** `HdrCast` has a Share page and a Watch page that use the same group and invite. Since 0.3 it is written in
  Rust (Tauri 2 UI) and replaces the `HdrCastSender.exe` / `HdrCastReceiver.exe` of 0.2. The protocol did not change, so
  friends still on 0.2.4 can watch and share with you.
- **Peer-to-peer over IPv6.** The two computers swap addresses through a small signaling server, then send to each other at
  the same moment, so both routers and firewalls let the stream through (hole punching). The signaling server only passes
  a few hundred bytes of signed metadata (who is live, candidate addresses); **the video never goes through it**.
- **Works without IPv6, too.** Over IPv4, HdrCast punches through the public address mapping. When the other side's router
  is a symmetric NAT (a new port for every destination, common in China), HdrCast forwards its own port on your router with
  UPnP and, after one click on **Allow…** for Windows Firewall, punches back to the other side's real port (see
  [docs/network.md](docs/network.md) §1.1).
- **One invite, good for ever.** Whoever starts the group sends friends a message with an `hdrcast:…` line; a friend copies
  the whole message, opens HdrCast and is asked to join.
- **HDR10 all the way:**
  - Windows: DXGI desktop duplication → D3D11 compute shader (scRGB → PQ / BT.2020 / P010) → NVENC / AMF / QSV / x265 HEVC Main10
  - macOS: ScreenCaptureKit (HDR capture from macOS 15) → VideoToolbox HEVC Main10
  - → SRT (AES-256, retransmission per viewer) → mpv with HDR passthrough. On an SDR display mpv tone-maps.
- **Several viewers at once.** One encode; each viewer gets a remux-only ffmpeg (no re-encoding) that retransmits for that
  viewer's distance. A viewer with a bad connection only affects themselves.
- **The computer's sound.** No virtual audio device, on the same clock as the picture. On Windows you can leave one app's
  sound out (your voice chat, say, so friends do not hear their own echo).
- **Connect without going live (0.3.1).** While the app runs, the computer shows as **Online** in the group. Somebody clicks
  **Connect**; once you allow it (or a rule does), capture starts, and it stops after the last viewer leaves. Like Parsec,
  you can connect back to your own computer at any time.
- **Remote control by computer (0.3.1).** Every computer has its own device code, and the sharer decides per computer who
  may watch without asking and who may control: your own laptop gets control as soon as it connects, friends only watch,
  anybody else has to ask every time. Input takes the same peer-to-peer path, end-to-end encrypted with a key only those
  two computers can work out.
- **English, Chinese and Japanese** in the app (**Settings → Language**; Automatic follows the system).

> **Status (0.3.1):** on Windows, capture, encoding, peer-to-peer, playback and the UI have been tested on real machines.
> **The macOS code has only been type-checked on Windows and has never run on a Mac**: for a first run, go through
> [docs/macos-checklist.md](docs/macos-checklist.md) step by step.

## What you need

- **Windows 10 or 11** (64-bit), or **macOS 13** or newer. Sharing HDR from a Mac needs macOS 15 and Apple silicon.
- **IPv6**, ideally. Peer-to-peer works best over IPv6 (it has no NAT, so the two computers can reach each other). Check at
  <https://test-ipv6.com>: "10/10", or at least "IPv6 address detected", is what you want. No IPv6? It can often be
  switched on in the router, or your ISP can enable it. Without IPv6 HdrCast uses IPv4, which works as long as the
  sharer's side is open (next point).
- **Windows Firewall: one click.** If the **Firewall** line in the app offers **Allow…**, click it once and confirm the
  Windows prompt. Some routers (common in China) give every destination a new port, and a PC only lets such packets in when
  Windows Firewall allows HdrCast. The rule is tied to the program's path, so every new version unzipped into a new
  folder needs this click again (the app asks). The app opens its port on the router by itself (UPnP) while it is
  online or plays, and closes it again when it stops.
- **ffmpeg** (to share) and **mpv** (to watch), built with SRT support. Windows: both already come in the `tools\` folder of
  the release. macOS: `brew install ffmpeg-full mpv` (Homebrew's plain `ffmpeg` has no SRT). See [Tools](#tools).
- A GPU that decodes 10-bit HEVC in hardware (NVIDIA GTX 10 series or newer, AMD RX 400 or newer, Intel 7th gen or newer,
  any Apple silicon Mac).
- Download bandwidth of about 1.3 × the stream's bitrate (a 40 Mbps stream needs about 50 Mbps). Sharing needs one stream's
  worth of upload per viewer (see [Sharing](#sharing)).

## Getting started

### Install

- **Windows:** unzip `HdrCast-<version>.zip` anywhere and run `HdrCast.exe`. ffmpeg, ffprobe and mpv are already in its
  `tools\` folder. (Friends usually get the zip from whoever runs the group; to make one, see [Building](#building).)
- **macOS:** drag `HdrCast.app` to Applications, then install the tools: `brew install ffmpeg-full mpv`. The first time you
  share, macOS asks for the Screen & System Audio Recording permission.

### Start or join a group

- **Start a group:** on the first start choose **Start a group**, then a group name and your name. The invite is copied to
  the clipboard: paste it into your WeChat / LINE / Discord group, and **only send it to people you trust**: anybody who has
  it can watch.
- **Join one:** **copy the whole invite message** a friend sent you, then open HdrCast. It asks whether to join: say yes and
  type the name your friends will see. If it does not ask, choose **I have an invite** (or **Settings → Paste Invite**).

### Watching

1. **Watch** lists the computers in the group that are online (refreshed every 10 s). Pick where you watch from under
   **Delay**: Japan 1.5 s, Australia 2 s, China 3.5 s.
2. A computer marked **LIVE** is broadcasting: click **Watch**. One that is only **Online** starts sharing when you click
   **Connect**, and its owner may have to let you in first (you see **WAITING**).
3. On an **HDR display**, switch HDR on first (Windows: `Win + Alt + B`). On an SDR display, set **Output** to SDR.
4. When it does not connect, the panel says why, for example "This PC has no IPv6 internet connection". See
   [If something is wrong](#if-something-is-wrong).

The delay is there on purpose: this is a broadcast that rides out a bumpy connection, not a remote desktop. For remote
control, use the **Fast** preset (see below).

### Sharing

1. On **Share**, pick the display, the bitrate and **Viewers at once** (and, if you like, **My upload speed**, to be warned
   before the stream outgrows your connection), then click **Go Live**.
   Every viewer costs one full stream of your upload: 3 viewers at 40 Mbps need about 150 Mbps up, retransmission included.
2. The panel at the top shows the bitrate, frame rate and dropped frames. The list below shows who is watching, over IPv6 or
   IPv4, their network ping (not the picture delay) and each viewer's delay setting.
3. In a voice chat with the group? On Windows, pick it under **Sound → Leave out** (for example `Discord.exe`) so
   your friends do not hear themselves come back through the stream. Needs Windows 10 21H1 or newer; one app at a time.
4. You do not have to go live at all: see the next section.

## Connect any time, and remote control

**Online or live.** While HdrCast runs and **Share → Who Can Connect → Available to connect** is on (the default), the
computer shows as **Online** in the group. When somebody clicks **Connect**, the rules below decide; the screen is only
captured once somebody has been let in, and capture stops 20 s after the last one leaves. **Go Live** opens it to the whole
group instead: everybody not set to **Never** may watch without asking.

**Closing the window keeps you online.** While Available to connect is on, the window's close button only tucks HdrCast
into the notification area (the tray; the menu bar on a Mac), and the computer stays online. Click the tray icon (or
**Open HdrCast** in its menu) to get the window back; **Quit HdrCast** really goes offline, after saying goodbye to viewers
and the signaling server. With Available to connect off, closing the window quits, as before. A red dot on the tray icon
means somebody is watching this computer right now. Starting HdrCast again while it runs only brings its window back.

**Online from sign-in.** **Settings → General → Start HdrCast when I sign in** starts HdrCast without a window every time you
sign in, so the computer is online without you doing anything, like Parsec. On Windows it is a value named `HdrCast` under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (`"…\HdrCast.exe" --background`), which Task Manager also lists under
Startup apps; the switch shows the real state, and says so when Task Manager turned it off or when it starts an older
copy in another folder. On macOS it is `~/Library/LaunchAgents/app.hdrcast.login.plist`.
When somebody asks to watch (or for control) while the window is closed, the window comes to the front and its taskbar
button flashes: click **Allow** or **Deny**. It does not take the keyboard away from what you are typing in. A request
nobody answers is turned down after 50 s.

**Who can connect** (on the Share page):

- **Everyone else in the group:** Watch *Allow / Ask Me / Never*, Control *Off / Ask Me / Automatic*. The default is Ask Me
  and Off. Friends on HdrCast 0.3.0 or older cannot prove which computer they use, so this rule always applies to them.
- **One rule per computer.** Computers are recognised by their **device code** (like `7KQ2-M9XD-4TPA`), not by their name.
  **Add a Computer…** lists the computers in the group that are online or asked to connect lately: choose **My Computer**
  (connects without asking, gets control at once) or **Trusted Friend** (watches without asking, control only when you say
  yes). **Always Allow** on a request adds that computer, too.
- Every computer's code is in **Settings → Group → This computer**. Before you give a computer control, compare the code
  with the one on that computer.

**Your laptop, able to control your desktop from anywhere:**

1. Install HdrCast 0.3.1 or newer on both and join the same group (copy the same invite). Give them different names
   (**Settings → My name**, say "Mo – laptop").
2. On the desktop: **Share → Who Can Connect → Add a Computer…**, find the laptop (check its code), choose **My Computer**.
3. Later, anywhere: on the laptop open HdrCast → **Watch** → the desktop → **Connect**. Nothing has to be clicked on the
   desktop, and you have control as soon as you are connected. HdrCast has to be running on the desktop (its window may be
   closed): turn on **Start HdrCast when I sign in** there, and it is, after every restart.

**As a viewer:** where the sharer gives you control automatically, you are in control as soon as you connect: the player
window sends your mouse and keyboard to their computer (`Ctrl + Alt + Enter` stays with your player: full screen on/off).
**Just Watch** hands control back, **Take Control** takes it again. Where the sharer set Ask Me, click **Request Control**
and wait for them to allow it. The sharer can **Take Back** control or **Disconnect** you at any time (after a disconnect
you cannot connect again for a minute).

- The picture is still behind by the SRT delay plus the player buffer: for remote control pick the **Fast** preset (300 ms)
  and lower **Settings → Player buffer**. Good for using a computer and changing settings, not for games (there is no
  relative mouse).
- A sharing Mac needs HdrCast allowed under System Settings → Privacy & Security → Accessibility. On Windows, programs
  running as administrator ignore the injected input unless HdrCast runs as administrator, too.
- A sharer on 0.2.4 offers no remote control; a sharer on 0.3.0 has no device codes and no Online, and uses its own
  *Off / Ask Me / Allow* setting.

## If something is wrong

The panel on the Watch page says what happened. The usual ones:

| Message | What to do |
|---|---|
| This PC has no IPv6 internet connection | Turn IPv6 on in your router, or ask your ISP; test-ipv6.com shows the state. If it adds *the host offered no public IPv4 address*, IPv4 was never tried: tell whoever runs the signaling server that its UDP port 18651 is closed. |
| … the host's PC only takes IPv4 from routers that keep ports … | Your router changes ports and the sharer's side is closed: ask them to click **Allow…** next to Firewall and to check that UPnP is on in their router. Or turn IPv6 on. |
| … although the host takes IPv4 from any port … | Something on your computer grabs UDP: a VPN, proxy or game accelerator (Clash TUN, UU, …). Turn it off, or let HdrCast bypass it. |
| The host's PC has no IPv6 internet connection | The friend who shares needs IPv6 (same advice, their side). |
| Neither side has IPv6 … | At least one of you needs IPv6; the rest of the message says why IPv4 did not work. |
| Both sides have IPv6 but the path is blocked | A VPN or a very strict firewall drops incoming UDP. Turn the VPN off; in the router, look for "IPv6 firewall: block all". |
| Not live right now / Nobody is online right now | HdrCast is not running on your friend's computer (they quit it, or turned off Available to connect). The list refreshes by itself. |
| … did not let you in | The sharer said no, or set your computer to Never. Ask them. |
| Nobody at … answered your request | The sharer was not at their computer to click Allow within 50 seconds. |
| … is full | The sharer's **Viewers at once** is reached. |
| The signaling server cannot be reached | Your internet is down, or the signaling server is. People already watching keep watching. |
| Picture stutters, then recovers every few seconds | Raise the delay (China: try a custom 4000-5000 ms), or ask for a lower bitrate. |
| Colours look grey and flat on an HDR display | HDR is off in the system (Windows: `Win + Alt + B`). |
| mpv says it cannot open `srt://` | Your mpv has no libsrt: on Windows use the zhongfly / shinchiro build (get-tools.cmd does). Homebrew's mpv has none either: on a Mac keep **Settings → Player input** at **ffmpeg pipe → mpv** (the default there). |

More in [docs/faq.md](docs/faq.md). When you ask a friend for help, send them what **Activity → Copy** gives you:
passwords and invites are masked in it.

## Privacy and security

- The video is AES-256 encrypted end to end (SRT), with a key derived from the invite. Remote-control input is encrypted,
  too (ChaCha20-Poly1305); between two 0.3.1 computers with a key only those two can work out.
- **The invite is the key to the group:** only share it with people you trust. To lock somebody out, start a new group and
  send the new invite to the others.
- The signaling server never sees a password or group key, and it cannot forge addresses: every message is signed with
  the group key. If it goes down, people already watching keep watching; only new connections have to wait.
- `device.json` in the settings folder is this computer's identity: whoever has it can pretend to be this computer. Do not
  send it to anybody, and do not copy it to another computer.

## How it works

```
┌────────────────────── the sharer's computer ─────────────────────┐
│ HdrCast (Share)                                                  │
│   capture engine (the same program in capture mode) → pipe       │
│   → ffmpeg encodes HEVC Main10 → MPEG-TS (stdout)                │
│   → fan-out: a remux ffmpeg per viewer (-c copy, SRT, 127.0.0.1) │
│   → HostLink: [own IPv6]:port ⇄ hole punching ⇄ each viewer      │
└───────────────┬──────────────────────────────────┬───────────────┘
                │ peer-to-peer (hole punched)      │ peer-to-peer (hole punched)
                ▼                                  ▼
     HdrCast (Watch) → mpv (Japan)      HdrCast (Watch) → mpv (China / Australia)

   hdrcast-signal on Tencent Cloud (signal/): only "who is live + candidate addresses",
   all signed with the group key; never any video
```

- Whenever **both sides have IPv6**, they connect (IPv6 has no NAT, so hole punching nearly always works); IPv4 is an extra
  attempt.
- The sharer uploads a copy for every viewer: 3 viewers × 40 Mbps ≈ 150 Mbps up, with room for retransmission. The app
  estimates it and warns when it goes over the upload speed you entered.

## Signaling server (deployed once)

`signal/` is a small service that only needs the Python standard library. The one invites point to runs on Tencent Cloud
at `http://101.43.23.94:18650`; [signal/README.md](signal/README.md) covers deploying and upgrading it
(`signal/deploy.sh --ssh ubuntu@101.43.23.94`) and the two ports to open in the cloud's security group: **TCP 18650** (the
API) and **UDP 18651** (the address echo for IPv4 hole punching: optional, but without it nobody without IPv6 can connect).
It cannot read or forge anything (see [Privacy and security](#privacy-and-security)). 0.3 did not change it.

## Tools

| Tool | Who needs it | Windows | macOS |
|---|---|---|---|
| ffmpeg | whoever shares (on a Mac, whoever watches, too) | in the release's `tools\` (BtbN gpl build, with libsrt and NVENC / AMF / QSV) | `brew install ffmpeg-full` (with libsrt and VideoToolbox; the plain `ffmpeg` formula has no libsrt) |
| mpv | whoever watches | in the release's `tools\` (zhongfly build, with libsrt) | `brew install mpv` (no libsrt: it plays through the ffmpeg pipe, see below) |
| WebView2 | the app's UI | part of Windows 11; on Windows 10 it normally comes with Edge | not needed (the system's WebKit) |

On Windows, `scripts\get-tools.cmd` downloads or updates ffmpeg and mpv (`sender`, `viewer` or `both`; add `force` to
download again), and `scripts\check-tools.cmd` checks the encoders and `srt://` support.

On a Mac, HdrCast finds Homebrew's `ffmpeg-full` by itself (it is keg-only, so it is on no PATH) and takes it over a plain
`ffmpeg`. Homebrew's mpv has no libsrt, so on a Mac **Settings → Player input** is **ffmpeg pipe → mpv** by default:
ffmpeg-full receives the stream and hands it to mpv.

Settings and logs are in `%APPDATA%\HdrCast\` (macOS: `~/Library/Application Support/HdrCast/`). `group.json` is the same
file 0.2 used, so an upgrade keeps you in your group. 0.3.1 adds `device.json` (this computer's private device key: never
send it anywhere, never copy it to a new computer) and `access.json` (who can connect).

## Building

Needs [Rust](https://rustup.rs) 1.85 or newer. Windows also needs the C++ build tools of Visual Studio; macOS needs the
Xcode command line tools and `cargo install tauri-cli --version "^2" --locked`.

```bash
./build.sh                    # cargo test + the signaling server's Python tests → build → package into dist/
./build.sh --no-test          # skip the tests
./build.sh --tools DIR        # take ffmpeg / ffprobe / mpv from DIR (--no-tools: leave them out)
cargo run -p hdrcast          # run the app (debug build)
node scripts/ui-preview.mjs   # only the UI, in a browser with mock data (http://localhost:5179)
```

The result on Windows is `dist/HdrCast-<version>/` (`HdrCast.exe`, `tools/`, `scripts/`, `docs/`, `signal/`) and a zip of
the same name; on macOS `dist/HdrCast.app` and a dmg. To try a build without touching your real group and settings, point
the environment variable `HDRCAST_DATA_ROOT` at another folder.

## Repository layout

```
app/         the HdrCast app (Tauri 2): runs capture + ffmpeg and mpv, commands, window, tray; icons/
ui/          the UI (plain ES modules + preact/htm, no Node build); mock.js lets it run alone in a browser
crates/
  core/      platform-neutral basics: address parsing, SRT URLs, log redaction, child processes, firewall;
             groups, invites, keys, signed messages
  p2p/       hole punching, HostLink / ViewerLink, UPnP, fan-out, signaling client, P2pHost / ViewerSession
  capture/   capture engine: Windows (DXGI / D3D11 / WASAPI) and macOS (ScreenCaptureKit) back ends, one main loop
  media/     ffmpeg / mpv arguments and output parsing, encoder profiles, sender.json / receiver.json
  control/   remote control: protocol (ChaCha20-Poly1305), key mapping, input injection (SendInput / Quartz)
             (device keys, signatures and the control key are in core/src/group/device.rs; who can connect is in
             app/src/access.rs and app/src/share.rs)
signal/      signaling server (Python standard library) + systemd unit + deploy.sh
scripts/     Windows batch files: get-tools / check-tools / dump-check; ui-preview.mjs previews the UI
docs/        design.md (the spec; §8 is the peer-to-peer protocol), network.md, faq.md, windows- and macos-checklist.md
tests/vectors/                     golden protocol vectors shared by the Rust, Python and C# tests
sender/ receiver/ shared/ tests/   the C# sources of 0.2, kept as the reference (the Rust tests compare with their output byte for byte)
build.sh     test + build + package
```

## Documentation

design.md and the two checklists are in Chinese for now; network.md and faq.md have a Chinese version next to them
(`network.zh-CN.md`, `faq.zh-CN.md`).

- [docs/design.md](docs/design.md): the design, final; §8 is the peer-to-peer protocol (invites, key derivation, signatures,
  signaling API, punch packets, and in §8.9 the side channel remote control uses).
- [docs/network.md](docs/network.md): how peer-to-peer gets through firewalls, who needs what, upload bandwidth, checking
  IPv6.
- [docs/faq.md](docs/faq.md): when it does not connect (follow the panel), HDR and SDR, bitrate and bandwidth.
- [docs/windows-checklist.md](docs/windows-checklist.md), [docs/macos-checklist.md](docs/macos-checklist.md): step-by-step
  checks on real machines, with the expected output.
- [signal/README.md](signal/README.md): deploying the signaling server.

## License

MIT, see [LICENSE](LICENSE).
