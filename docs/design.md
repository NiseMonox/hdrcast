# HdrCast 设计定稿（v0.3：Rust 重写，点对点）

> 本文是实装的唯一规格来源。所有参数都有来源：`docs/research/` 下的调研报告（读源码 + 本机实测）。
> 与本文冲突的实现以本文为准；本文有错就改本文。

## 0. 需求回顾

- 用户：本人（东京，Windows 11 + RTX 4090 + AW3225QF 4K HDR）和朋友们（中国 / 日本 / 澳大利亚，可能同时观看）。
- 码率可自定义（10～100 Mbps）；画面不糊、不卡；延迟可以大（1～5 s，"推流式"而非远程桌面式）。
- HDR10 全链路；带桌面音频。
- v0.2（2026-09）：**只有点对点模式**，不依赖用户家的 LXC：谁都能开播，同组朋友直接从主播电脑收流（§1、§8）。
  v0.1 的 MediaMTX 中继模式和手动直连模式已删除（AV1 只能走直连，所以一起去掉，只剩 HEVC）。
  腾讯云服务器只做信令（几百字节的签名元数据），**不转视频**；连不上时不中转，只提示原因。
- 很多人没有公网 IPv4，但大多有 IPv6：点对点优先走 IPv6 打洞，IPv4 打洞只是额外尝试。配对用邀请码，不手输地址。
- v0.3（2026-10）：**用 Rust 重写**。0.2 的 `HdrCastSender.exe` / `HdrCastReceiver.exe`（C#、WinForms）合并成一个应用 `HdrCast`
  （Tauri 2，界面见 §2.7）；支持 **macOS**（观看和开播，§10）；新增**远程控制**（§9）。架构不变：ffmpeg 编码、mpv 播放仍是外部进程。
  **线上协议仍是 v1**（§8）：0.3 与 0.2.4 互相能看，信令服务器没有改动；设置文件也沿用（`group.json`、`sender.json`、`receiver.json`）。
  下文凡是写 Windows 机制的地方（DXGI、WASAPI、命名管道、防火墙），macOS 的对应做法都在 §10。
- v0.3.1（2026-10）：接近 Parsec 的用法。**被控端不用手动开播**：应用开着就以"在线"出现在组里，别人点"连接"并被放行后才开始采集，
  最后一个人离开后自动停（§8.10）。**权限按电脑授予**：每台电脑有自己的设备密钥，主播给每台电脑定"能不能直接看、能不能控制"，
  例如自己的笔记本连上即可控制、朋友只能看（§8.10、§9）。协议仍是 v1，新字段都是可选的。

## 1. 架构

```
主播 PC                                                                     观众 PC
 采集引擎 → ffmpeg 编码 → MPEG-TS(stdout) → SwitchableStream → TsFanOut
    → 每位观众一个 remux ffmpeg（-c copy，SRT listener 127.0.0.1:Y，观众的 latency/oheadbw）
    → HostLink（WAN socket：[稳定 IPv6]:P、0.0.0.0:P4） ⇄ 打洞后的 UDP 路径 ⇄ ViewerLink ⇄ mpv（srt://127.0.0.1:L）
        │                                                                    │
        └──── 签名的 live / join / answer ──── 腾讯云 hdrcast-signal ────────┘
```

主播端的采集部分（子进程 A：`HdrCast` 自己的 `capture` 模式，DXGI 桌面复制 → D3D11 shader → P010/NV12 →
命名管道；WASAPI loopback → 音频管道）和编码 ffmpeg（子进程 B，只做编码 + 封装）见 §2；观众端由同一个应用启动 mpv，见 §3。

- 为什么这样分：SRT 本身端到端不变（AES、重传、按观众的 latency），HdrCast 只在两端各加一个透明 UDP 转发器负责打洞；
  ffmpeg 的 SRT listener 只接受一个连接，所以每位观众一个 remux 子进程，一路编码喂给所有子进程。
- 编码器 stdout 直接进分发器（`-progress` 改到 stderr，按"key=value"行过滤）：比让 ffmpeg 往 Windows 命名管道写输出更稳
  （后者要 `-y` 且依赖 CreateFile 对管道的处理，无法在 Linux 上验证）。编码器自动重启时新 stdout 接进同一个 SwitchableStream，观众不掉线。
- 实现：`crates/core`（邀请码、密钥、签名消息、group.json，以及地址 / SRT URL / 进程 / 防火墙这些基础件）、
  `crates/p2p`（打洞、链路、分发、信令客户端、P2pHost、ViewerSession）、`signal/`（信令服务器，Python，0.3 未改）。协议与格式见 §8。
  0.2 的 C# 源码（`shared/`、`sender/`、`receiver/`、`tests/`）留在仓库里当对照：Rust 的测试拿它的真实输出逐字节比过。

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

### 2.1 一个可执行文件的几种运行模式

| 命令行 | 作用 |
|---|---|
| `HdrCast` | 应用本身（§2.7） |
| `HdrCast --background` | 应用，但不显示窗口，只在通知区域（托盘）运行；"登录时自动启动"用的就是它（§2.7） |
| `HdrCast hdrcast:…` | 应用，启动后提示加入这条邀请 |
| `HdrCast list-outputs [--json]` | 枚举显示器：索引、设备名（Windows `\\.\DISPLAYn`，macOS 是 `CGDirectDisplayID`）、友好名、分辨率、刷新率、HDR 是否开启（Windows：`DXGI_OUTPUT_DESC1.ColorSpace == RGB_FULL_G2084_NONE_P2020`）、MaxLuminance |
| `HdrCast audio-devices` | 列出音频输出端点（Windows） |
| `HdrCast capture <参数>` | 采集引擎（无窗口，stdout 先写一行帧格式、再每秒写 JSON 统计，stderr 写日志）。应用把自己当子进程这样启动 |
| `HdrCast firewall-allow` | 加 Windows 防火墙入站规则后退出；由界面的 **放行…** 以管理员身份启动（§8.7） |

