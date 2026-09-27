#!/usr/bin/env bash
#
# build-rootfs.sh — assemble the base OTP+Elixir VFS cpio (src/otp-rootfs.cpio).
#
# The base cpio ships the OTP kernel/stdlib beams, the Elixir standard library,
# and the demo app's dependency beams. This script REFRESHES the two components
# that must be derived from source/toolchain (and which previously went stale
# because they were produced by hand):
#
#   1. tyn_boot.beam   — ALWAYS recompiled from src/erl/tyn_boot.erl. It went
#                        stale once (a shipped beam older than its source), which
#                        silently disabled runtime.exs evaluation and the boot
#                        resolver fix. Deriving it here makes that impossible.
#   2. Elixir OTP-app .app resource files (elixir/logger/eex/iex/mix) — copied
#                        from the pinned Elixir toolchain. Without elixir.app the
#                        application controller can't resolve `elixir` as a
#                        started app, and Ecto's Repo init fails with
#                        "module Code is not available" (Wall A).
#
# It seeds from the existing committed base cpio (SEED) and rewrites those two
# components, so the accreted, proven dependency set (Phoenix/Bandit/telemetry/
# thousand_island/…) is preserved byte-for-byte. Fully regenerating that
# dependency set from a mix build is a separate, larger task; see
# docs/BUILDING_ERTS.md §3.
#
# Requirements: the pinned toolchain (see .tool-versions) — erlc (OTP 27) and
# elixir (1.18.3-otp-27) on PATH — plus cpio. Run from anywhere.
#
#   ./build-rootfs.sh                 # rewrite src/otp-rootfs.cpio in place
#   OUT=/tmp/x.cpio ./build-rootfs.sh # write elsewhere
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SEED="${SEED:-$ROOT/src/otp-rootfs.cpio}"
OUT="${OUT:-$ROOT/src/otp-rootfs.cpio}"
ERLC="${ERLC:-erlc}"
ELIXIR="${ELIXIR:-elixir}"
BOOT_SRC="$ROOT/src/erl/tyn_boot.erl"
STATUS_SRC="$ROOT/src/erl/tyn_status.erl"   # standardized /status listener (operability)
ELIXIR_APPS="elixir logger eex iex mix"   # the OTP-app resource files Wall A needs

say() { printf '\033[1;36m[build-rootfs]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[build-rootfs] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

command -v "$ERLC"   >/dev/null 2>&1 || die "erlc not on PATH (need OTP 27 toolchain; see .tool-versions)"
command -v "$ELIXIR" >/dev/null 2>&1 || die "elixir not on PATH (need Elixir 1.18.3-otp-27; see .tool-versions)"
command -v cpio      >/dev/null 2>&1 || die "cpio not on PATH"
[ -f "$SEED" ]      || die "seed cpio not found: $SEED"
[ -f "$BOOT_SRC" ]  || die "tyn_boot source not found: $BOOT_SRC"
[ -f "$STATUS_SRC" ] || die "tyn_status source not found: $STATUS_SRC"

# Locate the Elixir lib dir (parent of elixir/logger/eex/iex/mix ebin dirs).
ELIXIR_LIB="${ELIXIR_LIB:-$("$ELIXIR" -e 'IO.puts(Path.expand(Path.join(to_string(:code.lib_dir(:elixir)), "..")))' 2>/dev/null)}"
[ -n "$ELIXIR_LIB" ] && [ -d "$ELIXIR_LIB" ] || die "could not locate Elixir lib dir (set ELIXIR_LIB=)"
say "Elixir lib dir: $ELIXIR_LIB"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

say "Unpacking seed base cpio ($(basename "$SEED"))"
( cd "$STAGE" && cpio -idm --quiet < "$SEED" )

# 1. tyn_boot.beam — always recompile from source, into the flat root (where the
#    code_server fallback loads it) and verify the result is the current logic.
say "Compiling tyn_boot.erl -> tyn_boot.beam"
"$ERLC" +deterministic -o "$STAGE" "$BOOT_SRC"   # +deterministic: no paths/timestamps in the beam
strings "$STAGE/tyn_boot.beam" 2>/dev/null | grep -q 'eval_runtime_config' \
  || die "compiled tyn_boot.beam lacks eval_runtime_config — source/toolchain mismatch"

# 1b. tyn_status.beam — the standardized /status listener (operability). Always
#     recompiled from source, same as tyn_boot, so it can't go stale.
say "Compiling tyn_status.erl -> tyn_status.beam"
"$ERLC" +deterministic -o "$STAGE" "$STATUS_SRC"
strings "$STAGE/tyn_status.beam" 2>/dev/null | grep -q 'snapshot_json' \
  || die "compiled tyn_status.beam lacks snapshot_json — source/toolchain mismatch"

# 2. Elixir OTP-app .app resource files — copy from the toolchain into the root.
for a in $ELIXIR_APPS; do
  src="$ELIXIR_LIB/$a/ebin/$a.app"
  [ -f "$src" ] || die "missing $a.app in toolchain at $src"
  cp "$src" "$STAGE/$a.app"
  say "added $a.app"
done

# Repack (same layout the original used: flat + nested, paths without leading ./).
# Deterministic: zero all mtimes and emit members in a stable (sorted) order, so
# the cpio is byte-for-byte reproducible and its md5 is a meaningful fingerprint
# of the inputs — the whole point of deriving these artifacts from source.
say "Repacking -> $OUT (deterministic)"
find "$STAGE" -exec touch -h -d @0 {} + 2>/dev/null || true
( cd "$STAGE" && find . -type f | LC_ALL=C sort | sed 's|^\./||' \
    | cpio -o -H newc --reproducible --quiet ) > "$OUT"

# Byte-check the built artifact: the .app files and a fresh tyn_boot.beam must be
# present in the OUTPUT, not merely have been staged. List once to a file and
# grep the file — grepping a pipe with `grep -q` under `pipefail` spuriously
# fails, because -q closes the pipe early and cpio dies on SIGPIPE.
cpio -t < "$OUT" 2>/dev/null > "$STAGE/.listing"
have() { grep -qxF "$1" "$STAGE/.listing"; }
for a in $ELIXIR_APPS; do
  have "$a.app" || die "$a.app missing from built cpio"
done
have "tyn_boot.beam" || die "tyn_boot.beam missing from built cpio"
have "tyn_status.beam" || die "tyn_status.beam missing from built cpio"

sz=$(wc -c < "$OUT" | tr -d ' ')
if command -v md5sum >/dev/null 2>&1; then md5=$(md5sum "$OUT" | awk '{print $1}');
elif command -v md5 >/dev/null 2>&1;   then md5=$(md5 -q "$OUT");
else md5="(no md5 tool)"; fi
say "DONE  $OUT  size=$sz  md5=$md5"
say "Elixir .app files present: $ELIXIR_APPS ; tyn_boot.beam freshly compiled."
