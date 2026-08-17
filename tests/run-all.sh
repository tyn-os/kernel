#!/usr/bin/env bash
# Phase-2 standing test harness (Layer 5) — the single entry point.
#
# Runs the FAST / every-build tier now (host `cargo test`: Layer-1 unit cores +
# Layer-3 host fuzz — no qemu, no cloud) and gates on it (exit 1 on any failure).
# The SCHEDULED tier (a running instance; Nitro for networking/SMP) is listed at
# the end — it takes minutes–hours and is not run here. See
# docs/PHASE2_TESTING_STATUS.md for the full layer status + named gaps.
set -uo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== FAST TIER (every-build, host cargo test) ==="
echo "--- Layer 1 unit cores + Layer 3 host fuzz (tests/unit) ---"
if ( cd "$DIR/unit" && ./run.sh ); then
  echo "FAST TIER: PASS"
else
  echo "FAST TIER: FAIL — gating." >&2
  exit 1
fi

cat <<'EOF'

=== SCHEDULED TIER (not run here — needs a running instance; Nitro for net/SMP) ===
Layer 2 — resiliency (BUG-8 regression + net-adversarial):
  tests/resiliency/tcg_flood_check.sh        # TCG mechanics gate (free)
  tests/resiliency/nitro_flood_repro.sh      # Nitro: conn-flood panic+recovery acceptance
  tests/tmpfs/drive_cap_concurrency.sh       # tmpfs cap under concurrent writers (TCG)
Layer 4 — soak (BUG-1 SMP guard 4a + drift 4b), scheduled/long:
  KERNEL=~/work/tyn-kernel-poison tests/soak/nitro_soak.sh   # teeth: must show large_md5>0
  tests/soak/nitro_soak.sh                                    # fixed: TYN_AMP_RUNTIME_MS=hours, large_md5==0

Deferred-with-reason (see docs/PHASE2_TESTING_STATUS.md):
  - in-situ memory-arg syscall fuzz: BLOCKED by the identity-map hazard (guard-pages)
  - Layer-2 malformed-HTTP/multipart breadth: optional (high-value find already banked)
EOF
