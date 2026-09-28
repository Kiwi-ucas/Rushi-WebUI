#!/usr/bin/env python3
"""E2E probe: LIVE_EDGE_TIGHT follow-gate check (v0.5.39).

Reproduces the "yanked to the bottom while I still had ~2 lines to go"
regression and validates the LIVE_EDGE_TIGHT (24 px) tightening of the
auto re-arm gates in web-leptos/src/pile.rs:

  * rearm:down          now requires d <= LIVE_EDGE_TIGHT (was <= 80 px)
  * rearm:passive-clamp now requires d <= LIVE_EDGE_TIGHT (was <= 80 px)
  * tier-1 round re-arm now requires prev_dist <= LIVE_EDGE_TIGHT

Scenarios (drive full Chromium `--headless=new` over CDP, read the
__rushiPile() diagnostic):

  1. rearm:down at the true bottom — release the follow with an up-tick at
     the bottom, then a down-tick at the bottom. EXPECT rearm:down +
     stick=true + dist~0 (reaching the bottom re-engages the follow).
  2. watching at the bottom survives a round-end shrink — stick=true at
     the bottom, hide the last card (the round-end finalize shape). EXPECT
     stick to STAY true, dist~0, pull:settle (the follow survives a round
     end while watching).
  3. released at the bottom re-arms on shrink (v0.5.38 guard) — release
     with an up-tick at the bottom (d~0, reading NOT latched), hide the
     new last card. EXPECT rearm:passive-clamp + stick=true. Under the
     pre-fix code this is the case that must keep working; the 24 px
     tightening must not wedge it.

Device-verify only (cannot be reached in headless): a reader parked ~2
lines (~40 px) above the bottom is NOT yanked when a round ends. The
headless synthetic wheel event does not physically scroll the viewport,
and the programmatic parking that would simulate it is pulled straight
back by the stale-prev_dist passive-clamp re-arm — which is the follow
behaviour we WANT, so the headless test cannot isolate the 40 px case.
To verify on a device: scroll up ~2 lines, let a round end, run
__rushiPile(), and expect stick=false, reading=false, prev_dist ≈ the
distance you read (> 24). A pre-fix build shows stick=true there
(yanked to the bottom).

Usage:
    e2e/liveedge_probe.py [port]
Exit codes: 0 pass, 1 a scenario contradicted the expectation, 2 inconclusive.
Stdlib only (raw-socket WebSocket, same pattern as touch_regress.py).
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
DEFAULT_CHROME = os.path.expanduser(
    "~/Library/Caches/ms-playwright/chromium-1148/"
    "chrome-mac/Chromium.app/Contents/MacOS/Chromium"
)
PROFILE_DIR = "/tmp/rushi-liveedge-regress-profile"

PILE_JS = "typeof __rushiPile === 'function' ? __rushiPile() : ''"

WHEEL = (
    "(function(){var el=document.getElementById('transcript');"
    "if(!el)return 'no-el';"
    "el.dispatchEvent(new WheelEvent('wheel',{deltaY:__D__,bubbles:true,cancelable:true}));"
    "return 'tick';})()"
)
HIDE_LAST = (
    "(function(){var el=document.getElementById('transcript');"
    "var cards=el.querySelectorAll('.event');"
    "var last=null;for(var i=cards.length-1;i>=0;i--){if(cards[i].offsetParent!==null){last=cards[i];break;}}"
    "if(!last)return JSON.stringify({err:'no-card'});"
    "last.style.display='none';return JSON.stringify({hidden:true});})()"
)


def fail_inconclusive(msg: str):
    print(f"INCONCLUSIVE: {msg}", file=sys.stderr)
    sys.exit(2)


# ── raw-socket WebSocket (pattern: e2e/touch_regress.py) ────────────
def ws_connect(ws_url: str):
    m = re.match(r"ws://([^:/]+)(?::(\d+))?(/.*)$", ws_url)
    if not m:
        raise RuntimeError(f"bad ws url: {ws_url}")
    host, port, path = m.group(1), m.group(2) or "80", m.group(3)
    s = socket.create_connection((host, int(port)), timeout=15)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(
        (
            f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(1024)
        if not chunk:
            raise RuntimeError("closed during ws handshake")
        buf += chunk
    head = buf.split(b"\r\n\r\n", 1)[0].decode(errors="replace").split("\r\n")[0]
    if "101" not in head:
        raise RuntimeError(f"ws handshake failed: {head}")
    rest = buf.split(b"\r\n\r\n", 1)[1]
    s.settimeout(3.0)
    return s, rest


def _read_exact(s, n, buf):
    while len(buf) < n:
        chunk = s.recv(65536)
        if not chunk:
            raise RuntimeError("closed mid-frame")
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
    if opcode == 9:
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


class Cdp:
    def __init__(self, s, buf):
        self.s = s
        self.buf = buf
        self.next_id = 1

    def eval_js(self, expr: str, await_promise: bool = False):
        mid = self.next_id
        self.next_id += 1
        ws_send_text(
            self.s,
            json.dumps(
                {
                    "id": mid,
                    "method": "Runtime.evaluate",
                    "params": {
                        "expression": expr,
                        "returnByValue": True,
                        "awaitPromise": await_promise,
                    },
                },
            ),
        )
        for _ in range(90):
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


def wait_for_target(port: int, url_frag: str, proc, timeout_s: float = 30.0):
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if proc.poll() is not None:
            return None
        try:
            with urllib.request.urlopen(f"http://{HOST}:{port}/json", timeout=3) as r:
                targets = json.load(r)
            for t in targets:
                if t.get("type") == "page" and url_frag in t.get("url", ""):
                    return t.get("webSocketDebuggerUrl")
        except Exception:
            pass
        time.sleep(0.5)
    return None


def parse_pile(report: str):
    stick = re.search(r"stick=(true|false)", report)
    hist = re.search(r"stick_hist=\[([^\]]*)\]", report)
    dist = re.search(r"dist=([\d.]+)", report)
    rd = re.search(r"reading=(true|false)", report)
    return {
        "raw": report,
        "stick": stick.group(1) if stick else None,
        "hist": hist.group(1) if hist else "",
        "dist": float(dist.group(1)) if dist else None,
        "reading": rd.group(1) if rd else None,
    }


def settle(cdp, seconds):
    time.sleep(seconds)
    return parse_pile(cdp.eval_js(PILE_JS) or "")


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8480
    base = f"http://{HOST}:{port}"
    chrome = os.environ.get("RUSHI_CHROMIUM", DEFAULT_CHROME)
    if not os.path.isfile(chrome):
        fail_inconclusive(f"chromium not found at {chrome}")
    if urllib.request.urlopen(f"{base}/", timeout=5).status != 200:
        fail_inconclusive(f"server not reachable at {base}")

    probe = socket.socket()
    probe.bind((HOST, 0))
    cdp_port = probe.getsockname()[1]
    probe.close()
    proc = subprocess.Popen(
        [
            chrome,
            "--headless=new",
            "--no-sandbox",
            "--disable-gpu",
            f"--remote-debugging-port={cdp_port}",
            f"--user-data-dir={PROFILE_DIR}",
            "--window-size=1280,800",
            base + "/",
        ],
        stdout=subprocess.DEVNULL,
        stderr=open("/tmp/rushi-liveedge-regress-chrome.log", "wb"),
    )
    print(f"== live-edge probe url={base}/ (LIVE_EDGE_TIGHT gate) ==")
    try:
        ws_url = wait_for_target(cdp_port, f"127.0.0.1:{port}", proc)
        if not ws_url:
            import pathlib

            log = pathlib.Path("/tmp/rushi-liveedge-regress-chrome.log")
            tail = log.read_text(errors="replace")[-2500:] if log.exists() else ""
            print(f"   chrome stderr tail:\n{tail}", file=sys.stderr)
            fail_inconclusive(f"no CDP page target within 30 s (port {cdp_port})")
        s, buf = ws_connect(ws_url)
        cdp = Cdp(s, buf)

        report = ""
        deadline = time.time() + 90
        while time.time() < deadline:
            v = cdp.eval_js(PILE_JS)
            if v and "init=1" in v:
                report = v
                break
            time.sleep(1.0)
        if not report:
            fail_inconclusive(f"pile never initialized (last: {report[:120]!r})")

        # Open a session with content, parked at the bottom (stick=true).
        n_items = cdp.eval_js("document.querySelectorAll('.session-item').length")
        n_items = int(n_items) if n_items and str(n_items).isdigit() else 1
        opened = False
        for i in range(n_items):
            cdp.eval_js(f"document.querySelectorAll('.session-item')[{i}].click(); 'ok'")
            deadline = time.time() + 8
            st = parse_pile("")
            while time.time() < deadline:
                st = parse_pile(cdp.eval_js(PILE_JS) or "")
                m_ev = re.search(r"events=(\d+)", st["raw"])
                m_cr = re.search(r"cards=(\d+)", st["raw"])
                m_lr = re.search(r"last_range=(\d+)", st["raw"])
                if (
                    st["stick"] == "true"
                    and m_ev and m_cr and m_lr
                    and int(m_ev.group(1)) > 0
                    and int(m_cr.group(1)) > 0
                    and int(m_lr.group(1)) > 0
                ):
                    opened = True
                    break
                time.sleep(0.5)
            if opened:
                break
        if not opened:
            fail_inconclusive(f"no session with content + stick=true (last: {st['raw'][:200]})")
        time.sleep(1.0)  # let history-load parking settle
        print(f"   session open: stick={st['stick']} dist={st['dist']}")

        # ── Scenario 1: rearm:down at the true bottom ─────────────────
        # An up-tick at the bottom releases the follow (stray tick:
        # d~0 so reading is NOT latched). The down-tick must re-arm it:
        # the user is at the live edge. Under the pre-fix band this
        # also fired at 80 px; v0.5.39 requires actually reaching the
        # bottom (d <= 24).
        cdp.eval_js(WHEEL.replace("__D__", "-120"))          # up-tick -> release:up-input
        st = settle(cdp, 0.6)
        released = st["stick"] == "false"
        cdp.eval_js(WHEEL.replace("__D__", "120"))           # down tick at the bottom
        st = settle(cdp, 0.6)
        p1 = st
        ok1 = (
            released
            and p1["stick"] == "true"
            and "rearm:down" in p1["hist"]
            and (p1["dist"] is None or p1["dist"] <= 5.0)
        )
        print(f"   1 down@0:  stick={p1['stick']} dist={p1['dist']} hist=[{p1['hist']}]")
        print(
            "   OK: reaching the true bottom re-armed the follow (rearm:down)"
            if ok1
            else "   REGRESSION: down tick at the bottom did not re-arm the follow"
        )

        # ── Scenario 2: watching at the bottom survives a round-end ────
        # stick=true (from S1). Hiding the last card is the round-end
        # finalize shape (content shrinks at the bottom). The follow must
        # SURVIVE via the settle pull — no re-arm is even needed.
        hid2 = cdp.eval_js(HIDE_LAST)
        st = settle(cdp, 1.0)
        p2 = st
        ok2 = (
            bool(hid2)
            and "err" not in str(hid2)
            and p2["stick"] == "true"
            and (p2["dist"] is None or p2["dist"] <= 5.0)
        )
        print(f"   2 shrink:  stick={p2['stick']} dist={p2['dist']} hist=[{p2['hist']}]")
        print(
            "   OK: round-end shrink kept the follow alive while watching"
            if ok2
            else "   REGRESSION: round-end shrink broke the follow (card-top wedge)"
        )

        # ── Scenario 3: released at the bottom re-arms on shrink ──────
        # The v0.5.38 regression guard: a stray up-tick at the bottom
        # releases the follow WITHOUT latching reading; the round-end
        # shrink must passively re-arm it. The v0.5.39 prev_dist<=24
        # tightening must not wedge this case (prev_dist~0 at the
        # bottom).
        cdp.eval_js(WHEEL.replace("__D__", "-120"))          # release at the very bottom
        st = settle(cdp, 0.6)
        hid3 = cdp.eval_js(HIDE_LAST)                        # shrink (new last card)
        st = settle(cdp, 1.0)
        p3 = st
        ok3 = (
            bool(hid3)
            and "err" not in str(hid3)
            and p3["stick"] == "true"
            and "rearm:passive-clamp" in p3["hist"]
        )
        print(
            f"   3 shrink(rel): stick={p3['stick']} dist={p3['dist']} "
            f"reading={p3['reading']} hist=[{p3['hist']}]"
        )
        print(
            "   OK: released-at-bottom re-armed on the round-end shrink (v0.5.38 guard)"
            if ok3
            else "   REGRESSION: round-end shrink did not re-arm a released follow at the bottom"
        )

        # ── Scenario 4: reader parked ~40 px up — device-verify only ───
        # Not assertable headlessly (see the module docstring).
        print("   4 parked@40: device-verify only")
        print("     on a device: scroll up ~2 lines, let a round end, then run")
        print("     __rushiPile(): expect stick=false, reading=false, prev_dist>24")
        print("     (a pre-fix build shows stick=true — the snap-back yank)")

        s.close()
        if ok1 and ok2 and ok3:
            print("   PASS: LIVE_EDGE_TIGHT gate behaves as designed (reachable cases)")
            return
        print(
            "FAIL: live-edge gate broken "
            f"(rearm@0 ok={ok1}, shrink-watch ok={ok2}, shrink-release ok={ok3})",
            file=sys.stderr,
        )
        sys.exit(1)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    main()
