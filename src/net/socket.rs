//! POSIX socket layer bridging ERTS syscalls to smoltcp.
//!
//! Provides socket/bind/listen/accept/send/recv/getsockopt/setsockopt
//! for TCP and UDP, backed by smoltcp's socket abstractions.
//!
//! Design follows Nanos (nanovms/nanos): each socket fd maps to a smoltcp
//! SocketHandle. The fd table coexists with VFS fds (which use 1000+).
//! Socket fds start at 500 to avoid collisions.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::serial_println;

/// Base fd for socket allocations (avoids collision with VFS fds at 1000+
/// and pipe fds at 200+).
const SOCK_FD_BASE: i32 = 500;
/// Advisory ceiling on total smoltcp sockets (listeners + live streams).
/// Not enforced — the `SocketSet` and fd table grow on demand — but kept as
/// documentation of the budget: 2 listening ports × LISTENER_POOL_SIZE (64)
/// = 128 idle listeners, leaving headroom for live connections.
#[allow(dead_code)]
const MAX_SOCKETS: usize = 256;

/// Size of the smoltcp listener pool pre-bound to a port at `sys_listen`
/// time. smoltcp's `tcp::Socket` only holds one connection at a time; with
/// a single listener, a second SYN that arrives before the first transitions
/// to Established receives a RST. We pre-bind a pool of listening sockets to
/// the same `IpListenEndpoint` — smoltcp routes each incoming SYN to one
/// available listener — and `sys_accept` adds a fresh replacement listener
/// every time it consumes an established connection, so the pool stays full.
///
/// Sized to the concurrency the clean-hardware (Nitro) sweep exposed: at
/// pool size 8 the stack RST'd every connection past ~8 simultaneous SYNs
/// (200/200 at N=5, but only ~8/200 at N≥25). 64 simultaneous in-flight
/// SYNs now each land on a free listener; ThousandIsland's acceptors refill
/// the pool as they consume connections. (This was masked on QEMU because
/// the SLIRP host bridge dropped the excess SYNs before they reached us.)
///
/// The `backlog` argument to `listen(2)` is intentionally ignored: it sets
/// the queue depth in real kernels, not the number of pre-bound sockets,
/// and BEAM passes values like 1024 that would exhaust the kernel heap if
/// taken literally as a socket count.
const LISTENER_POOL_SIZE: usize = 64;

/// BUG-8 — kernel-heap reserve that gates the accept path against a
/// connection-flood DoS.
///
/// The bug: a single unauthenticated client holding ~1000–1250 concurrent TCP
/// connections exhausted the shared 16 MiB kernel heap and panicked the kernel
/// (`alloc.rs:573`, a failed 32 KiB allocation — which is exactly the 32 KiB TX
/// buffer `install_fresh_listener` reserves per connection: see LISTENER_BUF_SIZE
/// / the TX buffer below, ~34 KiB total per accepted stream). tmpfs already caps
/// itself to protect this heap; the accept path did not.
///
/// The fix is heap-headroom **backpressure**, not a connection *count* cap — the
/// per-connection cost (~34 KiB) makes a safe count hard to pin, whereas free
/// heap is the invariant that actually matters and self-tunes. `sys_accept`
/// refuses to consume a new established connection while free heap is below this
/// reserve; the connection stays in the listener pool and smoltcp RSTs the
/// overflow at the SYN — a clean reject, no heap growth, no panic. The reserve
/// (4 MiB) keeps room for tmpfs (≤4 MiB is its own cap), the scheduler, and
/// in-flight allocation spikes, and is far above any single allocation (the
/// 32 KiB TX buffer, the ~216 KiB SocketSet `Vec` growth) so an accept can never
/// be the allocation that fails.
const ACCEPT_HEAP_RESERVE: usize = 4 * 1024 * 1024;

/// Socket type
#[derive(Clone, Copy, PartialEq)]
enum SockType {
    TcpStream,
    TcpListener,
    UdpDgram,
}

/// Per-socket state.
///
/// For `TcpListener`, the listener owns a pool of `tcp::Socket`s all
/// bound to the same endpoint: `handle` is the primary slot, `backlog`
/// holds the spares. `sys_accept` scans `[handle] + backlog` for an
/// Established/CloseWait socket and replaces that slot with a fresh
/// listener once it captures one.
struct Socket {
    fd: i32,
    sock_type: SockType,
    handle: SocketHandle,
    nonblock: bool,
    /// For listeners: spare listening handles (size = LISTENER_POOL_SIZE − 1
    /// after sys_listen, always refilled by sys_accept). Empty for streams.
    backlog: Vec<SocketHandle>,
    /// Local address after bind
    local_port: u16,
    local_addr: Ipv4Address,
    /// TcpStream: true between `connect()` and the moment we observe the
    /// SynSent→Established (success) or SynSent→Closed (failure) transition.
    /// Drives the getsockopt(SO_ERROR) / POLLOUT|POLLERR reporting ERTS waits on.
    connecting: bool,
    /// UdpDgram: default remote endpoint set by connect(), so send/2 (no addr)
    /// works. None until connected.
    udp_peer: Option<IpEndpoint>,
}

/// Ephemeral source-port allocator for outbound connect() and UDP binds.
/// 49152–65535 (IANA dynamic range), disjoint from the service ports (8080/9090)
/// and the listener pool (which binds the low service ports), so it never
/// collides with inbound.
static NEXT_EPHEMERAL: AtomicI32 = AtomicI32::new(49152);
fn alloc_ephemeral_port() -> u16 {
    let p = NEXT_EPHEMERAL.fetch_add(1, Ordering::Relaxed);
    // wrap within 49152..=65535
    let port = 49152 + ((p - 49152).rem_euclid(65535 - 49152 + 1));
    if port >= 65535 {
        NEXT_EPHEMERAL.store(49152, Ordering::Relaxed);
    }
    port as u16
}

