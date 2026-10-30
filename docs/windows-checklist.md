# Windows 验证清单（0.3，Rust 版）

自动测试（`cargo test --workspace`：协议 golden 向量、与 C# 0.2.4 的逐字节对照、模拟对称型 NAT 的打洞、假路由器上的 UPnP、
真 ffmpeg / 真 mpv / 真信令服务器的全栈测试、真显示器上的采集）跑通之后，下面是要在真机和真网络上看的部分，按顺序执行，每一步都写了预期结果。

环境：Windows 11 + RTX 4090 + AW3225QF（4K HDR），`HdrCast-<版本>.zip` 解压到任意目录（解压出的文件夹就叫 `HdrCast\`，不带版本号）。

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
- [ ] 0.3.3 起 PASS 前多两行 `zero copy: the shader writing the encoder's p010 surface gives the same frame`（nv12 一行同样）：零拷贝写 NVENC 纹理的
      shader 和读回内存的结果逐字节相同（design.md §2.8）。显卡或驱动做不了时这两行写 `zero copy: … not checked on this GPU (原因)`，不算失败。

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
- [ ] **防火墙** 一栏（开播 → 观众，或 设置 → 网络）：新目录第一次打开是 `Only replies get in…` 加 **放行…** 按钮。
      第一次开播程序会问一次是否放行 → 放行 → Windows UAC 确认 → 变成 `Friends can reach this PC through Windows Firewall.`；
      Windows 防火墙高级设置的入站规则里多了 "HdrCast"（程序 = 这个 exe，UDP，所有配置文件）。
- [ ] 升级：托盘图标 → **Quit HdrCast**，把下一个版本的 zip 解压覆盖到同一个位置（`HdrCast\` 里的文件被替换）。
      再打开：**防火墙** 一栏直接是 `Friends can reach this PC…`，不再问；**设置** 里 **Start HdrCast when I sign in** 仍然打开。
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
- [ ] 朋友（中国 / 澳洲）按 `README.md` 操作，地区预设按 `docs/network.zh-CN.md` §3；连不上时把面板原话和两边 test-ipv6.com 的结果记下来（**日志 → 复制**）。

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
- [ ] 主播点 **开始直播**，观众看了一会儿后点 **停止**：约 2 s 后直播自动结束（"正在观看"里先闪过倒计时；日志
      `everyone who watched has left: the broadcast ended`），主播页面提示一次、回到"可被连接"，观众的列表里主播变回"在线"。
      观众随后再点 **连接**：主播那边不弹请求（3 分钟内看过），几秒后出画面，主播面板是"按需分享中"。观众在悬浮窗里改延迟：直播不结束。
      开播后一直没人来：不会结束。关掉 **观众 → 观众都离开后结束直播**：没人看也一直播。
- [ ] 关掉 **开放连接**：主播从观众的列表里消失（10 s 内），正在按需观看的人被断开。
- [ ] `Ctrl + Alt + Enter` 只切换观众自己的全屏，不会发给主播。
- [ ] 主播点 **收回**：观众面板变成"主播不允许"，输入不再转发；观众按住的键在主播端不会卡住。
- [ ] 观众掉线（拔网线）：1.5 s 内主播端松开他按着的所有键和鼠标按钮。
- [ ] 0.3.0 的观众连 0.3.1 的主播：按"组里其他人"的规则（默认询问我），请求里写"旧版 HdrCast，无法核实是哪台电脑"，没有"总是允许"。
- 局限：以管理员身份运行的窗口不接受注入，除非打开设置里的 **控制以管理员身份运行的程序**（下一组）；Win 键、`Alt + Tab`、`Ctrl + Alt + Del` 被观众自己的系统拦下，发不过去。
  锁屏 / UAC 安全桌面时桌面复制拿不到画面，输入也进不去。主播的 HdrCast 要在运行（窗口可以关，见下）。

输入服务（0.3.6，主播这台；设计见 design.md §9.1；**第一条用探针在本机量过，第二条只在浏览器预览里看过；服务本身（注册、普通权限启动、SYSTEM 进程在你的会话里注入）还没有在真机上装过，从第三条起都是第一次**）：

- [ ] 以管理员身份运行一个带输入框的程序（比如"以管理员身份运行"的记事本）放到最前面。**先不打开开关**：观众（控制中）在上面点击、打字没有反应，
      主播日志约 2 s 后写 `Windows refuses the viewer's mouse and keyboard right now: …switch on "Control programs that run as administrator" in Settings…`
      （之后至少隔 2 分钟才再写）。普通窗口在前面时不写。
