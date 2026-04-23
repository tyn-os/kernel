//! Network stack — virtio-net device wrapper, smoltcp interface, socket layer.

pub mod device;
pub mod interface;
pub mod socket;
pub mod tcp_echo;

use smoltcp::iface::{Interface, SocketSet};
use smoltcp::time::Instant;
use virtio_drivers::transport::pci::PciTransport;

use crate::net::device::VirtioNetDevice;

/// Global network state — device, interface, and socket set.
/// Initialized by `init_with_transport()` during PCI enumeration.
pub struct NetState {
    pub sockets: SocketSet<'static>,
    pub iface: Interface,
    pub device: VirtioNetDevice<PciTransport>,
    start_tsc: u64,
}

impl NetState {
    /// Poll the smoltcp interface — processes incoming/outgoing packets.
    /// Must be called periodically (from ppoll handler or timer).
    pub fn poll(&mut self) {
        self.device.drain_completed_tx();
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
    }

    fn now(&self) -> Instant {
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        let ms = tsc.wrapping_sub(self.start_tsc) / 2_000_000;
        Instant::from_millis(ms as i64)
    }
}

static mut NET_STATE: Option<NetState> = None;
static NET_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Initialize networking with a virtio-net PCI transport.
/// Called from main.rs after PCI enumeration finds a network device.
pub fn init_with_transport(transport: PciTransport) {
    use crate::drivers::virtio::hal::TynHal;
    use crate::serial_println;
    use virtio_drivers::device::net::VirtIONet;

    const QUEUE_SIZE: usize = 16;
    const BUF_LEN: usize = 2048;

    let net = VirtIONet::<TynHal, _, QUEUE_SIZE>::new(transport, BUF_LEN)
        .expect("VirtIONet::new failed");
    serial_println!("[net] MAC={:02x?}", net.mac_address());

    let mut device = VirtioNetDevice::new(net);
    let iface = interface::build(&mut device);
    let sockets = SocketSet::new(alloc::vec::Vec::new());
    let start_tsc = unsafe { core::arch::x86_64::_rdtsc() };

    unsafe {
        NET_STATE = Some(NetState {
            sockets,
            iface,
            device,
            start_tsc,
        });
    }

    serial_println!("[net] initialized, IP={}", interface::KERNEL_IP);
}

/// Access the global network state (SMP-safe via spinlock).
pub fn with_net<F, R>(f: F) -> R
where
    F: FnOnce(&mut NetState) -> R,
{
    let _lock = NET_LOCK.lock();
    unsafe {
        match NET_STATE.as_mut() {
            Some(state) => f(state),
            None => panic!("net not initialized"),
        }
    }
}

/// Poll the network stack (SMP-safe via spinlock).
pub fn poll() {
    let _lock = NET_LOCK.lock();
    unsafe {
        if let Some(state) = NET_STATE.as_mut() {
            state.poll();
        }
    }
}

/// Check if networking is initialized.
pub fn is_initialized() -> bool {
    unsafe { NET_STATE.is_some() }
}
