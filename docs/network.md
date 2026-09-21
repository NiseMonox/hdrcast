# HdrCast 网络说明

默认是**点对点（Group / P2P）模式**：主播的电脑直接给每位观众发流，腾讯云上的信令服务器只负责牵线。
本文说明它怎么穿过路由器和防火墙、谁需要什么、IPv6 怎么自查、各地区的 SRT 延迟预设、缓冲公式、上行带宽，
以及旧的中继 / 直连模式（§8，可选）。协议细节见 `docs/design.md` §8。

## 1. 点对点模式怎么连上

```
            ① 我在播，候选地址是 [2400:…]:51234            ③ 我想看 小明，我的候选是 [240e:…]:40000
主播 ───────────────────────────────▶ 腾讯云 hdrcast-signal ◀─────────────────────────────── 观众
     ◀──────────── ④ 有人要看（long-poll 立刻返回）          ⑤ 应答：好，我的候选是…  ─────▶
                        （只存几百字节的签名元数据，从不转视频）

主播 [2400:…]:51234  ⇄  ⑥ 双方同时每 100 ms 互发 PUNCH，收到就回 ACK  ⇄  观众 [240e:…]:40000
                         两边的有状态防火墙都看到"自己先发出去了"，放行对方的回包
主播 remux ffmpeg (127.0.0.1) ⇄ HostLink ⇄ ⑦ SRT（AES-256）原样转发 ⇄ ViewerLink ⇄ mpv (127.0.0.1)
```

- **为什么 IPv6 能打洞**：IPv6 没有 NAT，每台电脑有全球地址和固定端口；家用路由器、光猫和 Windows 防火墙对 IPv6 都是
  "有状态"的：挡住没人请求过的入站，放行"自己先发出去"那条流的回包。双方同时向对方发包，两边都先有了出站，
  于是都放行。**谁都不用做端口转发，也不需要公网 IPv4。**
- **候选地址**：IPv6 用出口网卡上的**稳定**地址（Windows 默认用临时地址发包，但临时地址每天变，而且"从哪个地址发"
  必须和"公布哪个地址"一致，所以 socket 直接绑在稳定地址上）；IPv4 候选是局域网地址（同一局域网的人直连用）和经
  信令服务器 UDP 回显得到的公网映射（IPv4 打洞的额外尝试，碰上运营商级 NAT 基本不成）。
- **选路**：先收到 ACK 的路径胜出；IPv4 先通的话再等 200 ms，IPv6 也通就用 IPv6。主播跟着观众的选择走。
- **每位观众一条独立的 SRT 连接**：主播端每个观众一个 remux ffmpeg（`-c copy`，不重编码），按**该观众**选的地区延迟和
  oheadbw 重传；谁的网络差只影响谁。
- **断线恢复**：路径 15 s 没有任何包即判定断开，观众端自动重新加入（同一个本地端口，mpv 不用重启就能接上）；
  主播下播会发 BYE，观众端显示"直播结束，等待重新开播"。

### 1.1 谁需要什么

| | 主播 | 观众 |
|---|---|---|
| 公网 IPv4 | 不需要 | 不需要 |
| IPv6 | 需要（或者观众全在同一局域网） | 需要 |
| 路由器端口转发 / 防火墙规则 | **不需要** | **不需要** |
| Windows 防火墙 | 第一次开播可能弹"允许访问网络"，点允许（点取消理论上也能打洞，未实测） | 同左 |
| 上行带宽 | 码率 × 观众数 × 1.25~1.5（§7） | 很少（只有 ACK） |
| 下行带宽 | 很少 | 码率 × 1.3 |

### 1.2 连不上时状态栏说的是什么

| 提示 | 含义 / 怎么办 |
|---|---|
| This PC has no IPv6 internet connection… | 本机没有 IPv6 出口，而主播只有 IPv6：按 §2 打开 IPv6 |
| The host's PC has no IPv6 internet connection… | 主播那边没有 IPv6：让主播按 §2 打开 |
| Neither side has IPv6… | 双方都没有 IPv6，IPv4 打洞也没成：至少一方要有 IPv6 |
| Both sides have IPv6 but the path is blocked… | 都有 IPv6 但包过不去：多半是 VPN（关掉）或路由器"IPv6 防火墙：阻止所有入站"之类的严格设置（改成默认 / 标准） |
| Not live right now / Nobody in the group is live | 主播还没点 Start；列表每 10 s 自动刷新 |
| … is full | 达到主播设的 Max viewers |
| The signaling server cannot be reached | 本机断网，或信令服务器挂了；已经在看的人不受影响 |

