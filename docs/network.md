# HdrCast networking

**English** | [简体中文](network.zh-CN.md)

HdrCast works **peer-to-peer**: the sharer's computer sends the stream straight to every viewer, and the signaling server
on Tencent Cloud only introduces them to each other. This page explains how that gets through routers and firewalls, who
needs what, how to check IPv6, the SRT delay presets for each region, the buffer formula and the upload bandwidth. The
protocol itself is in [design.md](design.md) §8 (in Chinese).

## 1. How peer-to-peer connects

```
sharer                                  hdrcast-signal                                   viewer
                                        (Tencent Cloud)
  │                                            │                                            │
  │── 1. online, candidates [IPv6]:51234 ─────►│                                            │
  │                                            │◄── 2. who is online? (every 10 s) ─────────│
  │                                            │◄── 3. join, candidates [IPv6]:40000 ───────│
  │◄── 4. somebody wants to watch ─────────────│                                            │
  │── 5. answer: OK, candidates … ────────────►│── 5. the answer ──────────────────────────►│
  │                                            │                                            │
  │◄═════════════ 6. both send PUNCH every 100 ms, answer each with PUNCH_ACK ═════════════►│
  │◄═══════════════════════ 7. SRT (AES-256) over the same UDP path ═══════════════════════►│
```

The server only keeps a few hundred bytes of signed metadata and never passes on any video; step 4 is a long poll that
returns the moment a join arrives. In step 6 both stateful firewalls see "we sent first" and let the other side's replies
in. The SRT ends of step 7 are local on both computers: the sharer's remux ffmpeg (127.0.0.1) ⇄ HostLink ⇄ the punched
path ⇄ ViewerLink ⇄ mpv (127.0.0.1).

- **Why IPv6 punches through**: IPv6 has no NAT, so every computer has a global address and a fixed port. Home routers,
  ONTs and Windows Firewall all handle IPv6 "statefully": they block incoming traffic nobody asked for and let in the
  replies to a flow that went out first. When both sides send to each other at the same moment, each has sent first, so
  each lets the other in. **Nobody forwards a port, and nobody needs a public IPv4 address.**
- **Candidate addresses**: for IPv6, the **stable** address of the network adapter that leads to the internet. Windows
  sends from a temporary address by default, but temporary addresses change every day, and the address the packets come
  from has to be the one that was advertised, so the socket is bound to the stable address. For IPv4: the LAN address
  (for people on the same network), the public mapping learned from the signaling server's UDP echo, and the port the
  router forwards with UPnP (§1.1).
- **Learned addresses**: when the other side's packets arrive from an address and port it never advertised (a symmetric
  NAT opens a new port for every destination), that address becomes a punch target too: the other side's NAT opened
  exactly that hole for us, so packets sent there get in.
- **Choosing the path**: the first path to get an ACK wins; if IPv4 gets through first, HdrCast waits another 200 ms and
  takes IPv6 if that works too. The sharer follows the viewer's choice.
- **One SRT connection per viewer**: the sharer runs a remux ffmpeg for every viewer (`-c copy`, no re-encoding) that
  retransmits with **that viewer's** delay and oheadbw, so a bad connection only affects the viewer who has it.
- **Online or live (0.3.1)**: step 1 is the same either way. While HdrCast runs with **Available to connect** on, the
  computer announces itself as online, and its screen is only captured once somebody has been let in. The sharer's side
  then starts capture and the encoder before it answers (usually 1-2 s), so that the viewer's mpv has a picture to pick
  up straight away, and it stops both 20 s after the last viewer leaves. When the sharer's rule for the viewer is
  **Ask Me** and the sharer is not live, the answer waits until the sharer clicks **Allow** (at most 50 s). **Go Live**
  announces the computer as live and keeps the encoder running until everybody who watched has left: 2 s later the
  broadcast ends and the computer is online again (offline, with Available to connect off), unless **End the broadcast
  when everyone leaves** is switched off. A viewer who joins again afterwards is served on request, without the sharer's
  answer if they watched in the last 3 minutes; a path that goes quiet for less than 15 s does not count as leaving (see
  Reconnecting).
- **Reconnecting**: a path that carries no packets for 15 s counts as lost, and the viewer joins again by itself (on the
  same local port, so mpv picks the stream up again without a restart). When the sharer stops, it sends BYE, and the
  viewer's panel says "The stream ended - waiting for … to go live again" (or "… ended the session - asking again" when
  the computer was only online).