- [ ] 设置 → 通用 → **控制以管理员身份运行的程序**：开关是关的，下面写着它做什么（一个 Windows 服务，系统权限，只在有人控制时），Windows 会问一次。
- [ ] 打开开关：Windows 弹出权限确认（程序 `HdrCastInput.exe`）→ 允许 → 开关变成开，说明变成 "On. …"。核对：
      `C:\Program Files\HdrCast\Input\` 里有 `HdrCastInput.exe` 和 `protocol`；`sc qc HdrCastInput` 里启动类型是 DEMAND_START、SERVICE_START_NAME 是 LocalSystem、BINARY_PATH_NAME 带引号并以 `service --user S-1-5-21-…` 结尾；
      `sc sdshow HdrCastInput` 的最后一项是你自己的 SID，权限里有 `RP`（启动）、没有 `WP`（停止）和 `SD`（删除）；`sc query HdrCastInput` 是 STOPPED（它按需启动）。
- [ ] **在普通（非管理员）命令行里**运行 `sc start HdrCastInput`（或者直接让观众控制）：不弹权限确认；任务管理器"详细信息"里出现两个 `HdrCastInput.exe`：
      一个在会话 0（服务），一个在你的会话里、用户名 SYSTEM；约 45 s 后后者自己消失，服务回到 STOPPED。
      （方案成不成立靠这一条：提示"拒绝访问"是服务的访问控制没给你启动权；服务一启动就停了，看 `%ProgramData%\HdrCast\input-helper.log`。）
- [ ] 观众开始控制：主播日志 `input goes through the HdrCast input service`；上面那个管理员窗口现在能点、能打字，指针也到位。观众收回控制 / 离开：按住的键都松开，约 45 s 后进程和服务都停了。
- [ ] 输入进程在运行时在任务管理器里结束它：下一次输入时日志 `the HdrCast input service went away: injecting without it`，约 5 s 内自动再拉起，再写 `input goes through the HdrCast input service`。
- [ ] 观众正按着一个键时，在管理员命令行里运行 `sc stop HdrCastInput`：键被松开（不会卡住），日志 `service: stopped on request`，输入进程消失。
- [ ] 注销再登录、锁屏再解锁：服务按需再起，不残留 SYSTEM 进程。
- [ ] 解压新版覆盖 `D:\HdrCast`：不受影响（服务的程序在 Program Files 里，不占用 HdrCast 文件夹里的文件）。只有协议号变了，设置页才会显示"重新设置"。
- [ ] 关掉开关：权限确认一次，`C:\Program Files\HdrCast` 和服务都消失（`sc query HdrCastInput` 说没有这个服务）。`%ProgramData%\HdrCast\input-helper.log` 留着（它是日志）。
- [ ] Windows 安全中心（Defender）没有把 `HdrCastInput.exe` 标成威胁。标了的话：不要点"允许在设备上"，把它的名字和提示发回来，要判断是误报还是要改写法。
- 局限：UAC 提示和锁屏不在内（有意的：见 design.md §9.1）。

托盘与登录时启动（主播这台；Windows 11 新图标默认在任务栏的隐藏区里，可以在"任务栏设置 → 其他系统托盘图标"里让 HdrCast 常显）：

- [ ] 开放连接打开时点窗口的关闭：窗口消失、托盘里有图标（悬停显示"HdrCast – Available to connect"），观众的列表里仍是"在线"。
      单击托盘图标：窗口回来并在最前面；右键菜单 **打开 HdrCast** 同样。
- [ ] 窗口收在托盘里时观众点 **连接**：主播窗口自己出来（在其他窗口上面，任务栏按钮闪烁），弹出 **拒绝** / **允许**。
      主播这时正在别的程序里打字的话，键盘不应被抢走。观众请求控制（规则"询问我"）时也一样。
- [ ] 有人在看时托盘图标右下角有红点，悬停显示"Sharing this screen"；最后一个人离开约 20 s 后红点消失。
- [ ] 右键菜单 **退出 HdrCast**（有人在看时）：日志 `quitting`、`leaving the group session`，观众几秒内看到主播离开；任务管理器里 HdrCast / ffmpeg 全部消失。
- [ ] 关掉开放连接后点关闭：直接退出（和 0.3.0 一样）。
- [ ] 设置 → 通用 → **登录时自动启动 HdrCast**：`reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v HdrCast` 显示 `"…\HdrCast.exe" --background`；
      关掉后这个值消失。在任务管理器"启动应用"里把 HdrCast 禁用，再打开设置页：开关显示关，并写"已在任务管理器的'启动应用'里被关闭"；在这里打开即恢复。
- [ ] 打开它后注销再登录：没有窗口弹出，托盘里有图标，观众那边 10 s 内看到主播在线；登录后立刻打字（开始菜单搜索）不会丢字。
- [ ] HdrCast 已在运行（托盘里）时再双击 `HdrCast.exe`：不会出现第二个进程，原来的窗口出来；日志 `HdrCast was started again: showing the window`。

## 10. 只有在分享「旋转过的显示器」时才需要看

shader 里 90° 与 270° 的映射（`desk_to_src`）互为逆运算，DXGI 文档没写清 `DXGI_MODE_ROTATION_ROTATE90` 到底对应哪一个，只能实测：

- [ ] 把某块屏在 Windows 显示设置里转成 90°/270°，用 `scripts\dump-check.cmd` 抓一帧看 `frame1-sdr-tonemapped.png`。
- [ ] 画面方向正确即可；若转了 180° 的样子，把 `crates/capture/src/hdrcast_convert.hlsl` 里 `desk_to_src` 的 `rotation == 1` 与 `rotation == 3` 两行对调。

## 11. 出问题时看哪里

- 日志：应用里的 **日志** 页（**复制** 得到的内容已打码，可以直接发人），文件在 `%APPDATA%\HdrCast\logs\hdrcast-yyyyMMdd.log`。
  `[p2p]` 开头的行是打洞与信令，`[remux 名字]` 是那位观众的 ffmpeg，`[capture]` 是采集引擎，`[mpv]` / `[ipc]` 是播放器，`[control]` 是远程控制，
  `[net]` 是线路丢包统计（0.3.2，两端都有），`[guard]` 是观众端拦下受损画面的记录（0.3.2），`[keyframe]` 是按需关键帧（0.3.3，两端都有）。
- 配置：`%APPDATA%\HdrCast\sender.json`、`receiver.json`、`app.json`（删掉即恢复默认）、`group.json`（组和密钥，删掉即退出组）。
- 连不上：先看观看页面板的原话，对照 `docs/network.zh-CN.md` §1.2；画面卡顿 / 断线的排查顺序见 `docs/faq.zh-CN.md`。

## 12. 丢包时画面定格、线路统计（0.3.2）

主播和中国的朋友都装 0.3.2。单机回环（2026-10-23）已经核对过：旧的 `sender.json`（2 s / 2 s、没有 `settingsVersion`）读入后编码器参数是 `-bufsize 10000k -g 60`（40 Mbps、60 fps），
remux 子进程的 `inputbw=5465531`，观众走 `ffmpeg -f data` → 守卫 → mpv，d3d11va 硬解、HDR 直通。没有真实跨境线路，下面这些要和朋友一起看：

- [ ] 主播日志里 `[run]` 的 ffmpeg 命令行有 `-bufsize` = 码率的四分之一、`-g` = 帧率（关键帧 1 s）。
- [ ] 观众日志里有 `[app] ffmpeg receives the stream and HdrCast passes it on to mpv`；**设置 → 播放方式** 是 **自动（有 ffmpeg 就用）**。
- [ ] 看 15 s 后两边日志各有一行 `[net]`；之后丢包时每 30 s 一行。记下高峰时段（晚上）的 `lost on the way` 和观众那行的 `never recovered in time`。
- [ ] 线路丢包时观众的画面**停住**（最多约 1 s），然后从关键帧继续，**不再出现满屏色块**；声音不断。观看页显示"传输丢失 n 次 · 已拦下 n 帧受损画面 · 定格 n 次"，
      日志有 `[guard]` 行。主播 **开播** 页这位观众的一行显示"途中丢包 x %"。
- [ ] 经常定格时：观众把延迟改成 **中国**（3.5 s）或自定义加 500～1000 ms，看 `never recovered` 是否降到 0；还多就降码率（比如 40 → 25 Mbps）再看。
- [ ] 把观众的播放方式改成 **mpv 直接播放** 对比：丢包时又出现色块（守卫只在 ffmpeg 管道里起作用）。

## 13. 按需关键帧（0.3.3）

主播在 **开播 → 更多选项** 打开 **按需关键帧**（只有编码器是 NVENC 时才有这一项；打开后"前瞻"变灰），观众装 0.3.3（旧版本观众照常能看，只是不会请求）。
单机回环（2026-10-24，下文"单机回环"的做法，观众另设 `HDRCAST_SIMULATE_LOSS=4:30`、延迟 1500 ms）已经核对过：从模拟的丢包到强制的 IDR 离开编码器
72～99 ms（1080p60 HDR），画面停住 166～183 ms 的流内时间；同样的丢包关掉开关时每次停 1.0～1.5 s。4K60 HDR 40 Mbps 时停 200～233 ms，采集与封装 60 fps 不丢帧。
细节见 `docs/design.md` §2.8、§8.12。要和朋友一起看的：

- [ ] 主播日志有 `capture: encoding here: NVENC on …` 和 `[run] sending … (encoded by HdrCast itself: keyframes on request)`；ffmpeg 的命令行是
      `-f mpegts … -c:v copy`。没有这两行而有 `ffmpeg encodes instead` 时，引擎前面几行写着为什么（驱动太旧、不是 NVIDIA…），画面照常，只是没有按需关键帧。
- [ ] 观众加入时主播日志有 `[keyframe] <名字>: its stream started: a keyframe now`：画面出来得比以前快（不用等下一个定时关键帧）。
- [ ] 线路丢包时观众日志有 `[keyframe] asked the host for a keyframe: part of the stream is still missing on the way (… ms ago)`、
      `[keyframe] the host is making a keyframe`，稍后 `[keyframe] the picture moves again: it held … ms of the stream`。记下中国线路上的这个毫秒数
      （预计 300～500 ms，主要是发现丢包要等两轮重发），和关掉开关时的 `[guard]` 汇总比较。
- [ ] 主播日志的 `[keyframe] a keyframe went out … ms after …`：4K60 一般 20～40 ms。`… more lines like the ones above were left out` 很多时，说明丢包很频繁，
      这时更该降码率。
- [ ] 画质：打开前后看同一段画面（开关只去掉了前瞻，本机合成内容的 PSNR 只差 0.01 dB）。

## 14. 悬浮窗、播放窗口、分享中改设置（0.3.3）

观众端（design.md §3.1、§3.3）：

- [ ] 画面出来后右上角有个半透明小圆按钮（150 % 缩放时 60 px，离画面边 18 px），鼠标移上去变实。mpv 窗口没有因此失去焦点（远程控制时键盘照常给主播）。
- [ ] 单击按钮：展开成面板，按钮在右上时向左下展开。数据（分辨率 / 帧率 / 码率 / Ping / 丢包 / 解码）会动。标题栏的"—"、`Esc`、播放窗口里的
      `Ctrl+Alt+O`、观看页的 **悬浮窗** 按钮都能收起 / 展开。
- [ ] **拖动**（只有这项还没在真机上用鼠标试过）：拖按钮到画面左下角，松手后它留在那里；展开时面板向右上展开。拖面板的标题栏也行，收起后按钮在面板的角上。
      退出 HdrCast 再观看，按钮还在左下（配置目录的 `overlay.json`）。拖到画面外松手，按钮回到画面边上。拖完按钮后键盘回到播放器（全屏时任务栏不留在上面）。
- [ ] 拖动 / 改大小 mpv 窗口、切全屏：按钮跟着走，在画面上的相对位置不变。
- [ ] 切到别的程序：按钮和面板一起藏起来；点回播放器又出现。点 HdrCast 主窗口时还在。
- [ ] 面板里 **静音** / **取消静音** 立即生效；改延迟后 **应用**：几秒后用新延迟继续播放，面板一直在，"当前"变成新值，静音保持。
      **解码方式** 立即切换（日志 `[app] decoder: no, switched while playing`）。
- [ ] 播放窗口里的滚轮、空格、`q`、`m`、`9` / `0`、`f`、双击都没有反应；`Ctrl+Alt+Enter` 切全屏，`Esc` 退出全屏；把文件拖进播放窗口不会替换直播；
      拖画面不会移动窗口。点播放窗口的关闭按钮 = 停止观看（日志 `[app] the player window was closed`、`[app] stopped watching`），不会 3 s 后又弹出来。
- [ ] 远程控制中：除了 `Ctrl+Alt+Enter` / `Ctrl+Alt+O`，`Esc` 和其他键都到了主播那边；点悬浮窗不会把点击发给主播。

主播端（design.md §2.7）：

- [ ] 开播后开播页的设置不再变灰，画面一节下有"分享中改设置会立刻生效"的说明。改分辨率 / 帧率：日志 `the stream's settings changed: starting capture
      and encoder again (viewers stay connected)`，观众停一秒左右继续看，不用重新加入。连续拖码率滑块只重启一次。
- [ ] 打开 **按需关键帧** 后改码率：日志 `bitrate now … Mbps: the running encoder took it, no restart`，观众不中断。
- [ ] 码率调得比观众加入时更高（例如 25 → 40）：主播日志 `<名字>: the bitrate went up to 40 Mbps, more than their connection was set up for: it starts again`，
      观众约 10 s 后继续播放；调低时没有这一行。

## 15. 控制模式（0.3.7）

- [ ] 主播（有 NVIDIA 显卡）：**开播 → 控制模式** 里 **提供控制模式** 默认开着，下面是码率和帧率；没有 NVIDIA 显卡的电脑上显示"不提供"。
- [ ] 观众：**观看 → 连接 → 模式** 选 **控制**：延迟预设消失；在线列表里这台主播带"控制模式"，按钮是 **控制**。不提供的主播按钮照旧是 **观看 / 连接**，
      连上后观众日志 `this host does not offer control mode: watching with the usual delay`。
- [ ] 连上：主播日志 `[p2p] <名字> is joining in control mode`、`[ll] streaming: Streaming`、
      `[ll-capture] … capture: control mode: GPU scheduling class high, GPU thread priority 7`（不是管理员时可能是 unchanged）、
      `encoding here: NVENC … P1 ultra-low latency … keyframes on request`；观众日志 `control mode: HdrCast hands mpv each picture …`，mpv `Using hardware decoding`。
- [ ] 观看页面板：**延迟**（主播从采集到发出 + 半个往返）几毫秒到十几毫秒、帧率接近设定值、码率、**丢失的画面**；悬浮窗同样，没有延迟预设。
      开播页这位观众一行带"控制模式"、ping、Mbps。
- [ ] 鼠标、键盘照控制规则工作（§9）；声音从观众的默认输出出来，静音按钮有效。
- [ ] 最后一位控制模式观众走后 10 s：主播日志 `nobody is in control mode: stopping its stream until somebody connects`。
- [ ] 开播页改控制模式的码率：`control mode now at … Mbps: the running encoder took it`；改帧率：`control mode's settings changed: starting its capture and encoder again`，
      观众停顿一下后继续。关掉 **提供控制模式**：控制模式的观众被断开。
