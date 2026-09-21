# Windows 验证清单

Linux 这边能验证的（`dotnet build` / `dotnet test` / `relay/selftest.sh`）已经跑通；下面是**只能在 Windows 上做**的部分，按顺序执行，每一步都写了预期结果。全部通过后再叫朋友。

环境：Windows 11 + RTX 4090 + AW3225QF（4K HDR），`dist/HdrCast-<版本>/` 解压到任意目录（下文记为 `HdrCast\`）。

## 0. 准备

- [ ] Windows HDR 已打开（`Win + Alt + B`，或 设置 → 显示 → 使用 HDR）。
- [ ] **工具一键装**：双击 `scripts\get-tools.cmd`（两个都装）。它下载 BtbN 的 ffmpeg win64-gpl（含 nvenc/amf/qsv、libsrt、libplacebo、ddagrab）和 zhongfly 的 mpv（含 libsrt）到 `HdrCast\tools\`，装完自己验证 srt 支持并列出可用编码器。已经装过会跳过，`get-tools.cmd force` 强制重下。
- [ ] 手动装（脚本失败时）：ffmpeg 取 <https://www.gyan.dev/ffmpeg/builds/> 的 **release full**，mpv 取 <https://github.com/zhongfly/mpv-winbuild/releases> 的 `mpv-x86_64-*.7z`，把 `ffmpeg.exe` / `ffprobe.exe` / `mpv.exe` 放进 `HdrCast\tools\`。
- [ ] .NET Framework 4.8 是 Windows 10 1903+/11 自带的，不用装。
- [ ] 点对点：信令服务器已部署（`signal/deploy.sh`，腾讯云安全组放行 TCP 18650 / UDP 18651），`curl http://101.43.23.94:18650/v1/health` 返回 ok。
- [ ] 中继模式（可选）：LXC 上已运行 `relay/setup.sh` 与 `relay/up.sh`（见 §6）。

## 1. `scripts\check-tools.cmd`

双击运行。预期：

```
ffmpeg    path/version 显示 7.x 或 8.x
srt       : yes (libsrt)
hevc_nvenc: listed, test encode 8-bit=ok 10-bit=ok
av1_nvenc : listed, test encode 8-bit=ok 10-bit=ok        ← 4090 才有
hevc_amf  : not in this ffmpeg build / listed but FAIL      ← 没有 AMD 卡，FAIL 正常
hevc_qsv  : listed, FAIL                                   ← 没有 Intel 核显，FAIL 正常
mpv       version 0.40+ 或 git 日期，srt : yes
displays  list-outputs 的输出（见下一步）
```

`srt : NO` 说明下错了构建；`hevc_nvenc ... 10-bit=FAIL` 先更新 NVIDIA 驱动。

## 2. `HdrCastSender.exe list-outputs`