## 2. IPv6 自查与开启

1. 打开 <https://test-ipv6.com>：看到 "IPv6 address detected"（最好 10/10）就行。
2. 没有的话一般是路由器 / 光猫没开：
   - **日本**：NTT フレッツ系 / SoftBank 光 / au ひかり 基本都默认给 IPv6（IPoE）。SoftBank 光BBユニット 默认开着 IPv6。
   - **中国**：三大运营商家宽基本都已下发 IPv6，但不少光猫 / 路由器默认只开 IPv4。在路由器里找"IPv6 设置 / IPv4/IPv6 双栈 /
     IPv6 模式"打开（桥接光猫的话在你自己的路由器上开）；手机热点通常自带 IPv6，可以先用热点试。
   - **澳洲**：看 ISP，一部分 NBN ISP 默认给 IPv6，另一部分不提供；不提供的话只能换 ISP 或让同组有 IPv6 的人开播。
3. Windows 里 IPv6 默认是开的；装了 VPN / 加速器的话先关掉再试。

## 3. 各地区延迟预设与理由

观众在接收端选地区预设；点对点模式下这个值随加入请求发给主播，主播为这位观众起的 remux 子进程就用它
（中国预设同时把 oheadbw 提到 50 %）。所以同一场直播里，日本观众 1.5 s、中国观众 3.5 s 可以同时成立。

| 观众 | RTT 估计 | 丢包 | SRT latency | oheadbw |
|---|---|---|---|---|
| 日本国内 | 10~30 ms | ≈0 | **1500 ms** | 25 % |
| 澳大利亚 | 120~160 ms | 少量 | **2000 ms** | 25 % |
| 中国大陆（跨境） | 60~120 ms，但抖动和突发丢包多 | 1~5 % 常见 | **3500 ms** | 50 % |
| 发送端 → 中继（LAN，仅中继模式） | < 1 ms | 0 | **800 ms** | 25 % |

理由：

1. **Haivision 的 4×RTT 规则**：SRT latency = RTT 乘数 × RTT；在 0.1~0.2 % 丢包、无突发的"还不错"的网络上乘数约为 4。乘数低于 3 基本没意义，高于 20 说明线路已严重拥塞。
2. **官方保守表**（丢包率 → 乘数 / 带宽开销）：≤1 % → 3 / 1 %；≤3 % → 4 / 4 %；≤7 % → 6 / 9 %；≤10 % → 8 / 15 %；≤12 % → 8 / 20 %；≤21 % → 10 / 38 %；≤25 % → 13 / 46 %；≤30 % → 14 / 61 %；≤40 % → 30 / 97 %。
3. 按 4×RTT 算，澳洲只要 ~640 ms，中国只要 ~480 ms；但我们的目标是"推流式"而非远程桌面：多留几倍余量能扛住跨境线路的突发丢包（重传要在 latency 内完成），代价只是延迟大一点。中国线路按丢包 3~7 % 取乘数 6~8 并留余量，得到 3500 ms；`oheadbw` 也提高到 50 %，给重传留带宽。
4. latency 由两端协商取**最大值**（发送端的 `peerlatency` 与接收端的 `rcvlatency`）。点对点模式两端都用观众的值；中继模式下 MediaMTX 自己不设 latency（gosrt 默认 120 ms），所以客户端 URL 里的值就是生效值（实测：API 里 `msReceiveTsbPdDelay` / `msSendTsbPdDelay` 正好等于 URL 里的值）。
5. 端到端总延迟（点对点）≈ 接收 SRT (1.5~3.5 s) + mpv `cache-pause-wait` (1.5 s) + 编码 (~0.4 s)，即日本约 3.5 s、中国约 5.5 s；
   比中继模式少了"发送端 → 中继"那 0.8 s。中继模式（§8.1）再加 0.8 s。

> 观众可以自己调：卡顿（mpv 反复缓冲）就把 latency 加 500 ms 重新 Play；线路好就往下减。

## 4. 缓冲大小公式（必须做，否则高码率会断线）

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

## 5. 防火墙要开什么

**点对点模式：什么都不用开。** 打洞靠的就是防火墙对"自己先发出去的流"的放行。

旧模式（§8）才需要规则：

| 角色 | 模式 | 需要的规则 |
|---|---|---|
| 发送端 | 中继 / 直连拨号 | **无**（只有出站 UDP） |
| 发送端 | 直连·本机监听 | 入站 UDP 该端口 → `ffmpeg.exe`；路由器再转发同一端口 |
| 接收端 | 中继 / 直连拨号 | **无** |
| 接收端 | 直连·监听 | 入站 UDP 该端口 → `mpv.exe`；路由器转发 |

