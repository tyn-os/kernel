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
const MAX_SOCKETS: usize = 32;

/// Size of the smoltcp listener pool pre-bound to a port at `sys_listen`
/// time. smoltcp's `tcp::Socket` only holds one connection at a time; with
/// a single listener, a second SYN that arrives before the first transitions
/// to Established receives a RST. We pre-bind a pool of listening sockets to
/// the same `IpListenEndpoint` — smoltcp routes each incoming SYN to one
/// available listener — and `sys_accept` adds a fresh replacement listener
/// every time it consumes an established connection, so the pool stays full.
///
/// 8 is enough to absorb realistic bursts; ThousandIsland's 100-acceptor
/// default oversubscribes the pool but only N concurrent in-flight SYNs can
/// land before any acceptor consumes one.
///
/// The `backlog` argument to `listen(2)` is intentionally ignored: it sets
/// the queue depth in real kernels, not the number of pre-bound sockets,
/// and BEAM passes values like 1024 that would exhaust the kernel heap if
/// taken literally as a socket count.
const LISTENER_POOL_SIZE: usize = 8;

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
}

/// Global socket table. Guarded by its own spinlock; lock order is
/// SOCKETS → NET_LOCK whenever a syscall touches both.
static SOCKETS: spin::Mutex<Vec<Socket>> = spin::Mutex::new(Vec::new());
static NEXT_SOCK_FD: AtomicI32 = AtomicI32::new(SOCK_FD_BASE);

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
    NEXT_SOCK_FD.fetch_add(1, Ordering::Relaxed)
}

/// Size of rx/tx buffers for spare listeners installed by sys_listen and
/// sys_accept. Smaller than the 8 KiB used in sys_socket because (a) the
/// listener mostly only holds a SYN/SYN-ACK before sys_accept moves it
/// to a stream, and (b) we pre-allocate LISTENER_POOL_SIZE of them.
const LISTENER_BUF_SIZE: usize = 2048;

/// Create a new `tcp::Socket` bound to `endpoint` in Listen state, add it
/// to the SocketSet, and return its handle. Called inside `with_net`;
/// caller must already hold NET_LOCK.
fn install_fresh_listener(
    net: &mut crate::net::NetState,
    endpoint: IpListenEndpoint,
) -> SocketHandle {
    let rx_buf = tcp::SocketBuffer::new(alloc::vec![0u8; LISTENER_BUF_SIZE]);
    let tx_buf = tcp::SocketBuffer::new(alloc::vec![0u8; LISTENER_BUF_SIZE]);
    let listener = tcp::Socket::new(rx_buf, tx_buf);
    let h = net.sockets.add(listener);
    net.sockets.get_mut::<tcp::Socket>(h).listen(endpoint).ok();
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
                let rx_buf = udp::PacketBuffer::new(
                    alloc::vec![udp::PacketMetadata::EMPTY; 8],
                    alloc::vec![0u8; 8192],
                );
                let tx_buf = udp::PacketBuffer::new(
                    alloc::vec![udp::PacketMetadata::EMPTY; 8],
                    alloc::vec![0u8; 8192],
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
    });

    fd as i64
}

/// bind(fd, addr, addrlen) → 0 or error
pub fn sys_bind(fd: i32, addr_ptr: *const u8, _addrlen: u32) -> i64 {
    // Parse struct sockaddr_in { sa_family(2), sin_port(2), sin_addr(4), zero(8) }
    let (port, addr) = unsafe {
        let family = *(addr_ptr as *const u16);
        if family != 2 { return -97; } // AF_INET only
        let port = u16::from_be(*(addr_ptr.add(2) as *const u16));
        let ip_bytes = core::slice::from_raw_parts(addr_ptr.add(4), 4);
        let addr = Ipv4Address::new(ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3]);
        (port, addr)
    };

    with_socket(fd, |sock| {
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
            // replacement listener at 2 KiB. 8 listeners × 4 KiB = 32 KiB,
            // small enough not to fragment a busy 4 MiB heap.
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

    crate::serial_println!("[accept] connection established!");

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
    });

    // Fill in peer address if requested
    if !addr_ptr.is_null() {
        if let Some(remote) = remote {
            unsafe {
                // struct sockaddr_in
                *(addr_ptr as *mut u16) = 2; // AF_INET
                *(addr_ptr.add(2) as *mut u16) = remote.port.to_be();
                if let IpAddress::Ipv4(v4) = remote.addr {
                    core::ptr::copy_nonoverlapping(
                        v4.octets().as_ptr(),
                        addr_ptr.add(4),
                        4,
                    );
                }
                if !addrlen_ptr.is_null() {
                    *addrlen_ptr = 16;
                }
            }
        }
    }

    crate::serial_println!("[accept] returning new_fd={}", new_fd);
    new_fd as i64
}

