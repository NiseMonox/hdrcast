# Windows 验证清单（0.3，Rust 版）

自动测试（`cargo test --workspace`：协议 golden 向量、与 C# 0.2.4 的逐字节对照、模拟对称型 NAT 的打洞、假路由器上的 UPnP、
真 ffmpeg / 真 mpv / 真信令服务器的全栈测试、真显示器上的采集）跑通之后，下面是要在真机和真网络上看的部分，按顺序执行，每一步都写了预期结果。

环境：Windows 11 + RTX 4090 + AW3225QF（4K HDR），`dist/HdrCast-<版本>/` 解压到任意目录（下文记为 `HdrCast\`）。

> **2026-10-13 在主播这台机器上已经实测过的**（同一台电脑上开两个互相隔离的实例 + 本机信令服务器，见文末"单机回环"）：
> 第 1–4 步（工具、显示器列表、selftest、采集落盘，均与 0.2.4 的 exe 对比过）、第 6–7 步的出画面与统计（1080p SDR 和 4K HDR 两条链路、
> mpv HDR 直通）、第 9 步远程控制的请求 / 允许 / 收回和键鼠事件转发。**还没测的**：真实路由器上的 UPnP 转发、防火墙 **放行…**（要 UAC）、
> 跨网络的朋友（第 5 步后半）、长时间 4K60（第 8 步）。
> **2026-10-19（0.3.1）同样用单机回环实测过**第 9 步的：在线列表与设备码、请求 → 总是允许 → 按需开始采集（点连接到连上约 1.3 s）、20 s 无人自动停、
> "我的电脑"不弹窗直接连上并自动获得控制（模拟注入）、看的过程中放宽规则 10 s 内生效、断开后一分钟连不回、禁止 / 拒绝 / 两分钟内不再问、
> 有请求时开始直播自动放行、停止后回到在线。没测的：0.3.0 的 exe 当观众（只在代码层面测过不带签名的加入）、跨网络。

## 0. 准备

- [ ] Windows HDR 已打开（`Win + Alt + B`，或 设置 → 显示 → 使用 HDR）。
- [ ] **工具**：发行包的 `tools\` 里已带 BtbN 的 ffmpeg win64-gpl（含 nvenc/amf/qsv、libsrt）和 zhongfly 的 mpv（含 libsrt），版本见 `tools\README.txt`。
  用 `--no-tools` 打的小包才需要双击 `scripts\get-tools.cmd`：它下载到 `HdrCast\tools\`，装完自己验证 srt 支持并列出可用编码器。`get-tools.cmd force` 强制重下。
- [ ] 界面用系统自带的 WebView2（Windows 11 自带；Windows 10 没有的话装 "WebView2 Runtime"）。
- [ ] 信令服务器在线（已部署在腾讯云）：浏览器打开 <http://101.43.23.94:18650/v1/health> 显示 `{"ok":true,"v":1}`。
- 命令行模式的注意点：`HdrCast.exe` 是窗口程序，在交互式终端里直接敲 `HdrCast.exe list-outputs` 时终端不会等它结束（输出会和提示符混在一起）。
  用 `HdrCast.exe list-outputs | more`，或者用下面的两个脚本。

## 1. `scripts\check-tools.cmd`

双击运行。预期：

```
app       : ...\HdrCast.exe
ffmpeg    path/version
srt       : yes (libsrt)
hevc_nvenc: listed, test encode 8-bit=ok 10-bit=ok
hevc_amf  : listed, FAIL                                   ← 没有 AMD 卡，FAIL 正常
hevc_qsv  : listed, FAIL                                   ← 没有 Intel 核显，FAIL 正常
mpv       version 0.40+ 或 git 日期，srt : yes
displays  list-outputs 的输出（见下一步）
```

`srt : NO` 说明下错了构建；`hevc_nvenc ... 10-bit=FAIL` 先更新 NVIDIA 驱动。

## 2. `HdrCast.exe list-outputs | more`

预期每个显示器一行，例如：

```
idx  device         name                      size        pos          hz     hdr  colorspace  bits  maxnit  maxffl  minnit  adapter
0    \\.\DISPLAY2   AW3225QF                  3840x2160   0,0          239.99 on   12          10    500     ...
1    \\.\DISPLAY1   DELL AW2523HF             1920x1080   3840,0       360    off  8           8     ...
```

- [ ] HDR 显示器那一行 `hdr = on`（`colorspace = 12` 即 DXGI `RGB_FULL_G2084_NONE_P2020`）。关掉 Windows HDR 再跑一次应变成 `off`。
- [ ] `--json` 也能输出同样内容。应用里 **开播 → 显示器** 的卡片就是它（HDR 的带 `HDR` 徽标）。

## 3. `HdrCast.exe capture --selftest | more`

不需要显示器，只跑 compute shader，把 GPU 的结果和参考实现逐个比对，末尾打印 PASS / FAIL。
预期退出码 0、每行 `ok`。参考值（sdr-white 80，limited range 10 bit，程序内允许 ±2）：

| scRGB 输入 | 亮度 | 期望 Y10 |
|---|---|---|
| 0.0 | 0 nit | 64 |
| 0.5 | 40 nit | 431 |
| 1.0 | 80 nit | 490 |
| 2.5 | 200 nit | 571 |
| 12.5 | 1000 nit | 723 |
| 125 | 10000 nit | 940（限幅） |

- [ ] 中性灰的 Cb/Cr ≈ 512。

## 4. `scripts\dump-check.cmd`

先用记事本把脚本开头的 `OUTPUT` 改成第 2 步的索引，`SIZE` 改成该显示器的原生分辨率（如 `3840x2160`）。运行后在 `scripts\dump\` 生成：

- [ ] `frames.p010` 大小 = W×H×3 字节 × 帧数（4K：24 883 200 B/帧 × 30 fps × 3 s ≈ 2.2 GB；嫌大就先用 `SIZE=1920x1080` 测）。
- [ ] 终端打印 `YMIN/YAVG/YMAX`：普通桌面（白色窗口）`YMAX ≈ 490`，屏幕上有 HDR 内容时才会 > 500；`YMIN` 接近 64。若 `YMAX` 总是 940 或全部相同 → 转换错了。
- [ ] `frame1-pq-raw.png`：整体发灰、发平——**正常**，这是 PQ 编码直接当 sRGB 看的效果。
- [ ] `frame1-sdr-tonemapped.png`：看起来就是你的桌面，颜色不偏，鼠标指针在正确位置。
- [ ] 把 `FORMAT` 改成 `nv12` 再跑一次（可先关闭 Windows HDR），`frame1-sdr.png` 应同样正确。
- [ ] 结束后删掉 `scripts\dump\frames.*`。

## 5. 建组、加入、打洞

先在两台自己的电脑上测（同一局域网即可），再测外网。

- [ ] 第一次打开 `HdrCast.exe` 是欢迎页 → **新建群组** → 填组名和名字 → 提示邀请已复制；粘贴出来是一行说明加一行 `hdrcast:…`（约 200 字符）。
      `%APPDATA%\HdrCast\group.json` 已生成。（从 0.2 升级的电脑直接进主界面，组还在。）
- [ ] 另一台电脑复制这条消息后打开 HdrCast：弹"剪贴板里有邀请码" → 加入… → 填名字 → 左下角显示组名。点"以后再说"的话，同一个邀请不应再问第二次。
- [ ] **开播 → 开始直播**：**日志** 页有 `[p2p] candidates: [2400:…]:NNNNN, 192.168.x.x:NNNNN`。IPv6 必须是**稳定地址**：
      PowerShell 里 `Get-NetIPAddress -AddressFamily IPv6 | ft IPAddress,PrefixOrigin,SuffixOrigin,AddressState`，
      日志里的地址那一行 SuffixOrigin 应为 **Link**（不是 Random = 临时地址）。
- [ ] 信令服务器的 UDP 18651 放行后，日志还有 `[p2p] public IPv4 mapping: …`（没有也不影响 IPv6）。
- [ ] **防火墙** 一栏（开播 → 观众，或 设置 → 网络）：新版本目录第一次打开是 `Only replies get in…` 加 **放行…** 按钮。
      第一次开播程序会问一次是否放行 → 放行 → Windows UAC 确认 → 变成 `Friends can reach this PC through Windows Firewall.`；
      Windows 防火墙高级设置的入站规则里多了 "HdrCast"（程序 = 这个 exe，UDP，所有配置文件）。
- [ ] 开播几秒内日志有 `[p2p] router: UDP NNNNN forwarded to this PC (XG-100NE, internet address …:NNNNN)`；
      停止后有 `[p2p] router: forward of UDP NNNNN removed`（路由器 UPnP 页面里 HdrCast 那条也消失）。
      路由器关了 UPnP 时是 `could not forward UDP … no UPnP router answered`，面板说明 IPv4 没打开缺什么。
- [ ] 没有 IPv6 的朋友（对称型 NAT，例如江苏电信）点观看：主播日志 `connected via IPv4 …:端口 (a port their router picked for this PC …)`。
- [ ] 反过来让这位朋友开播、这边看（这边也要 **放行…**）：日志 `connected to … (a port the host's router picked …)`。
- [ ] **观看** 页 10 s 内出现主播 → **观看** → 面板 `Connected to …（IPv6 [2400:…]:NNNNN, ping N ms）` → mpv 出画面；
      主播的"正在观看"列表显示这位观众（IPv6 / ping / 延迟）。
- [ ] **外网**：笔记本连手机热点（确认 test-ipv6.com 有 IPv6），不改任何路由器设置，能连上。
- [ ] **新旧互通**：朋友还在用 0.2.4 的 `HdrCastReceiver.exe` 看这边的 0.3 开播，以及反过来，都能出画面（协议没变）。
- [ ] 关掉 mpv 窗口：约 10 s 内自动重连出画面（主播端日志会看到那位观众的 remux 子进程重启）。
- [ ] 主播 **停止**：观众显示 `The stream ended - waiting …`；再开播，观众自动重新接上。
- [ ] **同时观看人数** 设 1：第二个观众显示 `… is full`。
- [ ] 上行估算：码率 40、3 人、我的上行带宽 100 → 提示变红。
- [ ] 朋友（中国 / 澳洲）按 `README.en.md` 操作，地区预设按 `docs/network.md` §3；连不上时把面板原话和两边 test-ipv6.com 的结果记下来（**日志 → 复制**）。

## 6. 开播端：画面与统计

**开播** 页：选第 2 步那块显示器（卡片带 HDR，HDR 选"自动"）；编码器 NVENC，40 Mbps，60 fps，分辨率先选 1440p；声音开。开始直播后让一台观众连上。

- [ ] 任务管理器里 `HdrCast.exe` ×2（应用 + 采集引擎）+ 编码 `ffmpeg.exe` + 每位观众一个 remux `ffmpeg.exe`；几秒内面板开始刷新。
- [ ] 采集 ≈ 60 fps；桌面静止时"重复帧"增长是**正常的**（没有新帧就重发上一帧，保证恒定帧率）。
- [ ] 编码 ≈ 60 fps，speed ≈ 1.0×，丢帧不持续增长（开播头几秒编码器初始化时丢几十帧属正常）；GPU 转换耗时 4K 下 < 8 ms。
      码率在画面静止时远低于目标是正常的（桌面内容好压）。
- [ ] 点 **停止**：所有子进程都消失；直接关窗口也一样（先向观众和信令服务器告别，几秒后退出）。
- [ ] 任务管理器里结束编码 `ffmpeg.exe`：3 s 内自动重启，观众几秒内恢复，不需要重新点观看。

## 7. 观众端：画面

- [ ] 面板：硬解 `d3d11va`；下面一行 source 为 `bt.2020 / pq / …`；HDR 屏且 Windows HDR 已开时"丢帧"格下面显示 **HDR 直通**；
      缓冲在设定值附近波动；丢帧不持续增长。
- [ ] 画面亮度与主播一致：白色窗口不刺眼（80 nit 规范值），HDR 视频高光正常。发灰 → 观众端 Windows HDR 没开或输出选了 SDR。
- [ ] **全屏** 按钮可用，全屏时显示器 OSD 应显示 HDR 模式。
- [ ] SDR 屏（或把 Windows HDR 关掉）：输出选"SDR"，画面正常但高光被压缩，不应该发灰或过曝。
- [ ] 声音同步：主播播放一段有节奏的视频，观众端口型 / 节拍对得上（延迟约 3.5 s 是设计值）。
- [ ] 点 **停止**：mpv 进程消失，主播端这位观众离开。
- [ ] 设置 → 播放方式 选"ffmpeg 管道 → mpv"也能出画面（mpv 的 srt 支持坏掉时的备胎）。

## 8. 4K60 压力

- [ ] 分辨率"原生"（3840×2160）、80 Mbps、60 fps 推 10 分钟：采集稳定 60 fps，GPU 转换 < 8 ms，编码 speed ≥ 1.0×。
- [ ] 120 fps 档位：采集能到 120 说明 DXGI 帧率跟得上；否则退回 60。
- [ ] 两三位观众同时看时主播上行够不够："正在观看"上方的上行估算 vs 实际上行。

## 9. 随时连接与远程控制（0.3.1）

两台都是 0.3.1、在同一个组。"主播"不点开始直播。

- [ ] 主播的开播页顶部是"可被连接"和设备码；观众的观看页 10 s 内列出主播，标"在线"、同一个设备码，按钮是 **连接**。
- [ ] 观众点 **连接**：观众面板变成"等待同意"；主播弹出"某某想观看这台电脑"（带观众的设备码，和观众"设置 → 这台电脑"里显示的一致），
      侧边栏"开播"上有数字。**这时主播还没开始采集**（开播页没有面板，任务管理器里没有 ffmpeg）。
- [ ] 主播点 **总是允许**：开始采集，几秒内观众出画面；主播面板标题"按需分享中"；"谁可以连接"里多了观众那台电脑（观看：直接允许）。
- [ ] 观众点 **停止**：约 20 s 后主播停止采集（日志 `nobody is watching: stopping the stream`），回到"可被连接"。
- [ ] 主播在"谁可以连接"里把观众那台电脑的 **控制** 设成"自动获得"。观众再连接：主播那边**不弹任何东西**，观众连上即显示"你正在控制 …"，
      播放窗口里移动鼠标、点击、打字、滚轮都作用到主播的那块显示器上；主播的观众列表里带"控制中"。
- [ ] 观众点 **只看不控**：输入不再转发，按住的键在主播端不会卡住；**接管控制** 又恢复。
- [ ] 观众正在看时，主播把它的控制从"不允许"改成"自动获得"：10 s 内观众变成控制状态（不用重连）。
- [ ] 主播点观众旁边的 **断开**：观众 5 s 后显示"… did not let you in"，一分钟内连不回来。
- [ ] 主播把"组里其他人"的观看设成"询问我"，另一台没加规则的电脑来连接 → 弹出请求 → **拒绝**：对方显示"did not let you in"、不再重试；
      两分钟内再连直接被拒、不再弹。没人回答时 50 s 后对方显示"Nobody at … answered your request"。
- [ ] 有请求在等时主播点 **开始直播**：请求自动放行，观看列表里主播变成 LIVE。**停止** 后所有观众断开，主播回到"在线"。
- [ ] 关掉 **开放连接**：主播从观众的列表里消失（10 s 内），正在按需观看的人被断开。
- [ ] `Ctrl + Alt + Enter` 只切换观众自己的全屏，不会发给主播。
- [ ] 主播点 **收回**：观众面板变成"主播不允许"，输入不再转发；观众按住的键在主播端不会卡住。
- [ ] 观众掉线（拔网线）：1.5 s 内主播端松开他按着的所有键和鼠标按钮。
- [ ] 0.3.0 的观众连 0.3.1 的主播：按"组里其他人"的规则（默认询问我），请求里写"旧版 HdrCast，无法核实是哪台电脑"，没有"总是允许"。
- 局限：以管理员身份运行的窗口不接受注入（除非 HdrCast 也以管理员身份运行）；Win 键、`Alt + Tab`、`Ctrl + Alt + Del` 被观众自己的系统拦下，发不过去。
  锁屏 / UAC 安全桌面时桌面复制拿不到画面。主播要一直开着 HdrCast（关窗口就下线）。

## 10. 只有在分享「旋转过的显示器」时才需要看

shader 里 90° 与 270° 的映射（`desk_to_src`）互为逆运算，DXGI 文档没写清 `DXGI_MODE_ROTATION_ROTATE90` 到底对应哪一个，只能实测：

- [ ] 把某块屏在 Windows 显示设置里转成 90°/270°，用 `scripts\dump-check.cmd` 抓一帧看 `frame1-sdr-tonemapped.png`。
- [ ] 画面方向正确即可；若转了 180° 的样子，把 `crates/capture/src/hdrcast_convert.hlsl` 里 `desk_to_src` 的 `rotation == 1` 与 `rotation == 3` 两行对调。

## 11. 出问题时看哪里

- 日志：应用里的 **日志** 页（**复制** 得到的内容已打码，可以直接发人），文件在 `%APPDATA%\HdrCast\logs\hdrcast-yyyyMMdd.log`。
  `[p2p]` 开头的行是打洞与信令，`[remux 名字]` 是那位观众的 ffmpeg，`[capture]` 是采集引擎，`[mpv]` / `[ipc]` 是播放器，`[control]` 是远程控制。
- 配置：`%APPDATA%\HdrCast\sender.json`、`receiver.json`、`app.json`（删掉即恢复默认）、`group.json`（组和密钥，删掉即退出组）。
- 连不上：先看观看页面板的原话，对照 `docs/network.md` §1.2；画面卡顿 / 断线的排查顺序见 `docs/faq.md`。

## 单机回环（不碰真实群组、路由器和朋友）

开发时在一台电脑上同时跑主播和观众：

```bat
rem 本机信令服务器
python signal\hdrcast_signal.py --listen 127.0.0.1:28650 --echo-port 28651 --state-dir %TEMP%\hdrcast-signal

rem 两个互相隔离的配置目录（各放一份 group.json：同一个 gid / key，signal 写 http://127.0.0.1:28650，peer 和 me 不同）
set HDRCAST_NO_ROUTER=1            & rem 不做 UPnP
set HDRCAST_CONTROL_DRY_RUN=1      & rem 远程控制只写日志，不真的注入（同一台电脑上真注入会自己套住自己）
set HDRCAST_DATA_ROOT=D:\tmp\host   & start HdrCast.exe
set HDRCAST_DATA_ROOT=D:\tmp\viewer & start HdrCast.exe
```
