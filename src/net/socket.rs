//! POSIX socket layer bridging ERTS syscalls to smoltcp.
//!
//! Provides socket/bind/listen/accept/send/recv/getsockopt/setsockopt
//! for TCP and UDP, backed by smoltcp's socket abstractions.
//!
//! Design follows Nanos (nanovms/nanos): each socket fd maps to a smoltcp
//! SocketHandle. The fd table coexists with VFS fds (which use 1000+).
//! Socket fds start at 500 to avoid collisions.

use alloc::vec::Vec;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::serial_println;

/// Base fd for socket allocations (avoids collision with VFS fds at 1000+
/// and pipe fds at 200+).
const SOCK_FD_BASE: i32 = 500;
const MAX_SOCKETS: usize = 32;

/// Socket type
#[derive(Clone, Copy, PartialEq)]
enum SockType {
    TcpStream,
    TcpListener,
    UdpDgram,
}

/// Per-socket state
struct Socket {
    fd: i32,
    sock_type: SockType,
    handle: SocketHandle,
    nonblock: bool,
    /// For listeners: accepted connections waiting to be returned
    backlog: Vec<SocketHandle>,
    /// Local address after bind
    local_port: u16,
    local_addr: Ipv4Address,
}

/// Global socket table
static mut SOCKETS: Vec<Socket> = Vec::new();
static mut NEXT_SOCK_FD: i32 = SOCK_FD_BASE;

/// Check if an fd is a socket fd
pub fn is_socket_fd(fd: i32) -> bool {
    unsafe { SOCKETS.iter().any(|s| s.fd == fd) }
}

fn find_socket(fd: i32) -> Option<&'static mut Socket> {
    unsafe { SOCKETS.iter_mut().find(|s| s.fd == fd) }
}

fn alloc_fd() -> i32 {
    unsafe {
        let fd = NEXT_SOCK_FD;
        NEXT_SOCK_FD += 1;
        fd
    }
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

    unsafe {
        SOCKETS.push(Socket {
            fd,
            sock_type: st,
            handle,
            nonblock,
            backlog: Vec::new(),
            local_port: 0,
            local_addr: Ipv4Address::UNSPECIFIED,
        });
    }

    fd as i64
}

/// bind(fd, addr, addrlen) → 0 or error
pub fn sys_bind(fd: i32, addr_ptr: *const u8, _addrlen: u32) -> i64 {
    let sock = match find_socket(fd) {
        Some(s) => s,
        None => return -9, // -EBADF
    };

    // Parse struct sockaddr_in { sa_family(2), sin_port(2), sin_addr(4), zero(8) }
    let (port, addr) = unsafe {
        let family = *(addr_ptr as *const u16);
        if family != 2 { return -97; } // AF_INET only
        let port = u16::from_be(*(addr_ptr.add(2) as *const u16));
        let ip_bytes = core::slice::from_raw_parts(addr_ptr.add(4), 4);
        let addr = Ipv4Address::new(ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3]);
        (port, addr)
    };

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
                    Ok(()) => {},
                    Err(_) => return -98i64, // -EADDRINUSE
                }
                0
            })
        }
        SockType::TcpStream | SockType::TcpListener => {
            // TCP bind is deferred to listen/connect
            0
        }
    }
}

/// listen(fd, backlog) → 0 or error
pub fn sys_listen(fd: i32, _backlog: i32) -> i64 {
    let sock = match find_socket(fd) {
        Some(s) => s,
        None => return -9,
    };

    sock.sock_type = SockType::TcpListener;

    crate::net::with_net(|net| {
        let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
        let endpoint = if sock.local_addr == Ipv4Address::UNSPECIFIED {
            IpListenEndpoint { addr: None, port: sock.local_port }
        } else {
            IpListenEndpoint {
                addr: Some(IpAddress::Ipv4(sock.local_addr)),
                port: sock.local_port,
            }
        };
        match tcp.listen(endpoint) {
            Ok(()) => 0,
            Err(_) => -98, // -EADDRINUSE
        }
    })
}

