# Tyn

A minimal Rust microkernel purpose-built for BEAM.

No Linux. No POSIX. Just your Erlang/Elixir/Gleam code on bare metal.

## What is Tyn?

Tyn is a unikernel — a single-purpose operating system kernel that hosts one thing: the BEAM virtual machine. It replaces the entire Linux stack with ~6,300 lines of Rust, targeting KVM/QEMU cloud deployments.

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
│  Tyn Kernel (Rust, ~6,300 LOC)          │
│  SMP · Memory · Networking · VFS · I/O  │
├─────────────────────────────────────────┤
│  KVM / QEMU / Cloud Hypervisor          │
└─────────────────────────────────────────┘
```

Tyn runs the real, unmodified ERTS/BEAM — not a reimplementation. When OTP ships a new version, it should just work. This is the critical lesson from [LING](https://github.com/cloudozer/ling) (Erlang on Xen), which died because it reimplemented the VM and couldn't keep pace with upstream changes.

## Status

**OTP 27 BEAM running on bare metal with SMP, TCP, **Bandit + Plug**, and Elixir.**

```elixir
defmodule HelloPlug do
  import Plug.Conn
  def init(opts), do: opts
  def call(conn, _opts) do
    conn
    |> put_resp_content_type("text/plain")
    |> send_resp(200, "Hello from Bandit on Tyn!\n")
  end
end

{:ok, _} = Bandit.start_link(plug: HelloPlug, port: 8080)
```

```
$ curl http://localhost:5566/
Hello from Bandit on Tyn!
```

- OTP 27 ERTS boots with up to 8 CPUs, loads 150+ .beam files from in-memory VFS
- Full OTP kernel application starts — supervision trees, code_server, logger
- **Bandit** runs unmodified on top of **ThousandIsland** with the default `num_acceptors: 100` configuration — the full `DynamicSupervisor` → `Connection.start` → handler-spawn chain works
- **Plug pipeline**: `Plug.Conn` → `put_resp_content_type` → `send_resp` → host gets the response
- Elixir 1.18.3 runs: `IO.puts`, `System.version`, `Kernel.inspect` all work
- Reliability: from a clean clone, 3/3 boot+curl trials succeed end-to-end (boot reaches `bandit_listening` within 7–15s on KVM, then Bandit serves curl). Sequential request handling within a single boot is rock-solid (5/5).

### What works

- **8-way SMP** — ACPI/MADT CPU discovery, APIC timer calibration, AP trampoline (16→64 bit), per-CPU GDT/TSS/IST, GS_BASE per-CPU syscall data, IPI wakeup, preemptive user-mode scheduling
- **TCP networking** — `gen_tcp:listen/accept/send/close` end-to-end, POSIX socket layer → smoltcp TCP/IP → virtio-net PCI → QEMU → host
- **Elixir** — Elixir 1.18.3 .beam files load and execute on OTP 27
- **~50 Linux syscalls** — mmap, read, write, open, stat, pipe, ppoll, futex, clone, epoll, select, readv, ...
- **VFS** — cpio newc archive with OTP kernel/stdlib .beam files + optional Elixir
- **Boot** — Multiboot1, identity-mapped 4 GiB, ELF loader for static musl binaries
- **Threading** — up to 16 CPUs, per-thread kernel stacks, atomic futex, preemptive + deferred scheduling
- **I/O** — COM1 serial (stdin/stdout/stderr), PCI ECAM, virtio-net

### ERTS build configuration

ERTS is built from unmodified OTP 27 source — no patches, no special defines. The only non-default configure flags are `--disable-jit` and `--without-*` for unused applications.

Tyn uses a hybrid futex strategy:

- **During ERTS init** (~first 2 seconds): `futex_wait` returns immediately (spin-yield) to avoid a thread-progress registration deadlock where blocked threads prevent other threads from registering with the progress system.
- **After init**: `futex_wait` blocks properly — threads sleep and consume zero CPU until woken by `futex_wake`. Idle CPUs enter HLT.

The switch happens automatically after ERTS finishes loading boot modules. Normal operation uses real blocking semantics with proper sleep/wake.

### What's next

- **Phoenix** — Bandit + Plug works; Phoenix on top should "just work" subject to compiling its dependency tree against the same OTP/Elixir as the cpio
- **Concurrent-burst load** — sequential request handling is rock-solid; under a 5-simultaneous-curl burst, 2/5 succeed and 3 get `Connection reset by peer`. This is a smoltcp backpressure / connection-sup limit, not a kernel bug — see [MESSAGE_DELIVERY.md §B2.22](MESSAGE_DELIVERY.md)
- **BEAM JIT** — BeamAsm support (requires IST-safe preemption for clone child stacks)
- **Interactive shell** — IEx/Erlang shell with full stdin support

## Building & Running

### Prerequisites

- Rust nightly toolchain with `rust-src` component
- QEMU with KVM support (`qemu-system-x86_64`)
- A statically-linked `beam.smp` and the OTP/Elixir rootfs cpio. **Both are committed at `src/beam.smp.elf` and `src/otp-rootfs.cpio` so the kernel builds out of the box.** To rebuild them yourself, see "Building ERTS + VFS" below.

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

### Test the Bandit demo from host

```bash
# In another terminal while QEMU is running, after Tyn prints
# "bandit_listening" on the serial console (~12s after boot):
curl http://localhost:5555/
# → Hello from Bandit on Tyn!
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

## Architecture Diagrams

- [Module structure](docs/module-structure.md) — source file dependencies and line counts
- [Boot flow](docs/boot-flow.md) — from power-on to Erlang shell, syscall sequence
- [Runtime architecture](docs/runtime-arch.md) — CPU layout, futex strategy, memory map

## Investigation logs

These document the bug-class hunts that got Tyn from "ERTS boots" to "Bandit + Plug serves real traffic":

- [BOOT_RELIABILITY.md](BOOT_RELIABILITY.md) — failure modes, stack-layout trace through preemption + syscall, what fixes worked and why
- [MESSAGE_DELIVERY.md](MESSAGE_DELIVERY.md) — scheduler-wake / process-scheduling races, the watchdog-rescue fix, and the `sys_accept` race that was blocking ThousandIsland's concurrent-acceptor pattern (now fixed)

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