/// Global socket table. Guarded by its own spinlock; lock order is
/// SOCKETS → NET_LOCK whenever a syscall touches both.
static SOCKETS: spin::Mutex<Vec<Socket>> = spin::Mutex::new(Vec::new());
static NEXT_SOCK_FD: AtomicI32 = AtomicI32::new(SOCK_FD_BASE);

/// Closed-socket fd freelist. Each socket::close() pushes the fd
/// back; alloc_fd() pops one before bumping NEXT_SOCK_FD. Keeps the
/// active fd range proportional to peak simultaneous open count
/// instead of growing without bound — important so socket fds stay
/// below 1024, the FD_SETSIZE / fd_set bitmap limit used by musl
/// and various ERTS internals. Past fd 1023 those bitmaps silently
/// overflow, killing acceptors without visible errors.
static RECYCLED_FDS: spin::Mutex<Vec<i32>> = spin::Mutex::new(Vec::new());

/// TCP handles whose userspace fd has been closed but whose smoltcp
/// state machine has not yet reached `Closed`. `tcp::Socket::close()`
/// only requests the close — it sets the state to `FinWait1` /
/// `LastAck` and lets the next `Interface::poll()` actually transmit
/// the FIN. Removing the handle from the `SocketSet` before that poll
/// drops the in-flight FIN, leaving the remote half-open and the host
/// hostfwd in CLOSE_WAIT. So we defer the `SocketSet::remove()` until
/// `gc_closed_handles()` observes the state has reached `Closed`.
///
/// Lock order: SOCKETS → CLOSING_HANDLES → NET_LOCK.
///
/// Each entry pairs the handle with the uptime-ms it entered closing, so the
/// BUG-8 teardown reaper (`gc_closed_handles`) can age out sockets stranded in a
/// half-closed state (a connection flood FIN-closes accepted streams whose peers
/// have vanished; they sit in FinWait forever and never reach Closed, leaking
/// ~34 KiB each — which pins the heap at the accept reserve and blocks recovery).
static CLOSING_HANDLES: spin::Mutex<Vec<(SocketHandle, u64)>> =
    spin::Mutex::new(Vec::new());

/// BUG-8 teardown-reaper timeout. A socket in the CLOSING_HANDLES list that has
/// not reached Closed/TimeWait within this window is force-`abort()`ed (RST) so
/// the reap path frees it. Sized to spare legitimate slow closes (which complete
/// in ms–seconds) while reaping the stranded flood sockets (which sit forever):
/// 15 s is well past any real close but a bounded recovery delay after a flood.
const CLOSING_REAP_MS: u64 = 15_000;

/// Check if an fd is a socket fd.
pub fn is_socket_fd(fd: i32) -> bool {
    SOCKETS.lock().iter().any(|s| s.fd == fd)
}

/// Set the non-blocking flag on a socket fd. Called from `fcntl(F_SETFL)`.
/// Without this, ERTS's `inet_drv` keeps issuing `accept` calls that would
/// block under contention, instead of getting EAGAIN and retrying via epoll.
pub fn set_nonblock(fd: i32, nonblock: bool) {
    if let Some(s) = SOCKETS.lock().iter_mut().find(|s| s.fd == fd) {
        s.nonblock = nonblock;
    }
}

/// Run `f` on the `Socket` matching `fd` while holding the SOCKETS lock.
/// Returns `None` if no such fd exists. Closures may safely call into
/// `crate::net::with_net` (lock order is SOCKETS → NET_LOCK).
fn with_socket<R>(fd: i32, f: impl FnOnce(&mut Socket) -> R) -> Option<R> {
    SOCKETS.lock().iter_mut().find(|s| s.fd == fd).map(f)
}

fn alloc_fd() -> i32 {
    // Try the freelist first so closed-socket fd numbers get recycled
    // instead of monotonically climbing past FD_SETSIZE (1024).
    if let Some(fd) = RECYCLED_FDS.lock().pop() {
        return fd;
    }
    NEXT_SOCK_FD.fetch_add(1, Ordering::Relaxed)
}

/// RX buffer for spare listeners installed by sys_listen and sys_accept — kept
/// small (a) because the listener mostly only holds a SYN/SYN-ACK before
/// sys_accept moves it to a stream, and (b) we pre-allocate LISTENER_POOL_SIZE
/// of them across 2 ports.
const LISTENER_BUF_SIZE: usize = 2048;

/// TX buffer for those listeners — becomes the stream's send buffer after
/// accept, so it caps the in-flight (unacked) TCP window. At 2 KiB the window
/// was one segment, and a large `sendfile` response stalled one delayed-ACK per
/// 2 KiB (~34 KB/s on Nitro). 32 KiB lets ~16 segments fly before an ACK is
/// needed. Heap cost = LISTENER_POOL_SIZE × 2 ports × (RX + TX) ≈ 128 × 34 KiB
/// ≈ 4.5 MiB of the 16 MiB kernel heap.
const LISTENER_TX_BUF_SIZE: usize = 32768;

/// Create a new `tcp::Socket` bound to `endpoint` in Listen state, add it
/// to the SocketSet, and return its handle. Called inside `with_net`;
/// caller must already hold NET_LOCK.
fn install_fresh_listener(
    net: &mut crate::net::NetState,
    endpoint: IpListenEndpoint,
) -> SocketHandle {
    let rx_buf = tcp::SocketBuffer::new(alloc::vec![0u8; LISTENER_BUF_SIZE]);
    let tx_buf = tcp::SocketBuffer::new(alloc::vec![0u8; LISTENER_TX_BUF_SIZE]);
    let listener = tcp::Socket::new(rx_buf, tx_buf);
    let h = net.sockets.add(listener);
    {
        let s = net.sockets.get_mut::<tcp::Socket>(h);
        s.listen(endpoint).ok();
        // Nagle OFF by default. smoltcp defaults it ON; combined with delayed-ACK
        // that collapsed throughput to ~100 KB/s (a 1 MB dist term took ~10 s).
        // ERTS sets TCP_NODELAY on dist sockets but Tyn's setsockopt no-ops it, so
        // default it off at creation — covers accepted (acceptor-side) connections,
        // incl. the dist acceptor and Bandit's HTTP sockets, without relying on
        // which fd the app targets. NODELAY is the right default for a BEAM host.
        s.set_nagle_enabled(false);
    }
    h
}

