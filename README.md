# HdrCast

把 Windows 桌面（含 HDR10 与桌面音频）以 10~100 Mbps 的高码率"推流式"分享给中国 / 日本 / 澳大利亚的朋友，延迟 1.5~5 s，画面不糊不卡。私人用，**点对点**：谁都能开播，同组的朋友直接从主播的电脑收流，不需要中继、不需要公网 IPv4、不用改路由器。

- **点对点（IPv6 打洞）**：双方经信令服务器交换地址后同时给对方发包，两边路由器和 Windows 防火墙都会放行这条流。信令服务器只转几百字节的签名元数据（谁在播、候选地址），**视频从不经过它**。
- **一次邀请码，永久可用**：主播建组后发一段 `hdrcast:…` 给朋友，朋友复制整条消息打开任一 app 就会提示加入，贴一次两个 app 都配好。
- **HDR10 全链路**：DXGI 桌面复制 → 自研 D3D11 compute shader（scRGB → PQ / BT.2020 / P010）→ NVENC HEVC Main10 → SRT（AES-256，每位观众单独重传）→ mpv HDR 直通。SDR 观众由 mpv 做色调映射。
- **码率、帧率、分辨率随便选**：10~100 Mbps、30/60/120 fps、原生 / 1440p / 1080p；支持 hevc_nvenc / hevc_amf / hevc_qsv / libx265。
- **多人同时看**：一路编码，按观众各起一个只转封装的 ffmpeg（不重编码），每人按自己地区的延迟重传；谁卡了只影响他自己。
- **桌面音频**：WASAPI loopback，不用虚拟声卡；与视频同一时钟。
- **两个绿色 exe**：`HdrCastSender.exe`（开播）与 `HdrCastReceiver.exe`（观看），.NET Framework 4.8，Windows 10/11 直接跑；ffmpeg 与 mpv 用 `scripts\get-tools.cmd` 一键下载。

> 现状：点对点整条链路（邀请码、信令、打洞、多观众分发、HEVC Main10 PQ）已在 Linux 上用模拟的有状态防火墙端到端测过；
> **Windows 上的运行时（采集、GPU shader、WASAPI、真实路由器打洞）还没实测**，步骤见 [docs/windows-checklist.md](docs/windows-checklist.md)。

## 架构（点对点模式）

```
┌──────────────── 主播 Windows PC ─────────────────────────────┐
│ HdrCastSender.exe                                             │
│   采集引擎 (DXGI + D3D11 shader + WASAPI) → 命名管道          │
│   → ffmpeg 编码 HEVC Main10 → MPEG-TS (stdout)                │
│   → 分发器：每位观众一个 remux ffmpeg（-c copy，SRT，127.0.0.1）│
│   → HostLink：[本机 IPv6]:端口 ⇄ 打洞 ⇄ 各位观众               │
└───────────────┬───────────────────────────────┬──────────────┘
                │ IPv6 直连（打洞）                │ IPv6 直连（打洞）
                ▼                               ▼
   HdrCastReceiver.exe → mpv（日本）   HdrCastReceiver.exe → mpv（中国 / 澳洲）

   腾讯云 hdrcast-signal（signal/）：只存"谁在播 + 候选地址"，全部用组密钥签名；不转视频
```

- 只要**双方至少有一方的 IPv6 能用**并且另一方也有 IPv6 就能连（IPv6 没有 NAT，打洞几乎总能成功）；IPv4 打洞只是额外尝试。
- 主播要为每位观众各上传一份：3 人 × 40 Mbps ≈ 150 Mbps 上行（含重传余量）。发送端界面会估算并在超过你填的上行带宽时提醒。
- 旧的"中继"与"直连"模式仍在（下拉框里），适合有公网 IP 或自建 MediaMTX 的人，见 [docs/network.md](docs/network.md)。

## 快速开始：开播的人

1. **装工具**：双击 `scripts\get-tools.cmd`（只开播就 `get-tools.cmd sender`，会下载 ffmpeg；只看就 `viewer`，下载 mpv）。
2. **运行 `HdrCastSender.exe`**，右上 **Mode** 选 **Group: friends watch straight from this PC (P2P)**。
3. **Create group…**：起个组名和你的名字，邀请码自动复制到剪贴板。把它发到微信 / LINE / Discord 群里（**只发给信得过的人**：有码就能看）。
   朋友已经发过你邀请码的话，直接复制那条消息、点 **Paste invite**（或者打开 app 时它会自己提示）。
4. 选显示器、码率、**Max viewers**（可以顺手填 *my upload*，你家上行 Mbps），点 **Start**。
   状态栏 **Viewers** 一行显示谁在看、走 IPv6 还是 IPv4、往返延迟，以及当前需要的上行。

## 快速开始：看的人

英文版给朋友看：[README.en.md](README.en.md)。

