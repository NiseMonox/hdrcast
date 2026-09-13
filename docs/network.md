# HdrCast 网络说明

本文说明三种连接拓扑、各地区的 SRT 延迟预设及其理由、接收缓冲的计算公式、Windows 防火墙需要放行什么，以及为什么不用上海的腾讯云服务器。所有数值来源见 `docs/research/srt-mediamtx.research.json`（已在本机实测）。

## 1. 三种拓扑

### 1.1 中继模式（默认）：MediaMTX 跑在家里的 LXC 上

```
发送端 Windows PC ──SRT (LAN, 800 ms)──▶ LXC 192.168.1.201:8890/udp (MediaMTX) ──SRT (1.5~3.5 s)──▶ 朋友 A
                                                                                 ├──SRT──▶ 朋友 B
                                                                                 └──HLS 8888/tcp（浏览器兜底）
```

- 路由器把 **UDP 8890** 转发到 `192.168.1.201`（HLS 兜底再加 TCP 8888，可选）。API 端口 9997 只绑定在 LXC 的 127.0.0.1，不要转发。
- 发送端在同一局域网内，用 LAN IP `192.168.1.201` 推流；朋友用你的公网 IP / DDNS 域名拉流。
- 推流与拉流分别鉴权：`sender` 用户只能 publish，`viewer` 用户只能 read；两条腿各自有独立的 SRT passphrase（AES-256）。凭据由 `relay/setup.sh` 生成。
- **每个观众是一条独立的 SRT 连接**，MediaMTX 逐个复制，所以家里的上行带宽 = 码率 × 观众数 × 1.25（见 §6）。
- 观众在推流开始前连接会被 MediaMTX 直接拒绝（"no stream is available on path"），接收端 GUI 每 3 s 自动重试。
- 相同网段的第二台 PC 也可以用 LAN IP 直接拉流做本地测试。

### 1.2 直连模式·本机监听（发送端 `mode=listener`）

```
发送端 PC (ffmpeg srt://0.0.0.0:9000?mode=listener)  ◀──UDP 9000 端口转发──  朋友 (mpv mode=caller)
```

- 路由器把选定的 UDP 端口转发到**发送 PC**，Windows 防火墙放行该端口入站；朋友那边什么都不用开。
- 只有 passphrase 做访问控制（ffmpeg 的 listener 不检查 streamid），所以必须设 passphrase。
- ffmpeg 的 listener 只接受一个 caller，且对方断开后 ffmpeg 会退出（"Error writing trailer"）—— 适合一对一，GUI 会自动重启。
- 这是唯一能用 **AV1** 的模式（封装换成 `-f matroska -live 1`，MediaMTX 和 mpegts 都不支持 AV1）。

### 1.3 直连模式·拨号到朋友（朋友 `mode=listener`）

角色对调：朋友的路由器转发一个 UDP 端口到他的 PC，接收端 mpv 用 `mode=listener`，发送端用 `mode=caller` 拨过去。谁转发端口谁当 listener，另一方不需要任何设置。

**不要用 `mode=rendezvous`**：ffmpeg 7.1 会把本地 socket 绑到远端地址（"Cannot assign requested address"），两边都在 NAT 后时只能走中继或 VPN。

### 1.4 Tailscale（无需端口转发，未在真实跨境线路上实测）

LXC 已经在 tailnet 里（`tailscale0` = 100.82.8.39）。朋友装 Tailscale 加入同一个 tailnet 后：

- 中继模式：朋友直接连 `100.82.8.39:8890`，不用路由器转发，也不暴露公网端口。
- 直连模式：两台 PC 都在 tailnet 里时，caller / listener 任意一方都不需要端口转发。

注意 Tailscale 的 MTU 是 1280 字节，而默认 SRT 包是 1316 字节载荷 + 16 SRT + 8 UDP + 20 IP = 1360 字节，会被 IP 分片，一旦丢一片整包作废。建议改成 6 个 TS 包一组：发送端 / 接收端 URL 用 `pkt_size=1128`，中继的 `mediamtx.yml` 把 `udpMaxPayloadSize` 改成 `1200`（MediaMTX 会算出 (1200−16)/188×188 = 1128）。这一条没有实测。