```
netsh advfirewall firewall add rule name="HdrCast SRT in" dir=in action=allow protocol=UDP localport=9000
```

## 6. 腾讯云上的信令服务器做什么

`signal/hdrcast_signal.py`（只用 Python 标准库，systemd 跑，见 `signal/README.md`）：

- **存什么**：每个组的"谁在播"（主播名字、候选地址、码率、观众数，30 s 不续期即过期）、加入请求和应答（60 s 过期），
  以及组 id → 鉴权 key 的注册表（唯一写盘的数据）。
- **看不到什么**：组密钥、SRT passphrase、视频。所有消息都用组密钥派生的 key 签名，服务器不知道这个 key，
  所以就算服务器被人控制，也只能让人连不上，没法把观众引到别处。
- **流量**：每个在播的主播每 10 s 一个几百字节的心跳，每个观众每 10 s 一次列表查询。和视频比可以忽略，不影响那台机器上的生产服务。
- **端口**：TCP 18650（HTTP API）、UDP 18651（IPv4 地址回显，可选）。需要在腾讯云安全组放行。
- **挂了会怎样**：已经连上的人照常看；新的人加入不了、列表刷不出来，恢复后自动继续。

## 7. 上传带宽估算

```
主播需要的上行 ≈ 码率 × 观众数 × (1 + oheadbw)      # oheadbw 25 % → ×1.25，中国观众 50 % → ×1.5
```

例：40 Mbps，日本 1 人 + 澳洲 1 人 + 中国 1 人 ≈ 40 × (1.25 + 1.25 + 1.5) = 160 Mbps 上行。
日本的光纤上行一般够；国内家宽上行常见 30~100 Mbps、澳洲 NBN 常见 20~50 Mbps，朋友开播时要降码率或限人数。
发送端界面会按 Max viewers 估算，填了 *my upload* 就会在超过 80 % 时标红提醒。

## 8. 旧模式：中继 / 直连 / Tailscale（可选）

下拉框里还保留着这几种模式，适合有公网 IPv4、或者愿意自己跑 MediaMTX 的人。点对点模式不需要它们。

### 8.1 中继模式：MediaMTX（需要一台有公网入口的机器）

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

### 8.2 直连模式·本机监听（发送端 `mode=listener`）

```
发送端 PC (ffmpeg srt://0.0.0.0:9000?mode=listener)  ◀──UDP 9000 端口转发──  朋友 (mpv mode=caller)
```

- 路由器把选定的 UDP 端口转发到**发送 PC**，Windows 防火墙放行该端口入站；朋友那边什么都不用开。
- 只有 passphrase 做访问控制（ffmpeg 的 listener 不检查 streamid），所以必须设 passphrase。
- ffmpeg 的 listener 只接受一个 caller，且对方断开后 ffmpeg 会退出（"Error writing trailer"）—— 适合一对一，GUI 会自动重启。
- 这是唯一能用 **AV1** 的模式（封装换成 `-f matroska -live 1`，MediaMTX 和 mpegts 都不支持 AV1）。

### 8.3 直连模式·拨号到朋友（朋友 `mode=listener`）

角色对调：朋友的路由器转发一个 UDP 端口到他的 PC，接收端 mpv 用 `mode=listener`，发送端用 `mode=caller` 拨过去。谁转发端口谁当 listener，另一方不需要任何设置。

**不要用 `mode=rendezvous`**：ffmpeg 7.1 会把本地 socket 绑到远端地址（"Cannot assign requested address"），两边都在 NAT 后时只能走中继或 VPN。

### 8.4 Tailscale（无需端口转发，未在真实跨境线路上实测）

LXC 已经在 tailnet 里（`tailscale0` = 100.82.8.39）。朋友装 Tailscale 加入同一个 tailnet 后：

- 中继模式：朋友直接连 `100.82.8.39:8890`，不用路由器转发，也不暴露公网端口。
- 直连模式：两台 PC 都在 tailnet 里时，caller / listener 任意一方都不需要端口转发。

注意 Tailscale 的 MTU 是 1280 字节，而默认 SRT 包是 1316 字节载荷 + 16 SRT + 8 UDP + 20 IP = 1360 字节，会被 IP 分片，一旦丢一片整包作废。建议改成 6 个 TS 包一组：发送端 / 接收端 URL 用 `pkt_size=1128`，中继的 `mediamtx.yml` 把 `udpMaxPayloadSize` 改成 `1200`（MediaMTX 会算出 (1200−16)/188×188 = 1128）。这一条没有实测。

