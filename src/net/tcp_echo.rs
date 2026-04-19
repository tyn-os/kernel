//! Simple TCP echo server on port 8080.
//!
//! Listens for incoming connections, echoes any bytes received back to
//! the sender, and logs connection events to serial.

use crate::serial_println;
use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;

/// Port the echo server listens on.
pub const ECHO_PORT: u16 = 8080;

/// RX/TX buffer sizes for the echo socket.
const RX_BUFFER_SIZE: usize = 64;
const TX_BUFFER_SIZE: usize = 64;

/// Create and add the echo socket in a separate stack frame.
#[inline(never)]
fn add_echo_socket(sockets: &mut SocketSet<'static>) -> SocketHandle {
    create_and_listen(sockets)
}

/// Inner helper — socket construction uses its own stack frame.
#[inline(never)]
fn create_and_listen(sockets: &mut SocketSet<'static>) -> SocketHandle {
    serial_println!("[echo] Socket size={}", core::mem::size_of::<tcp::Socket>());
    serial_println!("[echo] alloc rx");
    let rx = tcp::SocketBuffer::new(alloc::vec![0u8; RX_BUFFER_SIZE]);
    serial_println!("[echo] alloc tx");
    let tx = tcp::SocketBuffer::new(alloc::vec![0u8; TX_BUFFER_SIZE]);
    serial_println!("[echo] Socket::new");
    let sock = tcp::Socket::new(rx, tx);
    serial_println!("[echo] add to set");
    let h = sockets.add(sock);
    serial_println!("[echo] listen");
    sockets.get_mut::<tcp::Socket>(h).listen(ECHO_PORT).expect("listen");
    serial_println!("[echo] done");
    h
}

/// State tracked for the echo server socket.
pub struct TcpEchoServer {
    handle: SocketHandle,
    connected: bool,
}

impl TcpEchoServer {
    /// Register a TCP socket with the given socket set and return a handle.
    pub fn new(sockets: &mut SocketSet<'static>) -> Self {
        let handle = add_echo_socket(sockets);
        Self {
            handle,
            connected: false,
        }
    }

    /// Run one pass of the echo server logic against the given socket set.
    pub fn poll(&mut self, sockets: &mut SocketSet<'static>) {
        let socket = sockets.get_mut::<tcp::Socket>(self.handle);

        // Log TCP state transitions
        static LAST_STATE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
        let state_byte = match socket.state() {
            tcp::State::Closed => 0,
            tcp::State::Listen => 1,
            tcp::State::SynReceived => 2,
            tcp::State::Established => 3,
            tcp::State::CloseWait => 4,
            _ => 99,
        };
        let prev = LAST_STATE.swap(state_byte, core::sync::atomic::Ordering::Relaxed);
        if prev != state_byte {
            serial_println!("[echo] state: {} -> {}", prev, state_byte);
        }

        if !socket.is_open() {
            if let Err(e) = socket.listen(ECHO_PORT) {
                serial_println!("[echo] listen failed: {:?}", e);
                return;
            }
            serial_println!("[echo] listening on port {}", ECHO_PORT);
        }

        if socket.may_recv() || socket.may_send() {
            if !self.connected {
                self.connected = true;
                serial_println!("[echo] CONNECTED");
            }
        }
        if !socket.may_recv() && !socket.may_send() && self.connected {
            self.connected = false;
            serial_println!("[echo] CLOSED");
        }

        if socket.can_recv() {
            let mut buf = [0u8; 512];
            match socket.recv_slice(&mut buf) {
                Ok(n) if n > 0 => {
                    if socket.can_send() {
                        if let Err(e) = socket.send_slice(&buf[..n]) {
                            serial_println!("[echo] send failed: {:?}", e);
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => serial_println!("[echo] recv error: {:?}", e),
            }
        }
    }
}
