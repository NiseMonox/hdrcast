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
- v0.3.2（2026-10）：**丢包不再花屏**。中国观众看日本主播时，传输中丢掉的数据让画面糊成色块，一直到下一个关键帧。
  现在观众端的守卫（§3.2）让画面停在最后一帧完好的画面上，等到关键帧再继续（Moonlight 的做法）；主播端把编码器的码率缓冲从 2 s 降到
  0.25 s、关键帧间隔从 2 s 降到 1 s，并按线上的真实码率告诉 SRT（§2.5、§2.6），丢包本身也少了；两端都从 SRT 的包头读出丢包、重发、
  补发不及和往返时间，写进日志、显示在界面上（§8.11）。线上协议不变。
- v0.3.3（2026-10）：**按需关键帧**。观众丢了补不回来的数据时向主播要一个新的关键帧，不再等下一个定时关键帧（最长 1 s，丢失跨过关键帧时更久）。
  为此编码可以搬进采集引擎：引擎自己驱动 NVENC（§2.8），ffmpeg 只加声音、原样复制视频；观众在数据还缺在路上时就经旁路通道请求，主播对所有观众
  合起来限速（§8.12）。在 **开播 → 更多选项 → 按需关键帧** 打开，只对 NVENC；做不到时照旧由 ffmpeg 编码。新字段 `kf` 是可选的，旧版本不受影响。
- v0.3.4（2026-10）：**升级不用再放行防火墙**。发行包里的文件夹固定叫 `HdrCast`，只有 zip 的名字带版本号（§6）：Windows 防火墙的规则和
  登录时启动的 Run 项认的都是 exe 的完整路径，新版本解压覆盖旧文件夹，两样都不用重做；以前每个版本一个文件夹，每次升级都要再点 **放行…**（UAC）。
  其余与 0.3.3 相同。
- v0.3.5（2026-10）：**观众都离开后自动结束直播**。开播后，看过这次直播的人都离开 2 s，直播就自动结束、停掉采集和编码器，这台电脑
  回到在线（关着开放连接时下线）；之后再连进来的观众按需分享，3 分钟内看过的不用再问（§8.10）。开播页 **观众** 一节可以关掉。
  `sender.json` 多了字段 `endWhenViewersLeave`（没有时为开），线上协议不变。
- v0.3.7（2026-10）：**控制模式**，为远程控制的超低延迟（§11）。观众在观看页选 **观看** 还是 **控制**；选控制时，提供它的主播（有 NVIDIA 显卡）
  另起一路照 Vibepollo 的方式编码的流（NVENC 超低延迟调优、一帧的 VBV、采到就编、不定时出关键帧），不经 SRT、不缓冲，每帧切片加 Reed-Solomon 校验、
  加密后走打洞出来的同一条路径，丢了不重发、只要关键帧；观众把收齐的每一帧直接写进 mpv（不缓冲、单线程硬解）。本机端到端 1080p 中位 14.5 ms、
  4K HDR 21.3 ms。线上协议的新字段 `ll` 是可选的，旧版本不受影响。

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

ffmpeg 的 mpegts 封装器**不支持 AV1-in-MPEG-TS**（本机实测 7.1、master，2026-10-24 又用随包的 9.0.2 试过：`av1_nvenc` 写成 private stream
（stream type 6），读回是 bin_data，解不了；`crates/media/tests/encoder_args_integration.rs` 的 `av1_really_cannot_be_muxed_into_mpegts` 盯着它，
哪天它失败了就该重新考虑）。点对点的分发靠 MPEG-TS（中途加入的观众靠 TS 的重复头恢复，丢包后靠它重新对齐，守卫看的也是 TS 的连续计数），
所以只能 **HEVC Main10**。v0.1 的直连模式可以用 Matroska 走 AV1，但直连模式已随中继一起删除。

解码不是问题：随包的 mpv 能用 d3d11va / nvdec / vulkan 硬解 AV1，ffmpeg 带 dav1d。要用 AV1，除了封装，还有几处只为 HEVC 写的：守卫的 RASL 办法（§3.2，
AV1 要换成别的，比如插一个"重复上一帧"的 show_existing_frame）、引擎里的 NVENC 编码（§2.8）、`encode/ts.rs` 的流类型，以及老版本的观众。
一次编码给所有观众，所以每位观众都得能硬解 AV1（RTX 30、RX 6000、Intel 11 代 / Arc 起），否则主播要多编一路 HEVC。
能省多少（2026-10-24，RTX 4090 的 NVENC，§2.5 那段合成素材，两边同样的参数：p5 hq、两个 B 帧、AQ、VBV 0.25 s、GOP 1 s；PSNR-Y / SSIM）：

| 码率 | HEVC | AV1 |
|---|---|---|
| 10 Mbps | 28.65 / 0.659 | 28.91 / 0.664 |
| 20 Mbps | 28.89 / 0.681 | 29.14 / 0.685 |
| 40 Mbps | 29.26 / 0.722 | 29.51 / 0.718 |

亮度上 AV1 一直高约 0.25 dB；按 SSIM 和带色度的 PSNR 看，10～20 Mbps 时略好，40 Mbps 时持平。大约是低码率时省 10～30 % 的码率，40 Mbps 时几乎没有。
要做的话得自己在 TS 里装 AV1（主播端的 TS 是自己写的；每位观众的 ffmpeg remux 换成自己的转发；观众端的守卫把 AV1 拆出来，重新封装成 mpv 能读的，例如 Matroska），
外加 HEVC 后备和版本协商。

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

另有一个很小的独立程序 `HdrCastInput.exe`（`service --user <SID>` / `serve` / `install --user <SID>` / `uninstall`，没有窗口）：它既是 Windows 服务也是服务在用户会话里启动的输入进程，让远程控制能进管理员身份运行的窗口，见 §9.1。

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
        --encode nvenc           在引擎里编码（§2.8）：视频管道里是 HEVC 的 MPEG-TS 而不是原始帧，stdin 收命令
          --bitrate <kbps> --vbv <kbit> --gop <帧> --bframes <n> --preset <1..7> --multipass off|quarter|full --aq <0..15>
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
- **关键帧间隔**默认 1 s（`-g` = fps × `gopSeconds`；0.2 / 0.3.1 是 2 s）：传输中丢掉的数据让观众的画面停到下一个关键帧（§3.2），
  1 s 让这段时间减半，代价是多大约 3 % 码率。Moonlight 用无限 GOP、丢包时向主播要 IDR；ffmpeg 命令行没有运行中要关键帧的办法。
  打开"按需关键帧"时编码改在采集引擎里做（§2.8），丢了数据的观众马上能拿到关键帧（§8.12），定时的关键帧照旧每秒一个。
- 引擎自己编码时（§2.8）视频输入换成引擎写的 TS，视频原样复制，其余不变：
  `-thread_queue_size 1024 -f mpegts -probesize 2000000 -analyzeduration 100000 -fpsprobesize 0 -i \\.\pipe\hdrcast-video-<pid>`、声音输入同上、
  `-map 0:v:0 -map 1:a:0 -c:v copy`（没有编码器参数和 `-fps_mode`；色彩标记在码流的 VUI 里）。0.1 s 足够找到流：第一帧就是带参数集的关键帧，
  帧率不用估计，每一帧都带时间戳。
- **码率缓冲**（VBV，`-bufsize` = 码率 × `bufferSeconds`）默认 0.25 s（0.2 / 0.3.1 是 2 s，界面上没有这一项）。它决定 CBR 下复杂画面能超出码率多少，
  而 SRT 给每位观众最多按 `inputbw × (1 + oheadbw)` 发（§2.6，重发也算在里面）：超出的部分在 SRT 的发送队列里排队，而 SRT 的期限从**采集时刻**算起，
  排队用掉的正是丢包重发的时间，排得太久的包到观众那里已经过期。实测（2026-10-23，NVENC p5 CBR 40 Mbps，静止和满屏噪点每 3 s 交替，
  按 SRT 的节流模拟发送队列，oheadbw 25 %）：

  | `bufferSeconds` | 最忙的一秒 | 发送前最多排队 | PSNR |
  |---|---|---|---|
  | 2（旧默认） | 82 Mbps | 860 ms | 27.05 dB |
  | 1 | 70 Mbps | 600 ms | 27.03 dB |
  | 0.5 | 56 Mbps | 265 ms | 27.01 dB |
  | **0.25** | 46 Mbps | 110 ms | 27.00 dB |

  2 s 时平均码率也冒到 45 Mbps（设的是 40）。朋友试过的 800 ms / 1000 ms 延迟下，排队 860 ms 的包第一次发出就已经晚了。Sunshine 用一帧的 VBV（超低延迟），
  这里是缓冲直播，0.25 s 足够平稳，画质几乎不变。
- **和 Vibepollo（Sunshine 的分支）的默认 NVENC 设置比**（`src/nvenc/nvenc_base.cpp`、`nvenc_config.h`：P1、超低延迟调优、不用 B 帧和 AQ、一帧的 VBV、
  只在被要求时出关键帧）。2026-10-24 实测，12 s 合成素材（分形缩放 + 测试图，1080p60 10-bit，极难压缩），同一块 RTX 4090 上 ffmpeg 的 hevc_nvenc
  按两边的参数编，PSNR-Y / SSIM 对源：

  | 设定码率 | 0.3.1（VBV 2 s，GOP 2 s） | 0.3.3 默认 | 0.3.3 按需关键帧（无前瞻） | Vibepollo 默认（P1） | Vibepollo P7 |
  |---|---|---|---|---|---|
  | 10 Mbps | 28.67 / 0.661 | 28.65 / 0.659 | 28.65 / 0.660 | 28.66 / 0.658 | 28.72 / 0.661 |
  | 20 Mbps | 28.94 / 0.686 | 28.89 / 0.681 | 28.82 / 0.681 | 28.87 / 0.675 | 28.93 / 0.676 |
  | 40 Mbps | 29.40 / 0.731（实际 43.6 Mbps） | 29.26 / 0.722（40.6） | 29.13 / 0.713（40.4） | 29.16 / 0.705（38.1） | 29.21 / 0.705（37.8） |

  同码率下 HdrCast 的设置略好，码率越高差得越多（B 帧、AQ 和 0.25 s 的 VBV 要有富余的码率才用得上）；一帧的 VBV 用不满设定的码率（40 Mbps 时只用到 38）。
  0.3.1 的画质高一点，有一部分是因为它多用了 9 % 的码率，另一部分是 2 s 的 VBV 能把码率挪给难的帧，代价就是上面那个突发。只是一段素材、一次测量：量级参考。
- **旧设置文件**：`sender.json` 没有 `settingsVersion`（0.3.2 之前写的）时，`SenderSettings::load` 把还是旧默认值的 `gopSeconds` 2 和 `bufferSeconds` 2
  换成新默认值，一次（之后文件里写 `"settingsVersion": 1`，再选 2 s 就是用户的选择）。`bufferSeconds` 的下限从 0.5 改成 0.25。
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
- `inputbw` 是**线上码率**（0.3.2 起，`RemuxChild::wire_bps`）：`视频码率 × 1.05 × 1360/1316 + 320 kbit/s`，即 MPEG-TS 封装和 CBR 的超出
  （NVENC 实测：40 Mbps 的视频封装成 TS 后是 41.2 Mbps（缓冲 0.25 s）到 42.8 Mbps（缓冲 2 s），取 5 %）、每个 1316 字节载荷上
  44 字节的 SRT / UDP / IP 头（SRT 节流时连头一起算），再加上声音。
  0.3.1 之前只告诉 SRT 视频码率，`oheadbw` 25 % 里实际只剩不到一半留给重发。`maxbw=0` + 固定 `inputbw` 比让 SRT 自己测输入码率（`inputbw=0`）好：
  桌面静止时 NVENC 的输出很低，测出的码率跟着变低，画面一动起来就被限在很低的速率上；`mininputbw` 能防这个，但 ffmpeg 的 `srt://` 不支持这个选项。
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
   下面是正在观看的人（设备码、IPv6 / IPv4、ping、延迟、远程控制状态与 允许 / 拒绝 / 收回、**断开**；开播中看过的人都离开后，这里是直播结束的倒计时，§8.10）。
   设置部分：显示器卡片（带 HDR 徽标、分辨率、刷新率、峰值亮度）；画面（码率滑块、帧率、分辨率、HDR 自动 / 始终 SDR、编码器 + **检测**，
   更多选项里是编码预设、关键帧间隔、B 帧、Lookahead、指针）；声音（开关、设备、AAC / Opus、排除程序）；
   **谁可以连接**（"开放连接"开关；"组里其他人"的 观看 / 控制 规则；每台电脑一行规则，可移除；**添加电脑…** 从在线或最近请求过的电脑里选：
   "我的电脑" = 直接允许 + 自动获得控制，"信任的朋友" = 直接允许 + 控制要询问）；观众（同时观看人数、上行带宽与估算、**观众都离开后结束直播**、防火墙 **放行…**）。
   底部固定一条：将要发送的格式 + **开始直播**。
