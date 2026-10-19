# hdrcast-signal

**English** | [简体中文](README.zh-CN.md)

The signaling (matchmaking) service of the peer-to-peer mode: a sharer posts "I am live + my candidate addresses", a viewer
sends a join request and the sharer answers it; once both have the other's candidate addresses they punch straight through
to each other. **It only passes on a few hundred bytes of metadata; the video never goes through it.** Every message is
signed with the group key, and the server knows neither the group key K nor msgKey, so it can neither read nor forge the
addresses. Requests are authenticated with an HMAC under serverKey. The protocol contract is `docs/design.md` §8 (in
Chinese); the golden vectors are in `tests/vectors/group-v1.json`.

| File | Purpose |
|---|---|
| `hdrcast_signal.py` | the service itself (standard library only, Python ≥ 3.6) |
| `hdrcast-signal.service` | systemd unit (DynamicUser, read-only system, MemoryMax=64M) |
| `deploy.sh` | deploys, upgrades or uninstalls it over ssh |
| `tests/test_signal.py` | tests (`python3 -m unittest discover -s signal/tests -v`) |

## Deploying

```bash
signal/deploy.sh --ssh ubuntu@101.43.23.94 --identity ~/.ssh/MyServer.pem            # TCP 18650 + UDP 18651
signal/deploy.sh --ssh ubuntu@101.43.23.94 --identity ~/.ssh/MyServer.pem --dry-run  # only print the commands
```

The server needs python3 ≥ 3.6, systemd ≥ 235 and an ssh user with passwordless sudo. The script first checks the versions
and the ports (it stops when another program holds them; an installed hdrcast-signal holding them makes it an upgrade),
test-loads the new version once with the server's own python3, then installs it to `/opt/hdrcast-signal/` and
`/etc/systemd/system/`, runs `enable --now` (`restart` for an upgrade) and finally requests `/v1/health` on the server.
It touches no other service and does not change the firewall.
Other ports: `--port 18660 --echo-port 18661`; `--echo-port 0` turns the UDP echo off.

## Tencent Cloud security group

In the console, add inbound rules to the instance's security group: **TCP 18650** (the HTTP API) and **UDP 18651** (the
echo: without it nobody learns their public IPv4 address, and viewers without IPv6 cannot connect at all), source
`0.0.0.0/0` (plus `::/0` when the instance has IPv6). `deploy.sh` does not change the host's firewall; if ufw / iptables
runs on the host, open the same ports there, too.

## Checking

```bash
curl http://101.43.23.94:18650/v1/health        # → {"ok":true,"v":1}
ssh -i ~/.ssh/MyServer.pem ubuntu@101.43.23.94 'systemctl status hdrcast-signal; journalctl -u hdrcast-signal -n 50'
```

The log has one line per request: source IP, method, path (without the query) and status code. It never records a key,
mac, nonce or body.

## Uninstalling

```bash
signal/deploy.sh --ssh ubuntu@101.43.23.94 --identity ~/.ssh/MyServer.pem --uninstall
```

Stops and removes the service and `/opt/hdrcast-signal`. The group registry `/var/lib/private/hdrcast-signal/groups.json`
stays (old invites keep working after a reinstall); to delete it as well, follow the hint the script prints at the end.

## Running locally

```bash
python3 signal/hdrcast_signal.py --listen 127.0.0.1:18650 --echo-port 18651 --state-dir /tmp/hdrcast-state
```

## State and limits

- Only the group registry (gid → serverKey) is written to disk (temporary file + fsync + atomic replace, mode 0600). Live
  records, join mailboxes and answers stay in memory and are gone after a restart; clients restore them with their next
  heartbeat. On a 404 `unknown group` a client may simply `PUT /v1/g/{gid}` again and retry.
- Limits: body ≤ 8192 bytes; 10 requests/s per source with bursts of 40 (IPv4 per address, IPv6 per /64); at most 64 long
  polls, `wait` ≤ 25 s; per group at most 32 live sharers and 256 answers (expiring after 60 s); each sharer's join mailbox
  holds at most 32 requests (expiring after 60 s); at most 500 groups.
- There is no API to delete a group. When the registry is full (507), stop the service, delete old groups from
  `groups.json` by hand and start it again.
