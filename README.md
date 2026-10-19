# HdrCast

把桌面（含 HDR10 与电脑声音）以 10~100 Mbps 的高码率"推流式"分享给中国 / 日本 / 澳大利亚的朋友，延迟 1.5~5 s，画面不糊不卡。私人用，**点对点**：谁都能开播，同组的朋友直接从主播的电脑收流，不需要中继、不需要公网 IPv4、不用改路由器。Windows 和 macOS 都能用。

- **一个应用**：`HdrCast`，左边栏切换「开播 / 观看」，群组和邀请码共用一份。0.3 起用 Rust 重写（Tauri 2 界面），取代 0.2 的 `HdrCastSender.exe` / `HdrCastReceiver.exe`；协议没变，0.2.4 的朋友仍然能互相看。
- **点对点（IPv6 打洞）**：双方经信令服务器交换地址后同时给对方发包，两边路由器和防火墙都会放行这条流。信令服务器只转几百字节的签名元数据（谁在播、候选地址），**视频从不经过它**。
- **没有 IPv6 也能连**：IPv4 走公网映射打洞；对方路由器是对称型 NAT（国内常见，每个目的地换端口）时，程序自动在路由器上用 UPnP 转发自己的端口，Windows 上再点一次防火墙的 **放行…**，按对方的真实端口反向打洞（见 [docs/network.md](docs/network.md) §1.1）。
- **一次邀请码，永久可用**：主播建组后发一段 `hdrcast:…` 给朋友，朋友复制整条消息打开 HdrCast 就会提示加入。
- **HDR10 全链路**：
  - Windows：DXGI 桌面复制 → D3D11 compute shader（scRGB → PQ / BT.2020 / P010）→ NVENC / AMF / QSV / x265 HEVC Main10
  - macOS：ScreenCaptureKit（macOS 15 起可采 HDR）→ VideoToolbox HEVC Main10
  - → SRT（AES-256，每位观众单独重传）→ mpv HDR 直通。SDR 观众由 mpv 做色调映射。
- **多人同时看**：一路编码，按观众各起一个只转封装的 ffmpeg（不重编码），每人按自己地区的延迟重传；谁卡了只影响他自己。
- **电脑声音**：不用虚拟声卡，与视频同一时钟。Windows 上可以排除一个程序的声音（例如语音软件，免得观众听到自己的回声）。
- **不用开播也能连（0.3.1）**：应用开着就以「在线」出现在组里，别人点「连接」、你同意（或规则允许）后才开始采集屏幕，最后一个人走后自动停。像 Parsec 那样随时连回自己的电脑。
- **按电脑授权的远程控制（0.3.1）**：每台电脑有自己的设备码，主播给每台电脑定「能不能直接看、能不能控制」：自己的笔记本连上就能控制，朋友只能看，别人每次都要问你。输入走同一条点对点路径，端到端加密，密钥只有那两台电脑算得出来。

> 现状（0.3.1）：Windows 上的采集、编码、点对点、播放和界面已在真机上测过；**macOS 的代码只在 Windows 上做过类型检查，还没有在 Mac 上跑过**，第一次运行请按 [docs/macos-checklist.md](docs/macos-checklist.md) 逐步核对。

## 架构

```
┌──────────────── 主播的电脑 ──────────────────────────────────┐
│ HdrCast（开播）                                               │
│   采集引擎（同一个程序的 capture 模式）→ 管道                  │
│   → ffmpeg 编码 HEVC Main10 → MPEG-TS (stdout)                │
│   → 分发器：每位观众一个 remux ffmpeg（-c copy，SRT，127.0.0.1）│
│   → HostLink：[本机 IPv6]:端口 ⇄ 打洞 ⇄ 各位观众               │
└───────────────┬───────────────────────────────┬──────────────┘
                │ 点对点（打洞）                   │ 点对点（打洞）
                ▼                               ▼
      HdrCast（观看）→ mpv（日本）      HdrCast（观看）→ mpv（中国 / 澳洲）

   腾讯云 hdrcast-signal（signal/）：只存"谁在播 + 候选地址"，全部用组密钥签名；不转视频
```

- 只要**双方都有 IPv6** 就能连（IPv6 没有 NAT，打洞几乎总能成功）；IPv4 打洞是额外尝试。
- 主播要为每位观众各上传一份：3 人 × 40 Mbps ≈ 150 Mbps 上行（含重传余量）。界面会估算并在超过你填的上行带宽时提醒。

## 快速开始：开播的人