2. **观看**：组里在线的电脑（每 10 s 向信令服务器要一次；不要更快，服务器按 IP 限流每秒 10 次）：开播中的标 LIVE、按钮 **观看**，
   按需的标"在线"、按钮 **连接**，都带设备码；延迟预设（快速 300 ms / 日本 / 澳大利亚 / 中国 / 自定义）；输出（自动 / HDR / SDR）。
   观看中顶部是面板：缓冲、帧率、码率、丢帧、是否 HDR 直通，**悬浮窗**（打开 / 收起播放器上的面板，§3.3）/ **全屏** / **停止**，
   以及远程控制一行（**请求控制** / **接管控制** / **只看不控**）；等主播回答时显示"等待同意"，被拒绝时显示原因和 **关闭**。
   观看中延迟一栏不能改（说明里指向悬浮窗：在那里改会重新加入）；硬解在设置页随时可改，立即生效。
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
- **分享中改设置**（0.3.3 起，`share.rs` 的 `apply_while_streaming`）：开播页的设置在分享中不再锁住，改了立即生效。每次保存后按新设置重新算一份
  `SendPlan`，和正在跑的比：一样就什么都不做；只差码率、并且编码器在采集引擎里（按需关键帧，§2.8）时，给引擎发 `bitrate <kbps> <vbv>`，不重启也不出 IDR；
  其余任何变化（分辨率、帧率、编码器、HDR、声音、关掉按需关键帧……）在 800 ms 后让采集和编码器重启一次（这段时间里接连的改动合并成一次），
  和崩溃保护一样，新编码器的输出接进同一个分发器，观众不断开，画面停一秒左右（守卫等下一个关键帧，§3.2）。设置不能用时（例如没有显示器）正在跑的照旧。
  界面在画面一节下写明这一点。
  码率**调高**时还有一件事：每位观众的 SRT 连接按他加入时的码率限速（remux 子进程的 `inputbw` = `RemuxChild::wire_bps`，§2.6），超过它的流会堵在发送队列里、
  过了延迟被丢掉。所以 `P2pHost::set_stream` 发现新码率高于某位观众连接的规格时重建那条连接（`resize_legs`）：从分发器摘下它的 remux 子进程、关掉输入，
  ffmpeg 正常收尾、SRT 正常关闭，观众那边立刻知道（不用等 5 s 的空闲超时），播放器照常 3 s 后重连，新的子进程按新码率建好，按需关键帧马上给它一个关键帧
  （3 s 没退出的子进程直接结束）。实测（2026-10-24，同机，25 → 40 Mbps）：观众 0.1 s 内断开，约 9 s 后重新播放。码率调低不用重建。
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

### 2.8 在采集引擎里编码（按需关键帧，0.3.3 起）

**为什么不能只用 ffmpeg 命令行**：运行中要不来关键帧。`-force_key_frames` 只能事先写好时间表或表达式（表达式读不到外部状态）；`source` 跟随输入帧的
关键帧标记，而 rawvideo 的每一帧都是关键帧；stdin 的交互命令只到滤镜，没有能改帧类型的滤镜。码率也不能在运行中改。要按需出 IDR，就得自己驱动编码器。

**选型**：

| 方案 | 能做 | 代价 | 结论 |
|---|---|---|---|
| ffmpeg 命令行（以前的唯一方式） | 四种编码器、参数调好了 | 运行中不能要关键帧、不能改码率 | 保留：默认，也是后备 |
| libavcodec（FFmpeg 的 DLL：hevc_nvenc / amf / qsv） | 编码器、AAC、mpegts 封装都现成；`pict_type = I` 加 `forced-idr` 强制 IDR；nvenc 改 `bit_rate` 会重新配置（但同时出一个 IDR） | 要随包带 FFmpeg 共享库，编译时还要头文件和导入库（本机没有，要下载 BtbN 的 gpl-shared）；结构体布局绑定一个 FFmpeg 大版本（现在是 63），`get-tools force` 换了大版本就失效；进程里多一层 C | 不选 |
| NVENC SDK 直连（驱动自带的 nvEncodeAPI64.dll，Sunshine 的 `nvenc_base.cpp` 也是这样） | IDR（`NV_ENC_PIC_FLAG_FORCEIDR`）；改码率不重置也不出 IDR（`nvEncReconfigureEncoder`）；以后可以做参考帧失效（`nvEncInvalidateRefFrames`）和 D3D11 零拷贝 | 只有 NVIDIA；结构体要自己写；TS 封装要自己写一小段 | **选它** |

理由：主播是 RTX 4090；不用多发任何文件（DLL 随驱动）；控制更细（改码率不出 IDR、以后的 RFI）；仍然在采集引擎这个子进程里，出事只是重启并退回 ffmpeg。
AMD / Intel / x265 照旧走命令行。头文件 nvEncodeAPI.h 12.1（MIT）只用来写绑定（`crates/capture/src/win/nvenc/ffi.rs`，只写用到的结构体，
其余并入保留数组）：每个结构体的大小、每个用到的字段的偏移和每个位域的位置都用 MSVC 从头文件打印出来，写成编译期断言。

**结构**（`hdrcast-capture` 的 `encode/` 和 `win/nvenc/`，应用的 `sender_runner`）：

- 引擎（`HdrCast capture … --encode nvenc …`）：采集照旧。采集用的显卡就是 NVIDIA 时**零拷贝**（见下面"零拷贝"）：NVENC 开在采集的 D3D11 设备上，
  转换 shader 直接写进 NVENC 注册的 P010 / NV12 纹理，帧不离开显存。否则（笔记本上屏幕接在核显上，或者零拷贝开不起来）走内存：每一帧（CPU 内存里的
  P010 / NV12，就是以前写进管道的那一帧）在帧队列的写线程上复制进 NVENC 的输入缓冲，NVENC 用自己在 NVIDIA 显卡上建的 D3D11 设备。两种都在写线程上
  同步编码；B 帧在后一个参考帧到时一起出来。
- 输出写成只有视频的 MPEG-TS（`encode/ts.rs`）进同一个视频管道：PAT / PMT 在开头和每个关键帧前，一帧一个 PES，PTS = 帧序号，DTS 按 B 帧的重排深度
  往前移（与 ffmpeg 的 hevc_nvenc 算 DTS 的办法相同），PCR 在视频 PID 上。ffmpeg 读它和原来的声音管道，`-c:v copy`（§2.5），所以分发器、remux 子进程和观众
  收到的 TS 仍是 ffmpeg 的封装器写的，和以前一样。
- 关键帧：NVENC 的 GOP 设为无限，引擎自己决定（`KeyframeClock`）：第一帧、上一个关键帧之后每 `--gop` 帧、被要求的那一帧；被要求的关键帧让计数
  重新开始（下一个定时的在它一个间隔之后）。
- 命令：引擎的 stdin，一行一个：`keyframe`、`bitrate <kbps> [<vbv kbit>]`。事件：stdout 上的 JSON 行
  `{"keyframe":{"frame":1234,"asked":true,"bytes":286320,"waited_ms":4.2}}`、`{"bitrate":{"kbps":30000,"ok":true}}`（不是扁平对象，不会被当成统计行）。
  应用的 `SenderRunner` 把引擎的 stdin 留着发命令（`request_keyframe`、`set_bitrate`），把事件交给按需关键帧的逻辑（§8.12）。
- 参数和 `encoder_profiles` 给 hevc_nvenc 的一样（`engine_encode_args`：preset P3 / P5 / P7、HQ 调优、CBR、VBV、二次编码 off / qres / fullres、空域 AQ 8、
  B 帧按设置且以中间帧作参考、Main10 / Main、high tier、VUI 色彩标记和 chroma 在左、每帧一个 slice、每个 IDR 前带 VPS/SPS/PPS），只有两处不同：
  **不用前瞻**（前瞻 20 帧会把被要求的关键帧也压后 20 帧，即 333 ms），所以也没有时域 AQ（它依赖前瞻）。界面上打开开关时"前瞻"变灰。
- 后备：引擎打不开编码器（没有 NVIDIA、驱动低于 API 12.1、画面超出编码器尺寸、编码会话已满）或编码时出错，以退出码 7（`ENCODER_UNAVAILABLE`）退出，
  打不开时在宣告帧格式之前就退出。`SenderRunner` 立即以 ffmpeg 编码重新开始（这一场分享里不再试），日志写明原因；画面照常，只是没有按需关键帧。

**零拷贝**（`win/surfaces.rs`、`win/converter.rs`、`win/nvenc/`）。以前每一帧 GPU → CPU（staging）→ NVENC 的输入缓冲（驱动再传回 GPU），4K60 P010 时
PCIe 上两次各 1.5 GB/s，CPU 上还有两次同样大小的内存复制。现在：

- 条件：采集的适配器是 NVIDIA，驱动有 D3D11.4 的 fence，P010 / NV12 纹理能当类型化 UAV 写。成功时日志 `capture: encoding here: … zero copy (N surfaces on the GPU)`；
  不行时 `capture: no zero copy (原因): the frames go to the encoder through memory`，接着走内存那条路（本机 2 号输出接在 AMD 核显上，就是这样）。两条路都
  开不起来才以退出码 7 退出，和以前一样。环境变量 `HDRCAST_NO_ZERO_COPY=1`（主播端，引擎继承应用的环境）强制走内存，用来对比，或者遇到处理不好共享设备的驱动时绕开。
- 先开 NVENC 会话、初始化（尺寸、格式不支持时到此为止，不白建纹理），再在采集设备上建 N 张 P010 / NV12 纹理（只要 `BIND_UNORDERED_ACCESS`，NVENC 注册
  不要求别的绑定）并逐张 `nvEncRegisterResource`。N = 引擎同时占着的帧（队列 4 + 最新一帧 + 正在转换的一帧 = 6）+ 编码器压着的 B 帧数。
- shader 多两个入口 `main_p010_planes` / `main_nv12_planes`：算式不变（和原入口共用 `p010_block`、`chroma8`），一个线程一个 2×2 块，经两个类型化 UAV 写纹理的
  两个平面（亮度 `R16_UINT` / `R8_UINT`，色度 `R16G16_UINT` / `R8G8_UINT`；P010 写 `值 << 6`）。`--selftest` 在 PASS 前多两行
  `zero copy: the shader writing the encoder's p010 / nv12 surface gives the same frame`：同一输入经两条路（读回内存 / 写纹理）出来的帧逐字节相同
  （P010 用原来的灰阶，NV12 用一张彩色图缩到一半，走采样器那条路）。
