# HdrCast 设计定稿（v0.2：点对点）

> 本文是实装的唯一规格来源。所有参数都有来源：`docs/research/` 下的调研报告（读源码 + 本机实测）。
> 与本文冲突的实现以本文为准；本文有错就改本文。

## 0. 需求回顾

- 用户：本人（东京，Windows 11 + RTX 4090 + AW3225QF 4K HDR）和朋友们（中国 / 日本 / 澳大利亚，可能同时观看）。
- 码率可自定义（10~100 Mbps）；画面不糊、不卡；延迟可以大（1~5 s，"推流式"而非远程桌面式）。
- HDR10 全链路；带桌面音频。
- v0.2（2026-09）：**只有点对点模式**，不依赖用户家的 LXC：谁都能开播，同组朋友直接从主播电脑收流（§1、§8）。
  v0.1 的 MediaMTX 中继模式和手动直连模式已删除（AV1 只能走直连，所以一起去掉，只剩 HEVC）。
  腾讯云服务器只做信令（几百字节的签名元数据），**不转视频**；连不上时不中转，只提示原因。
- 很多人没有公网 IPv4，但大多有 IPv6：点对点优先走 IPv6 打洞，IPv4 打洞只是额外尝试。配对用邀请码，不手输地址。

## 1. 架构

```
主播 PC                                                                     观众 PC
 采集引擎 → ffmpeg 编码 → MPEG-TS(stdout) → SwitchableStream → TsFanOut
    → 每位观众一个 remux ffmpeg（-c copy，SRT listener 127.0.0.1:Y，观众的 latency/oheadbw）
    → HostLink（WAN socket：[稳定 IPv6]:P、0.0.0.0:P4） ⇄ 打洞后的 UDP 路径 ⇄ ViewerLink ⇄ mpv（srt://127.0.0.1:L）
        │                                                                    │
        └──── 签名的 live / join / answer ──── 腾讯云 hdrcast-signal ────────┘
```

主播端 `HdrCastSender.exe` 的采集部分（子进程 A：同一个 exe 的 `capture` 模式，DXGI 桌面复制 → D3D11 shader → P010/NV12 →
命名管道；WASAPI loopback → 音频管道）和编码 ffmpeg（子进程 B，只做编码 + 封装）见 §2；观众端 `HdrCastReceiver.exe` 启动 mpv，见 §3。

- 为什么这样分：SRT 本身端到端不变（AES、重传、按观众的 latency），HdrCast 只在两端各加一个透明 UDP 转发器负责打洞；
  ffmpeg 的 SRT listener 只接受一个连接，所以每位观众一个 remux 子进程，一路编码喂给所有子进程。
- 编码器 stdout 直接进分发器（`-progress` 改到 stderr，按"key=value"行过滤）：比让 ffmpeg 往 Windows 命名管道写输出更稳
  （后者要 `-y` 且依赖 CreateFile 对管道的处理，无法在 Linux 上验证）。编码器自动重启时新 stdout 接进同一个 SwitchableStream，观众不掉线。
- 实现：`shared/Group`（邀请码、密钥、签名消息、group.json）、`shared/P2p`（打洞、链路、分发、信令客户端、P2pHost、ViewerSession）、
  `signal/`（信令服务器）。协议与格式见 §8。

### 1.1 为什么采集不用 ffmpeg 的 ddagrab

ddagrab 本身能拿到正确标注的 scRGB FP16 D3D11 帧，但 ffmpeg **没有任何无损桥梁**把打包 FP16 送进 zscale / libplacebo：
swscale 的 `rgbaf16` 输入路径经 16 位整数中转，把所有 > 1.0（即 > 80 nit）的值裁掉（7.1.5 / 8.0 / 9.0.1 三个版本本机实测：2.5 → 1.0，125 → 1.0）；
zscale 不收打包浮点；Vulkan / CUDA 不能导入 D3D11 纹理；libplacebo 把 `LINEAR` 当 203 nit SDR。
因此色彩转换必须在 GPU 上自己做，然后把 P010 喂给 ffmpeg —— 这正是 OBS 的做法。附带好处：音频可以在同一进程用 WASAPI loopback 抓，不用装虚拟声卡。

