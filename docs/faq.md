# HdrCast FAQ

**English** | [简体中文](faq.zh-CN.md)

## It does not connect. What now?

The panel on the **Watch** page says why. Most of its messages stay in English whatever language the app is set to, so
they can be passed on as they are, and they call the sharer "the host". [network.md](network.md) §1.2 goes through each
one.

- **"This PC has no IPv6…" / "The host's PC has no IPv6…"**: whichever side has no IPv6 turns it on in its router
  ([network.md](network.md) §2); <https://test-ipv6.com> shows where you stand. A phone hotspot usually comes with IPv6,
  which makes for a quick test.
- **"Both sides have IPv6 but the path is blocked"**: turn off VPNs and game accelerators; if the router has a strict
  option such as "IPv6 firewall: block all incoming", set it back to the default.
- **"…the host's PC only takes IPv4 from routers that keep ports…"**: the viewer's router uses a new port for every
  destination, and the sharer's side is not open. The sharer clicks **Allow…** on the **Firewall** line and makes sure
  UPnP is on in their router (while they share, the panel on their **Share** page says what is missing).
  **"…although the host takes IPv4 from any port…"**: a VPN, proxy or game accelerator on the viewer's side takes the UDP.
- **"Not live right now"**: HdrCast is not running on the other computer (or **Available to connect** is off there); the
  list refreshes every 10 s. **"… is full"**: the sharer's **Viewers at once** is reached.
- **WAITING, "Waiting for … to let you in…"**: the sharer has to say yes first (under **Who Can Connect** your computer is
  on **Ask Me**), or their computer is still starting to capture. **"… did not let you in"**: they said no, set your
  computer to **Never**, or have just disconnected you (then you cannot connect again for a minute). **"Nobody at …
  answered your request"**: nobody clicked **Allow** within 50 s.
- **To use another computer of your own from anywhere**: on that computer, **Share → Who Can Connect → Add a Computer…**
  → pick this one → **My Computer** (compare the device code first). From then on nothing has to be clicked over there;
  turn on **Settings → General → Start HdrCast when I sign in** there as well, so that it is online after every restart.
- **"The signaling server cannot be reached"**: this computer is offline, or the signaling server is down; people already
  watching are not affected.
- It connects, but **the picture never shows up**: mpv waits for the first keyframe (at most one keyframe interval, 1 s by
  default). After more than 10 s, look for errors in the `[remux …]` lines on the sharer's **Activity** page.

## Is IPv6 a must?

Peer-to-peer rests on IPv6 hole punching: IPv6 has no NAT, so when both sides send at the same moment, the packets get
through the stateful firewalls on both sides, with nothing to set up.