- 交接：引擎的帧池照旧管"哪一帧在用"，只是缓冲不带字节，下标就是表面号（`FrameBuffer::index`）。帧池给出的表面没有人在用（不在队列里、不是最新一帧、
  编码器没压着），转换只往这样的表面里写；重复上一帧就是把同一个表面再交一次。引擎线程 Dispatch 之后在 immediate context 上对一个 `ID3D11Fence` Signal
  再 Flush，写线程在 `nvEncMapInputResource` 之前等到这个值（`SetEventOnCompletion`，不占 CPU），不靠 NVENC 自己的隐式同步。反方向靠帧池：编码器在图像
  出来（`nvEncLockBitstream` 返回）、`nvEncUnmapInputResource` 之后才放开那一帧。B 帧那一次调用返回"need more input"时帧留在编码器里（同一个表面重复时
  只映射一次、计数），下一次成功的调用把前面的图像都编出来，再一起放开。一个设备两个线程用，建表面时打开 `ID3D11Multithread::SetMultithreadProtected`。
- 测试：`win::surfaces::tests::every_encoded_picture_is_the_one_the_shader_wrote` 照引擎的样子开两个线程（源每帧换一种灰，NV12 不带 B 帧、P010 带 2 个 B 帧，
  各 240 帧，帧池能给多快就转换多快），ffmpeg 解出来逐帧核对亮度。故意让编码器先拿到还没写的表面时它失败（第 0 帧读出 0）；把 B 帧的输入提前放开在
  驱动 621.01 上测不出来（二次编码关掉也一样，看样子 NVENC 提交时就取走了输入），照 API 的要求仍然压到图像出来。`tests/engine.rs` 的
  `with_zero_copy_no_surface_is_converted_into_while_it_is_in_use` 用假屏幕和压两帧的假编码器查引擎这一侧：不往在用的表面里写、图像次序不乱、帧池够用（不丢帧）。

**实测**（2026-10-24，RTX 4090，驱动 621.01，桌面；在应用里经过整条链路）：

| | 1080p60 P010 20 Mbps | 4K60 P010 40 Mbps |
|---|---|---|
| 从命令到关键帧写进管道（2 个 B 帧） | 7～22 ms | 20～42 ms |
| 采集 / 封装 | 60 fps，不丢帧 | 60 fps，不丢帧，ffmpeg speed 1 |

4K 不用 B 帧时这个时间是 14～30 ms：强制 IDR 时 NVENC 要先把排着的两个 B 帧编出来。ffprobe 读回：HEVC Main 10、yuv420p10le、BT.2020 / PQ、chroma left，
关键帧正好在引擎报告的帧上。画质（合成内容：分形缩放 + testsrc2，1080p60，NVENC P5 CBR 10 Mbps，VBV 0.25 s，PSNR）：前瞻 20 帧 31.27 dB，不用前瞻 31.26 dB
（大小相同），再关掉 B 帧 31.22 dB。这个码率下去掉前瞻几乎不损画质；码率高时损失大一些：另一段更难的素材 40 Mbps 时 PSNR-Y 29.26 → 29.13 dB、
SSIM 0.722 → 0.713，10 Mbps 时没有差别（§2.5 的对比表）。

**零拷贝的实测**（2026-10-24，同一台机器；`HdrCast capture … --encode nvenc --dump …` 每次 30 s，改前 → 改后交替各跑两次取平均；屏幕角上一个 2×2 像素、
每秒变色 250 次的窗口让桌面每帧都是新的，所以每帧都转换；CPU 是引擎进程占一个核的百分比，复制引擎和显存是 Windows GPU 计数器里这个进程的值，
关键帧是 stdin 每 1.37 s 一次 `keyframe` 到图像写进输出的 `waited_ms`，每格 38 次）：

| | 4K60 P010 40 Mbps | 1080p60 NV12 20 Mbps | 4K60 P010，2 个 B 帧 |
|---|---|---|---|
| 引擎 CPU（一个核的 %） | 28.9 → 2.1 | 8.3 → 2.5 | 33.3 → 2.7 |
| GPU 复制引擎（本进程） | 6.2 % → 0 | 1.0 % → 0 | 6.2 % → 0 |
| 引擎线程每次转换（`gpu_ms`，以前含等读回） | 3.1 → 0.08 ms | 0.49 → 0.08 ms | 3.4 → 1.4 ms |
| 关键帧延迟中位（最短～最长） | 21.3（13～29）→ 19.8（11～28）ms | 12.3（4～22）→ 11.1（4～20）ms | 24.4（14～48）→ 20.6（11～46）ms |
| GPU 用的系统内存 / 显存 | 281 → 162 MB / 482 → 492 MB | 55 → 40 MB / 113 → 102 MB | 475 → 308 MB / 656 → 666 MB |
| 帧率 / 丢帧 | 60 / 0 → 60 / 0 | 60 / 0 → 60 / 0 | 60 / 0 → 60 / 0 |

PCIe 上 GPU → 主机的流量（`nvidia-smi dmon` 的 txpci，整机的数）在一次后台比较安静的 4K60 对比里是 3963 → 275 MB/s，前后空闲时 209～569 MB/s；
主机 → GPU 那一侧被别的程序（这台机器上同时有视频解码、有时还有另一个 NVENC 会话）盖住，看不出干净的差。另外 4K60 测时整机 CPU 用量随同时运行的其他程序波动，
同样的改前程序另两次单独测是 32～38 %。关键帧只快了 1～4 ms：剩下的主要是等下一个帧节拍（最多 16.7 ms）和 IDR 本身的编码，B 帧时还要先把排着的
B 帧编出来。带 B 帧时引擎线程每次转换平均 1.4 ms（不带时 0.08 ms），原因没有查清，大概是 NVENC 的调用占着 Direct3D 的设备锁；帧率不受影响。
同一个程序走内存那条路（2 号输出，AMD 核显采集、RTX 4090 编码）照常 60 fps、不丢帧；`--size 8448x4752` 这种 NVENC 编不了的尺寸两条路都开不起来，
退出码 7、不宣告帧格式。经过命名管道交给 ffmpeg（`-f mpegts … -c:v copy` 加声音，和 `SenderRunner` 的命令一样）的 4K60 HDR（2 个 B 帧）和 1080p60 SDR
各跑了一遍：要的关键帧 14～41 ms 出来并让计数重来，`bitrate` 两次都 `ok`，ffprobe 读回 HEVC Main 10 / Main、BT.2020 PQ / BT.709，解码无错。

应用里的单机回环（`docs/windows-checklist.md` 文末的做法：两个实例、本机信令；主播 4K60 HDR 40 Mbps、2 个 B 帧、按需关键帧，观众 latency 1500 ms）：
主播日志 `zero copy (8 surfaces on the GPU)`，观众的 mpv 用 d3d11va 硬解 HEVC Main 10 3840x2160 60 fps、输出 p010（HDR 直通），观众开始看时的关键帧 34 ms 后
出来。主播的采集引擎（应用的子进程，带声音）CPU 3.2 %、复制引擎 0；同样设置加 `HDRCAST_NO_ZERO_COPY=1` 是 39.7 %、6.2 %。观众每 4 s 丢 30 个包
（`HDRCAST_SIMULATE_LOSS=4:30`）两种各跑约 3～4 分钟：从请求到关键帧离开编码器，零拷贝 17～43 ms（中位 32）、走内存 16～48 ms（中位 36）；画面停住
（观众每 10 s 的 `[guard]` 汇总）零拷贝 45 次平均 320 ms、走内存 54 次平均 304 ms。停多久看丢包落在哪里（落在关键帧上就要等下一个），和走哪条路无关：
§8.12 里 4K60 的 200～233 ms 是改之前那次（每次丢 60 个包）里短的那一簇，同一次里也有 416～583 ms 的。

**还没做**：

- 零拷贝只在采集和编码是同一块 NVIDIA 显卡时有效；屏幕接在核显上的笔记本仍走内存（跨适配器共享纹理也要过一次 PCIe，只能省掉 CPU 那两次复制）。
- 自适应码率：`bitrate` 命令已经通（`nvEncReconfigureEncoder`，不重置、不出 IDR；RTX 4090 报告支持 `DYN_BITRATE_CHANGE`），`SenderRunner::set_bitrate`
  目前只在分享中改码率时用（§2.7）。按 `SrtMonitor` 的丢包自动降码率时要注意：每位观众 remux 子进程的 `inputbw` 是开始时按码率定的，降码率没问题，
  升高时 `resize_legs` 会重建观众的连接（§2.7，观众停约 9 s）：自动升码率要么只升回观众加入时的码率，要么接受这一下。
- AMD（AMF）和 Intel（oneVPL）也能强制 IDR，要各自接。

## 3. 接收端

### 3.1 mpv 命令（来自 docs/research/mpv-hdr-windows，已核对 mpv 0.40/0.41 手册与源码）

```
mpv --vo=gpu-next --gpu-api=d3d11 --hwdec=d3d11va
    --target-colorspace-hint=yes            ← HDR 屏（Windows 必须已开 HDR）；SDR 屏改为 no 并加 --tone-mapping=bt.2390
    --hdr-compute-peak=auto
    --cache=yes --demuxer-readahead-secs=3 --cache-pause-initial=yes --cache-pause-wait=1.5 --demuxer-max-bytes=256MiB
    --video-sync=audio --interpolation=no --framedrop=vo
    --demuxer-lavf-probesize=5000000 --demuxer-lavf-analyzeduration=2
    --input-ipc-server=\\.\pipe\hdrcast-mpv --config=no --input-default-bindings=no --window-dragging=no --drag-and-drop=no
    --keep-open=no --force-window=yes --osc=no --osd-bar=no --media-controls=no --title="HdrCast"
    [--fs --fs-screen=N] [--mute=yes]
    "srt://127.0.0.1:<L>?mode=caller&passphrase=<组派生>&pbkeylen=32&latency=<µs>&rcvbuf=<bytes>&ffs=<n>&recv_buffer_size=8000000"
```

- SRT 选项一律放 URL 查询串（`--demuxer-lavf-o` 到不了协议层）。
- `--osc=no --osd-bar=no`（0.3.3 起）：直播流没有可以拖的进度，mpv 自带的屏幕控制栏（进度条和按钮，鼠标一动就盖在画面上，远程控制时也一样）和音量 / 跳转时的进度条都关掉。
- **播放窗口只管画面和远程控制**（0.3.3 起）：`--config=no`（不读用户自己的 mpv.conf / input.conf / 脚本，HdrCast 的选项说了算）、
  `--input-default-bindings=no`（mpv 自带的按键和鼠标动作全部不用：滚轮调音量、空格暂停、q 退出、双击全屏、9 / 0、m……）、`--window-dragging=no`
  （拖画面不移动窗口；远程控制时拖拽本来就该给主播，以前是控制期间临时关掉）、`--drag-and-drop=no`（拖进来的文件不会顶替直播）。
  播放器自己留下的只有 HdrCast 通过 IPC 绑定的几个（`hdrcast_control::keys::player_bindings`，mpv 的 IPC 一连上就绑，`file-loaded` 时再绑一次）：
  `Ctrl+Alt+O` 打开 / 收起悬浮窗（§3.3），`Ctrl+Alt+Enter` 切全屏，`Esc` 退出全屏（远程控制时这两个之外的键都给主播，`Esc` 也是），
  窗口的关闭按钮（`CLOSE_WIN`，以前是 mpv 自带的 quit，应用随后会把 mpv 再开起来）= 停止观看。全屏、静音等都在悬浮窗和观看页里。
  `keybind` 建的绑定不是"默认"级，不受 `--input-default-bindings=no` 影响。
- `--media-controls=no`（0.3.3 起，只在 Windows 上；mpv 0.39 起才有这个选项）：直播用不着 Windows 的媒体控制浮层。它还避免 mpv 退出时卡住：
  关掉屏幕控制栏后，mpv 0.41 打开失败时每次都卡在媒体控制（SystemMediaTransportControls）的收尾里，直到 12 s 后被杀掉，加上这个选项后 0.2 s 就退出了
  （2026-10-24 实测）。应用要等 mpv 退出才会重连，所以这一项不能省。
