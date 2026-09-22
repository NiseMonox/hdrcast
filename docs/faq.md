# HdrCast 常见问题

## 连不上怎么办？

点对点（Group）模式下，接收端状态栏会直接说原因，对照 `docs/network.md` §1.2：

- **"This PC has no IPv6…" / "The host's PC has no IPv6…"**：哪一方没 IPv6 就去哪一方的路由器打开 IPv6（`docs/network.md` §2），
  <https://test-ipv6.com> 可以自查。手机热点通常自带 IPv6，可以先用热点试一下。
- **"Both sides have IPv6 but the path is blocked"**：关掉 VPN / 加速器；路由器里有"IPv6 防火墙：阻止所有入站"之类的严格选项就改回默认。
- **"Not live right now"**：主播还没开播，列表每 10 s 刷新。**"… is full"**：主播的 Max viewers 满了。
- **"The signaling server cannot be reached"**：本机断网或信令服务器挂了；已经在看的人不受影响。
- 能连上但**画面一直不出来**：mpv 在等第一个关键帧（最多一个 GOP，默认 2 s）；超过 10 s 看日志里 `[remux …]` 的报错（在主播那边）。

## IPv6 是必须的吗？

点对点靠 IPv6 打洞：IPv6 没有 NAT，双方同时发包就能穿过两边的有状态防火墙，不用任何设置。
没有 IPv6 时 app 也会试 IPv4（经信令服务器的 UDP 回显拿到公网映射再打洞），但国内家宽、日本 v6プラス / DS-Lite、
澳洲部分 ISP 的 IPv4 都在运营商级 NAT 后面，基本打不通。你选了"不走腾讯云中转"，所以这种情况会明确提示，不会偷偷绕路。
同一局域网里的人不需要 IPv6（走局域网 IPv4）。

## 腾讯云服务器会看到什么？

只看到"谁在播"（主播名字、候选地址、码率、观众数）和加入 / 应答消息，全部用组密钥签名；
看不到组密钥、SRT passphrase，也碰不到视频。服务器被人控制的最坏结果是让人连不上，没法把观众引到别处。详见 `docs/network.md` §6。

## 为什么没有 AV1？

ffmpeg（7.1 与 master）的 MPEG-TS 封装器不支持 AV1：会写成 private stream (0x06)，读回来变成 `bin_data`。
点对点的分发是按 MPEG-TS 做的（中途加入的观众靠 TS 的重复头恢复），所以只能 **HEVC Main10**。
v0.1 的"直连模式"可以用 Matroska 走 AV1，但它要求一方做端口转发，已经随中继模式一起删除。
哪天 ffmpeg 支持 AV1-in-MPEG-TS 了，测试 `Av1_really_cannot_be_muxed_into_mpegts` 会开始失败，那就是重新考虑的信号。

## 观众的 mpv 为什么要先在 Windows 里打开 HDR？

mpv 的 HDR 直通 (`--vo=gpu-next --gpu-api=d3d11 --target-colorspace-hint=yes`) 只是向 DXGI 交换链声明 "我输出的是 PQ / BT.2020"，**它自己不会切换 Windows 的 HDR 模式**（旧驱动那种"mpv 会自动开 HDR"的说法从 NVIDIA R560 起已失效）。Windows HDR 没开时这个声明会被忽略或得到一个奇怪的画面（发灰/过暗）。
操作：`Win + Alt + B` 切换 HDR，或 设置 → 系统 → 显示 → 使用 HDR。接收端 GUI 的 "自动" 模式只在检测到 HDR 输出时才启用直通，其余情况走色调映射，所以忘了开也不至于全黑，只是画面不是真 HDR。

另外静态 HDR 元数据只在**全屏**时才会下发给显示器（Windows 限制），窗口化也是 HDR 但显示器不会切到 HDR 模式指示；想要"真全屏"就用 GUI 的全屏按钮（`--fs`，不要 `--d3d11-exclusive-fs`）。

## 观众是 SDR 显示器怎么办？

两个选择：

1. **mpv 色调映射（推荐）**：接收端选 "SDR 色调映射"（`--target-colorspace-hint=no --tone-mapping=bt.2390`，可选 `--hdr-compute-peak=yes`），mpv 在 GPU 上把 PQ 映射到 SDR，观感接近发送端的 SDR 桌面，高光被压缩。
2. 发送端切「强制 SDR」：桌面按 SDR 采集（nv12 / BT.709），所有人看 SDR，但发送端自己的 HDR 桌面在 SDR 采集下亮度会被裁切——只在观众全是 SDR 时用。

## 码率怎么选？

HEVC 10-bit、60 fps、桌面/游戏混合内容的经验值：