// ---- syscall implementations ----

/// socket(domain, type, protocol) → fd
pub fn sys_socket(domain: i32, sock_type: i32, _protocol: i32) -> i64 {
    // AF_INET = 2, AF_INET6 = 10
    if domain != 2 && domain != 10 {
        return -97; // -EAFNOSUPPORT
    }

    // SOCK_STREAM = 1 (TCP), SOCK_DGRAM = 2 (UDP)
    // Mask out SOCK_NONBLOCK (0x800) and SOCK_CLOEXEC (0x80000)
    let raw_type = sock_type & 0xf;
    let nonblock = (sock_type & 0x800) != 0;

    if raw_type != 1 && raw_type != 2 {
        return -93; // -EPROTONOSUPPORT
    }

    if !crate::net::is_initialized() {
        return -97; // -EAFNOSUPPORT — no network
    }

    let st = if raw_type == 1 { SockType::TcpStream } else { SockType::UdpDgram };

    let handle = crate::net::with_net(|net| {
        match raw_type {
            1 => {
                let rx_buf = tcp::SocketBuffer::new(alloc::vec![0u8; 8192]);
                let tx_buf = tcp::SocketBuffer::new(alloc::vec![0u8; 8192]);
                let tcp_socket = tcp::Socket::new(rx_buf, tx_buf);
                net.sockets.add(tcp_socket)
            }
            _ => {
                // Generous: DNS replies routinely exceed the old 512-byte
                // assumption with EDNS0, and several may queue before recv.
                let rx_buf = udp::PacketBuffer::new(
                    alloc::vec![udp::PacketMetadata::EMPTY; 16],
                    alloc::vec![0u8; 16384],
                );
                let tx_buf = udp::PacketBuffer::new(
                    alloc::vec![udp::PacketMetadata::EMPTY; 16],
                    alloc::vec![0u8; 16384],
                );
                let udp_socket = udp::Socket::new(rx_buf, tx_buf);
                net.sockets.add(udp_socket)
            }
        }
    });

    let fd = alloc_fd();

    SOCKETS.lock().push(Socket {
        fd,
        sock_type: st,
        handle,
        nonblock,
        backlog: Vec::new(),
        local_port: 0,
        local_addr: Ipv4Address::UNSPECIFIED,
        connecting: false,
        udp_peer: None,
    });

    fd as i64
}

/// connect(fd, addr, addrlen) → 0 or -errno. TCP is non-blocking: initiates the
/// SYN and returns -EINPROGRESS; ERTS then waits on epoll for POLLOUT and calls
/// getsockopt(SO_ERROR). UDP: records the default peer (send/2 needs no addr).
pub fn sys_connect(fd: i32, addr_ptr: *const u8, _addrlen: u32) -> i64 {
    // 3b.2: `addr_ptr` is a user sockaddr_in (dist's outbound connect exercises this) —
    // guarded read via the shared helper.
    let (port, addr) = match read_sockaddr_in(addr_ptr) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let remote = IpEndpoint { addr: IpAddress::Ipv4(addr), port };

    with_socket(fd, |sock| {
        match sock.sock_type {
            SockType::UdpDgram => {
                sock.udp_peer = Some(remote);
                // smoltcp requires a bound UDP socket before send.
                if sock.local_port == 0 {
                    sock.local_port = alloc_ephemeral_port();
                    crate::net::with_net(|net| {
                        let udp = net.sockets.get_mut::<udp::Socket>(sock.handle);
                        let _ = udp.bind(IpListenEndpoint { addr: None, port: sock.local_port });
                    });
                }
                0
            }
            SockType::TcpStream | SockType::TcpListener => {
                let local_port = if sock.local_port != 0 {
                    sock.local_port
                } else {
                    let p = alloc_ephemeral_port();
                    sock.local_port = p;
                    p
                };
                sock.sock_type = SockType::TcpStream;
                sock.connecting = true;
                crate::net::with_net(|net| {
                    // split borrow: connect needs iface context + the socket
                    let iface = &mut net.iface;
                    let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                    // Nagle OFF by default (see install_fresh_listener) — covers
                    // the initiator side: dist connect_node + all outbound TCP.
                    tcp.set_nagle_enabled(false);
                    match tcp.connect(iface.context(), remote, local_port) {
                        Ok(()) => -115, // -EINPROGRESS
                        Err(_) => {
                            sock.connecting = false;
                            -111 // -ECONNREFUSED (immediate; rare)
                        }
                    }
                })
            }
        }
    }).unwrap_or(-9)
}

/// bind(fd, addr, addrlen) → 0 or error
/// 3b.2: read a `struct sockaddr_in` (16 B) from a user pointer through the guard,
/// returning (port, ipv4). Err(errno): -EFAULT (bad/kernel pointer) or -EAFNOSUPPORT.
/// Shared by bind/connect/sendto — one audited user-read for the sockaddr shape.
fn read_sockaddr_in(addr_ptr: *const u8) -> Result<(u16, Ipv4Address), i64> {
    let mut sa = [0u8; 16];
    if unsafe { crate::uaccess::copy_from_user(&mut sa, addr_ptr as u64) }.is_err() {
        return Err(-14); // -EFAULT
    }
    if u16::from_ne_bytes([sa[0], sa[1]]) != 2 {
        return Err(-97); // -EAFNOSUPPORT (AF_INET only)
    }
    let port = u16::from_be_bytes([sa[2], sa[3]]);
    Ok((port, Ipv4Address::new(sa[4], sa[5], sa[6], sa[7])))
}

