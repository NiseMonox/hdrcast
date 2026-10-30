# macOS 验证清单（采集引擎）

采集引擎的 macOS 后端（`crates/capture/src/mac/` + `src/unix/`，ScreenCaptureKit）是**在 Windows 上写的**：那台机器只能做
`cargo check --target aarch64-apple-darwin / x86_64-apple-darwin`（类型检查通过、clippy 干净），写的时候没法运行，也没法链接。
下面是在真 Mac 上的第一次运行，按顺序做；每一步写了命令、预期输出、以及它验证的是哪一条"没法在 Windows 上验证的假设"
（编号 A1…A20，汇总在文末，按出错可能性从高到低排）。

> **进度（2026-10-30）**：已经在真 Mac（Apple 芯片、macOS 26.5.2）上用 Mac 版（`fix/macos-first-run` 分支）做了一部分：
> 构建通过（没有 warning，没有 `Undefined symbols`），`hdrcast-core`、`hdrcast-media`、`hdrcast` 的 `cargo test` 通过；
> 第 2、3 步；第 12 步的构建、启动和找到工具；第 13 步观看 Windows 主播，HDR 正常。**其余各步还没做**，包括采集 dump 和音频
> （第 4–10 步）、开播（第 14 步）、远程控制（第 15 步）和控制模式（第 17 步）。main 在 Mac 上用不了（窗口出不来，Homebrew 的
> ffmpeg / mpv 不带 SRT），Mac 需要的修改只在那个分支上，照着做时请看那个分支里的这份清单。

**请把每一步的终端输出原样留下来**（`2> engine.log` 里的日志尤其重要）：引擎会在日志里把系统实际给它的东西打印出来
（HDR 预设的像素格式、第一帧的格式 / 每行字节数 / 色彩标签、音频格式），这些就是对假设的回答。

环境：macOS 12.3 以上（音频要 13，HDR 要 15 + Apple 芯片），**直接在这台 Mac 的"终端"里操作，不要用 SSH**
（SSH 会话连不上 WindowServer，也弹不出授权框）。

## 0. 准备

- [ ] Xcode Command Line Tools：`xcode-select --install`（提供链接器和 SDK）。
- [ ] Rust 1.85 以上：`curl https://sh.rustup.rs -sSf | sh`，然后 `rustc --version`。
- [ ] ffmpeg（要带 zscale，Homebrew 的就有）：`brew install ffmpeg`，`ffmpeg -filters | grep zscale` 有输出。
- [ ] 取到仓库的 `rust-rewrite` 分支，`cd` 到仓库根目录。下文所有命令都在仓库根目录执行。
- [ ] 命令按 **bash** 写（用到了 `$F` 的自动分词和 `%1` 作业号）；macOS 默认是 zsh，先执行一次 `bash` 再往下做。

## 1. 构建与单元测试

```sh
cargo build -p hdrcast-capture 2>&1 | tail -3          # 调试版：前几步都用它
cargo build -p hdrcast-capture --release 2>&1 | tail -3
cargo test  -p hdrcast-capture 2>&1 | grep -E '^test result|FAILED|panicked'
```

- [ ] 两次构建都 `Finished`，**没有 warning，没有 `Undefined symbols`**。
  - 验证 **A18**：代码引用的每个系统符号（`SCStreamFrameInfoStatus`、`kCGColorSpaceITUR_2100_PQ`、
    `kCGDisplayStreamYCbCrMatrix_ITU_R_709_2`、`kCVImageBuffer…`、`CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` …）在 SDK 里都存在。
    符号全部来自 objc2 绑定，没有手写的 `extern`；若链接失败，把缺的符号名发回来。
- [ ] `cargo test` 每一行都是 `ok`，0 failed。其中这些测试**此前从未运行过**：
  - `unix::fifo::tests::*`（7 个）——验证 **A11**：`O_WRONLY|O_NONBLOCK` 打开 FIFO 在没有读端时返回 `ENXIO`；非阻塞写 + `poll` 能把
    3 MB 数据完整按序送达；读端关闭后写返回 `EPIPE`（`BrokenPipe`）；canceller 能把卡在满管道上的写线程放出来；
    同名管道只能有一个属主（`flock`），被杀引擎留下的 FIFO 会被接管；路径上的普通文件不会被动。
    `a_name_has_one_owner…` 要等 2 s 属正常。
  - `unix::process::tests::*`（3 个）、`unix::signals::tests::*`（1 个：`raise(SIGTERM)` 触发 stop）、`unix::time::tests::*`。
  - `mac::timer::tests::a_sleep_ends_on_time…`——验证 **A13**：`PreciseSleep` 的误差 < 2 ms（正常应在 0.1 ms 量级）。
    失败时信息里有 `overslept by …`，把数值发回来。
  - `timer::tests::*`、`tests/engine.rs`（11 个，假后端上的引擎循环）在 macOS 的计时器上也要通过。
- [ ] 以后把 `B=target/debug/hdrcast-capture` 记作引擎（`export B=$PWD/target/debug/hdrcast-capture`）。
  **第 2–8 步先用调试版**：调试版里 objc2 会在运行时把每一次 Objective-C 调用的参数 / 返回值编码与系统里的真实方法签名比对，
  不一致就 panic 并说明是哪个方法（验证 **A17**）。日志里出现 `unhandled error:` 或 `a capture callback panicked:` 就是这种情况，把整段发回来。

## 2. `$B capture --selftest`

不采集、不弹任何授权框，只报告这台 Mac 的能力。预期（数值随机器而变）：

```
selftest: the GPU colour self-test is Windows-only (on macOS ScreenCaptureKit converts the colours; ...)
macOS 15.1 on Apple silicon
  ScreenCaptureKit (macOS 12.3+):        yes
  system audio capture (macOS 13+):      yes
  HDR capture (macOS 15+, Apple silicon): yes
  screen recording permission:           NOT granted - allow it in System Settings > ...
displays:
idx  device         name                      size        pos          hz     hdr  ...
0    1              Built-in Retina Display   3024x1964   0,0          120    on   ...
selftest: SKIPPED - nothing to verify without capturing; see docs/macos-checklist.md
```

- [ ] 退出码 0（`echo $?`）。
- [ ] 版本号、芯片与"关于本机"一致；三行 yes/no 与系统版本相符（macOS 14：HDR 一行是 `no - HDR capture needs macOS 15 or later`；
      Intel：`no - HDR capture needs a Mac with Apple silicon`）。这些能力是运行时用 `respondsToSelector` / 系统版本查出来的，
      不是按编译时的 SDK 假定的；与实际不符就把输出发回来。

