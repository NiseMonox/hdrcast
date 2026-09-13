# HdrCast - how to watch

Your friend is sharing their Windows desktop (HDR10, with audio) through a private relay. This page is for the **viewer** side only.

## What you need

- Windows 10 (1903+) or 11. Nothing to install: `HdrCastReceiver.exe` runs on the .NET Framework that Windows already ships.
- **mpv** built with SRT support: download `mpv-x86_64-<date>.7z` from <https://github.com/shinchiro/mpv-winbuild-cmake/releases> or <https://github.com/zhongfly/mpv-winbuild/releases>, extract it and put `mpv.exe` next to `HdrCastReceiver.exe` (or in the `tools\` folder next to it).
- A GPU that decodes HEVC 10-bit in hardware (NVIDIA GTX 10-series+, AMD RX 400+, Intel 7th gen+ iGPU). 4K60 needs something recent (RTX 20+/RX 5000+).
- Download bandwidth of at least 1.3 x the stream bitrate (a 40 Mbps stream needs ~50 Mbps).
- The five values from your friend: **host**, **port** (8890), **user** (`viewer`), **password**, **passphrase**.

## Steps

1. Unzip the HdrCast folder anywhere. Run `scripts\check-tools.cmd` once: it must say `mpv ... srt : yes`.
2. If you have an **HDR monitor**, switch Windows HDR on: `Win + Alt + B` (or Settings > System > Display > Use HDR). mpv cannot turn it on for you; without it the picture looks washed out.
3. Run `HdrCastReceiver.exe`. Mode "Relay", paste host / port / user / password / passphrase.
4. Pick your region preset (Japan 1500 ms, Australia 2000 ms, China 3500 ms). It sets the SRT buffer; a bigger value survives worse networks at the cost of delay.
5. Display: "Auto" (HDR passthrough when Windows HDR is on, tone-mapping otherwise). On an SDR monitor choose "SDR tone-map".
6. Click Connect. "Waiting for stream..." means the sender is not live yet; the app retries every 3 s. Use the fullscreen button for real HDR fullscreen.

The end-to-end delay is 3-6 seconds by design (it is a broadcast, not a remote desktop).

## If something is wrong

| Symptom | Fix |
|---|---|
| "Waiting for stream..." forever | The sender has not started, or host / port is wrong, or the sender's router does not forward UDP 8890. |
| Disconnects immediately, log says `invalid passphrase` | Wrong passphrase (the viewer passphrase differs from the sender's one). |
| `authentication failed` | Wrong viewer password. |
| Picture stutters, then recovers every few seconds | Raise the latency preset (China: try 4000-5000 ms) or ask the sender to lower the bitrate. |
| Log says `No room to store incoming packet` | Raise "max bitrate" in the app (it sizes the receive buffer). |
| Colours look grey / flat on an HDR monitor | Windows HDR is off (`Win + Alt + B`). |
| Too dark / too bright on an SDR monitor | Choose "SDR tone-map" in the app. |
| mpv says it cannot open `srt://` | Your mpv build lacks libsrt. Use the shinchiro / zhongfly build, or pick the "ffmpeg pipe" frontend in the app. |
| No sound | Check the mpv window's audio device (press `#` in mpv to cycle) and Windows volume mixer. |

No GUI? `scripts\receive-example.cmd` contains the raw mpv command: edit the values at the top and run it.

Privacy: the stream is AES-256 encrypted between you and the relay; only people with the passphrase can decode it.
