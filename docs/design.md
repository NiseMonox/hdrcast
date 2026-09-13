# HdrCast 设计定稿（v0.1）

> 本文是实装的唯一规格来源。所有参数都有来源：`docs/research/` 下的调研报告（读源码 + 本机实测）。
> 与本文冲突的实现以本文为准；本文有错就改本文。

## 0. 需求回顾

- 用户：本人（东京，Windows 11 + RTX 4090 + AW3225QF 4K HDR）和朋友们（中国 / 日本 / 澳大利亚，可能同时观看）。
- 码率可自定义（10~100 Mbps）；画面不糊、不卡；延迟可以大（1~5 s，"推流式"而非远程桌面式）。
- HDR10 全链路；带桌面音频。
- 不使用腾讯云服务器；中继只放家里的 LXC（192.168.1.201）。

## 1. 架构

```
┌──────────────── 发送端 Windows PC ────────────────┐
│ HdrCastSender.exe (GUI)                            │
│   ├─ 子进程 A: HdrCastSender.exe capture ...       │   采集引擎（同一个 exe 的命令行模式）
│   │     DXGI Desktop Duplication (FP16 scRGB / BGRA8)
│   │     → D3D11 compute shader → P010 (HDR10) 或 NV12 (SDR)
│   │     → 命名管道 \\.\pipe\hdrcast-video           │
│   │     WASAPI loopback → f32le 48k 2ch → \\.\pipe\hdrcast-audio
│   └─ 子进程 B: ffmpeg.exe                          │   只做编码 + 封装 + 发送
│         -f rawvideo ... -i \\.\pipe\hdrcast-video   │
│         -f f32le ... -i \\.\pipe\hdrcast-audio      │
│         -c:v hevc_nvenc (main10) -c:a aac -f mpegts srt://...
└────────────────────────────────────────────────────┘
                     │ SRT (AES-256, latency 0.5~1 s, LAN)
                     ▼
┌──────── LXC 192.168.1.201 (Docker) ────────┐
│ MediaMTX 1.21  路径 screen                  │   路由器放行 8890/udp（可选 8888/tcp HLS）
│ publish 与 read 分别鉴权 + 分别 passphrase   │
└─────────────────────────────────────────────┘
                     │ SRT (latency 1.5~4 s，按地区)
                     ▼
┌──────────── 观众 Windows PC ────────────┐
│ HdrCastReceiver.exe (GUI) → mpv.exe      │   gpu-next + d3d11 + target-colorspace-hint
│   HDR 屏：PQ 直通；SDR 屏：mpv 色调映射   │
└─────────────────────────────────────────┘
```

### 1.1 为什么采集不用 ffmpeg 的 ddagrab

ddagrab 本身能拿到正确标注的 scRGB FP16 D3D11 帧，但 ffmpeg **没有任何无损桥梁**把打包 FP16 送进 zscale / libplacebo：
swscale 的 `rgbaf16` 输入路径经 16 位整数中转，把所有 > 1.0（即 > 80 nit）的值裁掉（7.1.5 / 8.0 / 9.0.1 三个版本本机实测：2.5 → 1.0，125 → 1.0）；
zscale 不收打包浮点；Vulkan / CUDA 不能导入 D3D11 纹理；libplacebo 把 `LINEAR` 当 203 nit SDR。
因此色彩转换必须在 GPU 上自己做，然后把 P010 喂给 ffmpeg —— 这正是 OBS 的做法。附带好处：音频可以在同一进程用 WASAPI loopback 抓，不用装虚拟声卡。

### 1.2 为什么默认 HEVC 而不是 AV1

ffmpeg（7.1 与 master）的 mpegts 封装器和 MediaMTX 都**不支持 AV1-in-MPEG-TS**（本机实测：写成 private stream，读回是 bin_data）。
走中继就只能 HEVC Main10。AV1 只在「直连模式」（无中继，`-f matroska -live 1` over SRT）可选。

