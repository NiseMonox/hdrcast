#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Tests for signal/hdrcast_signal.py against docs/design.md section 8 and tests/vectors/group-v1.json.

Run from the repository root:

    python3 -m unittest discover -s signal/tests -v

Servers run in-process on loopback with ephemeral ports and temporary state directories; the CLI
test starts one subprocess and always reaps it. The client helpers below re-implement the client side
of the protocol independently of the server code, the way the C# client has to.
"""
import base64
import hashlib
import hmac
import http.client
import json
import logging
import os
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SIGNAL_DIR = os.path.dirname(HERE)
REPO_ROOT = os.path.dirname(SIGNAL_DIR)
SERVER_SCRIPT = os.path.join(SIGNAL_DIR, "hdrcast_signal.py")
VECTORS_PATH = os.path.join(REPO_ROOT, "tests", "vectors", "group-v1.json")

sys.path.insert(0, SIGNAL_DIR)
import hdrcast_signal as hs  # noqa: E402

with open(VECTORS_PATH, encoding="utf-8") as _f:
    V = json.load(_f)


# ------------------------------------------------------------------------------------------ client side

def b64u(data):
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def b64u_decode(text):
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


def sub(group_key, label):
    return hmac.new(group_key, ("hdrcast/v1/" + label).encode("utf-8"), hashlib.sha256).digest()


def make_envelope(msg_key, kind, gid, key, payload):
    p = b64u(json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))
    m = b64u(hmac.new(msg_key, (kind + "\n" + gid + "\n" + key + "\n" + p).encode("utf-8"), hashlib.sha256).digest())
    return {"p": p, "m": m}


def auth_header(server_key, method, target, body=b"", ts=None, nonce=None):
    """X-Hdrcast-Auth exactly as section 8.5 describes it."""
    if ts is None:
        ts = int(time.time() * 1000)
    if nonce is None:
        nonce = b64u(os.urandom(8))
    message = "\n".join([method, target, str(ts), nonce, b64u(hashlib.sha256(body).digest())])
    mac = b64u(hmac.new(server_key, message.encode("utf-8"), hashlib.sha256).digest())
    return "%d.%s.%s" % (ts, nonce, mac)


def new_id():
    return b64u(os.urandom(16))


def _can_bind(family, host):
    try:
        sock = socket.socket(family, socket.SOCK_STREAM)
    except OSError:
        return False
    try:
        sock.bind((host, 0))
        return True
    except OSError:
        return False
    finally:
        sock.close()


HAVE_IPV6 = socket.has_ipv6 and _can_bind(socket.AF_INET6, "::1")


def wait_for(predicate, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.01)
    raise AssertionError("condition not reached within %.1f s" % timeout)


class Group(object):
    """A group as a client sees it: random K and gid, derived keys, envelope builder."""

    def __init__(self, key=None, gid=None):
        self.k = key or os.urandom(32)
        self.gid = gid or new_id()
        self.server_key = sub(self.k, "server")
        self.msg_key = sub(self.k, "msg")

    def envelope(self, kind, key, **payload):
        payload.setdefault("v", 1)
        payload.setdefault("t", int(time.time() * 1000))
        return make_envelope(self.msg_key, kind, self.gid, key, payload)


class Resp(object):
    def __init__(self, status, headers, body):
        self.status = status
        self.headers = headers
        self.body = body

    def json(self):
        return json.loads(self.body.decode("utf-8"))


class Client(object):
    """Keep-alive HTTP client that signs requests for one group."""

    def __init__(self, host, port, group=None, timeout=15):
        self.host, self.port, self.group, self.timeout = host, port, group, timeout
        self.conn = None
        self.sent = []          # (auth header, body) of every signed request, for the log test

    def close(self):
        if self.conn is not None:
            self.conn.close()
            self.conn = None

    def request(self, method, target, body=None, sign=True, key=None, header=None, headers=None):
        if isinstance(body, (dict, list)):
            body = json.dumps(body, separators=(",", ":")).encode("utf-8")
        hdrs = dict(headers or {})
        if header is None and sign:
            header = auth_header(key or self.group.server_key, method, target, body or b"")
        if header is not None:
            hdrs["X-Hdrcast-Auth"] = header
            self.sent.append((header, body or b""))
        if self.conn is None:
            self.conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
        self.conn.request(method, target, body=body, headers=hdrs)
        response = self.conn.getresponse()
        data = response.read()
        result = Resp(response.status, dict((k.lower(), v) for k, v in response.getheaders()), data)
        if response.will_close:
            self.close()
        return result


def read_response(sock, head_only=False):
    """Reads one HTTP response from a raw socket -> (status, headers, body)."""
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise AssertionError("connection closed before a complete response: %r" % buf)
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    lines = head.decode("latin-1").split("\r\n")
    headers = {}
    for line in lines[1:]:
        name, _, value = line.partition(":")
        headers[name.strip().lower()] = value.strip()
    length = 0 if head_only else int(headers.get("content-length", "0"))
    while len(rest) < length:
        chunk = sock.recv(4096)
        if not chunk:
            break
        rest += chunk
    if len(rest) > length:
        raise AssertionError("unexpected bytes after the response: %r" % rest[length:])
    return int(lines[0].split()[1]), headers, rest


def raw_connect(address, timeout=5):
    sock = socket.create_connection(address, timeout=timeout)
    return sock


def raw_request(address, data, timeout=5):
    """-> (status, headers, body, closed): `closed` is True if the server closed the connection promptly."""
    sock = raw_connect(address, timeout)
    try:
        sock.sendall(data)
        status, headers, body = read_response(sock)
        sock.settimeout(0.3)
        try:
            closed = sock.recv(1) == b""
        except socket.timeout:
            closed = False
        return status, headers, body, closed
    finally:
        sock.close()


# ------------------------------------------------------------------------------------------ fixtures

class ServerTestCase(unittest.TestCase):
    bind_host = "127.0.0.1"

    def start_server(self, host=None, state_dir=None, **kwargs):
        kwargs.setdefault("rate", None)     # no token bucket unless a test asks for one
        kwargs.setdefault("quiet", True)
        kwargs.setdefault("echo_port", None)
        kwargs.setdefault("poll_interval", 0.05)
        if state_dir is None:
            tmp = tempfile.TemporaryDirectory()
            self.addCleanup(tmp.cleanup)
            state_dir = tmp.name
        server = hs.SignalServer(host=host or self.bind_host, port=0, state_dir=state_dir, **kwargs)
        server.start()
        self.addCleanup(server.stop)
        return server

    def client(self, server, group=None, host=None):
        address = server.http_address
        client = Client(host or address[0], address[1], group)
        self.addCleanup(client.close)
        return client

    def register(self, server, group=None):
        group = group or Group()
        client = self.client(server, group)
        r = client.request("PUT", "/v1/g/" + group.gid, {"serverKey": b64u(group.server_key)})
        self.assertEqual(r.status, 201, r.body)
        return group, client

    def assertJsonError(self, resp, status, message=None):
        self.assertEqual(resp.status, status, resp.body)
        self.assertEqual(resp.headers.get("content-type"), "application/json")
        self.assertEqual(int(resp.headers.get("content-length")), len(resp.body))
        doc = resp.json()
        self.assertEqual(list(doc), ["error"])
        self.assertIsInstance(doc["error"], str)
        if message is not None:
            self.assertEqual(doc["error"], message)

    def assertNoContent(self, resp):
        self.assertEqual(resp.status, 204, resp.body)
        self.assertEqual(resp.body, b"")
        self.assertNotIn("content-length", resp.headers)
        self.assertNotIn("content-type", resp.headers)

    def capture_logs(self):
        lines = []

        class Collect(logging.Handler):
            def emit(self, record):
                lines.append(record.getMessage())

        handler = Collect()
        old_level = hs.log.level
        hs.log.addHandler(handler)
        hs.log.setLevel(logging.INFO)

        def restore():
            hs.log.removeHandler(handler)
            hs.log.setLevel(old_level)

        self.addCleanup(restore)
        return lines


# ------------------------------------------------------------------------------------------ vectors & pure functions

class VectorTests(unittest.TestCase):
    def setUp(self):
        self.k = b64u_decode(V["groupKey"])
        self.server_key = sub(self.k, "server")
        self.msg_key = sub(self.k, "msg")

    def test_key_derivation(self):
        self.assertEqual(self.k, bytes(range(32)))
        self.assertEqual(b64u(self.server_key), V["serverKey"])
        self.assertEqual(b64u(self.msg_key), V["msgKey"])
        self.assertEqual(b64u(sub(self.k, "srt"))[:32], V["srtPassphrase"])
        self.assertEqual(b64u(sub(self.k, "punch/" + V["sid"])), V["punchKey"])

    def test_ids_match_the_id_rule(self):
        for name in ("gid", "host", "viewer", "sid"):
            self.assertTrue(hs.ID_RE.match(V[name]), name)
            self.assertEqual(len(b64u_decode(V[name])), 16)

    def test_envelopes(self):
        for kind, key in (("live", V["host"]), ("join", V["host"]), ("answer", V["sid"])):
            with self.subTest(kind=kind):
                envelope = make_envelope(self.msg_key, kind, V["gid"], key, V[kind]["payload"])
                self.assertEqual(envelope, V[kind]["envelope"])
                body = json.dumps(envelope).encode("utf-8")
                self.assertEqual(hs.parse_envelope(body), (envelope["p"], envelope["m"]))

    def test_auth_vector_client_side(self):
        a = V["auth"]
        body = a["body"].encode("utf-8")
        self.assertEqual(json.loads(a["body"]), V["join"]["envelope"])
        self.assertEqual(b64u(hashlib.sha256(body).digest()), a["bodyHash"])
        self.assertEqual(b64u(hashlib.sha256(b"").digest()), a["emptyBodyHash"])
        self.assertEqual(auth_header(self.server_key, a["method"], a["path"], body, a["ts"], a["nonce"]), a["header"])

    def test_auth_vector_server_side(self):
        a = V["auth"]
        body = a["body"].encode("utf-8")
        key, method, path, ts = self.server_key, a["method"], a["path"], a["ts"]
        self.assertEqual(hs.auth_mac(key, method, path, str(ts), a["nonce"], body), a["mac"])
        self.assertEqual(hs.verify_auth(key, method, path, body, a["header"], ts), a["nonce"])
        # window: |now - ts| <= 300000 ms
        hs.verify_auth(key, method, path, body, a["header"], ts + 300000)
        hs.verify_auth(key, method, path, body, a["header"], ts - 300000)
        for now in (ts + 300001, ts - 300001):
            with self.assertRaises(hs.HttpError) as cm:
                hs.verify_auth(key, method, path, body, a["header"], now)
            self.assertEqual((cm.exception.status, cm.exception.message), (401, "stale timestamp"))
        # every signed element matters
        for args in ((key, "PUT", path, body), (key, method, path + "?x=1", body), (key, method, path, body + b" "),
                     (sub(os.urandom(32), "server"), method, path, body)):
            with self.assertRaises(hs.HttpError) as cm:
                hs.verify_auth(args[0], args[1], args[2], args[3], a["header"], ts)
            self.assertEqual((cm.exception.status, cm.exception.message), (401, "bad mac"))
        mac43 = a["mac"]
        for header in ("", "x", "%d.%s" % (ts, a["nonce"]), "0%d.%s.%s" % (ts, a["nonce"], mac43),
                       "%d.%s=.%s" % (ts, a["nonce"], mac43), "%d.%s.%s=" % (ts, a["nonce"], mac43),
                       "%d.%s.%s" % (ts, a["nonce"][:-1], mac43), "%d.%s.%s" % (ts, a["nonce"], mac43[:-1]),
                       "-%d.%s.%s" % (ts, a["nonce"], mac43), "%d.%s.%s.x" % (ts, a["nonce"], mac43)):
            with self.assertRaises(hs.HttpError) as cm:
                hs.verify_auth(key, method, path, body, header, ts)
            self.assertEqual((cm.exception.status, cm.exception.message), (401, "bad auth header"), header)
        with self.assertRaises(hs.HttpError) as cm:
            hs.verify_auth(key, method, path, body, None, ts)
        self.assertEqual((cm.exception.status, cm.exception.message), (401, "missing auth"))

    def test_b64u(self):
        self.assertEqual(hs.b64u(bytes(range(32))), V["groupKey"])
        self.assertEqual(hs.b64u_decode(V["groupKey"]), bytes(range(32)))
        self.assertEqual(hs.b64u_decode(V["groupKey"] + "="), bytes(range(32)))    # padding accepted (8.1)
        self.assertEqual(hs.b64u_decode(V["gid"] + "=="), bytes(range(0xA0, 0xB0)))
        for bad in ("a", "ab+c", "ab/c", "ab c", "a=b", None, 5):
            with self.assertRaises(ValueError):
                hs.b64u_decode(bad)

    def test_echo_vectors(self):
        e = V["echo"]
        request = bytes.fromhex(e["requestHex"])
        self.assertEqual(hs.build_echo_reply(request, ("203.0.113.9", 40001)).hex(), e["replyV4Hex"])
        self.assertEqual(hs.build_echo_reply(request, ("2001:db8::9", 40002, 0, 0)).hex(), e["replyV6Hex"])
        # a dual-stack socket sees IPv4 peers as ::ffff:a.b.c.d; they must still be reported as family 4
        self.assertEqual(hs.build_echo_reply(request, ("::ffff:203.0.113.9", 40001, 0, 0)).hex(), e["replyV4Hex"])
        link_local = hs.build_echo_reply(request, ("fe80::1%eth0", 5, 0, 2))
        self.assertEqual(link_local, request + b"\x06\x00\x05" + socket.inet_pton(socket.AF_INET6, "fe80::1"))
        for bad in (b"", request[:-1], request + b"\x00", b"HDRCECHX" + request[8:], b"hdrcecho" + request[8:]):
            self.assertIsNone(hs.build_echo_reply(bad, ("203.0.113.9", 40001)))
        self.assertIsNone(hs.build_echo_reply(request, ("203.0.113.9", 0)))

    def test_address_helpers(self):
        self.assertEqual(hs.normalize_ip("::ffff:192.0.2.1"), "192.0.2.1")
        self.assertEqual(hs.normalize_ip("2001:DB8::1"), "2001:db8::1")
        self.assertEqual(hs.normalize_ip("fe80::1%eth0"), "fe80::1")
        self.assertEqual(hs.rate_key("::ffff:192.0.2.1"), "192.0.2.1")
        self.assertEqual(hs.rate_key("192.0.2.1"), "192.0.2.1")
        self.assertEqual(hs.rate_key("2001:db8:1:2:3:4:5:6"), "2001:db8:1:2::/64")
        self.assertEqual(hs.rate_key("2001:db8:1:2::9"), "2001:db8:1:2::/64")
        self.assertNotEqual(hs.rate_key("2001:db8:1:3::1"), hs.rate_key("2001:db8:1:2::1"))

    def test_parse_listen(self):
        self.assertEqual(hs.parse_listen("[::]:18650"), ("::", 18650))
        self.assertEqual(hs.parse_listen("[::1]:5"), ("::1", 5))
        self.assertEqual(hs.parse_listen("0.0.0.0:18650"), ("0.0.0.0", 18650))
        self.assertEqual(hs.parse_listen("127.0.0.1:0"), ("127.0.0.1", 0))
        self.assertEqual(hs.parse_listen(":18650"), ("::", 18650))
        for bad in ("18650", "[::]", "::1:80", "127.0.0.1:99999", "[::]:x", "127.0.0.1"):
            with self.assertRaises(ValueError):
                hs.parse_listen(bad)

    def test_defaults_match_the_spec(self):
        with tempfile.TemporaryDirectory() as tmp:
            state = hs.SignalState(hs.Registry(tmp))
        self.assertEqual((state.live_ttl, state.join_ttl, state.answer_ttl, state.nonce_ttl), (30.0, 60.0, 60.0, 600.0))
        self.assertEqual((state.skew_ms, state.max_wait, state.max_longpolls), (300000, 25.0, 64))
        self.assertEqual((state.max_live_hosts, state.max_joins, state.max_answers, state.max_groups), (32, 32, 256, 500))
        self.assertEqual((state.buckets.rate, state.buckets.burst), (10.0, 40.0))
        self.assertEqual((hs.MAX_BODY, hs.DEFAULT_HTTP_PORT, hs.DEFAULT_ECHO_PORT), (8192, 18650, 18651))


# ------------------------------------------------------------------------------------------ HTTP basics

class HealthAndRoutingTests(ServerTestCase):
    def setUp(self):
        self.server = self.start_server()
        self.c = self.client(self.server)

    def test_health(self):
        r = self.c.request("GET", "/v1/health", sign=False)
        self.assertEqual(r.status, 200)
        self.assertEqual(r.json(), {"ok": True, "v": 1})
        self.assertEqual(r.headers["content-type"], "application/json")
        self.assertEqual(int(r.headers["content-length"]), len(r.body))

    def test_unknown_paths_404(self):
        gid, host = new_id(), new_id()
        for path in ("/", "/v1", "/v1/health/", "/v2/health", "/v1/g", "/v1/g/", "/v1/g/%s/" % gid,
                     "/v1/g/%s/nope" % gid, "/v1/g/%s/live/%s/x" % (gid, host), "//v1/health",
                     "/v1/g/%s/answer" % gid):
            with self.subTest(path=path):
                self.assertJsonError(self.c.request("GET", path, sign=False), 404, "not found")

    def test_wrong_method_405(self):
        gid, host, sid = new_id(), new_id(), new_id()
        cases = (("POST", "/v1/health", "GET"), ("GET", "/v1/g/" + gid, "PUT"),
                 ("DELETE", "/v1/g/%s/live" % gid, "GET"), ("GET", "/v1/g/%s/live/%s" % (gid, host), "PUT, DELETE"),
                 ("GET", "/v1/g/%s/live/%s/join" % (gid, host), "POST"),
                 ("POST", "/v1/g/%s/live/%s/joins" % (gid, host), "GET"),
                 ("DELETE", "/v1/g/%s/answer/%s" % (gid, sid), "PUT, GET"), ("PATCH", "/v1/g/" + gid, "PUT"))
        for method, path, allow in cases:
            with self.subTest(method=method, path=path):
                r = self.c.request(method, path, sign=False)
                self.assertJsonError(r, 405, "method not allowed")
                self.assertEqual(r.headers["allow"], allow)

    def test_head_gets_headers_only_and_keeps_the_connection_in_sync(self):
        sock = raw_connect(self.server.http_address)
        try:
            sock.sendall(b"HEAD /v1/health HTTP/1.1\r\nHost: x\r\n\r\n")
            status, headers, body = read_response(sock, head_only=True)
            self.assertEqual(status, 405)
            self.assertEqual(body, b"")
            sock.sendall(b"GET /v1/health HTTP/1.1\r\nHost: x\r\n\r\n")
            status, headers, body = read_response(sock)
            self.assertEqual((status, json.loads(body.decode())), (200, {"ok": True, "v": 1}))
        finally:
            sock.close()

    def test_bad_ids_400(self):
        good = new_id()
        for bad in ("short", "A" * 23, "A" * 21 + "!", "A" * 21 + "=", "%41" + "A" * 19, "A" * 21 + "."):
            cases = (("PUT", "/v1/g/" + bad), ("GET", "/v1/g/%s/live" % bad),
                     ("PUT", "/v1/g/%s/live/%s" % (good, bad)), ("POST", "/v1/g/%s/live/%s/join" % (good, bad)),
                     ("GET", "/v1/g/%s/live/%s/joins" % (bad, good)), ("GET", "/v1/g/%s/answer/%s" % (good, bad)))
            for method, path in cases:
                with self.subTest(path=path):
                    self.assertJsonError(self.c.request(method, path, body=b"{}", sign=False), 400, "bad id")

    def test_non_ascii_and_absolute_targets_400(self):
        address = self.server.http_address
        for target in (b"/v1/h\xc3\xa9alth", b"http://x/v1/health", b"*"):
            with self.subTest(target=target):
                status, headers, body, _ = raw_request(address, b"GET " + target + b" HTTP/1.1\r\nHost: x\r\n\r\n")
                self.assertEqual(status, 400)
                self.assertEqual(json.loads(body.decode()), {"error": "bad request target"})

    def test_malformed_requests_get_json_errors_and_close(self):
        address = self.server.http_address
        for request, status in ((b"GET /v1/health HTTP/x\r\n\r\n", 400), (b"GET /v1/health\r\n\r\n", 400),
                                (b"GET /" + b"a" * 70000 + b" HTTP/1.1\r\n\r\n", 414),
                                (b"GET /v1/health HTTP/1.1\r\nX: " + b"a" * 70000 + b"\r\n\r\n", 431)):
            with self.subTest(request=request[:30], status=status):
                got, headers, body, closed = raw_request(address, request)
                self.assertEqual(got, status)
                self.assertEqual(headers["content-type"], "application/json")
                self.assertIn("error", json.loads(body.decode()))
                self.assertTrue(closed)

    def test_keep_alive_framing(self):
        group, c = self.register(self.server)
        host = new_id()
        path = "/v1/g/%s/live/%s" % (group.gid, host)
        self.assertNoContent(c.request("PUT", path, group.envelope("live", host, host=host)))
        sock = c.conn.sock
        self.assertEqual(c.request("GET", "/v1/g/%s/live" % group.gid).status, 200)
        self.assertJsonError(c.request("GET", "/nope", sign=False), 404)
        self.assertNoContent(c.request("DELETE", path))
        self.assertEqual(c.request("GET", "/v1/health", sign=False).status, 200)
        self.assertIs(c.conn.sock, sock, "responses must keep the connection reusable")


# ------------------------------------------------------------------------------------------ registration & auth

class RegistrationTests(ServerTestCase):
    def setUp(self):
        self.server = self.start_server()

    def test_first_come_same_key_different_key(self):
        group = Group()
        c = self.client(self.server, group)
        body = {"serverKey": b64u(group.server_key)}
        r = c.request("PUT", "/v1/g/" + group.gid, body)
        self.assertEqual((r.status, r.json()), (201, {"ok": True}))
        r = c.request("PUT", "/v1/g/" + group.gid, body)
        self.assertEqual((r.status, r.json()), (200, {"ok": True}))
        other = Group(gid=group.gid)
        r = c.request("PUT", "/v1/g/" + group.gid, {"serverKey": b64u(other.server_key)}, key=other.server_key)
        self.assertJsonError(r, 409, "group exists with a different key")
        self.assertEqual(c.request("GET", "/v1/g/%s/live" % group.gid).status, 200)
        self.assertEqual(c.request("GET", "/v1/g/%s/live" % group.gid, key=other.server_key).status, 401)

    def test_proof_of_possession(self):
        group, attacker = Group(), Group()
        c = self.client(self.server, group)
        r = c.request("PUT", "/v1/g/" + group.gid, {"serverKey": b64u(group.server_key)}, key=attacker.server_key)
        self.assertJsonError(r, 401, "bad mac")
        self.assertJsonError(c.request("GET", "/v1/g/%s/live" % group.gid), 404, "unknown group")

    def test_padded_server_key_is_accepted(self):
        group = Group()
        c = self.client(self.server, group)
        r = c.request("PUT", "/v1/g/" + group.gid, {"serverKey": b64u(group.server_key) + "="})
        self.assertEqual(r.status, 201)
        self.assertEqual(c.request("PUT", "/v1/g/" + group.gid, {"serverKey": b64u(group.server_key)}).status, 200)

    def test_bad_bodies_400(self):
        group = Group()
        c = self.client(self.server, group)
        good = json.dumps({"serverKey": b64u(group.server_key)}).encode()
        for body, message in ((b"", "bad json"), (b"[]", "bad json"), (b"nope", "bad json"), (b"{}", "bad serverKey"),
                              (b'{"serverKey":5}', "bad serverKey"),
                              (json.dumps({"serverKey": b64u(os.urandom(31))}).encode(), "bad serverKey"),
                              (json.dumps({"serverKey": b64u(os.urandom(33))}).encode(), "bad serverKey"),
                              (b'{"serverKey":"***"}', "bad serverKey"),
                              (b'{"serverKey":"a","serverKey":"b"}', "bad json"),
                              (b"\xef\xbb\xbf" + good, "bad json"), (b"\xff" + good, "bad json"),
                              (b"[" * 4000 + b"]" * 4000, "bad json")):
            with self.subTest(body=body[:40]):
                self.assertJsonError(c.request("PUT", "/v1/g/" + group.gid, body), 400, message)

    def test_group_cap_507(self):
        server = self.start_server(max_groups=2)
        first, _ = self.register(server)
        self.register(server)
        extra = Group()
        c = self.client(server, extra)
        self.assertJsonError(c.request("PUT", "/v1/g/" + extra.gid, {"serverKey": b64u(extra.server_key)}),
                             507, "too many groups")
        c = self.client(server, first)
        self.assertEqual(c.request("PUT", "/v1/g/" + first.gid, {"serverKey": b64u(first.server_key)}).status, 200)

    def test_registry_write_failure_rolls_back(self):
        def fail(groups):
            raise OSError(28, "No space left on device")

        self.server.state.registry.save = fail
        group = Group()
        c = self.client(self.server, group)
        lines = self.capture_logs()
        r = c.request("PUT", "/v1/g/" + group.gid, {"serverKey": b64u(group.server_key)})
        self.assertJsonError(r, 500, "storage error")
        self.assertJsonError(c.request("GET", "/v1/g/%s/live" % group.gid), 404, "unknown group")
        self.assertEqual(self.server.state.nonce_count, 0)
        self.assertTrue(any("cannot save the group registry" in line for line in lines))


class AuthTests(ServerTestCase):
    def setUp(self):
        self.server = self.start_server()
        self.group, self.c = self.register(self.server)
        self.path = "/v1/g/%s/live" % self.group.gid

    def test_valid_request(self):
        r = self.c.request("GET", self.path)
        self.assertEqual(r.status, 200)
        doc = r.json()
        self.assertEqual(doc["hosts"], [])
        self.assertLess(abs(doc["now"] - time.time() * 1000), 5000)

    def test_missing_and_malformed_headers(self):
        self.assertJsonError(self.c.request("GET", self.path, sign=False), 401, "missing auth")
        good = auth_header(self.group.server_key, "GET", self.path)
        for header in ("", "garbage", good + ".x", good.replace(".", ":"), good[:-1], "Bearer " + good):
            with self.subTest(header=header):
                self.assertJsonError(self.c.request("GET", self.path, header=header), 401, "bad auth header")
        # surrounding whitespace is not part of a header value
        self.assertEqual(self.c.request("GET", self.path, header=" " + auth_header(self.group.server_key, "GET",
                                                                                   self.path) + "\t").status, 200)

    def test_bad_mac(self):
        wrong_key = sub(os.urandom(32), "server")
        self.assertJsonError(self.c.request("GET", self.path, key=wrong_key), 401, "bad mac")
        other_target = auth_header(self.group.server_key, "GET", self.path + "?x=1")
        self.assertJsonError(self.c.request("GET", self.path, header=other_target), 401, "bad mac")
        host = new_id()
        live = "/v1/g/%s/live/%s" % (self.group.gid, host)
        body = json.dumps(self.group.envelope("live", host, host=host)).encode()
        other_body = auth_header(self.group.server_key, "PUT", live, body + b" ")
        self.assertJsonError(self.c.request("PUT", live, body, header=other_body), 401, "bad mac")
        other_method = auth_header(self.group.server_key, "POST", live, body)
        self.assertJsonError(self.c.request("PUT", live, body, header=other_method), 401, "bad mac")
        self.assertNoContent(self.c.request("PUT", live, body))

    def test_query_is_signed_verbatim(self):
        host = new_id()
        signed = "/v1/g/%s/live/%s/joins?after=0&wait=0" % (self.group.gid, host)
        sent = "/v1/g/%s/live/%s/joins?wait=0&after=0" % (self.group.gid, host)
        header = auth_header(self.group.server_key, "GET", signed)
        self.assertJsonError(self.c.request("GET", sent, header=header), 401, "bad mac")
        self.assertEqual(self.c.request("GET", sent).status, 200)

    def test_time_window(self):
        now = int(time.time() * 1000)
        for ts in (now - 301000, now + 301000):
            header = auth_header(self.group.server_key, "GET", self.path, ts=ts)
            self.assertJsonError(self.c.request("GET", self.path, header=header), 401, "stale timestamp")
        for ts in (now - 290000, now + 290000):
            header = auth_header(self.group.server_key, "GET", self.path, ts=ts)
            self.assertEqual(self.c.request("GET", self.path, header=header).status, 200)

    def test_replayed_nonce(self):
        header = auth_header(self.group.server_key, "GET", self.path)
        self.assertEqual(self.c.request("GET", self.path, header=header).status, 200)
        self.assertJsonError(self.c.request("GET", self.path, header=header), 401, "replayed nonce")
        # same nonce, fresh timestamp and mac: still a replay
        nonce = header.split(".")[1]
        again = auth_header(self.group.server_key, "GET", self.path, ts=int(time.time() * 1000) + 5, nonce=nonce)
        self.assertJsonError(self.c.request("GET", self.path, header=again), 401, "replayed nonce")

    def test_unknown_group_404(self):
        stranger = Group()
        r = self.c.request("GET", "/v1/g/%s/live" % stranger.gid, key=stranger.server_key)
        self.assertJsonError(r, 404, "unknown group")

    def test_duplicate_auth_headers_401(self):
        header = auth_header(self.group.server_key, "GET", self.path)
        request = ("GET %s HTTP/1.1\r\nHost: x\r\nX-Hdrcast-Auth: %s\r\nX-Hdrcast-Auth: %s\r\n\r\n"
                   % (self.path, header, header)).encode()
        status, headers, body, _ = raw_request(self.server.http_address, request)
        self.assertEqual((status, json.loads(body.decode())), (401, {"error": "bad auth header"}))


# ------------------------------------------------------------------------------------------ live / join / answer

class LiveTests(ServerTestCase):
    def test_put_get_age_delete(self):
        server = self.start_server()
        group, c = self.register(server)
        host = new_id()
        envelope = group.envelope("live", host, host=host, name="小明", since=1, cands=["192.168.1.5:51235"],
                                  codec="hevc", mbps=40, max=4, viewers=0)
        path = "/v1/g/%s/live/%s" % (group.gid, host)
        self.assertNoContent(c.request("PUT", path, envelope))
        doc = c.request("GET", "/v1/g/%s/live" % group.gid).json()
        self.assertEqual(len(doc["hosts"]), 1)
        entry = doc["hosts"][0]
        self.assertEqual(sorted(entry), ["age_ms", "host", "m", "p"])
        self.assertEqual((entry["host"], entry["p"], entry["m"]), (host, envelope["p"], envelope["m"]))
        self.assertLess(entry["age_ms"], 200)
        # a client can still verify the envelope: the server returned it byte for byte
        self.assertEqual(make_envelope(group.msg_key, "live", group.gid, host,
                                       json.loads(b64u_decode(entry["p"]).decode("utf-8"))), envelope)
        time.sleep(0.25)
        self.assertGreaterEqual(c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"][0]["age_ms"], 240)
        self.assertNoContent(c.request("PUT", path, envelope))           # heartbeat resets the age
        self.assertLess(c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"][0]["age_ms"], 200)
        self.assertNoContent(c.request("DELETE", path))
        self.assertEqual(c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"], [])
        self.assertNoContent(c.request("DELETE", path))                  # idempotent

    def test_vector_group_round_trip(self):
        server = self.start_server()
        group = Group(key=b64u_decode(V["groupKey"]), gid=V["gid"])
        group, c = self.register(server, group)
        path = "/v1/g/%s/live/%s" % (V["gid"], V["host"])
        self.assertNoContent(c.request("PUT", path, V["live"]["envelope"]))
        entry = c.request("GET", "/v1/g/%s/live" % V["gid"]).json()["hosts"][0]
        self.assertEqual({"p": entry["p"], "m": entry["m"]}, V["live"]["envelope"])
        r = c.request("POST", V["auth"]["path"], V["auth"]["body"].encode("utf-8"))
        self.assertEqual((r.status, r.json()), (202, {"seq": 1}))
        self.assertNoContent(c.request("PUT", "/v1/g/%s/answer/%s" % (V["gid"], V["sid"]), V["answer"]["envelope"]))
        r = c.request("GET", "/v1/g/%s/answer/%s" % (V["gid"], V["sid"]))
        self.assertEqual(r.json(), V["answer"]["envelope"])

    def test_expiry(self):
        server = self.start_server(live_ttl=0.4)
        group, c = self.register(server)
        host = new_id()
        self.assertNoContent(c.request("PUT", "/v1/g/%s/live/%s" % (group.gid, host), group.envelope("live", host)))
        self.assertEqual(len(c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"]), 1)
        time.sleep(0.5)
        self.assertEqual(c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"], [])
        r = c.request("POST", "/v1/g/%s/live/%s/join" % (group.gid, host), group.envelope("join", host))
        self.assertJsonError(r, 404, "host not live")

    def test_hosts_are_listed_sorted(self):
        server = self.start_server()
        group, c = self.register(server)
        hosts = [new_id() for _ in range(5)]
        for host in hosts:
            self.assertNoContent(c.request("PUT", "/v1/g/%s/live/%s" % (group.gid, host), group.envelope("live", host)))
        listed = [h["host"] for h in c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"]]
        self.assertEqual(listed, sorted(hosts))

    def test_live_host_cap_429(self):
        server = self.start_server()
        group, c = self.register(server)
        hosts = [new_id() for _ in range(33)]
        for host in hosts[:32]:
            self.assertNoContent(c.request("PUT", "/v1/g/%s/live/%s" % (group.gid, host), group.envelope("live", host)))
        extra = "/v1/g/%s/live/%s" % (group.gid, hosts[32])
        self.assertJsonError(c.request("PUT", extra, group.envelope("live", hosts[32])), 429, "too many live hosts")
        self.assertNoContent(c.request("PUT", "/v1/g/%s/live/%s" % (group.gid, hosts[0]), group.envelope("live", hosts[0])))
        self.assertNoContent(c.request("DELETE", "/v1/g/%s/live/%s" % (group.gid, hosts[0])))
        self.assertNoContent(c.request("PUT", extra, group.envelope("live", hosts[32])))

    def test_envelope_validation(self):
        server = self.start_server()
        group, c = self.register(server)
        host = new_id()
        path = "/v1/g/%s/live/%s" % (group.gid, host)
        good = group.envelope("live", host)
        for body, message in (({"p": good["p"]}, "bad envelope"), ({"m": good["m"]}, "bad envelope"),
                              ({"p": 1, "m": good["m"]}, "bad envelope"), ({"p": "", "m": good["m"]}, "bad envelope"),
                              ({"p": "a b", "m": good["m"]}, "bad envelope"),
                              ({"p": good["p"], "m": good["m"][:-1]}, "bad envelope"),
                              ({"p": good["p"], "m": good["m"] + "A"}, "bad envelope"),
                              ({"p": good["p"] + "é", "m": good["m"]}, "bad envelope"),
                              ([good["p"], good["m"]], "bad json"), (b"{", "bad json"),
                              ('{"p":"%s","p":"%s","m":"%s"}' % (good["p"], good["p"], good["m"]), "bad json")):
            with self.subTest(body=str(body)[:50]):
                raw = body.encode() if isinstance(body, str) else body
                self.assertJsonError(c.request("PUT", path, raw), 400, message)
        extra = dict(good, x=1, note="dropped")
        self.assertNoContent(c.request("PUT", path, extra))
        entry = c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"][0]
        self.assertEqual(sorted(entry), ["age_ms", "host", "m", "p"])

    def test_body_size_limit(self):
        server = self.start_server()
        group, c = self.register(server)
        host = new_id()
        path = "/v1/g/%s/live/%s" % (group.gid, host)
        m = group.envelope("live", host)["m"]
        filler = 8192 - len(json.dumps({"p": "", "m": m}, separators=(",", ":")))
        body = json.dumps({"p": "A" * filler, "m": m}, separators=(",", ":")).encode()
        self.assertEqual(len(body), 8192)
        self.assertNoContent(c.request("PUT", path, body))
        # 8193: rejected from the header alone, without waiting for (or reading) the body
        header = auth_header(group.server_key, "PUT", path, body + b"A")
        request = ("PUT %s HTTP/1.1\r\nHost: x\r\nContent-Length: 8193\r\nX-Hdrcast-Auth: %s\r\n\r\n"
                   % (path, header)).encode()
        status, headers, rbody, closed = raw_request(server.http_address, request)
        self.assertEqual((status, json.loads(rbody.decode())), (413, {"error": "body too large"}))
        self.assertEqual(headers.get("connection"), "close")
        self.assertTrue(closed)


class EarlyAnswerTests(ServerTestCase):
    """An error sent before the body is read must survive the client still sending that body: the
    server half-closes and drains (RFC 9112 section 9.6) instead of resetting the connection, which on
    Windows would destroy the response before the client reads it."""

    def early_answer(self, server, head, body):
        sock = raw_connect(server.http_address)
        try:
            sock.sendall(head + body[:1000])        # the server answers before the body is complete
            status, headers, rbody = read_response(sock)
            time.sleep(0.05)
            sock.sendall(body[1000:])               # still transmitting: must not be reset
            time.sleep(0.1)
            sock.sendall(b"x" * 16)                 # a reset would surface here at the latest
            self.assertEqual(headers.get("connection"), "close")
            self.assertEqual(sock.recv(1), b"")     # orderly FIN
            return status, json.loads(rbody.decode())
        finally:
            sock.close()

    def test_oversized_body_gets_413(self):
        server = self.start_server()
        body = b"x" * 60000
        head = ("PUT /v1/g/%s/live/%s HTTP/1.1\r\nHost: x\r\nContent-Length: %d\r\n\r\n"
                % (new_id(), new_id(), len(body))).encode()
        self.assertEqual(self.early_answer(server, head, body), (413, {"error": "body too large"}))

    def test_rate_limited_request_with_body_gets_429(self):
        server = self.start_server(rate=0.001, burst=1)
        self.assertEqual(self.client(server).request("GET", "/v1/health", sign=False).status, 200)  # only token
        body = b"{" + b"x" * 8000 + b"}"
        head = ("PUT /v1/g/%s HTTP/1.1\r\nHost: x\r\nContent-Length: %d\r\n\r\n" % (new_id(), len(body))).encode()
        self.assertEqual(self.early_answer(server, head, body), (429, {"error": "rate limited"}))


class JoinTests(ServerTestCase):
    def setUp(self):
        self.server = self.start_server()
        self.group, self.c = self.register(self.server)
        self.host = new_id()
        self.base = "/v1/g/%s/live/%s" % (self.group.gid, self.host)
        self.assertNoContent(self.c.request("PUT", self.base, self.group.envelope("live", self.host)))

    def join(self, client=None, expect=202):
        envelope = self.group.envelope("join", self.host, sid=new_id(), viewer=new_id())
        r = (client or self.c).request("POST", self.base + "/join", envelope)
        self.assertEqual(r.status, expect, r.body)
        return envelope, r

    def test_seq_and_cursor(self):
        sent = []
        for expected_seq in (1, 2, 3):
            envelope, r = self.join()
            self.assertEqual(r.json(), {"seq": expected_seq})
            sent.append(envelope)
        doc = self.c.request("GET", self.base + "/joins?after=0&wait=0").json()
        self.assertEqual(doc["next"], 3)
        self.assertEqual(doc["joins"], [{"seq": i + 1, "p": e["p"], "m": e["m"]} for i, e in enumerate(sent)])
        doc = self.c.request("GET", self.base + "/joins?after=2&wait=0").json()
        self.assertEqual(([j["seq"] for j in doc["joins"]], doc["next"]), ([3], 3))
        doc = self.c.request("GET", self.base + "/joins?after=3&wait=0").json()
        self.assertEqual(doc, {"joins": [], "next": 3})
        doc = self.c.request("GET", self.base + "/joins").json()
        self.assertEqual(([j["seq"] for j in doc["joins"]], doc["next"]), ([1, 2, 3], 3))

    def test_host_not_live_404(self):
        other = new_id()
        r = self.c.request("POST", "/v1/g/%s/live/%s/join" % (self.group.gid, other), self.group.envelope("join", other))
        self.assertJsonError(r, 404, "host not live")

    def test_mailbox_full_429(self):
        for _ in range(32):
            self.join()
        _, r = self.join(expect=429)
        self.assertJsonError(r, 429, "mailbox full")

    def test_mailbox_entries_expire(self):
        server = self.start_server(join_ttl=0.4)
        group, c = self.register(server)
        host = new_id()
        base = "/v1/g/%s/live/%s" % (group.gid, host)
        self.assertNoContent(c.request("PUT", base, group.envelope("live", host)))
        for _ in range(32):
            self.assertEqual(c.request("POST", base + "/join", group.envelope("join", host)).status, 202)
        self.assertEqual(c.request("POST", base + "/join", group.envelope("join", host)).status, 429)
        time.sleep(0.5)
        r = c.request("POST", base + "/join", group.envelope("join", host))
        self.assertEqual((r.status, r.json()), (202, {"seq": 33}))
        doc = c.request("GET", base + "/joins?after=0").json()
        self.assertEqual(([j["seq"] for j in doc["joins"]], doc["next"]), ([33], 33))

    def test_delete_clears_the_mailbox(self):
        self.join()
        self.join()
        self.assertNoContent(self.c.request("DELETE", self.base))
        self.assertEqual(self.c.request("GET", self.base + "/joins?after=0").json(), {"joins": [], "next": 0})
        self.assertNoContent(self.c.request("PUT", self.base, self.group.envelope("live", self.host)))
        _, r = self.join()
        self.assertEqual(r.json(), {"seq": 1})

    def test_cursor_beyond_last_seq_resyncs(self):
        # e.g. the server restarted while the host kept its cursor
        self.assertEqual(self.c.request("GET", self.base + "/joins?after=57&wait=0").json(), {"joins": [], "next": 57})
        self.join()
        doc = self.c.request("GET", self.base + "/joins?after=57&wait=0").json()
        self.assertEqual(([j["seq"] for j in doc["joins"]], doc["next"]), ([1], 1))

    def test_bad_query_400(self):
        for query in ("after=-1", "after=x", "wait=abc", "wait=-1", "after=1&after=2", "wait=1e3", "after=1.5"):
            with self.subTest(query=query):
                self.assertJsonError(self.c.request("GET", self.base + "/joins?" + query), 400, "bad query")
        self.assertEqual(self.c.request("GET", self.base + "/joins?after=0&wait=0.2&x=1").status, 200)


class AnswerTests(ServerTestCase):
    def setUp(self):
        self.server = self.start_server()
        self.group, self.c = self.register(self.server)

    def test_put_get(self):
        sid = new_id()
        path = "/v1/g/%s/answer/%s" % (self.group.gid, sid)
        self.assertNoContent(self.c.request("GET", path))
        envelope = self.group.envelope("answer", sid, sid=sid, ok=True, reason="", cands=[])
        self.assertNoContent(self.c.request("PUT", path, envelope))
        for _ in range(2):      # reading does not consume it
            r = self.c.request("GET", path + "?wait=0")
            self.assertEqual((r.status, r.json()), (200, envelope))
            self.assertEqual(r.headers["content-type"], "application/json")
        replaced = self.group.envelope("answer", sid, sid=sid, ok=False, reason="full", cands=[])
        self.assertNoContent(self.c.request("PUT", path, replaced))
        self.assertEqual(self.c.request("GET", path).json(), replaced)

    def test_expiry(self):
        server = self.start_server(answer_ttl=0.3)
        group, c = self.register(server)
        sid = new_id()
        path = "/v1/g/%s/answer/%s" % (group.gid, sid)
        self.assertNoContent(c.request("PUT", path, group.envelope("answer", sid)))
        self.assertEqual(c.request("GET", path).status, 200)
        time.sleep(0.4)
        self.assertNoContent(c.request("GET", path))

    def test_answer_cap_429(self):
        server = self.start_server(max_answers=3)
        group, c = self.register(server)
        sids = [new_id() for _ in range(4)]
        for sid in sids[:3]:
            self.assertNoContent(c.request("PUT", "/v1/g/%s/answer/%s" % (group.gid, sid), group.envelope("answer", sid)))
        r = c.request("PUT", "/v1/g/%s/answer/%s" % (group.gid, sids[3]), group.envelope("answer", sids[3]))
        self.assertJsonError(r, 429, "too many answers")
        self.assertNoContent(c.request("PUT", "/v1/g/%s/answer/%s" % (group.gid, sids[0]), group.envelope("answer", sids[0])))


# ------------------------------------------------------------------------------------------ long-polls

class LongPollTests(ServerTestCase):
    def background(self, func):
        result = {}

        def run():
            try:
                result["resp"] = func()
            except Exception as e:      # reported by the test that reads it
                result["error"] = e
            result["t"] = time.monotonic()

        thread = threading.Thread(target=run)
        thread.daemon = True
        thread.start()
        return thread, result

    def live_host(self, server):
        group, c = self.register(server)
        host = new_id()
        base = "/v1/g/%s/live/%s" % (group.gid, host)
        self.assertNoContent(c.request("PUT", base, group.envelope("live", host)))
        return group, c, host, base

    def test_joins_wake_up_when_a_join_arrives(self):
        server = self.start_server()
        group, c, host, base = self.live_host(server)
        poller = self.client(server, group)
        thread, result = self.background(lambda: poller.request("GET", base + "/joins?after=0&wait=10"))
        wait_for(lambda: server.state.longpolls == 1)
        posted_at = time.monotonic()
        envelope = group.envelope("join", host)
        self.assertEqual(c.request("POST", base + "/join", envelope).status, 202)
        thread.join(5)
        self.assertNotIn("error", result)
        self.assertLess(result["t"] - posted_at, 1.0)
        doc = result["resp"].json()
        self.assertEqual(doc, {"joins": [{"seq": 1, "p": envelope["p"], "m": envelope["m"]}], "next": 1})

    def test_joins_time_out_with_an_empty_list(self):
        server = self.start_server()
        group, c, host, base = self.live_host(server)
        started = time.monotonic()
        doc = c.request("GET", base + "/joins?after=0&wait=1").json()
        elapsed = time.monotonic() - started
        self.assertEqual(doc, {"joins": [], "next": 0})
        self.assertGreaterEqual(elapsed, 0.9)
        self.assertLess(elapsed, 3)

    def test_wait_is_capped(self):
        server = self.start_server(max_wait=0.5)
        group, c, host, base = self.live_host(server)
        started = time.monotonic()
        self.assertEqual(c.request("GET", base + "/joins?after=0&wait=30").json(), {"joins": [], "next": 0})
        self.assertLess(time.monotonic() - started, 2)

    def test_answer_long_poll(self):
        server = self.start_server()
        group, c = self.register(server)
        sid = new_id()
        path = "/v1/g/%s/answer/%s" % (group.gid, sid)
        poller = self.client(server, group)
        thread, result = self.background(lambda: poller.request("GET", path + "?wait=10"))
        wait_for(lambda: server.state.longpolls == 1)
        put_at = time.monotonic()
        envelope = group.envelope("answer", sid, sid=sid, ok=True, reason="", cands=["192.168.1.5:51235"])
        self.assertNoContent(c.request("PUT", path, envelope))
        thread.join(5)
        self.assertNotIn("error", result)
        self.assertLess(result["t"] - put_at, 1.0)
        self.assertEqual((result["resp"].status, result["resp"].json()), (200, envelope))

    def test_answer_long_poll_times_out_with_204(self):
        server = self.start_server()
        group, c = self.register(server)
        started = time.monotonic()
        r = c.request("GET", "/v1/g/%s/answer/%s?wait=1" % (group.gid, new_id()))
        elapsed = time.monotonic() - started
        self.assertNoContent(r)
        self.assertGreaterEqual(elapsed, 0.9)
        self.assertLess(elapsed, 3)

    def test_long_poll_slots_exhausted_503(self):
        server = self.start_server(max_longpolls=2)
        group, c, host, base = self.live_host(server)
        pollers = [self.client(server, group) for _ in range(2)]
        waits = [self.background(lambda p=p: p.request("GET", base + "/joins?after=0&wait=10")) for p in pollers]
        wait_for(lambda: server.state.longpolls == 2)
        started = time.monotonic()
        r = c.request("GET", base + "/joins?after=0&wait=10")
        self.assertJsonError(r, 503, "too many long-polls")
        self.assertEqual(r.headers.get("retry-after"), "1")
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertEqual(c.request("GET", base + "/joins?after=0&wait=0").status, 200)   # no wait, no slot
        self.assertEqual(c.request("POST", base + "/join", group.envelope("join", host)).status, 202)
        for thread, result in waits:
            thread.join(5)
            self.assertEqual([j["seq"] for j in result["resp"].json()["joins"]], [1])
        wait_for(lambda: server.state.longpolls == 0)

    def test_stop_does_not_hang_on_pending_long_polls(self):
        server = self.start_server()
        group, c, host, base = self.live_host(server)
        poller = self.client(server, group)
        thread, result = self.background(lambda: poller.request("GET", base + "/joins?after=0&wait=20"))
        wait_for(lambda: server.state.longpolls == 1)
        idle = raw_connect(server.http_address)       # an idle keep-alive connection must not block stop()
        self.addCleanup(idle.close)
        wait_for(lambda: server.httpd.connection_count >= 3)
        started = time.monotonic()
        server.stop()
        self.assertLess(time.monotonic() - started, 3)
        thread.join(3)
        self.assertFalse(thread.is_alive())
        if "resp" in result:
            self.assertEqual(result["resp"].json(), {"joins": [], "next": 0})
        self.assertEqual(idle.recv(1), b"")


# ------------------------------------------------------------------------------------------ limits & framing

class LimitTests(ServerTestCase):
    def test_length_required_411(self):
        server = self.start_server()
        group, c = self.register(server)
        host = new_id()
        path = "/v1/g/%s/live/%s" % (group.gid, host)
        header = auth_header(group.server_key, "PUT", path)
        for extra in ("", "Transfer-Encoding: chunked\r\n"):
            with self.subTest(extra=extra):
                request = ("PUT %s HTTP/1.1\r\nHost: x\r\nX-Hdrcast-Auth: %s\r\n%s\r\n" % (path, header, extra)).encode()
                status, headers, body, closed = raw_request(server.http_address, request)
                self.assertEqual((status, json.loads(body.decode())), (411, {"error": "length required"}))
                self.assertTrue(closed)

    def test_bad_content_length_400(self):
        server = self.start_server()
        for value in ("abc", "-1", "1e3", "5\r\nContent-Length: 5"):
            with self.subTest(value=value):
                request = ("PUT /v1/g/%s HTTP/1.1\r\nHost: x\r\nContent-Length: %s\r\n\r\n" % (new_id(), value)).encode()
                status, headers, body, closed = raw_request(server.http_address, request)
                self.assertEqual((status, json.loads(body.decode())), (400, {"error": "bad content-length"}))
                self.assertTrue(closed)

    def test_body_on_get_400(self):
        server = self.start_server()
        request = b"GET /v1/health HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}"
        status, headers, body, _ = raw_request(server.http_address, request)
        self.assertEqual((status, json.loads(body.decode())), (400, {"error": "unexpected body"}))

    def test_token_bucket_429(self):
        server = self.start_server(rate=10, burst=40)
        c = self.client(server)
        responses = [c.request("GET", "/v1/health", sign=False) for _ in range(60)]
        self.assertEqual([r.status for r in responses[:40]], [200] * 40)
        limited = [r for r in responses[40:] if r.status == 429]
        self.assertTrue(limited, "no 429 after the burst")
        self.assertJsonError(limited[0], 429, "rate limited")
        self.assertEqual(limited[0].headers.get("retry-after"), "1")
        time.sleep(0.3)                      # 10 tokens/s refill
        self.assertEqual(c.request("GET", "/v1/health", sign=False).status, 200)

    def test_storage_budget_503(self):
        server = self.start_server(max_stored_bytes=2000)
        group, c = self.register(server)
        hosts = [new_id() for _ in range(3)]

        def put(host):
            envelope = group.envelope("live", host)
            envelope["p"] = "A" * 700
            return c.request("PUT", "/v1/g/%s/live/%s" % (group.gid, host), envelope)

        self.assertNoContent(put(hosts[0]))
        self.assertNoContent(put(hosts[1]))
        self.assertJsonError(put(hosts[2]), 503, "server busy")
        self.assertNoContent(c.request("DELETE", "/v1/g/%s/live/%s" % (group.gid, hosts[0])))
        self.assertNoContent(put(hosts[2]))
        for host in hosts:
            self.assertNoContent(c.request("DELETE", "/v1/g/%s/live/%s" % (group.gid, host)))
        self.assertEqual(server.state.stored_bytes, 0)

    def test_gc_releases_everything(self):
        server = self.start_server(live_ttl=0.2, join_ttl=0.2, answer_ttl=0.2, nonce_ttl=0.2)
        group, c = self.register(server)
        host, sid = new_id(), new_id()
        base = "/v1/g/%s/live/%s" % (group.gid, host)
        self.assertNoContent(c.request("PUT", base, group.envelope("live", host)))
        self.assertEqual(c.request("POST", base + "/join", group.envelope("join", host)).status, 202)
        self.assertNoContent(c.request("PUT", "/v1/g/%s/answer/%s" % (group.gid, sid), group.envelope("answer", sid)))
        self.assertGreater(server.state.stored_bytes, 0)
        self.assertGreater(server.state.nonce_count, 0)
        time.sleep(0.3)
        server.state.gc()
        internal = server.state.groups[group.gid]
        self.assertEqual((server.state.stored_bytes, server.state.nonce_count), (0, 0))
        self.assertEqual((internal.live, internal.mailboxes, internal.answers), ({}, {}, {}))

    def test_expect_100_continue(self):
        server = self.start_server()
        group, c = self.register(server)
        host = new_id()
        path = "/v1/g/%s/live/%s" % (group.gid, host)
        # too large: the final answer comes instead of "100 Continue"
        request = ("PUT %s HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 9000\r\n\r\n" % path).encode()
        status, headers, body, closed = raw_request(server.http_address, request)
        self.assertEqual((status, closed), (413, True))
        # acceptable: 100 Continue, then the body, then 204
        body = json.dumps(group.envelope("live", host)).encode()
        request = ("PUT %s HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: %d\r\nX-Hdrcast-Auth: %s\r\n\r\n"
                   % (path, len(body), auth_header(group.server_key, "PUT", path, body))).encode()
        sock = raw_connect(server.http_address)
        try:
            sock.sendall(request)
            status, headers, _ = read_response(sock)
            self.assertEqual(status, 100)
            sock.sendall(body)
            status, headers, _ = read_response(sock)
            self.assertEqual(status, 204)
        finally:
            sock.close()

    def test_connection_cap_503(self):
        server = self.start_server(max_conns=2)
        idle = [raw_connect(server.http_address) for _ in range(2)]
        for sock in idle:
            self.addCleanup(sock.close)
        wait_for(lambda: server.httpd.connection_count == 2)
        status, headers, body, closed = raw_request(server.http_address, b"")
        self.assertEqual((status, json.loads(body.decode()), closed), (503, {"error": "too many connections"}, True))
        for sock in idle:
            sock.close()
        wait_for(lambda: server.httpd.connection_count == 0)
        self.assertEqual(self.client(server).request("GET", "/v1/health", sign=False).status, 200)


# ------------------------------------------------------------------------------------------ persistence

class PersistenceTests(ServerTestCase):
    def test_registry_survives_a_restart_and_nothing_else_does(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        first = self.start_server(state_dir=tmp.name)
        group, c = self.register(first)
        host = new_id()
        base = "/v1/g/%s/live/%s" % (group.gid, host)
        self.assertNoContent(c.request("PUT", base, group.envelope("live", host)))
        self.assertEqual(c.request("POST", base + "/join", group.envelope("join", host)).status, 202)
        c.close()
        first.stop()

        path = os.path.join(tmp.name, "groups.json")
        self.assertEqual(os.listdir(tmp.name), ["groups.json"])      # no temp files left behind
        self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)
        with open(path, encoding="utf-8") as f:
            self.assertEqual(json.load(f), {"v": 1, "groups": {group.gid: b64u(group.server_key)}})

        second = self.start_server(state_dir=tmp.name)
        c = self.client(second, group)
        self.assertEqual(c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"], [])
        self.assertEqual(c.request("GET", base + "/joins?after=0").json(), {"joins": [], "next": 0})
        self.assertEqual(c.request("PUT", "/v1/g/" + group.gid, {"serverKey": b64u(group.server_key)}).status, 200)
        other = Group(gid=group.gid)
        r = c.request("PUT", "/v1/g/" + group.gid, {"serverKey": b64u(other.server_key)}, key=other.server_key)
        self.assertEqual(r.status, 409)

    def test_corrupt_registry_is_moved_aside(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        with open(os.path.join(tmp.name, "groups.json"), "wb") as f:
            f.write(b"{not json")
        with open(os.path.join(tmp.name, ".groups.json.abc.tmp"), "wb") as f:
            f.write(b"leftover of a crash")
        lines = self.capture_logs()
        server = self.start_server(state_dir=tmp.name)
        self.assertEqual(server.state.groups, {})
        names = sorted(os.listdir(tmp.name))
        self.assertEqual(len(names), 1)
        self.assertTrue(names[0].startswith("groups.json.corrupt-"), names)
        self.assertTrue(any("unreadable" in line for line in lines))
        self.register(server)
        self.assertIn("groups.json", os.listdir(tmp.name))


# ------------------------------------------------------------------------------------------ UDP echo

class EchoTests(unittest.TestCase):
    def start_echo(self, host, **kwargs):
        kwargs.setdefault("poll_interval", 0.05)
        echo = hs.EchoServer(host, 0, **kwargs)
        echo.start()
        self.addCleanup(echo.stop)
        return echo

    def udp(self, family, host, timeout=2.0):
        sock = socket.socket(family, socket.SOCK_DGRAM)
        self.addCleanup(sock.close)
        sock.bind((host, 0))
        sock.settimeout(timeout)
        return sock

    def roundtrip(self, sock, target):
        nonce = os.urandom(16)
        sock.sendto(b"HDRCECHO" + nonce, target)
        data, source = sock.recvfrom(256)
        self.assertEqual(source[1], target[1])
        self.assertEqual(data[:24], b"HDRCECHO" + nonce)
        return data

    def test_ipv4(self):
        echo = self.start_echo("127.0.0.1")
        sock = self.udp(socket.AF_INET, "127.0.0.1")
        request = bytes.fromhex(V["echo"]["requestHex"])
        sock.sendto(request, echo.address)
        data, _ = sock.recvfrom(256)
        # same layout as replyV4Hex, with our own address and port
        self.assertEqual(data, request + b"\x04" + struct.pack(">H", sock.getsockname()[1]) + socket.inet_aton("127.0.0.1"))
        self.assertEqual(len(data), len(bytes.fromhex(V["echo"]["replyV4Hex"])))

    @unittest.skipUnless(HAVE_IPV6, "no IPv6 loopback")
    def test_ipv6(self):
        echo = self.start_echo("::1")
        sock = self.udp(socket.AF_INET6, "::1")
        data = self.roundtrip(sock, echo.address[:2])
        self.assertEqual(data[24:], b"\x06" + struct.pack(">H", sock.getsockname()[1]) +
                         socket.inet_pton(socket.AF_INET6, "::1"))
        self.assertEqual(len(data), len(bytes.fromhex(V["echo"]["replyV6Hex"])))

    @unittest.skipUnless(socket.has_ipv6 and _can_bind(socket.AF_INET6, "::"), "no IPv6 sockets")
    def test_dual_stack_reports_ipv4_peers_as_ipv4(self):
        echo = self.start_echo("::")
        if not echo.dualstack:
            self.skipTest("dual-stack sockets unavailable")
        port = echo.address[1]
        sock4 = self.udp(socket.AF_INET, "127.0.0.1")
        data = self.roundtrip(sock4, ("127.0.0.1", port))
        self.assertEqual(data[24:], b"\x04" + struct.pack(">H", sock4.getsockname()[1]) + socket.inet_aton("127.0.0.1"))
        if HAVE_IPV6:
            sock6 = self.udp(socket.AF_INET6, "::1")
            data = self.roundtrip(sock6, ("::1", port))
            self.assertEqual(data[24], 6)

    def test_malformed_packets_are_ignored(self):
        echo = self.start_echo("127.0.0.1")
        sock = self.udp(socket.AF_INET, "127.0.0.1", timeout=0.3)
        for bad in (b"", b"HDRCECHO", b"HDRCECHO" + os.urandom(15), b"HDRCECHO" + os.urandom(17),
                    b"HDRCECHX" + os.urandom(16), b"hdrcecho" + os.urandom(16), b"\x00" * 24, b"HDRCECHO" * 100):
            sock.sendto(bad, echo.address)
        with self.assertRaises(socket.timeout):
            sock.recvfrom(256)
        sock.settimeout(2)
        self.roundtrip(sock, echo.address)          # still serving

    def test_rate_limited_per_source(self):
        echo = self.start_echo("127.0.0.1", rate=1.0, burst=3)
        sock = self.udp(socket.AF_INET, "127.0.0.1", timeout=0.3)
        for _ in range(10):
            sock.sendto(b"HDRCECHO" + os.urandom(16), echo.address)
        replies = 0
        try:
            while True:
                sock.recvfrom(256)
                replies += 1
        except socket.timeout:
            pass
        self.assertGreaterEqual(replies, 3)
        self.assertLessEqual(replies, 4)

    def test_signal_server_runs_the_echo(self):
        with tempfile.TemporaryDirectory() as tmp:
            server = hs.SignalServer(host="127.0.0.1", port=0, echo_port=0, state_dir=tmp, quiet=True,
                                     poll_interval=0.05)
            server.start()
            try:
                sock = self.udp(socket.AF_INET, "127.0.0.1")
                self.assertEqual(self.roundtrip(sock, server.echo_address)[24], 4)
            finally:
                server.stop()


# ------------------------------------------------------------------------------------------ IPv6 HTTP, logging

class IPv6HttpTests(ServerTestCase):
    @unittest.skipUnless(HAVE_IPV6, "no IPv6 loopback")
    def test_api_over_ipv6_loopback(self):
        server = self.start_server(host="::1")
        group, c = self.register(server)
        host = new_id()
        self.assertNoContent(c.request("PUT", "/v1/g/%s/live/%s" % (group.gid, host), group.envelope("live", host)))
        self.assertEqual(len(c.request("GET", "/v1/g/%s/live" % group.gid).json()["hosts"]), 1)

    @unittest.skipUnless(socket.has_ipv6 and _can_bind(socket.AF_INET6, "::"), "no IPv6 sockets")
    def test_dual_stack_listener_sees_ipv4_clients_as_ipv4(self):
        lines = self.capture_logs()
        server = self.start_server(host="::", quiet=False)
        if not server.httpd.dualstack:
            self.skipTest("dual-stack sockets unavailable")
        self.assertEqual(self.client(server, host="127.0.0.1").request("GET", "/v1/health", sign=False).status, 200)
        wait_for(lambda: len(lines) == 1)
        self.assertEqual(lines[0], "127.0.0.1 GET /v1/health 200")
        if HAVE_IPV6:
            self.assertEqual(self.client(server, host="::1").request("GET", "/v1/health", sign=False).status, 200)
            wait_for(lambda: len(lines) == 2)
            self.assertEqual(lines[1], "::1 GET /v1/health 200")


class LoggingTests(ServerTestCase):
    def test_one_line_per_request_and_no_secrets(self):
        lines = self.capture_logs()
        server = self.start_server(quiet=False)
        group, c = self.register(server)
        host, sid = new_id(), new_id()
        base = "/v1/g/%s/live/%s" % (group.gid, host)
        live = group.envelope("live", host)
        join = group.envelope("join", host)
        answer = group.envelope("answer", sid)
        c.request("PUT", base, live)
        c.request("POST", base + "/join", join)
        c.request("GET", base + "/joins?after=0&wait=0")
        c.request("PUT", "/v1/g/%s/answer/%s" % (group.gid, sid), answer)
        c.request("GET", "/v1/g/%s/answer/%s?wait=0" % (group.gid, sid))
        c.request("GET", "/v1/g/%s/live" % group.gid, key=sub(os.urandom(32), "server"))    # 401
        c.request("GET", "/nope?secret=1", sign=False)
        wait_for(lambda: len(lines) == 8)
        time.sleep(0.1)
        self.assertEqual(len(lines), 8, lines)
        for line in lines:
            self.assertRegex(line, r"\A127\.0\.0\.1 (GET|PUT|POST|DELETE) /[A-Za-z0-9_/-]* \d{3}\Z")
        self.assertEqual(lines[-1], "127.0.0.1 GET /nope 404")
        secrets = [b64u(group.k), b64u(group.server_key), b64u(group.msg_key)]
        for header, body in c.sent:
            secrets.extend(header.split(".")[1:])           # nonce, mac
            if body:
                secrets.append(body.decode("utf-8"))
        for envelope in (live, join, answer):
            secrets.extend([envelope["p"], envelope["m"]])
        text = "\n".join(lines)
        for secret in secrets:
            self.assertNotIn(secret, text)
        self.assertNotIn("?", text)

    def test_quiet(self):
        lines = self.capture_logs()
        server = self.start_server(quiet=True)
        c = self.client(server)
        c.request("GET", "/v1/health", sign=False)
        c.request("GET", "/nope", sign=False)
        time.sleep(0.2)
        self.assertEqual(lines, [])


# ------------------------------------------------------------------------------------------ command line

def read_line(stream, timeout):
    deadline = time.monotonic() + timeout
    fd = stream.fileno()
    buf = b""
    while not buf.endswith(b"\n"):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError("no complete line within %.0f s: %r" % (timeout, buf))
        ready, _, _ = select.select([fd], [], [], remaining)
        if ready:
            chunk = os.read(fd, 1)
            if not chunk:
                raise AssertionError("process ended: %r" % buf)
            buf += chunk
    return buf.decode("utf-8", "replace")


class CliTests(unittest.TestCase):
    def run_server(self, args, env_extra=None):
        env = dict(os.environ)
        for name in ("STATE_DIRECTORY", "JOURNAL_STREAM"):
            env.pop(name, None)
        env.update(env_extra or {})
        proc = subprocess.Popen([sys.executable, "-B", SERVER_SCRIPT] + args, stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=env)

        def reap():
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            proc.stderr.close()

        self.addCleanup(reap)
        return proc

    def test_serves_and_exits_cleanly_on_sigterm_and_sigint(self):
        for signum in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=signum):
                tmp = tempfile.TemporaryDirectory()
                self.addCleanup(tmp.cleanup)
                proc = self.run_server(["--listen", "127.0.0.1:0", "--echo-port", "0", "--state-dir", tmp.name,
                                        "--quiet"])
                line = read_line(proc.stderr, 15)
                match = re.search(r"HTTP on 127\.0\.0\.1:(\d+), UDP echo off, 0 groups registered, state dir (.+)$",
                                  line.strip())
                self.assertIsNotNone(match, line)
                self.assertEqual(os.path.realpath(match.group(2)), os.path.realpath(tmp.name))
                client = Client("127.0.0.1", int(match.group(1)))
                try:
                    r = client.request("GET", "/v1/health", sign=False)
                finally:
                    client.close()
                self.assertEqual((r.status, r.json()), (200, {"ok": True, "v": 1}))
                proc.send_signal(signum)
                self.assertEqual(proc.wait(timeout=10), 0)
                self.assertIn("stopping", proc.stderr.read().decode("utf-8", "replace"))

    def test_state_directory_environment(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        # an empty --state-dir (systemd without $STATE_DIRECTORY) falls back to the environment variable
        proc = self.run_server(["--listen", "127.0.0.1:0", "--echo-port", "0", "--state-dir", ""],
                               {"STATE_DIRECTORY": tmp.name})
        line = read_line(proc.stderr, 15)
        self.assertEqual(os.path.realpath(line.strip().rsplit("state dir ", 1)[1]), os.path.realpath(tmp.name))
        proc.send_signal(signal.SIGTERM)
        self.assertEqual(proc.wait(timeout=10), 0)

    def test_bad_arguments_exit_2(self):
        for args in (["--listen", "nope"], ["--echo-port", "70000"], ["--max-groups", "0"]):
            with self.subTest(args=args):
                proc = self.run_server(args)
                self.assertEqual(proc.wait(timeout=10), 2)


if __name__ == "__main__":
    unittest.main()
