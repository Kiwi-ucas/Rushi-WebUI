#!/usr/bin/env python3
"""E2E: rushi-web WS truncated-history protocol (v0.5.17).

Speaks raw WebSocket (stdlib only) against a live rushi-web instance:
1. connect -> first frame must be kind=history with window metadata
2. send load_earlier -> expect kind=history_page with the older slice
"""
import base64
import json
import os
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 8480
SESSION = sys.argv[2] if len(sys.argv) > 2 else "e2e"


def ws_connect(path: str):
    s = socket.create_connection((HOST, PORT), timeout=10)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(
        (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {HOST}:{PORT}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(1024)
        if not chunk:
            raise RuntimeError("connection closed during handshake")
        buf += chunk
    head, rest = buf.split(b"\r\n\r\n", 1)
    first = head.decode(errors="replace").split("\r\n")[0]
    if "101" not in first:
        raise RuntimeError(f"handshake failed: {first}")
    return s, rest


def read_exact(s, n, buf):
    while len(buf) < n:
        chunk = s.recv(65536)
        if not chunk:
            raise RuntimeError("connection closed mid-frame")
        buf += chunk
    out, buf = buf[:n], buf[n:]
    return out, buf


def ws_recv(s, buf):
    h, buf = read_exact(s, 2, buf)
    opcode = h[0] & 0x0F
    ln = h[1] & 0x7F
    if ln == 126:
        ext, buf = read_exact(s, 2, buf)
        ln = struct.unpack(">H", ext)[0]
    elif ln == 127:
        ext, buf = read_exact(s, 8, buf)
        ln = struct.unpack(">Q", ext)[0]
    payload, buf = read_exact(s, ln, buf)
    if opcode in (0, 1):
        return payload.decode("utf-8", "replace"), buf
    if opcode == 9:  # ping -> pong
        hdr = bytes([0x8A, len(payload)])
        s.sendall(hdr + payload)
        return None, buf
    if opcode == 8:  # close
        return None, buf
    raise RuntimeError(f"unexpected frame opcode {opcode}")


def ws_send_text(s, text: str):
    payload = text.encode()
    ln = len(payload)
    if ln < 126:
        hdr = bytes([0x81, 0x80 | ln])
    elif ln < 65536:
        hdr = bytes([0x81, 0x80 | 126]) + struct.pack(">H", ln)
    else:
        hdr = bytes([0x81, 0x80 | 127]) + struct.pack(">Q", ln)
    mask = os.urandom(4)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    s.sendall(hdr + mask + masked)


def next_frame(s, buf, want_kind, max_reads=200):
    for _ in range(max_reads):
        text, buf = ws_recv(s, buf)
        if text is None:
            continue
        items = json.loads(text)
        for it in items if isinstance(items, list) else [items]:
            if it.get("kind") == want_kind:
                # the server embeds "events" as an escaped JSON string
                ev = it.get("events")
                if isinstance(ev, str):
                    it = dict(it, events=json.loads(ev))
                return it, buf
        time.sleep(0.05)
    raise RuntimeError(f"never saw kind={want_kind}")


def main():
    s, buf = ws_connect(f"/ws/sessions/{SESSION}")

    # 1. history frame: last 200 of 300 lines
    h, buf = next_frame(s, buf, "history")
    evs = h["events"]
    assert len(evs) == 200, f"history page size {len(evs)} != 200"
    assert h["oldest_line"] == 101, f"oldest_line {h['oldest_line']} != 101"
    assert h["total_lines"] == 300, f"total_lines {h['total_lines']} != 300"
    assert h["has_more"] is True, "has_more should be True (300 > 200)"
    # v0.5.21: rounds across the FULL log (fixture: user_message every
    # 50th line -> 6 rounds after round 1 = 7 total), independent of
    # the window.
    assert h["total_rounds"] == 7, f"total_rounds {h.get('total_rounds')} != 7"
    assert evs[0]["i"] == 101 and evs[-1]["i"] == 300, "window content wrong"
    print("PASS history frame: 200 events, oldest_line=101, has_more=True, total_rounds=7")

    # 2. load_earlier page: 200 lines before line 101 -> lines 1..100? no:
    #    limit=200 before line 101 -> lines 1..100 (only 100 exist)
    ws_send_text(s, json.dumps([{"kind": "load_earlier", "before_line": 101, "limit": 200}]))
    p, buf = next_frame(s, buf, "history_page")
    pevs = p["events"]
    assert p["oldest_line"] == 1, f"oldest_line {p['oldest_line']} != 1"
    assert p["has_more"] is False, "has_more should be False at line 1"
    assert p["total_lines"] == 300
    assert p["total_rounds"] == 7, f"page total_rounds {p.get('total_rounds')} != 7"
    assert len(pevs) == 100, f"expected 100 events, got {len(pevs)}"
    assert pevs[0]["i"] == 1 and pevs[-1]["i"] == 100, "page content wrong"
    print("PASS history_page: 100 events, oldest_line=1, has_more=False, total_rounds=7")

    # 3. no older page left: load_earlier at line 1 -> empty page
    ws_send_text(s, json.dumps([{"kind": "load_earlier", "before_line": 1, "limit": 200}]))
    p2, buf = next_frame(s, buf, "history_page")
    assert p2["events"] == [] and p2["has_more"] is False, "expected empty terminal page"
    print("PASS terminal page: empty events, has_more=False")

    s.close()
    print("E2E OK")


if __name__ == "__main__":
    main()