/// 3b.2: write a `struct sockaddr_in` (16 B, zero-padded) + optional addrlen(=16) to
/// user pointers through the guard. No-op if `addr_ptr` is null. Returns 0 or -EFAULT.
/// Shared by accept/getsockname/getpeername — one audited user-write for the sockaddr.
fn write_sockaddr_in(addr_ptr: *mut u8, addrlen_ptr: *mut u32, port: u16, ip: [u8; 4]) -> i64 {
    if addr_ptr.is_null() {
        return 0;
    }
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&2u16.to_ne_bytes()); // AF_INET
    sa[2..4].copy_from_slice(&port.to_be_bytes());
    sa[4..8].copy_from_slice(&ip);
    if unsafe { crate::uaccess::copy_to_user(addr_ptr as u64, &sa) }.is_err() {
        return -14;
    }
    if !addrlen_ptr.is_null()
        && unsafe { crate::uaccess::copy_to_user(addrlen_ptr as u64, &16u32.to_ne_bytes()) }.is_err()
    {
        return -14;
    }
    0
}

pub fn sys_bind(fd: i32, addr_ptr: *const u8, _addrlen: u32) -> i64 {
    // Parse struct sockaddr_in { sa_family(2), sin_port(2), sin_addr(4), zero(8) }
    let (port, addr) = match read_sockaddr_in(addr_ptr) {
        Ok(v) => v,
        Err(e) => return e,
    };

    with_socket(fd, |sock| {
        // port 0 = "assign an ephemeral port" (e.g. gen_udp:open(0), which
        // inet_res uses for DNS). smoltcp binds the exact port given and rejects
        // 0, so allocate one from the dynamic range here.
        //
        // Making this succeed is what inet_res needs — but it also makes ERTS's
        // boot-time `inet_udp:open(0)` in inet_config:set_hostname/0 succeed,
        // which then resolves the node's hostname. If that hostname is dot-less
        // the resolver's domain stays "" and inet_config does a *native*
        // gethostbyname (the inet_gethost port program Tyn can't spawn → fatal).
        // The kernel therefore reports a DOTTED hostname from uname (see
        // sys_uname: "tyn.local"), so set_hostname/1 records a non-empty domain
        // and inet_config skips that native lookup. The two changes are a pair.
        let port = if port == 0 { alloc_ephemeral_port() } else { port };
        sock.local_port = port;
        sock.local_addr = addr;

        match sock.sock_type {
            SockType::UdpDgram => {
                crate::net::with_net(|net| {
                    let udp = net.sockets.get_mut::<udp::Socket>(sock.handle);
                    let endpoint = if addr == Ipv4Address::UNSPECIFIED {
                        IpListenEndpoint { addr: None, port }
                    } else {
                        IpListenEndpoint { addr: Some(IpAddress::Ipv4(addr)), port }
                    };
                    match udp.bind(endpoint) {
                        Ok(()) => 0,
                        Err(_) => -98i64, // -EADDRINUSE
                    }
                })
            }
            SockType::TcpStream | SockType::TcpListener => {
                // TCP bind is deferred to listen/connect
                0
            }
        }
    }).unwrap_or(-9)
}

/// listen(fd, backlog) → 0 or error
///
/// Pre-binds `LISTENER_POOL_SIZE` `tcp::Socket`s to the same endpoint so
/// concurrent SYNs each find a listener in Listen state. smoltcp routes
/// each SYN to one of them; `sys_accept` consumes the established one and
/// refills the slot. The user-passed `backlog` is ignored — see the
/// LISTENER_POOL_SIZE docs.
pub fn sys_listen(fd: i32, _backlog: i32) -> i64 {
    // BUG-8: baseline heap at listen time, so the accept-reserve can be sized/
    // read against a real number (free must stay comfortably above the reserve
    // for legit concurrency; free never dropping below it is what stops the DoS).
    serial_println!(
        "[net] listen fd={} — kernel heap free {} KiB / {} KiB (accept reserve {} KiB)",
        fd,
        crate::memory::heap::free_bytes() / 1024,
        crate::memory::heap::total_bytes() / 1024,
        ACCEPT_HEAP_RESERVE / 1024
    );
    with_socket(fd, |sock| {
        sock.sock_type = SockType::TcpListener;
        crate::net::with_net(|net| {
            let endpoint = if sock.local_addr == Ipv4Address::UNSPECIFIED {
                IpListenEndpoint { addr: None, port: sock.local_port }
            } else {
                IpListenEndpoint {
                    addr: Some(IpAddress::Ipv4(sock.local_addr)),
                    port: sock.local_port,
                }
            };

            // Primary slot: convert the existing socket (allocated by
            // sys_socket) into a listener on this endpoint.
            {
                let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                if tcp.listen(endpoint).is_err() {
                    return -98i64; // -EADDRINUSE
                }
            }

            // Spare listeners: create pool_size − 1 more, each bound to the
            // same endpoint. smoltcp matches an incoming SYN against the
            // first listener that's in Listen state for the destination
            // port, so any of them can serve.
            //
            // Buffers are 2 KiB instead of 8 KiB (as in sys_socket) — these
            // listeners only need to absorb a SYN/SYN-ACK plus the initial
            // HTTP request (≤ a few hundred bytes) before sys_accept hands
            // the socket off to user space; sys_accept also installs the
            // replacement listener at 2 KiB. 64 listeners × 4 KiB = 256 KiB
            // per listening port, small enough not to fragment the heap.
            sock.backlog.clear();
            for _ in 1..LISTENER_POOL_SIZE {
                let h = install_fresh_listener(net, endpoint);
                sock.backlog.push(h);
            }
            0
        })
    }).unwrap_or(-9)
}