- [ ] 丢包（单机回环，观众那份加 `set HDRCAST_SIMULATE_LOSS=3:40:2`：每 3 s 连丢 40 个控制模式的数据报，另外随机丢 2 %）：观众日志
      `[test] lost 40 datagrams …`，画面只停一两帧；面板的 **丢失的画面** 和 **FEC 补回** 增加。
- [ ] 延迟（可选，单机）：`cargo build --release -p hdrcast-capture --example latprobe`，然后
      `latprobe clock --x <主播采集的屏左上角> --y … --cell 24 --seconds 60`、`latprobe place --rect …`（把观众的 mpv 放到一块 SDR 屏上，打印画面区域）、
      `latprobe read --output <那块屏的序号> --region <mpv 里条码的位置> --seconds 20`（design §11.5；2026-10-30 本机 1080p120 中位 14.5 ms，4K HDR 21.3 ms）。

## 单机回环（不碰真实群组、路由器和朋友）

开发时在一台电脑上同时跑主播和观众：

```bat
rem 本机信令服务器
python signal\hdrcast_signal.py --listen 127.0.0.1:28650 --echo-port 28651 --state-dir %TEMP%\hdrcast-signal

rem 两个互相隔离的配置目录（各放一份 group.json：同一个 gid / key，signal 写 http://127.0.0.1:28650，peer 和 me 不同）
set HDRCAST_NO_ROUTER=1            & rem 不做 UPnP
set HDRCAST_CONTROL_DRY_RUN=1      & rem 远程控制只写日志，不真的注入（同一台电脑上真注入会自己套住自己）
set HDRCAST_DATA_ROOT=D:\tmp\host   & start HdrCast.exe
rem 试按需关键帧时，观众那份先加上这一行：每 4 s 把 30 个 SRT 包连同它们的全部重发丢掉
set HDRCAST_SIMULATE_LOSS=4:30
set HDRCAST_DATA_ROOT=D:\tmp\viewer & start HdrCast.exe
```
