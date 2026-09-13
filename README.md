# HdrCast

把 Windows 桌面（含 HDR10 与桌面音频）以 10~100 Mbps 的高码率"推流式"分享给中国 / 日本 / 澳大利亚的朋友，延迟 1.5~5 s，画面不糊不卡。私人用，不走任何第三方平台：中继只跑在家里的 Linux 容器上。

- **HDR10 全链路**：DXGI 桌面复制 → 自研 D3D11 compute shader（scRGB → PQ / BT.2020 / P010）→ NVENC HEVC Main10 → SRT（AES-256）→ MediaMTX 中继 → mpv HDR 直通。SDR 观众由 mpv 做色调映射，SDR 桌面自动走 NV12。
- **码率、帧率、分辨率随便选**：10~100 Mbps、30/60/120 fps、原生 / 1440p / 1080p；支持 hevc_nvenc / hevc_amf / hevc_qsv / libx265，直连模式还可选 AV1。
- **多人同时看**：MediaMTX 给每位观众各发一份；推流与拉流分别鉴权、分别加密。
- **桌面音频**：WASAPI loopback，不用虚拟声卡；与视频同一时钟。
- **两个绿色 exe**：`HdrCastSender.exe`（发送端 GUI + 采集引擎）与 `HdrCastReceiver.exe`（观众 GUI），.NET Framework 4.8，Windows 10/11 直接跑；ffmpeg 与 mpv 另行下载放旁边即可。

## 架构

```
┌──────────────── 发送端 Windows PC ─────────────────┐
│ HdrCastSender.exe (GUI)                             │
│   ├─ HdrCastSender.exe capture   采集引擎（同一 exe）│
│   │     DXGI 桌面复制 (scRGB FP16 / BGRA8)          │
│   │     → D3D11 compute shader → P010 (HDR) / NV12  │
│   │     → 命名管道 \\.\pipe\hdrcast-video            │
│   │     WASAPI loopback → f32le 48k → hdrcast-audio  │
│   └─ ffmpeg.exe   只做编码 + MPEG-TS + SRT 发送      │
│         hevc_nvenc main10 / aac 192k                 │
└──────────────────────┬──────────────────────────────┘
                       │ SRT (AES-256, 800 ms, 局域网)
                       ▼
┌────── 家里的 LXC 192.168.1.201 (Docker) ──────┐
│ MediaMTX 1.21  路径 screen                     │  路由器转发 8890/udp（可选 8888/tcp HLS）
│ sender 只能 publish / viewer 只能 read          │
│ publish 与 read 各一把 passphrase              │
└──────────┬──────────────┬─────────────────────┘
           │ SRT 1.5 s     │ SRT 2~3.5 s
           ▼              ▼
   观众 A（日本）      观众 B/C（澳洲 / 中国）
   HdrCastReceiver.exe → mpv.exe (gpu-next + d3d11, HDR 直通或色调映射)
```

## 快速开始：发送端（你）

1. **下载 ffmpeg**：<https://www.gyan.dev/ffmpeg/builds/> 的 **release full** 版，解压后把 `bin\ffmpeg.exe`（顺便 `ffprobe.exe`）放到 `HdrCastSender.exe` 旁边或 `tools\` 子目录（也可以放在 PATH 里）。
2. **在 LXC 上启动中继**：
   ```bash
   cd /projects/hdrcast
   relay/setup.sh      # 生成密码与 passphrase，打印给两端填的值（保存在 relay/.env）
   relay/up.sh         # docker compose 启动 MediaMTX；relay/logs.sh 看日志，relay/down.sh 停止
   ```
   路由器把 **UDP 8890** 转发到 `192.168.1.201`；宿主执行一次 `sysctl -w net.core.rmem_max=16777216`（脚本会提醒）。想先在本机验证整条链路：`relay/selftest.sh`（合成 HDR10 → 中继 → ffprobe/mpv 全自动校验）。
3. **运行 `HdrCastSender.exe`**：选显示器（HDR 状态会显示）、码率（预设 20/40/80 Mbps）、帧率；目标选"中继"，填 `192.168.1.201:8890`、用户 `sender` + 密码、**publish** passphrase，延迟 800 ms；点开始。状态栏显示采集 fps / 编码 fps / 码率 / 丢帧。

不想用 GUI 时：`scripts\send-example.cmd` 是同一条 ffmpeg 命令的裸奔版（先在第二个窗口起采集引擎）。

## 快速开始：观众（朋友）

英文版给朋友看：[README.en.md](README.en.md)。

