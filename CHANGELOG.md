# Changelog

Notable changes to Tyn, newest first. Dates are approximate.

## Unreleased

- **One-command deploy.** `tyn deploy` takes a Mix release to a running Nitro instance — packs it, builds/imports/registers the AMI (content-hashed, so unchanged redeploys skip the ~10-minute import), launches, injects config/secrets via `--env`, and reports the IP. `tyn iam-policy` prints the required IAM, and a preflight fails early on missing permissions. Replaces the previous multi-step manual flow.
- **HTTPS bulk bodies.** Fixed inbound TLS dropping response bodies over ~64 KB (the transport now chunks and drains rather than writing the whole body into rustls' send buffer at once). Byte-exact to 50 MB.
- **BEAM confined in ring 3.** The BEAM now runs as ring-3 userland with the kernel in ring 0 behind a hardware boundary (US/NX pages, SMEP/SMAP, pointer-checked syscalls). A memory-safety bug in the BEAM or a NIF can't reach the kernel — proven by violation on 16-vCPU Nitro (a deliberate BEAM write to a kernel address is denied and contained; the confused-deputy case is rejected). The old BUG-1 SMP red-zone race dissolved out of the rework. The TCB is now enforced, not just small.

## Earlier

- **In-guest TLS, both directions.** HTTPS terminates *and* originates inside the guest, no plaintext hop. Inbound via a rustls NIF behind a ThousandIsland transport (config-only wire-in); outbound via OTP's own `:ssl` backed by Tyn's RustCrypto NIF (ECDHE + ECDSA/RSA-PSS/Ed25519 verify) and a pure-Erlang asn1 shim. TLS 1.3, cert-verified, byte-exact on Nitro. (Unreviewed; TLS 1.2/mTLS not yet done.)
- **Basic distributed Erlang.** Two nodes cluster and stay connected on Nitro — `connect_node`, `rpc`, large-term transfer byte-exact, tick/`nodedown`. Static host mapping (no in-guest `.internal` DNS); a fixed pair, not turnkey multi-node.
- **Real wall clock.** `CLOCK_REALTIME` is kvmclock-backed on Nitro/KVM (nanosecond, host-drift-corrected), RTC-seeded elsewhere. No longer stuck at 1970.
- **Deploy your own app.** `tyn-pack` turns a Mix release into a bootable image — no Rust or kernel rebuild needed. A stock `mix phx.new` app runs unmodified.
- **`:crypto` NIF.** From-scratch Rust (RustCrypto primitives) fed by a kernel CSPRNG, statically linked into ERTS. Enables the full Phoenix stack (signed sessions, CSRF, tokens). (Unreviewed.)
- **Full Phoenix stack.** Stock `mix phx.new` — static assets, interactive LiveView, `runtime.exs`, signed cookies — serving on real AWS Nitro over a from-scratch ENA driver.
- **AWS Nitro.** From-scratch ENA driver; boots from a GRUB/multiboot image imported as an EBS snapshot.
- **8-way SMP, BeamAsm JIT** on unmodified OTP 27 / Elixir 1.18.3.
