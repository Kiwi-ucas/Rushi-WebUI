#!/usr/bin/env python3
"""E2E: rushi-web mobile sticky-bottom regression test (CDP runner).

Covers the v0.5.24 / v0.5.24.1 / v0.5.24.2 fixes to record_touch_input
and the flat-step sticky gate (web-leptos/src/pile.rs):

  * v0.5.24: the per-event 0.5 px touch dead zone was replaced by
    cumulative-drift hysteresis — direction latches after ~8 px of
    travel (TOUCH_INTENT_DRIFT_PX) regardless of event cadence.
  * v0.5.24.1: gesture boundaries come from real touchstart /
    touchend / touchcancel events, so a hesitant slow pull
    (micro-pauses mid-gesture) keeps its accumulated drift and still
    latches. While a finger is on the transcript (touch_active) the
    tight pin / settle pull / park are suspended so the native scroll
    is never fought.
  * v0.5.24.2: "reading mode" is the POSITIONAL reading latch (latched
    only when the user is actually far above the bottom, d > 80;
    self-cleared at the live edge) — NOT the input-event input_up
    latch. The v0.5.24.1 !input_up re-arm gates wedged the follow off
    after a round end whenever a stray up-tick at the bottom had
    latched input_up ("card top, no follow" regression); the gates
    now key on reading alone.

This test drives full Chromium in `--headless=new` mode through CDP.
Unlike the headless shell used by freeze_regress.py, --headless=new
fires requestAnimationFrame, which the pile engine needs: every
intent latch is consumed by sync_stick inside an rAF step. The test
dispatches full synthetic gestures (touchstart -> N touchmoves ->
touchend) on #transcript and reads the __rushiPile() console
diagnostic for the stick flag, input_up / reading / touch_active, and
the transition history.

Scenarios:
  0. open a session, wait for history cards + park at the bottom
     (stick=true)
  1. steady slow pull up   -> stick=false  (hist: release:up-input)
  2. steady slow pull down -> stick=true   (hist: rearm:down)
  3. HESITANT pull up (3 x 4 px, 400 ms pauses) from the stuck
     state -> stick=false (release:up-input) — the real-phone
     regression: under the v0.5.24 time-gap heuristic the pauses
     reset the drift anchor and a gentle pull could never latch.
  4. final pull down -> stick=true (hist: rearm:down)
  5. round-end shrink — the v0.5.24.2 regression guard: a stray
     up-tick at the bottom releases the follow WITHOUT latching
     reading (the bottom band, d <= 80); hiding the last card
     (content shrink — the round-end finalize shape) passively clamps
     the viewport and the passive-clamp re-arm must fire
     (stick=true, rearm:passive-clamp). Under v0.5.24.1 the
     !input_up gate blocked it, wedging the follow off and
     stranding the viewport at the card top.

Note: the live round-boundary re-arms (stream start / round end,
tier 1) need live events and are not exercised here; verify them on a
real session.

Prerequisites:
  * rushi-web server reachable at [port] with at least one session
    that has history events (the test opens the first sidebar entry).
  * full Chromium build in the Playwright cache (chromium-1148),
    NOT the headless_shell (no rAF there).

Usage:
    e2e/touch_regress.py [port]
      port : rushi-web port (default 8480)
Exit codes:
    0  all scenarios behave as expected
    1  a scenario contradicted the expected behaviour (regression)
    2  inconclusive (no CDP target, pile never initialized, or no
       session with content within the deadlines)

Stdlib only (raw-socket WebSocket, same pattern as truncation_ws.py
and freeze_regress.py).
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
PROFILE_DIR = "/tmp/rushi-touch-regress-profile"

PILE_JS = "typeof __rushiPile === 'function' ? __rushiPile() : ''"

# Full-gesture driver. cfg = {dir: -1 up / +1 down, segs, per, step,
# step_ms, pause_ms}. Dispatches a REAL gesture shape: one touchstart
# (the drift anchor), `segs` segments of `per` touchmoves of `step` px
# (with optional `pause_ms` hesitations BETWEEN segments), then
# touchend. The hesitant variant (pause_ms=400 > old 250 ms gap
# heuristic) is the regression case: under v0.5.24's time-gap
# re-anchor it could never accumulate the 8 px drift and release.
PULL_JS = """
(async (cfg) => {
  const el = document.getElementById('transcript');
  if (!el) return JSON.stringify({err: 'no #transcript'});
  const r = el.getBoundingClientRect();
  const cx = r.left + r.width / 2;
  let y = r.top + r.height * 0.7;
  const mk = (yy) => new Touch({identifier: 1, target: el, clientX: cx, clientY: yy});
  const ev = (type, touches, changed) => el.dispatchEvent(new TouchEvent(type, {
    touches, changedTouches: changed, bubbles: true, cancelable: true,
  }));
  const wait = (ms) => new Promise((res) => setTimeout(res, ms));
  ev('touchstart', [mk(y)], [mk(y)]);          // anchor at touchdown
  for (let s = 0; s < cfg.segs; s++) {
    for (let i = 1; i <= cfg.per; i++) {
      y += cfg.dir * cfg.step;
      ev('touchmove', [mk(y)], [mk(y)]);
      await wait(cfg.step_ms);
    }
    if (s < cfg.segs - 1 && cfg.pause_ms > 0) await wait(cfg.pause_ms);
  }
  ev('touchend', [], [mk(y)]);                 // gesture ends
  return JSON.stringify({done: true, total: cfg.segs * cfg.per * cfg.step});
})
""".strip()


def fail_inconclusive(msg: str):
    print(f"INCONCLUSIVE: {msg}", file=sys.stderr)
    sys.exit(2)


# ── raw-socket WebSocket (pattern: e2e/freeze_regress.py) ────────────
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


# ── CDP over the raw socket ──────────────────────────────────────────
class Cdp:
    def __init__(self, s, buf):
        self.s = s
        self.buf = buf
        self.next_id = 1

    def eval_js(self, expr: str, await_promise: bool = False):
        """Runtime.evaluate; returns the returnByValue value (or None)."""
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
            with urllib.request.urlopen(
                f"http://{HOST}:{port}/json", timeout=3
            ) as r:
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
    iu = re.search(r"input_up=(true|false)", report)
    rd = re.search(r"reading=(true|false)", report)
    ta = re.search(r"touch_active=(true|false)", report)
    return {
        "raw": report,
        "stick": stick.group(1) if stick else None,
        "hist": hist.group(1) if hist else "",
        "input_up": iu.group(1) if iu else None,
        "reading": rd.group(1) if rd else None,
        "touch_active": ta.group(1) if ta else None,
    }


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8480
    base = f"http://{HOST}:{port}"
    chrome = os.environ.get("RUSHI_CHROMIUM", DEFAULT_CHROME)
    if not os.path.isfile(chrome):
        fail_inconclusive(f"chromium not found at {chrome}")
    if urllib.request.urlopen(f"{base}/", timeout=5).status != 200:
        fail_inconclusive(f"server not reachable at {base}")

    # Free debug port: bind-0 trick. The probe socket MUST be closed
    # before chrome binds, or chrome's port bind collides and it falls
    # back to [::1] only (IPv4 /json polls then time out).
    probe = socket.socket()
    probe.bind((HOST, 0))
    cdp_port = probe.getsockname()[1]
    probe.close()
    proc = subprocess.Popen(
        [
            chrome,
            "--headless=new",  # fires rAF; the old headless shell does not
            "--no-sandbox",
            "--disable-gpu",
            f"--remote-debugging-port={cdp_port}",
            f"--user-data-dir={PROFILE_DIR}",
            "--window-size=1280,800",
            base + "/",
        ],
        stdout=subprocess.DEVNULL,
        stderr=open("/tmp/rushi-touch-regress-chrome.log", "wb"),
    )
    print(f"== touch regress url={base}/ (expect release + re-arm) ==")
    try:
        ws_url = wait_for_target(cdp_port, "127.0.0.1:8480", proc)
        if not ws_url:
            import pathlib

            log = pathlib.Path("/tmp/rushi-touch-regress-chrome.log")
            tail = log.read_text(errors="replace")[-2500:] if log.exists() else ""
            print(f"   chrome stderr tail:\n{tail}", file=sys.stderr)
            fail_inconclusive(f"no CDP page target within 30 s (port {cdp_port})")
        s, buf = ws_connect(ws_url)
        cdp = Cdp(s, buf)

        # Wait for pile init (wasm boot + Leptos mount + #transcript).
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

        # Open a session with content: step_full early-returns (and resets
        # all input state) while events.is_empty(), so the sticky logic only
        # runs for an open session. The sidebar's first item may be an
        # empty/new session, so click items until one loads history and the
        # viewport parks at the bottom (stick=true).
        n_items = cdp.eval_js(
            "document.querySelectorAll('.session-item').length"
        )
        n_items = int(n_items) if n_items and str(n_items).isdigit() else 1
        opened = False
        for i in range(n_items):
            cdp.eval_js(
                f"document.querySelectorAll('.session-item')[{i}].click(); 'ok'"
            )
            deadline = time.time() + 8
            st = parse_pile("")
            m_ev = m_cr = m_lr = None
            while time.time() < deadline:
                st = parse_pile(cdp.eval_js(PILE_JS) or "")
                m_ev = re.search(r"events=(\d+)", st["raw"])
                m_cr = re.search(r"cards=(\d+)", st["raw"])
                m_lr = re.search(r"last_range=(\d+)", st["raw"])
                if (
                    st["stick"] == "true"
                    and m_ev
                    and m_cr
                    and m_lr
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
            fail_inconclusive(
                "no open session with content + stick=true "
                f"(last: {st['raw'][:200]})"
            )
        print(
            f"   session open: stick={st['stick']} "
            f"events={m_ev.group(1)} cards={m_cr.group(1)}"
        )
        time.sleep(1.0)  # let history-load parking settle

        # Scenario 1: STEADY slow pull up — the v0.5.24 baseline. Per-
        # event displacement under the pre-fix dead zone, cumulative
        # over the post-fix drift threshold.
        print("   pull up   (steady, 25 x 0.4 px) ...")
        cdp.eval_js(
            f"{PULL_JS}({{dir:-1, segs:1, per:25, step:0.4, step_ms:10, pause_ms:0}})",
            await_promise=True,
        )
        time.sleep(0.7)  # let the rAF step run sync_stick
        st = parse_pile(cdp.eval_js(PILE_JS) or "")
        print(f"   after up:   stick={st['stick']} input_up={st['input_up']} hist=[{st['hist']}]")
        ok1 = st["stick"] == "false" and "release:up-input" in st["hist"]
        print(
            "   OK: steady pull-up released the follow"
            if ok1
            else "   REGRESSION: steady pull-up did not release the follow"
        )

        # Scenario 2: SLOW pull down -> re-arm (the viewport ends
        # within 80 px of the bottom, so down_intent re-arms).
        time.sleep(0.4)
        print("   pull down (steady, 25 x 0.4 px) ...")
        cdp.eval_js(
            f"{PULL_JS}({{dir:1, segs:1, per:25, step:0.4, step_ms:10, pause_ms:0}})",
            await_promise=True,
        )
        time.sleep(0.7)
        st = parse_pile(cdp.eval_js(PILE_JS) or "")
        print(f"   after down: stick={st['stick']} input_up={st['input_up']} hist=[{st['hist']}]")
        ok2 = st["stick"] == "true" and "rearm:down" in st["hist"]
        print(
            "   OK: pull-down re-armed the follow"
            if ok2
            else "   REGRESSION: pull-down did not re-arm the follow"
        )

        # Scenario 3: HESITANT pull up from the STUCK state — the
        # real-phone regression. Three 4 px segments with 400 ms
        # hesitations (each > the v0.5.24 250 ms gap heuristic, which
        # reset the drift anchor mid-pull so a gentle pull never
        # reached the 8 px latch). The v0.5.24.1 touchstart anchor
        # survives the pauses, so the cumulative 12 px must release.
        time.sleep(0.4)
        print("   pull up   (hesitant, 3 x 4 px with 400 ms pauses) ...")
        cdp.eval_js(
            f"{PULL_JS}({{dir:-1, segs:3, per:10, step:0.4, step_ms:10, pause_ms:400}})",
            await_promise=True,
        )
        time.sleep(0.7)
        st = parse_pile(cdp.eval_js(PILE_JS) or "")
        print(f"   after hesitant up: stick={st['stick']} input_up={st['input_up']} hist=[{st['hist']}]")
        ok3 = st["stick"] == "false" and "release:up-input" in st["hist"]
        print(
            "   OK: hesitant pull-up released the follow"
            if ok3
            else "   REGRESSION: hesitant pull-up did not release the follow"
        )

        # Scenario 4: final pull down — clean state + re-arm check.
        time.sleep(0.4)
        print("   pull down (steady, 25 x 0.4 px) ...")
        cdp.eval_js(
            f"{PULL_JS}({{dir:1, segs:1, per:25, step:0.4, step_ms:10, pause_ms:0}})",
            await_promise=True,
        )
        time.sleep(0.7)
        st = parse_pile(cdp.eval_js(PILE_JS) or "")
        print(f"   after final down: stick={st['stick']} input_up={st['input_up']} hist=[{st['hist']}]")
        ok4 = st["stick"] == "true" and "rearm:down" in st["hist"]
        print(
            "   OK: final pull-down re-armed the follow"
            if ok4
            else "   REGRESSION: final pull-down did not re-arm the follow"
        )

        # Scenario 5: round-end shrink — the reported regression
        # ("streaming card finishes, viewport ends at the card top,
        # follow dies"). Sequence:
        #  (a) a stray up-tick at the bottom releases the follow —
        #      input_up latches, but reading must NOT latch (the
        #      viewport is at the bottom, d <= 80 — v0.5.36);
        #  (b) let the 300 ms input window lapse;
        #  (c) simulate the round-end finalize: hide the last card,
        #      the content shrinks, the browser passively clamps the
        #      viewport (a scroll-top drop with no user input). The
        #      passive-clamp re-arm must fire (v0.5.16). Under
        #      v0.5.24.1 the !input_up gate blocked it, wedging the
        #      follow off and stranding the viewport at the card top.
        time.sleep(0.4)
        print("   stray up-tick at bottom (one wheel tick) ...")
        cdp.eval_js(
            "(function(){var el=document.getElementById('transcript');"
            "el.dispatchEvent(new WheelEvent('wheel',"
            "{deltaY:-120,bubbles:true,cancelable:true}));"
            "return 'ticked';})()",
        )
        time.sleep(0.4)
        st = parse_pile(cdp.eval_js(PILE_JS) or "")
        print(
            f"   after tick: stick={st['stick']} input_up={st['input_up']} "
            f"reading={st['reading']} hist=[{st['hist']}]"
        )
        ok5a = st["stick"] == "false" and st["reading"] == "false"
        print(
            "   OK: stray tick released the follow WITHOUT entering reading mode"
            if ok5a
            else "   REGRESSION: stray tick at the bottom latched reading mode"
        )
        time.sleep(0.5)  # lapse the 300 ms input window
        print("   round-end shrink (hide last card) ...")
        cdp.eval_js(
            "(function(){var el=document.getElementById('transcript');"
            "var cards=el.querySelectorAll('.event');"
            "var last=cards[cards.length-1];"
            "if(!last) return JSON.stringify({err:'no cards'});"
            "last.style.display='none';"
            "return JSON.stringify({hidden:true,n:cards.length});})()"
        )
        time.sleep(0.8)  # clamp + step + settle pull
        st = parse_pile(cdp.eval_js(PILE_JS) or "")
        print(
            f"   after shrink: stick={st['stick']} reading={st['reading']} "
            f"hist=[{st['hist']}]"
        )
        ok5 = st["stick"] == "true" and "rearm:passive-clamp" in st["hist"]
        print(
            "   OK: passive-clamp re-armed the follow at the live edge"
            if ok5
            else "   REGRESSION: follow died after the round-end shrink (card-top wedge)"
        )

        s.close()
        if ok1 and ok2 and ok3 and ok4 and ok5a and ok5:
            print("   PASS: all scenarios behave as expected")
            return
        print(
            "FAIL: sticky-bottom release/re-arm broken "
            f"(steady-up ok={ok1}, down ok={ok2}, hesitant-up ok={ok3}, "
            f"final-down ok={ok4}, tick-no-reading ok={ok5a}, shrink-rearm ok={ok5})",
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