在 `HdrCast\` 打开终端运行。预期每个显示器一行，例如：

```
idx  device         name                      size        pos          hz     hdr  colorspace  bits  maxnit  maxffl  minnit  adapter
0    \\.\DISPLAY1   AW3225QF                  3840x2160   0,0          239.76 on   12          10    1000    400     0.0001  NVIDIA GeForce RTX 4090
1    \\.\DISPLAY2   AW2523HF                  1920x1080   3840,0       360    off  8           8     0       0       0       NVIDIA GeForce RTX 4090
```

- [ ] HDR 显示器那一行 `hdr = on`（`colorspace = 12` 即 DXGI `RGB_FULL_G2084_NONE_P2020`）。关掉 Windows HDR 再跑一次应变成 `off`。
- [ ] `--json` 也能输出同样内容（GUI 用的就是它）。
- [ ] 索引是跨显卡的全局编号，记住要分享的那个索引（下文用 0）。

## 3. `HdrCastSender.exe capture --selftest`

不需要显示器，只跑 compute shader，把 GPU 的结果和 C# 参考实现逐个比对，末尾打印 PASS / FAIL。
预期退出码 0、每行 `ok`。参考值（sdr-white 80，limited range 10 bit，程序内允许 ±2）：

| scRGB 输入 | 亮度 | 期望 Y10 |
|---|---|---|
| 0.0 | 0 nit | 64 |
| 0.5 | 40 nit | 431 |
| 1.0 | 80 nit | 490 |
| 2.5 | 200 nit | 571 |
| 12.5 | 1000 nit | 723 |
| 125 | 10000 nit | 940（限幅） |

> 这些是 ST.2084 的教科书值。本机 ffmpeg 的 zscale（npl=80）给的是 491 / 573 / 943，差 ≤3 码值
> —— zimg 的取整方式与限幅上限略有不同，不影响观感；selftest 比的是 GPU 与 C# 是否一致。

- [ ] 中性灰的 Cb/Cr ≈ 512。
- [ ] 报错 `d3dcompiler_47` 找不到 → 这台 Windows 缺 DirectX 运行时（Win10 1903+ 自带，理论上不会发生）。

## 4. `scripts\dump-check.cmd`

先用记事本把脚本开头的 `OUTPUT` 改成第 2 步的索引，`SIZE` 改成该显示器的原生分辨率（如 `3840x2160`）。运行后在 `scripts\dump\` 生成：

- [ ] `frames.p010` 大小 = W×H×3 字节 × 帧数（4K：24 883 200 B/帧 × 30 fps × 3 s ≈ 2.2 GB；嫌大就先用 `SIZE=1920x1080` 测）。
- [ ] 终端打印 `YMIN/YAVG/YMAX`：普通桌面（白色窗口）`YMAX ≈ 490`，屏幕上有 HDR 内容（HDR 视频/游戏）时才会 > 500；`YMIN` 接近 64。若 `YMAX` 总是 940 或全部相同 → 转换错了。
- [ ] `frame1-pq-raw.png`：整体发灰、发平——**正常**，这是 PQ 编码直接当 sRGB 看的效果，证明数据不是黑屏或乱码。
- [ ] `frame1-sdr-tonemapped.png`：看起来就是你的桌面，颜色不偏（红绿蓝方向正确，肤色正常），鼠标指针在正确位置。若色相错乱 → BT.709→BT.2020 矩阵或 UV 顺序有误；若整体过暗 → `sdr-white` 与实际不符（Windows 的 SDR 亮度滑块不影响这里，规范值就是 80）。
- [ ] 把 `FORMAT` 改成 `nv12` 再跑一次（可先关闭 Windows HDR），`frame1-sdr.png` 应同样正确。
- [ ] 结束后删掉 `scripts\dump\frames.*`。

## 5. 点对点（Group 模式，默认）

先在两台自己的电脑上测（同一局域网即可），再测外网。

- [ ] 发送端 Mode 选 **Group: friends watch straight from this PC (P2P)** → **Create group…** → 起名 → 弹框说邀请已复制；
      粘贴出来是一行说明加一行 `hdrcast:…`（约 200 字符）。`%APPDATA%\HdrCast\group.json` 已生成。
- [ ] 另一台电脑复制这条消息后打开 `HdrCastReceiver.exe`：弹"要加入吗" → 是 → 填名字 → Mode 自动变成 **Group**。
      点"否"的话，同一个邀请不应再问第二次。
- [ ] 发送端 **Start**：日志有 `[p2p] candidates: [2400:…]:NNNNN, 192.168.x.x:NNNNN`。IPv6 必须是**稳定地址**：
      PowerShell 里 `Get-NetIPAddress -AddressFamily IPv6 | ft IPAddress,PrefixOrigin,SuffixOrigin,AddressState`，
      日志里的地址那一行 SuffixOrigin 应为 **Link**（不是 Random = 临时地址）。
- [ ] 信令服务器的 UDP 18651 放行后，日志还有 `[p2p] public IPv4 mapping: …`（没有也不影响 IPv6）。
- [ ] 状态栏 **Viewers**：`Live in "…" - 0 viewers | nobody yet`。
- [ ] 第一次 Start 可能弹 Windows 防火墙"允许访问"：**先点取消**，看观众还能不能连上（记录结果：打洞理论上不需要放行），再点允许。
- [ ] 接收端 **Watch** 列表 10 s 内出现主播 → **Play** → 状态 `Connected to …（IPv6 [2400:…]:NNNNN, N ms）` → mpv 出画面；
      发送端 **Viewers** 显示这位观众（IPv6 / RTT）。
- [ ] **外网**：笔记本连手机热点（确认 test-ipv6.com 有 IPv6），不改任何路由器设置，Play 能连上。这是打洞穿过家里路由器
      （如 光BBユニット）的关键一步。
- [ ] 关掉 mpv 窗口：约 10 s 内自动重连出画面（主播端日志会看到那位观众的 remux 子进程重启）。
- [ ] 发送端 **Stop**：观众显示 `The stream ended - waiting …`；再 **Start**，观众自动重新接上。
- [ ] 任务管理器里结束发送端的 ffmpeg.exe：自动重启后观众几秒内恢复，不需要重新 Play。
- [ ] **Max viewers** 设 1：第二个观众显示 `… is full`。
- [ ] 上行估算：码率 40、Max viewers 3、my upload 100 → 提示标红。
- [ ] 朋友（中国 / 澳洲）按 `README.en.md` 操作；连不上时把状态栏原话和两边 test-ipv6.com 的结果记下来。

## 6. 中继（在 LXC 上，可选）

```
cd /projects/hdrcast
relay/setup.sh            # 第一次：生成密码；再次运行会问是否覆盖
relay/selftest.sh         # 可选：本机合成 HDR10 → MediaMTX → ffprobe/mpv 全通过（结束后中继是停止状态）
relay/up.sh               # 正式启动
relay/logs.sh             # 看日志，Ctrl+C 退出
```

- [ ] `setup.sh` 打印的 sender/viewer 密码、两把 passphrase、URL 已保存（也在 `relay/.env`）。
- [ ] `sysctl net.core.rmem_max` ≥ 4000000（建议 16777216，写进 `/etc/sysctl.d/99-hdrcast.conf`）。
- [ ] 路由器：UDP 8890 → 192.168.1.201。
- [ ] `docker ps` 里 `mediamtx-relay` 是 `Up`；日志里有 `[SRT] started with listener on :8890 (UDP/SRT)` 且没有 `ERR`。

## 7. GUI 推流到中继（中继模式，可选）

运行 `HdrCastSender.exe`：

1. 显示器：选第 2 步的那块，状态显示 HDR；模式"自动"。
2. 画质：编码器 `hevc_nvenc`，码率 40 Mbps，60 fps，输出分辨率先选 2560×1440，质量档"均衡"，GOP 2 s。
3. 音频：开，默认设备，AAC 192k。
4. 目标：模式"中继"，主机 `192.168.1.201`，端口 8890，用户 `sender` + 密码，passphrase = **publish** passphrase，延迟 800 ms。
5. 点开始。

预期：

- [ ] 两个子进程启动（任务管理器里 `HdrCastSender.exe` ×2 + `ffmpeg.exe`），5 s 内状态栏开始刷新。
- [ ] 采集 fps ≈ 60；桌面静止时"重复帧"持续增长是**正常的**（DXGI 没有新帧就重发上一帧，保证恒定帧率）；拖动窗口时 `captured` 增长、`duplicated` 停止。
- [ ] 编码 fps ≈ 60，码率 ≈ 40 Mbps，speed ≈ 1.0x，drop 为 0；`gpu_ms` 在 4K 下应 < 5 ms。
- [ ] LXC：`relay/logs.sh` 出现 `[SRT] [conn 192.168.1.x:port] is publishing to path 'screen'`，`curl -s http://127.0.0.1:9997/v3/paths/list` 里 `ready: true`、`tracks: ["H265","MPEG-4 Audio"]`。
- [ ] `curl -s http://127.0.0.1:9997/v3/srtconns/list` 里发布连接的 `msReceiveTsbPdDelay` = 800，`packetsReceivedLoss` = 0。
- [ ] 点停止：两个子进程都消失；直接关闭 GUI 窗口也一样（进程树清理）。
- [ ] 把 ffmpeg 进程在任务管理器里杀掉：GUI 应提示异常退出并停止采集进程；勾选"自动重连"后 3 s 内重新开始。
- [ ] "复制 ffmpeg 命令"得到的命令与 `scripts\send-example.cmd` 结构一致，可粘贴到终端单独跑。