### 1.2 为什么只有 HEVC、没有 AV1

ffmpeg（7.1 与 master）的 mpegts 封装器**不支持 AV1-in-MPEG-TS**（本机实测：写成 private stream，读回是 bin_data；
`EncoderArgsIntegrationTests.Av1_really_cannot_be_muxed_into_mpegts` 盯着它）。点对点的分发靠 MPEG-TS（中途加入的观众靠 TS 的重复头恢复），
所以只能 **HEVC Main10**。v0.1 的直连模式可以用 Matroska 走 AV1，但直连模式已随中继一起删除。

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
- **与视频锁同一时钟**：以 QPC 为准计算「应已输出的采样数」= elapsed × 48000；实际写入落后超过 150 ms 则补零，超前超过 150 ms 则丢弃多余采样。这样 ffmpeg 两路都用理想时间戳也不会漂。
  容差必须远大于 loopback 的交付粒度：NAudio 轮询取数，音频一批几十毫秒、还略有延迟。v0.2.1 用 20 ms 时，写管道的线程在两批之间就补零，
  下一批又被当成"超前"砍掉开头，每秒几十次（引擎统计 `audio_underrun` ≈ 38/s），观众听到的是低沉的嗡嗡声。容差只用来吸收时钟漂移和真正的卡顿。
- **排除某个程序的声音**（`--audio-exclude <app.exe>`，GUI 的 *Leave out*，类似 Parsec 的 Echo Cancelling）：用 Windows 的进程 loopback
  （`ActivateAudioInterfaceAsync(VAD\Process_Loopback)`，`PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`，Windows 10 21H1+ / 11）
  抓"除这个进程树之外的全部桌面声音"，典型用法是排除语音聊天，免得同在语音里的朋友听到自己的回声。
  - 这个 API 一次只能排除**一个**进程树，所以目标是该程序名**最上层**的那个进程（Discord 的声音由主进程拉起的子进程播放）；
    程序没在运行时排除引擎自己（等于全抓）。每秒拍一次进程快照（Toolhelp32，一两毫秒），程序启动 / 退出 / 重启就换目标重开采集。
  - 虚拟端点没有 mix format，直接要 float 48 kHz 立体声（`AUTOCONVERTPCM`）；COM 全在一个 MTA 线程里做（引擎入口是 `[STAThread]`，
    激活回调在 STA 上需要消息泵）。系统不支持时退回普通 loopback 并写日志。
  - 实测（2026-09-24，Windows 11）：两个程序各放一个 −60 dBFS 的单音，排除其中一个后它在采集结果里是 −150 dBFS（没有），另一个仍是 −60 dBFS；
    被排除的程序在开播后才启动也在下一次检查时切换，没有漏出。
- 管道 `\\.\pipe\hdrcast-audio`，同为服务端。

### 2.5 ffmpeg 命令模板

```
ffmpeg -hide_banner -loglevel warning -nostdin -stats_period 1 -progress pipe:2
  -thread_queue_size 1024 -f rawvideo -pix_fmt {p010le|nv12} -video_size WxH -framerate FPS -i \\.\pipe\hdrcast-video-<pid>
  -thread_queue_size 1024 -f f32le -ar 48000 -ac 2 -i \\.\pipe\hdrcast-audio-<pid>    ← 音频关闭时省略
  {编码器参数：见 EncoderProfiles，来自 docs/research/encoder-settings}
  {HDR: -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv}
  {SDR: -color_primaries bt709  -color_trc bt709    -colorspace bt709    -color_range tv}
  -c:a aac -b:a 192k -ac 2                                                             ← 可选 libopus 128k
  -muxdelay 0 -muxpreload 0 -f mpegts -mpegts_flags +resend_headers pipe:1
```

- stdout（`pipe:1`）是 MPEG-TS 本身，进 P2P 分发（§8.8）；`-progress pipe:2` 的 `key=value` 行（fps、bitrate、speed、frame、drop_frames、
  dup_frames、out_time_us）和日志共用 stderr，按行的形状区分后给 GUI 显示。