同一份配置（同一个 `%APPDATA%\HdrCast`，或同一个 `HDRCAST_DATA_ROOT`）同时只运行一个应用：已经在运行时再启动（前三种），新进程把参数交给正在运行的那个后自己退出（§2.7）。

`main` 先把参数交给 `hdrcast_capture::cli_main`，是引擎模式就直接跑完退出，不创建窗口。Windows 的发行版是窗口子系统程序：
被应用带管道启动时 stdout / stderr 照常工作；在终端里手动跑时会附着到父控制台，但交互式终端不等它结束（用 `| more` 或写进 .cmd）。

采集引擎参数：

```
capture --output <idx>           DXGI 输出索引（list-outputs 给出的 index，按 adapter 顺序 EnumAdapters1 → EnumOutputs 全局编号）
        --fps <n>                目标恒定帧率，默认 60
        --format auto|p010|nv12  auto = 桌面是 HDR 就 p010，否则 nv12（选了哪个见 stdout 第一行，§2.2）
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
- **格式声明**：打开显示器之后、创建管道之前，向 stdout 写一行 `{"layout":{"format":"p010","width":3840,"height":2160}}`（dump 模式也写）。
  `--format auto` 时由后端决定（macOS 系统不给 HDR 采集时退回 nv12），所以管道里是什么**只以这一行为准**：应用读到它、并且两个管道都在等连接，
  才按它启动 ffmpeg（像素格式、尺寸、HDR 编码与否都取自这一行，`FfmpegPlan::for_frames`）。显式 `--format p010|nv12` 而后端给不了时，
  引擎报错退出（退出码 3），不写这一行，也不建管道——不会把另一种格式写进别人按指定格式读的管道。
  应用对 HDR 模式的显示器传 `auto`、SDR 传 `nv12`；面板显示实际发送的格式，HDR 屏退成 SDR 时附一句原因见日志。
- **恒定帧率**：用 `QueryPerformanceCounter` 做定时器；每个 tick 写一帧。节拍器与等待函数必须用**同一个时基**（`Stopwatch.GetTimestamp()` 的绝对值），混用「从 0 开始的 ElapsedTicks」会让等待永远立即返回，退化成按桌面刷新率忙转。`AcquireNextFrame(timeout=0 或很短)` 拿到新帧就转换，没有就重复上一帧（复制已转换好的缓冲，不重跑 shader）。这样 ffmpeg 侧用理想时间戳即可，不用 wallclock。
- 统计：每秒向 stdout 写一行 JSON：`{"t":秒,"captured":n,"duplicated":n,"dropped":n,"fps":x,"gpu_ms":x,"audio_underrun":n,"pipe_queue_ms":x}`。GUI 逐行解析显示
  （格式声明那一行没有顶层数字，不会被当成统计）。

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

- 直接用 WASAPI（`windows` crate；0.2 用的是 NAudio）：loopback 抓默认渲染设备，每 10 ms 取一次；同时开一路渲染流播放静音，保证渲染引擎不进入空闲（否则 loopback 停止吐数据）。
- 输出格式统一转换为 **f32le 48000 Hz 2ch**（设备 mix format 不是 48 kHz 时做跨缓冲连续的线性重采样）。
- **与视频锁同一时钟**：以 QPC 为准计算「应已输出的采样数」= elapsed × 48000；实际写入落后超过 150 ms 则补零，超前超过 150 ms 则丢弃多余采样。这样 ffmpeg 两路都用理想时间戳也不会漂。
  容差必须远大于 loopback 的交付粒度：0.2 的 NAudio 轮询取数，音频一批约 62 ms、还略有延迟。v0.2.1 用 20 ms 时，写管道的线程在两批之间就补零，
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

### 2.7 应用（Tauri 2）

一个窗口，Rust 后端（`app/`）+ 网页前端（`ui/`：原生 ES 模块 + vendored preact/htm，不需要 Node 构建，直接嵌进可执行文件）。
外观是系统风格的侧边栏 + 分组卡片，**黑白灰**为主，颜色只留给"直播中"的红点和报错文字；跟随系统浅色 / 深色；
Windows 11 用 Mica、macOS 用侧边栏毛玻璃作窗口背景（没有时退回不透明）；Windows 上自绘标题栏按钮，macOS 用系统的红黄绿。
界面文字有英 / 中 / 日三套（`ui/i18n.js`），默认跟随系统；引擎给出的状态句子和日志保持英文（它们是排障时互相发的东西）。

页面：
1. **开播**：顶部是状态卡片（"可被连接" / "未开放连接"，以及这台电脑的设备码，§8.10）。有人请求观看时出现"请求观看"列表
   （设备码或"旧版本"、剩余秒数，**拒绝** / **总是允许** / **允许**；同时弹出提示，窗口不在前台时任务栏闪烁，收在托盘里或最小化时窗口自己出来，见下）。
   有人在看或开播时状态卡片换成面板：码率 / 编码 fps / 采集 fps / 丢帧；开播时标题"直播中"、按钮 **停止**，按需时标题"按需分享中"、按钮 **全部断开**；
   下面是正在观看的人（设备码、IPv6 / IPv4、ping、延迟、远程控制状态与 允许 / 拒绝 / 收回、**断开**）。
   设置部分：显示器卡片（带 HDR 徽标、分辨率、刷新率、峰值亮度）；画面（码率滑块、帧率、分辨率、HDR 自动 / 始终 SDR、编码器 + **检测**，
   更多选项里是编码预设、关键帧间隔、B 帧、Lookahead、指针）；声音（开关、设备、AAC / Opus、排除程序）；
   **谁可以连接**（"开放连接"开关；"组里其他人"的 观看 / 控制 规则；每台电脑一行规则，可移除；**添加电脑…** 从在线或最近请求过的电脑里选：
   "我的电脑" = 直接允许 + 自动获得控制，"信任的朋友" = 直接允许 + 控制要询问）；观众（同时观看人数、上行带宽与估算、防火墙 **放行…**）。
   底部固定一条：将要发送的格式 + **开始直播**。
2. **观看**：组里在线的电脑（每 10 s 向信令服务器要一次；不要更快，服务器按 IP 限流每秒 10 次）：开播中的标 LIVE、按钮 **观看**，
   按需的标"在线"、按钮 **连接**，都带设备码；延迟预设（快速 300 ms / 日本 / 澳大利亚 / 中国 / 自定义）；输出（自动 / HDR / SDR）。
   观看中顶部是面板：缓冲、帧率、码率、丢帧、是否 HDR 直通，**全屏** / **停止**，以及远程控制一行（**请求控制** / **接管控制** / **只看不控**）；
   等主播回答时显示"等待同意"，被拒绝时显示原因和 **关闭**。
3. **设置**：语言、外观、登录时自动启动（见下）；群组（这台电脑的设备码、我的名字、邀请码复制 / 粘贴 / 新建）；ffmpeg / mpv 路径；播放器细项（硬解、缓冲、接收缓冲按多大码率算、峰值亮度、全屏用哪块屏、追赶、ffmpeg 管道兜底）；防火墙；关于。
4. **日志**：全部日志行（已脱敏），可筛选、复制、打开文件夹。

前后端之间：前端调命令（`app/src/commands.rs`），后端每 400 ms 推一份完整的状态快照（`status` 事件）和成批的日志行（`log` 事件），界面只按快照重画。
第一次运行（没有组）是欢迎页：**我有邀请码** / **新建群组**。Windows 上窗口回到前台时看一眼剪贴板（序号变了才读），有邀请就问要不要加入。

- 配置文件：`%APPDATA%\HdrCast\` 下的 `sender.json`、`receiver.json`（字段名和整数枚举与 0.2 的 DataContract 文件相同，旧文件直接可读）、
  `app.json`（界面语言 / 外观）、`access.json`（开放连接、组里其他人和每台电脑的 观看 / 控制 规则，§8.10）、
  `device.json`（这台电脑的设备私钥，§8.10，与 `group.json` 一样要保密）、`group.json`；`instance.lock` 和一闪而过的 `open-request.json` 见下面的"只运行一个"。
  环境变量 `HDRCAST_DATA_ROOT` 可以把整个目录换到别处（并行跑第二份配置用）。
- 应用一启动（有组、开放连接时）就在组里在线；这时关窗口只是把它收进通知区域（见本节末尾）。
- 崩溃保护：ffmpeg 或采集进程意外退出 → 停止另一方，3 s 后自动重启；新编码器的 stdout 接进同一个分发器，观众不用重新加入。
- 进程树：应用创建 **Job Object（KILL_ON_JOB_CLOSE）** 并把每个子进程加入，任务管理器强杀应用或应用崩溃时子进程一起死（macOS：退出时逐个杀进程组）。采集引擎另有 `--parent-pid`（用一个 `OpenProcess(SYNCHRONIZE)` 句柄轮询，不是每帧扫全系统进程表）。
- **启动顺序**：采集引擎必须先把**两个**管道都建好，GUI 轮询到管道出现后才启动 ffmpeg（固定 sleep 不够：引擎要先建 D3D11 设备并编译 shader）。音频管道必须在等待视频管道客户端**之前**创建，否则两边互等。
  轮询**只能用 `WaitNamedPipe`**，不能用 `File.Exists` / `GetFileAttributes`：对 `\\.\pipe\<name>` 查属性本身就是一次客户端连接，会占掉唯一的实例，ffmpeg 随后得到 ERROR_PIPE_BUSY（CRT 译成 `Error opening input: Invalid argument`），引擎写端报 `Pipe is broken`，无限重启（v0.2.0 在 Windows 上的实际故障，`NamedPipeProbeTests` 防回归）。
- 音频端点打不开时**不终止会话**：管道照建，写静音，日志说明原因（ffmpeg 已经被告知有音频输入，缺管道会直接退出）。
- 日志与界面里的命令行一律经 `SecretRedactor` 脱敏（passphrase 与邀请码），因为 README 会让用户把日志发出来排障。

**通知区域、登录时启动、只运行一个**（像 Parsec 那样不用管的主播；`app/src/tray.rs`、`autostart.rs`、`instance.rs`）：

- **托盘图标**：应用运行期间一直在通知区域（macOS 菜单栏）。Windows 上左键单击 = 打开窗口，右键菜单 **打开 HdrCast** / **退出 HdrCast**；
  macOS 上单击出同一个菜单，点程序坞图标（`RunEvent::Reopen`）也打开窗口。菜单和提示文字由页面按界面语言发给后端（`tray_texts`，字符串在 `ui/i18n.js`），
  页面发来之前是英文。提示文字跟着状态走："HdrCast" / "HdrCast – 开放连接中" / "HdrCast – 正在分享屏幕"；正在采集屏幕时图标右下角加上
  直播红点（`--live` 的 #ff3b30，外面一圈透明，深浅任务栏上都看得清）：窗口不在眼前时，有人在看这台电脑也一眼可知。
- **关窗口**：开放连接打开（并且有组）时，`CloseRequested` 只把窗口藏起来（日志 `window closed: still available …`），这台电脑照样在线；
  托盘图标没建起来时不藏（藏起来就再也打不开了）。否则照旧：在组里或在观看时先 `watch::stop` + `share::shut_down` 道别再退出，什么都没做时直接退出。
  托盘的 **退出** 走同一条道别的路（图标和窗口先消失）；系统要求的退出（macOS 的 Cmd+Q、程序坞的退出，`ExitRequested` 且不是应用自己发起的）也一样。
- **登录时自动启动**（设置 → 通用）：Windows 写 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的值 `HdrCast` = `"<exe>" --background`（REG_SZ），关掉时删除。
  开关显示的是注册表里的真实状态，不记在 `app.json` 里：值不存在 = 关；指向别的文件夹里的 HdrCast（旧版本）= 关，并写明那个路径，打开就改成这一份；
  任务管理器"启动应用"里关掉的项（`…\Explorer\StartupApproved\Run` 下 12 字节的值，首字节为奇数 = 关）也显示为关并说明，打开时把这个标记删掉。
  每次打开设置页都重新读一次。macOS 见 §10。
- **`--background` 启动**：窗口建成不可见、也不拿焦点（`focused(false)`：否则 WebView2 在创建时把键盘焦点移进网页，一个看不见的窗口成了前台窗口，
  登录后用户打的头几个字就没了）；`app_init` 带 `startHidden: true`（只给第一次），页面画好后不再 `show()`。页面照常加载，从托盘打开时立即可用。
  托盘图标建不起来时照常显示窗口。
- **窗口不在眼前时的请求**：有人请求观看（`share.rs` 的 knock）或请求控制（housekeeping 里发现新的 `requested`）时 `AppState::request_attention` →
  `window::surface`：窗口是隐藏或最小化的就显示出来，用一次短暂的置顶放到所有窗口上面，再让任务栏按钮闪烁。不像托盘的"打开"那样强行切到前台：
  用户正在别的程序里打字时，Windows 不让后台程序拿走键盘，窗口只是出现在上面。页面里照常弹出 **拒绝** / **允许** 提示和"请求观看"列表；50 s 没回答就是 `timeout`。
  选了显示窗口而不是系统通知：托盘气泡（`Shell_NotifyIcon` 的 NIF_INFO）tray-icon 不支持，点了也回不到应用；Windows 的 toast 要给未打包的程序注册 AUMID。
- **只运行一个**：应用启动时对 `<配置目录>/instance.lock` 加独占锁（Windows `LockFileEx`，macOS `flock`；进程怎么结束系统都会放开）。锁被占着 = 这份配置已经在运行：
  带 `--background` 的（登录项）直接退出；其他情况把参数写进 `open-request.json`（先写临时文件再改名），等最多 2 s 看它被取走，然后退出。
  正在运行的应用每 400 ms 的状态循环里取这个文件：显示并聚焦窗口，参数是邀请时弹出加入确认（`join-invite` 事件）。启动时先删掉没人取的旧请求。
  否则两个进程会用同一把设备密钥、同一个 peer id 在组里各宣告一次。不同的 `HDRCAST_DATA_ROOT` 各有各的锁，同机双配置测试不受影响。

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
- mpv 路径：同目录 `mpv.exe` / `tools\mpv.exe` / PATH；缺失时提示下载 shinchiro 或 zhongfly 构建（均带 libsrt）。macOS 见 §10。
- 远程控制开着时，应用另外观察 mpv 的 `mouse-pos` 和 `osd-dimensions`，并让 mpv 把按键转给自己（§9）。
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

Cargo workspace（Rust 1.85+，edition 2021）。全部用普通线程和阻塞 I/O，没有异步运行时（Tauri 自己的除外）。

| crate | 内容 |
|---|---|
| `crates/core`（hdrcast-core） | 地址解析、SRT URL / 缓冲计算、参数引用、日志脱敏、工具查找、路径、`LineProcess` / `ProcessJob`、Windows 防火墙；群组（密钥、邀请码、签名消息、`group.json`） |
| `crates/p2p`（hdrcast-p2p） | UDP 通道、打洞状态机、HostLink / ViewerLink、回显、UPnP、信令客户端、分发、P2pHost / ViewerSession、旁路通道 |
| `crates/capture`（hdrcast-capture） | 采集引擎：平台无关的主循环（节拍、重复帧、统计、音频对齐、管道写线程）+ 后端（`win/`：DXGI / D3D11 / WASAPI；`mac/`：ScreenCaptureKit；其余：桩） |
| `crates/media`（hdrcast-media） | 编码器档位、ffmpeg 命令、`-progress` 解析、编码器探测、`sender.json`；mpv 命令、IPC 消息、日志分类、`receiver.json` |
| `crates/control`（hdrcast-control） | 远程控制：报文、按键名、输入注入（§9） |
| `app`（hdrcast，产物 `HdrCast`） | Tauri 2 应用：窗口、命令、采集 + ffmpeg 的编排（`sender_runner`）、mpv 的编排（`mpv_runner`、`mpv_ipc`）、远程控制两端的粘合 |

- `build.sh`：`cargo test --workspace` + 信令服务器的 Python 测试 → Windows：`cargo build --release -p hdrcast`，装配 `dist/HdrCast-<版本>/`
  （`HdrCast.exe`、`tools/`、`scripts/`、`docs/`、`signal/`、README）并出 zip；macOS：`cargo tauri build` 出 `HdrCast.app` 和 dmg。
- 配置存 `%APPDATA%\HdrCast\`（macOS：`~/Library/Application Support/HdrCast/`）；日志存其下的 `logs/hdrcast-yyyyMMdd.log`。
- 界面不经打包器：`node scripts/ui-preview.mjs` 可以在浏览器里单独打开它（`ui/mock.js` 扮演后端）。
- 调试用的环境变量：`HDRCAST_DATA_ROOT`（另一份配置）、`HDRCAST_NO_ROUTER=1`（不做 UPnP）、`HDRCAST_CONTROL_DRY_RUN=1`（远程控制只写日志）、
  调试构建里的 `HDRCAST_DEVTOOLS_PORT`（把界面暴露给 DevTools 协议，脚本驱动真实窗口用）。

## 7. 验证

- 自动（`cargo test --workspace`，Windows 上跑）：
  - 协议 golden 向量（`tests/vectors/group-v1.json`，Python 测试也读它）；与 **C# 0.2.4 的真实输出**逐字节对照（core：1359 个刁钻输入；
    media：946 个向量，覆盖全部编码器参数表、ffmpeg / mpv 命令行、设置文件）；`group.json` 经真实的 .NET `DataContractJsonSerializer` 往返。
  - 打洞（模拟有状态防火墙、对称型 NAT 两个方向）、假路由器上的 UPnP、真 Python 信令服务器上的全栈（真 libx265 HDR10 编码 + P2pHost + 两个 ViewerSession + ffprobe）。
  - 把发行版 0.2.4 的 exe 加载起来与 Rust 版**互相观看**（两个方向都出 HEVC Main10 / PQ 画面）。
  - 每一组生成的编码器参数都让真 ffmpeg 编一次；mpv 的每个选项名和取值都问过真 mpv。
  - 采集引擎：假后端上的主循环契约；真显示器上的 `list-outputs` / selftest / P010 与 NV12 落盘，与 0.2.4 的 exe 对比。
  - 远程控制：报文、丢包重传、收回时松键、会话中途请求 / 交还控制；真实注入单独有一个默认忽略的测试（把指针"移"到原地、点一下 Shift）。
  - 设备密钥（§8.10）：签名绑定组 / 主播 / 会话 / 时间 / 候选、规范写法、双方算出同一把控制密钥；真信令服务器上的准入：
    晚到的放行（观众先显示等待、再重新打洞）、为别的会话签的 join 被拒且不交给应用、旧观众被拒后停止重试、签名的新观众照样能看不认设备的主播。
- 需要缺的工具时（Python、ffmpeg、mpv、0.2.4 的发行目录）测试会在 stderr 打印 `SKIPPED` 后通过；`HDRCAST_TOOLS` 指向工具目录。
- 真机清单：`docs/windows-checklist.md`（含"单机回环"的做法）、`docs/macos-checklist.md`。**macOS 的全部代码只在 Windows 上做过类型检查**
  （`cargo check --target aarch64-apple-darwin`；整个应用还需要给构建脚本一个假的 C 编译器），没有运行过。

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
- 0.3.1 起的可选字段：`live` 的 `"dk"`、`"od"`，`join` 与 `answer` 的 `"dk"`、`"ek"`、`"ds"`，`answer` 的 `reason` 新值 `denied` / `timeout`，见 §8.10。
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

### 8.9 旁路通道（side channel，0.3.0 起）

远程控制（观众的鼠标 / 键盘，由 `hdrcast-control` 处理）不另开连接，借打洞出来的同一条 UDP 路径传自己的小数据报。
P2P 层**只搬运、不解释**：不看载荷，也不替它加密或认证。线上协议仍是 v1：不用旁路通道时，每一个字节都与 0.2.4 相同。

```
偏移 长度
 0   8        magic    "HDRCSIDE"（ASCII）
 8   16       sid      （原始 16 字节，与控制包相同）