1. **Windows**：解压发行包，ffmpeg / ffprobe / mpv 已经在 `tools\` 里；运行 `HdrCast.exe`。
   **macOS**：把 `HdrCast.app` 拖进"应用程序"，再装工具：`brew install ffmpeg mpv`。第一次开播时系统会请求"屏幕与系统音频录制"权限。
2. 第一次打开选 **新建群组**：起个组名和你的名字，邀请码自动复制到剪贴板。把它发到微信 / LINE / Discord 群里（**只发给信得过的人**：有码就能看）。
   朋友已经发过你邀请码的话，复制那条消息后打开 HdrCast，它会自己提示加入。
3. 在 **开播** 页选显示器、码率、同时观看人数（可以顺手填上行带宽），点 **开始直播**。
   上方面板显示码率、帧率和丢帧；下面的列表里是谁在看、走 IPv6 还是 IPv4、网络 ping（不是画面延迟）以及每人的延迟设置。
   不点开始直播也可以：应用开着就是「在线」，组里的人可以点「连接」来请求观看，你点 **允许** 后才开始采集（见下面的「随时连接与远程控制」）。

## 快速开始：看的人

英文版给朋友看：[README.en.md](README.en.md)。

1. 装好同上（Windows 解压即用；macOS 需要 `brew install mpv`）。
2. **复制朋友发来的整条邀请消息**，打开 HdrCast：会弹出"要加入这个组吗"，点加入、填你的名字。
3. **观看** 页列出组里在线的电脑（每 10 秒刷新）：开播中的标 LIVE，点 **观看**；只是在线的点 **连接**，对方可能要先同意（这时会显示「等待同意」）。**延迟** 按你在哪选：日本 1.5 s / 澳洲 2 s / 中国 3.5 s。
4. 连不上时面板会直接说原因，比如"这台电脑没有 IPv6"或"主播没有 IPv6"。先去 <https://test-ipv6.com> 看看自己的 IPv6，见 [docs/faq.md](docs/faq.md)。
5. **HDR 显示器**：Windows 先打开 HDR（`Win + Alt + B`）；SDR 显示器把输出选成"SDR"。

## 随时连接与远程控制

**在线 / 开播**：HdrCast 开着、**开播 → 谁可以连接 → 开放连接** 打开（默认）时，这台电脑在组里显示为「在线」。有人点 **连接** 时按下面的规则处理，被放进来了才开始采集屏幕；最后一个人离开 20 秒后自动停。点 **开始直播** 则是对全组公开：除了设为"禁止"的，谁都可以直接看。

**谁可以连接**（开播页）：
- **组里其他人**：观看「直接允许 / 询问我 / 禁止」，控制「不允许 / 询问我 / 自动获得」。默认是询问我 + 不允许。0.3.0 及更早版本的朋友证明不了自己是哪台电脑，永远按这一条。
- **每台电脑一条规则**：按 **设备码**（如 `7KQ2-M9XD-4TPA`）认电脑，不按名字。**添加电脑…** 列出组里在线或最近请求过的电脑，选「我的电脑」（直接连 + 自动控制）或「信任的朋友」（直接看，控制要问你）。有人请求时点 **总是允许** 也会把那台电脑加进来。
- 每台电脑的设备码在 **设置 → 群组 → 这台电脑** 里。给某台电脑控制权之前，先在那台电脑上核对一下设备码。

**把自己的笔记本设成随时可以远程控制台式机**：
1. 两台电脑都装 HdrCast 0.3.1，并加入同一个组（复制同一个邀请码）。两台可以用不同的名字（设置 → 我的名字，比如"小明 笔记本"）。
2. 在台式机上：开播 → 谁可以连接 → **添加电脑…** → 找到笔记本（核对设备码）→ **我的电脑**。
3. 之后在外面打开笔记本的 HdrCast → 观看 → 台式机 → **连接**：不用台式机那边点任何东西，连上就能控制。台式机上 HdrCast 要一直开着。

**观众这边**：主播允许你自动控制时，连上就是控制状态，播放窗口里的鼠标和键盘会发到主播电脑（`Ctrl + Alt + Enter` 留给本地切全屏），可以点 **只看不控** 交还、再点 **接管控制** 拿回来；主播设成"询问我"时点 **请求控制**，主播同意后才生效。主播随时可以 **收回** 控制或 **断开** 你（被断开后一分钟内连不回去）。

- 画面延迟仍然是 SRT 延迟 + 播放器缓冲，远程控制时建议用「快速」预设（300 ms），并把 **设置 → 播放器缓冲** 调小。适合操作电脑、改设置，不适合打游戏（没有相对鼠标）。
- macOS 当主播时，需要在"系统设置 → 隐私与安全性 → 辅助功能"里允许 HdrCast。Windows 上，以管理员身份运行的程序窗口不接受注入，除非 HdrCast 也以管理员身份运行。
- 主播是 0.2.4 时没有远程控制；主播是 0.3.0 时没有设备码和"在线"，用它自己的「关闭 / 询问我 / 允许」。

## 信令服务器（只需部署一次）

`signal/` 是一个只用 Python 标准库的小服务，已部署在腾讯云上（`http://101.43.23.94:18650`，邀请码里写的就是它），部署 / 升级方法见 [signal/README.md](signal/README.md)：
`signal/deploy.sh --ssh ubuntu@101.43.23.94`，然后在腾讯云安全组放行 **TCP 18650** 和 **UDP 18651**（后者是 IPv4 打洞用的地址回显，可选）。
它看不到任何密码或组密钥，也伪造不了地址（所有消息用组密钥签名）；挂了的话，已经在看的人不受影响，只是新的人加入不了。0.3 没有改动它。