Without IPv6 the app uses IPv4: it learns its public address mapping from a UDP echo on the signaling server and punches
through that. As long as **one** side's router has a public IPv4 address and is "open" (the UPnP forward the app sets up
by itself, plus **Allow…** on the **Firewall** line), the other side can connect even from behind a symmetric NAT, the
kind common in China (tested on 2026-09-25: a friend on China Telecom in Jiangsu connected to a sharer in Japan). When
both sides sit behind carrier-grade NAT (some home broadband in China, v6プラス / DS-Lite in Japan, the IPv4 of some
Australian ISPs), it cannot get through, and IPv6 is the way. There is deliberately no relay through Tencent Cloud, so in
that case HdrCast says so plainly instead of quietly taking a detour. People on the same local network do not need IPv6
(they connect over the LAN's IPv4).

## What does the Tencent Cloud server see?

Only who is online or live (the name, candidate addresses, bitrate, number of viewers and, from 0.3.1, the computer's
public device key) and the join and answer messages, all signed with the group key. It sees neither the group key nor the
SRT passphrase, and never touches the video. The worst somebody who took over the server could do is keep people from
connecting; they could not send viewers anywhere else. More in [network.md](network.md) §6.

## Why no AV1?

ffmpeg's MPEG-TS muxer (7.1 and master) does not support AV1: it writes it as a private stream (0x06), which reads back as
`bin_data`. Peer-to-peer delivery is built on MPEG-TS (a viewer who joins midway picks the stream up from the headers TS
repeats), so it can only be **HEVC Main10**. The "direct mode" of v0.1 could carry AV1 in Matroska, but it needed one
side to forward a port and was removed together with the relay mode. The day ffmpeg can put AV1 into MPEG-TS, the test
`av1_really_cannot_be_muxed_into_mpegts` starts failing: that is the signal to think again.

## Why does a viewer have to switch HDR on in Windows first?

mpv's HDR passthrough (`--vo=gpu-next --gpu-api=d3d11 --target-colorspace-hint=yes`) only tells the DXGI swap chain "what
I output is PQ / BT.2020"; **it does not switch Windows into HDR mode by itself** (the old advice that mpv turns HDR on
for you no longer holds since NVIDIA's R560 drivers). With HDR off in Windows, that hint is ignored or gives an odd
picture (grey, or too dark). Switch HDR with `Win + Alt + B`, or in Settings → System → Display → Use HDR. **Watch →
Output → Automatic** only uses passthrough when it finds an HDR output and tone-maps otherwise, so forgetting it does not
give you a black screen, just a picture that is not real HDR.

Static HDR metadata only reaches the display in **full screen** (a Windows limitation): a window is HDR as well, but the
display does not switch into its HDR mode. For real full screen use the **Full Screen** button (`--fs`, not
`--d3d11-exclusive-fs`).

## The viewer has an SDR display

Two choices:

1. **Tone mapping in mpv (recommended)**: set **Watch → Output** to **SDR**
   (`--target-colorspace-hint=no --tone-mapping=bt.2390`; **Tone mapping** offers other curves). mpv maps PQ to SDR on
   the GPU, which looks close to the sharer's SDR desktop, with the highlights compressed.
2. The sharer sets **Share → HDR** to **Always SDR**: the desktop is captured as SDR (nv12 / BT.709) and everybody watches
   SDR, but the brightness of the sharer's own HDR desktop gets clipped when it is captured as SDR. Only use it when every
   viewer is on SDR.

## Which bitrate?

Rules of thumb for 10-bit HEVC at 60 fps, a mix of desktop and games:

| Resolution | Enough | Good | Best (beyond this NVENC gains nothing) |
|---|---|---|---|
| 1920×1080 | 12 Mbps | 20 Mbps | 30 Mbps |
| 2560×1440 | 20 Mbps | 35 Mbps | 50 Mbps |
| 3840×2160 | 40 Mbps | 60 Mbps | 80-100 Mbps |

- A desktop full of text needs more bitrate than a game (lots of fine detail); fast-moving games only stay sharp with a
  high bitrate.
- Sharing a game from a 4K display, setting **Share → Resolution** to 1440p usually looks sharper than 4K at a low
  bitrate, and takes half the bandwidth.
- The ceiling is set by **the sharer's upload ÷ (viewers × 1.25)**, not by how good you would like it to look: see the
  next question.
- Keep the keyframe interval at 1-2 s (1 s by default since 0.3.2, **Share → More options → Keyframe interval**): a
  viewer who joins sees nothing until the next IDR frame, and a picture damaged on the way is held until the next one
  (see below). With **mpv direct**, mpv prints "Could not find ref with POC" until then, which is normal.

## How much upload does sharing take?

The sharer's computer sends every viewer a copy of their own, so:

```
upload needed ≈ bitrate × viewers × 1.25   (SRT overhead oheadbw=25 %; ×1.5 for a viewer at oheadbw=50 %, the China preset)
```

- 40 Mbps × 2 viewers = 100 Mbps; 40 Mbps × 3 viewers (one of them in China) ≈ 160 Mbps.
- Measure your upload with a speed test first and keep 30 % spare. When it is not enough, lower the bitrate or the number
  of viewers; do not lower oheadbw (Haivision warns that with too little overhead the stream stalls as soon as the line
  loses packets).
- The **Share** page works out the upload for **Viewers at once**; fill in **My upload speed** and the estimate turns red
  above 80 % of it. Above the **Watching** list, "upload ≈ … Mbps" shows what the viewers connected right now need.
- Home broadband in China often has 30-100 Mbps of upload, NBN in Australia 20-50 Mbps: when a friend there shares with
  two or three people, the bitrate usually has to come down to 10-20 Mbps.

## It stutters or drops: what to check, in order

1. `No room to store incoming packet` in the viewer's player log (**Activity → Player**) → the receive buffer is too
   small: raise **Settings → Highest bitrate to expect** (it sizes rcvbuf / ffs), see [network.md](network.md) §4.
2. Packet loss on the way: the viewer stops, adds 500-1000 ms under **Watch → Delay → Custom** and connects again (the
   sharer uses this value for the connection to that viewer); if that does not help, lower the bitrate. When the sharer's
   upload falls short, every viewer stutters at once: check "upload ≈ … Mbps" above the **Watching** list on the sharer's
   **Share** page.
3. On the sharer's **Share** page the **Encoder** speed stays below 1.0×, or **Dropped frames** keeps climbing → the
   encoder cannot keep up: lower the resolution or the frame rate, or set **More options → Encoder preset** to **Speed**.
   (**Dropped frames** counts the frames the encoder dropped as well as those the capture could not hand over because
   ffmpeg was not reading, which is usually the same problem.)

## The picture freezes for a moment (it used to break up into coloured blocks)

When part of the stream is lost on the way and SRT cannot get it sent again within the viewer's delay, a few packets of
the stream are simply missing. The decoder does not notice: before 0.3.2 the broken picture, and every picture after it
up to the next keyframe, came out smeared and blocky. Since 0.3.2 the viewer's HdrCast holds the last good picture until
the next keyframe instead (at most one keyframe interval, 1 s by default; the sound goes on), like Moonlight does.

- It needs **Settings → Player input → Automatic (ffmpeg when found)** (the default) and the ffmpeg that comes with
  HdrCast: with **mpv direct**, damaged pictures are shown as before. The **Watch** page then says how often the picture
  was held, and the viewer's **Activity** page has `[guard]` lines.
- How lossy the path is: **loss … %** under the bitrate on the **Watch** page, and on the sharer's **Share** page next to
  each viewer **… % lost on the way** / **… too late to resend**. Both logs get a `[net]` line every 30 s while packets
  are lost: `…% lost on the way, n never recovered in time` (viewer) is what actually breaks pictures.
- Frequent freezes: the viewer stops and adds 500-1000 ms under **Watch → Delay → Custom** (or picks **China**, 3.5 s);
  if "never recovered" stays high, the sharer lowers the bitrate. Lots of "too late to resend" on the sharer's side
  means its upload or its bitrate is the problem.
- Since 0.3.2 the sharer's encoder also keeps its bitrate steadier (a 0.25 s rate buffer instead of 2 s) and SRT is told
  the stream's real rate, so fewer packets are lost in the first place: both sides should update.
- Shorter freezes (0.3.3): with an NVIDIA GPU the sharer turns on **Share → More options → Keyframe on request**. HdrCast
  then drives NVENC itself, and a viewer on 0.3.3 that loses part of the stream gets a fresh keyframe at once instead of
  at the next scheduled one: the picture usually holds for a few tenths of a second rather than up to a second (both logs
  get `[keyframe]` lines). Without NVENC, or when that does not work on the sharer's PC, ffmpeg encodes as before.

## Where does the sound come from? Do I need a virtual audio device?

No. On Windows the capture engine records the mix of the default playback device with WASAPI loopback (while playing
silence to it, so that the audio engine does not go to sleep), resamples it to 48 kHz stereo f32, lines it up with the
video on the same QPC clock and hands it to ffmpeg through a named pipe; ffmpeg encodes AAC at 192 kbps (or Opus). On a
Mac the system sound comes from ScreenCaptureKit, in the same stream as the picture. **Share → Sound** has a switch for
**Computer audio**, and on Windows an **Output device** to record another device than the default.

## We are all in one voice chat: will the viewers hear their own echo?

Not if the sharer picks the voice chat app under **Share → Sound → Leave out** (for example `Discord.exe`; the list shows
the apps playing sound right now, or type the name). Its sound, and that of the processes it starts, stays out of the
stream. It is the same Windows interface as Parsec's echo cancelling (Windows 10 21H1 or newer), and it leaves out one app
at a time. A sharing Mac cannot leave an app out.

## Why not capture with ffmpeg's own ddagrab?

ddagrab gets correct scRGB FP16 frames, but ffmpeg has no lossless way to hand packed FP16 to zscale or libplacebo:
swscale clips every value above 1.0 (above 80 nits), as tests with three versions showed, so HDR gets squashed into SDR.
HdrCast converts scRGB → PQ / BT.2020 → P010 in a D3D11 compute shader of its own, on the GPU, and only has ffmpeg encode
the P010, which is also what OBS does. Details in [design.md](design.md) §1.1 (in Chinese).

## Remote control does nothing in some windows. Why?

Windows only lets a program type into another program's windows when it has at least that program's rights (UAC's
"User Interface Privilege Isolation"). HdrCast runs with a normal user's rights, so while a window of a program that was
started **as administrator** is in front, Windows throws the viewer's mouse and keyboard away without a word. Parsec and
Sunshine do not have the problem because their installers add a Windows service that runs with higher rights.

HdrCast does the same. On the sharing PC, switch on **Settings → General → Control programs that run as
administrator**. Windows asks for permission once. That puts one small program, `HdrCastInput.exe`, into
`C:\Program Files\HdrCast\Input` and registers it as a Windows service, **HdrCastInput**, which runs as the system
account. A service cannot reach your desktop, so while somebody controls your computer HdrCast starts the service (you
are the only user who may) and the service starts the same program inside your session, where it types what HdrCast
sends it. Nothing runs the rest of the time: the service is not started at boot, and both stop about 45 seconds after
the last viewer. The same switch takes it away again.

- Without it, **Activity** says so when it notices: *Windows refuses the viewer's mouse and keyboard right now…*.
- It is the trade Parsec makes, too: any program running as you can ask the program to type, and so can reach windows
  of programs run as administrator without Windows' prompt. The program has every privilege taken off, only types, and
  only on your own desktop. If that is more than you want, leave it off, or run the program you want to control
  without administrator rights.
- Not covered: Windows' permission prompts (UAC) and the lock screen. They live on a separate desktop that neither
  HdrCast's capture nor its input can reach, and reaching it would let any program of yours press "Yes" on its own prompt.
- HdrCast is not code-signed, and an unsigned program that registers a service can look suspicious to Windows Security.
  If it reports `HdrCastInput.exe`, please say so in an issue instead of allowing it on the device.

## How safe is it?

- The 32-byte group key in the invite is the root of everything: the signaling auth key, the message signing key, the SRT
  passphrase (AES-256, `pbkeylen=32`) and each session's punch key are derived from it. **Whoever has the invite can join
  the group and watch**, so only send it to people you trust; to lock somebody out, start a new group and send the new
  invite to everybody else.
- The video is AES-256 encrypted all the way from the sharer's remux process to the viewer's mpv. Punch packets carry an
  HMAC; data from any address that has not passed it is dropped, so it never reaches the ffmpeg on the sharer's computer.
- From 0.3.1 every computer also has a device key of its own (in `device.json`: never send it anywhere, never copy it to
  another computer). The rules under **Who Can Connect** follow that key rather than a name anybody in the group could
  type, and remote-control input between two 0.3.1 computers is encrypted with a key only those two can work out.
- The signaling server only sees signed metadata (see "What does the Tencent Cloud server see?" above).

## Is the "ping 38 ms" the delay?

No. That is the round trip of the punched path (a network ping). The delay of the picture ≈ the viewer's SRT delay + the
player buffer (1.5 s, **Settings → Player buffer**) + encoding and decoding (about 0.5 s): measured at 3.6 s with the
Japan preset (2026-09-24, one PC over loopback, [network.md](network.md) §3), about 5.6 s with the China preset. The SRT
delay is the time window for retransmissions: it decides how much packet loss the stream rides out, **not the picture
quality**, which only depends on the bitrate and the encoder preset. On a good line a long delay and a short one look the
same; one is just a few seconds later.

## Can the delay be shorter?

For using the other computer, yes: connect in **Control mode** (0.3.7, **Watch → Connection → Mode → Control**). Nothing
is buffered and nothing is sent twice, the way Moonlight works: measured screen to screen on one PC at 120 fps, the
picture is 14.5 ms behind at 1080p and 21 ms in 4K HDR, plus the network's one-way delay. The sharer needs an NVIDIA GPU, and the line
should be good: a lost picture freezes the screen until a keyframe comes, so a lossy line freezes often (see the README's
Control mode section).

For watching, HdrCast is built like a broadcast. Pick **Fast** (300 ms) under **Watch → Delay**, or a **Custom** delay of about
4 × RTT (Japan about 200 ms, Australia about 700 ms), and lower **Settings → Player buffer** (mpv's `cache-pause-wait`) to
0.5 s: the whole delay comes down to about 1.5 s, but the picture stalls whenever the line hiccups. Going below 3 × RTT
is not recommended.