/// accept4(fd, addr, addrlen, flags) → new_fd or error
///
/// **Concurrent-acceptor correctness.** ERTS's `inet_drv` runs many
/// concurrent `gen_tcp:accept` waiters on the same listener (TI starts
/// 100). When a connection arrives, only ONE waiter must capture it.
/// We make the state-check + handle-steal + new-listener-install
/// atomic by doing all of it inside a single `with_net`. The losing
/// races see the freshly-installed listener (`Listen` state) and either
/// yield (blocking) or return EAGAIN (non-blocking).
pub fn sys_accept(fd: i32, addr_ptr: *mut u8, addrlen_ptr: *mut u32, flags: i32) -> i64 {
    // Snapshot listener metadata under the SOCKETS lock; the values needed
    // for the listen-endpoint reinstall don't change for the lifetime of
    // the listener fd.
    let snapshot = with_socket(fd, |sock| {
        if sock.sock_type != SockType::TcpListener {
            return Err(-95i64); // -EOPNOTSUPP
        }
        Ok((
            sock.nonblock || (flags & 0x800) != 0,
            sock.local_port,
            sock.local_addr,
        ))
    });
    let (nonblock_call, listen_port, listen_addr) = match snapshot {
        Some(Ok(v)) => v,
        Some(Err(e)) => return e,
        None => return -9, // -EBADF
    };

    // Scan the listener pool ({sock.handle} ∪ sock.backlog) for any socket
    // in Established/CloseWait. Whichever slot wins is replaced by a fresh
    // listener so the pool stays full. Each iteration runs under SOCKETS
    // and NET_LOCK; losers drop both locks and yield, never sleeping with
    // a spinlock held.
    let (accepted_handle, remote) = loop {
        crate::net::poll();

        let result = with_socket(fd, |sock| {
            crate::net::with_net(|net| {
                // BUG-8 heap-headroom backpressure decision, evaluated once per
                // capture attempt (under SOCKETS + NET): below the reserve we must
                // not create a new stream (each retains ~34 KiB and consuming one
                // also installs a fresh ~34 KiB listener — a flood otherwise grows
                // the SocketSet until a 32 KiB TX-buffer alloc fails and panics the
                // kernel). But we must NOT simply leave the established connection
                // in the pool: that strands it in a pool slot forever, and once all
                // 64 slots are stranded the listener is dead even after the flood
                // ends (no recovery). Instead, when over budget, we RESET the
                // connection's socket back to a listener in place — abort() RSTs the
                // client, listen() reuses the same buffers (no alloc under low heap)
                // — so the pool stays full and the node recovers once the flood
                // stops and heap frees.
                let over_budget = crate::memory::heap::free_bytes() < ACCEPT_HEAP_RESERVE;
                let endpoint = if listen_addr == Ipv4Address::UNSPECIFIED {
                    IpListenEndpoint { addr: None, port: listen_port }
                } else {
                    IpListenEndpoint {
                        addr: Some(IpAddress::Ipv4(listen_addr)),
                        port: listen_port,
                    }
                };

                // Try primary slot.
                let primary_state = {
                    let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                    tcp.state()
                };
                if primary_state == tcp::State::Established
                    || primary_state == tcp::State::CloseWait
                {
                    if over_budget {
                        // Reject in place: RST + re-listen on the same socket,
                        // reusing its buffers. Keeps the pool slot alive so the
                        // node recovers after the flood.
                        let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                        tcp.abort();
                        tcp.listen(endpoint).ok();
                        return None;
                    }
                    let accepted = sock.handle;
                    let remote = net.sockets
                        .get_mut::<tcp::Socket>(accepted)
                        .remote_endpoint();
                    sock.handle = install_fresh_listener(net, endpoint);
                    return Some((accepted, remote));
                }

                // Try each spare in backlog.
                for i in 0..sock.backlog.len() {
                    let h = sock.backlog[i];
                    let state = net.sockets.get_mut::<tcp::Socket>(h).state();
                    if state == tcp::State::Established
                        || state == tcp::State::CloseWait
                    {
                        if over_budget {
                            let tcp = net.sockets.get_mut::<tcp::Socket>(h);
                            tcp.abort();
                            tcp.listen(endpoint).ok();
                            return None;
                        }
                        let remote = net.sockets
                            .get_mut::<tcp::Socket>(h)
                            .remote_endpoint();
                        sock.backlog[i] = install_fresh_listener(net, endpoint);
                        return Some((h, remote));
                    }
                }
                None
            })
        }).flatten();

        if let Some(captured) = result {
            break captured;
        }
        if nonblock_call {
            return -11; // -EAGAIN
        }
        crate::sched::yield_current();
    };

    crate::vdbg!("[accept] connection established!");

    let new_fd = alloc_fd();
    let nonblock = (flags & 0x800) != 0;

    SOCKETS.lock().push(Socket {
        fd: new_fd,
        sock_type: SockType::TcpStream,
        handle: accepted_handle,
        nonblock,
        backlog: Vec::new(),
        local_port: listen_port,
        local_addr: listen_addr,
        connecting: false,
        udp_peer: None,
    });

    // Fill in peer address if requested (guarded). Best-effort: the connection is
    // already accepted (new_fd allocated), so a bad addr_ptr (EFAULT) just skips the
    // fill rather than leaking the fd — and skipping means the kernel never wrote it.
    if let Some(remote) = remote {
        let ip = if let IpAddress::Ipv4(v4) = remote.addr { v4.octets() } else { [0u8; 4] };
        let _ = write_sockaddr_in(addr_ptr, addrlen_ptr, remote.port, ip);
    }

    crate::vdbg!("[accept] returning new_fd={}", new_fd);
    new_fd as i64
}

/// getsockname(fd, addr, addrlen) → 0 or error
pub fn sys_getsockname(fd: i32, addr_ptr: *mut u8, addrlen_ptr: *mut u32) -> i64 {
    let local = match with_socket(fd, |sock| (sock.local_port, sock.local_addr)) {
        Some(v) => v,
        None => return -9,
    };

    write_sockaddr_in(addr_ptr, addrlen_ptr, local.0, local.1.octets())
}