## 3. `$B list-outputs` 与 `$B list-outputs --json`

- [ ] **不弹授权框**（枚举不需要录屏权限）。
- [ ] 每个显示器一行，**主显示器（有菜单栏的那个）是 idx 0**，其余按 `device` 数值升序。插拔外接屏后再跑，顺序规则不变。
- [ ] `device` 是十进制的 `CGDirectDisplayID`（内建屏通常是 `1`）；`name` 是"系统设置 → 显示器"里的名字。
      若 `name` 为空且日志有 `displays: the main thread does not answer`，说明 `NSScreen` 没问到——命令行下不该出现，出现就发回来。
- [ ] `size` 是**像素**不是点：内建 Retina 屏应是面板分辨率（如 14" MacBook Pro 默认缩放下 `3024x1964`）；
      在"显示器"里换一个缩放档位（"更多空间"）再跑，`size` 应变成该档位的后备缓冲尺寸（点数 × 2）。
      有旋转 90° 的外接屏的话：`size` 应是旋转后的宽高（竖屏 = 高大于宽），表里带 `r90` / `r270`。验证 **A10**。
- [ ] `hz`：ProMotion 屏 120，普通屏 60，外接屏是其当前刷新率（不是 0）。
- [ ] `hdr`：XDR 内建屏为 `on`；外接 HDR 显示器在"显示器 → 高动态范围"打开时 `on`、关掉后 `off`；
      **不少非 XDR 的 Mac 内建屏也会是 `on`**（它们支持 EDR，`maximumPotentialExtendedDynamicRangeColorComponentValue` 约为 2）——
      这是按定义得到的结果，请记下你的机器上每块屏的 `hdr` 值。macOS 14 及更早、或 Intel 机器上应全部是 `off`
      （`hdr` 的含义是"引擎会把这块屏采成 HDR10"，App 据此预告格式；真正告诉 ffmpeg 读什么的是引擎 stdout 的第一行，见第 7 步）。验证 **A9**。
- [ ] `--json` 输出同样内容（`"deviceName":"1"`、`"hdr":true` …）。

## 4. 录屏权限

第一次运行任何 `capture`（不带 `--selftest`）：

```sh
$B capture --output 0 --format nv12 --fps 30 --size 1280x720 --audio off --dump /tmp/hc-sdr.nv12 --seconds 1; echo "exit=$?"
```

- [ ] 系统弹出"'终端'想要录制此电脑的屏幕与音频"（从终端启动时，被授权的是**终端 App**，不是 hdrcast-capture；
      由 HdrCast.app 启动时才是 HdrCast）。引擎立即退出，日志最后一行：
      `[E] capture: macOS has not allowed this program to record the screen; allow it in System Settings > Privacy & Security > Screen & System Audio Recording ...`，
      `exit=3`。验证 **A12**。
- [ ] 到 系统设置 → 隐私与安全性 → 屏幕与系统音频录制（macOS 14 及更早叫"屏幕录制"）打开"终端"，**退出并重开终端**。
- [ ] `$B capture --selftest` 里 `screen recording permission` 变成 `granted`。
- macOS 15 起，系统会定期（约每月）再弹一次"…正在绕过系统私有窗口选取器直接访问屏幕"的确认框；这是 Apple 对直接采集的统一做法，不是故障。
  采集期间菜单栏会出现录屏指示。

## 5. SDR：1 秒 dump

**先别碰鼠标和键盘**（让屏幕静止），执行第 4 步那条命令，再执行：

```sh
ls -l /tmp/hc-sdr.nv12
F='-f rawvideo -pix_fmt nv12 -video_size 1280x720 -framerate 30 -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv'
ffmpeg -hide_banner -loglevel error -y $F -i /tmp/hc-sdr.nv12 -frames:v 1 -pix_fmt rgb24 /tmp/hc-sdr-1.png
ffmpeg -hide_banner $F -i /tmp/hc-sdr.nv12 -frames:v 1 -vf "signalstats,metadata=mode=print:key=lavfi.signalstats.YMIN,metadata=mode=print:key=lavfi.signalstats.YAVG,metadata=mode=print:key=lavfi.signalstats.YMAX" -f null - 2>&1 | grep signalstats
open /tmp/hc-sdr-1.png
```

预期日志（stderr）：

```
[I] capture: output 0 (display 1 Built-in Retina Display) 3024x1964 rot0 hdr=True -> nv12 1280x720 @30fps
[I] capture: first picture '420v' 1280x720, bytes per row 1280 / 1280, matrix=ITU_R_709_2 primaries=... transfer=...
```

stdout 每秒一行 JSON 统计，结束时再一行。

- [ ] `exit=0`，几百毫秒内就开始出统计。验证 **A6**：没有 `NSApplication`、没有 run loop 的进程里 ScreenCaptureKit 的回调照样到达。
      若卡 10 s 后报 `ScreenCaptureKit did not list the displays within 10s` / `did not start the capture within 10s`，这条假设错了。
- [ ] 文件大小是 `1382400`（= 1280×720×1.5）的整数倍，约 30 帧（29–31）。验证紧凑布局（Y 平面 + 交错 UV，无行填充）。
- [ ] 日志里有 `first picture '420v' 1280x720` 一行。`bytes per row` 两个数 ≥ 1280（大于 1280 说明有行填充，逐行拷贝的路径被走到）；
      `matrix=ITU_R_709_2`。若出现 `[W] capture: the pictures are tagged matrix=…`，把那一行发回来。
- [ ] **屏幕静止时第 1 帧的 PNG 就是桌面，不是黑屏**。验证 **A5**：流一开始就会送来一张带画面的帧（状态为 complete 或 started）。
      若是黑的：把 `-frames:v 1` 换成 `-vf "select=eq(n\,29)" -frames:v 1` 看最后一帧，并动着鼠标重录一次对比，结果都发回来。
- [ ] PNG 颜色正确（红绿蓝不串、不偏色、不发灰），鼠标指针在画面里（`--cursor off` 重录则没有）。
- [ ] 画面**铺满 1280×720、四周没有黑边**（内建屏是 16:10，这里故意要了 16:9，画面应被拉伸而不是加边）。验证 **A14**
      （`scalesToFit` + `preservesAspectRatio = NO`，后者要 macOS 14；macOS 12.3–13 上会保持比例、留黑边，属已知差异）。