- `rcvbuf/ffs` **必须**按 码率 × 延迟 计算（否则 > 40 Mbps + 2 s 时 libsrt 会 "No room to store incoming packet" 断线）。码率由用户在 GUI 填（或默认按 100 Mbps × 延迟上限算，宁大勿小）。
- mpv 只在打洞路径建好后启动；之后路径断了由 ViewerSession 在同一个本地端口上重新加入，mpv 每 3 s 自己重连。
- HDR 直通只在 `--wid` 嵌入或独立窗口下都以「窗口化 HDR」工作，静态元数据只在全屏时下发 → 提供全屏按钮（`--fs`，不用 `--d3d11-exclusive-fs`）。
- **不嵌入 libmpv**，独立启动 mpv.exe；用 JSON IPC 命名管道读 `demuxer-cache-duration`、`frame-drop-count`、`video-params`、`video-target-params`、`hwdec-current`、`estimated-vf-fps`、`paused-for-cache` 显示状态。
- **播放方式**（`receiver.json` 的 `frontend`，0.3.2 起）：0 **自动**（默认）= 找得到 ffmpeg 就走 ffmpeg 管道，否则 mpv 直接播放；
  1 **ffmpeg → HdrCast → mpv** = `ffmpeg -f data -i srt://127.0.0.1:<L>?... -map 0 -c copy -f data -flush_packets 1 pipe:1`，
  stdout 经过守卫（§3.2）进 `mpv -`（`-f data` 进出：不解复用也不重新封装，SRT 的载荷原样交出来，守卫才看得到原来的连续计数器和时间戳；
  mpv 也不需要带 libsrt）；2 **mpv 直接播放** = mpv 自己打开 `srt://`，传输中受损的画面照样显示。0 在 0.3.1 之前表示"mpv 直接播放"（当时的默认值），
  所以旧的设置文件都自动用上守卫。
- mpv 路径：同目录 `mpv.exe` / `tools\mpv.exe` / PATH；缺失时提示下载 shinchiro 或 zhongfly 构建（均带 libsrt）。macOS 见 §10。
- 远程控制开着时，应用另外观察 mpv 的 `mouse-pos` 和 `osd-dimensions`，并让 mpv 把按键转给自己（§9）。
- 观看中能直接改的：解码方式（`set_property hwdec`，同时改掉计划里的 `--hwdec=`，之后重开的 mpv 也用它）、静音（`mute`，同样写进计划，
  延迟变了重新加入时保留，停止观看后复位）；延迟要重新加入（§3.3）。
- 配置：`%APPDATA%\HdrCast\receiver.json`。

### 3.2 画面守卫：不让受损的画面上屏（0.3.2 起）

**问题**。SRT 在延迟窗口内重发丢掉的包；窗口过了还没补上的，接收端跳过去，流里就少了 7 个 TS 包。HEVC 解码器察觉不到：受损的那一帧照样解出来，
后面所有参考它的帧（到下一个关键帧为止）都跟着花，mpv 全都显示出来——2026-10-23 中国朋友看到的大片色块就是这个。
FFmpeg 的 HEVC 解码器把带 `AV_PKT_FLAG_CORRUPT` 的包当完好的解（libavcodec/hevc 不看这个标志），mpv 也不看。
Moonlight 的做法是：一帧不完整就丢掉，之后的帧也全丢，直到收到关键帧（它还会马上向主播要 IDR）。

**做法**（`crates/media/src/receiver/ts_guard.rs`，`TsGuard`）。播放方式为 ffmpeg 管道时（§3.1，默认），ffmpeg 把 SRT 的载荷原样交出来，
`mpv_runner` 把它经过守卫写进 mpv 的 stdin：

- 读 PAT / PMT 找到 HEVC 视频（stream_type 0x24；别的编码原样放过），按 PID 检查**连续计数器**；视频的每个 PES（一帧）先扣住，下一帧开始时再决定。
  再用**解码时间戳**检查整帧丢失（相邻两帧的 DTS 差超过 1.5 帧；计数器正好差 16 的倍数时只有它看得出来）。
- 完好的帧照原样放行。受损的帧，以及受损的**参考帧**之后、下一个关键帧之前的所有帧：把它们的 slice 的 NAL 类型改成 **RASL_N**，
  并把缺口之后的起始码破坏掉（`00 00 01` → `00 00 03`，后面的字节留在前一个 slice 里，不会被读）。解码器读完一个完好的 slice 头就跳过整帧：
  IDR 之后的 RASL 帧按标准就是不输出的。时间戳都还在，所以 mpv 看到的是一路正常的流，只是有些帧不出图像，画面停在最后一帧完好的画面上，声音照常。
  **不能直接把帧拿掉**：实测 mpv 发现视频队列空了就暂停声音缓冲（"Enter buffering"），拿掉 2 s 的帧就卡 0.5 s。
- 受损的帧没有完好的 slice 头（缺口在它前 128 字节以内），或受损的是关键帧本身：整帧拿掉（只空一帧，没关系）。
  只有不被参考的帧（NAL 类型为偶数的 TRAIL_N 等，NVENC 的 P B B 里每三帧一个）受损时，只藏这一帧，画面不停。
- 第一个完好的关键帧结束停帧。恢复点是 CRA 时改成 BLA_W_LP，它的前导帧（参考了丢掉的帧）也被跳过；流开头第一个关键帧之前的帧都不放行
  （观众中途加入时本来就看不到）。上一个关键帧是流中间没改过的 CRA 时（开放 GOP，libx265 默认），解码器会解 RASL 帧，改类型藏不住，
  这时受损的帧整帧拿掉。
- 视频 PID 的连续计数器在输出里重新编号，mpv 不会再报 "Packet corrupt"。音频和表照原样立即放行（视频最多晚一帧，按时间戳播放没有影响）。
- 日志：第一次停帧时写一行 `[guard] part of the stream was lost on the way: …`，之后最多每 10 s 一行汇总（停了几次、一共多久、拦下多少帧）。
  观看页上显示"传输丢失 n 次 · 已拦下 n 帧受损画面 · 定格 n 次"，停帧时显示正在等关键帧。

**验证**（`crates/p2p/tests/lossy_path_tests.rs`，真 libx265 + libsrt 走真的 HostLink / ViewerLink）：观众这边把 1/150 的首发数据包连同它们的所有重发都丢掉，
每一个都补不回来。照原样解码：282 帧里 131 帧是编码器从没产生过的（花屏）；经过守卫：149 帧，每一帧都和编码器的输出逐位相同，停帧 4 次。
对照：只给解码器加 `fflags=+discardcorrupt`（扔掉带损坏标志的包，靠 FFmpeg ≥ 8.0 的"参考帧缺失就不输出"）仍有 31 帧花屏（FFmpeg 9.0.2），
旧版 FFmpeg 更糟（缺失的参考帧用灰色补），所以"mpv 直接播放"保持原样，不加这个选项。mpv 在 d3d11va 硬解下播放经过守卫的流：不缓冲、不报错、不退回软解。

**局限**：缺口内视频包正好 16 的倍数、又没丢整帧时看不出来（极少）；停帧最长一个关键帧间隔（受损的就是关键帧时到下一个）。Moonlight 能在约一个 RTT 内
恢复，是因为它向主播要 IDR（或用 NVENC 的参考帧失效 RFI）。0.3.3 起主播打开"按需关键帧"时也这样做（§2.8、§8.12）：观众在数据还缺在路上时就请求，
停帧一般缩短到零点几秒。

### 3.3 播放器上的悬浮窗（0.3.3 起）

像 Parsec / Moonlight 那样，观看时不用切回 HdrCast 的窗口就能看状态、改观众自己能改的东西（`app/src/overlay.rs`、`ui/overlay.html` + `overlay.js`）。
编码（码率、分辨率、帧率、编码器）是主播的事，在主播的开播页改（§2.7 的"分享中改设置"）；观众这边能改的只有延迟、解码方式和静音。

- **样子**：开始观看就有一个小圆按钮（40 × 40，半透明，鼠标移上去才不透明）停在播放画面右上角；mpv 的窗口出来之前不显示。单击按钮展开成面板（宽 300，
  高度由页面量出来交给后端，`overlay_fit`）：主播名和播放时长；收到的分辨率 / 帧率 / HDR；码率、缓冲、Ping、丢包、丢帧、实际在用的解码器；守卫的统计（§3.2）；
  **延迟**（四个预设 + 数字框，**应用** = 重新加入，下面说明"画面会中断几秒"）；**解码方式**（四个选项，立即生效）；**全屏** / **静音**。
  标题栏的"—"、`Esc`、`Alt+F4` 收起成按钮，键盘交还播放器。播放窗口里的 `Ctrl+Alt+O` 和观看页的 **悬浮窗** 按钮也能展开 / 收起（不抢键盘）。
  始终是深色（画面上不刺眼），只用黑白灰，播放中的红点是唯一的颜色。
- **独立窗口**：Tauri 的第二个窗口（标签 `overlay`），无边框、透明（页面画圆按钮和圆角面板，周围透出画面）、置顶、不进任务栏，开始观看时建好、停止观看时销毁
  （延迟变了重新加入时保留）。它和主窗口用同一个 WebView2 数据目录和启动参数（`window::web_view_profile`）：同一个数据目录的第二个 WebView2 参数不同会建不起来。
  权限里加了 `overlay` 窗口（`capabilities/default.json`）。命令都是 async 的：布局要等主线程（`monitor_from_point`、`hwnd()`），在主线程上做会死锁，
  `CloseRequested`（`Alt+F4`）也是另开线程去收起。
- **不抢键盘**：显示不经过窗口库。tao 的 `show()` 用 `SW_SHOW`，会把窗口提到前台、拿走键盘（远程控制时键盘本该给主播）；`focused(false)` 只管创建后的第一次显示。
  所以窗口建成隐藏，之后一律 `ShowWindowAsync(SW_SHOWNOACTIVATE / SW_HIDE)`，位置和大小用 `SetWindowPos(HWND_TOPMOST, SWP_NOACTIVATE)` 一次设好（不会先变大小再挪位置）。
  tao 以为窗口一直是隐藏的：它只在自己的窗口标志真的变了时才重新 `ShowWindow`，悬浮窗建好后就不再改标志（置顶、边框等创建时定好），所以不会打架。
  按钮和面板被点击时照常成为前台（面板里有数字框）；收起、以及拖完按钮后把前台还给 mpv（`SetForegroundWindow`，这时 HdrCast 在前台，Windows 允许交出去），
  全屏的播放器也就重新盖住任务栏。观看页的 **全屏** 按钮之后也这样做。
- **跟着播放器**：每 100 ms 一次（`overlay` 线程，和命令共用一把锁）：按 mpv 的进程号枚举窗口，取最大的可见窗口，用它的客户区（`GetClientRect` + `ClientToScreen`，
  最小化时算没有）。按钮的位置存成在画面上的比例 `Spot { x, y }`（0～1，去掉两边 12 px 的边距和按钮本身；默认 1, 0 = 右上角），所以播放器移动、改大小、
  进出全屏时按钮按同样的比例跟过去。面板从按钮朝画面中间展开：按钮在右半边就以按钮的右边为准向左展开，在下半边就以按钮的下边为准向上展开，并且不超出屏幕的
  工作区。新建的窗口第一次挪动之前报的边框不准（实测：按带标题栏的边框算，按钮变成 82 × 116），所以每次都核对窗口实际在不在该在的地方，不在就再放一次
  （每次布局最多 5 次，不跟系统较劲）。缩放按目标屏幕算（150 % 时按钮 60 px）。
- **拖动**：按钮按下后移动超过 3 px 就交给系统拖（`startDragging`），没动就是单击；面板拖标题栏（`data-tauri-drag-region`）。系统的拖动循环在跑时
  （`GetGUIThreadInfo` 的 `GUI_INMOVESIZE`，且 `hwndMoveSize` 是悬浮窗）不去动它；松手后按落点算出新的比例（拖出画面的回到画面边上），存进配置目录的
  `overlay.json`，下次观看还在那里。拖的是面板时，按钮跟着面板走：每个方向取面板远的一边（右 / 下）或近的一边，看哪个与按钮落到的半边一致；
  面板横跨中线时两边都一致，保持原来展开的方向，免得收起后按钮跳到另一个角。
