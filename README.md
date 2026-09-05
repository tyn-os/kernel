# Tyn

A minimal Rust microkernel purpose-built for the BEAM.

No Linux. No POSIX. Your Erlang/Elixir code on bare metal.

> **Recent:** the BEAM now runs confined in ring 3 (isolated from the kernel); in-guest TLS works both directions; deploying your own app is one command (`tyn deploy`). See [CHANGELOG.md](CHANGELOG.md).

## What is Tyn?

Tyn is a unikernel: a single-purpose OS kernel that hosts exactly one thing — the BEAM virtual machine. It replaces the entire Linux stack with ~8,000 lines of Rust, and runs on KVM/QEMU and on **real AWS Nitro EC2**, driving the network with a from-scratch ENA driver and serving HTTP directly from the kernel.

The BEAM already has its own scheduler, process model, memory management, and distribution. A general-purpose kernel underneath duplicates most of that. Tyn removes the redundancy and gives the BEAM a host built for it and nothing else.

It runs the **real, unmodified ERTS** — not a reimplementation. A new OTP release should just work. That's the deliberate bet: reimplementing the BEAM (as [LING](https://github.com/cloudozer/ling) did on Xen) means owning a moving target forever — the SMP scheduler, the JIT, distribution. Hosting upstream ERTS keeps all of that upstream.

## Why

