#![no_std]
#![no_main]

extern crate alloc;

mod boot;

use core::panic::PanicInfo;
use tyn_kernel::serial_println;
use virtio_drivers::transport::pci::bus::{Cam, Command, MmioCam, PciRoot};
use virtio_drivers::transport::pci::{PciTransport, virtio_device_type};
use virtio_drivers::transport::{DeviceType, Transport};

/// ECAM PCI config base for q35 (from QEMU's seabios dev-q35.h).
const MMCONFIG_BASE: usize = 0xB000_0000;
const NET_QUEUE_SIZE: usize = 16;
const NET_BUF_LEN: usize = 2048;

#[unsafe(no_mangle)]
extern "C" fn main(_mbi: *const u8) -> ! {
    serial_println!("=== Tyn Kernel v{} ===", env!("CARGO_PKG_VERSION"));

    tyn_kernel::interrupts::init_idt();
    tyn_kernel::memory::heap::init_static();
    tyn_kernel::drivers::virtio::hal::init_dma();

    enumerate_pci(MMCONFIG_BASE as *mut u8);

    tyn_kernel::halt_loop();
}

fn enumerate_pci(mmconfig_base: *mut u8) {
    // SAFETY: mmconfig_base is the ECAM MMIO region at 0xB0000000,
    // identity-mapped in our page tables. Cam::Ecam describes the
    // 4096-byte-per-function config space layout for q35.
    let mut root = PciRoot::new(unsafe { MmioCam::new(mmconfig_base, Cam::Ecam) });

    for (dev_fn, info) in root.enumerate_bus(0) {
        serial_println!(
            "[pci] {}:{}.{} {:04x}:{:04x}",
            dev_fn.bus, dev_fn.device, dev_fn.function,
            info.vendor_id, info.device_id,
        );

        if let Some(vtype) = virtio_device_type(&info) {
            serial_println!("[pci]   VirtIO {:?}", vtype);
            root.set_command(
                dev_fn,
                Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER,
            );

            let transport =
                PciTransport::new::<tyn_kernel::drivers::virtio::hal::TynHal, _>(&mut root, dev_fn)
                    .expect("PciTransport::new failed");

            if transport.device_type() == DeviceType::Network {
                run_net(transport);
            }
        }
    }
}

fn run_net(transport: impl Transport) -> ! {
    use tyn_kernel::drivers::virtio::hal::TynHal;
    use virtio_drivers::device::net::VirtIONet;

    let net = VirtIONet::<TynHal, _, NET_QUEUE_SIZE>::new(transport, NET_BUF_LEN)
        .expect("VirtIONet::new failed");
    serial_println!("[net] MAC={:02x?}", net.mac_address());

    let mut device = tyn_kernel::net::device::VirtioNetDevice::new(net);
    let mut iface = tyn_kernel::net::interface::build(&mut device);
    let mut sockets = smoltcp::iface::SocketSet::new(alloc::vec::Vec::new());
    let mut echo = tyn_kernel::net::tcp_echo::TcpEchoServer::new(&mut sockets);

    // SAFETY: RDTSC is available on all x86_64 CPUs.
    let start_tsc = unsafe { core::arch::x86_64::_rdtsc() };
    let millis = || -> i64 {
        // SAFETY: RDTSC is available on all x86_64 CPUs.
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        (tsc.wrapping_sub(start_tsc) / 2_000_000) as i64
    };

    // Initial poll to transition socket to Listen state.
    let now = smoltcp::time::Instant::from_millis(millis());
    iface.poll(now, &mut device, &mut sockets);
    echo.poll(&mut sockets);

    serial_println!("[net] listening on port {}", tyn_kernel::net::tcp_echo::ECHO_PORT);

    loop {
        device.drain_completed_tx();
        let now = smoltcp::time::Instant::from_millis(millis());
        iface.poll(now, &mut device, &mut sockets);
        echo.poll(&mut sockets);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!("KERNEL PANIC: {}", info);
    tyn_kernel::halt_loop();
}
