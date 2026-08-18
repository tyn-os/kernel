# In-guest inbound TLS — home scoping + runtime gate (decision record)

Status: **decision pending (the wedge choice, user's).** This note is the record the decision rests on.
Uncommitted working doc. No kernel code committed for TLS yet.

## What is settled

### Step 0 (corrected): compile-only
rustls 0.23.43 + rustls-rustcrypto (RustCrypto primitives) + a custom getrandom backend + an explicit
`TimeProvider` **cross-compile** to `no_std` bare-metal `x86_64-tyn`, clean (only the kernel's own
soft-float warning). Pure Rust — no ring/aws-lc/openssl/libc/cc/std on the path.
**Earlier overstatements retracted:** no handshake had executed, no RcCsprng→RDRAND runtime wiring, and
std-on-musl was untouched. "It compiles" ≠ "it runs."

### Step A: integration point (settled, shared by both homes)
A **custom `ThousandIsland.Transport` backed by rustls** — the `gen_tcp`-level shim, **not** backing
Erlang's `:ssl`. Read, not inferred:
- Bandit HTTPS → `ThousandIsland.Transports.SSL`, one impl of the ~20-callback `ThousandIsland.Transport`
  behaviour; TLS lives only in `handshake/1` (`:ssl.handshake`), `recv`/`send`, `secure?`, and cert/key
  opts on `listen`. The TCP transport **no-ops `handshake/1`** — proof the behaviour cleanly quarantines
  "is there TLS" into swappable transport code.
- `bandit.ex` uses `Keyword.put_new(:transport_module, …)` → a user-supplied `transport_module` **wins**.
  Wire-in is **config-only**: `scheme: :https` + `thousand_island_options: [transport_module:
  Tyn.Transports.RustlsTLS, transport_options: [certfile:, keyfile:]]`. Zero Bandit/app code changes.
- Backing `:ssl` (option a) = reimplementing the `:ssl` OTP app (the thing this transport wraps). Rejected.

This seam is **identical for both homes** — the transport's `handshake/recv/send` either call a NIF
(Path 1) or route to kernel syscalls (Path 2). It is not what the remaining fork is about.

## Step 0.5 — the runtime gate: rustls EXECUTES (compile → run), on the real CSPRNG

An in-memory rustls loopback (a `ClientConnection`↔`ServerConnection` driven with no OS sockets), with
rustls's randomness routed through the getrandom **custom backend into a ChaCha20 CSPRNG** — the exact
construction of `src/rng.rs` (RDSEED→ChaCha20). Ed25519 self-signed cert; the client genuinely verifies
the server's handshake signature via the RustCrypto verifiers.

| Variant | Result |
|---|---|
| local aarch64 (std, test seed) | **PASS** — TLS 1.3, `TLS13_AES_128_GCM_SHA256`, app data byte-exact both ways |
| x86_64-gnu, **real RDSEED→ChaCha20** | **PASS** — same, on the actual Tyn CSPRNG construction |
| x86_64-**musl static** (`statically linked`, static-pie) | **PASS** — builds **and runs** |

Executed crypto path: X25519 KX, Ed25519 sign+verify, AES-128-GCM AEAD, HKDF-SHA256 transcript.

**Answers to the gate's questions:**
- **Does rustls execute a handshake?** Yes — three runtimes, TLS 1.3 completed + record layer byte-exact.
- **What runtime did it need?** A std host runtime with an in-memory loopback. No sockets, no OS TLS.
- **Is std-on-musl proven?** **Yes at the musl-userspace level** (the static musl binary ran). Residual:
  running that same musl NIF **under Tyn's syscall shim** inside `beam.smp` is not yet executed — but the
  syscalls std needs are enumerable and already serviced (`clock_gettime`/kvmclock, `futex`/threads,
  `mmap`/`brk`, and `getrandom` is a handled syscall — plus we force the custom backend→CSPRNG, removing
  the getrandom-syscall dependency entirely). Small, enumerated residual.
- **RNG on the real CSPRNG?** Yes — RDSEED→ChaCha20, not raw RDRAND. **Review-first**: TLS consumes
  randomness; RNG failure is catastrophic and test-vector-invisible. The CSPRNG feeding rustls is the #1
  thing to get external review on, whatever else.

## The home fork — and the finding that reframes it

| | Path 1 — NIF in BEAM (musl+std) | Path 2 — kernel termination (`x86_64-tyn`) |
|---|---|---|
| Foundation | std-on-musl **userspace proven**; under-Tyn-shim residual (small) | `no_std` compile proven; in-kernel runtime not yet executed |
| Effort / risk | medium / low | high / medium-high |
| Cost | loses kernel `sendfile` zero-copy (encrypt in NIF) | **ring-0 TLS parser** (grows attacker-reachable ring-0 code the BUG-8/fuzz arc was shrinking); rustls session state (~tens of KB/conn) on the **BUG-8-pressured kernel heap** |

### The ring-0 surface question — answered explicitly (and it cuts against Path 2's premise)
The wedge framing assumes Path 2 buys a **small safe TCB** / plaintext-in-small-TCB. **In Tyn's current
architecture it does not** — from Tyn's own ARCHITECTURE.md "Honest limits":

> "Ring-0 everything. No user/kernel privilege separation; BEAM and kernel share ring 0. A memory-safety
> bug in either is unconfined."

BEAM is identity-mapped, ring 0, in the **same 4 GiB address space** as the kernel. So:
- **Neither path has an *enforced* TCB boundary.** rustls-in-NIF and rustls-in-kernel both land in the
  same unconfined ring-0 domain; a bug in either reaches everything. Path 2's "plaintext lives in kernel
  memory, not BEAM memory" is **illusory** when they share one identity-mapped address space.
- Path 2's advantage over Path 1 is therefore **organizational** (smaller codebase to audit, cleaner
  separation), **not a hardening boundary**. Its costs (ring-0 attack surface, kernel-heap pressure) are
  real; its marginal *security* gain over Path 1 today is mostly not.