- **Security** — a general-purpose kernel carries drivers and subsystems a cloud BEAM workload never touches. Tyn includes only what the BEAM needs, so the trusted computing base is a few thousand lines of Rust. And it's now *enforced*, not just small: the BEAM runs in ring 3, isolated from the kernel by a hardware boundary, so a memory-safety bug in the BEAM or a NIF can't reach the kernel. See [What works](#what-works).
- **Simplicity** — a Tyn image is BEAM bytecode plus the Rust kernel. No OS services, no package manager, no user accounts. The application and its runtime, nothing else.
- **Density** — images are megabytes, not gigabytes. More nodes per host, lower cost.
- **Verifiability** — one runtime and a small TCB, structured so formal verification is tractable.

## Status

A stock `mix phx.new` Phoenix app — static assets, LiveView, sessions, outbound HTTP — runs unmodified on OTP 27 BEAM on real AWS Nitro. It serves HTTP under concurrency with byte-exact assets, over a from-scratch ENA driver and network stack — no Linux, no host networking. The BEAM runs confined in ring 3 (see [What works](#what-works)).

| | |
| --- | --- |
| Image size | ~45 MB (ERTS + OTP/Elixir rootfs + kernel) |
| Boot to serving HTTP | ~5 s (kernel → BEAM handoff ~430 ms; the rest is OTP startup + JIT codegen) |
| Boot reliability | clean, repeated launches on Nitro |

The serving path is `ENA hardware → admin queue → I/O queues → smoltcp → DHCP → gen_tcp → Bandit → Phoenix`, entirely inside the Rust kernel — the kernel talks to the NIC's descriptor rings directly.

> Throughput figures are omitted here pending a faithful Nitro benchmark. Early numbers were taken under QEMU/SLIRP, whose host networking distorted them; single-connection bulk HTTP/HTTPS on Nitro is ~13 MB/s, but a full benchmark will be published when it's run.

## Try it

**Run the demo** — a public AMI in `us-east-1`, no build required:

```bash
aws ec2 run-instances --image-id ami-0c13cb4a868a6e441 \
    --instance-type c5.large --region us-east-1
# open TCP 8080 in your security group; the instance takes ~1–2 min to launch
# (Tyn itself boots in ~5 s — the rest is EC2 provisioning), then:
curl http://<public-ip>:8080/                           # → Phoenix landing page
curl -s http://<public-ip>:8080/assets/app.js | wc -c    # → a static asset via kernel sendfile
# open http://<public-ip>:8080/counter in a browser — the LiveView counter increments live
```

**Deploy your own app** — one command takes a Mix release to a running instance:

```bash
tyn deploy --env DATABASE_URL=... my_app/    # release → AMI → running Nitro instance
```

It packs the release, builds and imports the image, registers the AMI (reused on unchanged redeploys), launches, and prints the IP and how to reach it. The full walkthrough — IAM policy, security groups, the IAM-gated serial console, config/secrets — is in [`docs/DEPLOY.md`](docs/DEPLOY.md). Instances accrue hourly charges; terminate when done.

## What works

- **Full Phoenix stack, stock app** — a `mix phx.new` app runs unmodified: static assets (`Plug.Static` / `send_file` over kernel `sendfile(2)`, no dependency patch), interactive LiveView (WebSocket mount + live updates), `runtime.exs`, signed cookies / CSRF / `Phoenix.Token`, and outbound TCP/UDP + DNS. Clean-clone validated byte-exact on Nitro; codified in [`tests/`](tests/).
- **BEAM confined in ring 3** — the BEAM runs as ring-3 userland with the kernel in ring 0 behind a hardware boundary: US/NX page permissions, SMEP/SMAP, and a syscall path that bounds-checks every user pointer. A memory-safety bug in the BEAM or a NIF faults and is contained; the kernel keeps serving. Proven by violation — a deliberate BEAM write to a kernel address is denied and contained, and the confused-deputy case (handing the kernel a bad pointer) is rejected — on 16-vCPU Nitro. This confines the kernel from the BEAM; the JIT region is necessarily user-RWX, so it doesn't internally harden the BEAM, and side-channel (Meltdown/Spectre) isolation is future work.
- **One-command deploy** — `tyn deploy` takes a Mix release to a running Nitro instance: packs it, builds/imports/registers the AMI (content-hashed, so unchanged redeploys skip the ~10-min import), launches, injects config/secrets via `--env`, and reports the IP. `tyn iam-policy` prints the required IAM, and a preflight fails early on missing permissions. A bash wrapper over `tyn-pack` + the AWS CLI.
- **8-way SMP** — ACPI/MADT CPU discovery, APIC timer calibration, AP trampoline (16→64-bit), per-CPU GDT/TSS/IST, per-CPU GS_BASE syscall data, IPI wakeup, preemptive user-mode scheduling.
- **BeamAsm JIT** — OTP 27 built `--enable-jit`. The timer preempts inside mmap'd JIT pages; `erlang:system_info(emu_flavor)` returns `jit`.
- **TCP/UDP networking** — `gen_tcp` / `gen_udp` end-to-end: POSIX socket layer → smoltcp → virtio-net (QEMU) or the from-scratch ENA driver (Nitro). On Nitro the address comes from DHCP, with lease renewal.
- **In-guest TLS, inbound *and* outbound** — HTTPS both terminates *and* originates inside the guest, with no plaintext hop to an edge terminator. *Inbound:* the `tyn_tls` rustls NIF behind a `ThousandIsland` transport, wired in config-only (no Bandit or app changes). *Outbound:* OTP's own `:ssl` does verified client TLS — `:httpc` / Finch / Postgrex just work — via Tyn's RustCrypto NIF (ECDHE + ECDSA/RSA-PSS/Ed25519 `verify`) and a pure-Erlang asn1 shim. Both proven on Nitro: TLS 1.3, cert-verified, byte-exact; `verify` passes a 22/22 adversarial suite. See [`docs/IN_GUEST_TLS.md`](docs/IN_GUEST_TLS.md). **Unreviewed; TLS 1.2/mTLS not yet done — see [Limitations](#limitations).**
- **`:crypto`** — a from-scratch Rust NIF (RustCrypto primitives) fed by a kernel CSPRNG (RDSEED → ChaCha20), statically linked into ERTS. Passes known-answer vectors and matches upstream OTP byte-for-byte. **Unreviewed — see [Limitations](#limitations).**
- **Distributed Erlang (basic, two-node)** — `net_kernel` distribution works node-to-node on Nitro: mutual `connect_node`, `rpc`, large-term transfer byte-exact, tick/`nodedown`. Needs static host mapping — no in-guest `.internal` DNS — so it's a fixed pair, not turnkey multi-node. Single-node is the default.
- **AWS Nitro deployment** — boots from a GRUB/multiboot disk image imported as an EBS snapshot. The ENA NIC (`1d0f:ec20`) is found via port-IO PCI config, since Nitro publishes no MCFG/ECAM.
- **Live eval shell** — over the AWS serial console (IAM-gated, no open port) or TCP. Evaluate Erlang or Elixir against the running BEAM.
- **Elixir 1.18.3** on OTP 27.
- **~50 Linux syscalls** — `mmap`, `read`, `write`, `open`, `stat`, `pipe`, `ppoll`, `futex`, `clone`, `epoll`, `readv`, `writev`, `sendfile`, `getrandom`, …
- **VFS** — read-only cpio (newc) holding the OTP + Elixir `.beam` files; application images are packed by `tyn-pack`.
- **Boot** — Multiboot1, ELF loader for static musl binaries, a 2 MiB/4 KiB page hierarchy with guard pages under kernel stacks.

```
>> erlang:system_info(emu_flavor).
jit
>> 'Elixir.System':version().
<<"1.18.3">>
```

## Limitations

Tyn is a specialized runtime, and these are first-class, not footnotes. Two kinds: **by design** — deliberate consequences of hosting one workload on KVM/Nitro — and **rough edges** — real gaps being closed. Read both before deploying.

**By design**

- **Writable storage is in-memory only.** `/tmp` and `/dev/shm` are a volatile tmpfs (4 MiB cap, lost on reboot), so `Plug.Upload` and scratch writes work within that budget. The application VFS is a read-only cpio; there is no persistent disk.
- **IPv4 only.** IPv6 socket binds are rewritten to IPv4-any at boot (stock Phoenix `runtime.exs` binds IPv6-any).
- **Runs on KVM or Nitro, not QEMU-TCG.** Under software emulation (`-accel tcg`) some images deterministically `#PF` at boot. Real hardware (Nitro, or KVM with `-enable-kvm`) is unaffected and is the standard of evidence.
- **Wall clock is kvmclock-backed on Nitro/KVM, RTC-seeded elsewhere.** On Nitro/KVM, `CLOCK_REALTIME` uses the paravirtual clock — nanosecond resolution, host-drift-corrected. Where kvmclock isn't exposed it falls back to an RTC-seeded, TSC-extrapolated clock: real UTC but second-resolution and drifts over long uptime. Monotonic time is exact.

**Working on it**

- **Crypto and TLS are unreviewed.** The `:crypto` and TLS NIFs (RustCrypto primitives, kernel-CSPRNG-fed) pass known-answer vectors and match upstream OTP byte-for-byte, and `verify` — the silent-MITM keystone — is adversarially tested, but the surface has had **no outside security review**. Don't rely on it for production security until it has (RNG-review first). TLS 1.2 and mTLS aren't done. Boot panics without a hardware RNG (RDRAND/RDSEED; present on c5/m5/t3 Nitro).
- **Clustering is not turnkey.** Two-node distribution is validated but needs static host mapping (no in-guest `.internal` DNS). Fine for a fixed pair; not drop-in multi-node discovery.
- **Confinement is against memory-safety faults, not side channels.** Ring-3 isolation protects the kernel from a BEAM memory bug (proven), but doesn't yet defend Meltdown/Spectre-class side channels — that needs separate page tables (KPTI), which is future work. The JIT region is user-RWX by necessity, so the BEAM isn't internally W^X-hardened.
- **No built-in observability yet.** Logs and BEAM stats are reachable over the eval shell / serial console, but there's no logs-off-the-box or metrics endpoint yet — operating a deployed instance is hands-on. Deployment (`tyn deploy`) is done; lifecycle/observability tooling is next.
- **LiveView on a bare IP needs `check_origin`.** Phoenix returns `403` on the LiveView WebSocket when the served host doesn't match the configured URL host. `check_origin: false` is fine for a throwaway IP demo, but for production set the real host list — `false` is a cross-site WebSocket-hijacking hole on a real deployment.

## Architecture

```
┌─────────────────────────────────────────┐
│  Applications (Elixir / Erlang)         │
├─────────────────────────────────────────┤
│  OTP / Supervision Trees                │
├─────────────────────────────────────────┤
│  ERTS / BEAM VM (unmodified · SMP · JIT)│   ring 3
├─────────────────────────────────────────┤
│  BEAM Host Interface (Rust)             │   ← syscall boundary
│  ~50 Linux syscalls emulated            │     (pointer-checked)
├─────────────────────────────────────────┤
│  Tyn Kernel (Rust · ~8,000 LOC)         │   ring 0
│  SMP · Memory · Networking · VFS · I/O  │
├─────────────────────────────────────────┤
│  KVM / QEMU / AWS Nitro                 │
└─────────────────────────────────────────┘
```

ERTS is built from unmodified OTP 27 source — no patches, no special defines — via the pinned, reproducible build in [`beam-build/`](beam-build/) (Alpine 3.19, GCC 13.2, musl 1.2.4, static, `--enable-jit --without-ssl`).

Deeper detail: [module structure](docs/module-structure.md) · [boot flow](docs/boot-flow.md) · [runtime architecture](docs/runtime-arch.md).

## Building & running

### Prerequisites

- **Rust** — the toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on first build.
- **A C toolchain** — `build-essential` or equivalent; rustc needs `cc` to link build scripts and proc-macros.
- **QEMU with KVM** (`qemu-system-x86_64`) — for local runs; use `-enable-kvm`, not TCG.
- **A static `beam.smp` + the OTP/Elixir rootfs cpio** — both committed (`src/beam.smp.elf`, `src/otp-rootfs.cpio`), so the kernel builds out of the box. To rebuild, see [`docs/BUILDING_ERTS.md`](docs/BUILDING_ERTS.md).

### Build

```bash
cargo build --release --target x86_64-tyn.json \
  -Zbuild-std=core,alloc,compiler_builtins \
  -Zbuild-std-features=compiler-builtins-mem
```

### Run

```bash
qemu-system-x86_64 \
  -kernel target/x86_64-tyn/release/tyn-kernel \
  -m 2560M -machine q35 -cpu host -enable-kvm -smp 8 \
  -nographic -no-reboot -serial mon:stdio \
  -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
  -netdev user,id=net0,hostfwd=tcp::5555-:8080,hostfwd=tcp::5567-:9090
```

The committed image boots a **minimal bench app** — small endpoints to confirm the kernel boots, serves, and runs the BEAM. It's *not* the full Phoenix demo; the stock-`phx.new` app is what the [public AMI](#try-it) runs and what you get by deploying your own app. Once the serial console prints `phoenix_listening`:

```bash
curl http://localhost:5555/          # landing page (endpoint list)
curl http://localhost:5555/health    # → {"status":"ok"}
curl http://localhost:5555/json      # live BEAM stats
nc localhost 5567                    # eval shell
```

> Use KVM (`-enable-kvm`), not TCG. Software emulation deterministically `#PF`s at boot on some images, and QEMU/SLIRP host networking was the bottleneck behind every early throughput figure. Benchmark on KVM or Nitro.

## Testing

`tests/` holds the capability suite. Every assertion checks **content, not status codes** — a truncated asset served as `200` is exactly the bug class this exists to catch.

```bash
tests/setup-test-app.sh      # builds a stock mix phx.new app; fails if any dep is patched
tests/run.sh <instance-ip>   # byte-exact assertions; non-zero exit gates a build
```

It covers byte-exact static assets, large transfers spanning many TX windows, inline and multi-send bodies, N=25 concurrency with per-response hashes, and interactive LiveView.

## Engineering record

The bug-class hunts behind the current state, kept because the negative results are as useful as the fixes:

- [`docs/SEND_CORRUPTION.md`](docs/SEND_CORRUPTION.md) — the TCP send-path corruption hunt: eliminated hypotheses, the non-perturbing trace that localized it, and the `sys_writev` partial-write root cause.
- [`docs/FUTEX_HISTORY.md`](docs/FUTEX_HISTORY.md) — the boot-path futex valve: the init-time thread-progress hazard and the ledger of rejected hypotheses.
- [`BUGS.md`](BUGS.md) (BUG-1) — the SMP corruption residual: a missing IST on the wakeup IPI let its interrupt frame land in the BEAM red zone under SMP. Fixed, and later dissolved outright by the ring-3 rework (the clean-stack transition removes the red-zone hazard by construction).

## Design principles

- **Run the real BEAM** — the actual ERTS, cross-compiled for Tyn's host interface, not a reimplementation. The BEAM's hard parts (SMP scheduler, JIT, GC, distribution) stay upstream.
- **Minimal kernel, maximal BEAM** — the kernel provides memory, interrupts, device access, and network. The BEAM does its own scheduling, memory management, code loading, and supervision.
- **Target KVM/virtio and Nitro** — standardized virtual hardware means a handful of small drivers; each NIC driver is a few hundred lines of Rust.
- **Built for verification** — minimal `unsafe`, explicit invariants, a small and now enforced TCB.

## Prior art

- **[LING](https://github.com/cloudozer/ling)** — Erlang on Xen. Proved the concept; reimplemented the BEAM and targeted only Xen.
- **[Nerves](https://nerves-project.org/)** — Elixir on embedded Linux. Complementary: Nerves owns embedded, Tyn targets cloud.
- **[GRiSP](https://www.grisp.org/)** — BEAM on RTEMS for IoT hardware.
- **[Asterinas](https://github.com/asterinas/asterinas)** — Rust Linux-compatible kernel; architectural reference.
- **[rcore-os/virtio-drivers](https://github.com/rcore-os/virtio-drivers)** · **[smoltcp](https://github.com/smoltcp-rs/smoltcp)** — used by Tyn.

## Related projects

- **[Vor](https://github.com/vorlang/vor)** — a BEAM-native language with compile-time verification.
- **[VorDB](https://github.com/vorlang/vordb)** — a CRDT-based distributed database built on Vor.

## License

MIT OR Apache-2.0