- [ ] 把一个纯白窗口（如空白的文本编辑）铺满屏幕再录：`YMAX` = 235、`YMIN` 接近 16（limited range）。`YMAX` 是 255 就说明给的是 full range。
- [ ] 原生尺寸再来一次（去掉 `--size`，ffmpeg 的 `-video_size` 换成第 3 步的 `size`，宽高为奇数时向下取偶）：
      文件大小 = W×H×1.5×帧数，PNG 清晰度与屏幕一致（不是被缩放过的）。
- [ ] 统计行：静止时 `captured` 很小、`duplicated` 增长；录制期间拖动窗口则 `captured` 接近 30/s。`gpu_ms`（这里是拷贝耗时）在 720p 下远小于 1。

## 6. 停止、残留管道、父进程

```sh
V="${TMPDIR%/}/hc-v.fifo"
$B capture --output 0 --format nv12 --fps 30 --size 1280x720 --audio off --video-pipe hc-v --pipe-timeout 60 2> /tmp/hc-engine.log &
sleep 2; ls -l "$V"                       # prw-------  ... hc-v.fifo ，旁边还有 hc-v.fifo.lock
ffmpeg -hide_banner -loglevel error -f rawvideo -pix_fmt nv12 -video_size 1280x720 -framerate 30 -i "$V" -f null - &
sleep 3; kill -INT %1; wait %1; echo "exit=$?"; ls "$V" 2>&1
```

- [ ] FIFO 在引擎启动后约 1 s 内出现，权限 `prw-------`。
- [ ] `kill -INT` 后 `exit=0`，日志没有报错，`ls` 报 `No such file`（FIFO 已删除），ffmpeg 自己正常结束（读到 EOF）。验证 **A20**（信号 → stop）与 **A8**
      （先摘输出、再 `stopCapture`、不死锁；整个退出应在 1 s 内）。
- [ ] 把 `kill -INT` 换成 `kill -9` 重做：FIFO 留在原地；**再启动一次同名引擎**，它应正常启动并能被 ffmpeg 读到（接管残留 FIFO）。
- [ ] 两个引擎同时用同一个 `--video-pipe` 名：第二个约 2 s 后退出，
      `cannot create the video pipe …: another capture engine is serving a pipe of this name`。
- [ ] 让 ffmpeg 先退出（`kill %2`）：引擎日志 `capture: the pipe consumer went away: Broken pipe (os error 32)`，退出码 4。

## 7. HDR dump（需要 macOS 15+、Apple 芯片、`hdr` 为 `on` 的屏）

这是**最没把握**的一步。屏幕上放一段 HDR 视频（YouTube 的 HDR 片源，或"照片"里的 HDR 照片）和一个白色窗口，然后：

```sh
$B capture --output 0 --format p010 --fps 30 --size 1920x1080 --audio off --dump /tmp/hc-hdr.p010 --seconds 2 2> /tmp/hc-hdr.log; echo "exit=$?"
cat /tmp/hc-hdr.log
```

预期日志（三种结局都"能用"，请看是哪一种）：

```
[I] capture: output 0 (...) hdr=True -> p010 1920x1080 @30fps
[I] capture: the system's HDR preset uses pixel format '....', colour space kCGColorSpaceITUR_2100_PQ, matrix ....
[I] capture: first picture 'x420' 1920x1080, bytes per row 3840 / 3840, matrix=ITU_R_2020 primaries=ITU_R_2020 transfer=SMPTE_ST_2084_PQ
[I] capture: HDR pictures arrive as 'x420' = p010
```

- [ ] **记下 `the system's HDR preset uses …` 整行**——这是 Apple 自己为"给别的屏幕看的 HDR 流"选的像素格式和矩阵，是判断后面一切的依据。
- [ ] 结局一（最好）：`HDR pictures arrive as 'x420' = p010`。验证 **A1**：ScreenCaptureKit 肯给 `'x420'`
      （10 bit 4:2:0 video range，本身就是 P010）。头文件列出的格式里**没有**它，所以引擎会等第一帧来确认。
- [ ] 结局二：`[W] capture: 'x420' = p010 does not work here (…); trying 'xf44' converted to p010 on the CPU`，随后
      `HDR pictures arrive as 'xf44' converted to p010 on the CPU`。说明 A1 不成立，走了头文件里写明支持的 `'xf44'`
      （10 bit 4:4:4 full range），由 CPU 转成 P010（`crates/capture/src/repack.rs::xf44_to_p010`，Windows 上有单元测试）。
      把括号里的原因原样发回来（是启动被拒 `SCStreamErrorDomain -38xx`，还是"delivered 'BGRA' …"）。
- [ ] 结局三：两种都不行，最后 `[W] capture: HDR capture does not work on this system; capturing SDR instead …`。画面仍能用，但是 SDR；
      此时文件是 nv12，下面的检查改用第 5 步的命令。把整份日志发回来。
- [ ] `first picture` 一行的三个标签应为 `matrix=ITU_R_2020 primaries=ITU_R_2020 transfer=SMPTE_ST_2084_PQ`。验证 **A2**：
      CoreGraphics 里**没有** BT.2020 的 `kCGDisplayStreamYCbCrMatrix` 常量（只有 709 / 601 / 240M），所以引擎没有设置 `colorMatrix`，
      靠系统按色彩空间自己选。若出现 `[W] … tagged matrix=ITU_R_709_2 where ITU_R_2020 …`：画面可用但色相会偏
      （ffmpeg 按 bt2020nc 标记，数据却是 709 矩阵）；把这一行发回来，修法是让 ffmpeg 那边按实际矩阵标记，或换一种配置方式。
      标签是 `-`（没有标签）则无法从日志判断，以下面的 PNG 为准。

检查文件（结局一、二）：

