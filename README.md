# Tyn

A minimal Rust microkernel purpose-built for BEAM.

No Linux. No POSIX. Just your Erlang/Elixir/Gleam code on bare metal.

## What is Tyn?

Tyn is a unikernel — a single-purpose operating system kernel that hosts one thing: the BEAM virtual machine. It replaces the entire Linux stack with ~3,200 lines of Rust, targeting KVM/QEMU cloud deployments.

The BEAM already has its own process model, scheduler, memory management, and distribution protocol. Linux sits underneath adding 30 million lines of unverified C that the BEAM neither needs nor benefits from. Tyn removes that.

## Why?

**Security.** A typical Linux kernel has thousands of CVEs across subsystems your BEAM workload never touches — USB drivers, filesystem code, GPU support. Tyn has none of that. The attack surface is a few thousand lines of Rust instead of 30 million lines of C.

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
│  ERTS / BEAM VM (unmodified)            │
├─────────────────────────────────────────┤
│  BEAM Host Interface (Rust)             │
│  ~50 Linux syscalls emulated            │
├─────────────────────────────────────────┤
│  Tyn Kernel (Rust, ~3,200 LOC)          │
│  Memory · Interrupts · VFS · Serial I/O │
├─────────────────────────────────────────┤
│  KVM / QEMU / Cloud Hypervisor          │
└─────────────────────────────────────────┘
```

Tyn runs the real, unmodified ERTS/BEAM — not a reimplementation. When OTP ships a new version, it should just work. This is the critical lesson from [LING](https://github.com/cloudozer/ling) (Erlang on Xen), which died because it reimplemented the VM and couldn't keep pace with upstream changes.

## Status

**Phase 3 complete — BEAM runs on bare metal with TCP networking.**

```
=== Tyn Kernel v0.1.0 ===
[pci] 0:2.0 VirtIO Network
[net] MAC=[52, 54, 00, 12, 34, 56]
[net] initialized, IP=10.0.2.15
[vfs] cpio: 155 files, 8113152 bytes
...51 .beam files loaded...
{listen,ok}
{accepted,#Port<0.155>}
done
```

```
$ echo "hello" | nc localhost 5555
Hi Tyn
```

- ERTS boots, loads 51 .beam files, starts 25 OTP processes
- `gen_tcp:listen` → `gen_tcp:accept` → `gen_tcp:send` works end-to-end
- Host connects via `nc`, receives response from Erlang code running on Tyn
- Full path: ERTS → syscall → smoltcp → virtio-net → QEMU → host

### What works

- Multiboot2 boot with identity-mapped 4 GiB address space
- GDT, IDT, TSS with IST for safe interrupt handling
- PIT timer (100 Hz) with preemptive scheduling
- ~50 Linux syscalls emulated (mmap, read, write, open, stat, pipe, ppoll, futex, clone, ...)
- POSIX socket layer (socket, bind, listen, accept, send, recv, setsockopt, getsockopt)
- ELF loader for static musl binaries
- In-memory VFS backed by cpio archive (start.boot + kernel/stdlib .beam files)
- Directory listing (getdents64) for OTP code_server
- Cooperative and preemptive threading (up to 24 threads)
- Per-thread kernel stacks and IST stacks
- Monotonic clock via RDTSC
- COM1 serial I/O (stdin/stdout/stderr)
- PCI bus enumeration (ECAM on q35)
- virtio-net driver via [virtio-drivers](https://github.com/rcore-os/virtio-drivers)
- TCP/IP networking via [smoltcp](https://github.com/smoltcp-rs/smoltcp)
- gen_tcp:listen/accept/send works — Erlang TCP server serves responses to host clients

### What's next

- **Interactive shell** — full stdin support so the Erlang shell accepts typed input
- **OTP 27 SMP** — multi-CPU kernel support to run modern ERTS with threading
- **Virtio networking** — connect ERTS inet to the virtio-net driver
- **Phoenix application** — run a web framework on Tyn
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
  -m 2G -machine q35 -cpu host -enable-kvm \
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
