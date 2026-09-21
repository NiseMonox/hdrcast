#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""hdrcast-signal: the rendezvous server for HdrCast peer-to-peer mode.

Contract: docs/design.md section 8 (protocol v1). Golden vectors: tests/vectors/group-v1.json.

Members of a group (everyone holding the invite's group key K) announce who is live, send join
requests and answers through this server. Everything they send is an opaque envelope {"p","m"}
signed with msgKey, which the server never learns, so it can neither read nor forge candidate
addresses. Requests are authenticated with serverKey (HMAC over method, target, time, nonce and
body). Video never touches this server. Only the group registry (gid -> serverKey) is written to
disk; live records, join mailboxes and answers live in memory and expire on their own.

The optional UDP echo (8.7) tells a client its public address:port as seen from here.

Standard library only, Python >= 3.6. Run with --help for the options.
"""
import argparse
import base64
import collections
import errno
import hashlib
import hmac
import http.server
import ipaddress
import json
import logging
import os
import re
import signal
import socket
import socketserver
import struct
import sys
import tempfile
import threading
import time

PROTOCOL_VERSION = 1
DEFAULT_HTTP_PORT = 18650
DEFAULT_ECHO_PORT = 18651
MAX_BODY = 8192                     # 8.5: larger bodies -> 413
AUTH_HEADER = "X-Hdrcast-Auth"
REGISTRY_FILE = "groups.json"

ID_RE = re.compile(r"\A[A-Za-z0-9_-]{22}\Z")        # gid / host / viewer / sid (8.1)
B64U_RE = re.compile(r"\A[A-Za-z0-9_-]*={0,2}\Z")
AUTH_RE = re.compile(r"\A(0|[1-9][0-9]{0,15})\.([A-Za-z0-9_-]{11})\.([A-Za-z0-9_-]{43})\Z")
ENVELOPE_P_RE = re.compile(r"\A[A-Za-z0-9_-]+={0,2}\Z")
ENVELOPE_M_RE = re.compile(r"\A[A-Za-z0-9_-]{43}=?\Z")  # b64u(HMAC-SHA256), 43 characters
SEQ_RE = re.compile(r"\A[0-9]{1,15}\Z")
WAIT_RE = re.compile(r"\A[0-9]{1,6}(?:\.[0-9]{1,3})?\Z")
CONTENT_LENGTH_RE = re.compile(r"\A[0-9]{1,10}\Z")
VISIBLE_ASCII_RE = re.compile(r"\A[\x21-\x7e]+\Z")
UNSAFE_LOG_CHARS_RE = re.compile(r"[^\x21-\x7e]")

ECHO_MAGIC = b"HDRCECHO"
ECHO_REQUEST_LEN = len(ECHO_MAGIC) + 16
V4_MAPPED_PREFIX = b"\x00" * 10 + b"\xff\xff"

# Rough bookkeeping cost of one stored record, counted against max_stored_bytes.
ENTRY_OVERHEAD = 160

# Lingering close: after our response, discard what the client still sends for at most this long.
LINGER_SECONDS = 1.0
LINGER_BYTES = 256 * 1024

# bind() errors meaning "no usable IPv6 here" (fall back to IPv4), as opposed to "port busy".
NO_IPV6_ERRNOS = frozenset(getattr(errno, name) for name in (
    "EAFNOSUPPORT", "EPROTONOSUPPORT", "EADDRNOTAVAIL", "ENOPROTOOPT") if hasattr(errno, name))

log = logging.getLogger("hdrcast-signal")


# ------------------------------------------------------------------------------------------ encodings

def b64u(data):
    """base64url without padding (8.1)."""
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def b64u_decode(text):
    """Decodes base64url; padding is optional (8.1). Raises ValueError on anything else."""
    if not isinstance(text, str) or not B64U_RE.match(text):
        raise ValueError("not base64url")
    text = text.rstrip("=")
    if len(text) % 4 == 1:
        raise ValueError("bad base64url length")
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


def now_ms():
    return int(time.time() * 1000)


class HttpError(Exception):
    """An error response: {"error": message} with the given status."""

    def __init__(self, status, message, retry_after=None, allow=None, close=False):
        Exception.__init__(self, message)
        self.status = status
        self.message = message
        self.retry_after = retry_after
        self.allow = allow
        self.close = close          # the request stream can no longer be trusted: close the connection


# ------------------------------------------------------------------------------------------ auth (8.5)

def auth_mac(server_key, method, target, ts, nonce, body):
    """mac of X-Hdrcast-Auth; `ts` and `nonce` are the exact strings of the header."""
    body_hash = b64u(hashlib.sha256(body).digest())
    message = "\n".join((method, target, ts, nonce, body_hash)).encode("utf-8")
    return b64u(hmac.new(server_key, message, hashlib.sha256).digest())


def parse_auth(header):
    """'<ts>.<nonce>.<mac>' -> (ts, nonce, mac) strings; HttpError(401) if missing or malformed."""
    if header is None:
        raise HttpError(401, "missing auth")
    match = AUTH_RE.match(header.strip(" \t"))
    if match is None:
        raise HttpError(401, "bad auth header")
    return match.group(1), match.group(2), match.group(3)


def check_auth(server_key, method, target, body, parsed, now, skew_ms):
    """Time window and mac (constant-time); HttpError(401) on failure. Nonce replay is checked by the caller."""
    ts, nonce, mac = parsed
    if abs(now - int(ts)) > skew_ms:
        raise HttpError(401, "stale timestamp")
    expected = auth_mac(server_key, method, target, ts, nonce, body)
    if not hmac.compare_digest(expected.encode("ascii"), mac.encode("ascii")):
        raise HttpError(401, "bad mac")


def verify_auth(server_key, method, target, body, header, now, skew_ms=300000):
    """parse_auth + check_auth; returns the nonce."""
    parsed = parse_auth(header)
    check_auth(server_key, method, target, body, parsed, now, skew_ms)
    return parsed[1]


# ------------------------------------------------------------------------------------------ addresses

def normalize_ip(host):
    """'::ffff:1.2.3.4' -> '1.2.3.4', drops an IPv6 zone; anything unparsable is returned as is."""
    host = host.split("%", 1)[0]
    try:
        ip = ipaddress.ip_address(host)
    except ValueError:
        return host
    if ip.version == 6 and ip.ipv4_mapped is not None:
        return str(ip.ipv4_mapped)
    return str(ip)


def rate_key(host):
    """Rate-limit bucket: the IPv4 address, or the /64 of an IPv6 address (one host owns a whole /64)."""
    host = normalize_ip(host)
    try:
        ip = ipaddress.ip_address(host)
    except ValueError:
        return host
    if ip.version == 6:
        return str(ipaddress.ip_network("%s/64" % ip, strict=False))
    return host


def format_addr(address):
    host, port = address[0], address[1]
    return ("[%s]:%d" if ":" in host else "%s:%d") % (host, port)


def build_echo_reply(data, addr):
    """8.7: 'HDRCECHO'+nonce(16) -> 'HDRCECHO'+nonce+family(4|6)+port(u16 BE)+address; None if malformed.

    `addr` is the source as returned by recvfrom(); IPv4-mapped IPv6 sources are reported as IPv4.
    """
    if len(data) != ECHO_REQUEST_LEN or data[:len(ECHO_MAGIC)] != ECHO_MAGIC:
        return None
    host, port = addr[0], addr[1]
    if not 0 < port < 65536:
        return None
    host = host.split("%", 1)[0]
    try:
        packed = socket.inet_pton(socket.AF_INET6, host)
    except (OSError, ValueError):
        try:
            packed = socket.inet_pton(socket.AF_INET, host)
        except (OSError, ValueError):
            return None
    if len(packed) == 16 and packed[:12] == V4_MAPPED_PREFIX:
        packed = packed[12:]
    family = 4 if len(packed) == 4 else 6
    return bytes(data) + struct.pack(">BH", family, port) + packed


# ------------------------------------------------------------------------------------------ rate limiting

class TokenBuckets(object):
    """One token bucket per key (`rate` tokens/s, capacity `burst`). Thread-safe, at most `max_keys` keys."""

    def __init__(self, rate, burst, max_keys=20000):
        self.rate = float(rate)
        self.burst = float(burst)
        self.max_keys = max_keys
        self._lock = threading.Lock()
        self._buckets = {}                  # key -> [tokens, last update]

    def take(self, key, now=None):
        if now is None:
            now = time.monotonic()
        with self._lock:
            bucket = self._buckets.get(key)
            if bucket is None:
                if len(self._buckets) >= self.max_keys:
                    self._gc_locked(now)
                    if len(self._buckets) >= self.max_keys:
                        return False
                self._buckets[key] = [self.burst - 1.0, now]
                return True
            tokens = min(self.burst, bucket[0] + (now - bucket[1]) * self.rate)
            bucket[1] = now
            if tokens >= 1.0:
                bucket[0] = tokens - 1.0
                return True
            bucket[0] = tokens
            return False

    def gc(self, now=None):
        with self._lock:
            self._gc_locked(time.monotonic() if now is None else now)

    def _gc_locked(self, now):
        # A bucket that has refilled completely is the same as a missing one.
        full = [key for key, b in self._buckets.items() if b[0] + (now - b[1]) * self.rate >= self.burst]
        for key in full:
            del self._buckets[key]


# ------------------------------------------------------------------------------------------ persistence

class Registry(object):
    """gid -> serverKey on disk, the only persistent state. Atomic replace, file mode 0600."""

    def __init__(self, state_dir):
        self.dir = os.path.abspath(state_dir)
        self.path = os.path.join(self.dir, REGISTRY_FILE)

    def load(self):
        os.makedirs(self.dir, mode=0o700, exist_ok=True)
        for name in os.listdir(self.dir):       # leftovers of a crash in the middle of save()
            if name.startswith("." + REGISTRY_FILE + ".") and name.endswith(".tmp"):
                try:
                    os.unlink(os.path.join(self.dir, name))
                except OSError:
                    pass
        try:
            with open(self.path, "rb") as f:
                raw = f.read()
        except FileNotFoundError:
            return {}
        try:
            doc = json.loads(raw.decode("utf-8"))
            if not isinstance(doc, dict) or doc.get("v") != 1 or not isinstance(doc.get("groups"), dict):
                raise ValueError("unexpected layout")
        except ValueError as e:     # includes UnicodeDecodeError and JSONDecodeError
            aside = "%s.corrupt-%d" % (self.path, int(time.time()))
            os.replace(self.path, aside)
            log.error("group registry %s is unreadable (%s); moved it to %s and starting empty",
                      self.path, type(e).__name__, aside)
            return {}
        groups, skipped = {}, 0
        for gid, key in doc["groups"].items():
            try:
                raw_key = b64u_decode(key)
            except ValueError:
                raw_key = b""
            if isinstance(gid, str) and ID_RE.match(gid) and len(raw_key) == 32:
                groups[gid] = raw_key
            else:
                skipped += 1
        if skipped:
            log.error("group registry %s: skipped %d malformed entries", self.path, skipped)
        return groups

    def save(self, groups):
        """`groups`: {gid: b64u(serverKey)}. Temp file in the same directory, fsync, rename, fsync dir."""
        data = json.dumps({"v": 1, "groups": groups}, indent=1, sort_keys=True).encode("utf-8") + b"\n"
        fd, tmp = tempfile.mkstemp(prefix="." + REGISTRY_FILE + ".", suffix=".tmp", dir=self.dir)
        try:
            try:
                if hasattr(os, "fchmod"):
                    os.fchmod(fd, 0o600)
                view = memoryview(data)
                while view:
                    view = view[os.write(fd, view):]
                os.fsync(fd)
            finally:
                os.close(fd)
            os.replace(tmp, self.path)
        except BaseException:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise
        try:
            dir_fd = os.open(self.dir, os.O_RDONLY)
            try:
                os.fsync(dir_fd)
            finally:
                os.close(dir_fd)
        except OSError:
            pass


# ------------------------------------------------------------------------------------------ in-memory state

def _cost(p, m):
    return len(p) + len(m) + ENTRY_OVERHEAD


class _Group(object):
    __slots__ = ("key", "cond", "live", "mailboxes", "answers", "nonces")

    def __init__(self, key, lock):
        self.key = key
        self.cond = threading.Condition(lock)   # shares the state lock: wait() releases it
        self.live = {}                          # host -> (p, m, stored_at)
        self.mailboxes = {}                     # host -> _Mailbox
        self.answers = {}                       # sid -> (p, m, expires_at)
        self.nonces = collections.OrderedDict() # nonce -> expires_at; insertion order == expiry order


class _Mailbox(object):
    __slots__ = ("last_seq", "entries")

    def __init__(self):
        self.last_seq = 0
        self.entries = collections.deque()      # (seq, p, m, expires_at), seq ascending


class SignalState(object):
    """All groups, their live records, join mailboxes, answers and nonce caches.

    One lock guards everything; every group's Condition is bound to it, so a long-poll releases the
    lock while it waits. Monotonic time for expiry, wall-clock milliseconds for the auth window.
    max_stored_bytes and max_nonces are not in the spec: they keep the worst case (many abusive
    sources at once) inside the unit's MemoryMax=64M; beyond them requests get 503 "server busy".
    """

    def __init__(self, registry, max_groups=500, live_ttl=30.0, join_ttl=60.0, answer_ttl=60.0,
                 nonce_ttl=600.0, skew_ms=300000, max_wait=25.0, max_longpolls=64, max_live_hosts=32,
                 max_joins=32, max_answers=256, rate=10.0, burst=40, max_stored_bytes=16 * 1024 * 1024,
                 max_nonces=50000):
        self.registry = registry
        self.max_groups = max_groups
        self.live_ttl = live_ttl
        self.join_ttl = join_ttl
        self.answer_ttl = answer_ttl
        self.nonce_ttl = nonce_ttl
        self.skew_ms = skew_ms
        self.max_wait = max_wait
        self.max_longpolls = max_longpolls
        self.max_live_hosts = max_live_hosts
        self.max_joins = max_joins
        self.max_answers = max_answers
        self.max_stored_bytes = max_stored_bytes
        self.max_nonces = max_nonces
        self.buckets = TokenBuckets(rate, burst) if rate else None
        self.lock = threading.Lock()
        self._registry_lock = threading.Lock()  # serializes registry writers (held across the disk write)
        self.groups = {}
        for gid, key in registry.load().items():
            self.groups[gid] = _Group(key, self.lock)
        self.longpolls = 0
        self.stored_bytes = 0
        self.nonce_count = 0
        self.closing = False

    # ---- request admission

    def allow(self, client_host):
        return self.buckets is None or self.buckets.take(rate_key(client_host))

    def authenticate(self, gid, method, target, body, header):
        parsed = parse_auth(header)                             # 401 missing / malformed
        with self.lock:
            key = self._group(gid).key                          # 404 unknown group
        check_auth(key, method, target, body, parsed, now_ms(), self.skew_ms)  # 401 stale / bad mac
        now = time.monotonic()
        with self.lock:
            self._use_nonce(self._group(gid), parsed[1], now)   # 401 replay

    def register(self, gid, key, method, target, body, header):
        """PUT /v1/g/{gid}: first come, first served. The header must verify with the offered key."""
        parsed = parse_auth(header)
        check_auth(key, method, target, body, parsed, now_ms(), self.skew_ms)
        now = time.monotonic()
        with self._registry_lock:
            with self.lock:
                group = self.groups.get(gid)
                if group is not None:
                    if not hmac.compare_digest(group.key, key):
                        raise HttpError(409, "group exists with a different key")
                    self._use_nonce(group, parsed[1], now)
                    return 200
                if len(self.groups) >= self.max_groups:
                    raise HttpError(507, "too many groups")
                group = _Group(key, self.lock)
                self._use_nonce(group, parsed[1], now)
                self.groups[gid] = group
                snapshot = dict((g, b64u(v.key)) for g, v in self.groups.items())
            try:
                self.registry.save(snapshot)
            except OSError as e:
                log.error("cannot save the group registry: %s", e.strerror or type(e).__name__)
                with self.lock:
                    if self.groups.get(gid) is group:
                        del self.groups[gid]
                        self._forget(group)
                raise HttpError(500, "storage error")
        return 201

    # ---- live records

    def live_put(self, gid, host, p, m):
        now = time.monotonic()
        with self.lock:
            group = self._group(gid)
            self._purge_live(group, now)
            old = group.live.get(host)
            if old is None and len(group.live) >= self.max_live_hosts:
                raise HttpError(429, "too many live hosts")
            self._charge(_cost(p, m) - (_cost(old[0], old[1]) if old else 0))
            group.live[host] = (p, m, now)

    def live_delete(self, gid, host):
        """Goes offline and drops the host's join mailbox (its seq counter restarts at 1)."""
        with self.lock:
            group = self._group(gid)
            old = group.live.pop(host, None)
            if old is not None:
                self.stored_bytes -= _cost(old[0], old[1])
            mailbox = group.mailboxes.pop(host, None)
            if mailbox is not None:
                for entry in mailbox.entries:
                    self.stored_bytes -= _cost(entry[1], entry[2])
            group.cond.notify_all()

    def live_list(self, gid):
        now = time.monotonic()
        with self.lock:
            group = self._group(gid)
            self._purge_live(group, now)
            hosts = [{"host": host, "p": rec[0], "m": rec[1], "age_ms": int((now - rec[2]) * 1000)}
                     for host, rec in sorted(group.live.items())]
        return {"now": now_ms(), "hosts": hosts}

    # ---- join mailboxes

    def join(self, gid, host, p, m):
        now = time.monotonic()
        with self.lock:
            group = self._group(gid)
            if not self._is_live(group, host, now):
                raise HttpError(404, "host not live")
            mailbox = group.mailboxes.get(host)
            if mailbox is None:
                mailbox = group.mailboxes[host] = _Mailbox()
            self._purge_mailbox(mailbox, now)
            if len(mailbox.entries) >= self.max_joins:
                raise HttpError(429, "mailbox full")
            self._charge(_cost(p, m))
            mailbox.last_seq += 1
            mailbox.entries.append((mailbox.last_seq, p, m, now + self.join_ttl))
            group.cond.notify_all()
            return mailbox.last_seq

    def joins(self, gid, host, after, wait):
        deadline = time.monotonic() + wait if wait > 0 else None
        with self.lock:
            group = self._group(gid)

            def ready():
                mailbox = group.mailboxes.get(host)
                if mailbox is None:
                    return []
                self._purge_mailbox(mailbox, time.monotonic())
                # A cursor beyond anything handed out means this server lost its memory (restart,
                # or the mailbox was dropped): hand out everything so the client can resync on `next`.
                floor = after if after <= mailbox.last_seq else 0
                return [entry for entry in mailbox.entries if entry[0] > floor]

            entries = self._wait(group, deadline, ready)
            joins = [{"seq": e[0], "p": e[1], "m": e[2]} for e in entries]
        return {"joins": joins, "next": entries[-1][0] if entries else after}

    # ---- answers

    def answer_put(self, gid, sid, p, m):
        now = time.monotonic()
        with self.lock:
            group = self._group(gid)
            self._purge_answers(group, now)
            old = group.answers.get(sid)
            if old is None and len(group.answers) >= self.max_answers:
                raise HttpError(429, "too many answers")
            self._charge(_cost(p, m) - (_cost(old[0], old[1]) if old else 0))
            group.answers[sid] = (p, m, now + self.answer_ttl)
            group.cond.notify_all()

    def answer_get(self, gid, sid, wait):
        deadline = time.monotonic() + wait if wait > 0 else None
        with self.lock:
            group = self._group(gid)

            def ready():
                rec = group.answers.get(sid)
                if rec is not None and rec[2] <= time.monotonic():
                    del group.answers[sid]
                    self.stored_bytes -= _cost(rec[0], rec[1])
                    return None
                return rec

            rec = self._wait(group, deadline, ready)
        return None if rec is None else {"p": rec[0], "m": rec[1]}

    # ---- housekeeping

    def gc(self):
        now = time.monotonic()
        with self.lock:
            for group in self.groups.values():
                self._purge_live(group, now)
                for host in list(group.mailboxes):
                    mailbox = group.mailboxes[host]
                    self._purge_mailbox(mailbox, now)
                    if not mailbox.entries and host not in group.live:
                        del group.mailboxes[host]
                self._purge_answers(group, now)
                self._purge_nonces(group, now)
        if self.buckets is not None:
            self.buckets.gc()

    def close(self):
        """Wakes every long-poll; they answer right away (empty) and no new ones start waiting."""
        with self.lock:
            self.closing = True
            for group in self.groups.values():
                group.cond.notify_all()

    # ---- internals (call with self.lock held)

    def _group(self, gid):
        group = self.groups.get(gid)
        if group is None:
            raise HttpError(404, "unknown group")
        return group

    def _wait(self, group, deadline, ready):
        result = ready()
        if result or deadline is None or self.closing:
            return result
        if self.longpolls >= self.max_longpolls:
            raise HttpError(503, "too many long-polls", retry_after=1)
        self.longpolls += 1
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return result
                group.cond.wait(remaining)
                result = ready()
                if result or self.closing:
                    return result
        finally:
            self.longpolls -= 1

    def _charge(self, delta):
        if delta > 0 and self.stored_bytes + delta > self.max_stored_bytes:
            raise HttpError(503, "server busy", retry_after=5)
        self.stored_bytes += delta

    def _use_nonce(self, group, nonce, now):
        self._purge_nonces(group, now)
        if nonce in group.nonces:
            raise HttpError(401, "replayed nonce")
        if self.nonce_count >= self.max_nonces:
            for other in self.groups.values():
                self._purge_nonces(other, now)
            if self.nonce_count >= self.max_nonces:
                raise HttpError(503, "server busy", retry_after=5)
        group.nonces[nonce] = now + self.nonce_ttl
        self.nonce_count += 1

    def _purge_nonces(self, group, now):
        nonces = group.nonces
        while nonces:
            expires = next(iter(nonces.values()))
            if expires > now:
                break
            nonces.popitem(last=False)
            self.nonce_count -= 1

    def _is_live(self, group, host, now):
        rec = group.live.get(host)
        if rec is None:
            return False
        if now - rec[2] >= self.live_ttl:
            del group.live[host]
            self.stored_bytes -= _cost(rec[0], rec[1])
            return False
        return True

    def _purge_live(self, group, now):
        for host in [h for h, rec in group.live.items() if now - rec[2] >= self.live_ttl]:
            rec = group.live.pop(host)
            self.stored_bytes -= _cost(rec[0], rec[1])

    def _purge_mailbox(self, mailbox, now):
        entries = mailbox.entries
        while entries and entries[0][3] <= now:
            entry = entries.popleft()
            self.stored_bytes -= _cost(entry[1], entry[2])

    def _purge_answers(self, group, now):
        for sid in [s for s, rec in group.answers.items() if rec[2] <= now]:
            rec = group.answers.pop(sid)
            self.stored_bytes -= _cost(rec[0], rec[1])

    def _forget(self, group):
        """Releases the accounting of a group being removed (registry write failed)."""
        self.nonce_count -= len(group.nonces)
        for rec in group.live.values():
            self.stored_bytes -= _cost(rec[0], rec[1])
        for mailbox in group.mailboxes.values():
            for entry in mailbox.entries:
                self.stored_bytes -= _cost(entry[1], entry[2])
        for rec in group.answers.values():
            self.stored_bytes -= _cost(rec[0], rec[1])
        group.cond.notify_all()


# ------------------------------------------------------------------------------------------ request parsing

ROUTES = (
    (("v1", "health"), ("GET",), "health"),
    (("v1", "g", "{gid}"), ("PUT",), "register"),
    (("v1", "g", "{gid}", "live"), ("GET",), "live_list"),
    (("v1", "g", "{gid}", "live", "{host}"), ("PUT", "DELETE"), "live"),
    (("v1", "g", "{gid}", "live", "{host}", "join"), ("POST",), "join"),
    (("v1", "g", "{gid}", "live", "{host}", "joins"), ("GET",), "joins"),
    (("v1", "g", "{gid}", "answer", "{sid}"), ("PUT", "GET"), "answer"),
)


def match_route(path):
    """-> (name, allowed methods, {placeholder: segment}) or None. Placeholders match any non-empty segment."""
    segments = path.split("/")[1:]
    for pattern, methods, name in ROUTES:
        if len(pattern) != len(segments):
            continue
        params = {}
        for pat, seg in zip(pattern, segments):
            if pat.startswith("{"):
                if not seg:
                    break
                params[pat[1:-1]] = seg
            elif pat != seg:
                break
        else:
            return name, methods, params
    return None


def _reject_duplicates(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise ValueError("duplicate key")
        obj[key] = value
    return obj


def parse_json_object(body):
    """UTF-8 JSON object (no BOM, no duplicate keys) or HttpError(400)."""
    try:
        obj = json.loads(body.decode("utf-8"), object_pairs_hook=_reject_duplicates)
    except (ValueError, RecursionError):
        raise HttpError(400, "bad json")
    if not isinstance(obj, dict):
        raise HttpError(400, "bad json")
    return obj


def parse_envelope(body):
    """{"p","m"} with base64url strings -> (p, m). Other fields are ignored and not stored."""
    obj = parse_json_object(body)
    p, m = obj.get("p"), obj.get("m")
    if not (isinstance(p, str) and isinstance(m, str) and ENVELOPE_P_RE.match(p) and ENVELOPE_M_RE.match(m)):
        raise HttpError(400, "bad envelope")
    return p, m


def parse_register_body(body):
    obj = parse_json_object(body)
    try:
        key = b64u_decode(obj.get("serverKey"))
    except ValueError:
        key = b""
    if len(key) != 32:
        raise HttpError(400, "bad serverKey")
    return key


def parse_query(query):
    params = {}
    for part in query.split("&") if query else ():
        if not part:
            continue
        name, _, value = part.partition("=")
        if name in params:
            raise HttpError(400, "bad query")
        params[name] = value
    return params


def parse_after(value):
    if value is None:
        return 0
    if not SEQ_RE.match(value):
        raise HttpError(400, "bad query")
    return int(value)


def parse_wait(value, max_wait):
    """Seconds, default 0, capped at max_wait (25)."""
    if value is None:
        return 0.0
    if not WAIT_RE.match(value):
        raise HttpError(400, "bad query")
    return min(float(value), max_wait)


def _clean_for_log(text, limit):
    if len(text) > limit:
        text = text[:limit] + "..."
    return UNSAFE_LOG_CHARS_RE.sub(lambda m: "\\x%02x" % min(ord(m.group()), 0xff), text)


class _ClientGone(Exception):
    pass


_STDLIB_ERRORS = {400: "bad request", 414: "request target too long", 431: "request header too large",
                  505: "http version not supported"}


# ------------------------------------------------------------------------------------------ HTTP

class SignalHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "hdrcast-signal/%d" % PROTOCOL_VERSION
    sys_version = ""
    timeout = 30                    # per socket operation: slow senders and idle keep-alive connections
    disable_nagle_algorithm = True

    def setup(self):
        http.server.BaseHTTPRequestHandler.setup(self)
        self.client_ip = normalize_ip(self.client_address[0])

    def handle_one_request(self):
        # The stdlib loop minus do_<METHOD> dispatch, plus JSON errors and one log line per response.
        self._status = None
        self._target = None
        self._route = None
        self._prepared = False
        self._body_len = 0
        self._body_read = False
        self.headers = None
        try:
            self.raw_requestline = self.rfile.readline(65537)
            if len(self.raw_requestline) > 65536:
                self.requestline = ""
                self.request_version = ""
                self.command = ""
                self.send_error(414)
                return
            if not self.raw_requestline:
                self.close_connection = True
                return
            if self.parse_request():
                self._handle()
        except (socket.timeout, ConnectionError):
            self.close_connection = True
        finally:
            if self._status is not None:
                self._log()

    def _handle(self):
        try:
            if not self._prepared:
                self._prepare()
            body = self._read_body()
            status, obj = self._dispatch(body)
        except HttpError as e:
            self._send_error(e)
            return
        except _ClientGone:
            self.close_connection = True
            return
        except Exception:
            log.error("internal error on %s %s", _clean_for_log(self.command or "-", 16), self._log_path(),
                      exc_info=True)
            self._send_error(HttpError(500, "internal error"))
            return
        self._send(status, obj)

    def handle_expect_100(self):
        # Answer "Expect: 100-continue" with the final error if the head alone already fails.
        try:
            self._prepare()
        except HttpError as e:
            self._send_error(e)
            return False
        return http.server.BaseHTTPRequestHandler.handle_expect_100(self)

    def _prepare(self):
        """Everything decidable from the request head: rate limit, route, method, ids, body framing."""
        self._prepared = True
        words = self.requestline.split()
        if len(words) >= 2:
            self._target = words[1]         # PATH_AND_QUERY exactly as sent (8.5)
        if not self.server.state.allow(self.client_address[0]):
            raise HttpError(429, "rate limited", retry_after=1)
        if len(words) != 3 or not self.request_version.startswith("HTTP/1."):
            raise HttpError(400, "bad request", close=True)
        target = self._target
        if not target.startswith("/") or not VISIBLE_ASCII_RE.match(target):
            raise HttpError(400, "bad request target")
        path, _, query = target.partition("?")
        route = match_route(path)
        if route is None:
            raise HttpError(404, "not found")
        name, methods, params = route
        if self.command not in methods:
            raise HttpError(405, "method not allowed", allow=", ".join(methods))
        for value in params.values():
            if not ID_RE.match(value):
                raise HttpError(400, "bad id")
        self._route = (name, params, query)
        self._check_framing()

    def _check_framing(self):
        if self.headers.get_all("Transfer-Encoding"):
            raise HttpError(411, "length required", close=True)     # chunked bodies are not supported
        values = self.headers.get_all("Content-Length") or []
        if not values:
            if self.command in ("PUT", "POST"):
                # no body by HTTP/1.1 rules, but a client that forgot the header may still send one
                raise HttpError(411, "length required", close=True)
            return
        if len(values) > 1 or not CONTENT_LENGTH_RE.match(values[0].strip()):
            raise HttpError(400, "bad content-length")
        length = int(values[0].strip())
        if length > MAX_BODY:
            raise HttpError(413, "body too large")
        if length and self.command not in ("PUT", "POST"):
            raise HttpError(400, "unexpected body")
        self._body_len = length

    def _read_body(self):
        if not self._body_len:
            return b""
        data = self.rfile.read(self._body_len)
        if len(data) != self._body_len:
            raise _ClientGone()
        self._body_read = True
        return data

    def _body_pending(self):
        """True if the request declared a body that has not been read (the connection must then close)."""
        if self._body_read or self.headers is None:
            return False
        if self.headers.get_all("Transfer-Encoding"):
            return True
        return any(v.strip() != "0" for v in (self.headers.get_all("Content-Length") or ()))

    def _auth_header(self):
        values = self.headers.get_all(AUTH_HEADER) or []
        if len(values) > 1:
            raise HttpError(401, "bad auth header")
        return values[0] if values else None

    def _dispatch(self, body):
        name, params, query = self._route
        state = self.server.state
        method, target = self.command, self._target
        if name == "health":
            return 200, {"ok": True, "v": PROTOCOL_VERSION}
        gid = params["gid"]
        header = self._auth_header()
        if name == "register":
            key = parse_register_body(body)
            return state.register(gid, key, method, target, body, header), {"ok": True}
        state.authenticate(gid, method, target, body, header)
        if name == "live_list":
            return 200, state.live_list(gid)
        if name == "live":
            if method == "PUT":
                p, m = parse_envelope(body)
                state.live_put(gid, params["host"], p, m)
            else:
                state.live_delete(gid, params["host"])
            return 204, None
        if name == "join":
            p, m = parse_envelope(body)
            return 202, {"seq": state.join(gid, params["host"], p, m)}
        if name == "joins":
            q = parse_query(query)
            after = parse_after(q.get("after"))
            wait = parse_wait(q.get("wait"), state.max_wait)
            return 200, state.joins(gid, params["host"], after, wait)
        # answer
        if method == "PUT":
            p, m = parse_envelope(body)
            state.answer_put(gid, params["sid"], p, m)
            return 204, None
        wait = parse_wait(parse_query(query).get("wait"), state.max_wait)
        envelope = state.answer_get(gid, params["sid"], wait)
        return (204, None) if envelope is None else (200, envelope)

    # ---- responses

    def _send(self, status, obj=None, retry_after=None, allow=None, close=False):
        """Writes the whole response at once. JSON responses carry Content-Type and Content-Length;
        204 carries neither (RFC 9110 section 8.6)."""
        if close or self.server.state.closing:
            self.close_connection = True
        reason = self.responses.get(status, ("",))[0]
        head = ["HTTP/1.1 %d %s" % (status, reason),
                "Server: " + self.server_version,
                "Date: " + self.date_time_string(),
                "Cache-Control: no-store"]
        body = b""
        if status != 204:
            body = json.dumps({} if obj is None else obj, separators=(",", ":")).encode("ascii")
            head.append("Content-Type: application/json")
            head.append("Content-Length: %d" % len(body))
        if retry_after is not None:
            head.append("Retry-After: %d" % retry_after)
        if allow is not None:
            head.append("Allow: " + allow)
        if self.close_connection:
            head.append("Connection: close")
        elif getattr(self, "request_version", "") == "HTTP/1.0":
            head.append("Connection: keep-alive")
        data = ("\r\n".join(head) + "\r\n\r\n").encode("latin-1")
        if self.command != "HEAD":
            data += body
        self._status = status
        self.wfile.write(data)

    def _send_error(self, e):
        self._send(e.status, {"error": e.message}, retry_after=e.retry_after, allow=e.allow,
                   close=e.close or self._body_pending())

    def send_error(self, code, message=None, explain=None):
        # Only reached from the stdlib request parser; the stream state is unknown, so always close.
        self.close_connection = True
        try:
            self._send(code, {"error": _STDLIB_ERRORS.get(code, "bad request")}, close=True)
        except OSError:
            pass

    # ---- logging: one line per response; never keys, macs, nonces, bodies or query strings

    def _log_path(self):
        return _clean_for_log(self._target.partition("?")[0], 200) if self._target else "-"

    def _log(self):
        if not self.server.quiet:
            log.info("%s %s %s %d", self.client_ip, _clean_for_log(self.command or "-", 16),
                     self._log_path(), self._status)

    def log_message(self, format, *args):
        pass


_REFUSED_BODY = b'{"error":"too many connections"}'
REFUSED_RESPONSE = (b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n"
                    b"Content-Length: " + str(len(_REFUSED_BODY)).encode("ascii") +
                    b"\r\nRetry-After: 1\r\nConnection: close\r\n\r\n" + _REFUSED_BODY)


class SignalHTTPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    """One daemon thread per connection, at most `max_conns` connections (more are refused with 503)."""

    daemon_threads = True
    allow_reuse_address = True
    request_queue_size = 128

    def __init__(self, address, family, dualstack, state, quiet=False, max_conns=100):
        self.address_family = family
        self.dualstack = dualstack
        self.state = state
        self.quiet = quiet
        self.max_conns = max_conns
        self._conn_lock = threading.Lock()
        self._conns = set()
        http.server.HTTPServer.__init__(self, address, SignalHandler)

    def server_bind(self):
        if self.address_family == socket.AF_INET6:
            self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0 if self.dualstack else 1)
        socketserver.TCPServer.server_bind(self)
        # HTTPServer.server_bind would call socket.getfqdn(), a reverse DNS lookup; not needed.
        self.server_name, self.server_port = self.server_address[0], self.server_address[1]

    def process_request(self, request, client_address):
        with self._conn_lock:
            accepted = len(self._conns) < self.max_conns and not self.state.closing
            if accepted:
                self._conns.add(request)
        if accepted:
            thread = threading.Thread(target=self._serve_connection, args=(request, client_address))
            thread.daemon = True
            try:
                thread.start()
                return
            except RuntimeError:        # cannot start a thread (TasksMax, memory)
                with self._conn_lock:
                    self._conns.discard(request)
        try:
            request.setblocking(False)
            request.send(REFUSED_RESPONSE)
            request.shutdown(socket.SHUT_WR)
        except OSError:
            pass
        self.close_request(request)                 # no lingering here: this is the accept thread
        if not self.quiet:
            log.info("%s - - 503 (too many connections)", normalize_ip(client_address[0]))

    def _serve_connection(self, request, client_address):
        try:
            self.finish_request(request, client_address)
        except Exception:
            self.handle_error(request, client_address)
        finally:
            self.shutdown_request(request)
            with self._conn_lock:                   # still counted while lingering
                self._conns.discard(request)

    def shutdown_request(self, request):
        """FIN, then discard what the client still sends until it closes (bounded by LINGER_*).

        Closing a socket with unread data (say, a body we answered 413 or 429 to without reading)
        makes the kernel send a RST, which can destroy the response before the client has read it.
        """
        try:
            request.shutdown(socket.SHUT_WR)
            request.settimeout(LINGER_SECONDS)
            deadline = time.monotonic() + LINGER_SECONDS
            drained = 0
            while drained < LINGER_BYTES and time.monotonic() < deadline:
                chunk = request.recv(65536)
                if not chunk:
                    break
                drained += len(chunk)
        except OSError:
            pass
        self.close_request(request)

    def handle_error(self, request, client_address):
        exc = sys.exc_info()[1]
        if isinstance(exc, (socket.timeout, ConnectionError)):
            return
        log.error("error while serving %s", normalize_ip(client_address[0]), exc_info=True)

    def close_connections(self):
        """Unblocks handler threads sitting in keep-alive reads (used at shutdown)."""
        with self._conn_lock:
            conns = list(self._conns)
        for conn in conns:
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    @property
    def connection_count(self):
        with self._conn_lock:
            return len(self._conns)


def make_http_server(host, port, state, **kwargs):
    """'::' (or '') binds dual-stack, falling back to 0.0.0.0 when the host has no IPv6."""
    if host in ("", "::"):
        try:
            return SignalHTTPServer(("::", port), socket.AF_INET6, True, state, **kwargs)
        except OSError as e:
            if e.errno not in NO_IPV6_ERRNOS:
                raise
        return SignalHTTPServer(("0.0.0.0", port), socket.AF_INET, False, state, **kwargs)
    family = socket.AF_INET6 if ":" in host else socket.AF_INET
    return SignalHTTPServer((host, port), family, False, state, **kwargs)


# ------------------------------------------------------------------------------------------ UDP echo (8.7)

def bind_udp(host, port):
    """Same address rules as make_http_server."""
    if host in ("", "::"):
        sock = None
        try:
            sock = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
            sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
            sock.bind(("::", port))
            return sock
        except OSError as e:
            if sock is not None:
                sock.close()
            if e.errno not in NO_IPV6_ERRNOS:
                raise
        host = "0.0.0.0"
    family = socket.AF_INET6 if ":" in host else socket.AF_INET
    sock = socket.socket(family, socket.SOCK_DGRAM)
    try:
        if family == socket.AF_INET6:
            sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        sock.bind((host, port))
    except BaseException:
        sock.close()
        raise
    return sock


class EchoServer(object):
    """Answers 'HDRCECHO'+nonce with the source address; ignores anything else; rate-limited per source
    (IPv4 address or IPv6 /64) and globally, so it is useless as a reflector."""

    def __init__(self, host, port, rate=5.0, burst=20, global_rate=500.0, poll_interval=0.5):
        self.sock = bind_udp(host, port)
        self.sock.settimeout(poll_interval)         # how quickly stop() is noticed
        self.address = self.sock.getsockname()
        self.dualstack = self.sock.family == socket.AF_INET6 and host in ("", "::")
        self._per_source = TokenBuckets(rate, burst, max_keys=20000)
        self._global = TokenBuckets(global_rate, global_rate, max_keys=1)
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        self._thread = threading.Thread(target=self._run, name="hdrcast-echo")
        self._thread.daemon = True
        self._thread.start()

    def _run(self):
        next_gc = time.monotonic() + 30
        while not self._stop.is_set():
            try:
                data, addr = self.sock.recvfrom(2048)
            except socket.timeout:
                data = None
            except OSError:
                if self._stop.is_set():
                    break
                data = None
                time.sleep(0.05)                    # never spin on a persistent socket error
            now = time.monotonic()
            if data is not None:
                reply = build_echo_reply(data, addr)
                if (reply is not None and self._per_source.take(rate_key(addr[0]), now)
                        and self._global.take("*", now)):
                    try:
                        self.sock.sendto(reply, addr)
                    except OSError:
                        pass
            if now >= next_gc:
                self._per_source.gc(now)
                next_gc = now + 30

    def stop(self):
        self._stop.set()
        if self._thread is not None:
            self._thread.join(2)
        self.sock.close()


# ------------------------------------------------------------------------------------------ assembly

class SignalServer(object):
    """HTTP API + optional UDP echo + periodic garbage collection, each in a background thread.

    echo_port: None disables the echo, 0 picks a free port (tests). poll_interval bounds how long stop()
    waits for the accept loop and the echo socket. Remaining keyword arguments go to SignalState
    (limits and TTLs; tests shorten them).
    """

    def __init__(self, host="::", port=DEFAULT_HTTP_PORT, echo_port=DEFAULT_ECHO_PORT, state_dir="state",
                 quiet=False, max_conns=100, gc_interval=5.0, echo_rate=5.0, echo_burst=20, poll_interval=0.5,
                 **limits):
        self.state = SignalState(Registry(state_dir), **limits)
        self.httpd = make_http_server(host, port, self.state, quiet=quiet, max_conns=max_conns)
        try:
            self.echo = None if echo_port is None else EchoServer(host, echo_port, echo_rate, echo_burst,
                                                                  poll_interval=poll_interval)
        except BaseException:
            self.httpd.server_close()
            raise
        self.poll_interval = poll_interval
        self.gc_interval = gc_interval
        self._stop = threading.Event()
        self._threads = []
        self._started = False
        self._stopped = False

    @property
    def http_address(self):
        return self.httpd.server_address[0], self.httpd.server_address[1]

    @property
    def echo_address(self):
        return None if self.echo is None else (self.echo.address[0], self.echo.address[1])

    def start(self):
        self._started = True
        for target, name in ((self._serve, "hdrcast-http"), (self._gc_loop, "hdrcast-gc")):
            thread = threading.Thread(target=target, name=name)
            thread.daemon = True
            thread.start()
            self._threads.append(thread)
        if self.echo is not None:
            self.echo.start()

    def _serve(self):
        self.httpd.serve_forever(poll_interval=self.poll_interval)

    def _gc_loop(self):
        while not self._stop.wait(self.gc_interval):
            try:
                self.state.gc()
            except Exception:
                log.error("garbage collection failed", exc_info=True)

    def stop(self):
        if self._stopped:
            return
        self._stopped = True
        self._stop.set()
        self.state.close()                          # pending long-polls answer now
        deadline = time.monotonic() + 1.0
        while self.state.longpolls and time.monotonic() < deadline:
            time.sleep(0.01)
        if self._started:
            self.httpd.shutdown()                   # stop accepting (only valid once serve_forever ran)
        self.httpd.server_close()
        self.httpd.close_connections()              # idle keep-alive connections
        if self.echo is not None:
            self.echo.stop()
        for thread in self._threads:
            thread.join(2)


def parse_listen(text):
    """'host:port' or '[v6]:port' -> (host, port); an empty host means '::'."""
    match = re.match(r"\A\[([0-9A-Za-z:.%]+)\]:([0-9]{1,5})\Z", text) or \
        re.match(r"\A([^\[\]:]*):([0-9]{1,5})\Z", text)
    if match is None or int(match.group(2)) > 65535:
        raise ValueError("expected host:port or [v6]:port")
    return match.group(1) or "::", int(match.group(2))


def _setup_logging():
    handler = logging.StreamHandler(sys.stderr)
    # journald adds its own timestamps
    fmt = "%(message)s" if os.environ.get("JOURNAL_STREAM") else "%(asctime)s %(message)s"
    handler.setFormatter(logging.Formatter(fmt))
    log.addHandler(handler)
    log.setLevel(logging.INFO)
    log.propagate = False


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="hdrcast_signal.py",
        description="HdrCast signaling server for peer-to-peer mode (docs/design.md section 8). "
                    "Relays small signed metadata only; video never passes through it.")
    parser.add_argument("--listen", default="[::]:%d" % DEFAULT_HTTP_PORT,
                        help="HTTP address, host:port or [v6]:port (default: %(default)s, dual-stack; "
                             "falls back to 0.0.0.0 on hosts without IPv6)")
    parser.add_argument("--echo-port", type=int, default=DEFAULT_ECHO_PORT,
                        help="UDP echo port, bound on the --listen host (default: %(default)s; 0 disables)")
    parser.add_argument("--state-dir", default=None,
                        help="directory for the group registry (default: $STATE_DIRECTORY, else ./state)")
    parser.add_argument("--max-groups", type=int, default=500,
                        help="maximum number of registered groups (default: %(default)s)")
    parser.add_argument("--quiet", action="store_true", help="do not log a line per request")
    args = parser.parse_args(argv)
    try:
        host, port = parse_listen(args.listen)
    except ValueError:
        parser.error("--listen must be host:port or [v6]:port, e.g. [::]:18650 or 0.0.0.0:18650")
    if not 0 <= args.echo_port <= 65535:
        parser.error("--echo-port must be 0..65535")
    if args.max_groups < 1:
        parser.error("--max-groups must be at least 1")
    state_dir = args.state_dir or os.environ.get("STATE_DIRECTORY", "").split(":")[0] or "state"

    _setup_logging()
    os.umask(0o077)
    try:
        server = SignalServer(host, port, args.echo_port or None, state_dir, quiet=args.quiet,
                              max_groups=args.max_groups)
    except OSError as e:
        log.error("cannot start: %s", e)
        return 1

    stop = threading.Event()

    def on_signal(signum, frame):
        stop.set()

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)
    server.start()
    httpd = server.httpd
    mode = " (dual-stack)" if httpd.dualstack else (" (IPv4 only)" if host in ("", "::") else "")
    echo = "off" if server.echo is None else "on " + format_addr(server.echo_address) + \
        (" (dual-stack)" if server.echo.dualstack else "")
    log.info("hdrcast-signal v%d: HTTP on %s%s, UDP echo %s, %d groups registered, state dir %s",
             PROTOCOL_VERSION, format_addr(server.http_address), mode, echo, len(server.state.groups),
             server.state.registry.dir)
    while not stop.wait(1.0):
        pass
    log.info("stopping")
    server.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
