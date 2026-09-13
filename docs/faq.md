# HdrCast 常见问题

## 为什么 AV1 只能直连，不能走中继？

ffmpeg（7.1 与 master）的 MPEG-TS 封装器不支持 AV1：会写成 private stream (0x06)，读回来变成 `bin_data`；MediaMTX 的 SRT/MPEG-TS 也只认 H264 / H265 / MPEG 视频和 Opus / AAC / MP3 / AC-3 音频。所以走中继只能 **HEVC Main10**。
AV1（`av1_nvenc`，RTX 40 系）只在「直连模式」可选：封装改成 `-f matroska -live 1`（或 NUT）直接 SRT 到对方。代价：Matroska 没有 TS 那样的包级重同步，丢包严重时恢复更慢；观众的 GPU 也要能硬解 AV1 10-bit（RTX 30+/RX 6000+/Intel Arc/12 代+ 核显），软解 dav1d 4K60 很吃 CPU。

## 观众的 mpv 为什么要先在 Windows 里打开 HDR？

mpv 的 HDR 直通 (`--vo=gpu-next --gpu-api=d3d11 --target-colorspace-hint=yes`) 只是向 DXGI 交换链声明 "我输出的是 PQ / BT.2020"，**它自己不会切换 Windows 的 HDR 模式**（旧驱动那种"mpv 会自动开 HDR"的说法从 NVIDIA R560 起已失效）。Windows HDR 没开时这个声明会被忽略或得到一个奇怪的画面（发灰/过暗）。
操作：`Win + Alt + B` 切换 HDR，或 设置 → 系统 → 显示 → 使用 HDR。接收端 GUI 的 "自动" 模式只在检测到 HDR 输出时才启用直通，其余情况走色调映射，所以忘了开也不至于全黑，只是画面不是真 HDR。

另外静态 HDR 元数据只在**全屏**时才会下发给显示器（Windows 限制），窗口化也是 HDR 但显示器不会切到 HDR 模式指示；想要"真全屏"就用 GUI 的全屏按钮（`--fs`，不要 `--d3d11-exclusive-fs`）。

## 观众是 SDR 显示器怎么办？

三个选择：

1. **mpv 色调映射（推荐）**：接收端选 "SDR 色调映射"（`--target-colorspace-hint=no --tone-mapping=bt.2390`，可选 `--hdr-compute-peak=yes`），mpv 在 GPU 上把 PQ 映射到 SDR，观感接近发送端的 SDR 桌面，高光被压缩。
2. **浏览器 HLS 兜底**：`http://viewer:密码@主机:8888/screen`，MediaMTX 原样输出 HEVC 10-bit fMP4，观感取决于浏览器是否会做 HDR→SDR（Edge/Chrome 带硬解 HEVC 时一般可以），延迟 3~6 s，没有实测。
3. 发送端切「强制 SDR」：桌面按 SDR 采集（nv12 / BT.709），所有人看 SDR，但发送端自己的 HDR 桌面在 SDR 采集下亮度会被裁切——只在观众全是 SDR 时用。

## 码率怎么选？

HEVC 10-bit、60 fps、桌面/游戏混合内容的经验值：

| 分辨率 | 够用 | 好 | 极致（NVENC 已无收益） |
|---|---|---|---|
| 1920×1080 | 12 Mbps | 20 Mbps | 30 Mbps |
| 2560×1440 | 20 Mbps | 35 Mbps | 50 Mbps |
| 3840×2160 | 40 Mbps | 60 Mbps | 80~100 Mbps |

- 文字多的桌面比游戏更需要码率（高频细节），快速运动的游戏 + 高码率才不糊。
- 4K 显示器分享游戏时，在 GUI 里把输出分辨率降到 2560×1440 通常比 4K 低码率更清楚，也少占一半带宽。
- 码率上限由**你的上行带宽 ÷ (观众数 × 1.25)** 决定，而不是画质意愿，见下一条。
- 中继模式下发送端 → LXC 只走局域网，随便用；瓶颈永远在家里的公网上行。
- GOP 保持 1~2 s（默认 2 s）：观众加入时要等到下一个 IDR 才有画面，mpv 在此期间会刷 "Could not find ref with POC"，是正常现象。

## 上传带宽怎么估？

中继把每个观众各发一份，所以：

```
上行需求 ≈ 码率 × 观众数 × 1.25（SRT 开销 oheadbw=25 %；中国观众 oheadbw=50 % 时按 ×1.5）
```