1. **装 mpv**：双击 `scripts\get-tools.cmd viewer`（或 `get-tools.cmd`，两个都装）。
2. **复制朋友发来的整条邀请消息**，打开 `HdrCastReceiver.exe`：会弹出"要加入这个组吗"，点是、填你的名字。没弹的话点 **Paste invite**。
3. Mode 自动切到 **Group**，**Watch** 列表里是正在直播的朋友（每 10 秒刷新）。**Latency** 按你在哪选：日本 1500 / 澳洲 2000 / 中国 3500 ms。点 **Play**。
4. 连不上时状态栏会直接说原因，比如"这台电脑没有 IPv6"或"主播没有 IPv6"。先去 <https://test-ipv6.com> 看看自己的 IPv6，见 [docs/faq.md](docs/faq.md)。
5. **HDR 显示器**先在 Windows 里打开 HDR（`Win + Alt + B`）；SDR 显示器选"SDR tone-map"。

## 信令服务器（只需部署一次）

`signal/` 是一个只用 Python 标准库的小服务，跑在腾讯云上（默认 `http://101.43.23.94:18650`），部署方法见 [signal/README.md](signal/README.md)：
`signal/deploy.sh --ssh ubuntu@101.43.23.94`，然后在腾讯云安全组放行 **TCP 18650** 和 **UDP 18651**（后者是 IPv4 打洞用的地址回显，可选）。
它看不到任何密码或组密钥，也伪造不了地址（所有消息用组密钥签名）；挂了的话，已经在看的人不受影响，只是新的人加入不了。

## 工具下载

| 工具 | 谁需要 | 下载 | 备注 |
|---|---|---|---|
| ffmpeg（full） | 开播的人 | <https://www.gyan.dev/ffmpeg/builds/> | 需要 libsrt；BtbN gpl 构建亦可 |
| mpv | 看的人 | <https://github.com/shinchiro/mpv-winbuild-cmake/releases> / <https://github.com/zhongfly/mpv-winbuild/releases> | 两家都带 libsrt |
| .NET Framework 4.8 | 两端 | Windows 自带 | 无需安装 |
| Python 3 | 信令服务器 | Ubuntu 自带 | 只用标准库 |

`scripts\get-tools.cmd` 一键下载 ffmpeg / mpv（`sender` / `viewer` / `both`，加 `force` 重新下载）；`scripts\check-tools.cmd` 检查编码器和 `srt://` 支持。

## 目录结构

```
sender/      HdrCastSender.exe：WinForms GUI + capture 采集引擎（DXGI / D3D11 / WASAPI）+ list-outputs
receiver/    HdrCastReceiver.exe：WinForms GUI，启动 mpv.exe 并通过 JSON IPC 显示状态
shared/      平台无关逻辑，同时编进两个 exe 与测试：
               Common（地址解析、SRT URL、日志脱敏）、Group（组、邀请码、密钥、签名消息）、
               P2p（打洞、HostLink/ViewerLink、分发、信令客户端、P2pHost/ViewerSession）、Gui（两个 app 共用的对话框）
signal/      信令服务器（Python 标准库）+ systemd 单元 + deploy.sh
tests/       xunit（net10，在 Linux 上跑，含打洞 / 真 ffmpeg / 真信令服务器的端到端测试）；vectors/ 是协议 golden 向量
relay/       可选：MediaMTX 中继（旧的中继模式）
scripts/     Windows 批处理：get-tools / check-tools / send-example / receive-example / dump-check
docs/        design.md（规格，§8 是点对点协议）、network.md、windows-checklist.md、faq.md、obs-alternative.md
build.sh     在 Linux 上构建并打包 dist/HdrCast-<版本>/
```

## 构建

```bash
./build.sh              # dotnet build 两个 exe (Release) → dotnet test → 打包 dist/HdrCast-<版本>/
./build.sh --no-test    # 跳过测试
./build.sh --e2e        # 额外跑 docker 中继回环（只和旧的中继模式有关）
python3 -m unittest discover -s signal/tests    # 信令服务器自己的测试
```

产物 `dist/HdrCast-<版本>/`：两个 exe、`scripts/`、`docs/`、`signal/`、`relay/`（不含密码文件）、README、LICENSE、空的 `tools/`。

## 文档

- [docs/design.md](docs/design.md) —— 设计定稿；§8 是点对点协议（邀请码、密钥派生、签名、信令 API、打洞包）。
- [docs/network.md](docs/network.md) —— 点对点怎么穿过防火墙、谁需要什么、上行带宽、IPv6 自查、旧的中继 / 直连模式。
- [docs/faq.md](docs/faq.md) —— 连不上怎么办（按状态栏提示查）、HDR 与 SDR、码率与带宽。
- [docs/windows-checklist.md](docs/windows-checklist.md) —— Windows 上逐步验证及预期输出。
- [signal/README.md](signal/README.md) —— 信令服务器部署。

## 许可

MIT，见 [LICENSE](LICENSE)。