## 2. 发送端

### 2.1 exe 的三种运行模式

| 命令行 | 作用 |
|---|---|
| `HdrCastSender.exe` | WinForms GUI |
| `HdrCastSender.exe list-outputs [--json]` | 枚举 DXGI 输出：索引、`\\.\DISPLAYn`、友好名、分辨率、刷新率、HDR 是否开启（`DXGI_OUTPUT_DESC1.ColorSpace == RGB_FULL_G2084_NONE_P2020`）、MaxLuminance |
| `HdrCastSender.exe capture <参数>` | 采集引擎（无窗口，stdout 写 JSON 统计，stderr 写日志） |

采集引擎参数：

```
capture --output <idx>           DXGI 输出索引（list-outputs 给出的 index，按 adapter 顺序 EnumAdapters1 → EnumOutputs 全局编号）
        --fps <n>                目标恒定帧率，默认 60
        --format auto|p010|nv12  auto = 桌面是 HDR 就 p010，否则 nv12
        --size WxH               可选，输出分辨率（shader 内双线性缩放）；缺省 = 显示器原生
        --sdr-white <nits>       scRGB 1.0 对应的亮度，默认 80（Windows 规范值；不要跟随 SDR 亮度滑块）
        --cursor on|off          默认 on
        --video-pipe <name>      默认 hdrcast-video   （实际路径 \\.\pipe\<name>）
        --audio-pipe <name>      默认 hdrcast-audio；--audio off 则不创建
        --audio on|off           默认 on
        --audio-device default|<endpoint id>
```

### 2.2 视频管道协议

- 采集引擎是 **管道服务端**（`NamedPipeServerStream`，Out，缓冲 64 MB），创建后等待 ffmpeg 连接。
- 帧格式：`p010le`（Y 平面 W×H×2 字节 + UV 交错平面 W×(H/2)×2 字节，10 bit 高位对齐即 `value << 6`）或 `nv12`（W×H + W×H/2）。W、H 强制偶数。
- **恒定帧率**：用 `QueryPerformanceCounter` 做定时器；每个 tick 写一帧。节拍器与等待函数必须用**同一个时基**（`Stopwatch.GetTimestamp()` 的绝对值），混用「从 0 开始的 ElapsedTicks」会让等待永远立即返回，退化成按桌面刷新率忙转。`AcquireNextFrame(timeout=0 或很短)` 拿到新帧就转换，没有就重复上一帧（复制已转换好的缓冲，不重跑 shader）。这样 ffmpeg 侧用理想时间戳即可，不用 wallclock。
- 统计：每秒向 stdout 写一行 JSON：`{"t":秒,"captured":n,"duplicated":n,"dropped":n,"fps":x,"gpu_ms":x,"audio_underrun":n}`。GUI 逐行解析显示。

### 2.3 色彩转换（compute shader，HLSL，运行时用 d3dcompiler_47 编译）

输入：scRGB 线性 FP16（BT.709 基色，1.0 = `sdrWhite` nit，默认 80）。输出：HDR10 = PQ、BT.2020、非恒定亮度、limited range、4:2:0 10 bit。