- 40 Mbps × 2 人 = 100 Mbps；40 Mbps × 3 人（含一位中国）≈ 160 Mbps。
- 先 speedtest 看上行，留 30 % 余量。上行不够就降码率或减少人数，不要把 oheadbw 调低（Haivision 的原话：太低时线路一有丢包"流就会立刻卡死"）。
- MediaMTX API（LXC 上 `curl -s http://127.0.0.1:9997/v3/srtconns/list | jq`）能看到每条连接的 `mbpsSendRate`、`packetsRetrans`、`packetsReceivedLoss`，是判断带宽是否够的最直接依据。

## 画面卡顿 / 断线，按什么顺序排查？

1. 接收端 mpv 日志里有 `No room to store incoming packet` → 接收缓冲不够，GUI 的「码率上限」填大一点（它决定 rcvbuf/ffs），见 `docs/network.md` §3。
2. 中继 API 里该观众的 `packetsReceivedLoss` / `packetsRetrans` 持续增长 → 线路丢包，给这位观众把 latency 加 500~1000 ms；仍不行就降码率。
3. 发送端状态栏 `speed` 持续 < 1.0× 或 `drop_frames` 增长 → 编码器跟不上：降分辨率 / 帧率，或质量档改成"速度优先"。
4. 采集统计 `dropped` 增长而 `captured` 正常 → 管道写入被 ffmpeg 阻塞，一般是上一条的编码器问题。
5. 中继日志 `reader is too slow` → 已经把 `writeQueueSize` 调到 2048，再出现说明观众线路太差，只能降码率。

## 观众连不上？

- 显示"等待推流…"：发送端还没开始，MediaMTX 不会让读者排队，接收端每 3 s 重试是正常的。
- 立即断开、日志 `invalid passphrase` / `passphrase not provided` → read passphrase 填错了（注意 publish 与 read 是两把不同的 passphrase）。
- `authentication failed` → viewer 密码错。
- 连接超时 → 路由器没转发 UDP 8890，或公网 IP 变了（用 DDNS）；可以先在同一局域网内用 `192.168.1.201` 测。
- mpv 说不认识 `srt://` → 这个 mpv 构建没带 libsrt，换 shinchiro / zhongfly 的构建，或在接收端 GUI 里选「用 ffmpeg 拉流」兜底。

## 音频怎么来的？需要虚拟声卡吗？

不需要。采集引擎用 WASAPI loopback 抓默认播放设备的混音（同时播放一路静音防止渲染引擎休眠），重采样成 48 kHz 立体声 f32，与视频用同一个 QPC 时钟对齐后通过命名管道给 ffmpeg，编码为 AAC 192k（或 Opus 128k）。在 GUI 里可以选别的输出设备，或关掉音频。

## 为什么采集不用 ffmpeg 自带的 ddagrab？

ddagrab 能拿到正确的 scRGB FP16 帧，但 ffmpeg 内部没有任何无损路径把打包 FP16 交给 zscale / libplacebo：swscale 会把 > 1.0（> 80 nit）的值全部裁掉（三个版本实测），所以 HDR 会被压成 SDR。HdrCast 用自己的 D3D11 compute shader 在 GPU 上做 scRGB → PQ/BT.2020 → P010，再把 P010 喂给 ffmpeg 只做编码，这也是 OBS 的做法。详见 `docs/design.md` §1.1。

## 安全性如何？

- 两条 SRT 腿都是 AES-256（passphrase 24 位随机，`pbkeylen=32`），载荷加密。
- SRT 握手本身不加密，streamid 里的用户名/密码是明文——它们只是"低价值令牌"，真正的保密靠 passphrase；不要在别处复用这些密码。
- MediaMTX 的 API 只绑定 LXC 的 127.0.0.1；HLS 端口有 basic auth；其余协议（RTSP/RTMP/WebRTC）全部关闭。
- 想更保险就走 Tailscale（`docs/network.md` §1.4），完全不开公网端口。

## 延迟能再低一点吗？

可以，但这是"推流式"设计：把观众的 latency 改成 4×RTT（日本 ~200 ms、澳洲 ~700 ms），mpv 的 `cache-pause-wait` 改成 0.5 s，总延迟能到 1.5 s 左右；线路一抖就会卡。不建议低于 3×RTT。远程桌面级的延迟不是这个工具的目标。
