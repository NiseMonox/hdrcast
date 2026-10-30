# HdrCast

[English](README.md) | **简体中文**

把桌面（含 HDR10 与电脑声音）以 10～100 Mbps 的高码率分享给远方的朋友（最初是为中国 / 日本 / 澳大利亚的朋友做的），延迟 1.5～5 s，画面不糊不卡。私人小圈子用，**点对点**：组里谁都能开播，其他人直接从主播的电脑收流。不需要中继服务器、不需要公网 IPv4、不用设端口转发。Windows 和 macOS 都能用。

- **一个应用**：`HdrCast` 有「开播」和「观看」两页，共用同一个群组和邀请码。0.3 起用 Rust 重写（Tauri 2 界面），取代 0.2 的 `HdrCastSender.exe` / `HdrCastReceiver.exe`；协议没变，还在用 0.2.4 的朋友仍然能和你互相看。
- **点对点（IPv6 打洞）**：两台电脑经信令服务器交换地址后同时给对方发包，两边的路由器和防火墙都会放行这条流。信令服务器只转几百字节的签名元数据（谁在播、候选地址），**视频从不经过它**。
- **没有 IPv6 也能连**：IPv4 走公网映射打洞；对方路由器是对称型 NAT（每个目的地换一个端口，国内常见）时，HdrCast 自动在你的路由器上用 UPnP 转发自己的端口，Windows 上再点一次防火墙的 **放行…**，按对方的真实端口反向打洞（见 [docs/network.zh-CN.md](docs/network.zh-CN.md) §1.1）。
- **一次邀请码，永久可用**：建组的人把含 `hdrcast:…` 的一段消息发给朋友，朋友复制整条消息、打开 HdrCast 就会提示加入。
- **HDR10 全链路**：
  - Windows：DXGI 桌面复制 → D3D11 compute shader（scRGB → PQ / BT.2020 / P010）→ NVENC / AMF / QSV / x265 HEVC Main10
  - macOS：ScreenCaptureKit（macOS 15 起可采 HDR）→ VideoToolbox HEVC Main10
  - → SRT（AES-256，每位观众单独重传）→ mpv HDR 直通。SDR 显示器由 mpv 做色调映射。