/// accept4(fd, addr, addrlen, flags) → new_fd or error
pub fn sys_accept(fd: i32, addr_ptr: *mut u8, addrlen_ptr: *mut u32, flags: i32) -> i64 {
    let sock = match find_socket(fd) {
        Some(s) => s,
        None => return -9,
    };

    if sock.sock_type != SockType::TcpListener {
        return -95; // -EOPNOTSUPP
    }

    // Poll the network and wait for an incoming connection.
    // For blocking sockets, loop with poll + yield until established.
    loop {
        crate::net::poll();

        let state = crate::net::with_net(|net| {
            let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
            tcp.state()
        });

        if state == tcp::State::Established || state == tcp::State::CloseWait {
            break;
        }

        if sock.nonblock || (flags & 0x800) != 0 {
            return -11; // -EAGAIN
        }

        // Blocking: yield and retry
        crate::sched::yield_current();
    }

    crate::serial_println!("[accept] connection established!");

    // Connection established on the listening socket.
    // In smoltcp, a listening socket transitions to "established" when
    // a connection arrives. We need to extract it and create a new
    // listening socket for the next connection.
    let remote = crate::net::with_net(|net| {
        let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
        tcp.remote_endpoint()
    });

    // Create a new socket for the accepted connection
    // (swap: the current handle becomes the accepted connection,
    //  create a fresh listener on the same port)
    let accepted_handle = sock.handle;
    let listen_port = sock.local_port;
    let listen_addr = sock.local_addr;

    let new_listener_handle = crate::net::with_net(|net| {
        let rx_buf = tcp::SocketBuffer::new(alloc::vec![0u8; 8192]);
        let tx_buf = tcp::SocketBuffer::new(alloc::vec![0u8; 8192]);
        let new_tcp = tcp::Socket::new(rx_buf, tx_buf);
        let h = net.sockets.add(new_tcp);
        let tcp = net.sockets.get_mut::<tcp::Socket>(h);
        let endpoint = if listen_addr == Ipv4Address::UNSPECIFIED {
            IpListenEndpoint { addr: None, port: listen_port }
        } else {
            IpListenEndpoint {
                addr: Some(IpAddress::Ipv4(listen_addr)),
                port: listen_port,
            }
        };
        tcp.listen(endpoint).ok();
        h
    });

    // Update the listener socket to use the new handle
    sock.handle = new_listener_handle;

    // Create a new fd for the accepted connection
    let new_fd = alloc_fd();
    let nonblock = (flags & 0x800) != 0;

    unsafe {
        SOCKETS.push(Socket {
            fd: new_fd,
            sock_type: SockType::TcpStream,
            handle: accepted_handle,
            nonblock,
            backlog: Vec::new(),
            local_port: listen_port,
            local_addr: listen_addr,
        });
    }

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
    let sock = match find_socket(fd) {
        Some(s) => s,
        None => return -9,
    };

    if !addr_ptr.is_null() {
        unsafe {
            core::ptr::write_bytes(addr_ptr, 0, 16);
            *(addr_ptr as *mut u16) = 2; // AF_INET
            *(addr_ptr.add(2) as *mut u16) = sock.local_port.to_be();
            core::ptr::copy_nonoverlapping(
                sock.local_addr.octets().as_ptr(),
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
    let sock = match find_socket(fd) {
        Some(s) => s,
        None => return -9,
    };

    let remote = crate::net::with_net(|net| {
        let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
        tcp.remote_endpoint()
    });

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
    let sock = match find_socket(fd) {
        Some(s) => s,
        None => return -9,
    };

    let data = unsafe { core::slice::from_raw_parts(buf, len) };

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
        SockType::UdpDgram => {
            // For sendto with destination address
            // TODO: parse dest_addr
            -95 // -EOPNOTSUPP for now
        }
        _ => -9,
    }
}

/// recv/recvfrom/read on a socket fd
pub fn sys_recvfrom(fd: i32, buf: *mut u8, len: usize, _flags: i32,
                    _src_addr: *mut u8, _addrlen: *mut u32) -> i64 {
    let sock = match find_socket(fd) {
        Some(s) => s,
        None => return -9,
    };

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
}

/// Check socket readiness for ppoll/select.
/// Returns POLLIN/POLLOUT bitmask.
pub fn poll_socket(fd: i32) -> u16 {
    const POLLIN: u16 = 0x0001;
    const POLLOUT: u16 = 0x0004;
    const POLLHUP: u16 = 0x0010;
    const POLLERR: u16 = 0x0008;

    let sock = match find_socket(fd) {
        Some(s) => s,
        None => return 0,
    };

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
                let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                let mut events = 0u16;
                // Listener is "readable" when a connection is established
                if tcp.is_active() { events |= POLLIN; }
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
}

/// Check if any socket has a pending event (connection ready, data available).
pub fn any_socket_ready() -> bool {
    unsafe {
        for sock in SOCKETS.iter() {
            if sock.fd < 0 { continue; }
            if poll_socket(sock.fd) & 0x0001 != 0 { // POLLIN
                return true;
            }
        }
    }
    false
}

/// Close a socket fd
pub fn close(fd: i32) {
    unsafe {
        if let Some(idx) = SOCKETS.iter().position(|s| s.fd == fd) {
            let sock = &SOCKETS[idx];
            crate::net::with_net(|net| {
                match sock.sock_type {
                    SockType::TcpStream | SockType::TcpListener => {
                        let tcp = net.sockets.get_mut::<tcp::Socket>(sock.handle);
                        tcp.close();
                    }
                    SockType::UdpDgram => {
                        let udp = net.sockets.get_mut::<udp::Socket>(sock.handle);
                        udp.close();
                    }
                }
                // Remove from smoltcp after close
                net.sockets.remove(sock.handle);
            });
            SOCKETS.remove(idx);
        }
    }
}