## 工具

| 工具 | 谁需要 | Windows | macOS |
|---|---|---|---|
| ffmpeg | 开播的人 | 发行包 `tools\` 自带（BtbN gpl 构建，带 libsrt 和 NVENC/AMF/QSV） | `brew install ffmpeg`（带 libsrt 和 VideoToolbox） |
| mpv | 看的人 | 发行包 `tools\` 自带（zhongfly 构建，带 libsrt） | `brew install mpv` |
| WebView2 | 界面 | Windows 11 自带；Windows 10 一般已随 Edge 装好 | 不需要（系统自带 WebKit） |
| Python 3 | 信令服务器 | – | Ubuntu 自带，只用标准库 |

Windows 上 `scripts\get-tools.cmd` 一键下载 / 更新 ffmpeg / mpv（`sender` / `viewer` / `both`，加 `force` 重新下载）；`scripts\check-tools.cmd` 检查编码器和 `srt://` 支持。
设置和日志在 `%APPDATA%\HdrCast\`（macOS：`~/Library/Application Support/HdrCast/`），与 0.2 共用同一份 `group.json`，升级后不用重新加入。
0.3.1 起多了 `device.json`（这台电脑的设备私钥：别人拿到它就能冒充这台电脑，不要发出去；换电脑时不要拷贝它）和 `access.json`（谁可以连接）。

## 目录结构

```
app/         HdrCast 应用（Tauri 2）：进程编排（采集 + ffmpeg、mpv）、命令、窗口；icons/ 是图标
ui/          界面（原生 ES 模块 + preact/htm，不需要 Node 构建）；mock.js 可以让它在浏览器里单独预览
crates/
  core/      平台无关的基础：地址解析、SRT URL、日志脱敏、进程管理、防火墙；群组、邀请码、密钥、签名消息
  p2p/       打洞、HostLink / ViewerLink、UPnP、分发、信令客户端、P2pHost / ViewerSession
  capture/   采集引擎：Windows（DXGI / D3D11 / WASAPI）与 macOS（ScreenCaptureKit）后端，共用同一个主循环
  media/     ffmpeg / mpv 的参数与输出解析、编码器档位、sender.json / receiver.json
  control/   远程控制：协议（ChaCha20-Poly1305）、按键映射、输入注入（SendInput / Quartz）
             （设备密钥、签名和控制密钥的协商在 core/src/group/device.rs，谁可以连接在 app/src/access.rs 和 share.rs）
signal/      信令服务器（Python 标准库）+ systemd 单元 + deploy.sh
scripts/     Windows 批处理：get-tools / check-tools / dump-check；ui-preview.mjs 预览界面
docs/        design.md（规格，§8 是点对点协议）、network.md、faq.md、windows-checklist.md、macos-checklist.md
build.sh     测试 + 构建 + 打包
sender/ receiver/ shared/ tests/   0.2 的 C# 源码，作为对照保留（Rust 版的测试逐字节对照过它的输出）
```

## 构建

需要 [Rust](https://rustup.rs)（1.85+）。Windows 另需 Visual Studio 的 C++ 生成工具；macOS 需要 Xcode 命令行工具和 `cargo install tauri-cli --version "^2" --locked`。

```bash
./build.sh              # cargo test + 信令服务器的 Python 测试 → 构建 → 打包到 dist/
./build.sh --no-test    # 跳过测试
cargo run -p hdrcast    # 直接运行（调试构建）
node scripts/ui-preview.mjs   # 只在浏览器里看界面（http://localhost:5179，用模拟数据）
```

产物：Windows 是 `dist/HdrCast-<版本>/`（`HdrCast.exe`、`tools/`、`scripts/`、`docs/`、`signal/`）和同名 zip；macOS 是 `dist/HdrCast.app` 和 dmg。
想不碰真实的群组和设置试一个构建：设环境变量 `HDRCAST_DATA_ROOT` 指向另一个文件夹。

## 文档

- [docs/design.md](docs/design.md) —— 设计定稿；§8 是点对点协议（邀请码、密钥派生、签名、信令 API、打洞包、§8.9 远程控制用的旁路通道）。
- [docs/network.md](docs/network.md) —— 点对点怎么穿过防火墙、谁需要什么、上行带宽、IPv6 自查。
- [docs/faq.md](docs/faq.md) —— 连不上怎么办（按面板提示查）、HDR 与 SDR、码率与带宽。
- [docs/windows-checklist.md](docs/windows-checklist.md)、[docs/macos-checklist.md](docs/macos-checklist.md) —— 在真机上逐步验证及预期输出。
- [signal/README.md](signal/README.md) —— 信令服务器部署。

## 许可

MIT，见 [LICENSE](LICENSE)。
