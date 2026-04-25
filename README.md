# Tyn

A minimal Rust microkernel purpose-built for BEAM.

No Linux. No POSIX. Just your Erlang/Elixir/Gleam code on bare metal.

## What is Tyn?

Tyn is a unikernel — a single-purpose operating system kernel that hosts one thing: the BEAM virtual machine. It replaces the entire Linux stack with ~5,000 lines of Rust, targeting KVM/QEMU cloud deployments.

The BEAM already has its own process model, scheduler, memory management, and distribution protocol. A general-purpose OS kernel underneath duplicates much of what the BEAM provides natively. Tyn explores what happens when you remove that redundancy and give BEAM a purpose-built host.

## Why?

**Security.** A general-purpose kernel includes subsystems for hardware a cloud BEAM workload never uses — USB, GPUs, dozens of filesystems, thousands of device drivers. Tyn includes only what BEAM needs, reducing the attack surface to a few thousand lines of Rust.

**Simplicity.** A Tyn image contains only BEAM bytecode and the Rust kernel. No general-purpose OS services, no package management, no user accounts — just your application and its runtime.

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

**OTP 27 BEAM running on bare metal with SMP, TCP networking, and Elixir.**

```
$ echo "hello" | nc localhost 5555
Hello from OTP 27 on Tyn!
```

```
{otp27,"27"}
{listening,8080}
{accepted,#Port<0.4>}
done
```

- OTP 27 ERTS boots with 8 CPUs, loads 80+ .beam files from in-memory VFS
- Full OTP kernel application starts — supervision trees, code_server, logger
- Erlang TCP server: `gen_tcp:listen` → `accept` → `send` → host receives data
- Elixir 1.18.3 runs: `IO.puts`, `System.version`, `Kernel.inspect` all work
- Stable under load — serves TCP connections indefinitely, 41 OTP processes, zero crashes

### What works

- **8-way SMP** — ACPI/MADT CPU discovery, APIC timer calibration, AP trampoline (16→64 bit), per-CPU GDT/TSS/IST, GS_BASE per-CPU syscall data, IPI wakeup, preemptive user-mode scheduling
- **TCP networking** — `gen_tcp:listen/accept/send/close` end-to-end, POSIX socket layer → smoltcp TCP/IP → virtio-net PCI → QEMU → host
- **Elixir** — Elixir 1.18.3 .beam files load and execute on OTP 27
- **~50 Linux syscalls** — mmap, read, write, open, stat, pipe, ppoll, futex, clone, epoll, select, readv, ...
- **VFS** — cpio newc archive with OTP kernel/stdlib .beam files + optional Elixir
- **Boot** — Multiboot1, identity-mapped 4 GiB, ELF loader for static musl binaries
- **Threading** — up to 16 CPUs, per-thread kernel stacks, atomic futex, preemptive + deferred scheduling
- **I/O** — COM1 serial (stdin/stdout/stderr), PCI ECAM, virtio-net

### What's next

- **Phoenix/Bandit** — run a web framework on Tyn
- **Interactive shell** — IEx/Erlang shell with full stdin support
- **Formal verification** — Verus proofs for critical kernel paths

## Building & Running

### Prerequisites

- Rust nightly toolchain with `rust-src` component
- QEMU with KVM support (`qemu-system-x86_64`)
- Pre-built BEAM binary and OTP rootfs (see "Building ERTS + VFS" below)

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
# → conn 1 procs 41
```

## Building ERTS + VFS

Tyn embeds a statically-linked ERTS binary and a cpio archive of .beam files directly in the kernel image. Here's how to build them.

### Cross-compile OTP 27 ERTS

```bash
# On an x86_64 Linux host with musl-gcc installed:
git clone --branch OTP-27.3.4.2 https://github.com/erlang/otp.git otp27
cd otp27

# Configure for static musl (no JIT, minimal dependencies)
./configure --disable-jit --without-javac --without-odbc --without-wx \
  --without-termcap --without-ssl --without-ssh --without-megaco \
  --without-diameter --without-observer --without-debugger \
  --without-et --without-reltool --without-common-test --without-eunit \
  --without-edoc --without-eldap --without-ftp --without-tftp \
  --without-snmp --without-docs --without-mnesia \
  CC=musl-gcc CFLAGS="-O2 -static" LDFLAGS=-static

# Build
make -j$(nproc)

# The static beam.smp binary:
ls bin/x86_64-pc-linux-musl/beam.smp
# → ~9 MB statically linked ELF
```

### Package the VFS (cpio archive)

```bash
# Create an OTP release directory with .beam files
mkdir -p staging/otp/bin
cp otp27/bin/start.boot staging/otp/bin/

# Copy kernel and stdlib .beam files (with versioned paths for boot script)
for d in otp27/lib/kernel-*/ebin otp27/lib/stdlib-*/ebin; do
  versioned=$(basename $(dirname $d))
  mkdir -p staging/otp/lib/$versioned/ebin
  cp $d/*.beam staging/otp/lib/$versioned/ebin/
done

# Copy .beam files to root for code_server fallback loading
cp otp27/lib/kernel-*/ebin/*.beam staging/
cp otp27/lib/stdlib-*/ebin/*.beam staging/

# Create the cpio archive
cd staging
find . -type f | sed 's|^\./||' | cpio -o -H newc > ../src/otp-rootfs.cpio

# Copy the ERTS binary
cp otp27/bin/x86_64-pc-linux-musl/beam.smp ../src/beam.smp.elf
```

### Elixir support (optional)

```bash
# Download prebuilt Elixir for OTP 27
curl -L -o elixir.zip \
  https://github.com/elixir-lang/elixir/releases/download/v1.18.3/elixir-otp-27.zip
unzip elixir.zip -d elixir

# Add Elixir .beam files to the staging root
cp elixir/lib/elixir/ebin/*.beam staging/
cp elixir/lib/iex/ebin/*.beam staging/

# Rebuild cpio with Elixir included
cd staging && find . -type f | sed 's|^\./||' | cpio -o -H newc > ../src/otp-rootfs.cpio
```

## Design Principles

**Run the real BEAM.** Not a reimplementation — the actual ERTS, cross-compiled for Tyn's host interface.

**Purpose-built for BEAM.** The kernel hosts one runtime and nothing else. This constraint enables a small trusted computing base and a clean verification story.

**Minimal kernel, maximal BEAM.** The kernel provides only what BEAM needs — memory, interrupts, device access, network. BEAM handles its own scheduling, memory management, code loading, and supervision.

**Target KVM/virtio.** Standardized virtual hardware means the kernel only needs a handful of drivers. The entire device layer is a few hundred lines of Rust.

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