- **只在播放器前面**：只有前台窗口是 mpv、HdrCast 自己的窗口（或它们拥有的弹出窗口，`GA_ROOTOWNER`）时才显示，切到别的程序就藏起来，切回来再出现；
  前台一时没有窗口（切换过程中）时保持原样。没有 mpv 窗口时（连接中、重新加入、最小化）按钮不显示；展开的面板留在原处，好看到"正在重新连接"。
- **能改的东西**：解码方式和静音见 §3.1；延迟（`watch_set_latency`）保存后 `watch::rejoin`：结束当前会话、用同一个主播重新 `start`（新的延迟在加入时谈好，
  主播三分钟内自动放行刚走的观众），悬浮窗不关，面板在这几秒里显示"正在重新连接…"。设置页里改硬解也立即生效；两边的改动通过 `receiver` 事件互相同步，
  主窗口之后保存设置时不会把悬浮窗改的值冲掉。
- **macOS**：还没有找 mpv 窗口和判断前台的实现：按钮放在 HdrCast 窗口所在屏幕的右上角、观看期间一直显示，不跟着播放器，也不记拖动的位置（未在真机上运行）。

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
| `crates/capture`（hdrcast-capture） | 采集引擎：平台无关的主循环（节拍、重复帧、统计、音频对齐、管道写线程）+ 后端（`win/`：DXGI / D3D11 / WASAPI；`mac/`：ScreenCaptureKit；其余：桩）；控制模式观众放声音（`playback`） |
| `crates/media`（hdrcast-media） | 编码器档位、ffmpeg 命令、`-progress` 解析、编码器探测、`sender.json`；mpv 命令、IPC 消息、日志分类、`receiver.json`；控制模式的分片 / 纠错 / 加密 / 声音包（`low_latency`，§11） |
| `crates/control`（hdrcast-control） | 远程控制：报文、按键名、输入注入（§9） |
| `app`（hdrcast，产物 `HdrCast`） | Tauri 2 应用：窗口、命令、采集 + ffmpeg 的编排（`sender_runner`）、mpv 的编排（`mpv_runner`、`mpv_ipc`）、远程控制两端的粘合、控制模式的两端（`low_latency_host`、`low_latency_viewer`） |

- `build.sh`：`cargo test --workspace` + 信令服务器的 Python 测试 → Windows：`cargo build --release -p hdrcast`，装配 `dist/HdrCast/`
  （`HdrCast.exe`、`tools/`、`scripts/`、`docs/`、`signal/`、README）并打成 `dist/HdrCast-<版本>.zip`；macOS：`cargo tauri build` 出 `HdrCast.app` 和 dmg。
  zip 里的文件夹名不带版本号：防火墙规则（§8.7）和登录时启动的 Run 项（§2.7）认的都是 exe 的完整路径，新版本解压覆盖旧文件夹，两者都不用重做。
- 配置存 `%APPDATA%\HdrCast\`（macOS：`~/Library/Application Support/HdrCast/`）；日志存其下的 `logs/hdrcast-yyyyMMdd.log`。
- 界面不经打包器：`node scripts/ui-preview.mjs` 可以在浏览器里单独打开它（`ui/mock.js` 扮演后端）。
- 调试用的环境变量：`HDRCAST_DATA_ROOT`（另一份配置）、`HDRCAST_NO_ROUTER=1`（不做 UPnP）、`HDRCAST_CONTROL_DRY_RUN=1`（远程控制只写日志）、
  `HDRCAST_SIMULATE_LOSS=<秒>[:<包数>[:<百分比>]]`（观众的链路每隔这么多秒把接下来的这么多个 SRT 数据包连同它们的全部重发丢掉，默认 20 个；单机试按需关键帧用。
  控制模式（§11）的画面和声音数据报同样每隔这么多秒连丢这么多个，第三个数是另外随机丢掉的百分比）、
  `HDRCAST_NO_ZERO_COPY=1`（主播端引擎编码时不用零拷贝，帧经内存交给 NVENC，§2.8）、调试构建里的 `HDRCAST_DEVTOOLS_PORT`（把界面暴露给 DevTools 协议，脚本驱动真实窗口用）。

## 7. 验证

- 自动（`cargo test --workspace`，Windows 上跑）：
  - 协议 golden 向量（`tests/vectors/group-v1.json`，Python 测试也读它）；与 **C# 0.2.4 的真实输出**逐字节对照（core：1359 个刁钻输入；
    media：946 个向量，覆盖全部编码器参数表、ffmpeg / mpv 命令行、设置文件）；`group.json` 经真实的 .NET `DataContractJsonSerializer` 往返。
  - 打洞（模拟有状态防火墙、对称型 NAT 两个方向）、假路由器上的 UPnP、真 Python 信令服务器上的全栈（真 libx265 HDR10 编码 + P2pHost + 两个 ViewerSession + ffprobe）。
  - 按需关键帧（0.3.3）：`kf` 只在为 true 时写在最后、不写时消息与 0.3.2 逐字节相同；真信令服务器上 `kf` 只在两边都声明时通行、与 `rc` 互不相干；
    请求 / 回答报文与 MAC；主播的限速（300 ms、推迟、已经发过、定时的马上就来）和观众的重发；引擎的关键帧时钟、命令、事件、TS 封装（PAT / PMT / PES /
    PCR、连续计数器）；真 ffmpeg 读 B 帧 HEVC 的 TS 加声音、`-c:v copy` 出每一帧；`SrtMonitor` 的持久缺口。NVENC 本身在本机用引擎落盘和 ffprobe 核对（§2.8）。
  - 丢包的整条路径（0.3.2）：真 libx265 + remux 子进程 + HostLink / ViewerLink + 以 `-f data` 收流的 ffmpeg，观众这边丢掉一部分首发包和它们的全部重发；
    两端的 `SrtMonitor` 数到的丢失与实际一致，经过守卫解出的每一帧都与编码器的输出逐位相同（§3.2）。守卫本身另有单元测试（构造的 TS：参考帧 / 非参考帧 /
    关键帧受损、整帧丢失而计数器不变、中途加入、CRA 改 BLA、重复包、失步）。
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
- 0.3.3 起的可选字段：`live` 与 `join` 的 `"kf":true`（按需关键帧），见 §8.12。
- 0.3.7 起的可选字段：`live` 与 `join` 的 `"ll":true`（控制模式），见 §11.1。
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
ffmpeg -hide_banner -loglevel warning -probesize 2000000 -analyzeduration 500000 -fflags +discardcorrupt -f mpegts -i pipe:0
       -map 0 -c copy -f mpegts -mpegts_flags +resend_headers -muxdelay 0 -muxpreload 0
       "srt://127.0.0.1:<Y>?mode=listener&passphrase=…&pbkeylen=32&latency=<观众要求>&maxbw=0&inputbw=…&oheadbw=<观众要求>&…"
```

- `-probesize 2000000`（0.3.2 起，原 65536）：子进程从流中间开始读，探测时必须看到一个 PMT（每 0.1 s 一个），2 MB 在 160 Mbps 以内都够，
  0.5 s 的 `analyzeduration` 照样封顶。64 KB 时，在两个 PMT 之间启动的子进程认不出音频是什么，报 "Cannot map stream #0:1" 退出：
  40 Mbps 下 30 次中途启动失败 13 次（2026-10-23 真实日志里中国朋友的一次加入就是这样），2 MB 时 0 次。
- `-fflags +discardcorrupt`（0.3.2 起）：分发器队列溢出时被截断的画面在这里扔掉。重新封装后连续计数器是新的，观众端的守卫（§3.2）看不出它残缺，
  只能从之后的时间戳空缺发现丢了帧，那时这一帧已经上屏。

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
- 0.3.3 起旁路通道还有第二种用途，按需关键帧（§8.12），标志是 `kf`，与 `rc` 各管各的：只向声明了**这种用途**的对端发这种数据报
  （`P2pHost::send_side_for(sid, SideUse, …)`、`ViewerSession::send_side_for(SideUse, …)`）。两种载荷靠第一个字节区分：远程控制是方向字节 1 / 2，
  按需关键帧是 `K` / `k`。0.3.7 起第三种用途是控制模式（§11.3），标志 `ll`，第一个字节 `V`（画面分片）、`A`（声音）、`R`（请求和报告）、`r`（回答），
  只在 `join` 带 `ll` 的会话里收发（`SideUse::LowLatency`）。这样的会话没有 remux 子进程（`HostLink::add_direct_session`），
  一帧的数据报一次交给链路（`HostLink::send_side_many`）。
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
- 最后一个观众离开 20 s 后停止采集和编码。"全部断开" / "停止"会断开所有观众，各自一分钟内不能重连。
- 开播期间，看过这次直播的人都离开 2 s 后直播自动结束（`sender.json` 的 `endWhenViewersLeave`，开播页 **观众** 一节的
  **观众都离开后结束直播**，默认开；关掉则和以前一样，没人看也一直播）。结束和点 **停止** 一样：去掉 `live`、停掉采集和编码器、
  回到在线（关着开放连接时下线），只是不断开任何人。只算路径真正连通过的观众（`ViewerState::Watching`）：开播后还没人看过、
  或者来的人始终没连通时不会结束。只等 2 s（`UNWATCHED_END`），不像按需那样等 20 s：结束后再来的观众照上面的规则进来，
  3 分钟内放行过（或看过）的不用再问，只是编码器要重新启动（本机实测：重新加入 1.1 s 后主播看到人，4.2 s 出画面）。
  路径不到 15 s 没包不算断，短的网络抖动不会让列表变空；在悬浮窗里改延迟是先离开再加入，本机实测列表没有空过，
  信令慢、超过 2 s 才加入时直播先结束，观众照样按需回来。代价：关着开放连接时结束就是下线，晚回来的观众连不回来。
  有人连进来（包括正在放行、正在打洞的）就重新计时。housekeeping 每 400 ms 看一次，到时在 `starting`
  锁里再确认一遍没人回来、还是同一次直播（用户可能已经停止又重新开始）才结束（`share.rs` 的 `end_unwatched`）。
  "正在观看"里显示倒计时（状态里的 `endsInSec`）；结束后页面提示一次（`endedUnwatched` 计数加一），日志
  `everyone who watched has left: the broadcast ended`。
- 码率、人数上限、ffmpeg 路径可以在运行中改（`P2pHost::set_stream` / `set_ffmpeg_path`，设置一变就生效），`announce_now` 让下一次心跳马上发出。

### 8.11 SRT 链路统计（0.3.2 起）

两端的转发器（`HostLink`、`ViewerLink`）把经过的每个 SRT 包交给 `SrtMonitor`（`crates/p2p/src/srt_monitor.rs`）。SRT 只加密载荷，16 字节的包头是明文，
读包头不改动任何字节、也不需要密钥：

- 数据包：序号（31 位，回绕）；序号落在已见最大序号之后是首发，否则是重发（旧版本不设 R 位，所以不靠它）。
- ACK（类型 2）：下一个期望的序号、RTT 和它的方差（µs）、空闲缓冲、接收速率（包/s）、链路容量估计（包/s）、接收速率（字节/s）；轻 ACK 只有第一项。
- NAK（类型 3）：丢失的序号，区间写成 `first | 0x80000000, last`。DROPREQ（类型 7）：发送端放弃的首尾序号。握手（类型 0）：新连接，序号重新开始。

统计（各自去重，按序号记在 2^17 位的窗口里）：首发、重发、接收端报告丢失、发送端放弃（来不及重发）、**补不回来**（接收端的 ACK 越过了一个从没到过的序号：
只有观众端看得到，就是让画面受损的那些）、RTT、链路容量估计；最近约 10 s 另算一份。ACK 里的接收速率是最近几个包的突发速率，不是流的码率，不拿来显示。

- 主播：每位观众一份。**开播** 页"正在观看"的每一行显示"途中丢包 x %"和"n 个包来不及补发"；日志里开始 15 s 后写一行，之后丢包时每 30 s 一行：
  `[net] <名字>, last 30 s: 43.6 Mbps sent, 2.10% lost on the way, 2.31% sent again, 0 too late to send again; round trip 92 ms; …`。
  分发器给这位观众的队列被清空时（它的 remux 子进程跟不上，半秒的流在发出前就丢了）也写一行。
