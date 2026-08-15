#!/bin/bash
# Phase-2 Layer-2 network adversarial: slow-loris + fd/socket-exhaustion against
# a running Tyn (Bandit :8080). Runs from an attacker host that can reach the
# target. Three-part per attack (behaviour, not status):
#   TEETH     the attack genuinely reaches/stresses the server (enough conns)
#   (1)+(2)   slow-loris: legit request still served DURING (not starved/crashed)
#   (3)       recovery: legit request works AFTER the attack stops
# fd-exhaustion "during" is MEASURED not asserted — a saturated server may
# legitimately refuse (clean limiting); the hard asserts are teeth + recovery.
# The no-crash / no-corruption side is read from the target's SERIAL by the
# caller (TCG log, or Nitro console) — not observable from the attacker host.
#
# Usage: drive_net_attacks.sh <target-ip> [port]
# Env:   SL_N (slow-loris conns, default 200), FD_N (fd conns, default 3000),
#        SL_HOLD/FD_HOLD hold seconds. Use small SL_N/FD_N for TCG mechanics.
set -u
T="${1:?usage: drive_net_attacks.sh <ip> [port]}"; P="${2:-8080}"
HERE="$(cd "$(dirname "$0")" && pwd)"
SL_N="${SL_N:-200}"; FD_N="${FD_N:-3000}"; SL_HOLD="${SL_HOLD:-40}"; FD_HOLD="${FD_HOLD:-20}"
B="http://$T:$P"
hc() { curl -s --max-time 8 "$B/health" 2>/dev/null; }
fail=0
say() { if [ "$2" = "$3" ]; then echo "  PASS  $1"; else echo "  FAIL  $1 (want '$3' got '$2')"; fail=1; fi; }
teeth() { if [ "${2:-0}" -ge "$3" ] 2>/dev/null; then echo "  PASS  TEETH $1 (=$2 >= $3)"; else echo "  FAIL  TEETH $1 (=$2 < $3)"; fail=1; fi; }
# Poll a file for a pattern up to N seconds — the connect loop takes variable
# time (SLIRP slow, or thousands of conns), so wait for the OPENED marker rather
# than a fixed sleep that races it.
wait_line() { for _ in $(seq 1 "$3"); do grep -qE "$2" "$1" 2>/dev/null && return 0; sleep 1; done; return 1; }

echo "=== baseline /health ==="
base=$(hc); echo "  -> [$base]"; say "baseline serves L2_OK" "$base" "L2_OK"

echo "=== ATTACK 1: slow-loris ($SL_N partial conns, hold ${SL_HOLD}s) ==="
: > /tmp/l2_sl.out
python3 "$HERE/slowloris.py" "$T" "$P" "$SL_N" "$SL_HOLD" > /tmp/l2_sl.out 2>&1 &
SLPID=$!
wait_line /tmp/l2_sl.out "SLOWLORIS_OPENED" 90 || echo "  (warn: no SLOWLORIS_OPENED within 90s)"
opened=$(grep -oE "SLOWLORIS_OPENED [0-9]+" /tmp/l2_sl.out | head -1 | awk '{print $2}')
during=$(hc); echo "  opened=$opened ; /health DURING -> [$during]"
teeth "slow conns established" "$opened" "$(( SL_N/4 ))"
say "(1) legit request served DURING slow-loris (not starved)" "$during" "L2_OK"
wait $SLPID 2>/dev/null; sleep 3
after=$(hc); echo "  /health AFTER -> [$after]"
say "(3) recovery after slow-loris" "$after" "L2_OK"

echo "=== ATTACK 2: fd/socket exhaustion ($FD_N abandoned conns, hold ${FD_HOLD}s) ==="
: > /tmp/l2_fd.out
python3 "$HERE/fd_exhaust.py" "$T" "$P" "$FD_N" "$FD_HOLD" > /tmp/l2_fd.out 2>&1 &
FDPID=$!
wait_line /tmp/l2_fd.out "FDEXH_OPENED" 120 || echo "  (warn: no FDEXH_OPENED within 120s)"
opened2=$(grep -oE "FDEXH_OPENED [0-9]+" /tmp/l2_fd.out | head -1 | awk '{print $2}')
refused2=$(grep -oE "REFUSED [0-9]+" /tmp/l2_fd.out | head -1 | awk '{print $2}')
during2=$(hc); echo "  opened=$opened2 refused=$refused2 ; /health DURING -> [${during2:-<none>}] (measured, not asserted)"
teeth "conns pushed at socket layer" "$opened2" "$(( FD_N/10 ))"
wait $FDPID 2>/dev/null; sleep 5
after2=$(hc); echo "  /health AFTER -> [$after2]"
say "(3) recovery after fd-exhaustion" "$after2" "L2_OK"

echo "  ---- slow-loris opened=$opened ; fd opened=$opened2 refused=$refused2 ----"
if [ "$fail" = 0 ]; then echo "NET_ATTACKS: PASS (attacker-side: teeth + serve-during + recovery)"; exit 0; fi
echo "NET_ATTACKS: FAIL"; exit 1
