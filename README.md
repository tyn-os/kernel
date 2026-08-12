# Tyn

A minimal Rust microkernel purpose-built for BEAM.

No Linux. No POSIX. Just your Erlang/Elixir code on bare metal.

## What is Tyn?

Tyn is a unikernel — a single-purpose operating system kernel that hosts one thing: the BEAM virtual machine. It replaces the entire Linux stack with ~8,000 lines of Rust, and runs on both KVM/QEMU and **real AWS Nitro EC2**, where it drives the network with a from-scratch ENA NIC driver and serves production HTTP traffic.

The BEAM already has its own process model, scheduler, memory management, and distribution protocol. A general-purpose OS kernel underneath duplicates much of what BEAM provides natively. Tyn explores what happens when you remove that redundancy and give BEAM a purpose-built host.

Tyn runs the real, unmodified ERTS/BEAM — not a reimplementation. When OTP ships a new version, it should just work. This is partly why Tyn leans on upstream ERTS rather than reimplementing it, as the pioneering [LING](https://github.com/cloudozer/ling) (Erlang on Xen) did — keeping in step with fast-moving upstream OTP is a lot to take on alongside a reimplemented VM.

## Why?

**Security.** A general-purpose kernel includes subsystems for hardware a cloud BEAM workload never uses — USB, GPUs, dozens of filesystems, thousands of device drivers. Tyn includes only what BEAM needs, reducing the attack surface to a few thousand lines of Rust.

**Simplicity.** A Tyn image contains only BEAM bytecode and the Rust kernel. No general-purpose OS services, no package management, no user accounts — just your application and its runtime.

**Density.** Tyn images are megabytes, not gigabytes. More BEAM nodes per host, lower cloud costs.

**Verifiability.** One runtime, a small trusted computing base, and a kernel structured for formal verification.

## Status

**A stock `mix phx.new` Phoenix app — static assets, LiveView, sessions, outbound HTTP — runs unmodified on OTP 27 BEAM on bare metal, on real AWS Nitro.**

Measured on a stock **c5.large**, driven from a separate EC2 instance (real ENA NIC, no host loopback):

| | |
| --- | --- |
| Throughput | **4,175 req/s** at 25-way concurrency (2000/2000, zero failures) |
| Concurrency | 100% success through **N=50** |
| Image size | **~45 MB** (ERTS + OTP/Elixir rootfs + kernel) |
| Boot to serving HTTP | **~5 s** (kernel → BEAM handoff in ~430 ms; the rest is OTP startup + JIT codegen) |
| Boot reliability | **~97%** on Nitro (62/64 across two 32-trial sweeps) — see Limitations |

The full path is `ENA hardware → admin queue → I/O queues → smoltcp → DHCP → gen_tcp → Bandit → Phoenix`, entirely inside the Rust kernel. No Linux, no host networking — the kernel talks to the NIC's descriptor rings directly.

## Try it

A public AMI is available in `us-east-1` — under two minutes, no build required.

```bash
aws ec2 run-instances --image-id ami-09619e2d139f2a57d \
    --instance-type c5.large --region us-east-1
# open port 8080 in your security group, wait ~10s, then:
curl http://<public-ip>:8080/                          # → Phoenix landing page
curl -s http://<public-ip>:8080/assets/big.bin | wc -c  # → 1500000, a static asset via kernel sendfile
# then open http://<public-ip>:8080/counter in a browser — the LiveView counter increments live
```

The full walkthrough — security groups, the IAM-gated serial console shell, and deploying **your own** app with `tyn-pack` — is in [`docs/DEPLOY.md`](docs/DEPLOY.md).

Instances accrue hourly charges. Terminate when you're done.

## What works

- **Full Phoenix stack on a stock app** — a stock `mix phx.new` app runs unmodified: **static assets** (`Plug.Static` / `send_file` via kernel `sendfile(2)` + `dup(2)`, no dependency patch), **interactive LiveView** (WebSocket mount + live updates over the socket), **`runtime.exs`** evaluation (env vars, deep-merged config), signed cookies / CSRF / `Phoenix.Token`, and **outbound TCP/UDP + DNS**. Clean-clone validated byte-exact on real Nitro and codified in [`tests/`](tests/).
- **8-way SMP** — ACPI/MADT CPU discovery, APIC timer calibration, AP trampoline (16→64 bit), per-CPU GDT/TSS/IST, GS_BASE per-CPU syscall data, IPI wakeup, preemptive user-mode scheduling.
- **BeamAsm JIT** — OTP 27.3.4.2 built with `--enable-jit`. The timer trampoline preempts inside mmap'd JIT pages; the host-side stat/dir syscalls handle the validation the BeamAsm loader performs. `erlang:system_info(emu_flavor)` returns `jit`.
- **TCP/UDP networking** — `gen_tcp` and `gen_udp` end-to-end: POSIX socket layer → smoltcp → **virtio-net (QEMU)** or a **from-scratch ENA driver (AWS Nitro)**. On Nitro the address comes from **DHCP**, with lease renewal for long-lived instances.
- **AWS Nitro deployment** — boots from a GRUB/multiboot disk image imported as an EBS snapshot. The ENA NIC (`1d0f:ec20`) is found via port-IO PCI config, since Nitro publishes no MCFG/ECAM.
- **`:crypto`** — a from-scratch Rust NIF (RustCrypto primitives) fed by a kernel CSPRNG (RDSEED → ChaCha20), statically linked into ERTS. Passes known-answer vectors and cross-checks byte-for-byte against upstream OTP. **Unreviewed — see Limitations.**
- **Live eval shell** — over the AWS serial console (IAM-gated, no open port) or TCP. Evaluate Erlang or Elixir against a running BEAM; bindings persist per session.
- **Elixir 1.18.3** — loads and executes on OTP 27.
- **~50 Linux syscalls** — `mmap`, `read`, `write`, `open`, `stat`, `pipe`, `ppoll`, `futex`, `clone`, `epoll`, `select`, `readv`, `writev`, `sendfile`, `dup`, `getrandom`, …
- **VFS** — read-only cpio (newc) with OTP + Elixir `.beam` files; application images are packed by `tyn-pack`.
- **Boot** — Multiboot1, identity-mapped 4 GiB, ELF loader for static musl binaries.

```
$ ssh <instance>.port0@serial-console.ec2-instance-connect.us-east-1.aws
>> erlang:system_info(emu_flavor).
jit
>> 'Elixir.System':version().
<<"1.18.3">>
>> erlang:system_info(process_count).
351
```

## Limitations

Tyn runs a real, unmodified OTP 27 + Phoenix stack, but it is a specialized runtime with deliberate constraints. **These are first-class, not footnotes — read them before deploying.**

- **No in-guest TLS.** `ssl`, `public_key`, and `asn1` are stubs — they satisfy the dependency graph but provide no functions. **Terminate TLS at the load balancer** and serve plain HTTP in-guest. An `https:` listener starts cleanly and then `:undef`s at request time; `tyn-pack` warns when it detects one.
- **Crypto is from-scratch and unreviewed.** It passes known-answer vectors and matches upstream OTP byte-for-byte, but has had **no outside security review** — don't trust it for production session security until it has. Boot also **panics without a hardware RNG** (RDRAND/RDSEED; present on the c5/m5/t3 Nitro families).
- **Wall clock is pinned at the epoch (1970).** Monotonic time is correct. Anything depending on *absolute* wall-clock time breaks: TLS certificate date validation (another reason to terminate TLS at the LB), absolute cookie/token expiry, `Date` headers. Relative timers and `Phoenix.Token` max-age are fine. No RTC/kvmclock yet.
- **No writable filesystem.** The VFS is a read-only cpio. Writes to `/tmp`, cwd, or `/dev/shm` return `enoent`; file uploads (`Plug.Upload`) and anything needing scratch disk do not work.
- **No distributed Erlang.** No `epmd`, no `net_kernel`. Single node only.
- **IPv4 only.** IPv6 socket binds are rewritten to IPv4-any at boot (stock Phoenix `runtime.exs` binds IPv6-any).
- **~3% cold-boot stall.** A residual liveness stall during ERTS SMP init — no crash, no corruption, and **retry always succeeds.** Mitigate with orchestration-layer retry (standard cloud practice); two retries give ~99.97% effective. Full history and the hypothesis ledger: [`docs/FUTEX_HISTORY.md`](docs/FUTEX_HISTORY.md).
- **Use KVM or Nitro, not QEMU-TCG.** Under software emulation (`-accel tcg`) some images deterministically `#PF` at boot. Real hardware (Nitro, or KVM with `-enable-kvm`) is unaffected and is the standard of evidence.
- **Deploying LiveView on a bare IP needs `check_origin`.** Phoenix returns `403` on the LiveView WebSocket when the served host (e.g. `http://<ip>:8080`) doesn't match the configured URL host. `check_origin: false` is fine for a throwaway IP demo — but for production set the **real host list**, because `false` is a cross-site WebSocket-hijacking hole on a real deployment.

## Architecture

```
┌─────────────────────────────────────────┐
│  Applications (Elixir / Erlang)         │
├─────────────────────────────────────────┤
│  OTP / Supervision Trees                │
├─────────────────────────────────────────┤
│  ERTS / BEAM VM (unmodified, SMP, JIT)  │
├─────────────────────────────────────────┤
│  BEAM Host Interface (Rust)             │
│  ~50 Linux syscalls emulated            │
├─────────────────────────────────────────┤
│  Tyn Kernel (Rust, ~8,000 LOC)          │
│  SMP · Memory · Networking · VFS · I/O  │
├─────────────────────────────────────────┤
│  KVM / QEMU / AWS Nitro                 │
└─────────────────────────────────────────┘
```

ERTS is built from unmodified OTP 27 source — no patches, no special defines — via the pinned, reproducible build in [`beam-build/`](beam-build/) (Alpine 3.19, GCC 13.2, musl 1.2.4, static, `--enable-jit --without-ssl`).

Tyn uses a conservative futex valve: `futex_wait` **spin-yields through boot** — avoiding a rare init-time thread-progress deadlock that real blocking exposes — then switches to real blocking (so idle CPUs enter `HLT`) once the app has finished starting, signalled by the boot harness's `serial_shell ready` marker. The trigger is deliberately late and past the whole init window; earlier proxies (module-open count, `managed_count`, listen-port) each fired *before* the deadlock and reintroduced the stall. On the GCC-14 boot-stress amplifier this takes the stall from ~15% to **0/32**. Full diagnosis, the five ruled-out mechanisms, and the workaround-hygiene backstory are in [`docs/FUTEX_HISTORY.md`](docs/FUTEX_HISTORY.md).

More: [module structure](docs/module-structure.md) · [boot flow](docs/boot-flow.md) · [runtime architecture](docs/runtime-arch.md)

## Building & Running

### Prerequisites

- Rust — the toolchain is pinned in `rust-toolchain.toml` (a specific nightly with `rust-src`);
  `rustup` installs it automatically on first build. Just have `rustup`.
- A C toolchain — `build-essential` (Debian/Ubuntu) or equivalent; rustc needs `cc` to link build
  scripts and proc-macros.
- QEMU with KVM (`qemu-system-x86_64`) — for the local run only; **use `-enable-kvm`, not TCG**.
- A statically-linked `beam.smp` and the OTP/Elixir rootfs cpio — **both are committed** at `src/beam.smp.elf` and `src/otp-rootfs.cpio`, so the kernel builds out of the box. To rebuild them, see [`docs/BUILDING_ERTS.md`](docs/BUILDING_ERTS.md).

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

The committed image (`src/beam.smp.elf` + `src/otp-rootfs.cpio`) boots a **minimal bench app** — small endpoints to confirm the kernel boots, serves, and runs the BEAM. It is *not* the full Phoenix demo; the stock-`phx.new` app with static assets and LiveView (the capability claims above) is what the [public AMI](#try-it) runs and what you get by [packaging your own app](docs/DEPLOY.md). Once the serial console prints `phoenix_listening`:

```bash
curl http://localhost:5555/          # landing page (endpoint list)
curl http://localhost:5555/health    # → {"status":"ok"}
curl http://localhost:5555/hello     # → Hello from Phoenix on Tyn!
curl http://localhost:5555/json      # live BEAM stats
nc localhost 5567                    # eval shell
```

> **Use KVM (`-enable-kvm`), not TCG.** Software emulation (`-accel tcg`) deterministically `#PF`s at boot on some images. QEMU/SLIRP is a development convenience, not a performance environment — its host networking was the bottleneck behind every early throughput figure Tyn recorded. Benchmark on KVM or Nitro.

## Testing

`tests/` holds the capability suite. Every assertion checks **content**, not status codes — a truncated asset served as `200` is precisely the class of bug this exists to catch.

```bash
tests/setup-test-app.sh      # builds a stock mix phx.new app; fails if any dep is patched
tests/run.sh <instance-ip>   # byte-exact assertions; non-zero exit gates a build
```

It covers byte-exact static assets, large (1.5 MB) transfers spanning many TX windows, inline and multi-send bodies, N=25 concurrency with per-response hashes, and interactive LiveView.

## Engineering record

The bug-class hunts behind the current state, kept because the negative results are as useful as the fixes:

- [`docs/FUTEX_HISTORY.md`](docs/FUTEX_HISTORY.md) — the cold-boot stall: current reliability numbers, a ledger of rejected hypotheses, confirmed facts, and the open questions.
- [`docs/SEND_CORRUPTION.md`](docs/SEND_CORRUPTION.md) — the TCP send-path corruption hunt: four eliminated hypotheses, the non-perturbing trace technique that localized it, and the `sys_writev` partial-write root cause.

## Design Principles

**Run the real BEAM.** Not a reimplementation — the actual ERTS, cross-compiled for Tyn's host interface.

**Purpose-built for BEAM.** The kernel hosts one runtime and nothing else. That constraint enables a small trusted computing base and a clean verification story.

**Minimal kernel, maximal BEAM.** The kernel provides only what BEAM needs — memory, interrupts, device access, network. BEAM handles its own scheduling, memory management, code loading, and supervision.

**Target KVM/virtio and AWS Nitro.** Standardized virtual hardware means a handful of drivers — each NIC driver is a few hundred lines of Rust.

**Designed for verification.** Structured for future formal verification with Verus: minimal unsafe code, explicit invariants, a small trusted computing base.

## Prior Art

- **[LING](https://github.com/cloudozer/ling)** — Erlang on Xen. Proved the concept; reimplemented BEAM and targeted only Xen.
- **[Nerves](https://nerves-project.org/)** — Elixir on embedded Linux. Complementary: Nerves owns embedded, Tyn targets cloud.
- **[GRiSP](https://www.grisp.org/)** — BEAM on RTEMS for IoT hardware.
- **[Asterinas](https://github.com/asterinas/asterinas)** — Rust Linux-compatible kernel. Architectural reference.
- **[rcore-os/virtio-drivers](https://github.com/rcore-os/virtio-drivers)** · **[smoltcp](https://github.com/smoltcp-rs/smoltcp)** — used by Tyn.

## Related Projects

- **[Vor](https://github.com/vorlang/vor)** — a BEAM-native language with compile-time verification
- **[VorDB](https://github.com/vorlang/vordb)** — a CRDT-based distributed database built on Vor

## License

MIT OR Apache-2.0