```hlsl
// 常量与 OBS color.effect 一致；scRGB 1.0(=80 nit) → Y10 490，2.5(=200 nit) → 571，125 → 940（限幅）
// 本机 zscale(npl=80) 实测为 491 / 573 / 943，差 ≤3 码值（zimg 的取整与限幅上限略有不同）
static const float3x3 M709to2020 = {
  0.6274039, 0.3292830, 0.0433131,
  0.0690973, 0.9195404, 0.0113623,
  0.0163914, 0.0880133, 0.8955953 };
float pq_oetf(float x /* 单位: 10000 nit 归一 */) {
  const float m1 = 0.1593017578125, m2 = 78.84375, c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
  float p = pow(max(x, 0.0), m1);
  return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}
// 每像素：
float3 lin2020 = max(mul(M709to2020, scrgb.rgb), 0.0);        // 负值 = BT.2020 色域外 → 裁
float3 E = saturate(float3(pq_oetf(lin2020.r * sdrWhite / 10000.0),
                           pq_oetf(lin2020.g * sdrWhite / 10000.0),
                           pq_oetf(lin2020.b * sdrWhite / 10000.0)));
float Y  = 0.2627 * E.r + 0.6780 * E.g + 0.0593 * E.b;
float Cb = (E.b - Y) / 1.8814;
float Cr = (E.r - Y) / 1.4746;
uint y10  = (uint)round(64.0 + 876.0 * Y);          // limited range 10 bit
uint cb10 = (uint)round(512.0 + 896.0 * Cb);
uint cr10 = (uint)round(512.0 + 896.0 * Cr);
// P010 存储：y10 << 6，cb10 << 6，cr10 << 6
```

- 每个线程处理 2×2 像素块：4 个 Y，色度取 4 像素 PQ 编码后的平均（与 OBS 一致，简单够用）。
- 输出写入一个 `RWByteAddressBuffer`，布局即完整 P010 帧（Y 平面在前，UV 平面在后）；`CopyResource` 到 staging buffer，`Map` 后一次性写管道。
- 缩放：以 UV 坐标 + 线性采样器读源纹理（FP16 支持线性过滤）。
- SDR 桌面（BGRA8 sRGB）：同一 shader 的 nv12 变体：sRGB 值直接当 BT.709 gamma 编码用（不做 EOTF），`Y = 0.2126R + 0.7152G + 0.0722B`，`Cb = (B−Y)/1.8556`，`Cr = (R−Y)/1.5748`，8 bit limited（16 + 219Y，128 + 224C）。
- 鼠标：DXGI 提供指针形状（color / monochrome / masked color）与位置，CPU 端解码成 BGRA 小纹理，在 shader 中按位置 alpha 混合（HDR 路径下把 sRGB 光标解码到线性再乘 sdrWhite/80，粗略即可）。

### 2.4 音频

- `NAudio.Wasapi` 2.2.1（netstandard2.0）：`WasapiLoopbackCapture` 抓默认渲染设备；同时开一路 `WasapiOut` 播放静音，保证渲染引擎不进入空闲（否则 loopback 停止吐数据）。
- 输出格式统一重采样/转换为 **f32le 48000 Hz 2ch**（设备 mix format 若不同则用 NAudio 的 `MediaFoundationResampler` 或简单线性重采样）。
- **与视频锁同一时钟**：以 QPC 为准计算「应已输出的采样数」= elapsed × 48000；实际写入落后超过 20 ms 则补零，超前超过 20 ms 则丢弃多余采样。这样 ffmpeg 两路都用理想时间戳也不会漂。
- 管道 `\\.\pipe\hdrcast-audio`，同为服务端。

### 2.5 ffmpeg 命令模板

```
ffmpeg -hide_banner -loglevel warning -stats_period 1 -progress pipe:1 -nostdin
  -thread_queue_size 1024 -f rawvideo -pix_fmt {p010le|nv12} -video_size WxH -framerate FPS -i \\.\pipe\hdrcast-video
  -thread_queue_size 1024 -f f32le -ar 48000 -ac 2 -i \\.\pipe\hdrcast-audio          ← 音频关闭时省略
  {编码器参数：见 EncoderProfiles，来自 docs/research/encoder-settings}
  {HDR: -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv}
  {SDR: -color_primaries bt709  -color_trc bt709    -colorspace bt709    -color_range tv}
  -c:a aac -b:a 192k -ac 2                                                             ← 可选 libopus 128k
  -muxdelay 0 -muxpreload 0 -f mpegts -mpegts_flags +resend_headers
  "srt://HOST:PORT?{SRT 参数}"
```

