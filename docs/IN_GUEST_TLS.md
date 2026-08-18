# In-guest inbound TLS (rustls + pure-Rust crypto)

**Status: proven on real Nitro.** A real external `curl --tlsv1.3` terminates HTTPS
*inside the Tyn guest* via rustls — no edge/ALB terminator, **zero plaintext hop to
the workload**. This is the security-wedge (small-safe-Rust-TCB) inbound capability.

> **UNREVIEWED pending audit — RNG review-first.** The whole crypto surface (rustls +
> the RustCrypto provider) is pure-Rust but not yet externally reviewed. The
> RDSEED→ChaCha20 CSPRNG feeding rustls is the #1 review target: TLS consumes
> randomness, and RNG failure is catastrophic *and* test-vector-invisible.
>
> This does **not** yet deliver a *confined* small TCB — BEAM and kernel share ring 0
> (see ARCHITECTURE.md "Honest limits"). "In-guest, no plaintext on the wire, small
> auditable codebase" is true now; "confined small TCB" needs the BEAM-isolation arc.

## Architecture (Path 1 — NIF in BEAM, sans-IO)

```
external client ──TLS──▶ kernel TCP socket (smoltcp) ──ciphertext──▶ BEAM
                                                                       │  gen_tcp
                                          Tyn.Transports.RustlsTLS ◀───┘  (owns socket)
                                                     │ feed/pull_send/read_plain/write_plain
                                              tyn_tls NIF (rustls, static in beam.smp)
                                                     │ plaintext
                                                  Bandit / Plug / app
```

- **`tyn_tls` NIF** (`beam-build/tls-nif/`): rustls 0.23 + `rustls-rustcrypto` provider,
  **sans-IO** — BEAM owns the kernel TCP socket; the NIF only transforms bytes. Hand-wired
  erl_nif (mirrors `crypto-nif`), `std`, sessions in a handle table.
  - **RNG**: getrandom custom backend → an RDSEED→ChaCha20 CSPRNG (the `src/rng.rs`
    construction). Not raw RDRAND, not the getrandom syscall.
  - **Time**: rustls's std `TimeProvider` → `clock_gettime` → kvmclock `CLOCK_REALTIME`.
- **`Tyn.Transports.RustlsTLS`** (`examples/tls/`): a `ThousandIsland.Transport`
  implementation. Wire-in is **config-only** — `scheme: :https` +
  `thousand_island_options: [transport_module: Tyn.Transports.RustlsTLS]` — zero Bandit
  or app code changes (Bandit's `put_new` lets a user transport_module win).

## Build (combined beam)

The shipped `src/beam.smp.elf` is now built with **both** NIFs:

```
# build crypto.a and tyn_tls.a (musl staticlibs), place in beam-build/nifs/, then:
./beam-build/build-beam.sh --nif-modules "crypto tyn_tls"
```

Two multi-NIF-path fixes were required in `beam-build/Dockerfile` (this path had only
ever been exercised with a single NIF):
1. **`--enable-static-nifs` list is space-separated, not comma** — OTP's
   `$(subst $(comma),$(space),…)` expands `$(space)` empty here and concatenates the
   archive paths.
2. **`-Wl,-z,muldefs`** — the two Rust staticlibs (`crypto` = no_std, `tyn_tls` = std)
   each bundle the Rust runtime lang-items (`rust_begin_unwind`, `__rust_alloc*`). With
   `crypto.a` ordered first, its complete EnifAlloc allocator + panic handler win
   uniformly (no split-allocator hazard; rustls's heap routes through `enif_alloc`).
   Runtime-validated: both NIFs exercised together in one beam (crypto op + a full
   handshake) with no fault. Proper follow-up: merge both NIFs into one crate.

## Key injection (never in the public AMI)

Cert + key are injected via boot-config, **not** hardcoded/baked into app source:

```
tyn-pack <release> --base src/otp-rootfs.cpio -o app.cpio \
  --env TYN_TLS_CERT_B64=$(base64 -w0 cert.pem) \
  --env TYN_TLS_KEY_B64=$(base64 -w0 key.pem)
```

`tyn_boot` `os:putenv`s these before `runtime.exs`, so the app reads them at start
(see `examples/tls/application.ex`), writes to tmpfs, and points `certfile`/`keyfile`
there. v1 uses a self-signed cert; key **rotation** and the real secrets story are the
orchestration-layer follow-on.

## Nitro validation (this capability)

- HTTPS terminates in-guest, byte-exact response, **TLS 1.3** (`TLS_AES_256_GCM_SHA384 /
  X25519 / ED25519`), zero plaintext hop.
- HTTP plaintext path un-regressed (TLS is additive).
- **Zero `[syscall] UNHANDLED`** on serial — every std syscall the NIF makes is serviced
  on the real target under real traffic (`src/syscall.rs` makes UNHANDLED loud post-boot).
- rustls session state (~tens of KB/conn) lives in BEAM heap — isolated from the BUG-8
  kernel-heap ceiling (a Path-1 property).

## Crate list + audit status (first cut)

rustls (Cure53-audited core; the provider is the new part) · curve25519/ed25519/x25519-dalek
(best-scrutinized) · aes-gcm/chacha20poly1305 (broad use, uneven audit) · p256/p384/ecdsa
(field arithmetic, higher-risk) · rsa 0.10-rc (release-candidate; Marvin advisory N/A to a
TLS-1.3 *signing* server) · sha2/hmac/hkdf · getrandom (thin — the CSPRNG is the review
target). Overlaps the shipped `:crypto` NIF at newer versions → the "unify onto one
primitive layer" follow-up.

## Follow-ups
- Outbound TLS (client-side rustls + embedded CA bundle).
- Merge `crypto` + `tyn_tls` into one crate (one runtime; retires the muldefs hack) =
  the ":crypto unify onto one primitive layer" item.
- mTLS / client-cert verify (zero-trust-complete inbound).
- BEAM/kernel isolation — the load-bearing prerequisite for the *confined* small-TCB claim.