- 默认编码器：`hevc_nvenc`，`-profile:v main10`（HDR）/ `main`（SDR）。备选按探测结果：`hevc_amf`、`hevc_qsv`、`libx265`。
- 输出像素格式按编码器显式指定：硬件编码器吃 `p010le` / `nv12`（即采集引擎的原生输出），libx265 只收平面格式，必须写 `yuv420p10le` / `yuv420p`，否则 ffmpeg 可能协商成 8 bit，HDR 精度悄悄丢失。
- **libx265 的坑（本机实测 7.1.5）**：ffmpeg 的 libx265 封装只把 `-colorspace` 和 `-color_range` 传给 x265，`-color_primaries` 与 `-color_trc` **不会**进 VUI（ffprobe 读回是 `unknown`）。必须在 `-x265-params` 里写 `colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:range=limited`，否则观众端拿到的是无标记流，会按 SDR 显示。
- 编码器探测：启动时运行 `ffmpeg -hide_banner -encoders`，再对候选编码器做 1 帧 `-f lavfi -i nullsrc ... -f null -` 试编码判断可用。

### 2.6 SRT 参数（单位：`latency` 等是**微秒**）

两个 SRT 端点都在本机回环上，中间的 WAN 段由 HdrCast 自己的 UDP 链路搬运（§8.6）：

| 端点 | URL |
|---|---|
| 主播端：每位观众的 remux 子进程 | `srt://127.0.0.1:<Y>?mode=listener&passphrase=<组派生>&pbkeylen=32&pkt_size=1316&transtype=live&latency=<观众要求>&maxbw=0&inputbw=<bitrate/8>&oheadbw=<观众要求>&sndbuf=..&ffs=..&listen_timeout=-1` |
| 观众端：mpv | `srt://127.0.0.1:<L>?mode=caller&passphrase=<组派生>&pbkeylen=32&pkt_size=1316&transtype=live&latency=<地区预设>&rcvbuf=..&ffs=..&recv_buffer_size=8000000&connect_timeout=5000` |

- 缓冲大小公式：`packets = latency_s × bitrate_bps / (1316×8)`；`ffs ≥ packets`，`sndbuf/rcvbuf ≥ packets × 1472`（`SrtBufferSizing`）。
- listener 只能绑具体地址：libsrt 绑 `[::]` 必须显式设 SRTO_IPV6ONLY，而 ffmpeg 没有这个选项（实测 "Operation not supported"）。这里只绑 127.0.0.1，不受影响。
- 不用 ffmpeg 的 `mode=rendezvous`（它会 bind 到远端地址）；打洞由 HdrCast 自己做。

### 2.7 GUI（WinForms，代码生成 UI，不用设计器 resx）

区块：
1. 显示器：下拉列表（来自 `list-outputs`），显示 HDR 状态；刷新按钮。模式：自动 / 强制 SDR。
2. 画质：编码器（探测结果）、码率 Mbps（数字框 + 预设 20/40/80）、帧率（30/60/120）、输出分辨率（原生 / 2560×1440 / 1920×1080）、质量档（速度优先 / 均衡 / 画质优先 → 映射到各编码器 preset）、GOP 秒（默认 2）、B 帧开关、lookahead 开关。
3. 音频：开关、设备、编码（AAC 192k / Opus 128k）。
4. 组（Share with your group）：组名、Create group… / Paste invite / Copy invite、Max viewers、本机上行（用于"需要多少上行"的提示）。
5. 控制：Start / Stop、Open logs；状态栏：采集 fps / 重复帧 / 编码 fps / 码率 / speed / 丢帧 / 已运行时间 / 观众（名字、IPv6/IPv4、RTT、所需上行）。
6. ffmpeg 路径：自动查找（exe 同目录 `ffmpeg.exe`、`tools\ffmpeg.exe`、PATH），可浏览；缺失时提示下载 gyan.dev **full** 版。
- 配置文件：`%APPDATA%\HdrCast\sender.json`（`DataContractJsonSerializer`）。
- 崩溃保护：ffmpeg 或采集进程意外退出 → 停止另一方，3 s 后自动重启；新编码器的 stdout 接进同一个分发器，观众不用重新加入。
- 进程树：两个 GUI 都创建 **Job Object（KILL_ON_JOB_CLOSE）** 并把每个子进程加入，任务管理器强杀 GUI 或 GUI 崩溃时子进程一起死。采集引擎另有 `--parent-pid`（用一个 `OpenProcess(SYNCHRONIZE)` 句柄轮询，不是每帧扫全系统进程表）。
- **启动顺序**：采集引擎必须先把**两个**管道都建好，GUI 轮询到管道出现后才启动 ffmpeg（固定 sleep 不够：引擎要先建 D3D11 设备并编译 shader）。音频管道必须在等待视频管道客户端**之前**创建，否则两边互等。
  轮询**只能用 `WaitNamedPipe`**，不能用 `File.Exists` / `GetFileAttributes`：对 `\\.\pipe\<name>` 查属性本身就是一次客户端连接，会占掉唯一的实例，ffmpeg 随后得到 ERROR_PIPE_BUSY（CRT 译成 `Error opening input: Invalid argument`），引擎写端报 `Pipe is broken`，无限重启（v0.2.0 在 Windows 上的实际故障，`NamedPipeProbeTests` 防回归）。
