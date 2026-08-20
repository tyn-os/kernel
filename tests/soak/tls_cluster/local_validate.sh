#!/bin/bash
# FREE harness-mechanics gate for the soak (run on the build host BEFORE any
# Nitro spend — validate the EXACT driver + app + bounded-eval logic against a
# real Tyn node, cheaply, single-node.
#
# What this DOES validate (free, single-node): the TLS/HTTP boundary serves, the
# /diag endpoint parses, soak.py drives sustained load + scrapes + emits a
# bounded-quantities VERDICT, and the node-restart probe has teeth (health
# recovers, measured). want_peers=0 so the dist-specific assertions are skipped.
#
# What this does NOT validate: two-node clustering, the network-blip probe, and
# long-run kvmclock drift — those need real inter-guest networking + hours and
# are authoritative only on the staged Nitro run (nitro_soak.sh). See README.
#
# Mirrors tests/resiliency/tcg_flood_check.sh (boot -> wait -> drive). Uses KVM
# if available, else TCG (mechanics only). One variable at a time; leak-proof.
set -u
KERNEL="${KERNEL:-/home/ubuntu/kernel/target/x86_64-tyn/release/tyn-kernel}"
BASE="${BASE:-/home/ubuntu/kernel/src/otp-rootfs.cpio}"
REL="${REL:-/home/ubuntu/soak_app/_build/prod/rel/soak_app}"   # built per README
WORK="${WORK:-/home/ubuntu/work}"
HERE="$(cd "$(dirname "$0")" && pwd)"
DUR="${DUR:-180}"                 # short mechanics run
ACCEL="${ACCEL:-kvm}"             # kvm on the build host; tcg is mechanics-only
LOG=$WORK/soak_local.log; CPIO=$WORK/soak_local.cpio; RAW=$WORK/soak_local.raw
mkdir -p "$WORK"; rm -f "$LOG"

# self-safe: this process's cmdline is "bash <path>", never contains qemu-system.
pkill -9 -f qemu-system-x86_64 2>/dev/null; sleep 2

# TLS and PROBE are OPT-IN (default off) for the free TCG gate:
#   * TLS=1   also injects a self-signed cert so the rustls HTTPS boundary comes
#     up. Under TCG this depends on emulated RDSEED + the tyn_tls NIF and is
#     fragile; the TLS boundary is Nitro-authoritative, so the default HTTP-only
#     run cleanly isolates the *harness* mechanics from TLS-NIF availability.
#   * PROBE=1 fires a mid-run node-restart. Under TCG boot is slow (tens of s),
#     so recovery may exceed the budget — a TCG artifact, not a harness bug. The
#     restart teeth are exercised on Nitro (fast boot); default off here.
TLS="${TLS:-0}"; PROBE="${PROBE:-0}"
PACK_ENV=""
if [ "$TLS" = 1 ]; then
  CERT=$WORK/soak_cert.pem; KEY=$WORK/soak_key.pem
  [ -f "$CERT" ] || openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
    -nodes -keyout "$KEY" -out "$CERT" -days 2 -subj "/CN=soak.local" >/dev/null 2>&1
  PACK_ENV="--env TYN_TLS_CERT_B64=$(base64 -w0 "$CERT") --env TYN_TLS_KEY_B64=$(base64 -w0 "$KEY")"
fi

/home/ubuntu/kernel/tyn-pack "$REL" --base "$BASE" -o "$CPIO" $PACK_ENV >/dev/null 2>&1 \
  || { echo "FAIL pack"; exit 1; }
KERNEL=$KERNEL bash "$WORK/mkdisk.sh" "$CPIO" "$RAW" >/dev/null 2>&1 || { echo "FAIL mkdisk"; exit 1; }

ACCEL_ARGS="-accel $ACCEL"; [ "$ACCEL" = kvm ] && ACCEL_ARGS="-accel kvm -cpu host -smp 2"
[ "$ACCEL" = tcg ] && ACCEL_ARGS="-accel tcg -cpu max"
boot_qemu() {
  timeout $((DUR + 180)) qemu-system-x86_64 $ACCEL_ARGS -m 2560M -machine q35 \
    -drive file="$RAW",format=raw,if=ide -no-reboot -nographic -serial "file:$1" \
    -device virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off \
    -netdev user,id=n0,hostfwd=tcp::5680-:8080,hostfwd=tcp::5643-:8443 </dev/null >/dev/null 2>&1 &
}
boot_qemu "$LOG"; QP=$!

up=0
for i in $(seq 1 200); do
  curl -s --max-time 3 http://localhost:5680/health 2>/dev/null | grep -q L2_OK && { up=1; echo "node up ${i}s"; break; }
  kill -0 $QP 2>/dev/null || { echo "qemu died — see $LOG"; break; }
  sleep 1
done
[ "$up" != 1 ] && { echo "FAIL: node never served /health"; kill -9 $QP 2>/dev/null; exit 1; }

# confirm /diag parses before committing to the run.
echo "== /diag sanity =="; curl -s --max-time 4 http://localhost:5680/diag | head -c 400; echo

NODEURL="http://localhost:5680"
if [ "$TLS" = 1 ] && curl -sk --max-time 4 https://localhost:5643/health | grep -q L2_OK; then
  echo "HTTPS boundary up — driving TLS"; NODEURL="https://localhost:5643"
fi

PROBE_ARGS=""
if [ "$PROBE" = 1 ]; then
  RESTART="kill -9 $QP; sleep 3; $(declare -f boot_qemu); boot_qemu $LOG.2"
  PROBE_ARGS="--probe at=$((DUR/2)):$RESTART"
fi

echo "== soak.py mechanics run (${DUR}s, TLS=$TLS PROBE=$PROBE) =="
python3 "$HERE/soak.py" --nodes "$NODEURL" --duration-s "$DUR" --scrape-s 10 --rps 8 \
  --https-insecure --max-drift-ms 5000 $PROBE_ARGS --out "$WORK/soak_local.jsonl"
RC=$?

pkill -9 -f qemu-system-x86_64 2>/dev/null
echo "== local_validate exit=$RC (0=harness verdict PASS; mechanics gate, not the measurement) =="
exit $RC
