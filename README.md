# Tyn

A minimal Rust microkernel purpose-built for BEAM.

No Linux. No POSIX. Just your Erlang/Elixir/Gleam code on bare metal.

## What is Tyn?

Tyn is a unikernel — a single-purpose operating system kernel that hosts one thing: the BEAM virtual machine. It replaces the entire Linux stack with ~5,000 lines of Rust, targeting KVM/QEMU cloud deployments.

The BEAM already has its own process model, scheduler, memory management, and distribution protocol. Linux sits underneath adding 40 million lines of unverified C that the BEAM neither needs nor benefits from. Tyn removes that.

## Why?

**Security.** A typical Linux kernel has thousands of CVEs across subsystems your BEAM workload never touches — USB drivers, filesystem code, GPU support. Tyn has none of that. The attack surface is a few thousand lines of Rust instead of 40 million lines of C.

**Simplicity.** Everything in a Tyn image is either BEAM bytecode or Rust compiled for this kernel. Nothing else. No systemd, no shell, no package manager, no users, no cron.

**Boot speed.** Tyn boots in milliseconds, not seconds. For elastic cloud deployments where BEAM nodes scale up and down, this matters.

**Density.** Tyn images are megabytes, not gigabytes. More BEAM nodes per host, lower cloud costs.

## Architecture

```
┌─────────────────────────────────────────┐
│  Applications (Elixir / Erlang / Gleam) │
├─────────────────────────────────────────┤
│  OTP / Supervision Trees                │
├─────────────────────────────────────────┤
│  ERTS / BEAM VM (unmodified, SMP)       │
├─────────────────────────────────────────┤
│  BEAM Host Interface (Rust)             │
│  ~50 Linux syscalls emulated            │
├─────────────────────────────────────────┤
│  Tyn Kernel (Rust, ~5,000 LOC)          │
│  SMP · Memory · Networking · VFS · I/O  │
├─────────────────────────────────────────┤
│  KVM / QEMU / Cloud Hypervisor          │
└─────────────────────────────────────────┘
```

Tyn runs the real, unmodified ERTS/BEAM — not a reimplementation. When OTP ships a new version, it should just work. This is the critical lesson from [LING](https://github.com/cloudozer/ling) (Erlang on Xen), which died because it reimplemented the VM and couldn't keep pace with upstream changes.

## Status

**OTP 27 BEAM running on bare metal with SMP + TCP networking + Elixir.**

```
{otp27,"27"}
Hello from Elixir on Tyn!
{tcp_listen,{ok,#Port<0.3>}}
```

- OTP 27 ERTS boots with 8 CPUs, loads 83+ .beam files from in-memory VFS
- Full OTP kernel application starts — supervision trees, code_server, logger
- Elixir modules load and execute (`IO.puts` works)
- `gen_tcp:listen` succeeds — smoltcp TCP/IP stack wired to virtio-net
- 8-CPU SMP with per-CPU syscall state, preemptive scheduling, APIC timers

### What works

- **SMP** — ACPI MADT parsing, Local APIC timer calibration, AP trampoline (16-bit → 64-bit), per-CPU GDT/TSS/IST, GS_BASE per-CPU syscall data, IPI wakeup
- **Preemptive scheduling** — timer yields user-mode code directly, deferred reschedule for kernel-mode, per-CPU run queues with load balancing
- **~50 Linux syscalls** — mmap, read, write, open, stat, pipe, ppoll, futex, clone, epoll, select, readv, ...
- **Networking** — POSIX socket layer → smoltcp TCP/IP → virtio-net PCI → QEMU, gen_tcp:listen works
- **VFS** — cpio newc archive with 480+ files (OTP kernel/stdlib + Elixir core .beam files)
- **Boot** — Multiboot1, identity-mapped 4 GiB (1 GiB huge pages), ELF loader for static musl binaries
- **Interrupts** — IDT with IST, APIC EOI, lock-free crash handlers for SMP
- **Threading** — up to 16 threads, per-thread kernel stacks, atomic futex with per-address spinlocks
- **Time** — monotonic clock via RDTSC, APIC timer calibration against PIT
- **I/O** — COM1 serial (stdin/stdout/stderr), PCI ECAM enumeration

### What's next

- **TCP accept** — fix blocking accept loop for OTP 27's inet driver
- **Phoenix application** — run a web framework on Tyn
- **Interactive shell** — full stdin support so the Erlang shell accepts typed input
- **Formal verification** — Verus proofs for critical kernel paths

## Building & Running

### Prerequisites

- Rust nightly toolchain with `rust-src` component
- QEMU with KVM support (`qemu-system-x86_64`)
- Pre-built BEAM binary and OTP rootfs (see [BUILDING.md](BUILDING.md))

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
  -netdev user,id=net0,hostfwd=tcp::5555-:8080
```

### Test TCP from host

```bash
# In another terminal while QEMU is running:
echo "hello" | nc localhost 5555
# → Hi Tyn
```

## Design Principles

**Run the real BEAM.** Not a reimplementation — the actual ERTS, cross-compiled for Tyn's host interface.

**Everything is BEAM or Rust.** No Linux, no POSIX, no arbitrary binaries. This constraint enables the small trusted computing base and clean verification story.

**Minimal kernel, maximal BEAM.** The kernel provides only what BEAM needs — memory, interrupts, device access, network. BEAM handles its own scheduling, memory management, code loading, and supervision.

**Target KVM/virtio.** Standardized virtual hardware eliminates the driver diversity problem. The kernel's entire driver layer is a few hundred lines of Rust.

**Designed for verification.** The kernel is structured for future formal verification with Verus. Minimal unsafe code, explicit invariants, small trusted computing base.

## Prior Art

- **[LING](https://github.com/cloudozer/ling)** — Erlang on Xen. Proved the concept. Died because it reimplemented BEAM and targeted only Xen.
- **[Nerves](https://nerves-project.org/)** — Elixir on embedded Linux. Complementary — Nerves owns embedded, Tyn targets cloud.
- **[GRiSP](https://www.grisp.org/)** — BEAM on RTEMS for IoT hardware. Different niche.
- **[Asterinas](https://github.com/asterinas/asterinas)** — Rust Linux-compatible kernel. Architectural reference.
- **[rcore-os/virtio-drivers](https://github.com/rcore-os/virtio-drivers)** — VirtIO drivers used by Tyn.
- **[smoltcp](https://github.com/smoltcp-rs/smoltcp)** — TCP/IP stack used by Tyn.

## Related Projects

Tyn is part of a broader ecosystem:

- **[Vor](https://github.com/vorlang/vor)** — A BEAM-native language with compile-time verification
- **[VorDB](https://github.com/vorlang/vordb)** — A CRDT-based distributed database built on Vor

## License

MIT OR Apache-2.0
