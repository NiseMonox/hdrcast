# HdrCast - how to watch (and stream)

Your friend shares their Windows desktop (HDR10, with audio) straight from their PC to yours: **peer-to-peer**, no relay server,
no manual port forwarding. A small signaling server only introduces the two PCs to each other; the video never passes through it.
Anyone in the group can also stream to the others.

## What you need

- Windows 10 (1903+) or 11. Nothing to install: the apps run on the .NET Framework that Windows already ships.
- **IPv6.** Peer-to-peer works over IPv6 (it has no NAT, so the two PCs can reach each other). Check at <https://test-ipv6.com>:
  "10/10" or at least "IPv6 address detected" is what you want. No IPv6? It can often be switched on in the router, or your ISP
  can enable it. Without IPv6 the app uses IPv4, which works as long as the streamer's side is open (next point).
- **Firewall: one click.** If the **Firewall** line in the app is orange, click **Allow…** once and confirm Windows' prompt. Some
  routers (common in China) give every destination a new port, and a PC only lets such packets in when Windows Firewall allows
  HdrCast. The app also opens its port on the router by itself (UPnP) while it streams or plays, and closes it when it stops.
- **mpv** built with SRT support. It is already in the `tools\` folder of the download. For a newer one, double-click
  `scripts\get-tools.cmd force` (or put `mpv.exe` from <https://github.com/zhongfly/mpv-winbuild/releases> into `tools\`).
- A GPU that decodes HEVC 10-bit in hardware (NVIDIA GTX 10-series+, AMD RX 400+, Intel 7th gen+). 4K60 needs RTX 20+/RX 5000+.
- Download bandwidth of about 1.3 x the stream bitrate (a 40 Mbps stream needs ~50 Mbps).
- **The invite** your friend sent you: a message with a line starting with `hdrcast:`.

## Watching

1. Unzip the HdrCast folder anywhere. mpv is already in `tools\`, so there is nothing else to download.
2. **Copy the whole invite message** from the chat, then start `HdrCastReceiver.exe`. It asks "Join this group?": say yes and
   type the name your friends will see. If it does not ask, click **Paste invite**.
3. The **Watch** list shows who is live right now (refreshed every 10 s). Pick your region in
   **Latency**: Japan 1500 ms, Australia 2000 ms, China 3500 ms. Click **Play**.
4. On an **HDR monitor**, switch Windows HDR on first: `Win + Alt + B` (or Settings > System > Display > Use HDR). On an SDR
   monitor choose "SDR tone-map" under Display.

The delay is 3-6 seconds by design: it is a broadcast, not a remote desktop.

## Streaming to the group

1. Start `HdrCastSender.exe` (ffmpeg is already in `tools\`).
2. The **Share with your group** box shows the group. If you already joined with the receiver, it is there;
   otherwise **Paste invite**.
3. Pick the monitor and bitrate, set **Max viewers**, click **Start**. Every viewer costs one full stream of *your upload*:
   three viewers at 20 Mbps need about 75 Mbps up. The app estimates it; enter your upload next to "my upload" to get a warning.
   If Windows Firewall does not let HdrCast in yet, Start offers to fix that once: say yes and confirm Windows' prompt.
4. Talking with the group in a voice chat? Pick it under **Desktop audio → Leave out** (for example `Discord.exe`) so your
   friends do not hear themselves coming back through the stream. Needs Windows 10 21H1 or newer.

## If something is wrong

The status line under **Watch** says what happened. The usual ones:

| Message | What to do |
|---|---|
| This PC has no IPv6 internet connection | Turn IPv6 on in your router, or ask your ISP. test-ipv6.com shows the state. If it adds *the host offered no public IPv4 address*, IPv4 was never tried: tell whoever runs the signaling server that its UDP port 18651 is closed. |
| ... the host's PC only takes IPv4 from routers that keep ports ... | Your router changes ports and the streamer's side is closed: ask them to click **Allow…** next to Firewall and to check UPnP is on in their router. Or turn IPv6 on. |
| ... although the host takes IPv4 from any port ... | Something on your PC grabs UDP: a VPN, proxy or game accelerator (Clash TUN, UU, ...). Turn it off, or let HdrCastReceiver bypass it. |
| The host's PC has no IPv6 internet connection | The friend streaming needs IPv6 (same advice, their side). |
| Both sides have IPv6 but the path is blocked | A VPN or a very strict firewall drops incoming UDP. Turn the VPN off; on the router look for "IPv6 firewall: block all". |
| Not live right now / Nobody in the group is live | Your friend has not pressed Start yet. The list refreshes by itself. |
| ... is full | The streamer's "Max viewers" is reached. |
| The signaling server cannot be reached | Your internet is down, or the signaling server is. People already watching keep watching. |
| Picture stutters, then recovers every few seconds | Raise the latency (China: try 4000-5000 ms) or ask for a lower bitrate. |
| Colours look grey / flat on an HDR monitor | Windows HDR is off (`Win + Alt + B`). |
| mpv says it cannot open `srt://` | Your mpv lacks libsrt: use the zhongfly / shinchiro build (get-tools.cmd does). |

Privacy: the video is AES-256 encrypted end to end with a key derived from the invite. The invite itself is the key to the
group: only share it with people you trust. To lock someone out, the owner creates a new group and sends the new invite to the others.