1. **下载 mpv**（必须是带 libsrt 的构建）：<https://github.com/shinchiro/mpv-winbuild-cmake/releases> 或 <https://github.com/zhongfly/mpv-winbuild/releases> 的 `mpv-x86_64-*.7z`，把 `mpv.exe` 放到 `HdrCastReceiver.exe` 旁边或 `tools\`。
2. **运行 `HdrCastReceiver.exe`**：粘贴你发给他的 主机（公网 IP / DDNS）、端口 8890、用户 `viewer` + 密码、**read** passphrase；地区选 日本 1500 ms / 澳洲 2000 ms / 中国 3500 ms；点连接。推流没开始时会显示"等待推流…"并自动重试。
3. **HDR 显示器**先在 Windows 里打开 HDR（`Win + Alt + B`），mpv 才能直通；SDR 显示器选"SDR 色调映射"。全屏按钮 = 真 HDR 全屏。

裸奔版：`scripts\receive-example.cmd`。浏览器兜底：`http://viewer:密码@主机:8888/screen`（SDR 观感看浏览器）。

## 工具下载

| 工具 | 谁需要 | 下载 | 备注 |
|---|---|---|---|
| ffmpeg（full） | 发送端 | <https://www.gyan.dev/ffmpeg/builds/> | essentials 也带 libsrt；BtbN gpl 构建亦可 |
| mpv | 观众 | <https://github.com/shinchiro/mpv-winbuild-cmake/releases> / <https://github.com/zhongfly/mpv-winbuild/releases> | 两家都带 libsrt；mpv.io 官网列表里的也是这两家 |
| Docker + Compose | 中继（LXC） | 已装 | 镜像 `bluenviron/mediamtx:1.21.0` |
| .NET Framework 4.8 | 两端 | Windows 自带 | 无需安装 |

`scripts\check-tools.cmd` 会检查 ffmpeg / mpv 是否找到、hevc_nvenc / av1_nvenc / hevc_amf / hevc_qsv 是否可用、mpv 是否支持 `srt://`。

## 目录结构

```
sender/      HdrCastSender.exe：WinForms GUI + capture 采集引擎（DXGI / D3D11 / WASAPI）+ list-outputs
receiver/    HdrCastReceiver.exe：WinForms GUI，启动 mpv.exe 并通过 JSON IPC 显示状态
shared/      平台无关逻辑（SRT URL / 缓冲计算、ffmpeg -progress 与 mpv IPC 解析、设置模型、编码器档位），同时编进两个 exe 与测试
tests/       xunit（net10，在 Linux 上跑）
relay/       MediaMTX：docker-compose.yml、mediamtx.template.yml、setup.sh / up.sh / down.sh / logs.sh、selftest.sh
scripts/     Windows 批处理：check-tools / send-example / receive-example / dump-check
docs/        design.md（规格）、network.md、windows-checklist.md、faq.md、obs-alternative.md、research/（调研报告 JSON）
build.sh     在 Linux 上构建并打包 dist/HdrCast-<版本>/
```

## 构建

Linux（本机：Debian 13 + .NET 10 SDK）或 Windows 都可以；Linux 上用 `Microsoft.NETFramework.ReferenceAssemblies` 交叉构建 net48 WinForms，已验证。

```bash
./build.sh              # dotnet build 两个 exe (Release) → dotnet test → 打包 dist/HdrCast-0.1.0/
./build.sh --no-test    # 跳过测试
```

产物 `dist/HdrCast-<版本>/`：两个 exe（及可能的 DLL）、`relay/`（不含密码文件）、`scripts/`、`docs/`、README、LICENSE、空的 `tools/`。把整个目录复制到 Windows，ffmpeg.exe / mpv.exe 放进 `tools\` 即可。

单独构建：`dotnet build sender/HdrCast.Sender.csproj -c Release`，`dotnet test tests/HdrCast.Tests.csproj`。

## 文档

- [docs/design.md](docs/design.md) —— 设计定稿，所有参数的来源。
- [docs/network.md](docs/network.md) —— 三种拓扑、地区延迟预设的理由、缓冲公式、防火墙、为何不用腾讯云、上行带宽估算。
- [docs/windows-checklist.md](docs/windows-checklist.md) —— Windows 上逐步验证（list-outputs → --selftest → dump → 推流 → 观众）及预期输出。
- [docs/faq.md](docs/faq.md) —— AV1 为何只能直连、mpv 与 Windows HDR、SDR 屏、码率与带宽、排障顺序。
- [docs/obs-alternative.md](docs/obs-alternative.md) —— 用 OBS 当备用发送端（未实测）。

## 许可

MIT，见 [LICENSE](LICENSE)。