- **多人同时看**：只编码一次，每位观众各起一个只转封装的 ffmpeg（不重编码），按各自的距离重传；谁的网络差只影响他自己。
- **电脑声音**：不用虚拟声卡，与画面同一时钟。Windows 上可以排除一个程序的声音（例如语音软件，免得朋友听到自己的回声）。
- **不用开播也能连（0.3.1）**：应用开着，这台电脑就以「在线」出现在组里。别人点 **连接**、你同意（或规则允许）后才开始采集，最后一个人走后自动停。像 Parsec 那样，随时连回自己的电脑。
- **按电脑授权的远程控制（0.3.1）**：每台电脑有自己的设备码，主播按电脑决定谁不用问就能看、谁能控制：自己的笔记本连上就能控制，朋友只能看，其他人每次都要问你。输入走同一条点对点路径，端到端加密，密钥只有那两台电脑算得出来。
- **丢包不再花屏（0.3.2）**：线路（比如跨境到中国）丢了补不回来的数据时，观众的画面停在最后一帧完好的画面上，等到下一个关键帧再继续，而不是满屏色块，和 Moonlight 一样；声音不停。主播的编码器码率更平稳，丢包本身也少了；两端都会显示并记录线路丢包有多严重（[docs/faq.zh-CN.md](docs/faq.zh-CN.md)）。
- **为远程控制的控制模式（0.3.7）**：在观看页选 **控制** 而不是 **观看**，有 NVIDIA 显卡的主播就会发一路专门用来操作电脑的流，和 Sunshine / Vibepollo 发给 Moonlight 的一样：NVENC 超低延迟调优、采到一帧就发一帧、用 Reed-Solomon 校验代替重发，mpv 收齐一帧就显示。同一台电脑上从屏幕到屏幕实测：1080p 14.5 ms、4K HDR 21 ms，而不是几秒（[控制模式](#控制模式)）。
- **界面有英文、中文和日文**（**设置 → 语言**；「跟随系统」按系统语言显示）。

> **现状（0.3.7）**：Windows 上的采集、编码、点对点、播放和界面已在真机上测过。Mac 版是 [`fix/macos-first-run`](https://github.com/NiseMonox/hdrcast/tree/fix/macos-first-run) 分支：main 加上 Mac 需要的修改（main 本身在 Mac 上连窗口都出不来）。在 Apple 芯片的 Mac（macOS 26.5.2）上能构建、测试通过，观看可用，HDR 也正常；**从 Mac 开播和远程控制还没有在 Mac 上试过**，步骤见 [docs/macos-checklist.md](docs/macos-checklist.md)。

## 需要什么

- **Windows 10 或 11**（64 位），或 **macOS 13** 及以上。在 Mac 上分享 HDR 需要 macOS 15 和 Apple 芯片。
- **最好有 IPv6**。点对点在 IPv6 上最好用（IPv6 没有 NAT，两台电脑能直接互通）。在 <https://test-ipv6.com> 检查：看到 "10/10"，至少是 "IPv6 address detected" 就行。没有 IPv6？通常可以在路由器里打开，或者请运营商开通。没有 IPv6 时 HdrCast 走 IPv4，只要主播那边是开放的就能连（见下一条）。
- **Windows 防火墙：点一次**。应用里 **防火墙** 那一行出现 **放行…** 时点一下，并在 Windows 的提示里确认。有些路由器（国内常见）每个目的地换一个端口，这样的包只有在 Windows 防火墙放行 HdrCast 时才进得来。这条规则认的是程序所在的路径：升级时把新版本解压覆盖到同一个 `HdrCast` 文件夹（见 [安装](#安装)），规则就一直有效；放到别的文件夹的副本要再点一次（应用会问）。应用在线或播放时会自己在路由器上打开端口（UPnP），停下后再关掉。
- **ffmpeg**（开播用）和 **mpv**（观看用），都要带 SRT 支持。Windows：发行包的 `tools\` 里已经带了。macOS：`brew install ffmpeg-full mpv`（Homebrew 普通的 `ffmpeg` 不带 SRT）。见 [工具](#工具)。
- 能硬解 10 bit HEVC 的显卡（NVIDIA GTX 10 系及以上、AMD RX 400 及以上、Intel 7 代及以上、任何 Apple 芯片的 Mac）。
- 下行带宽约为码率的 1.3 倍（40 Mbps 的流要 50 Mbps 左右）。开播的人每位观众要占一路上行（见 [开播](#开播)）。

## 开始使用

### 安装

- **Windows**：把 `HdrCast-<版本>.zip` 解压到任意位置，运行解压出来的 `HdrCast` 文件夹里的 `HdrCast.exe`。ffmpeg、ffprobe 和 mpv 已经在它的 `tools\` 里。（朋友一般从建组的人那里拿到这个 zip；自己打包见 [构建](#构建)。）
- **Windows 上升级**：先在通知区域的 HdrCast 图标上选 **退出 HdrCast**，再把新的 zip 解压到放 `HdrCast` 的那个文件夹（资源管理器的"全部解压缩"里把目标改成那个文件夹，比如 `D:\`），替换旧文件。`HdrCast` 文件夹名不带版本号，所以防火墙规则和 **登录时自动启动 HdrCast** 都继续有效。
- **macOS**：把 `HdrCast.app` 拖进"应用程序"，再装工具：`brew install ffmpeg-full mpv`。第一次开播时系统会请求"屏幕与系统音频录制"权限。

### 建组或加入

- **新建群组**：第一次打开选 **新建群组**，起个组名和你的名字。邀请码会自动复制到剪贴板：把它发到微信 / LINE / Discord 群里，**只发给信得过的人**：有码就能看。
- **加入**：先 **复制朋友发来的整条邀请消息**，再打开 HdrCast：它会问要不要加入，点加入、填上朋友们会看到的名字。没有提示的话，选 **我有邀请码**（或 **设置 → 粘贴邀请码**）。

### 观看

1. **观看** 页列出组里在线的电脑（每 10 秒刷新）。**延迟** 按你在哪里看来选：日本 1.5 s / 澳大利亚 2 s / 中国 3.5 s。
2. 标着 **LIVE** 的正在开播：点 **观看**。只是 **在线** 的，点 **连接** 后它才开始分享，对方可能要先同意（这时显示 **等待同意**）。
3. **HDR 显示器**：先打开系统的 HDR（Windows：`Win + Alt + B`）；SDR 显示器把 **输出** 选成 SDR。
4. 连不上时面板会直接说原因，比如 "This PC has no IPv6 internet connection"，见 [出问题时](#出问题时)。
5. 观看时画面右上角有个小圆按钮（可以拖到任意位置，下次还在那里）。单击它（或在播放窗口里按 `Ctrl + Alt + O`）打开 **悬浮窗**：收到的分辨率、帧率、码率、Ping、丢包，**延迟**（点 **应用** 后用新的延迟重新连接，画面会中断几秒），**解码方式** 和 **静音**（立即生效），以及 **全屏** 和 **断开连接**。控制着主播时还有 **显示器** 一栏，选主播分享哪块屏（双方都要 0.3.9）。按钮和悬浮窗只在播放窗口上，别的窗口挡到前面时就让开。播放窗口本身没有控制栏，也没有 mpv 的快捷键：`Ctrl + Alt + Enter` 切全屏，`Esc` 退出全屏，关掉播放窗口就是停止观看。

延迟是有意留的：这是能扛住网络波动的直播，不是远程桌面。远程控制时用 **快速** 预设（见下文）。

### 开播

1. 在 **开播** 页选显示器、码率和 **同时观看人数**（可以顺手填上 **我的上行带宽**，超出时会提醒你），点 **开始直播**。每位观众占一整路上行：3 人 × 40 Mbps 约需 150 Mbps 上行（含重传余量）。
2. 上方面板显示码率、帧率和丢帧；下面的列表里是谁在看、走 IPv6 还是 IPv4、网络 ping（不是画面延迟）以及每人的延迟设置。
3. 分享中也能改设置，立刻生效，观众不会断开：编码器会重启约一秒；打开 **按需关键帧** 时改码率连这一下都没有。码率调得比某位观众开始看时更高时，这位观众会重新连接一次（约十秒）。
4. 和大家在语音里聊天？Windows 上在 **声音 → 排除程序** 里选它（例如 `Discord.exe`），朋友就不会从直播里听到自己的回声。需要 Windows 10 21H1 或更新；一次只能排除一个程序。
5. 也可以完全不开播：见下一节。

## 随时连接与远程控制

**在线 / 开播**：HdrCast 在运行、**开播 → 谁可以连接 → 开放连接** 打开（默认）时，这台电脑在组里显示为 **在线**。有人点 **连接** 时按下面的规则处理，被放进来了才开始采集屏幕；最后一个人离开 20 秒后自动停。点 **开始直播** 则是对全组公开：除了设为 **禁止** 的，谁都可以直接看。看过直播的人都离开 2 秒后，直播自动结束，这台电脑回到 **在线**（关着开放连接时下线）：观众再连接就按需分享，3 分钟内看过的不用你再同意。想让没人看的直播也一直开着，关掉 **开播 → 观众 → 观众都离开后结束直播**。

**关掉窗口也在线**：开放连接打开时，点窗口的关闭按钮只是把 HdrCast 收进任务栏右下角的通知区域（托盘；macOS 是菜单栏），这台电脑照样在线。单击托盘图标（或菜单里的 **打开 HdrCast**）把窗口叫回来；**退出 HdrCast** 才真正下线，会先跟观众和信令服务器道别。开放连接关着时，关窗口就是退出（和以前一样）。有人正在看这台电脑时，托盘图标右下角有个红点。已经在运行时再打开 HdrCast，只会把原来的窗口叫出来。

**登录后自动在线**：**设置 → 通用 → 登录时自动启动 HdrCast**。之后每次登录，HdrCast 不弹窗口、直接在托盘里运行并在组里在线，像 Parsec 一样不用管。Windows 上它是注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 里名为 `HdrCast` 的一项（`"…\HdrCast.exe" --background`），任务管理器的"启动应用"里也能看到、关掉；设置里的开关显示的是系统里的真实状态（在任务管理器里被关掉、或者启动的是另一个文件夹里的旧版本时会写明）。macOS 上是 `~/Library/LaunchAgents/app.hdrcast.login.plist`。窗口没开着的时候有人请求观看（或请求控制），窗口会自己弹到最前面并让任务栏按钮闪烁，你点 **允许** / **拒绝**；正在用别的程序打字时它不会抢走键盘。50 秒没人回答就自动拒绝。

**谁可以连接**（开播页）：

- **组里其他人**：观看「直接允许 / 询问我 / 禁止」，控制「不允许 / 询问我 / 自动获得」。默认是询问我 + 不允许。0.3.0 及更早版本的朋友证明不了自己是哪台电脑，永远按这一条。
- **每台电脑一条规则**：按 **设备码**（如 `7KQ2-M9XD-4TPA`）认电脑，不按名字。**添加电脑…** 列出组里在线或最近请求过的电脑，选 **我的电脑**（不用问就能连，连上就能控制）或 **信任的朋友**（不用问就能看，控制要你同意）。有人请求时点 **总是允许** 也会把那台电脑加进来。
- 每台电脑的设备码在 **设置 → 群组 → 这台电脑** 里。给某台电脑控制权之前，先和那台电脑上显示的设备码核对一下。

**把自己的笔记本设成随时可以远程控制台式机**：

1. 两台电脑都装 HdrCast 0.3.1 或更新版本，并加入同一个组（复制同一个邀请码）。两台用不同的名字（**设置 → 我的名字**，比如"小明 笔记本"）。
2. 在台式机上：**开播 → 谁可以连接 → 添加电脑…** → 找到笔记本（核对设备码）→ **我的电脑**。
3. 之后在哪里都行：打开笔记本的 HdrCast → **观看** → 台式机 → **连接**。台式机那边不用点任何东西，连上就能控制。台式机上 HdrCast 要在运行（窗口关掉、留在托盘里就行）：在台式机上打开 **登录时自动启动 HdrCast**，重启之后也不用管。

**观众这边**：主播允许你自动控制时，连上就是控制状态，播放窗口里的鼠标和键盘会发到主播电脑（`Ctrl + Alt + Enter` 切全屏和 `Ctrl + Alt + O` 悬浮窗留给本地播放器）。点 **只看不控** 交还，再点 **接管控制** 拿回来；主播设成"询问我"时点 **请求控制**，等主播同意。主播随时可以 **收回** 控制或 **断开** 你（被断开后一分钟内连不回去）。

- Mac 上，主播是 Windows 时 `Command` 当 `Ctrl` 用、`Control` 当 Win 键用（`Command + C` 就是复制），和 Parsec 打开 "Swap Command and Ctrl" 一样；`Option` 是 `Alt`（双方都要 0.3.9；旧版主播把 `Command` 当 Win 键）。macOS 自己先拿走的快捷键（`Command + Tab`、`Command + 空格`、`Control + 方向键`）和单独按下的 `Control`、`Shift` 这类键到不了主播。
- 要操作对方的电脑，用 **控制模式** 连接（见下）：画面只晚几十毫秒，而不是观看时的几秒。观看模式保留 SRT 延迟和播放器缓冲，适合看，用来操作电脑会觉得慢。还没有相对鼠标（抓鼠标的游戏不行）。
- Mac 当主播时，需要在"系统设置 → 隐私与安全性 → 辅助功能"里允许 HdrCast。Windows 上，以管理员身份运行的程序的窗口不接受注入的输入；在当主播的那台电脑上打开 **设置 → 控制以管理员身份运行的程序** 即可（Windows 会请求一次确认，详见 [FAQ](docs/faq.zh-CN.md)）。
- 主播是 0.2.4 时没有远程控制；主播是 0.3.0 时没有设备码和"在线"，用它自己的「关闭 / 询问我 / 允许」。

## 控制模式

观看是按直播设计的：SRT 留 0.3～3.5 s 的窗口重发丢掉的包，播放器再缓冲 1.5 s，跨境也清楚，但比现场晚几秒。**控制模式**（0.3.7）反过来，用来操作对方的电脑：什么都不缓冲，什么都不重发。

- **观众**：**观看 → 连接 → 模式 → 控制**，然后点那台电脑旁边的 **控制**。提供控制模式的电脑带"控制模式"标记；不提供的（旧版 HdrCast、没有 NVIDIA 显卡）照常观看。延迟预设用不上。观看页面板（和播放画面上的悬浮窗）显示主播和网络加上的延迟、帧率、码率、传输中丢失的画面。
- **主播**：**开播 → 控制模式**：**提供控制模式**（默认开）、它自己的码率（30 Mbps）和最高帧率（60）。画面就是上面设置的显示器、分辨率、HDR、指针和声音。需要 NVIDIA 显卡，只在有人用控制模式时运行。谁能连、谁能控制，还是原来的规则说了算。
- **原理**：另起一个采集引擎，按 Vibepollo 的方式编码（NVENC P1 + 超低延迟调优、一帧的码率缓冲、不用 B 帧、只在被要求时出关键帧），桌面一变就发出那一帧。每帧切成数据报，加 20 % 的 Reed-Solomon 校验，用 ChaCha20-Poly1305 加密，走同一条点对点路径。观众拼好一帧就直接写进 mpv（不缓存、解码器单线程：mpv 默认的 4 个线程会压住 3 帧）。拼不起来的帧让画面停一下，主播马上发关键帧。声音是不压缩的 PCM，由 HdrCast 自己从 30 ms 的缓冲里播放。
- **实测**（同一台电脑，从屏幕到屏幕，docs/design.md §11.5）：1080p 120 fps 中位 14.5 ms，丢包时 15.1 ms；4K HDR 120 fps 21.3 ms。真实网络上再加单程的网络延迟。
- **最适合局域网或近而干净的线路**。丢包多的线路（比如跨境到中国）上会更常停顿：改用观看，或把它的码率调低。控制模式在 Mac 上的那一部分（播放、声音）还没有在 Mac 上跑过（[docs/macos-checklist.md](docs/macos-checklist.md) 第 17 步）。

## 出问题时

观看页的面板会说明发生了什么。连接诊断在中文界面里也是英文（方便直接发给别人排障），常见的有：

| 提示 | 怎么办 |
|---|---|
| This PC has no IPv6 internet connection | 在路由器里打开 IPv6，或者找运营商；test-ipv6.com 可以看状态。如果后面还有 *the host offered no public IPv4 address*，说明根本没试 IPv4：告诉信令服务器的维护者它的 UDP 18651 端口没开。 |
| … the host's PC only takes IPv4 from routers that keep ports … | 你的路由器会换端口，而主播那边没开放：请主播点防火墙旁边的 **放行…**，并确认他们路由器的 UPnP 是开着的。或者打开 IPv6。 |
| … although the host takes IPv4 from any port … | 你电脑上有东西拦了 UDP：VPN、代理或游戏加速器（Clash TUN、UU 等）。关掉它，或者让 HdrCast 绕过它。 |
| The host's PC has no IPv6 internet connection | 主播那边需要 IPv6（同样的办法，在他们那边做）。 |
| Neither side has IPv6 … | 至少一方需要 IPv6；后半句说明了 IPv4 为什么没通。 |
| Both sides have IPv6 but the path is blocked | VPN 或很严的防火墙丢弃了进来的 UDP。关掉 VPN；路由器里找找"IPv6 防火墙：全部阻止"之类的选项。 |
| Not live right now / 现在没有人在线 | 朋友电脑上的 HdrCast 没在运行（退出了，或关掉了开放连接）。列表会自己刷新。 |
| … did not let you in | 主播拒绝了，或者把你的电脑设成了禁止。问问他们。 |
| Nobody at … answered your request | 主播 50 秒内不在电脑前，没点允许。 |
| … is full | 已经达到主播设的 **同时观看人数**。 |
| The signaling server cannot be reached | 你的网络断了，或者信令服务器挂了。已经在看的人不受影响。 |
| 画面卡一下、过几秒又恢复 | 加大延迟（中国可以试自定义 4000～5000 ms），或者请主播降低码率。 |
| HDR 显示器上颜色发灰、发平 | 系统的 HDR 没打开（Windows：`Win + Alt + B`）。 |
| mpv 说打不开 `srt://` | 你的 mpv 没带 libsrt：Windows 上用 zhongfly / shinchiro 的构建（get-tools.cmd 下载的就是）。Homebrew 的 mpv 也不带：Mac 上装好 `ffmpeg-full`，**设置 → 播放方式** 保持 **自动**（或 **ffmpeg → HdrCast → mpv**）。 |

更多见 [docs/faq.zh-CN.md](docs/faq.zh-CN.md)。找朋友帮忙时，把 **日志 → 复制** 的内容发给他：里面的密码和邀请码都已经打码。

## 隐私与安全

- 视频端到端 AES-256 加密（SRT），密钥由邀请码派生。远程控制的输入也加密（ChaCha20-Poly1305）；两台 0.3.1 电脑之间用只有这两台算得出来的密钥。
- **邀请码就是进组的钥匙**：只发给信得过的人。想把某人请出去，就新建一个组，把新邀请码发给其他人。
- 信令服务器看不到任何密码或组密钥，也伪造不了地址（所有消息都用组密钥签名）。它挂了的话，已经在看的人不受影响，只是新的连接要等它恢复。
- 设置文件夹里的 `device.json` 是这台电脑的身份：别人拿到它就能冒充这台电脑。不要发给任何人，也不要拷到别的电脑上。

## 原理

```
┌─ 主播的电脑 ────────────────────────────────────────────────
│ HdrCast（开播）
│   采集引擎（同一个程序的 capture 模式）→ 管道
│   → ffmpeg 编码 HEVC Main10 → MPEG-TS (stdout)
│   → 分发器：每位观众一个 remux ffmpeg（-c copy，SRT，127.0.0.1）
│   → HostLink：[本机 IPv6]:端口 ⇄ 打洞 ⇄ 各位观众
└──┬──────────────────────────────────────────────────────────
   ├─ 点对点（打洞）→ HdrCast（观看）→ mpv（日本）
   └─ 点对点（打洞）→ HdrCast（观看）→ mpv（中国 / 澳大利亚）

腾讯云上的 hdrcast-signal（signal/）：只存「谁在播 + 候选地址」，全部用组密钥签名；不转视频
```

- 只要**双方都有 IPv6** 就能连（IPv6 没有 NAT，打洞几乎总能成功）；IPv4 打洞是额外的尝试。
- 主播要为每位观众各上传一份：3 人 × 40 Mbps ≈ 150 Mbps 上行（含重传余量）。界面会估算，并在超过你填的上行带宽时提醒。

## 信令服务器（只需部署一次）

`signal/` 是一个只用 Python 标准库的小服务。邀请码里写的那台部署在腾讯云上（`http://101.43.23.94:18650`），部署和升级见 [signal/README.zh-CN.md](signal/README.zh-CN.md)（`signal/deploy.sh --ssh ubuntu@101.43.23.94`），并在腾讯云安全组放行 **TCP 18650**（API）和 **UDP 18651**（IPv4 打洞用的地址回显：可选，但不开的话没有 IPv6 的人都连不上）。它看不到也伪造不了任何东西（见 [隐私与安全](#隐私与安全)）。0.3 没有改动它。

## 工具

| 工具 | 谁需要 | Windows | macOS |
|---|---|---|---|
| ffmpeg | 开播的人（Mac 上看的人也要） | 发行包 `tools\` 自带（BtbN gpl 构建，带 libsrt 和 NVENC / AMF / QSV） | `brew install ffmpeg-full`（带 libsrt 和 VideoToolbox；普通的 `ffmpeg` 不带 libsrt） |
| mpv | 看的人 | 发行包 `tools\` 自带（zhongfly 构建，带 libsrt） | `brew install mpv`（不带 libsrt：经 ffmpeg 管道播放，见下） |
| WebView2 | 界面 | Windows 11 自带；Windows 10 一般已随 Edge 装好 | 不需要（系统自带 WebKit） |

Windows 上 `scripts\get-tools.cmd` 一键下载 / 更新 ffmpeg 和 mpv（`sender` / `viewer` / `both`，加 `force` 重新下载）；`scripts\check-tools.cmd` 检查编码器和 `srt://` 支持。

Mac 上 HdrCast 会自己找到 Homebrew 的 `ffmpeg-full`（它是 keg-only，不在 PATH 里），并优先于普通的 `ffmpeg`。Homebrew 的 mpv 不带 libsrt，但只要 **设置 → 播放方式** 是 **自动**（默认），就由 ffmpeg-full 接收流，再交给 mpv，不受影响。

设置和日志在 `%APPDATA%\HdrCast\`（macOS：`~/Library/Application Support/HdrCast/`）。`group.json` 与 0.2 共用，升级后不用重新加入。0.3.1 起多了 `device.json`（这台电脑的设备私钥：不要发出去，换电脑时不要拷贝它）和 `access.json`（谁可以连接）。

## 构建

需要 [Rust](https://rustup.rs) 1.85 或更新。Windows 另需 Visual Studio 的 C++ 生成工具；macOS 需要 Xcode 命令行工具和 `cargo install tauri-cli --version "^2" --locked`。

```bash
./build.sh                    # cargo test + 信令服务器的 Python 测试 → 构建 → 打包到 dist/
./build.sh --no-test          # 跳过测试
./build.sh --tools DIR        # 从 DIR 取 ffmpeg / ffprobe / mpv（--no-tools：不带工具）
cargo run -p hdrcast          # 直接运行（调试构建）
node scripts/ui-preview.mjs   # 只在浏览器里看界面，用模拟数据（http://localhost:5179）
```

产物：Windows 是 `dist/HdrCast/`（`HdrCast.exe`、`tools/`、`scripts/`、`docs/`、`signal/`）和装着这个文件夹的 `dist/HdrCast-<版本>.zip`（只有 zip 的名字带版本号，见 [安装](#安装)）；macOS 是 `dist/HdrCast.app` 和 dmg。想不碰真实的群组和设置试一个构建：把环境变量 `HDRCAST_DATA_ROOT` 指向另一个文件夹。

## 目录结构

```
app/         HdrCast 应用（Tauri 2）：运行采集 + ffmpeg 和 mpv、命令、窗口、托盘；icons/ 是图标
ui/          界面（原生 ES 模块 + preact/htm，不需要 Node 构建）；mock.js 可以让它在浏览器里单独预览
crates/
  core/      平台无关的基础：地址解析、SRT URL、日志脱敏、子进程、防火墙；群组、邀请码、密钥、签名消息
  p2p/       打洞、HostLink / ViewerLink、UPnP、分发、信令客户端、P2pHost / ViewerSession
  capture/   采集引擎：Windows（DXGI / D3D11 / WASAPI）与 macOS（ScreenCaptureKit）后端，共用同一个主循环
  media/     ffmpeg / mpv 的参数与输出解析、编码器档位、sender.json / receiver.json
  control/   远程控制：协议（ChaCha20-Poly1305）、按键映射、输入注入（SendInput / Quartz）
             （设备密钥、签名和控制密钥在 core/src/group/device.rs；谁可以连接在 app/src/access.rs 和 app/src/share.rs）
signal/      信令服务器（Python 标准库）+ systemd 单元 + deploy.sh
scripts/     Windows 批处理：get-tools / check-tools / dump-check；ui-preview.mjs 预览界面
docs/        design.md（规格，§8 是点对点协议）、network.md、faq.md、windows-checklist.md、macos-checklist.md
tests/vectors/                     协议的 golden 向量，Rust、Python 和 C# 的测试共用
sender/ receiver/ shared/ tests/   0.2 的 C# 源码，作为对照保留（Rust 版的测试逐字节对照过它的输出）
build.sh     测试 + 构建 + 打包
```

## 文档

- [docs/design.md](docs/design.md) —— 设计定稿；§8 是点对点协议（邀请码、密钥派生、签名、信令 API、打洞包、§8.9 远程控制用的旁路通道）。
- [docs/network.zh-CN.md](docs/network.zh-CN.md) —— 点对点怎么穿过防火墙、谁需要什么、上行带宽、IPv6 自查。
- [docs/faq.zh-CN.md](docs/faq.zh-CN.md) —— 连不上怎么办（按面板提示查）、HDR 与 SDR、码率与带宽。
- [docs/windows-checklist.md](docs/windows-checklist.md)、[docs/macos-checklist.md](docs/macos-checklist.md) —— 在真机上逐步验证及预期输出。
- [signal/README.zh-CN.md](signal/README.zh-CN.md) —— 信令服务器部署。

## 许可

MIT，见 [LICENSE](LICENSE)。`crates/capture/src/win/nvenc/ffi.rs` 里有取自 NVIDIA `nvEncodeAPI.h` 的声明（MIT，© NVIDIA Corporation），许可声明就在那个文件里。