- `-progress pipe:1` 输出 `key=value` 行（fps、bitrate、speed、frame、drop_frames、dup_frames、out_time_us），GUI 解析显示。
- 默认编码器：`hevc_nvenc`，`-profile:v main10`（HDR）/ `main`（SDR）。备选按探测结果：`hevc_amf`、`hevc_qsv`、`libx265`。AV1（`av1_nvenc`）**只在直连模式**可选，此时封装用 `-f matroska -live 1`（MediaMTX 不支持）。
- 输出像素格式按编码器显式指定：硬件编码器吃 `p010le` / `nv12`（即采集引擎的原生输出），CPU 编码器只收平面格式，必须写 `yuv420p10le` / `yuv420p`，否则 ffmpeg 可能协商成 8 bit，HDR 精度悄悄丢失。
- **libx265 的坑（本机实测 7.1.5）**：ffmpeg 的 libx265 封装只把 `-colorspace` 和 `-color_range` 传给 x265，`-color_primaries` 与 `-color_trc` **不会**进 VUI（ffprobe 读回是 `unknown`）。必须在 `-x265-params` 里写 `colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:range=limited`，否则观众端拿到的是无标记流，会按 SDR 显示。
- **libsvtav1 的坑（本机实测）**：`-maxrate` 只在 CRF 模式可用（否则报 "Max Bitrate only supported with CRF mode"），`rc=2`（CBR）要求低延迟预测结构（否则报 "CBR Rate control is currently not supported for SVT_AV1_PRED_RANDOM_ACCESS"）。所以这条路走 VBR（`rc=1`），码率由 SRT 侧兜。
- 编码器探测：启动时运行 `ffmpeg -hide_banner -encoders`，再对候选编码器做 1 帧 `-f lavfi -i nullsrc ... -f null -` 试编码判断可用。

### 2.6 SRT 参数（单位：`latency` 等是**微秒**）

| 场景 | 发送端 URL 参数 |
|---|---|
| 中继推流（默认） | `streamid=publish:screen:sender:<pw>&passphrase=<PUB_KEY>&pbkeylen=32&pkt_size=1316&transtype=live&latency=800000&maxbw=0&inputbw=<bitrate/8>&oheadbw=25&sndbuf=64000000&ffs=64000&connect_timeout=5000` |
| 直连·本机监听 | `mode=listener&passphrase=<KEY>&pbkeylen=32&latency=<按地区>&pkt_size=1316&listen_timeout=-1&maxbw=0&inputbw=..&oheadbw=..&sndbuf=..&ffs=..`（本机需路由器转发该 UDP 端口） |
| 直连·拨号到朋友 | `mode=caller&passphrase=<KEY>&pbkeylen=32&latency=<按地区>&pkt_size=1316&...`（朋友侧监听） |

- 缓冲大小公式：`packets = latency_s × bitrate_bps / (1316×8)`；`ffs ≥ packets`，`sndbuf/rcvbuf ≥ packets × 1472`。GUI 按码率 × 延迟自动算。
- **禁用 rendezvous**（ffmpeg 会 bind 到远端地址，无法用）。

### 2.7 GUI（WinForms，代码生成 UI，不用设计器 resx）