- 音频端点打不开时**不终止会话**：管道照建，写静音，日志说明原因（ffmpeg 已经被告知有音频输入，缺管道会直接退出）。
- 日志与界面里的命令行一律经 `SecretRedactor` 脱敏（passphrase 与邀请码），因为 README 会让用户把日志发出来排障。

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
    "srt://127.0.0.1:<L>?mode=caller&passphrase=<组派生>&pbkeylen=32&latency=<µs>&rcvbuf=<bytes>&ffs=<n>&recv_buffer_size=8000000"
```

- SRT 选项一律放 URL 查询串（`--demuxer-lavf-o` 到不了协议层）。
- `rcvbuf/ffs` **必须**按 码率 × 延迟 计算（否则 > 40 Mbps + 2 s 时 libsrt 会 "No room to store incoming packet" 断线）。码率由用户在 GUI 填（或默认按 100 Mbps × 延迟上限算，宁大勿小）。
- mpv 只在打洞路径建好后启动；之后路径断了由 ViewerSession 在同一个本地端口上重新加入，mpv 每 3 s 自己重连。
- HDR 直通只在 `--wid` 嵌入或独立窗口下都以「窗口化 HDR」工作，静态元数据只在全屏时下发 → 提供全屏按钮（`--fs`，不用 `--d3d11-exclusive-fs`）。
- **不嵌入 libmpv**，独立启动 mpv.exe；用 JSON IPC 命名管道读 `demuxer-cache-duration`、`frame-drop-count`、`video-params`、`video-target-params`、`hwdec-current`、`estimated-vf-fps`、`paused-for-cache` 显示状态。
- 兜底：「用 ffmpeg 拉流」模式 = `ffmpeg -i srt://127.0.0.1:<L>?... -c copy -f mpegts - | mpv -`（万一 mpv 构建没带 libsrt）。
- mpv 路径：同目录 `mpv.exe` / `tools\mpv.exe` / PATH；缺失时提示下载 shinchiro 或 zhongfly 构建（均带 libsrt）。
- 配置：`%APPDATA%\HdrCast\receiver.json`。

## 4. 信令服务器（signal/）

- `signal/hdrcast_signal.py`：只用 Python 标准库（≥ 3.6），实现 §8.5 的 HTTP API 和 §8.7 的 UDP 回显；只把组注册表写盘。
- `signal/hdrcast-signal.service`：systemd 加固运行（DynamicUser、ProtectSystem=strict、MemoryMax=64M）；`signal/deploy.sh` 经 ssh 安装 / 升级 / 卸载。
- 部署在腾讯云 `101.43.23.94`：TCP 18650（API）、UDP 18651（IPv4 回显，可选），安全组放行。它只转几百字节的签名元数据，从不转视频。

## 5. 网络与地区预设

| 观众 | RTT 估计 | SRT latency | oheadbw |
|---|---|---|---|
| 日本 | 10~30 ms | 1500 ms | 25 |
| 澳大利亚 | 120~160 ms | 2000 ms | 25 |
| 中国（跨境丢包） | 60~120 ms + 丢包 | 3500 ms | 50 |

