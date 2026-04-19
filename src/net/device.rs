//! smoltcp `Device` implementation wrapping `VirtIONet`.
//!
//! RX: calls `VirtIONet::receive()` directly (no `can_recv` guard).
//! TX: non-blocking `transmit_begin`, completed descriptors drained each poll.

use crate::drivers::virtio::hal::TynHal;
use crate::serial_println;
use alloc::vec::Vec;
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use virtio_drivers::device::net::VirtIONet;
use virtio_drivers::transport::Transport;

const QUEUE_SIZE: usize = 16;

/// smoltcp device backed by a VirtIONet driver.
pub struct VirtioNetDevice<T: Transport> {
    /// The underlying virtio-net driver.
    pub inner: VirtIONet<TynHal, T, QUEUE_SIZE>,
    pending_tx: Vec<(u16, Vec<u8>)>,
}

impl<T: Transport> VirtioNetDevice<T> {
    /// Wrap a `VirtIONet` driver for use with smoltcp.
    pub fn new(inner: VirtIONet<TynHal, T, QUEUE_SIZE>) -> Self {
        Self {
            inner,
            pending_tx: Vec::new(),
        }
    }

    /// Drain completed TX descriptors so buffers can be freed and reused.
    pub fn drain_completed_tx(&mut self) {
        let raw = &mut self.inner.inner;
        while let Some(token) = raw.poll_transmit() {
            if let Some(idx) = self.pending_tx.iter().position(|(t, _)| *t == token) {
                let (_t, buf) = self.pending_tx.remove(idx);
                // SAFETY: `buf` is the same buffer passed to `transmit_begin`
                // when this token was issued.
                unsafe {
                    raw.transmit_complete(token, &buf).ok();
                }
            }
        }
    }

    /// Read the device's MAC address.
    pub fn mac_address(&self) -> [u8; 6] {
        self.inner.mac_address()
    }
}

impl<T: Transport> Device for VirtioNetDevice<T> {
    type RxToken<'a> = VirtioRxToken where T: 'a;
    type TxToken<'a> = VirtioTxToken<'a, T> where T: 'a;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1536;
        caps.max_burst_size = Some(1);
        caps.medium = Medium::Ethernet;
        caps
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self.inner.receive() {
            Ok(rx_buf) => {
                let packet = rx_buf.packet().to_vec();
                serial_println!("[rx] {} bytes", packet.len());
                self.inner.recycle_rx_buffer(rx_buf).unwrap();
                Some((
                    VirtioRxToken { packet },
                    VirtioTxToken { device: self },
                ))
            }
            Err(virtio_drivers::Error::NotReady) => None,
            Err(e) => {
                serial_println!("[rx] error: {:?}", e);
                None
            }
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken { device: self })
    }
}

/// Received packet data, consumed by smoltcp.
pub struct VirtioRxToken {
    packet: Vec<u8>,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.packet)
    }
}

/// Transmit token — holds a mutable reference to the device for sending.
pub struct VirtioTxToken<'a, T: Transport> {
    device: &'a mut VirtioNetDevice<T>,
}

impl<T: Transport> TxToken for VirtioTxToken<'_, T> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.device.drain_completed_tx();

        let raw = &mut self.device.inner.inner;
        let hdr_len = raw.fill_buffer_header(&mut [0u8; 32]).unwrap_or(12);
        let mut buf = alloc::vec![0u8; hdr_len + len];
        raw.fill_buffer_header(&mut buf).expect("fill header");
        let result = f(&mut buf[hdr_len..]);

        serial_println!("[tx] {} bytes", len);
        // SAFETY: `buf` contains a valid virtio-net header followed by the
        // Ethernet frame. The buffer is kept alive in `pending_tx` until
        // `transmit_complete` is called with the matching token.
        match unsafe { raw.transmit_begin(&buf) } {
            Ok(token) => {
                self.device.pending_tx.push((token, buf));
            }
            Err(e) => {
                serial_println!("[tx] ERROR: {:?}", e);
            }
        }
        result
    }
}
