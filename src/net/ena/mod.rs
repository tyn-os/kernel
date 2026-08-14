//! ENA (Elastic Network Adapter) — AWS Nitro NIC driver.
//!
//! Phase 1 scope: PCI discovery, BAR mapping, version-register read,
//! and (next iteration) admin-queue init + GET_FEATURE round-trip.
//! No data path here yet — Phase 2 lands TX/RX queues and the smoltcp
//! Device trait. See directions/ENA_DRIVER.md for the full plan.

pub mod admin;
pub mod device;
pub mod regs;
pub mod ring;

use crate::serial_println;
use core::ptr::read_volatile;

/// Amazon's PCI vendor ID. Same on every ENA variant.
pub const ENA_VENDOR_ID: u16 = 0x1d0f;

/// All known ENA PCI device IDs (PF/VF, ±LLQ).
/// LLQ = Low Latency Queue — a doorbell-and-data-in-host-memory variant
/// that we ignore in Phase 1; the standard queues work on all variants.
pub const ENA_DEVICE_IDS: [u16; 4] = [0x0ec2, 0x1ec2, 0xec20, 0xec21];

#[inline]
pub fn is_ena(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == ENA_VENDOR_ID && ENA_DEVICE_IDS.contains(&device_id)
}

/// Probe an ENA device that PCI enumeration found. The caller has
/// already enabled MEMORY_SPACE + BUS_MASTER in the PCI command
/// register and resolved BAR0 to a physical (== virtual, thanks to
/// identity mapping) address.
///
/// Phase 1 result: log version registers and DEV_STS so a serial-log
/// dump from Nitro confirms we can actually MMIO-read the device.
pub fn probe(bar0_addr: u64, device_id: u16, location: (u8, u8, u8)) {
    serial_println!(
        "[ena] device {:04x}:{:04x} at {:02x}:{:02x}.{} BAR0={:#x}",
        ENA_VENDOR_ID, device_id, location.0, location.1, location.2, bar0_addr
    );

    // SAFETY: BAR0 is identity-mapped MMIO; reads are aligned u32s.
    unsafe {
        let version      = read_volatile((bar0_addr + regs::VERSION) as *const u32);
        let ctrl_version = read_volatile((bar0_addr + regs::CONTROLLER_VERSION) as *const u32);
        let caps         = read_volatile((bar0_addr + regs::CAPS) as *const u32);
        let caps_ext     = read_volatile((bar0_addr + regs::CAPS_EXT) as *const u32);
        let dev_sts      = read_volatile((bar0_addr + regs::DEV_STS) as *const u32);
        serial_println!(
            "[ena] version={:#010x} ctrl_version={:#010x} caps={:#010x} caps_ext={:#010x} dev_sts={:#010x}",
            version, ctrl_version, caps, caps_ext, dev_sts);
        serial_println!(
            "[ena] reset_timeout={}ms ready={} fatal={}",
            regs::caps_reset_timeout_ms(caps),
            (dev_sts & regs::DEV_STS_READY) != 0,
            (dev_sts & regs::DEV_STS_FATAL_ERROR) != 0);
    }

    serial_println!("[ena] Phase 1 probe complete");
}

/// Phase 2 milestone A: bring up the admin queue and read device attributes
/// (MAC, max MTU). Returns `true` on success. No data path yet — I/O queues
/// and the smoltcp Device trait land in the next milestone.
pub fn init(bar0_addr: u64) -> bool {
    let mut aq = match admin::AdminQueue::init(bar0_addr) {
        Ok(aq) => aq,
        Err(e) => {
            serial_println!("[ena] admin queue init failed: {}", e);
            return false;
        }
    };

    let attrs = match aq.get_device_attributes() {
        Ok(attrs) => attrs,
        Err(e) => {
            serial_println!("[ena] get_device_attributes failed: {}", e);
            return false;
        }
    };
    serial_println!(
        "[ena] dev attrs: mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} max_mtu={} phys_addr_width={} features={:#010x}",
        attrs.mac[0], attrs.mac[1], attrs.mac[2],
        attrs.mac[3], attrs.mac[4], attrs.mac[5],
        attrs.max_mtu, attrs.phys_addr_width, attrs.supported_features);

    // Phase 2B: bring up I/O queues and hand the device to the smoltcp
    // network stack (which runs DHCP to obtain the VPC address).
    match device::EnaDevice::new(&mut aq, bar0_addr, attrs.mac) {
        Ok(dev) => {
            crate::net::init_with_ena(dev);
            true
        }
        Err(e) => {
            serial_println!("[ena] I/O queue setup failed: {}", e);
            false
        }
    }
}
