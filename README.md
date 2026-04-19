# Tyn

A minimal Rust microkernel purpose-built for BEAM.

No Linux. No POSIX. Just your Erlang/Elixir/Gleam code on bare metal.

## What is Tyn?

Tyn is a unikernel — a single-purpose operating system kernel that hosts one thing: the BEAM virtual machine. It replaces the entire Linux stack with a few thousand lines of Rust, targeting KVM/QEMU cloud deployments.

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
├─────────────────────────────────────────┤
│  Tyn Kernel (Rust)                      │
│  Memory · Interrupts · virtio · TCP/IP  │
├─────────────────────────────────────────┤
│  KVM / QEMU / Cloud Hypervisor          │
└─────────────────────────────────────────┘
```

Tyn runs the real, unmodified ERTS/BEAM — not a reimplementation. When OTP ships a new version, it should just work. This is the critical lesson from [LING](https://github.com/cloudozer/ling) (Erlang on Xen), which died because it reimplemented the VM and couldn't keep pace with upstream changes.

## Status

🚧 **Early development — Phase 2 complete.**

The kernel boots via multiboot2 on QEMU/KVM, initializes memory with identity-mapped page tables, discovers PCI devices, negotiates with a virtio-net device, and runs a TCP echo server via [smoltcp](https://github.com/smoltcp-rs/smoltcp).

```
$ nc localhost 5555
Hello Tyn!
Hello Tyn!
```

### What works

- Multiboot2 boot with identity-mapped 4 GiB address space
- GDT, IDT, interrupt handling (exceptions + PIC timer)
- Physical frame allocator and kernel heap
- PCI bus enumeration (ECAM on q35)
- virtio-net driver via [virtio-drivers](https://github.com/rcore-os/virtio-drivers) crate
- TCP/IP networking via smoltcp
- TCP echo server on port 8080

### What's next

- **Phase 3:** BEAM host interface — implement the syscall/libc shim that lets ERTS run on Tyn
- **Phase 4:** Boot ERTS and reach an Erlang shell over the network
- **Phase 5:** Run a Phoenix application
- **Phase 6:** Production hardening, cloud image packaging, formal verification with [Verus](https://github.com/verus-lang/verus)

## Building & Running

### Prerequisites

- Rust nightly toolchain with `rust-src` component
- QEMU with KVM support (`qemu-system-x86_64`)

### Build

```bash
cargo build --release --target x86_64-tyn.json \
  -Zbuild-std=core,alloc,compiler_builtins \
  -Zbuild-std-features=compiler-builtins-mem
```

### Run

```bash
qemu-system-x86_64 \
  -enable-kvm \
  -machine q35 \
  -kernel target/x86_64-tyn/release/tyn-kernel \
  -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
  -netdev user,id=net0,hostfwd=tcp::5555-:8080 \
  -serial stdio \
  -display none \
  -m 128M
```

Key flags:
- `-machine q35` — provides ECAM PCI config space at `0xB0000000`
- `disable-legacy=on,disable-modern=off` — modern virtio-pci v1.0+ only
- `hostfwd=tcp::5555-:8080` — forward host port 5555 to guest TCP echo server

### Test TCP echo

```bash
# In another terminal:
echo "Hello Tyn!" | nc localhost 5555
# → Hello Tyn!
```

## Design Principles

**Run the real BEAM.** Not a reimplementation — the actual ERTS, cross-compiled for Tyn's host interface.

**Path A: everything is BEAM or Rust.** No Linux, no POSIX, no arbitrary binaries. This constraint enables the small trusted computing base and clean verification story.

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