| 分辨率 | 够用 | 好 | 极致（NVENC 已无收益） |
|---|---|---|---|
| 1920×1080 | 12 Mbps | 20 Mbps | 30 Mbps |
| 2560×1440 | 20 Mbps | 35 Mbps | 50 Mbps |
| 3840×2160 | 40 Mbps | 60 Mbps | 80~100 Mbps |

- 文字多的桌面比游戏更需要码率（高频细节），快速运动的游戏 + 高码率才不糊。
- 4K 显示器分享游戏时，在 GUI 里把输出分辨率降到 2560×1440 通常比 4K 低码率更清楚，也少占一半带宽。
- 码率上限由**主播的上行带宽 ÷ (观众数 × 1.25)** 决定，而不是画质意愿，见下一条。
- GOP 保持 1~2 s（默认 2 s）：观众加入时要等到下一个 IDR 才有画面，mpv 在此期间会刷 "Could not find ref with POC"，是正常现象。

## 上传带宽怎么估？

主播的电脑给每个观众各发一份，所以：

```
上行需求 ≈ 码率 × 观众数 × 1.25（SRT 开销 oheadbw=25 %；中国观众 oheadbw=50 % 时按 ×1.5）
```

- 40 Mbps × 2 人 = 100 Mbps；40 Mbps × 3 人（含一位中国）≈ 160 Mbps。
- 先 speedtest 看上行，留 30 % 余量。上行不够就降码率或减少人数，不要把 oheadbw 调低（Haivision 的原话：太低时线路一有丢包"流就会立刻卡死"）。
- 发送端界面按 Max viewers 估算上行，填了 *my upload* 会在超过 80 % 时标红；状态栏 **Viewers** 显示当前实际需要的上行。
- 国内家宽上行常见 30~100 Mbps、澳洲 NBN 常见 20~50 Mbps：朋友开播给两三个人看时，码率一般要降到 10~20 Mbps。

## 画面卡顿 / 断线，按什么顺序排查？

1. 接收端 mpv 日志里有 `No room to store incoming packet` → 接收缓冲不够，GUI 的「码率上限」填大一点（它决定 rcvbuf/ffs），见 `docs/network.md` §4。
2. 线路丢包：让这位观众把 latency 加 500~1000 ms 再 Play（点对点模式下这个值直接用于主播给他的那条连接）；仍不行就降码率。
   主播的上行不够时所有观众一起卡：看状态栏 **Viewers** 里的 upload 估算。
3. 发送端状态栏 `speed` 持续 < 1.0× 或 `drop_frames` 增长 → 编码器跟不上：降分辨率 / 帧率，或质量档改成"速度优先"。
4. 采集统计 `dropped` 增长而 `captured` 正常 → 管道写入被 ffmpeg 阻塞，一般是上一条的编码器问题。

## 音频怎么来的？需要虚拟声卡吗？

不需要。采集引擎用 WASAPI loopback 抓默认播放设备的混音（同时播放一路静音防止渲染引擎休眠），重采样成 48 kHz 立体声 f32，与视频用同一个 QPC 时钟对齐后通过命名管道给 ffmpeg，编码为 AAC 192k（或 Opus 128k）。在 GUI 里可以选别的输出设备，或关掉音频。

## 为什么采集不用 ffmpeg 自带的 ddagrab？

ddagrab 能拿到正确的 scRGB FP16 帧，但 ffmpeg 内部没有任何无损路径把打包 FP16 交给 zscale / libplacebo：swscale 会把 > 1.0（> 80 nit）的值全部裁掉（三个版本实测），所以 HDR 会被压成 SDR。HdrCast 用自己的 D3D11 compute shader 在 GPU 上做 scRGB → PQ/BT.2020 → P010，再把 P010 喂给 ffmpeg 只做编码，这也是 OBS 的做法。详见 `docs/design.md` §1.1。

## 安全性如何？

- 邀请码里的 32 字节组密钥是一切的根：派生出信令鉴权 key、消息签名 key、SRT passphrase（AES-256，`pbkeylen=32`）和每个会话的打洞 key。
  **有邀请码就能进组看**，只发给信得过的人；要踢人就新建一个组、把新邀请码发给其余的人。
- 视频从主播的 remux 子进程到观众的 mpv 端到端 AES-256；打洞包带 HMAC，未通过验证的来源发来的数据一律丢弃，碰不到主播本机的 ffmpeg。
- 信令服务器只见签名的元数据（见上面"腾讯云服务器会看到什么"）。

## 延迟能再低一点吗？

可以，但这是"推流式"设计：把观众的 latency 改成 4×RTT（日本 ~200 ms、澳洲 ~700 ms），mpv 的 `cache-pause-wait` 改成 0.5 s，总延迟能到 1.5 s 左右；线路一抖就会卡。不建议低于 3×RTT。远程桌面级的延迟不是这个工具的目标。