/// getpeername(fd, addr, addrlen) → 0 or error
pub fn sys_getpeername(fd: i32, addr_ptr: *mut u8, addrlen_ptr: *mut u32) -> i64 {
    let remote = match with_socket(fd, |sock| {
        crate::net::with_net(|net| {
            let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
            tcp.remote_endpoint()
        })
    }) {
        Some(v) => v,
        None => return -9,
    };

    match remote {
        Some(ep) => {
            let ip = if let IpAddress::Ipv4(v4) = ep.addr { v4.octets() } else { [0u8; 4] };
            write_sockaddr_in(addr_ptr, addrlen_ptr, ep.port, ip)
        }
        None => -107, // -ENOTCONN
    }
}

/// setsockopt(fd, level, optname, optval, optlen) → 0 or error
pub fn sys_setsockopt(fd: i32, level: i32, optname: i32, optval: *const u8, optlen: u32) -> i64 {
    if !is_socket_fd(fd) {
        return -9;
    }
    // Accept common options silently
    // SOL_SOCKET=1: SO_REUSEADDR=2, SO_RCVBUF=8, SO_SNDBUF=7, SO_KEEPALIVE=9, SO_PRIORITY=12, SO_LINGER=13
    // SOL_IP=0/IPPROTO_IP=0: IP_TOS=1
    // SOL_TCP=6/IPPROTO_TCP=6: TCP_NODELAY=1
    match (level, optname) {
        (1, 2) | (1, 7) | (1, 8) | (1, 9) | (1, 12) | (1, 13) => 0, // SOL_SOCKET options
        (0, 1) => 0, // IP_TOS
        (6, 1) => 0, // TCP_NODELAY — no-op: Nagle is already OFF by default on all
                     // Tyn TCP sockets (see install_fresh_listener / connect), which
                     // is what NODELAY=1 asks for, so accepting it is correct.
        _ => 0,      // Accept all others silently
    }
}

/// getsockopt(fd, level, optname, optval, optlen) → 0 or error
pub fn sys_getsockopt(fd: i32, level: i32, optname: i32, optval: *mut u8, optlen: *mut u32) -> i64 {
    if !is_socket_fd(fd) {
        return -9;
    }
    // Return sensible defaults
    if optval.is_null() || optlen.is_null() {
        return -14; // -EFAULT
    }
    // SO_ERROR reports the outcome of a non-blocking connect(): 0 once
    // Established, ECONNREFUSED once the SYN failed (SynSent→Closed). ERTS reads
    // this the moment the socket becomes writable/errored.
    let so_error: i32 = with_socket(fd, |sock| {
        if sock.sock_type == SockType::TcpStream && sock.connecting {
            crate::net::with_net(|net| {
                use smoltcp::socket::tcp::State;
                let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                match tcp.state() {
                    State::Established => { sock.connecting = false; 0 }
                    State::Closed | State::TimeWait | State::CloseWait => {
                        sock.connecting = false; 111 // ECONNREFUSED
                    }
                    _ => 0, // still in progress
                }
            })
        } else { 0 }
    }).unwrap_or(0);

    // 3b.2: `optlen` and `optval` are user pointers — read the caller's buffer size,
    // then write the value + updated length through the guard.
    let mut lenbuf = [0u8; 4];
    if unsafe { crate::uaccess::copy_from_user(&mut lenbuf, optlen as u64) }.is_err() {
        return -14;
    }
    let len = u32::from_ne_bytes(lenbuf);
    // (value, width): SO_LINGER is two i32 zeros (8 B); SO_ERROR reports so_error;
    // everything else we accept as a 4-byte 0 (matches the prior no-op semantics).
    let (val, width): (i32, u32) = match (level, optname) {
        (1, 4) => (so_error, 4),  // SO_ERROR
        (1, 13) => (0, 8),        // SO_LINGER (l_onoff, l_linger both 0)
        _ => (0, 4),
    };
    if len < width {
        return 0; // caller buffer too small — leave untouched, as before
    }
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&val.to_ne_bytes()); // low i32 = value; SO_LINGER's high i32 stays 0
    if unsafe { crate::uaccess::copy_to_user(optval as u64, &out[..width as usize]) }.is_err() {
        return -14;
    }
    if unsafe { crate::uaccess::copy_to_user(optlen as u64, &width.to_ne_bytes()) }.is_err() {
        return -14;
    }
    0
}

/// send/sendto/write on a socket fd
pub fn sys_sendto(fd: i32, buf: *const u8, len: usize, _flags: i32,
                  dest_addr: *const u8, _addrlen: u32) -> i64 {
    // `data` is a fat pointer over the user send buffer; the actual read happens
    // inside `send_slice`, bracketed by the guard below (3b.2). Building the slice
    // does not touch the memory.
    let data = unsafe { core::slice::from_raw_parts(buf, len) };

    // For UDP sendto, the destination is in dest_addr (sockaddr_in) — a user source.
    // NULL (plain send/2 on a connected UDP socket) → fall back to the stored peer;
    // a bad/non-AF_INET dest_addr also falls back (best-effort, never derefs kernel).
    let dest = if dest_addr.is_null() {
        None
    } else {
        match read_sockaddr_in(dest_addr) {
            Ok((port, v4)) => Some(IpEndpoint { addr: IpAddress::Ipv4(v4), port }),
            Err(_) => None,
        }
    };

    with_socket(fd, |sock| {
        match sock.sock_type {
            SockType::UdpDgram => {
                let endpoint = match dest.or(sock.udp_peer) {
                    Some(e) => e,
                    None => return -89i64, // -EDESTADDRREQ
                };
                // smoltcp requires the socket be bound before send.
                if sock.local_port == 0 {
                    sock.local_port = alloc_ephemeral_port();
                    crate::net::with_net(|net| {
                        let udp = net.sockets.get_mut::<udp::Socket>(sock.handle);
                        let _ = udp.bind(IpListenEndpoint { addr: None, port: sock.local_port });
                    });
                }
                let r = crate::net::with_net(|net| {
                    let udp = net.sockets.get_mut::<udp::Socket>(sock.handle);
                    // 3b.2: `data` reads the user send buffer — validate + stac around it.
                    match unsafe { crate::uaccess::with_user_access(buf as u64, len as u64, |_| {
                        udp.send_slice(data, endpoint)
                    }) } {
                        Ok(Ok(())) => len as i64,
                        Ok(Err(udp::SendError::BufferFull)) => -11, // -EAGAIN
                        Ok(Err(_)) => -22, // -EINVAL
                        Err(_) => -14, // -EFAULT (bad/kernel buffer pointer)
                    }
                });
                crate::net::poll(); // flush the datagram now
                r
            }
            SockType::TcpStream => {
                crate::net::with_net(|net| {
                    let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                    if !tcp.can_send() {
                        return -11i64; // -EAGAIN
                    }
                    // 3b.2: `data` reads the user send buffer — validate + stac around it.
                    match unsafe { crate::uaccess::with_user_access(buf as u64, len as u64, |_| tcp.send_slice(data)) } {
                        Ok(Ok(sent)) => sent as i64,
                        Ok(Err(_)) => -104, // -ECONNRESET
                        Err(_) => -14, // -EFAULT
                    }
                })
            }
            SockType::TcpListener => -95, // -EOPNOTSUPP
        }
    }).unwrap_or(-9)
}