- 观众：**观看** 页码率一格下面显示"丢包 x %"；日志同样格式：`[net] last 30 s: … Mbps arrived, …% lost on the way, n never recovered in time; …`。
- 这些数字是排查"画面花了 / 卡了"的依据：丢包率高而补不回来的少 → 延迟够；补不回来的多 → 加延迟或降码率；主播端"来不及重发"多 → 主播上行或码率的问题。
  `lossy_path_tests` 用真 libsrt 核对：两端数到的丢失与实际丢掉的包数一致，观众端"补不回来"的数目也一致。

### 8.12 按需关键帧（0.3.3 起）

观众丢了补不回来的数据时，画面停在最后一帧完好的画面上，直到下一个关键帧（§3.2）。主播的编码在采集引擎里时（§2.8）可以马上补一个关键帧。

**缓冲直播里什么时候要**。Moonlight 没有缓冲，丢了就是丢了，马上要 IDR，一个 RTT 后就恢复。这里观众比主播晚一个 SRT latency（1.5～3.5 s）播放：守卫
在播放时才看到受损的画面，那时数据离开主播已经一个 latency；这时要来的关键帧在流里比受损的画面晚一个 latency，而关键帧每秒一个，观众的缓冲里早就有
定时的关键帧了。所以要在数据**还缺在路上**时要，而不是等画面停住：

1. 观众的 `ViewerLink` 让 `SrtMonitor` 记下 SRT 数据序号里的缺口；一个缺口过了 `2 × (RTT + 4 × RTT 方差)`（SRT 已经有时间要两次重发；至少 60 ms）
   还没补上，就请求（`Reason::Missing`，"离开主播大约是 缺口的年龄 + 半个 RTT 之前"）。这时离画面停住还有将近一个 latency；新关键帧在流里只比受损的
   画面晚"发现的时间 + 半个 RTT + 主播的处理"。之后 SRT 还是补上了的话，这个关键帧就是多余的（随机丢包要连丢两轮重发才会这样，很少）。
2. 守卫在播放时停帧（`GuardEvent::Froze`）也请求（`Reason::Froze`，"一个 latency 之前"）。这时一般已经有关键帧在路上，主播回答"丢失之后已经发过"；
   latency 比关键帧间隔短（"快速" 300 ms）时它才起作用。
3. 主播自己也会要，不用观众开口，旧版本的观众也受益：某位观众的 SRT 流开始时（加入、播放器重连：`HostLink` 看到新连接的第一个数据包），以及它在分发器
   里的队列满了被清空时（只在它的流开始之后：之前 remux 子进程在等播放器，不读，队列每半秒满一次）。

**报文**（旁路通道的载荷，26 字节；`hdrcast_media::keyframe`）：

```
偏移 长度
 0   1   种类  'K'（0x4B）请求，观众 → 主播；'k'（0x6B）回答，主播 → 观众
 1   4   seq   观众在这个会话里的请求号，从 1 起（大端）；回答照抄
 5   4   ms    请求：丢掉的数据大约多久之前离开主播（观众的估计）；回答：起作用的那个关键帧多久之前发出 / 多久之后发出
 9   1   why   请求：1 守卫停帧 / 2 数据还缺在路上；回答：1 正在做 / 2 丢失之后已经发过 / 3 定时的马上就来 / 4 推迟 ms 后做 / 5 做不到（ffmpeg 在编码）
10  16   mac   HMAC-SHA256(keyframeKey(sid), 字节 0..9) 的前 16 字节；keyframeKey(sid) = sub("kf/" + sid)（§8.3）
```

**主播怎么决定**（`KeyframeGovernor`，一路编码给所有观众，每个关键帧对每个人都是一个大画面）：

- 丢失之后已经发出过关键帧（定时的，或者别人要的）→ 回答 2，不做。
- 定时的关键帧 150 ms 内就到（以它最近一次报告的关键帧加一个间隔计；迟了不到 50 ms 也算）→ 回答 3。
- 离上一次被要求的关键帧不到 300 ms → 推迟到满 300 ms，回答 4；届时如果已经有关键帧在所有这些丢失之后发出，就不做。
- 否则马上做（`keyframe` 命令给引擎），回答 1。编码器不在引擎里（开关关着、不是 NVENC、引擎退回了 ffmpeg）→ 回答 5。
- 主播按会话记着最后一个请求号和它的回答：同一个请求再来（回答丢了）照原样再答一次，更旧的忽略。

**观众**（`KeyframeAsker`）：同时只有一个请求，50 ms 没有回答就再发同一个（时间更新），最多 5 次；请求在途时又丢了，等这个了结后再问
（前一个的回答不一定盖得住后一次丢失）。

**能力标志**：`live` 的 `"kf":true` = 这个主播接受请求（设置打开、编码器是 NVENC；按每次心跳时的设置），`join` 的 `"kf":true` = 这个观众会请求、懂回答。
都只在为 true 时写，写在最后（`live`：`…[,"dk":…][,"od":true][,"kf":true]`；`join`：`…[,"rc":true][,"dk","ek","ds"][,"kf":true]`）；缺省 = 否，
不带 `kf` 的消息与 0.3.2 逐字节相同。主播只给 join 带 `kf` 的会话发回答，观众只向 live 带 `kf` 的主播发请求（以本次加入时读到的为准）。
0.3.2 及更早：它们的 live 没有 `kf`，新观众不会去问；它们的 join 没有 `kf`，新主播不给它们回答，但第 3 种情况主播自己做的关键帧它们照样用上。

**实测**（单机回环，2026-10-24；观众 latency 1500 ms，`HDRCAST_SIMULATE_LOSS=4:30`；1080p60 HDR 20 Mbps，桌面）：

- 观众的链路丢掉 30 个包后 61～76 ms 发出请求，主播立刻回答"正在做"，7～22 ms 后关键帧写进管道：**从模拟的丢包到强制的 IDR 离开编码器 72～99 ms**
  （之后经 ffmpeg、分发器、remux 子进程到观众的链路，本机上再几毫秒）。
- 1.5 s 后守卫在播放时停帧，又问一次，主播回答"丢失之后已经发过"（约 0.4～0.5 s 前）。
- 画面停住的时间（流内时间，从受损的画面到关键帧）166～183 ms（10～11 帧）。同样的丢包、开关关着：每次 1.0～1.5 s（例如 3 次共 3.0 s、180 帧），
  这段桌面画面很静、P 帧很小，30 个包就跨过了半秒，常常连关键帧一起丢。
- 4K60 HDR 40 Mbps（每次 60 个包）：请求 61～109 ms，关键帧 20～42 ms 后发出，停住 200～233 ms；采集和封装都是 60 fps、不丢帧。
- 丢失持续得久一些时（另一次同样的设置，画面里东西多些，这 30 个包分散在 0.1～0.2 s 里），先要来的关键帧也可能丢在里面：之后的缺口再请求，被 300 ms 的
  限速推迟到第一个关键帧之后 300 ms，那一个补上，这样的一次停 0.5 s 左右（516～533 ms）；丢失短的照旧 166～233 ms。

**局限**：发现要等 `2 × (RTT + …)`：中国的线路（RTT 约 100 ms）上约 250～300 ms，停帧大约 0.4 s，而以前平均 0.5 s、最长 1 s 以上（丢失跨过关键帧时）。
拥塞造成的丢包（带宽不够）时关键帧本身又加码率，300 ms 的限速只能限制它；那时真正需要的是降码率（§2.8 的"自适应码率"）。

## 9. 远程控制（0.3.0 起）

观众可以请求使用主播的鼠标和键盘。输入走 §8.9 的旁路通道，所以只有双方都是 0.3+ 才有。观看模式下画面延迟还是 SRT 延迟 + 播放器缓冲，
这是"帮忙点一下"的功能；0.3.7 起观众可以选 **控制模式**（§11）：不缓冲的低延迟流，像远程桌面那样用。

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
  这个段只在主播允许期间启用（拖拽不会拖动播放器窗口：`--window-dragging=no` 一直开着，§3.1）；`Ctrl+Alt+ENTER`（本地切全屏）和 `Ctrl+Alt+O`
  （悬浮窗，§3.3）留给播放器自己，`Esc` 和其余的键都给主播。窗口的关闭按钮不是键，控制时也是停止观看。
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
- 局限：Windows 上以管理员身份运行的窗口不接受主播应用自己的注入，要打开设置里的"控制以管理员身份运行的程序"（§9.1）；
  UAC 提示和锁屏连服务也进不去（有意的，见 §9.1）；Win 键、`Alt+Tab`、`Ctrl+Alt+Del` 在观众自己的系统里就被拦下了；没有相对鼠标（不适合 FPS）。
- 主播和观众在同一台电脑上试的时候，真注入会让播放窗口里的指针反过来驱动自己：设 `HDRCAST_CONTROL_DRY_RUN=1`，主播端只把收到的动作写进日志
  （不写是哪个字符键，日志是要发给别人看的）。

### 9.1 输入服务：控制以管理员身份运行的窗口（0.3.6 起，Windows）

**问题**。Windows 的 UIPI（用户界面特权隔离，UAC 的一部分）只让完整性级别不低于目标窗口的进程向它发输入。HdrCast 以登录用户的"中"级别运行；
前台窗口属于以管理员身份启动的程序（"高"，比如以管理员运行的 Claude 桌面版）时，`SendInput` 照样返回成功，输入被系统悄悄丢掉
（量过：指针不动，`SetCursorPos` 失败，`ERROR_ACCESS_DENIED`），于是"远程控制没反应"。Parsec、Sunshine（和它的分支）没有这个问题，
因为它们的安装程序注册了 Windows 服务，让输入进程以 SYSTEM 身份跑在用户的会话里，级别比谁都高。

**做法**（`crates/control/src/elevated/`、`src/bin/hdrcast_input.rs`）：同样的结构，三部分，都在一个 `HdrCastInput.exe` 里。
（服务本身待在 session 0，碰不到用户的桌面，所以"服务 + 它在用户会话里启动的输入进程"这两个进程是躲不掉的；Sunshine 是 `sunshinesvc.exe` + `sunshine.exe`，Parsec 是 `pservice` + `parsecd`。）

- **服务**（`HdrCastInput service --user <SID>`，`service.rs`）。LocalSystem，**按需启动**（`SERVICE_DEMAND_START`，不是开机自启）；它只做一件事：在用户的会话里用自己的令牌启动输入进程，输入进程走了它就停。
  - 找会话：`WTSEnumerateSessionsW` 找状态为 Active 的会话，`WTSQueryUserToken` 取各会话用户的 SID，和注册服务时写进命令行的 `--user` 比；找不到（用户没登录）就停并写日志。远程桌面登录的用户也行。
  - 令牌：复制自己的 SYSTEM 令牌（`DuplicateTokenEx`，主令牌），`SetTokenInformation(TokenSessionId)` 改到那个会话（Sunshine 也这样做；要 SeTcb，LocalSystem 有，进来先把 SeTcb / SeAssignPrimaryToken / SeIncreaseQuota 打开，它们平时是关着的），
    再 `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)` 去掉全部特权：输入进程要的是**级别**（UIPI 比的是完整性级别），不是特权；万一它有漏洞，拿到的尽量少。
  - 启动：`CreateProcessAsUserW`，`lpDesktop = winsta0\default`，不要窗口，命令行 `"<exe>" serve --user <SID> --stop-event <名>`。
  - 停：Windows 要求停止（卸载、关机）时，服务置位一个命名事件（`Global\HdrCastInputStop-…`，访问控制只给 SYSTEM），输入进程听到后先断开所有客户端、松开他们按住的键再退出；4 s 内没走才 `TerminateProcess`。
    输入进程自己退出（最后一个观众走了 45 s）时服务随之停止。