观众的地区预设随加入请求发给主播，主播给他的 remux 子进程就用这个 latency / oheadbw。
总延迟 ≈ 观众的 SRT latency + mpv `cache-pause-wait`（1.5 s）+ 编码（~0.4 s）。

## 6. 工程与构建

- `sender/`、`receiver/`：net48 WinForms，`Microsoft.NETFramework.ReferenceAssemblies`，`ILRepack.Lib.MSBuild.Task` 把 SharpDX(.DXGI/.Direct3D11/.D3DCompiler 4.2.0)、NAudio.Core/NAudio.Wasapi 2.2.1 合并成单 exe；合并失败则退回「exe + DLL 目录」。
- `shared/`：平台无关逻辑（命令行构造、SRT URL / 缓冲计算、`-progress` 与 mpv IPC 解析、设置模型、编码器档位表），以源码链接方式编进两个 exe 与 `tests/`（net10 + xunit，本机运行）。
- `build.sh`：`dotnet build -c Release` 两个工程 → `dotnet test` + 信令服务器的 Python 测试 → `dist/HdrCast-<ver>/`（exe、`signal/`、`scripts/`、`docs/`、README）。
- 配置存 `%APPDATA%\HdrCast\`；日志存 `%APPDATA%\HdrCast\logs\`。

## 7. 验证计划

Linux 本机：`dotnet build` 0 警告；`dotnet test` 覆盖命令行构造 / URL / 缓冲计算 / 解析器 / 邀请码与签名（golden 向量）/ 打洞（模拟有状态防火墙）/
全栈端到端（真 Python 信令服务器 + 真 libx265 HDR10 编码 + P2pHost + 两个 ViewerSession + ffprobe）；`python3 -m unittest discover -s signal/tests`。
Windows（只能由用户做）：`list-outputs` 正确识别 HDR；`capture` 单独跑 10 s 落盘为 `.p010` 用 ffmpeg 转 PNG 检查亮度；完整推流；朋友端 mpv HDR 直通 / SDR 色调映射；4K60 下采集 fps 与 GPU 占用。

## 8. 点对点模式协议（v1）

> P2P 模式：主播 PC 自己把流分给每个观众，**不需要中继**。腾讯云上的 `hdrcast-signal` 只做牵线（谁在播、交换候选地址），
> 所有消息都用组密钥签名，服务器既看不到密钥也伪造不了地址；视频从不经过它。本节是 C# 客户端与 `signal/hdrcast_signal.py`
> 共同遵守的契约，golden 向量在 `tests/vectors/group-v1.json`，两边的测试都读它。

### 8.1 编码约定

- `b64u(x)`：base64url（RFC 4648 §5），**无** `=` 填充。解码时也接受有填充的输入。
- 所有 JSON 用 UTF-8，不带 BOM；签名永远针对**原样字节**（`p` 字段里的 base64url 串），不做 JSON 规范化。
- 时间戳：`t`、`now` 等是 Unix 毫秒（整数）；`age_ms` 是服务器算的“存了多久”。
- id：`gid`、`host`、`viewer`、`sid` 都是 16 个随机字节的 `b64u`，正好 22 字符，匹配 `^[A-Za-z0-9_-]{22}$`。

### 8.2 邀请码

```
hdrcast:<B>.<C>
B = b64u(UTF-8 JSON)        C = b64u(SHA-256(JSON 字节)) 的前 8 个字符
JSON = {"v":1,"s":"http://101.43.23.94:18650","g":"<gid>","k":"<b64u(32 字节组密钥 K)>","n":"<组名>"}
```

- `v` 必须是 1（更大 → “邀请码来自更新的版本”）；`s` 是信令服务的 base URL（`http://` 或 `https://`，无结尾 `/`）；
  `k` 解码后必须正好 32 字节；`n` 1–40 个字符。未知字段忽略。
- 从聊天文本里提取：先删掉全文的零宽字符（U+200B/200C/200D/2060/FEFF，有的聊天软件会插进 `hdrcast:` 里防自动链接），
  再做 NFKC（全角 `ｈｄｒｃａｓｔ：` 也认），不区分大小写找 `hdrcast:`；之后只收 base64url 字母和 `.`，
  跳过空白（空格、制表、CR、LF、NBSP），遇到其他字符停止。payload 里不会有 `.`，
  所以**第一个** `.` 之前是 B，之后的 8 个字符是 C，再往后属于聊天原文、忽略。
  没有 `.` 或校验不足 8 位 → “不完整”；校验不符 → “已损坏”。文本里有多个时取第一个校验通过的。
