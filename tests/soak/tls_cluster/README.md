# Sustained TLS + cluster soak

> Lives in `tests/soak/tls_cluster/`. Sibling `tests/soak/nitro_soak.sh` is a
> *different* soak — the BUG-1 SMP-regression + drift guard — don't conflate them.

The integrated durability test: TLS at the client boundary + real inter-node
distribution traffic + sustained load, over **hours**, asserting that measured
quantities stay **bounded** — not just "still up." It targets the bug class no
point-in-time test catches: slow accretion and drift under sustained integrated
load. (Direction: `directions/SUSTAINED_TLS_CLUSTER_SOAK.md`.)

## What's here

| File | Role |
| --- | --- |
| `soak_app/` | The workload (Elixir): `/health` (recovery control), `/diag` (the bounded-quantities instrument), `/work` + `/connect`, an inter-node `DistWorker` that keeps the cluster link *used*, and a config-only in-guest TLS boundary. |
| `soak.py` | The driver: sustained load, `/diag` scrape, kvmclock-drift computation, restart/blip probes **with teeth** (fires them, then measures recovery), and the trailing-vs-leading **bounded-quantities verdict**. Stdlib only. |
| `local_validate.sh` | FREE single-node mechanics gate on the build host — run before any spend. |
| `nitro_soak.sh` | STAGED 2-node Nitro run (leak-proof + deadman). Run only in a watched window. |

## What is measured (bounded over the run — content, not status)

- **TLS session heap accretion** — `mem.binary` (rustls/`:ssl` session state + large
  terms land in the BEAM binary heap). The load-bearing leak; paired with kernel
  `heap_free` from the `[diag]` serial trace (`src/net/mod.rs`, VERBOSE-gated) so we
  see *which* heap moves.
- **fd/socket/process drift** — `process_count`, `port_count`, `atom_count`, `ets`.
- **Distribution stability** — roundtrips climb, **mismatches == 0** (verified byte-exact
  with `=:=`, never `erlang:md5` — it's flaky on large binaries here, see
  `docs/DIST_ACCEPT_HUNT.md`), peers stay put, no spurious `nodedown`.
- **kvmclock long-run drift** — node `os:system_time` vs host UTC at each scrape (the
  deferred hours-long measurement; immediate drift was already proven zero).
- **Latency creep** and **zero steady-state request errors** (outside probe windows).

## Free-validation scope (the honest split)

Two-node clustering was proven on **Nitro** (real ENA, `docs/DIST_ACCEPT_HUNT.md`),
**never locally** — the local dist work used a tap + host-driver on a *single* node,
because two QEMU guests clustering needs inter-guest L2 + DHCP-on-a-bridge that isn't
a solved path in this repo. So:

- **`local_validate.sh` (free, single node)** validates the *harness mechanics*: the
  TLS/HTTP boundary serves, `/diag` parses, `soak.py` drives + scrapes + emits a
  bounded verdict, and the **node-restart probe has teeth** (health recovers, measured).
  `want_peers=0`, so dist-specific assertions are skipped. This de-risks the harness
  before spending.
- **`nitro_soak.sh` (paid, watched, 2 nodes)** is authoritative for the parts that need
  real inter-guest networking + hours: **two-node clustering, the network-blip probe,
  and long-run kvmclock/heap drift.**

Running local 2-node via a `tap`+bridge+`dnsmasq` DHCP is possible as a stretch, but
it's unproven here; don't let it gate the free mechanics validation.

## Assemble the workload

The app is drop-in lib files (same pattern as `examples/tls`). On the build host:

1. `mix new soak_app --sup`; add `{:bandit, "~> 1.0"}` (pulls `plug`, `thousand_island`).
2. Copy `soak_app/*.ex` into `lib/soak_app/`, and the TLS transport files from
   `examples/tls/` (`tyn_tls.ex`, `rustls_tls.ex`) into `lib/soak_app/`.
3. Point `application.ex`'s `mod:` at `SoakApp.Application`; `MIX_ENV=prod mix release`.
4. Pack into the **A′-capable beam base** (the outbound-TLS / clean-cpio base from
   `examples/tls/aprime_pack.sh`, since the workload does TLS both directions), injecting
   the cert: `tyn-pack ... --env TYN_TLS_CERT_B64=$(base64 -w0 cert.pem) --env TYN_TLS_KEY_B64=...`.

The node must boot **distributed with an IP-literal name** (`n@<priv-ip>`, fixed cookie)
per the proven dist-spike recipe (`docs/DIST_ACCEPT_HUNT.md`) — no in-guest DNS. The
`/connect` route is the host-driven formation hook the orchestrator calls once both IPs
are known.

## Run

```bash
# 1. FREE mechanics gate (build host) — MUST pass before any spend:
DUR=180 tests/soak/tls_cluster/local_validate.sh      # exit 0 = harness verdict PASS

# 2. STAGED Nitro run — watched window only:
SG_ID=sg-xxxx IMAGE=/dev/shm/soak-disk.raw DUR=9000 tests/soak/tls_cluster/nitro_soak.sh
```

## Nitro run result (2026-08-20, single-node, `nitro_soak_1node.sh`)

First paid run was **single-node** (2-node dist-boot isn't wired yet — see below). 2h on
c5.xlarge driving in-guest HTTPS: **verdict PASS** — 65,726 req / **0 err** / 0 mismatch /
0 UNHANDLED; **binary heap dead flat ~1975 KiB** (zero TLS session-heap accretion);
**kvmclock drift flat, max 50ms over 2h** (TCG was ~0.85 ms/s — the instrument's teeth make
the flat-on-Nitro reading meaningful); proc/port/atom/ets bounded. $0 leaked.

**Caveat — restart-recovery teeth were INERT.** The probe used `aws ec2 reboot-instances`
(ACPI soft reboot), which **Tyn ignores** (no ACPI-reboot handler → no-op; verified: one
boot sequence, continuous err=0). soak.py's `recovered:true` was a false pass. Real
recovery teeth need a **hard cycle** (stop/start or terminate+relaunch) **+ IP-rediscovery**
(stop/start reassigns the public IP, breaking the fixed `--nodes` URL). TODO for the next
window. New finding: *Tyn ignores ACPI soft-reboot — an EC2 "reboot" won't cycle a node.*

**Two-node still deferred:** dist-boot is not wired into the shippable path (no
`-name`/`epmd_module` in the kernel argv; `soak_app` ships no `epmd_module`) — the spike
used uncommitted scaffolding. Wiring it (dynamic `-name n@<dhcp-ip>` or runtime
`net_kernel:start/2` self-naming + `epmd_module` + cookie via boot.config) is its own effort.

## Scope guard (wedge)

This test is wedge-agnostic and worth building under any direction. But **acting on its
findings splits**: (a) *harden what exists* (survive restart/blip/reconnect on the
current static-mapped path) is defensible under any wedge — let the probe results scope
it. (b) *turnkey discovery* (in-guest DNS / `.internal` so clustering is drop-in) is a
**conscious wedge bet**, not something to accrete into off the back of a green soak.
Findings inform that call; they don't pre-commit it.

## No commits unless asked

The harness commits as a scheduled-tier test once proven. Any real bug found → the
standard playbook (confirm deterministic → pin → fix → measured acceptance), its own
direction if it's real work.
