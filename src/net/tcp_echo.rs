//! TCP echo server on port 8080.
//!
//! Listens for connections, echoes received bytes back, logs state transitions.

use crate::serial_println;
use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::tcp;

/// Port the echo server listens on.
pub const ECHO_PORT: u16 = 8080;

const RX_BUF_SIZE: usize = 512;
const TX_BUF_SIZE: usize = 512;

/// TCP echo server state.
pub struct TcpEchoServer {
    handle: SocketHandle,
    connected: bool,
}

impl TcpEchoServer {
    /// Create an echo socket and register it with the socket set.
    pub fn new(sockets: &mut SocketSet<'static>) -> Self {
        let rx = tcp::SocketBuffer::new(alloc::vec![0u8; RX_BUF_SIZE]);
        let tx = tcp::SocketBuffer::new(alloc::vec![0u8; TX_BUF_SIZE]);
        let sock = tcp::Socket::new(rx, tx);
        let handle = sockets.add(sock);
        sockets
            .get_mut::<tcp::Socket>(handle)
            .listen(ECHO_PORT)
            .expect("listen");
        Self {
            handle,
            connected: false,
        }
    }

    /// Run one pass of the echo logic. Call this after each `iface.poll()`.
    pub fn poll(&mut self, sockets: &mut SocketSet<'static>) {
        let socket = sockets.get_mut::<tcp::Socket>(self.handle);

        if !socket.is_open() {
            if let Err(e) = socket.listen(ECHO_PORT) {
                serial_println!("[echo] listen failed: {:?}", e);
                return;
            }
        }

        if (socket.may_recv() || socket.may_send()) && !self.connected {
            self.connected = true;
            serial_println!("[echo] connected");
        }
        if !socket.may_recv() && !socket.may_send() && self.connected {
            self.connected = false;
            serial_println!("[echo] closed");
        }

        if socket.can_recv() {
            let mut buf = [0u8; 512];
            match socket.recv_slice(&mut buf) {
                Ok(n) if n > 0 => {
                    if socket.can_send() {
                        if let Err(e) = socket.send_slice(&buf[..n]) {
                            serial_println!("[echo] send error: {:?}", e);
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => serial_println!("[echo] recv error: {:?}", e),
            }
        }
    }
}