```sh
F='-f rawvideo -pix_fmt p010le -video_size 1920x1080 -framerate 30 -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv'
ls -l /tmp/hc-hdr.p010                                           # 6220800 (=1920*1080*3) 的整数倍，约 60 帧
ffmpeg -hide_banner $F -i /tmp/hc-hdr.p010 -frames:v 1 -vf "signalstats,metadata=mode=print:key=lavfi.signalstats.YMIN,metadata=mode=print:key=lavfi.signalstats.YAVG,metadata=mode=print:key=lavfi.signalstats.YMAX" -f null - 2>&1 | grep signalstats
ffmpeg -hide_banner -loglevel error -y $F -i /tmp/hc-hdr.p010 -frames:v 1 -pix_fmt rgb48be /tmp/hc-hdr-pq-raw.png
ffmpeg -hide_banner -loglevel error -y $F -i /tmp/hc-hdr.p010 -frames:v 1 -vf "zscale=transfer=linear:npl=100,format=gbrpf32le,zscale=primaries=bt709,tonemap=tonemap=hable:desat=0,zscale=transfer=bt709:matrix=bt709:range=pc,format=rgb24" /tmp/hc-hdr-tonemapped.png
open /tmp/hc-hdr-pq-raw.png /tmp/hc-hdr-tonemapped.png
```

- [ ] `YMIN` 接近 64，`YMAX` ≤ 940（10 bit limited range）。全是 0 / 全相同 / 超过 940 → 位布局或量程不对
      （`'x420'` 应是 16 bit 小端、10 bit 在高位；`'xf44'` 是 full range，由引擎换算成 limited）。
- [ ] **只有白色 SDR 窗口时的 `YMAX` 请记下来**，这是 **A3**：Apple 的"canonical display" HDR 把 SDR 白放在多少 nit 没有文档。
      对照：490 = 80 nit（Windows 引擎的默认）、509 = 100 nit、572 = 203 nit（BT.2408 参考白）。
      画面里有 HDR 高光时 `YMAX` 应明显更高。若白窗口和 HDR 高光的 `YMAX` 一样 → 实际采到的是 SDR（`captureDynamicRange` 没起作用）。
- [ ] `hc-hdr-pq-raw.png` 发灰发平——正常（PQ 信号直接当 sRGB 看）。
- [ ] `hc-hdr-tonemapped.png` 看起来就是你的桌面：**色相正确**（红是红、肤色正常）。若偏色，把 `F` 里的 `-colorspace bt2020nc` 换成
      `-colorspace bt709` 再出一张对比，哪张对就说明数据实际用的是哪个矩阵（对应 A2）。
- [ ] `--format auto` 在 `hdr=on` 的屏上应与 `--format p010` 走同一条路，区别只在退路：`auto` 在 `x420`、`xf44` 都不行时退成 nv12 照常录，
      `p010` 则报错退出（`this display cannot be captured as p010 here (it would come out as nv12)`，不建管道）。
      两种情况下 stdout 第一行都是格式声明，例如 `{"layout":{"format":"p010","width":1920,"height":1080}}`——**App 就是按这一行启动 ffmpeg 的**，
      请确认它和日志里 `-> p010 1920x1080` / `HDR pictures arrive as …` 一致（`$B capture … --format auto … --dump … | head -1`）。
      在 `hdr=off` 的屏上 `--format p010` 会先打印
      `[W] capture: --format p010 was asked for, but the display is not in an HDR (extended dynamic range) state; capturing SDR instead …`，然后同样报错退出。
- [ ] 换成原生尺寸、60 fps、release 版（`target/release/hdrcast-capture`）录 3 秒：统计里 `fps`≈60、`dropped` 不增长，
      `gpu_ms`（拷贝 / 转换耗时）：`'x420'` 路径 4K 下应在 2–4 ms，`'xf44'` 路径会高（若 > 10 ms 请报）。

## 8. 音频（需要 macOS 13+）

播放一段有节奏的音乐或视频，然后（引擎写两个 FIFO，ffmpeg 两路都读，录 10 秒）：

```sh
V="${TMPDIR%/}/hc-v.fifo"; A="${TMPDIR%/}/hc-a.fifo"
$B capture --output 0 --format nv12 --fps 30 --size 1280x720 --video-pipe hc-v --audio-pipe hc-a > /tmp/hc-stats.jsonl 2> /tmp/hc-av.log &
while [ ! -p "$V" ] || [ ! -p "$A" ]; do sleep 0.1; done
ffmpeg -hide_banner -loglevel error -y \
  -thread_queue_size 1024 -f rawvideo -pix_fmt nv12 -video_size 1280x720 -framerate 30 -i "$V" \
  -thread_queue_size 1024 -f f32le -ar 48000 -ac 2 -i "$A" \
  -t 10 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -c:a aac -b:a 192k /tmp/hc-av.mp4
wait %1; echo "exit=$?"
cat /tmp/hc-av.log; tail -2 /tmp/hc-stats.jsonl
ffmpeg -hide_banner -i /tmp/hc-av.mp4 -vn -af volumedetect -f null - 2>&1 | grep -E 'mean_volume|max_volume'
open /tmp/hc-av.mp4
```

预期日志：

```
[I] capture: waiting for the video pipe client on /var/folders/.../T/hc-v.fifo (30s)
[I] capture: video pipe connected
[I] audio: pipe client connected
[I] audio: ScreenCaptureKit delivers Float 48000 Hz 2 ch 32 bit, one buffer per channel, 480 frames per buffer
...
[E] capture: the pipe consumer went away: Broken pipe (os error 32)        ← ffmpeg 录满 10 s 退出，exit=4，正常
```

- [ ] 有 `audio: ScreenCaptureKit delivers …` 一行，请原样记下（格式、是否 `one buffer per channel`、每个缓冲多少帧）。验证 **A7**：
      音频输出在流启动前就加上、之后才接上消费者也能收到数据；格式是 Float32 / 48 kHz / 立体声；
      `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` 的"先问大小再取"两步调用可用。
      若出现 `[W] audio: … such buffers are left out`，把那一行发回来（里面有系统实际给的格式）。
- [ ] `max_volume` 明显高于 -91 dB（-91 dB = 全是静音）；`hc-av.mp4` 里**能听到刚才播放的声音，左右声道没反，音画同步**
      （拍手 / 鼓点对得上；引擎的对齐容差是 150 ms）。
- [ ] 统计最后一行的 `audio_underrun`：播放期间应基本不增长（10 s 内个位数）。再做一次**全程静音**的录制，看它是否持续增长——
      如果静音时系统不送音频缓冲，引擎会自己补零，`audio_underrun` 会涨，这属于预期，但请把两次的数值都记下。
