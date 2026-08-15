#!/usr/bin/env python3
# fd/socket exhaustion: open N connections and ABANDON them (connect, send
# nothing, hold). Drives the server's per-connection socket/fd allocation toward
# its bound — the end-to-end analogue of the Layer-1 fd-table exhaustion path.
# A clean server bounds this (refuses/queues new connects with a normal error),
# never an OOB/crash. Refusals are the *expected* clean-limiting signal, counted.
# Args: <target> <port> <n> <hold_seconds>
import socket, time, sys

target, port, n, hold = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
socks, refused, timedout, errs = [], 0, 0, 0
for _ in range(n):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(2)
        s.connect((target, port))
        socks.append(s)  # connected + abandoned (no bytes sent)
    except ConnectionRefusedError:
        refused += 1     # clean limiting: the server/kernel bounded us
    except socket.timeout:
        timedout += 1
    except Exception:
        errs += 1
print("FDEXH_OPENED %d REFUSED %d TIMEOUT %d ERR %d" % (len(socks), refused, timedout, errs), flush=True)

time.sleep(hold)  # hold them open so the server stays saturated during the driver's probe

for s in socks:
    try: s.close()
    except Exception: pass
print("FDEXH_CLOSED", flush=True)