- **输入进程**（`HdrCastInput serve`，`server.rs`）。以 SYSTEM（无特权）跑在用户会话里，监听 `\\.\pipe\hdrcast-input-<协议号>-<用户 SID>`，单向（应用→输入进程）。`FILE_FLAG_FIRST_PIPE_INSTANCE`：已经有一个在听就退出。
  访问控制（SDDL）：SYSTEM 和对象所有者全权；用户只有"写数据 + 读属性"（`0x100082`，所以用户的普通程序开不了管道的第二个实例）；完整性标签"中、禁止低写"（低完整性的沙箱进不来）。
  总留着一个空闲实例，免得第二个客户端撞上"全忙"。每个连接一个线程、一个 `Replay`：必须先收到 `Hello`；记着按住的键、鼠标键和修饰键，连接断了（应用崩溃也一样）就全部松开；
  协议外的字节、同时按住超过 256 个键都直接断开。进程是每显示器 DPI 感知的，指针坐标是物理像素。手动运行 `HdrCastInput serve`（调试用）服务的是运行它的用户。
  **不跟着切到安全桌面**（UAC 提示、锁屏），见下。
- **客户端**（`client.rs`，在 HdrCast 里）。观众拿到控制权时（`HostControl` 第一次 `granted`，调用 `Injector::activate(true)`；收回、观众离开时是 `false`），`ElevatedInjector` 在后台线程里：
  管道不在就 `StartService`（服务的访问控制让注册它的那个用户启动，普通权限也行，不弹 UAC；服务正在停时每 300 ms 再试），最多等 5 s，连上管道，先发 `Hello`（共享显示器的矩形），之后每个动作一帧。
  连上之前的头几百毫秒、没装服务、应用自己已经是管理员时，照旧自己 `SendInput`（键盘状态是系统的，不是哪个程序的，中途换路不会卡键）；
  输入进程中途没了（写管道失败）就切回自己注入并每 5 s 再试一次，日志里说一声（同一个问题只说一次，直到它又好了）。
