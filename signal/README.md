# hdrcast-signal

P2P 模式的信令（牵线）服务：主播把「我在播 + 候选地址」挂上来，观众发加入请求、主播回应答，双方拿到彼此的候选地址后直接打洞连接。
**只转发几百字节的元数据，视频从不经过它。** 所有消息都用组密钥签名，服务器不知道组密钥 K 和 msgKey，看不到也伪造不了地址；
请求用 serverKey 做 HMAC 鉴权。协议契约见 `docs/design.md` §8，golden 向量在 `tests/vectors/group-v1.json`。

| 文件 | 作用 |
|---|---|
| `hdrcast_signal.py` | 服务本体（仅标准库，Python ≥ 3.6） |
| `hdrcast-signal.service` | systemd 单元（DynamicUser、只读系统、MemoryMax=64M） |
| `deploy.sh` | 通过 ssh 部署 / 升级 / 卸载 |
| `tests/test_signal.py` | 测试（`python3 -m unittest discover -s signal/tests -v`） |

## 部署

```bash
signal/deploy.sh --ssh ubuntu@101.43.23.94 --identity ~/.ssh/MyServer.pem            # TCP 18650 + UDP 18651
signal/deploy.sh --ssh ubuntu@101.43.23.94 --identity ~/.ssh/MyServer.pem --dry-run  # 只打印命令，不执行
```

要求：目标机 python3 ≥ 3.6、systemd ≥ 235、ssh 用户免密 sudo。脚本先检查版本和端口（被别的程序占用就中止；被已装的
hdrcast-signal 占用算升级），在服务器上用它自己的 python3 试加载一次新版本，再装到 `/opt/hdrcast-signal/` 和
`/etc/systemd/system/`，`enable --now`（升级则 `restart`），最后在服务器上请求 `/v1/health`。不碰其它服务，也不改防火墙。
换端口：`--port 18660 --echo-port 18661`；`--echo-port 0` 关闭 UDP 回显。

## 腾讯云安全组

在控制台给实例的安全组加入站规则：**TCP 18650**（HTTP API）和 **UDP 18651**（回显，可选），来源 `0.0.0.0/0`
（实例有 IPv6 再加 `::/0`）。`deploy.sh` 不改主机防火墙；如果主机上开着 ufw/iptables，同样的端口也要放行。

## 检查

```bash
curl http://101.43.23.94:18650/v1/health        # → {"ok":true,"v":1}
ssh -i ~/.ssh/MyServer.pem ubuntu@101.43.23.94 'systemctl status hdrcast-signal; journalctl -u hdrcast-signal -n 50'
```

日志每个请求一行：来源 IP、方法、路径（不含 query）、状态码；不记录 key、mac、nonce、body。

## 卸载

```bash
signal/deploy.sh --ssh ubuntu@101.43.23.94 --identity ~/.ssh/MyServer.pem --uninstall
```

停止并删除服务和 `/opt/hdrcast-signal`。组注册表 `/var/lib/private/hdrcast-signal/groups.json` 会保留（重装后旧邀请码仍然有效），
要彻底删掉按脚本最后的提示执行。

## 本地运行

```bash
python3 signal/hdrcast_signal.py --listen 127.0.0.1:18650 --echo-port 18651 --state-dir /tmp/hdrcast-state
```

## 状态与限制

- 只有组注册表（gid → serverKey）写盘（临时文件 + fsync + 原子替换，权限 0600）；在播记录、加入信箱、应答只在内存里，
  重启即清空，客户端靠下一次心跳恢复。收到 404 `unknown group` 时客户端可以直接重新 `PUT /v1/g/{gid}` 再重试。
- 限制：body ≤ 8192 字节；每个来源 10 次/秒、突发 40（IPv4 按地址，IPv6 按 /64）；long-poll 最多 64 个、`wait` ≤ 25 s；
  每组最多 32 个在播主播、256 个应答（60 s 过期）；每个主播的加入信箱最多 32 条（60 s 过期）；最多 500 个组。
- 组没有删除接口。组数满了（507）就停服务、手工编辑 `groups.json` 删掉旧组、再启动。
