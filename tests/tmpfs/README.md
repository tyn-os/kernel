# tmpfs regression fixture

Validates the in-memory writable filesystem (`src/tmpfs.rs`) mounted at `/tmp`
and `/dev/shm`. Two layers, both **behavior-based and byte-exact** (the house
rule — assert bytes/size/effect, never a status code):

1. **Syscall self-test** (`probe_a.ex`) — runs at boot inside a real BEAM node
   and prints `PB_…` lines. Exercises the full contract: `System.tmp_dir`,
   write→read byte-exact (small + 2 MiB), the write-temp-then-rename upload
   pattern, `mkdir`/`ls`/`rm`, N concurrent hash-checked writes, and the ENOSPC
   ceiling (over-cap write returns `{:error, :enospc}` without crashing).
2. **Real HTTP multipart upload** (`upload_router.ex`) — a Bandit + Plug
   endpoint whose `Plug.Parsers` multipart writes each part to a temp file that
   `Plug.Upload.random_file!/1` creates in `System.tmp_dir` (i.e. tmpfs). The
   handler reads that file back and returns its `sha256`; the driver compares it
   against the bytes it sent. This is the literal headline capability (row 2).

## Files

| File | Role |
|---|---|
| `probe_a.ex` | `ProbeA.run_probe/0` — the `PB_` syscall self-test |
| `upload_router.ex` | `ProbeA.UploadRouter` — Plug multipart `/upload` + `/health` |
| `application.ex` | supervision tree: Bandit(:8080) + the probe Task (no Repo) |
| `drive_upload.sh` | boots the disk in QEMU, runs the self-test + uploads, asserts |

App `mix.exs` deps for the fixture:
`{:bandit, "~> 1.5"}, {:plug, "~> 1.16"}` (plus whatever the base app already has).

## Reproduce (build host with the toolchain)

```sh
# 1. build the kernel (any host): cargo build --release --target x86_64-tyn.json ...
# 2. on the build host, pack the app and build a disk:
cd <kernel>            # base cpio is CWD-relative
./tyn-pack <rel>/probe_a -o work/tmpfs-probe.cpio
KERNEL=work/tyn-kernel-tmpfs bash work/mkdisk.sh work/tmpfs-probe.cpio work/disk_tmpfs.raw
# 3. boot + drive:
bash tests/tmpfs/drive_upload.sh     # edit the disk path at the top if needed
```

## Expected — self-test (every line as shown; all green)

```
[tmpfs] mounted /tmp and /dev/shm (cap 4096 KiB)
PB_BEGIN
PB tmp_dir: "/tmp"                       # was nil on the read-only VFS
PB ls_empty_at_boot: []                  # volatile — empty every boot
PB write_read_exact: true                # small write→read byte-exact
PB large_file_exact: {true, 2097152}     # 2 MiB byte-exact + correct size
PB rename_pattern: {true, true}          # upload temp→rename→read exact, source gone
PB mkdir_ls: ["f.txt"]                    # mkdir + write into subdir + ls
PB rm_gone: false                        # File.exists? == false after rm (i.e. gone)
PB concurrent_hash: "8/8"                # 8 concurrent distinct hash-checked writes
PB enospc_clean: {:error, :enospc}       # over-cap (6 MiB > 4 MiB) → ENOSPC, no crash
PB node_alive_after_enospc: true
PB_END node_alive=true
```

## Expected — HTTP multipart (byte-exact sha256 round-trip)

```
UPLOAD small bytes=4096   BYTE_EXACT=YES
UPLOAD conc1..conc6 131072 BYTE_EXACT=YES   # ×6 concurrent
UPLOAD 256KiB             BYTE_EXACT=YES
UPLOAD large (≥1 MiB)     http=400          # KNOWN wall — NOT tmpfs (see below)
```

**Known wall — large multipart (capability-map row 2b, not tmpfs).** Multipart parts ≥~1 MiB return
HTTP 400 from `Plug.Parsers` *before any tmpfs write*. The `/raw` endpoint is the discriminator:

```
RAW 262144 / 1048576 / 3145728  http=200 BYTE_EXACT=YES   # raw inbound body fine to 3 MiB
MULTIPART 262144                http=200 BYTE_EXACT=YES
MULTIPART 1048576               http=400                  # only the multipart path fails
```

So the wall is neither the socket layer (raw 3 MiB byte-exact) nor tmpfs (self-test stores a 2 MiB
file + 8 concurrent writers byte-exact) — it is isolated to the Plug/Bandit multipart parser and is
tracked separately. This fixture asserts tmpfs + small uploads; the large-multipart line documents the
open wall so the regression does not silently imply the large-upload user-story is closed.

## Notes

- `==` (Elixir value comparison) and `sha256` are the byte-exact instruments.
  `erlang:md5` is **not** used here — it is intermittently non-deterministic for
  large binaries on Tyn (see `docs/DIST_ACCEPT_HUNT.md`).
- tmpfs is pure in-kernel memory (no hardware/timing coupling), so a QEMU/TCG
  boot is a faithful confirmation; behavior is identical on bare-metal Nitro.
- Cap is 4 MiB (`CAP` in `src/tmpfs.rs`), shared out of the 16 MiB kernel heap.

## Layer-2 addition: cap under concurrent writers (`probe_cap.ex`)

`probe_cap.ex` (`CapProbe.run/0`) is the Phase-2 **Layer-2** adversarial test of
the 4 MiB byte-cap under *concurrent* writers racing the boundary — the in-situ
counterpart to the Layer-1 `src/tmpfs_tree.rs::grant_write` unit test (which tests
the arithmetic on a slice; this tests the live node store under real load). It
races 16 writers × 512 KiB (8 MiB demand) at the 4 MiB cap and prints `CC_` lines
asserted three-part (teeth / clean-handling / no-corruption / recovery).

- `build_cap_app.sh` — assembles a minimal no-dep app that runs `CapProbe` at boot.
- `drive_cap_concurrency.sh` — packs+boots it on TCG and asserts the `CC_` verdict.

Validated on TCG **and** on real 4-core Nitro SMP (8 ok / 8 enospc, total exactly
4 MiB, no interleave, recovery — the coarse-Mutex invariant holds under genuine
parallelism).
