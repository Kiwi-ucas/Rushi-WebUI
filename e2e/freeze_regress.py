#!/usr/bin/env python3
"""E2E: rushi-web v0.5.23 UI-freeze regression test (CDP runner).

Drives the in-app `?test=freeze` flood test through the real headless
Chromium shell over CDP, in REAL time:

  * rAF never fires in the headless shell, so the in-app driver runs a
    16 ms timer chain (see FLOOD_FRAME_MS in web-leptos/src/ws.rs).
  * The shell's `--dump-dom` only ever fires at page load, and a
    virtual-time budget fast-forwards timer due-times (responsiveness
    reads ~0 no matter the load). So the shell is driven through
    `--remote-debugging-port` and the verdict is read from the DOM in
    real time: the app writes `data-freeze-verdict` / `-gap` /
    `-timer` on <html> (+ <title>) when the test finishes.

Usage:
    e2e/freeze_regress.py [port] [mode]
      port : rushi-web port (default 8480)
      mode : "default" expect PASS | "raw" expect FAIL | "both"
Exit codes:
    0  verdict matches expectation
    1  verdict contradicts expectation (regression / lost discrimination)
    2  inconclusive (no verdict within the deadline, launch failure)

Stdlib only (raw-socket WebSocket, same pattern as truncation_ws.py).
"""
import base64
import json
import os
import re
import socket
import struct
import subprocess
import sys
import time
import urllib.request

HOST = "127.0.0.1"
DEFAULT_SHELL = os.path.expanduser(
    "~/Library/Caches/ms-playwright/chromium_headless_shell-1148/chrome-mac/headless_shell"
)
# Real-time deadlines for the in-app test to reach a verdict:
#   default: ~6 s driver + re-parse work
#   raw:     pre-fix per-delta behaviour; the flood work grows with the
#            document and can take well over a minute in a debug WASM.
DEADLINE_DEFAULT_S = 90
DEADLINE_RAW_S = 300

PROBE_JS = """
(() => {
  const d = document.documentElement;
  return JSON.stringify({
    verdict: d.getAttribute("data-freeze-verdict") || "",
    gap: d.getAttribute("data-freeze-gap") || "",
    timer: d.getAttribute("data-freeze-timer") || "",
    title: document.title || ""
  });
})()
""".strip()


def fail_inconclusive(msg: str):
    print(f"INCONCLUSIVE: {msg}", file=sys.stderr)
    sys.exit(2)