### 1.1 Who needs what

| | Sharer | Viewer |
|---|---|---|
| IPv6 | Simplest when both sides have it: then nothing below matters | Same |
| Public IPv4 | Without IPv6, at least one side's router needs a public IPv4 address (not 10.x / 100.64-127.x / 172.16-31.x / 192.168.x) | Same |
| Port forwarding on the router | Nothing to do by hand: while HdrCast is online or live, it forwards its own IPv4 port with UPnP, and deletes the forward when it stops (the router needs UPnP on, which most have by default) | Same (while watching) |
| Windows Firewall | Click **Allow…** when the **Firewall** line in the app offers it (Windows asks for administrator approval once; updates unzipped over the same `HdrCast` folder keep it, a copy in another folder needs another click; not needed on macOS) | Same (only matters when the sharer is behind a symmetric NAT) |
| Upload | bitrate × viewers × 1.25-1.5 (§7) | Very little (only acknowledgements) |
| Download | Very little | bitrate × 1.3 |

A side without IPv6 can only use IPv4, and that needs:

1. **UDP 18651 open on the signaling server** (§6). The public IPv4 candidate only comes from that echo. Without it the
   sharer advertises nothing but IPv6 and LAN addresses, and the viewer has no IPv4 address to try at all (sharer's log:
   `public IPv4 check: no answer…`; panel on the sharer's **Share** page: `no public IPv4 found`).
2. **Routers that keep ports, or a side that is open**:
   - **Cone NAT** (the same inside port goes out on the same public port to every destination; most home routers): both
     sides send to each other's public mapping at the same moment, the router and Windows Firewall let the replies in, and
     there is nothing to open.
   - **Symmetric NAT** (a new public port for every destination; common with Chinese ISPs and carrier-grade NAT): its
     packets come from a port the other side has never seen, so the other side's router and Windows Firewall drop them
     as unrequested. Then the **other** side has to accept such packets: a UPnP forward on its router (HdrCast sets it up
     by itself) plus a Windows Firewall rule (click **Allow…**). Once a packet gets in, HdrCast notes the real port and
     punches back to it (design.md §8.6), so either side can be the sharer, as long as the other side is open. **When
     both sides are behind symmetric NATs, IPv4 cannot get through**: at least one side needs IPv6.
   - An example (2026-09-25): a friend (China Telecom in Jiangsu, no IPv6) advertised port :20833, and their packets
     reached the sharer from :20834. With only the firewall rule on the sharer's side, not a single packet arrived at the
     network adapter (the router dropped them all); once the router's UPnP forward was in place, it connected in under a
     second.

### 1.2 What the messages say when it does not connect

These are the messages on the Watch panel unless the first column says where else to look. Most come from the connection
engine and stay in English whatever language the app is set to (they also end up in the log people send when something
breaks), and they call the sharer "the host".

| Message | What it means / what to do |
|---|---|
| This PC has no IPv6 internet connection, and the host offered no public IPv4 address… | This computer has no IPv6, and the sharer advertised no public IPv4 address (the sharer's UDP echo got no answer, §1.1 point 1), so IPv4 was never tried: turn IPv6 on here as in §2, or open UDP 18651 on the signaling server first |
| This PC has no IPv6 internet connection, and the IPv4 attempt did not get through… | No IPv6 here, and IPv4 hole punching failed too (at least one NAT does not play along, §1.1 point 2; when it adds *this PC's public IPv4 address could not be found*, this computer did not even learn its own public IPv4): turn IPv6 on as in §2 |
| …did not get through: the host's PC only takes IPv4 from routers that keep ports… | The sharer's IPv4 is not open (no router forward, or no firewall rule), and this computer's router most likely is a symmetric NAT: ask the sharer to click **Allow…** and to check that UPnP is on in their router; or turn IPv6 on here |
| …did not get through although the host takes IPv4 from any port… | The sharer's side is open, yet the packets do not arrive: a VPN / proxy / accelerator on this computer (Clash TUN, UU and the like) takes the UDP. Turn it off, or let HdrCast bypass it |
| Panel on the sharer's **Share** page: `IPv4 closed to routers that change ports: …` | The sharer's IPv4 is not open, and after the colon it says what is missing: `no router forward (UPnP)` → turn UPnP on in the router; `no firewall rule (click Allow)` → click **Allow…**; `router behind another NAT` → there is another NAT outside the router (an ONT in router mode, or carrier-grade NAT), and forwarding cannot help |
| Log: `[p2p] router: could not forward UDP …` | UPnP is off in the router, or the router refused the forward: the reason follows the dash (a refusal quotes the router's error code). HdrCast tries again after a minute, then less often; once UPnP is on there is no need to restart HdrCast |
| Sharer's log: `connected via IPv4 … (a port their router picked for this PC …)` | The other side is behind a symmetric NAT and got in through the IPv4 port this side opened: all is well |
| Sharer's log: `left (could not connect: no candidate answered; …)` | Not a single packet from the other side arrived. After the semicolon comes the likely reason: if this side's IPv4 is open, something on their side takes the UDP; if not, open it first as the message says |
| The host's PC has no IPv6 internet connection… | The sharer has no IPv6: they turn it on as in §2 |
| Neither side has IPv6… | Neither side has IPv6, and IPv4 did not get through either: at least one side needs IPv6 |
| Panel on the sharer's **Share** page: `no public IPv4 found`; log: `public IPv4 check: no answer…` | The signaling server's UDP 18651 does not answer: open it in the Tencent Cloud security group (§6). The sharer tries again every minute and the viewer before every join, so nobody has to restart once it is open |
| Both sides have IPv6 but the path is blocked… | Both sides have IPv6, but the packets do not get through: most likely a VPN (turn it off), or a strict router setting such as "IPv6 firewall: block all incoming" (set it back to default / standard) |
| WAITING: Waiting for … to let you in… | The sharer has to let this computer in first (their rule for it is **Ask Me**, and they see the request with this computer's device code), or their computer is still starting to capture |
| … did not let you in | The sharer clicked **Deny**, set this computer to **Never**, or disconnected it less than a minute ago. HdrCast stops trying: ask them |
| Nobody at … answered your request | Nobody clicked **Allow** within 50 s |
| Not live right now / Nobody is online right now | HdrCast is not running on the other computer (or **Available to connect** is off there); the list refreshes every 10 s |
| … is full | The sharer's **Viewers at once** is reached |
| The signaling server cannot be reached | This computer is offline, or the signaling server is down; people already watching are not affected |

## 2. Checking and turning on IPv6

1. Open <https://test-ipv6.com>: "IPv6 address detected" (10/10 at best) means you are fine.
2. If not, usually the router or the ONT has it off:
   - **Japan**: lines on NTT FLET'S, SoftBank Hikari and au Hikari all come with IPv6 (IPoE) by default; SoftBank's
     Hikari BB Unit has IPv6 on by default.
   - **China**: the three big carriers give home broadband IPv6 almost everywhere, but many ONTs and routers only have
     IPv4 on by default. Look in the router for "IPv6 settings", "IPv4/IPv6 dual stack" or "IPv6 mode" (with a bridged
     ONT, set it on your own router); a phone hotspot usually comes with IPv6, so try one first.
   - **Australia**: it depends on the ISP. Some NBN ISPs give IPv6 by default, others do not offer it; then the only ways
     are another ISP, or letting somebody in the group who has IPv6 do the sharing.
3. IPv6 is on in Windows by default; if a VPN or accelerator is installed, turn it off before you test.

## 3. Delay presets per region, and why

The viewer picks a preset under **Watch → Delay**. It goes with the join request to the sharer, whose remux process for
that viewer uses it (the China preset also raises oheadbw to 50 %). So in one session a viewer in Japan at 1.5 s and one
in China at 3.5 s are both served as they should be. The presets were sized for a sharer in Japan:

| Viewer in | RTT (estimate) | Loss | SRT delay | oheadbw |
|---|---|---|---|---|
| Japan | 10-30 ms | ≈0 | **1500 ms** | 25 % |
| Australia | 120-160 ms | a little | **2000 ms** | 25 % |
| Mainland China (cross-border) | 60-120 ms, but lots of jitter and bursts of loss | 1-5 % is common | **3500 ms** | 50 % |

**Fast** (300 ms, oheadbw 25 %) is for remote control: the picture stays close behind, but there is little time for
retransmissions, so it wants a clean connection over a short distance. With **Custom**, a delay of 3000 ms or more asks
for oheadbw 50 % like China, anything shorter 25 %.

Why these values:

1. **Haivision's 4 × RTT rule**: SRT latency = RTT multiplier × RTT; on a "fairly good" network with 0.1-0.2 % loss and
   no bursts, the multiplier is about 4. Below 3 it makes little sense; above 20 the line is badly congested.
2. **The official conservative table** (packet loss → multiplier / bandwidth overhead): ≤1 % → 3 / 1 %; ≤3 % → 4 / 4 %;
   ≤7 % → 6 / 9 %; ≤10 % → 8 / 15 %; ≤12 % → 8 / 20 %; ≤21 % → 10 / 38 %; ≤25 % → 13 / 46 %; ≤30 % → 14 / 61 %;
   ≤40 % → 30 / 97 %.
3. By 4 × RTT, Australia would need only about 640 ms and China about 480 ms. But the aim is a broadcast, not a remote
   desktop: a margin of several times rides out the bursts of loss on cross-border links (a retransmission has to land
   within the delay), at the price of a little more delay. For China the multiplier for 3-7 % loss is 6-8, plus margin,
   which gives 3500 ms; `oheadbw` goes up to 50 % as well, to leave bandwidth for the retransmissions.
4. The two ends negotiate the delay and the **larger** value wins (the sender's `peerlatency` against the receiver's
   `rcvlatency`). Both ends use the viewer's value, so what the viewer picks is what applies (measured:
   `msReceiveTsbPdDelay` / `msSendTsbPdDelay` in the API equal the value in the URL).
5. End-to-end delay ≈ SRT on the receiving side (1.5-3.5 s) + mpv's `cache-pause-wait` (**Settings → Player buffer**,
   1.5 s) + encoding (about 0.4 s), so about 3.5 s for Japan and 5.5 s for China. Measured on 2026-09-24 on one PC over
   loopback (NVENC → SRT 1500 ms → mpv with the viewer's exact options; a UTC timestamp burnt into the picture, compared
   through screenshots over IPC): **3.58 s**, the same three times, with mpv's `demuxer-cache-duration` steady at
   1.55 s. The "ping 38 ms" in the app is the network round trip, not the picture delay.
6. The delay decides how much **packet loss** the stream rides out (the retransmission window), not the picture quality:
   on a good line 1.5 s and 3.5 s look the same, one just runs two seconds later; quality only depends on the bitrate and
   the encoder preset. Viewers across the border in China should pick the China preset (3500 ms, oheadbw 50 %);
   otherwise retransmissions of the occasional lost packet arrive too late and the picture freezes until the next
   keyframe (before 0.3.2 it broke up into blocks; see [faq.md](faq.md)).
7. SRT's deadline for a packet counts from when it was captured, not from when it was first sent. Whatever waits in the
   sharer's SRT queue uses up that time: SRT sends at most `inputbw × (1 + oheadbw)`, resends included, so a
   bitrate peak above it waits. Since 0.3.2 the encoder's rate buffer is 0.25 s (it was 2 s, which let a busy second
   reach twice the bitrate and packets wait up to 860 ms before their first send: with a delay of 1 s they arrived
   too late without any loss at all), and `inputbw` is the stream's real rate on the wire, not the video bitrate
   (docs/design.md §2.5, §2.6). Both logs now say how lossy the path is: the `[net]` lines (design.md §8.11).

> Viewers can tune it themselves: if the picture keeps stalling (mpv buffering again and again), stop, add 500 ms under
> **Watch → Delay → Custom** and connect again; on a good line go down instead.

## 4. Buffer size formula (a must, or high bitrates drop)

libsrt's default receive buffer holds only 8192 packets (about 10.8 MB). When delay × bitrate is larger, the packets that
do not fit into the retransmission window are dropped: the picture breaks up, or the connection drops altogether
(48 Mbps × 2 s does it on one PC). So both ends put these values into the URL (implemented in
`crates/core/src/common/srt_buffer_sizing.rs`):

```
packets = ceil( latency_s × bitrate_bps / (1316 × 8) )      # 1316 bytes of payload per SRT packet
ffs     = max( 25600, smallest power of 2 ≥ packets × 1.2 )   # SRTO_FC, the flow-control window (in packets)
rcvbuf  = ffs × 1472                                          # SRTO_RCVBUF must be ≤ ffs × (MSS − 28)
sndbuf  = same as rcvbuf (sender)
recv_buffer_size = 8000000                                    # UDP socket buffer (Linux caps it at net.core.rmem_max, silently)
```

Examples:

| Bitrate × delay | packets | ffs | rcvbuf |
|---|---|---|---|
| 40 Mbps × 1.5 s | 5 700 | 25 600 (default) | 37 683 200 |
| 50 Mbps × 2 s | 9 499 | 25 600 | 37 683 200 |
| 100 Mbps × 2 s | 18 997 | 25 600 | 37 683 200 |
| 100 Mbps × 3.5 s | 33 245 | 65 536 | 96 468 992 |

The viewer does not know which bitrate the sharer is going to use, so it sizes the buffer for **Settings → Highest
bitrate to expect** (100 Mbps by default; larger is safer, it only costs memory).

## 5. What to open in the firewall

- **Both sides have IPv6, or both are behind cone NATs**: nothing. Hole punching relies on the router and Windows
  Firewall letting in the replies to "a flow we sent first".
- **One side is behind a symmetric NAT** (§1.1 point 2): the other side has to accept "unrequested" UDP, which takes both
  of these:
  - **Router**: HdrCast forwards its current IPv4 port to this computer with UPnP by itself. The lease is 1 hour,
    renewed every 20 minutes while HdrCast is online, live or watching, and the forward is deleted when it stops; if
    HdrCast crashes, the forward runs out by itself within the hour. In the log these are the `[p2p] router: …` lines.
    UPnP has to be on in the router settings (it is by default on most).
  - **Windows Firewall**: click **Allow…** when the **Firewall** line offers it (on the **Share** page, at the top
    while it is missing and under **Viewers** otherwise, and in **Settings → Network**). After the Windows UAC prompt there is an inbound rule "HdrCast": only
    for this exe, only UDP, for every network type (the two rules of 0.2, "HdrCast Sender" and "HdrCast Receiver", can
    stay). Block rules left over from clicking "Cancel" on a Windows prompt are disabled, not deleted. While the rule
    is missing, HdrCast asks once per run as soon as this computer can be connected to (**Available to connect** on,
    or **Go Live**); a viewer is asked once too, when an IPv4 attempt failed in a way the rule could help with.
  - The rule is tied to the full path of the exe. The folder in the zip is plain `HdrCast` (only the zip's name carries
    the version), so a new version unzipped over the old folder keeps the rule, while a copy in any other folder needs
    another click. Up to 0.3.3 the folder was named after the version (say `D:\HdrCast-0.3.1\`), so the move to
    `HdrCast` takes this click once more. To undo it: Windows Security → Firewall & network protection → Advanced
    settings → Inbound Rules, and delete the rule.
- **Is that safe?** All it opens is one UDP port of HdrCast itself. HdrCast only accepts punch packets signed with the
  group key and data from confirmed addresses, and drops everything else; the video itself is encrypted with SRT's
  AES-256.

## 6. What the signaling server on Tencent Cloud does

`signal/hdrcast_signal.py` (Python standard library only, run by systemd, see [signal/README.md](../signal/README.md)):

- **What it keeps**: for every group, who is online or live (name, device key, candidate addresses, bitrate, viewer
  count; gone after 30 s without a renewal), join requests and answers (gone after 60 s), and the registry of group id →
  auth key (the only thing it writes to disk).
- **What it cannot see**: the group key, the SRT passphrase, the video. Every message is signed with a key derived from
  the group key, which the server does not know, so even somebody who took over the server could only keep people from
  connecting, not send viewers anywhere else.
- **Traffic**: a heartbeat of a few hundred bytes every 10 s from every computer that is online or live, and a list
  request every 10 s from every HdrCast that is running. Next to the video that is nothing, and it does not get in the way
  of the production services on that machine.
- **Ports**: TCP 18650 (HTTP API) and UDP 18651 (IPv4 address echo), both opened in the Tencent Cloud security group. UDP
  18651 may only stay closed if everybody has IPv6: without it nobody learns a public IPv4 mapping, and viewers without
  IPv6 cannot connect at all (§1.1).
- **When it is down**: people already connected keep watching; nobody new can join and the list does not load; it all
  carries on once the server is back.

## 7. Estimating upload bandwidth

```
upload the sharer needs ≈ bitrate × viewers × (1 + oheadbw)      # oheadbw 25 % → ×1.25, viewers in China at 50 % → ×1.5
```

Example: 40 Mbps, one viewer each in Japan, Australia and China ≈ 40 × (1.25 + 1.25 + 1.5) = 160 Mbps of upload. Fibre
upload in Japan is usually enough; home broadband in China often has 30-100 Mbps of upload and NBN in Australia
20-50 Mbps, so when a friend there shares, they have to lower the bitrate or the number of viewers. The **Share** page
estimates the upload for **Viewers at once**, and once **My upload speed** is filled in, it turns the estimate red above
80 % of it.
