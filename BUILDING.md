# Building ERTS for Tyn

Tyn embeds a statically-linked BEAM binary and OTP root filesystem (cpio) directly into the kernel image. This document describes how to reproduce both.

## OTP 20 Non-SMP (current — single-threaded, no threading issues)

### Prerequisites

- Ubuntu 24.04 or similar (x86_64)
- `musl-tools` (`sudo apt install musl-tools`)
- `autoconf`, `m4`, `gcc`, `make`, `libncurses-dev`

### Build beam binary

```bash
# Clone OTP 20.3 source
git clone --depth 1 --branch OTP-20.3.8.26 https://github.com/erlang/otp.git otp20
cd otp20

# Generate configure scripts
./otp_build autoconf

# Patch: skip fork of erl_child_setup (unikernel has no fork/exec)
sed -i 's/    i = fork();/    i = 1; \/\* TYN: skip fork *\//' erts/emulator/sys/unix/sys_drivers.c
sed -i 's/close(fds\[1\]);/\/\* TYN: keep pipe open \*\//' erts/emulator/sys/unix/sys_drivers.c

# Configure: non-SMP, no threads, static musl
cd erts
ERL_TOP=$(dirname $PWD) CC=musl-gcc CFLAGS='-O2 -static -fcommon' LDFLAGS='-static' \
  ./configure --disable-smp-support --disable-threads --disable-hipe --without-termcap
cd ..

# Build emulator only
ERL_TOP=$PWD make -j$(nproc) emulator

# Strip
strip bin/x86_64-unknown-linux-gnu/beam -o beam.elf
# Result: ~2.7 MB static musl binary
```

### Build OTP root filesystem (cpio)

The BEAM needs `start.boot` and kernel/stdlib `.beam` files. These come from the OTP source tarball which includes precompiled `.beam` files:

```bash
# Download source tarball (has precompiled .beam files)
wget https://erlang.org/download/otp_src_20.3.tar.gz
tar xzf otp_src_20.3.tar.gz

# Create OTP root structure
mkdir -p otp-root/bin otp-root/erts-9.3/bin \
         otp-root/lib/kernel/ebin otp-root/lib/stdlib/ebin

cp otp_src_20.3/bootstrap/bin/start.boot otp-root/bin/
cp beam.elf otp-root/erts-9.3/bin/beam
cp otp_src_20.3/lib/kernel/ebin/*.beam otp_src_20.3/lib/kernel/ebin/kernel.app \
   otp-root/lib/kernel/ebin/
cp otp_src_20.3/lib/stdlib/ebin/*.beam otp_src_20.3/lib/stdlib/ebin/stdlib.app \
   otp-root/lib/stdlib/ebin/

# IMPORTANT: directory names must NOT have version suffixes
# (ERTS looks for lib/kernel/ebin/, not lib/kernel-5.4.3/ebin/)

# Create cpio archive
mkdir staging && cp -r otp-root staging/otp
cd staging && find otp -type f | cpio -o -H newc > ../otp-rootfs.cpio
# Result: ~7.8 MB cpio with ~155 files
```

### Install into kernel

```bash
cp beam.elf /path/to/kernel/src/beam.smp.elf
cp otp-rootfs.cpio /path/to/kernel/src/otp-rootfs.cpio
```

The kernel includes these via `include_bytes!()` in `main.rs`.

## OTP 27 SMP (future — requires multi-CPU kernel support)

### Build

```bash
git clone --depth 1 --branch OTP-27.3.4 https://github.com/erlang/otp.git otp27
cd otp27

# Patch spin counts (critical for single/few-CPU operation)
sed -i 's/#define ETHR_MTX_DEFAULT_MAIN_SPINCOUNT_MAX 2000/#define ETHR_MTX_DEFAULT_MAIN_SPINCOUNT_MAX 1/' \
    erts/include/internal/ethr_mutex.h
sed -i 's/#define ETHR_MTX_DEFAULT_MAIN_SPINCOUNT_BASE 800/#define ETHR_MTX_DEFAULT_MAIN_SPINCOUNT_BASE 1/' \
    erts/include/internal/ethr_mutex.h
sed -i 's/#define ETHR_MTX_DEFAULT_MAIN_SPINCOUNT_INC 50/#define ETHR_MTX_DEFAULT_MAIN_SPINCOUNT_INC 0/' \
    erts/include/internal/ethr_mutex.h
sed -i 's/#define ETHR_MTX_DEFAULT_AUX_SPINCOUNT 50/#define ETHR_MTX_DEFAULT_AUX_SPINCOUNT 1/' \
    erts/include/internal/ethr_mutex.h
# (repeat for RWMTX variants and erl_process.c/erl_process_lock.c spin counts)

# Disable monotonic time check (no runtime flag exists)
sed -i 's/#define ERTS_CHECK_MONOTONIC_TIME 1/#define ERTS_CHECK_MONOTONIC_TIME 0/' \
    erts/emulator/beam/erl_time.h
sed -i 's/#ifdef ERTS_CHECK_MONOTONIC_TIME/#if ERTS_CHECK_MONOTONIC_TIME/g' \
    erts/emulator/beam/erl_time_sup.c

# Configure
CC=musl-gcc CFLAGS='-O2 -static' LDFLAGS='-static' \
  ./configure --disable-jit --without-termcap \
    --without-wx --without-odbc --without-javac \
    --without-ssl --without-ssh --without-docs

# Build
make -j$(nproc) emulator
strip bin/x86_64-pc-linux-musl/beam.smp -o beam.smp.elf
# Result: ~8.1 MB static musl binary
```

### Status

The OTP 27 SMP binary boots on real Linux (21 futex calls, opens start.boot, loads .beam files). On Tyn's single-CPU kernel, it deadlocks on mutex contention during the multi-threaded init sequence. This requires multi-CPU support (`-smp 2` in QEMU with per-CPU scheduling) to resolve.

### Runtime flags for SMP

When calling `beam.smp` directly (not via the `erl` script), use `-` prefix flags:

```
beam.smp -S 1:1 -SDcpu 1:1 -SDio 1 -A 1 -- -root /otp -bindir /otp/erts-15.2.7/bin -noshell
```
