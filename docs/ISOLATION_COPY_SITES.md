# Isolation — kernel↔user copy-site draft (Stage 1 static audit)

> **STATUS: STATIC DRAFT — explicitly incomplete.** Produced by *reading* the syscall
> handlers for raw user-pointer dereferences (Isolation Stage 1, ISOLATION_STAGE1_CORRECTED.md).
> Code-reading misses sites; that is exactly why the **Stage-3 ring-3 SMAP fault-hunt
> validates and completes this list** (any site SMAP faults on that is missing here =
> a caught gap; any site here SMAP never faults on = prune as dead). Treat this as the
> starting list to build Stage 2/3 against — **not** the audited-complete boundary.
> The completeness guarantee is earned at Stage 3, not here.

At Stage 3 BEAM moves to ring 3 and its pages become US=1 (attributed now, Stage 1).
Every site below then needs a bounds-checked `copy_from_user` / `copy_to_user` (or an
explicit `stac`/`clac` window) instead of a raw deref, because a raw kernel deref of a
US=1 page faults under SMAP. Direction: **R** = kernel reads user memory, **W** = kernel
writes user memory.

## Direct user-pointer handlers (`src/syscall.rs` unless noted)

| Handler | line | user pointer(s) | R/W | notes |
|---|---|---|---|---|
| `sys_write` | 917 | `buf: *const u8` | R | hot serving path (socket TX) — the SMAP-window cost lands here |
| `sys_read` | 999 | `buf: *mut u8` | W | hot serving path (socket RX) — SMAP-window cost |
| `sys_read_stdin` | 1072 | `buf: *mut u8` | W | |
| `sys_uname` | 1347 | `buf: *mut u8` | W | fills `utsname` |
| `sys_sched_getaffinity` | 1371 | `mask: *mut u8` | W | |
| `sys_clock_getres` | 1387 | `res: *mut u64` | W | |
| `sys_stat` | 1563 | `path_ptr: *const u8`, `buf: *mut u8` | R+W | read path string, write statbuf |
| `sys_fstat` | 1651 | `buf: *mut u8` | W | |
| `sys_newfstatat` | 1689 | `path_ptr: *const u8`, `buf: *mut u8` | R+W | |
| `sys_getrandom` | 1703 | `buf: *mut u8` | W | |
| `sys_pipe` | 1714 | `fds: *mut i32` | W | writes 2 fds |
| `sys_readlink` | 2058 | `path: *const u8`, `buf: *mut u8` | R+W | |
| `sys_clock_gettime` | 2368 | `tp: *mut u64` | W | high-frequency |
| `sys_sendfile` | 2423 | `offset_ptr: *mut u64` | R+W | in/out offset |
| `sys_writev` | 2507 | `iov: *const IoVec` **+ each `v.base`** | R | **double-deref** (see below) |
| `sys_getcwd` | 2543 | `buf: *mut u8` | W | |
| `sys_open`/`openat` | 1439 | path `*const u8` (from `a0`/`a1`) | R | path string, NUL-scan length |
| `sys_epoll_ctl` | 1785 | `event_ptr: u64` | R | reads `epoll_event` |
| `sys_epoll_wait` | 1817 | `events_ptr: u64` | W | writes ready events |
| `sys_ppoll` | 1902 | `fds_ptr`, `timeout_ptr` | R+W | pollfd array (R fd/events, W revents) |
| `sys_futex` | 2018 | `uaddr: u64`, `timeout_ptr` | R(+W) | **futex word polled repeatedly** — hot; W on some ops |
| `readv` path | 621 | each `v.base` | W | **double-deref** |

## Socket handlers (`src/net/socket.rs`)

| Handler | line | user pointer(s) | R/W | notes |
|---|---|---|---|---|
| `sys_connect` | 300 | `addr_ptr: *const u8` | R | reads `sockaddr_in` |
| `sys_setsockopt` | 692 | `optval: *const u8` | R | |
| `sys_getsockopt` | 711 | `optval: *mut u8`, `optlen: *mut u32` | R+W | optlen in/out |
| `sys_sendto` | 766 | `buf: *const u8` (+ addr) | R | socket TX |
| `sys_recvfrom` | 830 | `buf: *mut u8` (+ addr/addrlen) | W | socket RX |

## Sites needing special care at Stage 3

- **iovec double-deref (`writev`/`readv`):** the `iov` array is a user pointer AND each
  `iov_base` inside it is a user pointer. Stage 3 must bounds-check **both** levels — copy
  the iovec array in first, then validate each `base`/`len` before touching it. A naive
  single copy_from_user misses the inner pointers.
- **futex `uaddr` (2018):** the kernel reads/writes the futex word repeatedly on a hot
  blocking path (see docs/FUTEX_PROTOCOL.md). A per-poll copy would be expensive — Stage 3
  should map/pin or use a single stac/clac window, not copy-per-iteration.
- **The hot serving path is `read`/`write`/`recvfrom`/`sendto`** — these carry the socket
  data and are where the Stage-3 SMAP-window / copy cost shows up on the 11.8 MB/s
  baseline. Measure that increment when Stage 3 lands.
- **Path strings (`open`/`stat`/`readlink`/`connect`):** length is a NUL-scan of user
  memory (unbounded read) — Stage 3 must bound the scan to the user region + a max length.

## Known incompleteness (why this is a draft)

Not audited exhaustively: `ioctl` arg pointers, `getdents`/`getdents64` buffers, `nanosleep`
timespec, `recvmsg`/`sendmsg` iovecs, `arch_prctl` (fs_base — value vs pointer), robust-list
/ set_tid_address stored pointers, and any handler that stashes a user pointer for later
async use. The **Stage-3 SMAP run under exercised load (serving + dist + file I/O)** is what
turns this draft into the audited boundary — it faults on every real site, including the
ones this reading missed.
