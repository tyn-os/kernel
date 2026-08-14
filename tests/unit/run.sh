#!/bin/bash
# Layer-1 host unit tests for the kernel's pure cores (cpio parser, RTC decode, …).
# The repo-root .cargo/config forces a bare-metal target + build-std on the whole
# tree; tests/unit/.cargo/config neutralizes build-std, and this runner targets the
# host (computed, not hardcoded) so prebuilt std + the test harness are available.
set -u
cd "$(dirname "$0")" || exit 1
HOST=$(rustc -vV | sed -n 's/^host: //p')
exec cargo test --target "$HOST" "$@"