- [ ] `--audio-exclude Music` 之类的参数：日志一行 `audio: leaving one program out (…) is not supported on macOS yet; capturing all of the system audio`，其余照常。
- [ ] `$B audio-devices` 没有任何输出（macOS 上没有设备可选），退出码 0。
- [ ] 引擎自己不出声，所以 `excludesCurrentProcessAudio` 在这里看不出效果；等 App 接上后确认 App 自己的提示音不会被采进去（**A19**）。

## 9. 流被系统中断（可选，但值得做一次）

按第 8 步的方式启动（ffmpeg 的 `-t` 改成 120），过程中依次：

- [ ] 在"显示器"里改一次分辨率 / 缩放档位。两种结果都可以：画面不断（ScreenCaptureKit 自己适应，输出尺寸不变）；或日志出现
      `capture: acquire failed: ScreenCaptureKit ended the stream: …` → `desktop duplication lost …; retrying` → `capture: duplication restored`，
      mp4 里画面短暂重复后恢复，**声音在恢复后还在**。验证 **A15**。请把 `ended the stream:` 后面的错误码记下来。
- [ ] 让显示器睡眠再唤醒（热角或合盖几秒）：同上；超过 30 s 没恢复则引擎以退出码 5 结束（与 Windows 行为一致）。
- [ ] 点菜单栏的录屏指示 → 停止共享：引擎应**不再重连**，日志
      `capture: conversion failed: the capture was stopped from the macOS screen-sharing menu (…)`，退出码 3。
- [ ] 拔掉正在采集的外接屏：`retry failed: display N is not available for capture …` 每 0.5 s 一次，30 s 后退出码 5；期间插回去则恢复。

## 10. 4K / 5K 60 fps 吞吐（release 版）

macOS 的 FIFO 内核缓冲只有 16–64 KiB 且不能调大（Windows 上管道缓冲是 64 MB），一帧 4K P010（25 MB）要分几百次写完。验证 **A16**：

```sh
B=$PWD/target/release/hdrcast-capture; V="${TMPDIR%/}/hc-v.fifo"
SIZE=3840x2160      # 第 3 步 list-outputs 里这块屏的 size（奇数向下取偶）
FMT=nv12 PIX=nv12   # 第二轮改成 FMT=p010 PIX=p010le（仅当第 7 步是结局一或二）
$B capture --output 0 --format $FMT --size $SIZE --fps 60 --audio off --video-pipe hc-v > /tmp/hc-stats.jsonl 2> /tmp/hc-perf.log &
while [ ! -p "$V" ]; do sleep 0.1; done
ffmpeg -hide_banner -loglevel error -thread_queue_size 1024 -f rawvideo -pix_fmt $PIX -video_size $SIZE -framerate 60 -i "$V" -t 20 -c:v hevc_videotoolbox -b:v 40M -f null -
wait %1; head -3 /tmp/hc-perf.log; tail -5 /tmp/hc-stats.jsonl
```

- [ ] 日志第一行 `-> nv12 3840x2160 @60fps`（或 `-> p010 …`）与 `FMT` / `SIZE` 一致——不一致时 ffmpeg 读到的就是乱码，本轮作废。
- [ ] `fps` 稳定在 60 左右，`dropped` 不持续增长，`pipe_queue_ms` 不长期顶在 66（= 队列 4 帧全满）。
      若 `dropped` 一直涨而编码器并不慢（把 `-c:v hevc_videotoolbox …` 换成 `-c:v rawvideo` 也一样），瓶颈就是 FIFO，
      需要换传输方式（Unix 域套接字 / 共享内存），把统计发回来。
- [ ] "活动监视器"里 `hdrcast-capture` 的 CPU：记下数值（管道写入 + 每帧一次拷贝，预计 4K60 下占一个核的相当一部分）。

## 11. 出问题时发回什么

- 出问题那一步的**完整** stderr 日志和 stdout 统计，以及 `$B capture --selftest`、`$B list-outputs` 的输出、`sw_vers`。
- 崩溃（`Segmentation fault` / `abort`）：`~/Library/Logs/DiagnosticReports/hdrcast-capture-*.ips` 里的崩溃线程调用栈。
- 系统侧的 ScreenCaptureKit 日志：`log show --last 2m --predicate 'subsystem == "com.apple.screencapturekit" OR process == "replayd"' | tail -100`。

---

## 假设一览（按出错可能性从高到低）