- **设置**（设置 → 通用 → **控制以管理员身份运行的程序**，Windows 的权限确认一次）。应用像 `firewall-allow` 那样（`ShellExecuteEx` 的 `runas`，等退出码）以管理员身份启动自己旁边的
  `HdrCastInput.exe install --user <SID>`。它把自己复制到 `%ProgramFiles%\HdrCast\Input\`（路径用 `SHGetKnownFolderPath` 取，不信环境变量；**以 SYSTEM 运行的文件必须是普通程序改不了的**），写下协议号，
  向服务管理器注册服务 `HdrCastInput`（`CreateServiceW`：LocalSystem、按需启动、命令行里程序带引号），并设服务的访问控制：SYSTEM 和管理员照默认，装它的这个用户只能**查询和启动**，不能停止、修改、删除。
  关掉开关就是同样的 `uninstall`（停服务、删服务、删这两个文件和空文件夹；用应用旁边那份来跑，装好的那份删不掉自己）。服务管理器是直接调 API 问的，不看任何工具的输出，所以和系统语言无关。
- **协议**（`elevated::Frame`）：1 字节标签 + 定长字段，小端：`Hello`（显示器 x、y、宽、高，各 f64）、`Pointer`（x、y，u16）、`Button`（键、按下）、`Wheel`（轴、i16）、
  `Modifier`（位、按下）、`Key`（`Key::code()`，u32；按下）。协议号写在管道名里：应用和输入进程不一致时互相看不见；设置页拿装好时写下的协议号和这一版比，
  不一致就显示"重新设置"。
- **信任**。输入进程听从"这个用户自己的进程"写进管道的输入，不再多问：等于允许这个用户的普通程序越过 UAC 去操作管理员窗口（微软不把 UAC 当安全边界；Parsec 的服务也是同一个交换）。
  它能做的只有输入；只在有人被授予控制时才会被启动（服务按需启动），45 s 后自己退出；它是 SYSTEM，所以：程序放在 Program Files、令牌去掉全部特权、不碰安全桌面。设置页默认关着。
- **不碰安全桌面是有意的**。要操作 UAC 提示和锁屏，输入进程得每次发输入前先切到当前的输入桌面（Sunshine 就是这么做的）；那样用户的任何程序都能经管道替 UAC 提示按"是"。
  而且 HdrCast 的采集在应用进程里，采不到那张桌面，观众也看不见提示，所以收益是零。真要做，得连采集一起改成跟着桌面切换。
- **没装服务时**。`ElevatedInjector` 每 2 s 用 `SetCursorPos`（设到指针当前位置，不会动）看一次系统是不是在拒绝（前台是管理员窗口、屏幕锁着、权限提示在前面时它会失败；
  `SendInput` 不会，它永远报成功），在拒绝就在日志里说一次（至少隔 120 s），并指向设置页的开关。试过不能靠读前台窗口的令牌来判断：普通权限读不了管理员进程的令牌。
- **杀毒软件**。一个没签名的程序注册 SYSTEM 服务、复制令牌、在别的会话里起进程，启发式扫描可能不喜欢（曾经有一次，测试里把一个 exe 写进"登录时启动"，Defender 就按行为把那个 exe 的哈希标成了可疑，连发行版 exe 也读不出来了）。
  Sunshine、Parsec 都签了名；HdrCast 没有，所以第一次点开关时留意 Defender 的提示。
- **验证过的**（2026-10-29，本机，没有注册任何服务）：
  - 输入进程那一层：手动在管理员 shell 里起它（高级别），普通权限的探针（`crates/control/examples/inject_probe.rs`，无窗口，`explorer.exe inject_probe.exe` 启动才是普通级别）对着前台的管理员窗口：
    自己注入，指针不动、按键 0 个到达；经输入进程，指针精确到位，`hdrcast` 七个字符打进了管理员窗口的文本框。
  - 协议、`Replay`、管道服务端和客户端（多客户端、坏字节、重复启动、输入进程不在、连接途中收回控制、停止时松开客户端按住的键）有单元测试，管理员和普通权限下都跑过。
  - 服务里能单独拿出来试的部分：令牌复制 + 去特权后只剩 SeChangeNotifyPrivilege，用户和级别不变；用这样的令牌 `CreateProcessAsUserW` 启动 `cmd.exe /c exit 7` 拿到退出码 7；会话枚举找得到活动会话；
    把需要的特权打开；真的 `HdrCastInput serve --stop-event` 在事件被置位后以 0 退出；手动运行 `service` 会说"要由 Windows 当服务运行"（`tests/input_program.rs`、`cargo test -p hdrcast-control -- --ignored`）。
  - **没验证过**：注册服务、普通权限 `StartService`（服务访问控制）、服务管理器的状态流程、`WTSQueryUserToken` 按 SID 找会话、把 SYSTEM 令牌移到用户会话后真的在那里起进程并注入、停止时的优雅退出、UAC 流程——都要真的装一个服务，见 windows-checklist §9。

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
| 工具在 `tools\` | Homebrew：`/opt/homebrew/bin`、`/usr/local/bin`（`brew install ffmpeg mpv`），或应用包里的 `Resources/tools` |
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

## 11. 控制模式：为远程控制的超低延迟（0.3.7 起）

观看（§2、§3）是缓冲直播：SRT 的延迟窗口（0.3～3.5 s）给丢包重发留时间，mpv 再缓冲 1.5 s，画面又稳又清楚，跨境也行，但比现场晚好几秒。
远程控制要的是反过来：鼠标一动画面就跟上，丢一点画质可以，晚不行。所以控制模式走 Moonlight 和 Sunshine / Vibepollo 的路线：**不缓冲、不重发**，
每一帧采到就编码，编码完就发，收齐就显示；丢了的帧用前向纠错补，补不回来就停在上一帧、马上要关键帧。

### 11.1 两种模式怎么选

- 观众在 **观看** 页选 **模式**：**观看**（以前的方式）/ **控制**（`receiver.json` 的 `controlMode`，默认观看）。选了控制时延迟预设不显示（用不上），
  提供控制模式的电脑在在线列表里带"控制模式"标记，按钮是 **控制**；不提供的（旧版本、没有 NVIDIA 显卡、主播关掉了）照常观看，日志里说一句。
- 主播在 **开播** 页的 **控制模式** 一节：**提供控制模式**（默认开）、码率（默认 30 Mbps）、最高帧率（默认 60）。画面的其余部分（显示器、分辨率、HDR、
  指针、声音）跟开播的设置一样（`sender.json` 的 `controlMode`、`controlBitrateMbps`、`controlFps`）。只有这台电脑有 NVIDIA 显卡（看显示器所在适配器的名字）
  而且引擎没有报告编不了（退出码 7）时才提供；编不了时这一次运行里不再提供，页面上说明。
- 线上协议仍是 v1：`live` 的 `"ll":true` = 这个主播提供控制模式；`join` 的 `"ll":true` = 这个观众要控制模式（只发给 live 带 `ll` 的主播）。都只在为 true 时写，
  写在最后（`live`：`…[,"kf":true][,"ll":true]`；`join`：`…[,"kf":true][,"ll":true]`），不带它们的消息与 0.3.6 逐字节相同。
  主播收到带 `ll` 的 join 而此刻不提供（刚关掉）时回答 `busy`，观众过一会儿再读 live 记录，没有 `ll` 就按观看加入。
- 权限照旧（§8.10、§9）：观看规则决定能不能连，控制规则决定鼠标键盘。控制模式只是另一路流；被设成"不允许控制"的电脑选了控制模式也能连上，
  看到的是低延迟的画面，控制一行照常说明不能控制。
- 两种观众可以同时在：控制模式有自己的采集引擎和编码器（下面），和观看用的那一路并排跑，互不影响。

### 11.2 主播：按 Vibepollo 的方式编码

控制模式的采集引擎是另一个 `HdrCast capture` 进程，参数 `--encode nvenc --low-latency`（`EncodeArgs::low_latency`），编码设置照 Vibepollo 的默认值
（`src/nvenc/nvenc_base.cpp`、`nvenc_config.h`）：

| | 观看（§2.8 按需关键帧时） | 控制模式 | Vibepollo 默认 |
|---|---|---|---|
| 预设 / 调优 | P3～P7 / HIGH_QUALITY | **P1 / ULTRA_LOW_LATENCY** | P1 / ULTRA_LOW_LATENCY |
| 码率控制 / VBV | CBR / 0.25 s | CBR / **一帧**（码率 ÷ 帧率） | CBR / 一帧 |
| B 帧 / AQ | 按设置 / 8 | **0 / 关** | 0 / 关 |
| 二次编码 | 按预设 | 四分之一分辨率 | 四分之一分辨率 |
| 关键帧 | 每秒一个 + 被要求的 | **只有第一帧和被要求的**（`--gop 0`） | 只有被要求的 |

- **发帧时机**（`EagerPacer`）。观看用固定节拍：节拍到了才把最新的画面交给编码器，新画面平均要等半个间隔、最多一个。控制模式没有节拍：桌面复制
  一交出新画面（`AcquireNextFrame` 等的就是它）就编码，只是不早于上一帧之后 0.9 个帧间隔（`--fps` 是上限，刷新率正好等于它的显示器不会被拖一帧）。
  画面不变时每 100 ms 把上一帧再编一次（几百字节的 P 帧）：观众借它分清"画面静止"和"丢了一帧"，也借它把要来的关键帧带出来。
  等编码器的队列只有一帧：编码器忙时新画面替掉旧的，编出来的永远是最新的。
- **不能发的时候不采集**。离上一帧不到 0.9 个间隔时引擎干脆不去取桌面（桌面复制会把这段时间里最新的画面留着，到时一取就是最新的）。
  第一版每个桌面画面都转换（360 Hz 的副屏一秒 360 次，只发 120 次）：GPU 和零拷贝时编码器与转换共用的 D3D11 设备（及其锁）一直被占着，
  从采集到写出 7.3 ms（中位，编码器那一段 5.7 ms）；改成到时才取以后是 1.96 ms（1080p120，下面 §11.5 的同一套测法）。
- **GPU 优先级**：控制模式的引擎像 Sunshine / Vibepollo 那样把自己的 GPU 工作排到别的程序前面（`D3DKMTSetProcessSchedulingPriorityClass` 设 HIGH，
  NVIDIA 上 REALTIME 曾让驱动卡死；`IDXGIDevice::SetGPUThreadPriority(7)`），日志里写拿到了什么（Windows 可能不给非管理员的进程）。本机这一项单独量不出差别
  （GPU 不忙），留着给开着游戏的主播。
- **引擎到应用**。视频管道里不是 MPEG-TS，而是一帧一条记录（`encode/frames.rs`）：长度、帧号、标志（关键帧 / 重复 / 被要求的）、从采集到写出的微秒数、
  编码用的微秒数，后面是 HEVC 访问单元（Annex B，关键帧前带 VPS / SPS / PPS）。长度在前，应用读到一帧就知道一帧结束了，不用等下一帧。没有 ffmpeg、
  没有分发器和 remux 子进程。stdin 照旧收 `keyframe` 和 `bitrate`。
- **声音**。引擎的声音管道（f32、48 kHz、立体声）也由应用自己读，切成 5 ms 的 16 bit PCM 包。不压缩（约 1.5 Mbit/s）：不用另起 ffmpeg，
  观众那边也不用解码器。
- 应用（`app/src/low_latency_host.rs`）：第一位控制模式的观众被放进来时启动引擎（等第一帧出来再回答 join，和观看一样），最后一位走了 10 s 后停；
  引擎意外退出 1 s 后重启（帧号接着数，观众不会看到号往回走）。开播页改码率时直接发给运行中的编码器（`nvEncReconfigureEncoder`，不出关键帧），
  改别的（帧率、分辨率……）重启引擎；关掉"提供控制模式"会断开控制模式的观众。

### 11.3 传输：分片、纠错、不重发

控制模式的数据报走旁路通道（§8.9），只发给 join 带 `ll` 的会话（`SideUse::LowLatency`），第一个字节区分用途：`V` 一帧的分片、`A` 声音（主播→观众），
`R` 请求和报告（观众→主播）、`r` 回答（主播→观众）；远程控制是 1 / 2，按需关键帧是 `K` / `k`，不会混。

- **加密**（`low_latency::seal`）：每个数据报 = 种类（1 字节）+ 计数器（8 字节，每个方向从 1 起）+ ChaCha20-Poly1305 密文 + 16 字节标签；
  密钥 = HMAC-SHA256(这个会话的远程控制密钥（§8.10，和观众设备协商出来的那把）, "hdrcast/v1/low-latency")，nonce = 方向 + 计数器，
  附加数据 = "HDRCLL1" + 前 9 字节。接收端记着最近 8192 个计数器，重放和更旧的一律丢掉。组里的其他人读不到画面（观看的画面是组派生口令的 SRT AES）。
- **分片**（`low_latency::video`）：一帧的载荷 = 帧头（帧长、主播从采集到发出的 µs、编码 µs）+ 访问单元，切成大小相等的偶数字节分片，每片最多 1162 字节
  （加上 12 字节的分片头和 25 字节的封装是 1199 字节，在旁路通道 1200 字节的上限里，IPv6 最小 MTU 也装得下）。分片头：帧号、关键帧标志、片号、
  数据片数、校验片数。
- **前向纠错**：校验片 = 数据片的 20 %（Moonlight 的默认值），至少 2 片；Reed-Solomon over GF(2^16)（`reed-solomon-simd`，Leopard 算法，纯 Rust，
  x86-64 和 AArch64 有 SIMD）。一帧一个块，收到任意"数据片数"那么多片就能还原，没有 Moonlight 那种一块最多 255 片、大帧要分 4 块的限制。
  4K 关键帧 2 MB 是 1722 + 345 片。
- **发送**：一帧的数据报分批交给 socket（每批 32 个），批与批之间按 800 Mbit/s 的速度停顿（Sunshine 的 `ratecontrol`：千兆的 80 %）：不是一下子全砸出去，
  也不为一帧拖太久。多位控制模式观众依次发。
- **不重发**。观众（`app/src/low_latency_viewer.rs` + `Assembler`）按帧号收：数据片齐了直接交出，不齐但总片数够了用校验片还原；
  后一帧的分片到了、或者这一帧 50 ms 没有新分片，这一帧就算丢了（一帧的包是连着发的，隔这么久说明剩下的丢在路上了）。
  丢了一帧后面的帧都解不了：画面停在上一帧（什么都不写给 mpv），马上请求关键帧（`R` 1：请求号 + 丢的帧号），之后每 1.5 个往返（20～250 ms）再问一次，
  直到关键帧到。主播（一路编码给所有控制模式观众）：丢失之后已经发过关键帧 → 回答"已发"；250 ms 内已经向编码器要过 → "正在做"；
  否则发 `keyframe` 给引擎 → "正在做"。新开的 mpv（启动或崩溃重启）也是先等关键帧。
- **报告**：观众每 0.5 s 发一次（`R` 2：自己的时钟、往返时间、显示 / 丢失的帧数、FEC 补回的分片数、等关键帧的次数），主播原样回声（`r` 2）→ 观众得到往返时间；
  主播把这些显示在开播页的观众一行。

### 11.4 观众：播放

- **画面**：mpv 照旧负责解码、显示和远程控制的输入（§3.1、§9），只是输入换成 stdin 上的原始 HEVC（`mpv_command::build_low_latency`）。每写完一帧接着写一个
  AUD（访问单元分隔符，NAL 类型 35）：mpv 的 HEVC 解析器要看到下一个访问单元的开头才交出这一帧，有了 AUD 就不用等下一帧。选项（Windows）：
  `--untimed --video-latency-hacks=yes --hwdec-threads=1 --vd-lavc-threads=1 --cache=no --framedrop=no --demuxer-lavf-format=hevc
  --demuxer-lavf-o-add=fflags=+nobuffer --demuxer-lavf-probe-info=nostreams --demuxer-lavf-analyzeduration=0.1 --stream-buffer-size=4k --aid=no
  --d3d11-sync-interval=0`，其余（显卡 API、硬解、HDR、窗口、输入、全屏）和观看相同。macOS 没有 `--d3d11-sync-interval`。
- **mpv 自己的延迟**（2026-10-29 本机实测，RTX 4090，360 Hz 的 1080p 副屏；`latprobe`：把编了帧号条码的 HEVC 按时写进 mpv 的 stdin，用桌面复制读回屏幕上的条码，
  写入 → 出现在桌面上的时间，每格约 1100 帧）：

  | mpv 设置 | 中位 | 范围（p10～p90） |
  |---|---|---|
  | 默认 `--hwdec-threads=4`，30 / 60 / 120 fps | 105.7 / 53.9 / 29.0 ms | 一直是约 3 帧 |
  | `--hwdec-threads=1`（d3d11va） | **6.0 ms** | 5.1～6.6 ms |
  | 再加 `--d3d11-sync-interval=0` | **4.0 ms** | 3.8～5.2 ms |
  | 软解（`--hwdec=no`） | 6.2 ms | 5.5～6.8 ms |
  | `--hwdec=nvdec --hwdec-threads=1` | 6.5 ms | 5.7～7.0 ms |
  | `--hwdec=d3d11va-copy`（4 线程） | 89.0 ms | |

  罪魁是 mpv 给硬解开的 4 个帧线程（`--hwdec-threads` 默认 4）：解码器按帧并行，就要压着 3 帧；交换链深度、`--video-sync`、`--vo=gpu` / `gpu-next`
  都只差 1～2 ms。观看模式有 1.5 s 的缓冲，这 50 ms 无所谓，所以没改。
- **声音**：不经过 mpv（它的音频缓冲和同步会拖住画面），应用自己放（`hdrcast_capture::playback`：Windows 是 WASAPI 共享模式、自己的 48 kHz 16 bit 立体声
  由系统转换，设备里只留约 20 ms；macOS 是 AudioQueue，3 个 10 ms 缓冲）。前面一个 30 ms 的抖动缓冲（`AudioJitter`）：丢的包补静音，太晚的扔掉，
  网络一阵阵来时砍到目标值，两端声卡时钟的漂移靠偶尔（百分之一）丢一帧或重复一帧跟住。静音按钮作用在这里。
- 观看页和悬浮窗在控制模式下显示：延迟（主播从采集到发出 + 半个往返；不含解码和显示）、显示的帧率、码率、丢失的帧和 FEC 补回的包；
  停帧等关键帧时说明。延迟预设不显示。

### 11.5 实测（端到端）

测法（2026-10-30，本机，两个实例、本机信令，见 windows-checklist §15）：一个置顶、点不着的小窗口把当前 QPC 时间（0.1 ms 为单位，24 位，加同步和校验位）
画成条码，每次桌面合成画一次，放在主播采集的那块屏上；观众的 mpv 窗口放在 1080p 360 Hz 的副屏上；再用桌面复制读副屏上 mpv 画面里的条码，和这张桌面画面的
呈现时间（`LastPresentTime`）相减。包括：条码窗口的合成、主播的采集、转换、编码、封装、FEC、加密、发送、观众的接收、拼帧、mpv 的解码和显示、桌面合成；
不包括显示器扫描出来的时间（哪个程序都一样）。主播和观众在同一块 RTX 4090 上（编码和 4K 的解码、色调映射抢同一块 GPU），网络是回环。

| 采集 | 设置 | 中位 | p10～p90 | p99 |
|---|---|---|---|---|
| 1080p SDR（360 Hz 副屏） | 120 fps、20 Mbps、零拷贝、每帧都转换（第一版） | 18.1 ms | 13.3～22.5 | 27.0 |
| 同上 | 走内存（`HDRCAST_NO_ZERO_COPY=1`） | 15.5 ms | 11.0～19.3 | 23.4 |
| 同上 | **零拷贝、到时才取（现在）** | **14.5 ms** | 10.2～19.4 | 22.3 |
| 同上 | 丢包：每 3 s 连丢 40 个数据报 + 随机 2 %（`HDRCAST_SIMULATE_LOSS=3:40:2`） | 15.1 ms | 11.0～20.4 | 23.8 |
| 4K HDR（AW3225QF，240 Hz） | 120 fps、40 Mbps、零拷贝、P010 / Main10 PQ | **21.3 ms** | 16.5～27.2 | 151.6 |

- 丢包那一次（约 36 s）：FEC 补回 1351 个分片；12 次连丢共丢了 32 帧，每次都在下一两帧就由关键帧接上，连开头加起来画面一共停了 0.52 s；声音丢了 221 个包、
  断了 6 次。（回环上往返只有 0.1 ms：真实线路上一次丢帧大约停一个往返加一帧。）
- 4K 那一格的 p99 是少数几次一百多毫秒的停顿，同一块 GPU 上同时做 4K 编码、4K 解码和 HDR→SDR 的色调映射；分开两台电脑时应该没有。
- 主播从采集到发出（`hostMs`）：1080p 1.5～3 ms，4K HDR 3～6 ms。
- 作为对照：观看模式最快的"快速"预设（SRT 300 ms）加上 mpv 的缓冲是 1 s 以上（§5、§3.1）。

### 11.6 局限

- 主播只能是有 NVIDIA 显卡的 Windows 电脑（NVENC 在引擎里）；AMD / Intel / Mac 主播不提供控制模式。
- **macOS 观众的播放和声音没在真机上跑过**（AudioQueue 只做过类型检查）；mpv 的 `--hwdec-threads=1` 对 VideoToolbox 是否同样必要也没量过。
- 丢一帧就要等关键帧（一个往返左右，关键帧本身又大）；Sunshine 的参考帧失效（RFI，`nvEncInvalidateRefFrames`）能更快、更小地恢复，还没做。
- 没有自适应码率，FEC 固定 20 %：丢包多的线路上画面会频繁停顿，这时应该降码率，或者用观看模式。
- 声音不加 FEC，丢的包是一小段静音；不压缩，约 1.5 Mbit/s。
- 一个引擎的画面给所有控制模式观众，任何一位丢帧要来的关键帧大家都收到。