/// getsockname(fd, addr, addrlen) → 0 or error
pub fn sys_getsockname(fd: i32, addr_ptr: *mut u8, addrlen_ptr: *mut u32) -> i64 {
    let local = match with_socket(fd, |sock| (sock.local_port, sock.local_addr)) {
        Some(v) => v,
        None => return -9,
    };

    if !addr_ptr.is_null() {
        unsafe {
            core::ptr::write_bytes(addr_ptr, 0, 16);
            *(addr_ptr as *mut u16) = 2; // AF_INET
            *(addr_ptr.add(2) as *mut u16) = local.0.to_be();
            core::ptr::copy_nonoverlapping(
                local.1.octets().as_ptr(),
                addr_ptr.add(4),
                4,
            );
            if !addrlen_ptr.is_null() {
                *addrlen_ptr = 16;
            }
        }
    }
    0
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
            if !addr_ptr.is_null() {
                unsafe {
                    core::ptr::write_bytes(addr_ptr, 0, 16);
                    *(addr_ptr as *mut u16) = 2; // AF_INET
                    *(addr_ptr.add(2) as *mut u16) = ep.port.to_be();
                    if let IpAddress::Ipv4(v4) = ep.addr {
                        core::ptr::copy_nonoverlapping(
                            v4.octets().as_ptr(),
                            addr_ptr.add(4),
                            4,
                        );
                    }
                    if !addrlen_ptr.is_null() {
                        *addrlen_ptr = 16;
                    }
                }
            }
            0
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
        (6, 1) => 0, // TCP_NODELAY
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
    unsafe {
        let len = *optlen;
        match (level, optname) {
            (1, 12) => { // SO_PRIORITY
                if len >= 4 { *(optval as *mut i32) = 0; *optlen = 4; }
            }
            (0, 1) => { // IP_TOS
                if len >= 4 { *(optval as *mut i32) = 0; *optlen = 4; }
            }
            (1, 13) => { // SO_LINGER
                if len >= 8 {
                    *(optval as *mut i32) = 0; // l_onoff
                    *((optval as *mut i32).add(1)) = 0; // l_linger
                    *optlen = 8;
                }
            }
            (1, 4) => { // SO_ERROR
                if len >= 4 { *(optval as *mut i32) = 0; *optlen = 4; }
            }
            _ => {
                if len >= 4 { *(optval as *mut i32) = 0; *optlen = 4; }
            }
        }
    }
    0
}

/// send/sendto/write on a socket fd
pub fn sys_sendto(fd: i32, buf: *const u8, len: usize, _flags: i32,
                  _dest_addr: *const u8, _addrlen: u32) -> i64 {
    let data = unsafe { core::slice::from_raw_parts(buf, len) };

    with_socket(fd, |sock| {
        match sock.sock_type {
            SockType::TcpStream => {
                crate::net::with_net(|net| {
                    let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                    if !tcp.can_send() {
                        return -11i64; // -EAGAIN
                    }
                    match tcp.send_slice(data) {
                        Ok(sent) => sent as i64,
                        Err(_) => -104, // -ECONNRESET
                    }
                })
            }
            SockType::UdpDgram => -95, // -EOPNOTSUPP
            _ => -9,
        }
    }).unwrap_or(-9)
}

/// recv/recvfrom/read on a socket fd
pub fn sys_recvfrom(fd: i32, buf: *mut u8, len: usize, _flags: i32,
                    _src_addr: *mut u8, _addrlen: *mut u32) -> i64 {
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
                    let dest = unsafe { core::slice::from_raw_parts_mut(buf, len) };
                    match tcp.recv_slice(dest) {
                        Ok(n) => n as i64,
                        Err(_) => -104, // -ECONNRESET
                    }
                })
            }
            _ => -9,
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
                    let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                    let mut events = 0u16;
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
/// Removes the socket from the table under the SOCKETS lock, then
/// hands it to smoltcp under NET_LOCK. Holding both locks while
/// calling tcp.close() avoids leaving a half-removed entry visible
/// to other CPUs.
pub fn close(fd: i32) {
    let mut sockets = SOCKETS.lock();
    let Some(idx) = sockets.iter().position(|s| s.fd == fd) else { return };
    let sock = sockets.remove(idx);
    crate::net::with_net(|net| {
        match sock.sock_type {
            SockType::TcpStream => {
                let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                tcp.close();
                net.sockets.remove(sock.handle);
            }
            SockType::TcpListener => {
                // Listener pool: take down every slot.
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