- 撤销 = 新建一个组重新发码（旧组的 K 作废）。

### 8.3 密钥派生

`sub(label) = HMAC-SHA256(K, UTF-8("hdrcast/v1/" + label))`，32 字节。

| 用途 | 派生 | 说明 |
|---|---|---|
| 请求鉴权 | `serverKey = sub("server")` | 建组时交给信令服务器 |
| 消息签名 | `msgKey = sub("msg")` | 服务器不知道，伪造不了在播信息 / 加入 / 应答 |
| SRT 加密 | `passphrase = b64u(sub("srt"))` 的前 32 个字符，`pbkeylen=32` | 端到端 AES-256 |
| 打洞 | `punchKey(sid) = sub("punch/" + sid)` | 每个会话一把 |

### 8.4 签名信封

```
{"p":"<b64u(payload JSON)>","m":"<b64u(HMAC-SHA256(msgKey, kind + "\n" + gid + "\n" + key + "\n" + p))>"}
```

| kind | key | payload |
|---|---|---|
| `live` | host id | `{"v":1,"t":ms,"host":id,"name":"小明","since":ms,"cands":["[2400:…]:51234","192.168.1.5:51235"],"codec":"hevc","mbps":40,"max":4,"viewers":1}` |
| `join` | host id | `{"v":1,"t":ms,"sid":id,"viewer":id,"name":"小红","cands":[…],"latency":3500,"overhead":50}` |
| `answer` | sid | `{"v":1,"t":ms,"sid":id,"ok":true,"reason":"","cands":[…]}`（`reason`：`full` / `stopped` / `busy`） |

- `live` 可选字段 `"open4":true|false`（0.2.4 起）：主播的 IPv4 端口收不收得下任意地址端口来的包（路由器转发 + 防火墙规则，§8.7）；
  还不知道时不写，旧主播也不写。观众据此说明 IPv4 失败是哪一边的问题。
- 收方先验 `m`（常量时间比较），再解 `p`；payload 里的 `host` / `sid` 必须等于信封所在的 key。
- `t` 与本机时间相差超过 10 分钟的 `join` 丢弃；主播缓存已处理的 `sid` 10 分钟防重放。
- `cands` 每项是 `Endpoint` 能解析的 `host:port`（IPv6 带方括号），最多 8 项。

### 8.5 信令 HTTP API（`<s>/v1`）

鉴权头（`/v1/g/...` 全部需要）：

```
X-Hdrcast-Auth: <ts>.<nonce>.<mac>
ts    = Unix 毫秒              nonce = b64u(8 个随机字节)（11 字符）
mac   = b64u(HMAC-SHA256(serverKey, METHOD + "\n" + PATH_AND_QUERY + "\n" + ts + "\n" + nonce + "\n" + b64u(SHA-256(body))))
```

服务器要求 `|now - ts| ≤ 300 000`，nonce 10 分钟内不得重复（重复 → 401），mac 不符 → 401，组不存在 → 404。
`PATH_AND_QUERY` 是请求行里的原样目标（例如 `/v1/g/<gid>/live/<host>/joins?after=3&wait=20`）。空 body 的哈希是 SHA-256 of 空串。