| # | 假设 | 哪一步验证 | 错了的表现 / 去哪改 |
|---|---|---|---|
| A1 | HDR 用 `'x420'`（`kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange`）ScreenCaptureKit 肯给。**头文件只列了 BGRA / l10r / 420v / 420f / xf44 / RGhA** | 7 | 自动退到 `'xf44'` + CPU 转换，再不行退到 SDR；`mac/video.rs::plans`、`mac/output.rs::PixelPlan` |
| A2 | HDR 的 YCbCr 矩阵是 BT.2020。任务里写的 `kCGDisplayStreamYCbCrMatrix_ITU_R_2020` **不存在**，所以没设 `colorMatrix`，沿用系统 HDR 预设的值 | 7（`first picture` 行的 `matrix=`） | 日志有 `tagged matrix=…` 警告、色相偏；`mac/stream.rs::configuration` |
| A3 | `captureDynamicRange = HDRCanonicalDisplay` + `kCGColorSpaceITUR_2100_PQ` 得到的就是 HDR10；SDR 白的亮度未知（Windows 引擎是 80 nit） | 7（白窗口的 `YMAX`） | 观众端 SDR 内容偏亮 / 偏暗，需要在编码或播放侧补偿 |
| A4 | 从 `streamConfigurationWithPreset:`（HDR canonical 预设）得到的配置可以再改宽高 / 像素格式 / 帧率 | 7 | 启动被拒或格式不对 → 走 A1 的退路；`mac/stream.rs::configuration` 里改回 `SCStreamConfiguration::new()` |
| A5 | 流启动后立刻有一张带画面的帧（`complete` 或 `started`），静止屏幕也一样；idle 帧没有像素 | 5（静止录制第 1 帧不黑） | 开头黑屏直到屏幕有变化；`mac/output.rs::on_picture` |
| A6 | 没有 `NSApplication` / run loop 的子进程里 ScreenCaptureKit 可用，完成回调在它自己的队列上到达 | 5 | 10 s 超时；需要在引擎进程里起 run loop |
| A7 | 音频：流启动前加音频输出、之后才接消费者可行；Float32 48 kHz 2 ch（通常非交错）；两步取 `AudioBufferList` | 8 | 没声音 / `audio_underrun` 狂涨 / `such buffers are left out`；`mac/audio.rs` |
| A8 | `removeStreamOutput` / `stopCapture` 不会和回调队列互相等待；丢弃顺序（音频 → 管道 → 视频）下流被正确停掉 | 6 | 退出卡住（最多 2 s 的等待之外）；`mac/stream.rs::Session` |
| A9 | 命令行进程主线程上直接用 `NSScreen`（无 `NSApplication`）可行；`deviceDescription["NSScreenNumber"]` 是 `CGDirectDisplayID`；`maximumPotential…ColorComponentValue > 1` 表示"现在能显示 HDR" | 2、3 | 名字为空 / `hdr` 不对；`mac/displays.rs` |
| A10 | 像素尺寸 = `CGDisplayBounds`（点）×（当前模式像素宽 ÷ 点宽），旋转 / 缩放档位下也对 | 3 | `size` 与实际不符 → 原生尺寸采集被缩放；`mac/displays.rs::describe` |
| A11 | FIFO：非阻塞打开的 `ENXIO`、`poll` + 非阻塞写、`EPIPE`、`flock` 属主锁 | 1（单元测试）、6 | 测试失败；`unix/fifo.rs` |
| A12 | 权限：`CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess`；授权记在"负责进程"（终端或 HdrCast.app）上；授权后需重启进程 | 4 | 不弹框 / 授权后仍被拒 |
| A13 | `PreciseSleep` = `nanosleep` + 300 µs 自旋，线程设为 user-interactive QoS 后唤醒延迟远小于 1 ms | 1、10（`fps` 稳定） | 帧间隔抖动；`mac/timer.rs` |
| A14 | `scalesToFit = YES` + `preservesAspectRatio = NO`（macOS 14+）让画面铺满任意输出尺寸 | 5 | 黑边 / 画面只占一角；`mac/stream.rs::configuration` |
| A15 | 系统中断流时走 `stream:didStopWithError:`；重建一条流即可恢复；用户点"停止共享"的错误码是 -3817 | 9 | 中断后不恢复，或用户停止后被重新连上 |
| A16 | FIFO 的吞吐够 4K60 P010（1.5 GB/s） | 10 | `dropped` 持续增长 |
| A17 | `define_class!` 定义的输出 / 委托对象的方法签名与协议一致；所有 objc2 绑定的签名与系统一致 | 调试版的每一步 | 调试版 panic 并指出方法名 |
| A18 | 链接：所有符号存在；按现在的链接方式可执行文件需要 macOS 12.3+ 才能启动（ScreenCaptureKit 是强链接） | 1 | 链接失败；老系统上启动即崩 |
| A19 | `excludesCurrentProcessAudio`、`showsCursor`、`queueDepth = 3`、`minimumFrameInterval = 1/fps` 按文档工作 | 5、8、10 | 光标 / 帧率 / 回声异常 |
| A20 | SIGINT / SIGTERM / SIGHUP 处理函数里只做一次原子写；`SA_RESTART` | 1、6 | Ctrl+C 不能干净退出 |

---

# 第二部分：整个应用（HdrCast.app）

上面只验证了采集引擎。应用的其余部分（界面、点对点、mpv 编排、远程控制）是平台无关的 Rust，已经在 Windows 上实测过；
macOS 特有的只有下面几处，写的时候同样只做过类型检查（`cargo check -p hdrcast --target aarch64-apple-darwin`；真机上做过哪些见开头的进度）。
采集引擎的第 1–8 步通过之后再做这一部分。

## 12. 构建与启动

```sh
brew install ffmpeg mpv
cargo install tauri-cli --version "^2" --locked
./build.sh --no-test          # 产物：dist/HdrCast.app 和 dmg；或者先 cargo run -p hdrcast 直接跑调试版
open dist/HdrCast.app
```

- [ ] 构建通过。失败时把 `dist/logs/build.log` 末尾发回来（最可能出问题的是 `app/src/window.rs` 的 macOS 分支和 `app/tauri.conf.json` 的 bundle 配置）。
- [ ] 窗口：左上角是系统的红黄绿按钮，没有标题文字；侧边栏是半透明的毛玻璃（跟随窗口激活状态）；右上角**没有** Windows 式的最小化 / 关闭按钮；
      拖动侧边栏顶部或内容区顶部能移动窗口。浅色 / 深色跟随系统，**设置 → 外观** 能强制切换（毛玻璃也跟着变）。
- [ ] 没有自签名 / 公证，第一次打开要在访达里右键 → 打开（或 `xattr -dr com.apple.quarantine dist/HdrCast.app`）。
- [ ] **设置 → 工具**：ffmpeg、mpv 都显示"已找到"，路径是 `/opt/homebrew/bin/…`（Intel Mac 是 `/usr/local/bin/…`）。
- [ ] 设置和日志在 `~/Library/Application Support/HdrCast/`；**设置 → 设置与日志 → 打开文件夹** 用访达打开它。
- [ ] 没有"防火墙"一栏（那是 Windows 防火墙的）；**开播 → 声音** 里没有"排除程序"。

## 13. 观看（主播是 Windows）

- [ ] 复制邀请消息 → 欢迎页点 **我有邀请码** → 加入。（macOS 上不会自动读剪贴板，所以不会弹"剪贴板里有邀请码"。）
- [ ] **观看** 页 10 s 内列出正在直播的 Windows 主播 → **观看** → 面板 `Connected to …` → mpv 窗口出画面、有声音。
      - 验证：`crates/p2p` 的 macOS 地址选择（日志 `[p2p] this PC offers: [2400:…]:NNNNN, 192.168.x.x:NNNNN`，IPv6 应是**非临时**地址；
        `ifconfig | grep inet6` 里带 `temporary` 的那个不应该被选中。选中了也能连，只是地址每天会变）。
      - 验证：mpv 的 IPC 走 Unix 套接字（日志 `[ipc] IPC: connected to /…/hdrcast-mpv-<pid>.sock`），面板的缓冲 / 帧率 / 码率在刷新。