## 8. 观众端（先自己测，中继模式）

同一台 PC（第二块显示器）或局域网里另一台 PC，运行 `HdrCastReceiver.exe`：

- 模式"中继"，主机 `192.168.1.201`（外网朋友填公网 IP/DDNS），端口 8890，用户 `viewer` + 密码，passphrase = **read** passphrase，地区"日本 1500 ms"，码率上限 100 Mbps，显示"自动"。

预期：

- [ ] 推流未开始时显示"等待推流…"并每 3 s 重试；推流开始后 ≤ 5 s 出画面。
- [ ] 状态栏：`hwdec-current = d3d11va`，`video-params` 为 `pq / bt.2020`，HDR 屏且 Windows HDR 已开时 `video-target-params` 也是 `pq`（直通）；缓存时长在 1.5 s 附近波动；`frame-drop-count` 不持续增长。
- [ ] 画面亮度与发送端一致：白色窗口不刺眼（80 nit 规范值），HDR 视频高光正常。发灰 → 观众端 Windows HDR 没开或选了"SDR 色调映射"。
- [ ] 全屏按钮可用，全屏时显示器 OSD 应显示 HDR 模式。
- [ ] SDR 屏（或把 Windows HDR 关掉）：选"SDR 色调映射"，画面正常但高光被压缩，不应该发灰或过曝。
- [ ] 声音同步：发送端播放一段有节奏的视频，观众端口型/节拍对得上（延迟约 4 s 是设计值）。
- [ ] 关闭 mpv 窗口 / 点断开：mpv 进程消失。
- [ ] 「用 ffmpeg 拉流」兜底模式也能出画面（把 mpv 的 srt 支持想成坏掉时的备胎）。