| 方法 & 路径 | body | 成功 | 说明 |
|---|---|---|---|
| `GET /v1/health` | – | 200 `{"ok":true,"v":1}` | 不需要鉴权 |
| `PUT /v1/g/{gid}` | `{"serverKey":"<b64u 32 字节>"}` | 201 新建 / 200 已存在且相同 | 先到先得；鉴权头用 body 里的 serverKey 计算（证明持有）；不同 key → 409 |
| `PUT /v1/g/{gid}/live/{host}` | 信封（live） | 204 | 心跳，每 10 s 一次，30 s 未续期即过期 |
| `DELETE /v1/g/{gid}/live/{host}` | – | 204 | 下播；同时清空该主播的加入信箱 |
| `GET /v1/g/{gid}/live` | – | 200 `{"now":ms,"hosts":[{"host":id,"p":…,"m":…,"age_ms":n}]}` | 只含未过期的 |
| `POST /v1/g/{gid}/live/{host}/join` | 信封（join） | 202 `{"seq":n}` | 主播不在线 → 404；信箱满（32）→ 429 |
| `GET /v1/g/{gid}/live/{host}/joins?after={seq}&wait={s}` | – | 200 `{"joins":[{"seq":n,"p":…,"m":…}],"next":n}` | long-poll，`wait` ≤ 25；超时返回空数组；`next` = 最大 seq（没有则回显 `after`） |
| `PUT /v1/g/{gid}/answer/{sid}` | 信封（answer） | 204 | 60 s 过期 |
| `GET /v1/g/{gid}/answer/{sid}?wait={s}` | – | 200 信封 / 204 超时 | long-poll，`wait` ≤ 25 |

- 错误统一 `{"error":"<短句>"}`：400 格式错、401 鉴权失败、404、405、409、413 body > 8192 字节、429 限流、503 long-poll 满（64 个）、507 组数达上限（500）。
- 限流：按来源 IP 令牌桶（每秒 10 个、突发 40）。加入信箱每个主播最多 32 条、60 s 过期；每组最多 32 个在播主播。
- 持久化：只有组注册表（gid → serverKey）写盘（原子替换）；在播 / 信箱 / 应答只在内存。日志不打印 key、mac、body。
- 服务实现：`signal/hdrcast_signal.py`（仅标准库，Python ≥ 3.6，ThreadingMixIn），systemd 单元 `signal/hdrcast-signal.service`，
  部署 `signal/deploy.sh`。

### 8.6 打洞控制包（UDP，58 字节）

```
偏移 长度
 0   8   magic  "HDRCP2P1"（ASCII）
 8   1   type   1 PUNCH  2 PUNCH_ACK  3 KEEPALIVE  4 BYE
 9   1   role   1 主播  2 观众
10  16   sid    （原始 16 字节）
26   8   ts     发送方单调时钟微秒，大端 uint64
34   8   echo   PUNCH_ACK 回显所答 PUNCH 的 ts；其他为 0
42  16   mac    HMAC-SHA256(punchKey(sid), 字节 0..41) 的前 16 字节
```

- 长度 ≠ 58、magic 不符、type/role 未知、sid 未知、mac 不符、role 与自己相同 → 不是控制包。
- 双方拿到对方候选后，每 100 ms 向对方**每个**候选发 PUNCH，最多 15 s；收到有效 PUNCH 立刻回 PUNCH_ACK（发回来源地址）。
  观众以第一个 PUNCH_ACK 的来源为路径；200 ms 内 IPv6 也确认则优先 IPv6。主播把数据发往观众**最近一次数据的来源**（限已确认地址）。
- **学来的候选**（0.2.4 起）：有效 PUNCH / PUNCH_ACK 的来源不在对方候选里（对称型 NAT 给这个目的地新开了端口），就把来源加进
  打洞目标（每会话最多 16 个候选）。只要一方收得下未请求的包（§8.7 路由器转发 + 防火墙规则），两个方向都能连：主播从观众的 PUNCH
  学到观众的真实端口，观众从主播的 PUNCH 学到主播的真实端口，往那里打洞，对方 NAT 为我们开的口子放行，于是 PUNCH_ACK 能回来。
- 路径确认后停止 PUNCH；2 s 内没往对方发过任何包就发 KEEPALIVE；15 s 收不到对方任何包判定断开；主动结束发 3 次 BYE。
- 不是控制包、且来源是本会话已确认地址的数据报按 SRT 数据转发；其他来源一律丢弃。

### 8.7 候选地址

- IPv6：UDP `connect` 到 `[2001:4860:4860::8888]:53`（不发包）得到出口源地址；Windows 上再换成同一网卡上的**非临时**全球地址
  （SuffixOrigin ≠ Random，DAD Preferred，排除 link-local / ULA / Teredo 2001::/32 / 6to4 2002::/16）。WAN socket **绑在这个具体地址上**，
  否则 Windows 出站会用临时地址，与公布的候选对不上。