24   1..1200  payload  使用者自己的内容
```

- **接收判定顺序**（§8.6 的补充）：先看是不是控制包，再看是不是 UDP 回显的回复（§8.7），然后看是不是旁路数据报，剩下的才按 SRT 数据处理。
  以 `HDRCSIDE` 开头的数据报**永远不会**当作 SRT 转给 ffmpeg / mpv：格式不对（没有载荷、载荷超过 1200 字节、头不完整）也是直接丢弃。
- **接受条件**：`sid` 是一个在用的会话，**并且**来源地址是该会话已确认的地址（即收到过 punchKey MAC 正确的控制包的地址）。
  两条都满足才把载荷交给使用者注册的处理函数；其他情况一律丢弃。主播按 `sid` 找会话，观众只认当前会话的 `sid`（重新加入后旧 `sid` 作废）。
- **记账与 SRT 数据相同**：收到一个被接受的旁路数据报会刷新 15 s 的静默计时，主播也会把数据路径跟到它的来源地址；
  发出一个旁路数据报算作"发过包"，2 s 内不再补 KEEPALIVE。发送走会话当前选定的路径；会话没连上、或载荷不在 1..1200 字节内 → 不发（返回 false）。
- **大小**：24 + 1200 = 1224 字节，小于 SRT 数据报（1316 字节载荷 + 16 字节头），所以 SRT 走得通的路径它也走得通。不分片、不重传、不保序，
  这些由使用者自己处理（远程控制：指针位置取最新，按键重发到对方确认为止）。
- **安全**：链路只保证"来自本会话已确认过的地址"，与 SRT 数据同等（SRT 自己有 AES，旁路通道没有）。载荷的保密、防伪造、防重放由使用者负责：
  远程控制用 `controlKey(sid) = sub("ctl/" + sid)`（派生方式同 §8.3，每个会话一把）做 AEAD。

**能力标志**。0.2.4 不认识这种数据报，会把它当 SRT 塞给 ffmpeg / mpv，所以只能发给明确说过"我懂"的对端：

| 消息 | 可选字段 | 含义 |
|---|---|---|
| `live` | `"rc":true` | 这个主播会说旁路通道，并且现在提供远程控制。主播每次心跳读一次当前设置；不提供时不写 |
| `join` | `"rc":true` | 这个观众会说旁路通道，并请求控制。不请求时不写 |

- 只在为 true 时写，写在已有字段之后（`live`：`…,"viewers":1[,"open4":…][,"rc":true]`；`join`：`…,"overhead":50[,"rc":true]`）；缺省 = 否。
  不带 `rc` 的消息与 0.2.4 逐字节相同，`tests/vectors/group-v1.json` 的 golden 信封不变。
- **发送规则**：主播只向 `join` 带 `rc` 的会话发；观众只在所看主播的 `live` 记录带 `rc` 时发（以本次加入时读到的记录为准，主播中途打开开关要到下次加入才生效）。
  接收一侧不看标志（没声明的对端本来就不会发）。
- 旧版本不受影响：C# 0.2.4 读消息时按字段名取值，未知字段忽略；信令服务器只存取 `p` / `m` 两个串，不解析载荷。
  已实测（`crates/p2p/tests/csharp_interop_tests.rs`，把发行版里的 `HdrCastReceiver.exe` / `HdrCastSender.exe` 加载进 PowerShell 5.1 直接驱动它们的
  `ViewerSession` / `P2pHost`）：带 `rc` 的 `live` 记录，0.2.4 观众照常加入并出画面；带 `rc` 的 `join`，0.2.4 主播照常接受并出画面；两个方向都没有任何旁路数据报发出。

接口（Rust，`hdrcast-p2p`）：`HostLink::set_side_handler(fn(sid, payload))` / `send_side(sid, payload)`，`ViewerLink::set_side_handler(fn(payload))` / `send_side(payload)`；
`P2pHost` 与 `ViewerSession` 原样透传（处理函数在 `start()` 前后设置都可以），并负责上面的发送规则：`P2pHostOptions.offer_remote_control`（→ `live` 的 `rc`）、
`ViewerInfo.wants_control`（← `join` 的 `rc`）、`ViewerSessionOptions.request_control`（→ `join` 的 `rc`）、`ViewerSession::host_offers_control()`（← `live` 的 `rc`）、
`ViewerSession::sid()`（当前会话 id，用来派生 `controlKey`）。处理函数在网络线程上被调用，要短，不能阻塞。

### 8.10 设备密钥与按需分享（0.3.1 起）

组里每个人都有组密钥 K，凡是用 K 签的东西（peer id、名字）组里任何人都写得出来。所以主播给某一台电脑的特权（不问就能连、连上即可控制）
认的是只有那台电脑才有的密钥，而不是 peer id：

- **设备密钥**：每个安装第一次运行时生成一对 Ed25519 密钥，私钥存在 `device.json`（与 `group.json` 一样要保密；换组不变）。
  公钥的 base64url（43 字符，只接受规范写法：多余的位和 `=` 一律拒绝）记作 `dk`。
- **设备码**：`SHA-256(dk 的 32 字节)` 的前 60 位，按 Crockford base32（没有 I、L、O、U）写成 12 个字符、每 4 个一组，如 `7KQ2-M9XD-4TPA`。
  界面上到处显示它，让人核对"是不是我那台电脑"；规则匹配的是完整的 `dk`，不是设备码。

**消息里的可选字段**（都写在已有字段之后；旧版本按字段名取值、忽略未知字段，同 §8.9 的 `rc`；不带它们的消息与 0.3.0 逐字节相同）：

| 消息 | 字段 | 含义 |
|---|---|---|
| `live` | `"dk":"…"` | 主播的设备公钥（0.3.1+ 总是写）。观众据此只认这台设备签名的 answer |
| `live` | `"od":true` | 按需：没有开播，只是在线，有人被放进来时才开始采集。只在为 true 时写，写在 `dk` 之后 |
| `join` | `"dk"`、`"ek"`、`"ds"` | 观众的设备公钥、本次会话的 X25519 交换公钥、Ed25519 签名（三个都在才算数） |
| `answer` | `"dk"`、`"ek"`、`"ds"` | 主播的设备公钥、交换公钥（拒绝时没有 `ek`）、签名 |
| `answer` | `"reason":"denied"` / `"timeout"` | 主播的规则或主播本人拒绝 / 主播没在时限内回答。新观众收到后停止重试；旧观众当作 busy，过一会儿再试 |

**签名的内容**（UTF-8，各项之间一个 `\n`；`cands` 是候选列表按 §8.7 规整后再写成的 JSON 数组文本，收方用自己读到的列表重算）：

```
join:   "hdrcast/v1/join-device"   gid  host  sid  viewer  t  cands  dk  ek
answer: "hdrcast/v1/answer-device" gid  sid  t  ok(true|false)  reason  cands  观众的ek(没有为空)  dk  主播的ek(没有为空)
```

- 主播验 join（`verify_strict`）：通过 → 这个 join 出自持有该 `dk` 私钥的电脑；带了 `dk` 却验不过 → 直接回答 `denied`，不交给应用（有人在冒充别的电脑）；
  没带 → 旧版本观众，只能用"组里其他人"的规则。
- 观众：所看主播的 `live` 带 `dk` 时，只认用这把密钥签名、并且是回给自己这个 `ek` 的 answer。签名不对的 answer 当作没收到
  （组里任何人都能往 `/answer/{sid}` 写），等真正的那份把它覆盖掉。`live` 不带 `dk`（旧主播）时照旧接受。

**远程控制密钥**：join 和 answer 都带 `ek` 时，双方各算 `shared = X25519(自己的交换私钥, 对方的 ek)`（结果全零就作废），
`controlKey = HMAC-SHA256(shared, "hdrcast/v1/ctl-dh\n" + sid + "\n" + 观众的ek + "\n" + 主播的ek)`，取代 §8.9 的 `sub("ctl/" + sid)`。
组里其他人读得到 sid 和双方公钥，但算不出这把密钥：插不进一个受信任的会话，也读不到观众打的字。
任何一方没有 `ek`（旧版本）时双方都用 `sub("ctl/" + sid)`，这样的会话只适用"组里其他人"的控制规则。
局限：画面（SRT）仍用组派生的口令，组里的人仍可能在打洞阶段抢走一条路径；设备密钥保护的是"谁能进来、谁能控制"，不是画面本身。

**按需分享**（`app/src/share.rs`，`P2pHostOptions::admit` / `on_demand` / `device`）：

- 应用开着、有组、`access.json` 的 `available`（"开放连接"，默认开）为真时，P2pHost 一直在线：心跳带 `"od":true`，长轮询 join；采集引擎和编码器不运行。
  "开始直播"把 `od` 去掉并让编码器一直运行；停止后回到在线（关闭"开放连接"时则下线）。
- 每个通过检查的 join 先交给准入函数（在网络线程上，只做决定，慢的事放到别的线程）：
  1. 一分钟内被主播"断开"过的 → `denied`（否则可以直接连的设备断开后立刻就回来了）。
  2. 按规则（§9）：禁止 → `denied`；允许 → 放行；询问我 → 开播中直接放行，3 分钟内放行过（或正在看）的同一台设备 / 同一 peer id 的旧版本直接放行，
     2 分钟内被拒绝过的 → `denied`，否则弹出请求等主播回答，最多 50 s（之后 `timeout`）。同一台设备再来的请求替换前一个。
  3. 放行：没在采集就按当时的设置启动采集 + 编码，等 MPEG-TS 真正流出来（最多 25 s；失败就回答 `busy`，界面说明原因）再 `P2pHost::admit`：
     起 remux 子进程、打洞、签名回答 `ok`。这样观众的 mpv 一启动就有画面可接。整个准入在主播侧通常 1~2 s（本机实测 1.2 s）。
  P2pHost 自己兜底：90 s 没人决定的 join 回答 `timeout`。
- 观众端：所看主播的 `live` 带 `dk` 时最多等 60 s（2 s 没回答就显示"等待对方回答"）；回答来得晚（> 5 s）时用同一个 sid 重新打洞
  （第一轮 15 s 可能已经用完）。旧主播照旧等 20 s。
- 最后一个观众离开 20 s 后停止采集和编码（开播期间不停）；"全部断开" / "停止"会断开所有观众，各自一分钟内不能重连。
- 码率、人数上限、ffmpeg 路径可以在运行中改（`P2pHost::set_stream` / `set_ffmpeg_path`，设置一变就生效），`announce_now` 让下一次心跳马上发出。

## 9. 远程控制（0.3.0 起）

观众可以请求使用主播的鼠标和键盘。输入走 §8.9 的旁路通道，所以只有双方都是 0.3+ 才有；画面延迟还是 SRT 延迟 + 播放器缓冲，
这是"帮忙点一下"的功能，不是远程桌面（建议配合 300 ms 的"快速"预设）。

**谁说了算**（0.3.1 起按电脑）。主播的 `access.json`：

- "组里其他人"一条规则：观看（直接允许 / 询问我 / 禁止）+ 控制（不允许 / 询问我 / 自动获得）。也适用于所有旧版本观众：它们证明不了自己是哪台电脑。
  默认观看"询问我"、控制"不允许"；0.3.0 在 `app.json` 里的 `remoteControl`（0 关 / 1 询问 / 2 允许）在第一次运行 0.3.1 时变成这里的控制规则。
- 每台电脑（按 `dk`，§8.10）一条自己的规则。设备自己的控制规则只用在控制密钥是和这台设备协商出来的会话上；其余会话用"组里其他人"的。
- `live` 只在有人可能得到控制时带 `rc`（"其他人"或某台电脑的控制不是"不允许"）。规则在会话中途放宽时，观众端在下一次读在播列表时（≤ 10 s）开始用旁路通道。
- 规则改了立即作用于正在看的人；改成"禁止"的会被断开。主播随时可以对某个观众**收回**控制，或**断开**它。

观众的 join 总是带 `rc`（"我会说旁路通道"）。连上后它先不要控制（标志 bit0 = 0），只每 500 ms 发一个心跳，主播的回复告诉它会得到什么：
已允许（`Granted`）→ 自动接管，除非用户点过"只看不控"；需要询问（`Pending`）→ 界面给出"请求控制"，点了才把 bit0 置 1、在主播那边弹出请求；
不允许（`Denied`）。观众随时可以"只看不控"（bit0 置 0，主播松开它按住的一切）或再"接管控制"；同一会话里主播答应过一次就不再问。

**观众端不碰操作系统**：画面本来就在 mpv 的窗口里，mpv 的 JSON IPC 能给出一切。

- 指针：观察属性 `mouse-pos`（`x, y, hover`）和 `osd-dimensions`（窗口大小与画面四周的黑边），换算成"画面内的比例坐标"；在黑边上或窗口外时不发位置。
- 按键 / 鼠标键 / 滚轮：应用先问 mpv 自己的客户端名（`client_name`，形如 `ipc_0`），再 `define-section` 一个输入段，把**每个键名 × 16 种修饰键组合**
  都绑定到 `script-binding <客户端名>/hdrcast-input`（mpv 对修饰键是精确匹配，所以要全列出来；约 2600 行）。之后每次按键 mpv 发来
  `client-message ["key-binding","hdrcast-input","d-|u-|r-|p-…","Ctrl+c",…]`，带按下 / 抬起 / 重复和键名。非 ASCII 字符由 `any_unicode` 兜住。
  这个段只在主播允许期间启用（同时把 `window-dragging` 关掉，否则拖拽会拖动播放器窗口）；`Ctrl+Alt+ENTER` 留给本地切全屏。
- mpv 报的字符已经带了 Shift（`A`、`!`），修饰键只作为别的键的前缀出现（`Ctrl+c`），单独按 Shift / Ctrl 不会上报。

**报文**（旁路通道的载荷）：

```
 0  1   dir      1 观众→主播   2 主播→观众
 1  8   counter  大端，每个方向严格递增（既是 AEAD 的 nonce，也用来防重放 / 丢弃迟到的包）
 9  ..  ChaCha20-Poly1305(key = controlKey(sid), nonce = dir || 0 0 0 || counter, aad = "HDRCCTL1")
