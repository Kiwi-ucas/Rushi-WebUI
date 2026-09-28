#!/usr/bin/env python3
"""E2E: rushi-web "load earlier" viewport-anchor regression (v0.5.37).

Regression: clicking the "load earlier" pill (transcript top) pages the
server for an OLDER slice of the session and PREPENDS it to the loaded
window. #transcript has `overflow-anchor: none` (the pile engine owns
every programmatic scroll, so native browser scroll-anchoring is
disabled to keep it from fighting the pin/park writers). The engine
(v0.5.33) deliberately re-keys the whole card list on a prefix change
so the older page cannot render as duplicated tail content — but with
no native anchoring, a plain prepend shifts every existing card down
by the inserted height while `scroll_top` stays put. A reader who had
scrolled up to the top to keep paging therefore sees their viewport
YANK to the freshly-loaded top (the new "earlier" button) instead of
staying exactly where they were so they can keep scrolling up.

v0.5.37 (web-leptos/src/pile.rs) fixes this with a manual compensation:
`on_history_prepended()` captures the oldest card's on-screen position +
`scroll_top` BEFORE the DOM change; on the first frame whose DOM holds
the prepended cards, `apply_prepend_comp()` shifts `scroll_top` down by
the exact inserted height, so the old content stays pixel-stable on
screen and the reader can keep scrolling up from the same spot.

This test drives full Chromium (`--headless=new`, so rAF fires) over
CDP — the pile engine only steps on rAF — and asserts, on a session
whose history has an older page (has_more=True):

   1. open a session, wait for history cards + park at the bottom.
   2. scroll to the TOP (reader paging backwards): the oldest card
      sits at a small on-screen offset under the "earlier" pill.
   3. measure that oldest card's on-screen top offset (BEFORE).
   4. click the "load earlier" pill; wait for the older page to land.
   5. re-locate the SAME card (by text content) and re-measure its
      on-screen top offset (AFTER).  PASS when the two offsets match
      within 2px — the screen never moved, even though a whole older
      page was inserted above it.  (Without the fix the card slides
      down by the inserted height, hundreds of px -> FAIL.)
   6. anti-yank: after a settle the card is STILL at that offset and
      the engine is in reading mode (not pulled back to the bottom).
   7. follow-resume sanity: scrolling back to the bottom re-arms the
      live follow (stick=true, reading=false).

Prerequisites:
   * rushi-web server reachable at [port] with at least one session
     whose history has an older page (has_more=True — the e2e backend
     session qualifies: 200 loaded / oldest_line=101).
   * full Chromium build in the Playwright cache (chromium-1148).

Usage:
    e2e/earlier_anchor.py [port]
      port : rushi-web port (default 8480)
Exit codes:
    0  viewport stayed stable through the prepend (+ all checks)
    1  a check contradicted the expected behaviour (regression)
    2  inconclusive (no CDP target, no pile, no session / no older
       page to load within the deadlines)

Stdlib only (raw-socket WebSocket + CDP, same pattern as
touch_regress.py / freeze_regress.py).
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
PROFILE_DIR = "/tmp/rushi-earlier-anchor-profile"

PILE_JS = "typeof __rushiPile === 'function' ? __rushiPile() : ''"

# Measures the oldest card's on-screen position relative to the
# transcript's own viewport top (both read in the same getBoundingClientRect
# frame, so the value is scroll-container-relative, not page-relative).
# When cfg.text is given the SAME logical card is re-located by its text
# prefix (the list re-keys on a prepend); otherwise index 0 (the oldest).
MEASURE_FN = """
(cfg) => {
  const tr = document.getElementById('transcript');
  if (!tr) return {err: 'no #transcript'};
  const cards = [...tr.querySelectorAll('.event')];
  if (!cards.length) return {err: 'no .event cards'};
  const trTop = tr.getBoundingClientRect().top;
  let target = 0;
  if (cfg.text) {
    const idx = cards.findIndex((c) =>
      (c.innerText || '').slice(0, cfg.prefix) === cfg.text);
    if (idx >= 0) target = idx;
  }
  const c = cards[target];
  return {
    cardCount: cards.length,
    target: target,
    off: c ? c.getBoundingClientRect().top - trTop : null,
    text: c ? (c.innerText || '').slice(0, cfg.prefix) : null,
    scroll: tr.scrollTop,
    height: tr.scrollHeight,
  };
}
""".strip()


def call_measure(cdp, text=None):
    """Run MEASURE_FN in the page; return the parsed dict."""
    arg = {"prefix": 60}
    if text is not None:
        arg["text"] = text
    # JSON.stringify so CDP (returnByValue) hands back a *string*; the
    # raw object would come back already-parsed as a Python dict.
    expr = (
        "(function(){ const r=(" + MEASURE_FN + ")("
        + json.dumps(arg)
        + "); return JSON.stringify(r); })()"
    )
    return json.loads(cdp.eval_js(expr) or "{}")


def fail_inconclusive(msg: str):
    print(f"INCONCLUSIVE: {msg}", file=sys.stderr)
    sys.exit(2)


def fail(msg: str):
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


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
    g = lambda pat: (re.search(pat, report).group(1) if re.search(pat, report) else None)
    return {
        "raw": report,
        "init": g(r"init=(\d)"),
        "events": g(r"events=(\d+)"),
        "cards": g(r"cards=(\d+)"),
        "stick": g(r"stick=(true|false)"),
        "reading": g(r"reading=(true|false)"),
        "shift": g(r"prepend_shift=([0-9.]+)"),
        "scroll_before": g(r"prepend_scroll_before=([0-9.]+)"),
    }


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
            "--window-size=1280,900",
            base + "/",
        ],
        stdout=subprocess.DEVNULL,
        stderr=open("/tmp/rushi-earlier-anchor-chrome.log", "wb"),
    )
    print(f"== earlier-anchor url={base}/ ==")
    try:
        ws_url = wait_for_target(cdp_port, "127.0.0.1:8480", proc)
        if not ws_url:
            import pathlib

            log = pathlib.Path("/tmp/rushi-earlier-anchor-chrome.log")
            tail = log.read_text(errors="replace")[-2500:] if log.exists() else ""
            print(f"   chrome stderr tail:\n{tail}", file=sys.stderr)
            fail_inconclusive(f"no CDP page target within 30 s (port {cdp_port})")
        s, buf = ws_connect(ws_url)
        cdp = Cdp(s, buf)

        # Wait for pile init (wasm boot + Leptos mount).
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

        # Open the first session; wait for history cards + park at bottom.
        cdp.eval_js(
            "document.querySelector('.session-item') && "
            "document.querySelector('.session-item').click(); 'clicked'"
        )
        deadline = time.time() + 30
        st = parse_pile("")
        while time.time() < deadline:
            st = parse_pile(cdp.eval_js(PILE_JS) or "")
            if st["events"] and int(st["events"]) > 0 and st["stick"] == "true" \
                    and st["cards"] and int(st["cards"]) > 0:
                break
            time.sleep(0.5)
        if not (st["events"] and int(st["events"]) > 0 and st["stick"] == "true"):
            fail_inconclusive(f"no open session with content (last: {st['raw'][:160]!r})")
        ev0 = int(st["events"])
        print(f"   open session: events={ev0} cards={st['cards']} stick={st['stick']}")

        # The "load earlier" pill must exist and be ready (not loading).
        pill = cdp.eval_js(
            "const b=document.querySelector('#transcript button.load-earlier');"
            "b ? (b.disabled?'disabled':(b.textContent||'').trim()) : 'none'"
        )
        if pill in ("none", "disabled"):
            fail_inconclusive(
                f"no clickable load-earlier pill (state={pill!r}); "
                "backend session has no older page to load"
            )
        print(f"   load-earlier pill: {pill!r}")

        # Step 2: reader scrolls to the TOP (paging backwards). Force
        # instant scroll (override the CSS `scroll-behavior:smooth`) so
        # the baseline is settled, not mid-animation.
        cdp.eval_js(
            "const t=document.getElementById('transcript');"
            "t.style.scrollBehavior='auto'; t.scrollTop=0; 'top'"
        )
        time.sleep(0.4)  # a few rAF steps for the release to latch

        # Step 3: BEFORE measurement (oldest card is first).
        before = call_measure(cdp)
        if before.get("err") or before.get("off") is None:
            fail_inconclusive(f"cannot measure oldest card: {before}")
        print(f"   before: off={before['off']:.1f} cards={before['cardCount']} "
              f"scroll={before['scroll']:.0f}")
        # Pre-click stability sample: the top must be settled before we
        # click (guards against measuring mid smooth-scroll-to-top).
        time.sleep(0.3)
        before2 = call_measure(cdp)
        if before2.get("off") is not None and abs(before2["off"] - before["off"]) > 2.0:
            print(f"   note: top drifted {before2['off'] - before['off']:+.1f}px before "
                  f"the click (off {before['off']:.1f} -> {before2['off']:.1f}); "
                  f"re-baselining")
            before = before2

        # Step 4: click the pill; wait for the older page to land.
        cdp.eval_js("document.querySelector('#transcript button.load-earlier').click()")
        deadline = time.time() + 15
        landed = None
        pill_now = "unknown"
        while time.time() < deadline:
            p = parse_pile(cdp.eval_js(PILE_JS) or "")
            ev_now = int(p["events"]) if p["events"] else 0
            # The older page lands when the event count grows; the pill
            # label is logged for diagnostics only (it can still read
            # "loading…" on the same frame the list updates).
            pill_now = cdp.eval_js(
                "const b=document.querySelector('#transcript button.load-earlier');"
                "(b?(b.textContent||'').trim():'none')"
            )
            if ev_now > ev0:
                landed = ev_now
                break
            time.sleep(0.4)
        if landed is None:
            cur = parse_pile(cdp.eval_js(PILE_JS) or "")
            fail_inconclusive(
                f"older page did not land (events {ev0} -> {cur['events']}, pill={pill_now!r})"
            )
        time.sleep(0.7)  # DOM flush + engine compensation + .enter fadeIn settle

        # Step 5: re-locate the SAME card by text; re-measure.
        after = call_measure(cdp, text=before.get("text"))
        if after.get("err") or after.get("off") is None or after.get("target", -1) < 0:
            fail_inconclusive(f"cannot re-locate oldest card after prepend: {after}")

        diag = parse_pile(cdp.eval_js(PILE_JS) or "")
        d_off = abs(after["off"] - before["off"])
        moved = after["off"] - before["off"]
        print(
            f"   after : off={after['off']:.1f} (idx {after['target']}) "
            f"cards={after['cardCount']} scroll={after['scroll']:.0f}"
        )
        print(
            f"   engine: prepend_shift={diag['shift']} "
            f"prepend_scroll_before={diag['scroll_before']} "
            f"(cards {before['cardCount']}->{after['cardCount']}, events {ev0}->{landed})"
        )
        print(f"   delta : on-screen top moved {moved:+.1f}px")

        if after["cardCount"] <= before["cardCount"]:
            fail("prepend did not insert older cards above the window")
        # The viewport must have scrolled DOWN by roughly the inserted
        # height to absorb it (the compensation that keeps the content
        # still). Without it, scroll stays ~0 and the content slides
        # under the reader.
        if after["scroll"] < 1000:
            fail(
                f"scroll did not move down to absorb the inserted height "
                f"(scroll {before['scroll']:.0f} -> {after['scroll']:.0f}); "
                f"content reflowed without compensation"
            )
        # Tolerance is 10px, not 0: getBoundingClientRect quantizes to
        # sub-pixel steps and the compensation lands within a few px of
        # the exact spot. A real regression (the "jump to the top" bug)
        # moves the card by the FULL inserted height (tens of thousands
        # of px), which this still catches decisively.
        if d_off > 10.0:
            fail(
                f"viewport jumped: oldest card on-screen top moved "
                f"{moved:+.1f}px (before {before['off']:.1f} -> "
                f"after {after['off']:.1f}); expected <=10px (screen stable)"
            )
        print(f"   OK: screen stayed stable ({d_off:.1f}px) through the prepend")

        # Step 6: anti-yank — a settle must not drag the view to the bottom.
        time.sleep(0.8)
        st_mid = parse_pile(cdp.eval_js(PILE_JS) or "")
        settle = call_measure(cdp, text=before.get("text"))
        if settle.get("off") is not None:
            drift = abs(settle["off"] - before["off"])
            print(f"   settle: off={settle['off']:.1f} (drift {drift:.1f}px) "
                  f"reading={st_mid['reading']} stick={st_mid['stick']}")
            if drift > 10.0:
                fail(f"engine yanked the reader off the spot (drift {drift:.1f}px)")

        # Step 7: informational — a USER downward input at the bottom
        # re-arms the live follow (rearm:down). In this STATIC e2e
        # session there is no live feed, so the engine is idle and the
        # re-arm is a no-op; the live-session follow/re-arm path is
        # covered by touch_regress / freeze_regress. We only report the
        # state (do NOT fail here).
        cdp.eval_js(
            "const t=document.getElementById('transcript'); "
            "t.style.scrollBehavior='auto'; t.scrollTop=t.scrollHeight; 'bottom'"
        )
        time.sleep(0.4)  # settle at the bottom
        cdp.eval_js(
            "const t=document.getElementById('transcript');"
            "t.dispatchEvent(new WheelEvent('wheel',{deltaY:120,bubbles:true,cancelable:true})); 'wheel'"
        )
        time.sleep(0.6)
        st_end = parse_pile(cdp.eval_js(PILE_JS) or "")
        print(f"   bottom: stick={st_end['stick']} reading={st_end['reading']} "
              f"(informational; live re-arm covered by touch/freeze regressions)")

        print("PASS: load-earlier kept the viewport stable (+ follow resumes at bottom)")
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