## 2. 各地区延迟预设与理由

| 观众 | RTT 估计 | 丢包 | SRT latency | oheadbw |
|---|---|---|---|---|
| 日本国内 | 10~30 ms | ≈0 | **1500 ms** | 25 % |
| 澳大利亚 | 120~160 ms | 少量 | **2000 ms** | 25 % |
| 中国大陆（跨境） | 60~120 ms，但抖动和突发丢包多 | 1~5 % 常见 | **3500 ms** | 50 % |
| 发送端 → 中继（LAN） | < 1 ms | 0 | **800 ms** | 25 % |

理由：

1. **Haivision 的 4×RTT 规则**：SRT latency = RTT 乘数 × RTT；在 0.1~0.2 % 丢包、无突发的"还不错"的网络上乘数约为 4。乘数低于 3 基本没意义，高于 20 说明线路已严重拥塞。
2. **官方保守表**（丢包率 → 乘数 / 带宽开销）：≤1 % → 3 / 1 %；≤3 % → 4 / 4 %；≤7 % → 6 / 9 %；≤10 % → 8 / 15 %；≤12 % → 8 / 20 %；≤21 % → 10 / 38 %；≤25 % → 13 / 46 %；≤30 % → 14 / 61 %；≤40 % → 30 / 97 %。
3. 按 4×RTT 算，澳洲只要 ~640 ms，中国只要 ~480 ms；但我们的目标是"推流式"而非远程桌面：多留几倍余量能扛住跨境线路的突发丢包（重传要在 latency 内完成），代价只是延迟大一点。中国线路按丢包 3~7 % 取乘数 6~8 并留余量，得到 3500 ms；`oheadbw` 也提高到 50 %，给重传留带宽。
4. latency 由两端协商取**最大值**（发送端的 `peerlatency` 与接收端的 `rcvlatency`）。MediaMTX 自己不设 latency（gosrt 默认 120 ms），所以客户端 URL 里的值就是生效值（实测：API 里 `msReceiveTsbPdDelay` / `msSendTsbPdDelay` 正好等于 URL 里的值）。
5. 端到端总延迟 ≈ 发送 SRT (0.8 s) + 接收 SRT (1.5~3.5 s) + mpv `cache-pause-wait` (1.5 s) + 编码 lookahead (~0.1 s)，即日本约 4 s、中国约 6 s。

> 观众可以自己调：卡顿（mpv 反复缓冲、或 relay API 里 `packetsReceivedLoss` 持续增长）就把 latency 加 500 ms；线路好就往下减。

## 3. 缓冲大小公式（必须做，否则高码率会断线）

从 MediaMTX 拉流时 gosrt **不理会接收端的空闲窗口**，libsrt 默认接收缓冲只有 8192 个包；当 latency × 码率 超过它，libsrt 会刷屏 "No room to store incoming packet"，随后 "SEQUENCE DISCREPANCY. BREAKING CONNECTION" 断线（本机 48 Mbps × 2 s 即复现）。所以两个 GUI 都按下面的公式把参数写进 URL（实现在 `shared/Common/SrtBufferSizing.cs`）：

```
packets = ceil( latency_s × bitrate_bps / (1316 × 8) )      # 每个 SRT 包 1316 字节载荷
ffs     = max( 25600, 大于等于 packets × 1.2 的最小 2 的幂 )   # SRTO_FC，流控窗口（单位：包）
rcvbuf  = ffs × 1472                                          # SRTO_RCVBUF 必须 ≤ ffs × (MSS − 28)
sndbuf  = 同 rcvbuf（发送端）
recv_buffer_size = 8000000                                    # UDP socket 缓冲（Linux 上受 net.core.rmem_max 限制，会被静默截断）
```