区块：
1. 显示器：下拉列表（来自 `list-outputs`），显示 HDR 状态；刷新按钮。模式：自动 / 强制 SDR。
2. 画质：编码器（探测结果）、码率 Mbps（数字框 + 预设 20/40/80）、帧率（30/60/120）、输出分辨率（原生 / 2560×1440 / 1920×1080）、质量档（速度优先 / 均衡 / 画质优先 → 映射到各编码器 preset）、GOP 秒（默认 2）、B 帧开关、lookahead 开关。
3. 音频：开关、设备、编码（AAC 192k / Opus 128k）。
4. 目标：模式（中继 / 直连监听 / 直连拨号）、主机、端口、路径/streamid 用户密码、passphrase、延迟 ms（地区预设：日本 1500 / 澳洲 2000 / 中国 3500）。
5. 控制：开始 / 停止、复制 ffmpeg 命令、打开日志目录；状态栏：采集 fps / 重复帧 / 编码 fps / 码率 / speed / 丢帧 / 已运行时间。
6. ffmpeg 路径：自动查找（exe 同目录 `ffmpeg.exe`、`tools\ffmpeg.exe`、PATH），可浏览；缺失时提示下载 gyan.dev **full** 版。
- 配置文件：`%APPDATA%\HdrCast\sender.json`（`DataContractJsonSerializer`）。
- 崩溃保护：ffmpeg 或采集进程意外退出 → 停止另一方，显示原因，可选「自动重连」（间隔 3 s 重启）。
- 进程树：两个 GUI 都创建 **Job Object（KILL_ON_JOB_CLOSE）** 并把每个子进程加入，任务管理器强杀 GUI 或 GUI 崩溃时子进程一起死。采集引擎另有 `--parent-pid`（用一个 `OpenProcess(SYNCHRONIZE)` 句柄轮询，不是每帧扫全系统进程表）。
- **启动顺序**：采集引擎必须先把**两个**管道都建好，GUI 轮询到管道出现后才启动 ffmpeg（固定 sleep 不够：引擎要先建 D3D11 设备并编译 shader）。音频管道必须在等待视频管道客户端**之前**创建，否则两边互等。
- 音频端点打不开时**不终止会话**：管道照建，写静音，日志说明原因（ffmpeg 已经被告知有音频输入，缺管道会直接退出）。
- 日志与界面里的命令行一律经 `SecretRedactor` 脱敏（passphrase 与 streamid 里的密码），因为 README 会让用户把日志发出来排障。

## 3. 接收端

### 3.1 mpv 命令（来自 docs/research/mpv-hdr-windows，已核对 mpv 0.40/0.41 手册与源码）

```
mpv --vo=gpu-next --gpu-api=d3d11 --hwdec=d3d11va
    --target-colorspace-hint=yes            ← HDR 屏（Windows 必须已开 HDR）；SDR 屏改为 no 并加 --tone-mapping=bt.2390
    --hdr-compute-peak=auto
    --cache=yes --demuxer-readahead-secs=3 --cache-pause-initial=yes --cache-pause-wait=1.5 --demuxer-max-bytes=256MiB
    --video-sync=audio --interpolation=no --framedrop=vo
    --demuxer-lavf-probesize=5000000 --demuxer-lavf-analyzeduration=2
    --input-ipc-server=\\.\pipe\hdrcast-mpv --keep-open=no --force-window=yes --title="HdrCast"
    [--fs --fs-screen=N]
    "srt://HOST:8890?streamid=read:screen:viewer:<pw>&passphrase=<READ_KEY>&latency=<µs>&rcvbuf=<bytes>&ffs=<n>&recv_buffer_size=8000000"
```

- SRT 选项一律放 URL 查询串（`--demuxer-lavf-o` 到不了协议层）。
- `rcvbuf/ffs` **必须**按 码率 × 延迟 计算（否则 > 40 Mbps + 2 s 时 libsrt 会 "No room to store incoming packet" 断线）。码率由用户在 GUI 填（或默认按 100 Mbps × 延迟上限算，宁大勿小）。
- MediaMTX 在推流未开始时会直接拒绝读者 → GUI 循环重试（3 s 间隔，显示"等待推流…"）。
- HDR 直通只在 `--wid` 嵌入或独立窗口下都以「窗口化 HDR」工作，静态元数据只在全屏时下发 → 提供全屏按钮（`--fs`，不用 `--d3d11-exclusive-fs`）。
- **不嵌入 libmpv**，独立启动 mpv.exe；用 JSON IPC 命名管道读 `demuxer-cache-duration`、`frame-drop-count`、`video-params`、`video-target-params`、`hwdec-current`、`estimated-vf-fps`、`paused-for-cache` 显示状态。
- 兜底：「用 ffmpeg 拉流」模式 = `ffmpeg -i srt://... -c copy -f mpegts - | mpv -`（万一 mpv 构建没带 libsrt）。
- mpv 路径：同目录 `mpv.exe` / `tools\mpv.exe` / PATH；缺失时提示下载 shinchiro 或 zhongfly 构建（均带 libsrt）。
- 直连模式：接收端也可做 `mode=listener`（朋友转发端口）或 `mode=caller`。
- 配置：`%APPDATA%\HdrCast\receiver.json`。

