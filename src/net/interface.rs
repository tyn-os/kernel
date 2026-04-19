//! smoltcp Interface configuration — static IP for QEMU user-mode networking.

use crate::net::device::VirtioNetDevice;
use smoltcp::iface::{Config, Interface};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};
use virtio_drivers::transport::Transport;

pub const KERNEL_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
pub const GATEWAY_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
pub const PREFIX_LEN: u8 = 24;

pub fn build<T: Transport>(device: &mut VirtioNetDevice<T>) -> Interface {
    let mac = EthernetAddress(device.mac_address());
    let mut config = Config::new(HardwareAddress::Ethernet(mac));
    let tsc = unsafe { core::arch::x86_64::_rdtsc() };
    config.random_seed = tsc;

    let mut iface = Interface::new(config, device, Instant::from_millis(0));
    iface.set_any_ip(true);

    iface.update_ip_addrs(|ip_addrs| {
        ip_addrs
            .push(IpCidr::new(IpAddress::Ipv4(KERNEL_IP), PREFIX_LEN))
            .expect("adding kernel IP failed");
    });

    iface
        .routes_mut()
        .add_default_ipv4_route(GATEWAY_IP)
        .expect("adding default route failed");

    iface
}