/// recv/recvfrom/read on a socket fd
pub fn sys_recvfrom(fd: i32, buf: *mut u8, len: usize, _flags: i32,
                    src_addr: *mut u8, addrlen: *mut u32) -> i64 {
    // Poll network first to process any pending incoming packets. Done
    // outside the SOCKETS lock so concurrent senders/recvers on other
    // fds aren't blocked.
    crate::net::poll();

    let result = with_socket(fd, |sock| {
        match sock.sock_type {
            SockType::TcpStream => {
                crate::net::with_net(|net| {
                    let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                    if !tcp.can_recv() {
                        if !tcp.is_active() {
                            return 0i64; // EOF — connection closed
                        }
                        return -11; // -EAGAIN
                    }
                    // 3b.2: `dest` is the user recv buffer — validate + stac around it.
                    let dest = unsafe { core::slice::from_raw_parts_mut(buf, len) };
                    match unsafe { crate::uaccess::with_user_access(buf as u64, len as u64, |_| tcp.recv_slice(dest)) } {
                        Ok(Ok(n)) => n as i64,
                        Ok(Err(_)) => -104, // -ECONNRESET
                        Err(_) => -14, // -EFAULT
                    }
                })
            }
            SockType::UdpDgram => {
                crate::net::with_net(|net| {
                    let udp = net.sockets.get_mut::<udp::Socket>(sock.handle);
                    if !udp.can_recv() {
                        return -11i64; // -EAGAIN
                    }
                    match udp.recv() {
                        Ok((data, meta)) => {
                            let n = data.len().min(len);
                            // 3b.2: copy the datagram into the user buffer (kernel src →
                            // user dest — guard the dest write).
                            if unsafe { crate::uaccess::copy_to_user(buf as u64, &data[..n]) }.is_err() {
                                return -14;
                            }
                            // recvfrom must fill src_addr — the resolver verifies the reply
                            // came from the nameserver and silently drops it otherwise.
                            let ep = meta.endpoint;
                            let ip = if let IpAddress::Ipv4(v4) = ep.addr { v4.octets() } else { [0u8; 4] };
                            let _ = write_sockaddr_in(src_addr, addrlen, ep.port, ip);
                            n as i64
                        }
                        Err(_) => -11, // -EAGAIN
                    }
                })
            }
            SockType::TcpListener => -9,
        }
    });

    let result = match result {
        Some(r) => r,
        None => {
            static MISS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            if MISS.fetch_add(1, Ordering::Relaxed) < 5 {
                crate::serial_println!("[recv-miss] fd={} (socket not found)", fd);
            }
            return -9;
        }
    };

    static REC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let n = REC.fetch_add(1, Ordering::Relaxed);
    if n < 50 {
        crate::serial_println!("[recv] fd={} len={} -> {}", fd, len, result);
    }
    result
}

/// Check socket readiness for ppoll/select.
/// Returns POLLIN/POLLOUT bitmask.
pub fn poll_socket(fd: i32) -> u16 {
    const POLLIN: u16 = 0x0001;
    const POLLOUT: u16 = 0x0004;
    const POLLHUP: u16 = 0x0010;
    const _POLLERR: u16 = 0x0008;

    with_socket(fd, |sock| {
        crate::net::with_net(|net| {
            match sock.sock_type {
                SockType::TcpStream => {
                    use smoltcp::socket::tcp::State;
                    let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                    let mut events = 0u16;
                    if sock.connecting {
                        // Non-blocking connect in flight: writable == connected.
                        // Report the failure (SynSent→Closed) as POLLOUT|POLLERR
                        // so ERTS wakes and reads SO_ERROR (getsockopt), instead
                        // of hanging on a refused/timed-out connection.
                        match tcp.state() {
                            State::Established => { sock.connecting = false; events |= POLLOUT; }
                            State::Closed | State::TimeWait | State::CloseWait => {
                                events |= POLLOUT | _POLLERR;
                            }
                            _ => {} // still connecting
                        }
                        return events;
                    }
                    if tcp.can_recv() { events |= POLLIN; }
                    if tcp.can_send() { events |= POLLOUT; }
                    if !tcp.is_active() && !tcp.is_listening() {
                        events |= POLLHUP;
                    }
                    events
                }
                SockType::TcpListener => {
                    // Pool is readable when ANY slot has an Established
                    // connection ready to be accepted.
                    let mut events = 0u16;
                    if net.sockets.get_mut::<tcp::Socket>(sock.handle).is_active() {
                        events |= POLLIN;
                    } else {
                        for &h in sock.backlog.iter() {
                            if net.sockets.get_mut::<tcp::Socket>(h).is_active() {
                                events |= POLLIN;
                                break;
                            }
                        }
                    }
                    events
                }
                SockType::UdpDgram => {
                    let udp = net.sockets.get_mut::<udp::Socket>(sock.handle);
                    let mut events = 0u16;
                    if udp.can_recv() { events |= POLLIN; }
                    if udp.can_send() { events |= POLLOUT; }
                    events
                }
            }
        })
    }).unwrap_or(0)
}