# ── raw-socket WebSocket (pattern: truncation_ws.py) ──────────────────
def ws_connect(ws_url: str):
    m = re.match(r"ws://([^:/]+)(?::(\d+))?(/.*)$", ws_url)
    if not m:
        raise RuntimeError(f"bad ws url: {ws_url}")
    host, port, path = m.group(1), m.group(2) or "80", m.group(3)
    s = socket.create_connection((host, int(port)), timeout=15)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(
        (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
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
            raise RuntimeError("connection closed during ws handshake")
        buf += chunk
    head = buf.split(b"\r\n\r\n", 1)[0].decode(errors="replace").split("\r\n")[0]
    if "101" not in head:
        raise RuntimeError(f"ws handshake failed: {head}")
    rest = buf.split(b"\r\n\r\n", 1)[1]
    s.settimeout(2.0)
    return s, rest


def _read_exact(s, n, buf):
    while len(buf) < n:
        chunk = s.recv(65536)
        if not chunk:
            raise RuntimeError("connection closed mid-frame")
        buf += chunk
    return buf[:n], buf[n:]


def ws_recv(s, buf):
    h, buf = _read_exact(s, 2, buf)
    opcode = h[0] & 0x0F
    ln = h[1] & 0x7F
    if ln == 126:
        ext, buf = _read_exact(s, 2, buf)
        ln = struct.unpack(">H", ext)[0]
    elif ln == 127:
        ext, buf = _read_exact(s, 8, buf)
        ln = struct.unpack(">Q", ext)[0]
    payload, buf = _read_exact(s, ln, buf)
    if opcode in (0, 1):
        return payload.decode("utf-8", "replace"), buf
    if opcode == 9:  # ping -> pong
        s.sendall(bytes([0x8A, len(payload)]) + payload)
        return None, buf
    if opcode == 8:
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


# ── CDP over the raw socket ───────────────────────────────────────────
class Cdp:
    def __init__(self, s, buf):
        self.s = s
        self.buf = buf
        self.next_id = 1

    def eval_js(self, expr: str):
        """Runtime.evaluate; returns the returnByValue value (or None)."""
        mid = self.next_id
        self.next_id += 1
        ws_send_text(
            self.s,
            json.dumps(
                {
                    "id": mid,
                    "method": "Runtime.evaluate",
                    "params": {"expression": expr, "returnByValue": True},
                },
            ),
        )
        # The response arrives in ~ms; allow CDP event noise + a slow
        # socket timeout before giving up on this round.
        for _ in range(60):
            try:
                text, self.buf = ws_recv(self.s, self.buf)
            except socket.timeout:
                return None
            if text is None:
                continue
            msg = json.loads(text)
            if msg.get("id") == mid:
                return msg.get("result", {}).get("result", {}).get("value")
        return None


def wait_for_target(port: int, url_frag: str, timeout_s: float = 30.0):
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://{HOST}:{port}/json") as r:
                targets = json.load(r)
            for t in targets:
                if t.get("type") == "page" and url_frag in t.get("url", ""):
                    return t.get("webSocketDebuggerUrl")
        except Exception:
            pass
        time.sleep(0.5)
    return None


def run_case(port: int, mode: str):
    base = f"http://{HOST}:{port}"
    url = f"{base}/?test=freeze"
    expect = "PASS" if mode == "default" else "FAIL"
    deadline_s = DEADLINE_DEFAULT_S if mode == "default" else DEADLINE_RAW_S
    if mode == "raw":
        url = f"{base}/?test=freeze&flood=raw"

    shell = os.environ.get("RUSHI_HEADLESS_SHELL", DEFAULT_SHELL)
    if not os.path.isfile(shell):
        fail_inconclusive(f"headless shell not found at {shell}")
    if not urllib.request.urlopen(f"{base}/", timeout=5).status == 200:
        fail_inconclusive(f"server not reachable at {base}")

    # Free debug port: bind-0 trick.
    probe = socket.socket()
    probe.bind((HOST, 0))
    port = probe.getsockname()[1]
    probe.close()

    proc = subprocess.Popen(
        [
            shell,
            "--headless",
            "--no-sandbox",
            "--disable-gpu",
            f"--remote-debugging-port={port}",
            url,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    print(f"== mode={mode} url={url} (expect {expect}) ==")
    try:
        ws_url = wait_for_target(port, "test=freeze")
        if not ws_url:
            fail_inconclusive("no CDP page target within 30 s")
        s, buf = ws_connect(ws_url)
        try:
            cdp = Cdp(s, buf)
            deadline = time.time() + deadline_s
            last = None
            while time.time() < deadline:
                val = cdp.eval_js(PROBE_JS)
                if val:
                    probe_res = json.loads(val)
                    last = probe_res
                    if probe_res.get("verdict"):
                        break
                time.sleep(1.0)
            if not last or not last.get("verdict"):
                fail_inconclusive(
                    f"no verdict within {deadline_s:.0f}s (title={last.get('title')!r} if seen)"
                )
            verdict = last["verdict"]
            print(
                f"   verdict: {verdict}"
                f" (gap {last.get('gap') or '?'}ms,"
                f" timer {last.get('timer') or '?'}ms)"
            )
            if verdict == "SKIP":
                fail_inconclusive("test self-reported SKIP")
            if verdict == expect:
                print(f"   OK: {mode} mode behaves as expected ({expect})")
                return
            if expect == "PASS":
                print(f"REGRESSION: expected PASS but got {verdict}", file=sys.stderr)
            else:
                print(
                    f"DISCRIMINATION LOST: raw mode unexpectedly {verdict}",
                    file=sys.stderr,
                )
            sys.exit(1)
        finally:
            s.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8480
    mode = sys.argv[2] if len(sys.argv) > 2 else "both"
    if mode not in ("default", "raw", "both"):
        fail_inconclusive(f"unknown mode: {mode}")
    if mode in ("default", "both"):
        run_case(port, "default")
    if mode in ("raw", "both"):
        run_case(port, "raw")
    if mode == "both":
        run_case(port, "default")  # stability re-check after the heavy run


if __name__ == "__main__":
    main()