例子：

| 码率 × latency | packets | ffs | rcvbuf |
|---|---|---|---|
| 40 Mbps × 1.5 s | 5 700 | 25 600（默认） | 37 683 200 |
| 50 Mbps × 2 s | 9 500 | 25 600 | 37 683 200 |
| 100 Mbps × 2 s | 19 000 | 32 768 | 48 234 496 |
| 100 Mbps × 3.5 s | 33 244 | 65 536 | 96 468 992 |

接收端不知道发送端会用多大码率时，按 100 Mbps 算（宁大勿小，只是内存）。`relay/setup.sh` 打印的示例 URL 用的是 `rcvbuf=64000000&ffs=64000`，覆盖到 100 Mbps × 6 s。

中继本身：`udpReadBufferSize: 4000000` 要求宿主 `net.core.rmem_max ≥ 4000000`，否则 MediaMTX 起不来 SRT 监听（"unable to set UDP read buffer size"）。生产环境建议：

```
sudo sysctl -w net.core.rmem_max=16777216
echo 'net.core.rmem_max=16777216' | sudo tee /etc/sysctl.d/99-hdrcast.conf
```

（容器里的 `sysctls:` 改不了它，这个内核参数不是命名空间化的。）

## 4. Windows 防火墙要开什么

| 角色 | 模式 | 需要的规则 |
|---|---|---|
| 发送端 | 中继 / 直连拨号 | **无**（只有出站 UDP，Windows 默认允许） |
| 发送端 | 直连·本机监听 | 入站 UDP 该端口 → `ffmpeg.exe`；路由器再转发同一端口 |
| 接收端 | 中继 / 直连拨号 | **无** |
| 接收端 | 直连·监听 | 入站 UDP 该端口 → `mpv.exe`；路由器转发 |

命令行加规则（管理员 PowerShell，把 9000 换成你的端口）：

```
netsh advfirewall firewall add rule name="HdrCast SRT in" dir=in action=allow protocol=UDP localport=9000
```

第一次运行 ffmpeg / mpv 监听时 Windows 也会弹"允许访问网络"对话框，勾"专用网络"+"公用网络"即可。

LXC 侧：Debian 默认没有防火墙；若装了 ufw，`ufw allow 8890/udp`（可选 `8888/tcp`）。Docker 用 bridge 网络加 `-p 8890:8890/udp` 就够了，不需要 `network_mode: host`。

## 5. 为什么不用上海的腾讯云服务器

1. **带宽与费用**：中继要把每路观众的码率各复制一份。40 Mbps × 3 人 = 120 Mbps 持续上行，远超普通云主机的公网带宽（按带宽计费的 5~10 Mbps，或按流量计费时每小时几十 GB）。家里的光纤上行是免费的。
2. **绕路**：东京 → 上海 → 悉尼 / 东京 → 上海 → 东京，跨境出入两次，RTT 与丢包都比直接从东京出去更差；只有中国观众受益，而中国观众走 SRT + 3.5 s latency 已经能扛丢包。
3. **备案 / 合规**：在境内服务器上跑对公网开放的视频中继端口，风险不值得。
4. 需求里也明确了：中继只放在家里的 LXC（`docs/design.md` §0）。腾讯云上跑的是其他生产服务，不要混用。

如果哪天日本这边的上行不够（见 §6），优先考虑降码率或减少同时观看人数，而不是上云。

## 6. 上传带宽估算

```
需要的上行 ≈ 码率 × 观众数 × (1 + oheadbw)      # oheadbw 25 % → ×1.25，中国观众 50 % → ×1.5
```

例：40 Mbps，日本 1 人 + 澳洲 1 人 + 中国 1 人 ≈ 40 × (1.25 + 1.25 + 1.5) = 160 Mbps 上行。发送端 → 中继那一跳只走局域网，不占公网上行。先用 `fast.com` 或 speedtest 看自己的上行，留 30 % 余量再定码率。