## 4. 中继（relay/）

- `docker-compose.yml` + `mediamtx.yml`：见调研交付（已 `--validate-conf` 并跑通 50/100 Mbps HEVC10）。要点：`writeQueueSize: 2048`、`udpReadBufferSize: 4000000`（宿主 `sysctl -w net.core.rmem_max=16777216`）、`authInternalUsers` 分 sender / viewer、路径 `screen` 分 `srtPublishPassphrase` / `srtReadPassphrase`、`hls: yes`（浏览器兜底，SDR 观感由浏览器决定）。
- `relay/setup.sh`：生成随机密码与 passphrase 写入 `mediamtx.yml` 与 `relay/.env`，打印给发送端 / 接收端填的 URL；`relay/selftest.sh`：用本机 ffmpeg 生成合成 HDR10 HEVC 源推到中继，再用 ffprobe 读回校验 `smpte2084 / bt2020 / yuv420p10le`。
- 端口：8890/udp（必转发）、8888/tcp（可选）、9997 仅本机。

## 5. 网络与地区预设

| 观众 | RTT 估计 | SRT latency | oheadbw |
|---|---|---|---|
| 日本 | 10~30 ms | 1500 ms | 25 |
| 澳大利亚 | 120~160 ms | 2000 ms | 25 |
| 中国（跨境丢包） | 60~120 ms + 丢包 | 3500 ms | 50 |

发送端 → 中继在 LAN 内：800 ms 足够。总延迟 ≈ 发送 SRT + 接收 SRT + mpv `cache-pause-wait` + 编码 lookahead（~0.1 s）。

## 6. 工程与构建

- `sender/`、`receiver/`：net48 WinForms，`Microsoft.NETFramework.ReferenceAssemblies`，`ILRepack.Lib.MSBuild.Task` 把 SharpDX(.DXGI/.Direct3D11/.D3DCompiler 4.2.0)、NAudio.Core/NAudio.Wasapi 2.2.1 合并成单 exe；合并失败则退回「exe + DLL 目录」。
- `shared/`：平台无关逻辑（命令行构造、SRT URL / 缓冲计算、`-progress` 与 mpv IPC 解析、设置模型、编码器档位表），以源码链接方式编进两个 exe 与 `tests/`（net10 + xunit，本机运行）。
- `build.sh`：`dotnet build -c Release` 两个工程 → `dist/HdrCast-<ver>/`（exe、`relay/`、`scripts/`、`docs/`、README）。
- 配置存 `%APPDATA%\HdrCast\`；日志存 `%APPDATA%\HdrCast\logs\`。

## 7. 验证计划

Linux 本机：`dotnet build` 0 警告；`dotnet test` 覆盖命令行构造 / URL / 缓冲计算 / 解析器；`relay/selftest.sh` 跑通合成 HDR10 → MediaMTX → ffprobe。
Windows（只能由用户做）：`list-outputs` 正确识别 HDR；`capture` 单独跑 10 s 落盘为 `.p010` 用 ffmpeg 转 PNG 检查亮度；完整推流；朋友端 mpv HDR 直通 / SDR 色调映射；4K60 下采集 fps 与 GPU 占用。