```

- 观众→主播明文：版本 1、标志（bit0 = 想要控制）、指针 x / y（0..65535，跨画面）、指针是否在画面内、第一个事件的序号、事件数（≤ 32）、
  事件 ×N（各 8 字节：类型 1 键下 / 2 键上 / 3 鼠标键下 / 4 鼠标键上 / 5 滚轮 / 6 文本，修饰键位 Shift=1 Ctrl=2 Alt=4 Meta=8，键码，参数）。
- 主播→观众明文：版本 1、授权状态（0 不允许 / 1 要问主播（已请求时：等主播回答） / 2 已允许，与观众要不要无关）、已执行到的事件序号。
- 指针位置取最新；按键、鼠标键、滚轮在收到确认前每 40 ms 重发，主播按序号**只执行一次**。指针移动最多每 8 ms 发一次；没事可说时每 500 ms 发一个心跳
  （让主播知道请求还在，也拿回授权状态）。5 s 没有任何回答 → 界面显示"这位主播没有开放"。
- 键码：字符键就是 Unicode 码点；命名键（Enter、方向键、F1–F12、小键盘…）排在 0x110000 之后（`crates/control/src/keys.rs`，只增不改）。

**主播端执行**（`HostControl` + `Injector`）：

- 修饰键：每个事件带着它发生时的修饰键位，执行前把"我们按下的修饰键"同步成它（缺的按下、多的抬起），所有键和鼠标键都抬起后把修饰键全松开。
- 字符键：Windows 用 `VkKeyScanW` 找到当前键盘布局里打出这个字符的键和所需的 Shift / Ctrl / Alt，缺的修饰键只在这一下按键周围点一下；
  布局里打不出的字符（中文等）用 `KEYEVENTF_UNICODE` 当文本输入。虚拟键都带上扫描码（有的游戏只认它）。指针用 `SendInput` 的虚拟桌面绝对坐标，
  限制在正在分享的那块显示器的矩形里。macOS 用 Quartz 事件（§10）。
- **不留按住的键**：收回、观众离开、会话结束时全部松开；1.5 s 没收到这位观众的任何报文也全部松开；序号出现缺口（观众的队列溢出过）时先全部松开再继续。
- 局限：Windows 上以管理员身份运行的窗口不接受注入；Win 键、`Alt+Tab`、`Ctrl+Alt+Del` 在观众自己的系统里就被拦下了；没有相对鼠标（不适合 FPS）。
- 主播和观众在同一台电脑上试的时候，真注入会让播放窗口里的指针反过来驱动自己：设 `HDRCAST_CONTROL_DRY_RUN=1`，主播端只把收到的动作写进日志
  （不写是哪个字符键，日志是要发给别人看的）。

## 10. macOS（0.3.0 起，尚未在真机上运行）

| Windows | macOS |
|---|---|
| DXGI 桌面复制 + D3D11 shader | ScreenCaptureKit（`SCStream`）。色彩转换由系统做：SDR 取 `420v`（NV12），HDR 取 10 bit 双平面（先试 `x420`，不给就取 `xf44` 在 CPU 上降成 4:2:0），PQ / BT.2020，需要 macOS 15 + Apple 芯片 |
| WASAPI loopback | 同一条 `SCStream` 的系统音频（macOS 13+，`excludesCurrentProcessAudio`）。没有"排除某个程序" |
| 命名管道 `\\.\pipe\<name>` | 临时目录下的 FIFO（`mkfifo`，0600，写端非阻塞 + `poll`）。4K60 P010 是 1.5 GB/s，FIFO 的吞吐是否够要实测 |
| HDR 屏总能采成 p010 | 应用对 HDR 屏传 `--format auto`：`x420`、`xf44` 都被系统拒绝时引擎退回 nv12，并在格式声明里说出来（§2.2），应用照它启动 ffmpeg，面板显示 SDR 和原因。只有命令行上显式写 `--format p010` 时才报错退出。显示器列表里的 `hdr` 表示"这块屏现在有 EDR 余量，并且这台 Mac 能采 HDR"，只用来预告格式 |
| NVENC / AMF / QSV / x265 | `hevc_videotoolbox`（HDR：`-profile:v main10`、`p010le`；`-realtime 1 -allow_sw 0`）或 x265 |
| mpv `--gpu-api=d3d11 --hwdec=d3d11va` | `--vo=gpu-next --hwdec=videotoolbox`（其余选项相同；HDR 直通是否还需要别的选项要在 XDR 屏上看） |
| mpv IPC 命名管道 | 临时目录下的 Unix 套接字 |
| 工具在 `tools\` | Homebrew：`/opt/homebrew/bin`、`/usr/local/bin`（`brew install ffmpeg-full mpv`；ffmpeg / ffprobe 先找 keg-only 的 `/opt/homebrew/opt/ffmpeg-full/bin`，在 PATH 之前，因为普通的 `ffmpeg` 不带 libsrt；Homebrew 的 mpv 也不带，所以 Mac 上播放方式默认是 ffmpeg 管道），或应用包里的 `Resources/tools` |
| `%APPDATA%\HdrCast` | `~/Library/Application Support/HdrCast` |
| Windows 防火墙规则 + **放行…** | 没有对应物（界面不显示这一栏）；UPnP 照做 |
| 稳定 IPv6 地址：`GetAdaptersAddresses` 的 SuffixOrigin / DAD 状态 | `getifaddrs` + `SIOCGIFAFLAG_IN6`（跳过 temporary / deprecated） |
| `SendInput` | Quartz 事件（`CGEventPost`），需要"辅助功能"授权；字符键按 US 键位表给键码，表里没有的当 Unicode 文本；双击靠自己数点击次数 |
| 剪贴板里有邀请时自动提示 | 不做（macOS 会向用户报告每一次读剪贴板）；用 **我有邀请码** / **粘贴邀请码** |
| Mica | 侧边栏毛玻璃（`NSVisualEffectView`），系统的红黄绿按钮 |
| 通知区域图标：左键打开窗口，右键菜单 | 菜单栏图标，单击出菜单（打开 / 退出）；窗口藏起来时点程序坞图标（`RunEvent::Reopen`）也打开窗口；程序坞图标一直在 |
| 登录时启动：`HKCU\…\Run` 的 `HdrCast` 值，任务管理器能关 | `~/Library/LaunchAgents/app.hdrcast.login.plist`：`ProgramArguments` = 程序本身 + `--background`，`RunAtLoad`，`LimitLoadToSessionType` = Aqua，`ProcessType` = Interactive。只写 / 删文件，不 `launchctl load`（会马上再启动一份）也不 `bootout`（会结束登录时启动的那一份），下次登录生效。开关的状态 = 文件存在且指向这个程序；在"系统设置 → 通用 → 登录项"里被关掉的读不到。程序在 App Translocation 的临时路径里（从"下载"直接打开的）时拒绝，请先拖进"应用程序" |
| 只运行一个：`instance.lock` | 同一套（`flock`）。从访达 / 程序坞打开时 LaunchServices 本来就只开一个，会把已经在运行的那个叫到前面（`Reopen`） |

权限：开播要"屏幕与系统音频录制"（第一次开播时系统弹框，允许后要重开应用）；当主播被远程控制要"辅助功能"。