/// Check if any socket has a pending event (connection ready, data available).
///
/// Snapshots the fd list under SOCKETS, then drops the lock before
/// poll_socket re-acquires it per fd — avoids spin::Mutex recursion.
pub fn any_socket_ready() -> bool {
    const POLLIN: u16 = 0x0001;
    let fds: Vec<i32> = SOCKETS.lock().iter().filter(|s| s.fd >= 0).map(|s| s.fd).collect();
    fds.into_iter().any(|fd| poll_socket(fd) & POLLIN != 0)
}

/// Close a socket fd.
///
/// Removes the socket from the SOCKETS table immediately so future
/// fd-keyed syscalls return EBADF. The smoltcp handle disposition
/// depends on the socket type:
///
/// * `TcpStream`: smoltcp's `tcp::Socket::close()` only requests the
///   close — the FIN is sent on the next `Interface::poll()` and the
///   state machine then needs the remote's FIN/ACK before reaching
///   `Closed`. Removing the handle now would drop the in-flight FIN
///   and leak the connection on the remote side. So we park the
///   handle in `CLOSING_HANDLES` and let `gc_closed_handles()` (run
///   from `NetState::poll()`) remove it once the state machine has
///   actually finished.
/// * `TcpListener`: in `Listen` state, `close()` transitions straight
///   to `Closed` (no peer to coordinate with), so removal is safe
///   immediately. Same for every spare in the listener pool.
/// * `UdpDgram`: no connection state — instant removal.
pub fn close(fd: i32) {
    let mut sockets = SOCKETS.lock();
    let Some(idx) = sockets.iter().position(|s| s.fd == fd) else { return };
    let sock = sockets.remove(idx);
    drop(sockets);
    // Return the fd number to the freelist so the next alloc_fd()
    // reuses it. Keeps the active fd range bounded by peak open
    // count instead of growing monotonically past FD_SETSIZE (1024).
    RECYCLED_FDS.lock().push(fd);
    crate::net::with_net(|net| {
        match sock.sock_type {
            SockType::TcpStream => {
                let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                // Clean FIN close. Tried abort() to reap the handle
                // immediately and keep the SocketSet small, but Bandit
                // treats the resulting RST as a connection error and
                // burns time on supervisor-tree handling — measured
                // throughput dropped ~50% (5.3 -> 2.5 req/s on
                // sequential 1000). Stick with FIN; the larger
                // SocketSet is the lesser cost.
                tcp.close();
                CLOSING_HANDLES.lock().push((sock.handle, net.uptime_ms()));
            }
            SockType::TcpListener => {
                let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                tcp.close();
                net.sockets.remove(sock.handle);
                for h in sock.backlog.iter() {
                    net.sockets.get_mut::<tcp::Socket>(*h).close();
                    net.sockets.remove(*h);
                }
            }
            SockType::UdpDgram => {
                let udp = net.sockets.get_mut::<udp::Socket>(sock.handle);
                udp.close();
                net.sockets.remove(sock.handle);
            }
        }
    });
}

/// Reap TCP handles whose state machine has finished. Runs from
/// `NetState::poll()` after `iface.poll()` so any handles that
/// transitioned during this poll get freed in the same cycle.
///
/// We reap both `Closed` (fully torn down) AND `TimeWait` (waiting
/// out 2×MSL). For short-lived HTTP connections, holding sockets
/// through the full 2×MSL bloats the `SocketSet` and slows every
/// subsequent `iface.poll()` linearly with the number of handles.
/// Skipping `TimeWait` is a minor RFC-793 deviation (it protects
/// against delayed duplicate segments from the previous incarnation
/// of the 4-tuple) and is the standard server-side trade-off; the
/// previous version that reaped only `Closed` dropped throughput to
/// ~1 req/s under sustained load because TIME_WAIT sockets piled up.
///
/// Caller holds NET_LOCK (via `with_net`).
pub fn gc_closed_handles(net: &mut crate::net::NetState) {
    // SMP note: called from NetState::poll() under the net lock, so the
    // state read + abort() + remove() below are serialized with the accept path
    // (which also touches the SocketSet only under that lock). No other CPU can
    // mutate a socket mid-reap. CLOSING_HANDLES is a leaf lock (same order as
    // close(): net -> CLOSING_HANDLES).
    let now = net.uptime_ms();
    let mut closing = CLOSING_HANDLES.lock();
    let mut i = 0;
    while i < closing.len() {
        let (h, t0) = closing[i];
        let state = net.sockets.get::<tcp::Socket>(h).state();
        let done = state == tcp::State::Closed || state == tcp::State::TimeWait;
        // BUG-8 teardown reaper: a socket still not Closed after CLOSING_REAP_MS
        // is stranded mid-teardown (the flood signature — the peer vanished after
        // our FIN, so it sits in FinWait/LastAck forever, leaking ~34 KiB). Force
        // it to Closed with abort() so it frees this pass. The timeout spares
        // legit closes (ms–seconds); only genuinely-stuck sockets age out.
        let stranded = !done && now.wrapping_sub(t0) >= CLOSING_REAP_MS;
        if stranded {
            net.sockets.get_mut::<tcp::Socket>(h).abort();
        }
        if done || stranded {
            net.sockets.remove(h);
            closing.swap_remove(i);
        } else {
            i += 1;
        }
    }
}

