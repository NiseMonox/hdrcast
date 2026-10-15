# HdrCast - how to watch (and share)

Your friend shares their desktop (HDR10, with sound) straight from their computer to yours: **peer-to-peer**, no relay server,
no manual port forwarding. A small signaling server only introduces the two computers to each other; the video never passes
through it. Anyone in the group can also share to the others. One app, **HdrCast**, for Windows and macOS.

## What you need

- Windows 10 (1903+) or 11, or macOS 13+ (sharing in HDR from a Mac needs macOS 15).
- **IPv6.** Peer-to-peer works over IPv6 (it has no NAT, so the two computers can reach each other). Check at
  <https://test-ipv6.com>: "10/10" or at least "IPv6 address detected" is what you want. No IPv6? It can often be switched on
  in the router, or your ISP can enable it. Without IPv6 the app uses IPv4, which works as long as the sharer's side is open
  (next point).
- **Windows Firewall: one click.** If the **Firewall** line in the app offers **Allow…**, click it once and confirm Windows'
  prompt. Some routers (common in China) give every destination a new port, and a PC only lets such packets in when Windows
  Firewall allows HdrCast. The app also opens its port on the router by itself (UPnP) while it shares or plays, and closes it
  when it stops.
- **mpv** (to watch) and **ffmpeg** (to share), built with SRT support.
  Windows: both are already in the `tools\` folder of the download (`scripts\get-tools.cmd force` fetches newer ones).
  macOS: `brew install mpv ffmpeg`.
- A GPU that decodes HEVC 10-bit in hardware (NVIDIA GTX 10-series+, AMD RX 400+, Intel 7th gen+, any Apple silicon Mac).
- Download bandwidth of about 1.3 x the stream bitrate (a 40 Mbps stream needs ~50 Mbps).
- **The invite** your friend sent you: a message with a line starting with `hdrcast:`.

## Watching

1. Windows: unzip the HdrCast folder anywhere and start `HdrCast.exe`. macOS: drag `HdrCast.app` to Applications and open it.
2. **Copy the whole invite message** from the chat first. HdrCast then asks "Join this group?": say yes and type the name your
   friends will see. If it does not ask, choose **I have an invite** (or **Settings > Paste Invite**).
3. **Watch** lists who is live right now (refreshed every 10 s). Pick where you are under **Delay**: Japan 1.5 s,
   Australia 2 s, China 3.5 s. Click **Watch**.
4. On an **HDR display**, switch HDR on first (Windows: `Win + Alt + B`). On an SDR display set **Output** to SDR.

The delay is 3-6 seconds by design: it is a broadcast, not a remote desktop.

## Sharing to the group

1. Open **Share**, pick the display and bitrate, set **Viewers at once**, click **Go Live**. Every viewer costs one full
   stream of *your upload*: three viewers at 20 Mbps need about 75 Mbps up. The app estimates it; enter your upload speed to
   get a warning. macOS asks once for the Screen & System Audio Recording permission.
2. Talking with the group in a voice chat? On Windows pick it under **Sound > Leave out** (for example `Discord.exe`) so your
   friends do not hear themselves coming back through the stream. Needs Windows 10 21H1 or newer.

## Remote control (optional)

A viewer can ask to use the sharer's mouse and keyboard, from the player window.

- Viewer: switch **Remote control** on under **Watch > Connection** before clicking Watch, and use the **Fast** delay.
  `Ctrl + Alt + Enter` stays with your own player (full screen on/off).
- Sharer: under **Share > Viewers** choose **Off**, **Ask Me** or **Allow**. With Ask Me every request shows a prompt; the
  viewer list has **Take Back** at any time. On macOS allow HdrCast under System Settings > Privacy & Security > Accessibility.
- The picture is still a few hundred milliseconds or more behind, so this is for helping with a menu, not for playing.

## If something is wrong

The panel on the Watch page says what happened. The usual ones:

| Message | What to do |
|---|---|
| This PC has no IPv6 internet connection | Turn IPv6 on in your router, or ask your ISP. test-ipv6.com shows the state. If it adds *the host offered no public IPv4 address*, IPv4 was never tried: tell whoever runs the signaling server that its UDP port 18651 is closed. |
| ... the host's PC only takes IPv4 from routers that keep ports ... | Your router changes ports and the sharer's side is closed: ask them to click **Allow…** next to Firewall and to check UPnP is on in their router. Or turn IPv6 on. |
| ... although the host takes IPv4 from any port ... | Something on your computer grabs UDP: a VPN, proxy or game accelerator (Clash TUN, UU, ...). Turn it off, or let HdrCast bypass it. |
| The host's PC has no IPv6 internet connection | The friend sharing needs IPv6 (same advice, their side). |
| Both sides have IPv6 but the path is blocked | A VPN or a very strict firewall drops incoming UDP. Turn the VPN off; on the router look for "IPv6 firewall: block all". |
| Not live right now / Nobody is live right now | Your friend has not pressed Go Live yet. The list refreshes by itself. |
| ... is full | The sharer's "Viewers at once" is reached. |
| The signaling server cannot be reached | Your internet is down, or the signaling server is. People already watching keep watching. |
| Picture stutters, then recovers every few seconds | Raise the delay (China: try a custom 4000-5000 ms) or ask for a lower bitrate. |
| Colours look grey / flat on an HDR display | HDR is off in the system (Windows: `Win + Alt + B`). |
| mpv says it cannot open `srt://` | Your mpv lacks libsrt: use the zhongfly / shinchiro build (get-tools.cmd does) or Homebrew's. |

When you ask a friend for help, send them what **Activity > Copy** gives you: passwords and invites are masked in it.

Friends still on HdrCast 0.2 (`HdrCastSender` / `HdrCastReceiver`) can watch and share with you as before; only remote control
needs 0.3 on both sides.

Privacy: the video is AES-256 encrypted end to end with a key derived from the invite, and so is remote-control input. The
invite itself is the key to the group: only share it with people you trust. To lock someone out, create a new group and send
the new invite to the others.