- [ ] 面板里硬解是 `videotoolbox`。显示 `no` 或空 → mpv 的 `--hwdec=videotoolbox` 在这个 mpv / 这个 GPU 上下文里没生效：把 **设置 → 硬件解码** 改成 `videotoolbox-copy` 再试，结果发回来。
- [ ] HDR：在 XDR / HDR 显示器上，输出选"自动"或"HDR"，面板"丢帧"格下面显示 **HDR 直通**，高光明显比 SDR 亮。
      不亮 → 把 mpv 的日志（**日志** 页筛"播放器"）发回来；macOS 上 mpv 的 HDR 直通可能还需要 `--gpu-api=vulkan` 之类的选项，
      现在的参数表（`crates/media/src/receiver/mpv_command.rs`）故意没加任何没把握的选项。
- [ ] **全屏** 按钮、**停止** 按钮可用；停止后 mpv 进程消失（`pgrep mpv` 无输出）。直接退出 HdrCast 时 mpv 也跟着退出。
- [ ] 悬浮窗（design.md §3.3）：观看时 mpv 画面的右上角有个小圆按钮，单击展开成面板，"—" 收起；面板里 **静音**、**解码方式** 立即生效。
      圆按钮的四角应该是透明的（看得到后面的东西）；是黑色方块 → 透明窗口在这里不生效，截图发回来。跟着播放器走、拖动、断开连接见第 18 步。
- [ ] 播放窗口里滚轮、空格、`q` 都没反应（`--input-default-bindings=no`）；`Ctrl+Option+Enter` 切全屏；点窗口的关闭按钮 = 停止观看。

## 14. 开播（观众是 Windows）

- [ ] **开播** 页的显示器卡片：名字、像素尺寸、刷新率正确；XDR 屏带 HDR 徽标。编码器列表是 `VideoToolbox` 和 `x265`。
- [ ] 点 **检测**：VideoToolbox 显示 ok（日志 `[probe] hevc_videotoolbox: ok`）。失败时把那一行发回来（参数表在 `crates/media/src/sender/encoder_profiles.rs`）。
- [ ] **开始直播**：第一次会弹"屏幕与系统音频录制"授权 → 允许 → **退出并重新打开 HdrCast** → 再开播。
      日志依次出现 `[run] …HdrCast capture …`、`[capture] … first picture …`、`[state] streaming: Streaming`；面板采集 ≈ 60 fps，编码 speed ≈ 1.0×。
      - 4K / 5K 屏如果"丢帧"持续增长：是第 10 步说的 FIFO 吞吐问题，先把分辨率降到 1440p 确认，再把统计发回来。
- [ ] Windows 观众看到的画面颜色正常（SDR 屏开播：不偏色；HDR 屏开播：面板 source 是 `bt.2020 / pq`，白色窗口不刺眼）、声音同步。
- [ ] HDR 屏、HDR 选"自动"时，日志有 `[run] sending HEVC (Apple VideoToolbox) … HDR10`，面板"丢帧"格下面是 `p010 / HDR10 …`。
- [ ] 系统给不了 HDR 采集时（第 7 步是结局三的机器）：照常开播，只是发 SDR——日志 `[capture] … capturing SDR instead …`、
      `[run] sending … SDR`、`[run] the display is in HDR mode, but it could not be captured as HDR here …`；
      面板"丢帧"格下面是 `nv12 / SDR …`，磁贴下方有一句"这块屏幕开着 HDR，但……推的是 SDR"。观众画面正常（不是花屏）。
- [ ] **停止**：`pgrep -fl 'HdrCast|ffmpeg'` 只剩应用本身；临时目录里没有残留的 `hdrcast-*.fifo`。

## 15. 远程控制

- [ ] Mac 当**观众**：Windows 主播在"谁可以连接"里把这台 Mac 的控制设成"自动获得"（或"询问我"，Mac 上点 **请求控制**、主播允许），
      连上后在 mpv 窗口里移动鼠标、点击、打字、滚轮，主播那边跟着动。
      0.3.9 起（双方都要）：主播是 Windows 时 `Command` 当 `Ctrl`、`Control` 当 Win 键，详见第 18 步；0.3.8 的主播把 `Command` 当 Win 键。
- [ ] Mac 当**主播**：在"谁可以连接"里把观众的控制设成"询问我"，观众请求后点允许 → 第一次会要求在"系统设置 → 隐私与安全性 → 辅助功能"里勾选 HdrCast；
      勾选后再点一次允许。观众的指针被限制在正在分享的那块显示器上；双击、拖拽窗口、滚动、`Shift` 组合键都正常。
      - 验证：`crates/control/src/inject/mac.rs`（Quartz 事件、US 键位表、双击计数）。键位不对（例如 `;` 打出别的字符）说明主播用的不是 US 键盘布局：把现象发回来。
- [ ] 主播点 **收回** 或观众断线：按住的键 / 鼠标按钮 1.5 s 内被松开。

## 16. 菜单栏图标与登录时启动（`app/src/tray.rs`、`autostart.rs`、`instance.rs`）

先把 `HdrCast.app` 拖进"应用程序"再测：从"下载"直接打开的 app 跑在 App Translocation 的临时路径里，登录项会指向一个重启后就没了的地方
（那种情况下打开开关应该报"Move HdrCast to the Applications folder first"）。

- [ ] 菜单栏右上有 HdrCast 图标；单击出菜单 **打开 HdrCast** / **退出 HdrCast**。图标是彩色小方块（没有做成模板图），深浅菜单栏上都看得清就行。
- [ ] 开放连接打开时点红色关闭按钮：窗口消失，HdrCast 仍在运行，Windows 观众的列表里仍"在线"。点程序坞图标或菜单里的 **打开**：窗口回来。
- [ ] Windows 观众来连接、窗口关着时：窗口自己出来，程序坞图标跳动，弹出 **拒绝** / **允许**。
- [ ] 有人在看时菜单栏图标右下角有红点。
- [ ] 菜单里 **退出**、或 `Cmd + Q`（有人在看时）：观众几秒内看到主播离开；`pgrep -fl 'HdrCast|ffmpeg'` 没有残留。
- [ ] **设置 → 通用 → 登录时自动启动 HdrCast**：`plutil -p ~/Library/LaunchAgents/app.hdrcast.login.plist` 显示 `ProgramArguments` =
      `/Applications/HdrCast.app/Contents/MacOS/HdrCast`、`--background`，`RunAtLoad` = 1；系统可能弹"已添加后台项目"的通知。关掉后文件消失。
- [ ] 打开它后注销再登录：没有窗口弹出，菜单栏和程序坞里有 HdrCast，Windows 观众 10 s 内看到它在线。
      "系统设置 → 通用 → 登录项"里能看到它；在那里关掉后 HdrCast 的开关**仍显示开**（读不到这个状态，是已知局限）。