- **The small-safe-TCB wedge claim is not currently true for *either* path.** It requires **BEAM
  isolation** (ring 3, or separate page tables) — a separate, larger architectural bet. TLS-in-kernel is
  **downstream of that, not a substitute for it.**

### What IS true for both paths
**In-guest termination, no plaintext on the wire** — TLS terminates inside the guest, no edge/ALB box,
no plaintext hop to an external terminator. That is a genuine differentiator vs edge-termination, and
**Path 1 delivers it at far lower risk.**

## Recommendation (the decision is the user's — this is the steer)
1. **Ship the capability via Path 1 (NIF).** It delivers the real, defensible story (in-guest TLS, no
   plaintext on the wire) on a foundation now proven to *run*, at low risk, reusing the Step-0 crate set.
2. **Treat "small safe TCB" as its own arc = BEAM isolation**, not as something TLS-in-kernel manufactures.
   Path 2 becomes worth its ring-0 cost **only once the kernel is actually a confined, smaller TCB** (i.e.
   after BEAM is pushed out of ring-0 / its own address space). Until then Path 2 is a higher-cost lateral
   move, not the purity destination.
3. This dissolves the "don't build Path 1 as a reluctant stepping stone" worry: Path 1 is **not** a
   stepping stone to a purity Path 2 already has — Path 2 doesn't have it yet either. Both are
   "no-plaintext-on-wire"; pick the cheaper one to ship, and sequence the TCB-purity work explicitly.
4. **RNG review-first** regardless of path — the RDSEED→ChaCha20 CSPRNG feeding rustls is the #1 review
   target. Mark the whole crypto surface "unreviewed pending audit" when TLS lands.

**If the intended product is specifically "TLS in a confined small safe-Rust TCB"**, the honest reading
is: that needs the isolation arc first; choosing Path 2 now buys the ring-0 cost without the purity payoff.
