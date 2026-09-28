#!/usr/bin/env python3
"""E2E: rushi-web "load earlier" flicker regression (v0.5.38).

Companion to earlier_anchor.py (which pins the viewport position). This
pins the VISUAL behaviour: a load-earlier prepend re-keys the whole card
list (the v0.5.33 ev_gen bump), so every EXISTING card re-mounts. Each
re-mounted card would replay its `.event.enter` fade-in (opacity 0->1,
0.15s), which reads as the whole transcript flashing before the older
cards appear — the "all cards flicker once" report.

v0.5.38 (web-leptos/src/pile.rs + style.css) suppresses that: while
#transcript carries `.prepend-quiet` (set in pile::on_history_prepended
for the re-mount frame) the rule
    #transcript.prepend-quiet .event.enter { animation: none; opacity:1 }
makes re-mounted cards appear instantly; the class + `.enter` are
stripped ~400ms later so the fade can't replay on class removal.

This test drives full Chromium (`--headless=new`) over CDP and asserts,
on a session with an older page (has_more=True):

   1. open a session, scroll to the TOP (reader paging backwards).
   2. capture the oldest card's 60-char text prefix (it is re-located by
      text after the prepend, since the list re-keys).
   3. click the "load earlier" pill.
   4. in-page: wait for the older page to land (card count grows), then
      densely sample that SAME card's computed `opacity` over ~500ms at
      20ms cadence right from the flush.
      PASS when the minimum sampled opacity stays >= 0.85 — the card
      never faded in, i.e. no flicker. (Without the fix the re-mounted
      card starts at opacity 0 and climbs to 1 over 0.15s, so the
      minimum dips to ~0.)
   5. also record whether `.prepend-quiet` was held on #transcript in
      the settle window (the mechanism that suppresses the fade).

Prerequisites: same as earlier_anchor.py (an e2e session whose history
has an older page, full Chromium in the Playwright cache).

Usage:
     e2e/earlier_flicker.py [port]
Exit codes:
     0  no flicker (pre-existing card never faded through the prepend)
     1  a check contradicted the expected behaviour (flicker present)
     2  inconclusive (no CDP target / no pile / no session / no older
        page to load within the deadlines)

Stdlib only (raw-socket WebSocket + CDP, same pattern as
earlier_anchor.py / touch_regress.py / freeze_regress.py).
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
PROFILE_DIR = "/tmp/rushi-earlier-flicker-profile"

PILE_JS = "typeof __rushiPile === 'function' ? __rushiPile() : ''"

# Densely sample the computed opacity of the card whose innerText starts
# with `prefix`. First waits in-page for the prepend to land (card count
# grows past `minCards`), THEN densely samples for `ms` ms at `step` ms
# cadence and reports the minimum opacity. The in-page wait makes the
# dense window start right at the flush, so a .enter fade replay (the
# flicker) — which dips opacity to ~0 and climbs to 1 over 0.15s — is
# reliably captured; a suppressed mount holds at 1. `find` re-finds the
# card by text on every tick, tracking it across the re-mount (its index
# jumps up, but its text prefix is unchanged).
OPACITY_SAMPLE_FN = """
(async (cfg) => {
  const tr = document.getElementById('transcript');
  if (!tr) return JSON.stringify({err: 'no #transcript'});
  const cards = () => [...tr.querySelectorAll('.event')];
  const find = () => cards().find(
    (c) => (c.innerText || '').slice(0, cfg.prefixLen) === cfg.prefix);
  // Wait for the prepend to land: card count grows past cfg.minCards.
  const w0 = performance.now();
  let landed = false;
  while (performance.now() - w0 < cfg.waitMs) {
    if (cards().length >= cfg.minCards) { landed = true; break; }
    await new Promise((r) => setTimeout(r, 15));
  }
  // From the flush onward, densely sample the card's opacity.
  let minOp = Infinity, samples = 0, quietSeen = false;
  const s0 = performance.now();
  await new Promise((res) => {
    function tick() {
      const c = find();
      if (c) {
        const o = parseFloat(getComputedStyle(c).opacity);
        if (Number.isFinite(o) && o < minOp) minOp = o;
        samples++;
      }
      if (tr.classList.contains('prepend-quiet')) quietSeen = true;
      if (performance.now() - s0 >= cfg.ms) res();
      else setTimeout(tick, cfg.step);
    }
    tick();
  });
  return JSON.stringify({
    landed,
    minOp: Number.isFinite(minOp) ? minOp : null,
    samples,
    quietSeen,
  });
})
""".strip()


def sample_opacity(cdp, prefix, min_cards, wait_ms=8000, ms=500, step=20):
    """Run OPACITY_SAMPLE_FN in the page; return the parsed dict."""
    cfg = {
        "prefix": prefix,
        "prefixLen": len(prefix),
        "minCards": min_cards,
        "waitMs": wait_ms,
        "ms": ms,
        "step": step,
    }
    expr = (
        "(function(){ return (" + OPACITY_SAMPLE_FN + ")("
        + json.dumps(cfg)
        + "); })()"
    )
    v = cdp.eval_js(expr, await_promise=True)
    return json.loads(v or "{}")


# --- oldest-card measurement (same helper as earlier_anchor.py) --------
MEASURE_FN = """
(cfg) => {
  const tr = document.getElementById('transcript');
  if (!tr) return {err: 'no #transcript'};
  const cards = [...tr.querySelectorAll('.event')];
  if (!cards.length) return {err: 'no .event cards'};
  const c = cards[cfg.target];
  return {
    cardCount: cards.length,
    off: c ? c.getBoundingClientRect().top - tr.getBoundingClientRect().top : null,
    text: c ? (c.innerText || '').slice(0, cfg.prefix) : null,
  };
}
""".strip()


def call_measure(cdp, target=0):
    arg = {"target": target, "prefix": 60}
    expr = (
        "(function(){ const r=(" + MEASURE_FN + ")("
        + json.dumps(arg)
        + "); return JSON.stringify(r); })()"
    )
    return json.loads(cdp.eval_js(expr) or "{}")


def fail_inconclusive(msg):
    print(f"INCONCLUSIVE: {msg}", file=sys.stderr)
    sys.exit(2)


def fail(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


# ── raw-socket WebSocket (pattern: e2e/earlier_anchor.py) ─────────────
def ws_connect(ws_url):
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


def ws_send_text(s, text):
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

    def eval_js(self, expr, await_promise=False):
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


def wait_for_target(port, url_frag, proc, timeout_s=30.0):
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


def parse_pile(report):
    g = lambda pat: (re.search(pat, report).group(1) if re.search(pat, report) else None)
    return {
        "raw": report,
        "events": g(r"events=(\d+)"),
        "cards": g(r"cards=(\d+)"),
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
        stderr=open("/tmp/rushi-earlier-flicker-chrome.log", "wb"),
    )
    print(f"== earlier-flicker url={base}/ ==")
    try:
        ws_url = wait_for_target(cdp_port, "127.0.0.1:8480", proc)
        if not ws_url:
            import pathlib

            log = pathlib.Path("/tmp/rushi-earlier-flicker-chrome.log")
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

        # The session list reorders as the live loop updates, so the
        # FIRST item is not guaranteed to be the busy e2e session.
        # Walk the items (cap 6) until one yields history content.
        n_items = cdp.eval_js(
            "[...document.querySelectorAll('.session-item')].length"
        )
        n_items = int(n_items) if n_items else 0
        st = parse_pile("")
        opened = False
        for idx in range(min(n_items, 6)):
            cdp.eval_js(
                f"document.querySelectorAll('.session-item')[{idx}].click(); 'clicked'"
            )
            deadline = time.time() + 8
            while time.time() < deadline:
                st = parse_pile(cdp.eval_js(PILE_JS) or "")
                if st["events"] and int(st["events"]) > 0 and st["cards"] and int(st["cards"]) > 0:
                    opened = True
                    break
                time.sleep(0.5)
            if opened:
                break
        if not opened:
            fail_inconclusive(f"no session with content among {n_items} items (last: {st['raw'][:160]!r})")
        ev0 = int(st["events"])
        print(f"   open session: events={ev0} cards={st['cards']}")

        pill = cdp.eval_js(
            "const b=document.querySelector('#transcript button.load-earlier');"
            "b ? (b.disabled?'disabled':(b.textContent||'').trim()) : 'none'"
        )
        if pill in ("none", "disabled"):
            fail_inconclusive(f"no clickable load-earlier pill (state={pill!r})")
        print(f"   load-earlier pill: {pill!r}")

        # Reader scrolls to the TOP (paging backwards); settle.
        cdp.eval_js(
            "const t=document.getElementById('transcript');"
            "t.style.scrollBehavior='auto'; t.scrollTop=0; 'top'"
        )
        time.sleep(0.4)

        before = call_measure(cdp, target=0)
        if before.get("err") or not before.get("text"):
            fail_inconclusive(f"cannot capture oldest card: {before}")
        prefix = before["text"]
        min_cards = int(before["cardCount"]) + 1
        print(f"   before: cards={before['cardCount']} prefix={prefix[:30]!r}...")

        # Click, then run the in-page sampler (it waits for the landing
        # itself, then densely samples the pre-existing card's opacity).
        cdp.eval_js("document.querySelector('#transcript button.load-earlier').click()")
        op = sample_opacity(cdp, prefix, min_cards)
        if op.get("err"):
            fail_inconclusive(f"opacity sampler failed: {op}")
        if not op.get("landed"):
            fail_inconclusive(
                f"older page did not land within {8}s (cards {before['cardCount']} -> "
                f"{call_measure(cdp, 0).get('cardCount')}); no older page to load"
            )
        print(f"   older page landed; opacity sampler: samples={op['samples']} "
              f"prepend-quiet seen={op['quietSeen']}")

        after = call_measure(cdp, target=0)
        if int(after.get("cardCount", 0)) <= min_cards - 1:
            fail("prepend did not insert older cards above the window")

        if not op["quietSeen"]:
            fail(
                "prepend-quiet class was never held during the settle window; "
                "the fade suppression did not engage (flicker risk)"
            )
        if op["minOp"] is None:
            fail_inconclusive("opacity sampler saw no matching card (cannot judge)")
        if op["minOp"] < 0.85:
            fail(
                f"pre-existing card faded through the prepend "
                f"(min opacity {op['minOp']:.2f} < 0.85) — the flicker is "
                "present (fade replayed on re-mount)"
            )
        print(
            f"   OK: pre-existing card held opacity >= {op['minOp']:.2f} "
            f"(no fade replay) through the prepend — no flicker"
        )
        print("PASS: load-earlier prepend is flicker-free (cards do not re-fade)")
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
