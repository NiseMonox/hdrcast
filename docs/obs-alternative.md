# 用 OBS 作为备用 HDR 发送端（未实测）

> **整篇未实测。** HdrCast 自己的采集引擎是首选；本文只记录当采集引擎在某台机器上出问题时，用 OBS Studio 30+ 把同样的 HDR10 HEVC 流推到同一个 MediaMTX 中继的思路。观众端、中继、URL 与凭据完全不变。

## 1. OBS 视频设置：P010 / Rec.2100 PQ

设置 → 高级 → 视频：

| 项 | 值 |
|---|---|
| 颜色格式 | **P010 (10-bit, 4:2:0, 2 planes)** |
| 色彩空间 | **Rec. 2100 (PQ)** |
| 色彩范围 | 有限（Limited） |
| SDR 白色等级 | 80 nits（与 HdrCast 的 `--sdr-white 80` 一致；OBS 默认 300，会把桌面白色推到 300 nit，观众看着更亮） |
| HDR 标称峰值等级 | 1000 nits |

来源：「显示器采集」，采集方式选 Windows 10 (1903+)（DXGI），HDR 显示器要求 Windows HDR 已打开。游戏采集也支持 HDR。

## 2. 输出：Custom Output (FFmpeg) → mpegts → srt://

设置 → 输出 → 输出模式「高级」→ **录像** 标签：

| 项 | 值 |
|---|---|
| 类型 | 自定义输出 (FFmpeg) |
| FFmpeg 输出类型 | 输出到 URL |
| 文件路径或 URL | `srt://192.168.1.201:8890?streamid=publish:screen:sender:<sender密码>&passphrase=<publish passphrase>&pbkeylen=32&pkt_size=1316&transtype=live&latency=800000&maxbw=0&inputbw=5000000&oheadbw=25&sndbuf=64000000&ffs=64000&connect_timeout=5000` |
| 容器格式 | `mpegts` |
| 混流器设置 | `mpegts_flags=+resend_headers` |
| 视频比特率 | 40000 Kbps（`inputbw` = 码率/8 字节，40 Mbps → 5000000） |
| 关键帧间隔 | 120 帧（60 fps × 2 s） |
| 视频编码器 | `hevc_nvenc`（AMD：`hevc_amf`；Intel：`hevc_qsv`） |
| 视频编码器设置 | `profile=main10 rc=cbr preset=p4 tune=hq bf=0` |
| 音频比特率 | 192 Kbps |
| 音频编码器 | `aac` |

然后用「开始录制」（不是"开始推流"）启动，因为 Custom Output 挂在录像输出下。也可以把它设成"开始推流"用的服务：设置 → 推流 → 服务「自定义」，服务器填同一个 srt:// URL，编码器选 NVENC HEVC；OBS 28+ 允许自定义服务用 HEVC，走的是同一个 ffmpeg mpegts 路径，但 SRT 参数是否全部透传到 libsrt 没有验证。

## 3. 需要确认的点（没验证）

1. OBS 自带的 FFmpeg 是否编进了 libsrt（OBS 28+ 的 Windows 安装包应该带，日志里出现 "Opening srt://" 即可）。
2. OBS 的 FFmpeg 输出是否把 `color_primaries=bt2020 / color_trc=smpte2084 / colorspace=bt2020nc` 写进 HEVC VUI。观众端用 `ffprobe srt://...` 看 `color_transfer=smpte2084`；缺失时 mpv 会当 SDR 处理导致画面发灰，可在观众端加 `--vf=format=gamma=pq:primaries=bt.2020` 兜底。
3. `x265`/NVENC 的 mastering-display / MaxCLL SEI OBS 不会写，观众端显示器拿不到静态元数据——对屏幕分享无所谓。
4. 中继侧完全一样：`relay/up.sh` 启动后 `relay/logs.sh` 看到 `is publishing to path 'screen'` 即成功。

## 4. 与 HdrCast 采集引擎的差别

| | HdrCast capture | OBS |
|---|---|---|
| 色彩转换 | 自己的 compute shader，sdr-white 固定 80 | OBS 自己的 shader，SDR 白可调 |
| 音频 | WASAPI loopback 自带 | OBS 桌面音频 |
| 帧率 | QPC 恒定帧率，无新帧时重复 | OBS 渲染循环 |
| 安装 | 单个 exe | 装 OBS |
| 状态显示 | GUI 状态栏（采集/编码/码率/丢帧） | OBS 统计窗口 |