- [ ] HdrCast 运行时在访达里再打开 HdrCast.app：不会出现第二个进程（`pgrep -fl HdrCast` 只有一个应用进程），原来的窗口出来。
      登录项启动的那一份同样如此（它是 launchd 直接启动的，LaunchServices 是否把它认作同一个应用要实测；不认时 `instance.lock` 会让第二个进程把窗口叫出来后退出）。

## 17. 控制模式（0.3.7；Mac 当观众，主播是有 NVIDIA 显卡的 Windows）

Mac 多半是拿来操作别的电脑的那一台。控制模式里 Mac 自己的部分只做过类型检查：声音走 AudioQueue（`crates/capture/src/mac/playback.rs`），
mpv 用 `crates/media/src/receiver/mpv_command.rs` 里的 `low_latency_args`（`--untimed`、`--video-latency-hacks=yes`、`--hwdec-threads=1`，从标准输入读裸 HEVC）。

- [ ] 主播 **开播 → 控制模式 → 提供控制模式** 开着（默认开）。Mac 上 **观看 → 连接 → 模式 → 控制**，主播那一行带"控制模式"标记 → 点 **控制**。
      日志依次有 `[app] connecting to … in control mode …`、`[app] control mode: HdrCast hands mpv each picture …`；mpv 窗口出画面，
      面板没有延迟预设，"延迟"格写着 `主播 … ms · ping … ms`，还有帧率、码率和"丢失的画面"。
- [ ] 声音：有、不断续，面板里声音的欠载数不一直涨。没有声音时日志有 `[ll] no sound: …`（AudioQueue 打不开）：把那一行发回来。
- [ ] 延迟：主播屏幕上开一个毫秒秒表，用手机同时拍主播的屏幕和 Mac 上的 mpv 窗口，读数之差就是从屏幕到屏幕的延迟。
      Windows 观众在同一台电脑上是 14.5 ms（1080p 120 fps）；Mac 多一两个刷新周期正常。多出 50 ms 以上 → mpv 在 VideoToolbox 上还在攒帧：
      把 **设置 → 硬件解码** 改成 `videotoolbox-copy` 再拍一次，两次的照片和 mpv 日志（**日志** 页筛"播放器"）都发回来。
- [ ] 移动鼠标、打字：和第 15 步一样能控制，指针跟手。指针落点：Mac 上指到画面的四个角和正中，主播的指针也在那里（0.3.7 的主播只走到一半，0.3.8 修好，
      见 design §9"指针在哪块显示器上"）；主播日志有 `[control] a viewer's pointer moves on \\.\DISPLAYn: 宽x高 at (x, y)`。
- [ ] 主播端拔一下网线（或关掉 Wi-Fi）两三秒再接上：画面停住，面板写着"传输中丢了一帧：画面停在上一帧，等关键帧"；接上后一两秒内画面接着动，
      不用重新连接（连接 15 s 收不到东西才算断）。

## 18. 悬浮窗只在播放窗口上、Mac 的按键、断开连接、切换显示器（0.3.9；Mac 当观众，主播是 Windows，两边都要 0.3.9）

Mac 这边的做法见 design §3.3 的 macOS 一条和 §9、§9.2；全部只做过类型检查，下面每一项都是第一次在真机上跑。

- [ ] **只在播放窗口上**：小圆按钮出现在 mpv 画面的右上角（离边 12 点），不再在 HdrCast 窗口所在屏幕的角上。拖动、缩放 mpv 窗口：按钮跟着走。
      切到别的程序、把它的窗口（HdrCast 自己的主窗口也一样）拖到按钮上面：按钮藏起来；挪开又出来。别的程序在前台、但窗口没压到按钮时，
      按钮留在画面上（这是有意的：它只是画面的一部分）。
      最小化 mpv、切到别的桌面（Space）：按钮不见；回来又在。按钮不对（位置偏、跟不上、一直不出来）→ 截图 + 日志发回来。
- [ ] **不抢键盘**：按钮出现时，mpv 仍是前台程序（菜单栏左上角还是 mpv），在 mpv 里打字照常到主播。
- [ ] **点击和拖动**：mpv 在前台时单击按钮，一下就展开（不需要点两下）；"—" 或 `Esc` 收起后前台回到 mpv（菜单栏是 mpv），直接打字就到主播。
      拖按钮到画面左下角松手，它留在那里；退出 HdrCast 再观看还在左下。拖的时候按钮不往回跳。
- [ ] **全屏**：`Control+Option+Enter` 或面板里的 **全屏**：mpv 铺满屏幕，没有切到新桌面的动画（`--native-fs=no`），按钮还在画面上。退出全屏同样。
- [ ] **按键**（主播 Windows）：在主播上开记事本，Mac 上 `Command+A` 全选、`Command+C` / `Command+V` 复制粘贴、`Command+Z` 撤销、`Command+S` 保存都照
      Windows 的 `Ctrl` 组合生效；`Control+D` 显示桌面、`Control+E` 打开资源管理器、`Control+R` 打开运行（Win 键组合）；`Option+Tab` 切回上一个窗口（`Alt+Tab`）。
      `Command+Q`、`Command+W`、`Command+H`、`Command+M` 都发给主播（mpv 不会退出、关闭、隐藏或最小化，`--macos-menu-shortcuts=no`）。
      不这样 → 主播日志里看这位观众的输入到了没有（`[control] … input is arriving`），把现象发回来。已知到不了的：`Command+Tab`、`Command+空格`、
      `Control+方向键`（macOS 自己用），单独按 `Control`（开始菜单）或 `Shift`（切中英文）。
- [ ] **断开连接**：面板最下面的 **断开连接**：mpv 和悬浮窗一起关掉，观看页回到"未在观看"，日志 `[app] stopped watching`。
- [ ] **切换显示器**（主播有两块以上的屏）：控制着主播时，面板里有 **显示器** 一栏，每块屏一个按钮（名字、分辨率），正在分享的那块高亮。点另一块：
      显示"正在切换…"，一两秒后画面换成那块屏，高亮跟过去；指针落点跟着换（指四个角对得上）。主播日志
      `[control] <Mac 的名字> switched the shared display to <名字> (宽x高)`，主播开播页的显示器选择也变了。点 **只看不控** 后这一栏消失。
      0.3.8 的主播：没有这一栏（正常）。