## 9. 外网观众（中继模式）

- [ ] 用手机热点连接一台笔记本，按 §8 填公网 IP，能出画面 → 端口转发正确。
- [ ] 朋友按 `README.en.md` 操作；他那边 `docs/network.md` §3 的地区预设。跨境（中国）若卡顿，先把 latency 提到 4000~5000 ms 再降码率。
- [ ] LXC `srtconns/list` 里每位观众的 `msSendTsbPdDelay` 等于他填的 latency，`packetsRetrans` 有少量是正常的，`packetsReceivedLoss` 应为 0（那是发送端那条腿）。

## 10. 4K60 压力

- [ ] 输出分辨率改"原生"（3840×2160）、80 Mbps、60 fps 推 10 分钟：采集 fps 稳定 60，`gpu_ms` < 8，编码 speed ≥ 1.0x，GPU 占用（任务管理器 Video Encode + 3D）不影响正在玩的游戏帧率太多。
- [ ] 120 fps 档位：采集 fps 能到 120 说明 DXGI 帧率跟得上；否则退回 60。
- [ ] 观众端 4K60 HEVC 10-bit 的 d3d11va 解码在 RTX 30/40、RX 6000+、Intel Arc 上没问题；老机器降到 1440p。

## 11. 只有在分享「旋转过的显示器」时才需要看

shader 里 90° 与 270° 的映射（`desk_to_src`）互为逆运算，DXGI 文档没写清 `DXGI_MODE_ROTATION_ROTATE90` 到底对应哪一个，只能实测：

- [ ] 把某块屏在 Windows 显示设置里转成「横向（翻转）」以外的 90°/270°，用 `scripts\dump-check.cmd` 抓一帧看 `frame1-sdr-tonemapped.png`。
- [ ] 画面方向正确即可；若上下颠倒 90°（即转了 180° 的样子），把 `shared/Capture/ConvertShaderSource.cs` 里 `desk_to_src` 的 `rotation == 1` 与 `rotation == 3` 两行对调。
- 180° 那条已确认正确；不旋转的显示器走 `FlagExact` 路径，与此无关。

## 12. 出问题时看哪里

- 日志：`%APPDATA%\HdrCast\logs\`（sender / receiver / capture / ffmpeg / mpv 分文件，按天）。
- 配置：`%APPDATA%\HdrCast\sender.json`、`receiver.json`（删掉即恢复默认）。
- 中继：`relay/logs.sh`，API `http://127.0.0.1:9997/v3/paths/list`、`/v3/srtconns/list`。
- 排查顺序见 `docs/faq.md`「画面卡顿 / 断线」。
