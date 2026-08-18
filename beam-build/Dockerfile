# Reproducible build of Tyn's beam.smp: a fully static, musl-libc, BeamAsm-JIT
# Erlang/OTP emulator. This exists because the original beam.smp was a committed
# binary blob with no reproducible build — when the old build box was terminated
# we lost the ability to patch OTP, rebuild the JIT/no-JIT variants, or enable
# static NIFs. This Dockerfile pins every input so the emulator is reproducible
# from git.
#
# Alpine is musl-native, so its g++ builds the C++17 BeamAsm JIT (asmjit) against
# musl with no cross-toolchain. The result is a static x86_64 musl binary matching
# what the original build produced (OTP 27.3.4.2 / ERTS 15.2.7.1, -static, JIT).
#
# Build:  docker build -t tyn-beam --build-arg STATIC_NIFS=<a.a,...> .
# Extract: the emulator is copied to /out/beam.smp (stripped).

# --- pinned inputs ---
# Alpine 3.19 pins GCC 13.2 + musl 1.2.4 to match the ORIGINAL beam.smp toolchain
# (Ubuntu GCC 13.3 / musl 1.2.4). GCC 14 (Alpine 3.21) measurably worsened the
# cold-boot futex-stall reliability under TCG -smp 1 — the emulator's scheduler
# timing is codegen-sensitive, so matching the compiler major version matters.
ARG ALPINE_VERSION=3.19
FROM alpine:${ALPINE_VERSION}

ARG OTP_VERSION=27.3.4.2
# Space-separated NIF module names to compile from nifs/<name>.c and link
# statically via --enable-static-nifs. Empty = baseline (no NIFs). The static-
# NIF spike sets e.g. NIF_MODULES=tyn_nif.
ARG NIF_MODULES=
# ERTS target triple (OTP normalizes to this even on Alpine).
ARG TARGET=x86_64-pc-linux-musl

RUN apk add --no-cache \
      build-base perl perl-utils autoconf automake \
      ncurses-dev ncurses-static \
      zlib-dev zlib-static \
      linux-headers bash file

WORKDIR /build
RUN wget -O otp.tar.gz \
      "https://github.com/erlang/otp/releases/download/OTP-${OTP_VERSION}/otp_src_${OTP_VERSION}.tar.gz" \
 && tar xzf otp.tar.gz && rm otp.tar.gz

# Any static-NIF archives get copied in and passed to --enable-static-nifs.
COPY nifs/ /build/nifs/

WORKDIR /build/otp_src_${OTP_VERSION}

# Fully static (-static) musl build with the BeamAsm JIT enabled and no SSL
# (Tyn has no OpenSSL — that's the whole reason for the crypto NIF work). Trim
# GUI/optional apps that we never ship and that only slow the build.
ENV CFLAGS="-O2" \
    CXXFLAGS="-O2 -std=c++17" \
    # -z muldefs: two independent Rust static NIF archives (crypto = no_std,
    # tyn_tls = std) each bundle the Rust runtime lang-items (rust_begin_unwind,
    # __rust_alloc_error_handler, the __rust_alloc allocator shims). Linking both
    # is a legit multiple-definition. muldefs takes the FIRST archive's copy; with
    # crypto.a ordered first (NIF_MODULES="crypto tyn_tls") its COMPLETE EnifAlloc
    # allocator set + panic handler win uniformly, so there is no split-allocator
    # hazard and rustls's heap routes through enif_alloc (ERTS-accounted). Proper
    # fix later: merge both NIFs into one crate (one runtime) — the ":crypto
    # unify onto one primitive layer" roadmap item.
    LDFLAGS="-static -Wl,-z,muldefs"
# Baseline configure flags (kept identical whether or not we add static NIFs, so
# the only variable in the spike is the NIF linkage — the bisection the plan asks
# for). --without-ssl: Tyn has no OpenSSL (the reason for the crypto NIF work).
ENV CFG="--enable-jit --without-ssl --disable-dynamic-ssl-lib --without-wx --without-observer --without-debugger --without-et --without-megaco --without-odbc --without-jinterface"
RUN set -eux; \
    STATIC_NIF_ARG=""; \
    if [ -n "${NIF_MODULES}" ]; then \
      # 1. configure once to generate the *_config.h headers the NIF needs.
      ./configure ${CFG}; \
      # 2. compile each NIF (-DSTATIC_ERLANG_NIF -> static init symbol) and
      #    archive it for --enable-static-nifs.
      INCS="-Ierts/emulator/beam -Ierts/include -Ierts/include/${TARGET} -Ierts/emulator/${TARGET} -Ierts/${TARGET}"; \
      NIF_LIST=""; \
      for m in ${NIF_MODULES}; do \
        # OTP derives the expected static-init symbol from the ARCHIVE basename
        # (<name>.a -> <name>_nif_init), matching the symbol ERL_NIF_INIT emits
        # from the module name. So archives are named after the module, NO lib
        # prefix: tyn_nif.a -> tyn_nif_nif_init, found by load_nif("tyn_nif").
        # A C NIF (nifs/<m>.c) is compiled here; a Rust NIF ships a prebuilt
        # nifs/<m>.a (cargo staticlib) which we use as-is.
        if [ -f /build/nifs/${m}.c ]; then \
          gcc -O2 -fno-common -DSTATIC_ERLANG_NIF ${INCS} \
              -c /build/nifs/${m}.c -o /build/nifs/${m}.o; \
          ar rcs /build/nifs/${m}.a /build/nifs/${m}.o; \
        elif [ ! -f /build/nifs/${m}.a ]; then \
          echo "FATAL: no /build/nifs/${m}.c or ${m}.a"; exit 1; \
        fi; \
        # SPACE-separated, not comma: OTP's Makefile.in does
        # `$(subst $(comma),$(space),STATIC_NIFS)` but $(space) expands empty in
        # this context, concatenating multiple archives into one bogus path
        # (crypto.a,tyn_tls.a -> crypto.atyn_tls.a). Passing spaces up front makes
        # that subst a no-op. Single-NIF is unaffected (no separator).
        NIF_LIST="${NIF_LIST:+${NIF_LIST} }/build/nifs/${m}.a"; \
      done; \
      STATIC_NIF_ARG="--enable-static-nifs=${NIF_LIST}"; \
    fi; \
    # 3. (re)configure with the static-NIF archive, then build the emulator.
    # Quote STATIC_NIF_ARG so a multi-archive (space-separated) list stays ONE
    # configure argument; skip it entirely when no NIFs (baseline build).
    if [ -n "${STATIC_NIF_ARG}" ]; then ./configure ${CFG} "${STATIC_NIF_ARG}"; else ./configure ${CFG}; fi; \
    make -j"$(nproc)" emulator \
      || echo "NOTE: 'make emulator' returned nonzero — the erts/etc helper" \
              "binaries (erlexec, epmd, ...) don't link under -static, but Tyn" \
              "loads beam.smp directly and gets erl_child_setup from the cpio." \
              "The staging step below fails hard if beam.smp itself is missing."

# Locate + stage the emulator. BeamAsm builds beam.smp (the SMP+JIT emulator).
RUN set -eux; \
    mkdir -p /out; \
    BEAM="$(find . -type f -name beam.smp | head -1)"; \
    test -n "$BEAM" || { echo "FATAL: beam.smp was not built"; exit 1; }; \
    cp "$BEAM" /out/beam.smp.full; \
    cp "$BEAM" /out/beam.smp; \
    strip /out/beam.smp; \
    file /out/beam.smp; \
    ls -l /out