- IPv4：绑 `0.0.0.0`；候选为出口局域网地址（connect `8.8.8.8:53` 取源地址，只对同一局域网有用），以及（可选）经信令服务器 UDP 回显得到的公网映射。
- UDP 回显（可选，默认端口 18651）：请求 `"HDRCECHO"` + 16 字节 nonce；回复 `"HDRCECHO"` + nonce + 族（4 或 6，1 字节）+ 端口（大端 2 字节）+ 地址（4 或 16 字节）。
  没回复时写一次日志并重试（主播每分钟一次，直到拿到为止）。观众**每次加入前**都重做一次（0.2.4 起）：NAT 映射可能在两次加入
  之间过期换端口。拿不到时观众的失败提示说 "the host offered no public IPv4 address"，而不是笼统的 "IPv6 only"。
- **路由器端口转发**（UPnP IGD v1/v2，`RouterPortMapping`，0.2.4 起）：主播开播、观众观看期间，从局域网地址向 `239.255.255.250:1900`
  发 SSDP 搜索（InternetGatewayDevice:1/2、WANIPConnection:1/2、WANPPPConnection:1）→ 读 LOCATION 的设备描述（不处理 DTD）找
  WANIPConnection / WANPPPConnection 的 controlURL → `GetExternalIPAddress` → `AddPortMapping`（UDP，外部端口 = IPv4 socket 端口，
  内部客户端 = 连向路由器时的本机地址，描述 `HdrCast`，租期 3600 s）。外部端口被占（718）换随机端口再试三次；只支持永久租期（725）
  就用 0 并确保停止时删除。每 20 min 续租，失败则重新搜索；找不到路由器 / 被拒绝时 60 s 起指数退避重试（最长 15 min），同样的结果只记一次日志。
  停止时 `DeletePortMapping`。成功且路由器外部地址是公网 → `外部地址:外部端口` 作为候选；外部地址是私网 / CGNAT（双重 NAT）→ 不公布，
  状态说明转发帮不上。本机自己就有公网 IPv4 → 不找路由器，直接算"开着"。
- **Windows 防火墙**：没有规则时只放行"本程序发过包的地址端口"的回包，对称型 NAT 的来包会被丢掉。界面用 COM `HNetCfg.FwPolicy2`
  只读检查（当前网络配置文件、FirewallEnabled、BlockAllInboundTraffic、指向本 exe 的入站 UDP 规则，阻止规则优先）；
  **Allow…** 以管理员身份运行 `<exe> firewall-allow`（UAC），加入站 UDP 允许规则（所有配置文件，任意端口 / 地址），停用本 exe 的入站阻止规则。
- 主播"开着"（`open4`）= 路由器转发成功（或本机有公网 IPv4）且防火墙放行。

### 8.8 主播端分发

编码 ffmpeg 把 MPEG-TS 写到 **stdout**（`-progress pipe:2`，进度行与日志共用 stderr，按 `key=value` 形状区分），经 `SwitchableStream`
（编码器重启时接上新的 stdout）进 `TsFanOut`：按观众各一条有界队列（码率 × 0.5 s，至少 512 KB；满了清空，解码器在下一个关键帧恢复）
喂给每个观众自己的 remux 子进程：

```
ffmpeg -hide_banner -loglevel warning -probesize 65536 -analyzeduration 500000 -f mpegts -i pipe:0
       -map 0 -c copy -f mpegts -mpegts_flags +resend_headers -muxdelay 0 -muxpreload 0
       "srt://127.0.0.1:<Y>?mode=listener&passphrase=…&pbkeylen=32&latency=<观众要求>&maxbw=0&inputbw=…&oheadbw=<观众要求>&…"
```

`HostLink` 为每个会话开一个 `127.0.0.1` 本地 socket 连到 `<Y>`，在 WAN 与子进程之间原样搬运数据报。
观众端 `ViewerLink` 在 `127.0.0.1:<L>` 上等 mpv（`srt://127.0.0.1:<L>`，caller），搬到主播的已确认地址。
本机已实测：SRT（含 AES）穿过这种透明转发器正常；一路编码分给两个 remux 子进程，中途加入的观众也能读到 HEVC Main10 / PQ。
