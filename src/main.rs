#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod boot;

use core::panic::PanicInfo;
use tyn_kernel::serial_println;

/// ECAM PCI config base for q35 machine (from QEMU seabios/src/hw/dev-q35.h).
const MMCONFIG_BASE: usize = 0xB000_0000;

#[unsafe(no_mangle)]
extern "C" fn main(_mbi: *const u8) -> ! {
    // Serial
    serial_println!("=== Tyn Kernel v{} (multiboot) ===", env!("CARGO_PKG_VERSION"));

    // IDT
    tyn_kernel::interrupts::init_idt();
    serial_println!("[boot] IDT OK");

    // Heap — static array, no page table ops needed
    tyn_kernel::memory::heap::init_static();
    serial_println!("[boot] heap OK");

    // DMA region init
    tyn_kernel::drivers::virtio::hal::init_dma();

    // PCI via ECAM (q35 machine)
    serial_println!("");
    enumerate_pci(MMCONFIG_BASE as *mut u8);

    serial_println!("Entering poll loop");
    loop {
        x86_64::instructions::hlt();
    }
}

fn enumerate_pci(mmconfig_base: *mut u8) {
    use virtio_drivers::transport::pci::bus::{Cam, Command, MmioCam, PciRoot};
    use virtio_drivers::transport::pci::{PciTransport, virtio_device_type};
    use virtio_drivers::transport::{DeviceType, Transport};

    serial_println!("[pci] ECAM base={:#x}", mmconfig_base as u64);
    // Test: read from ECAM base
    let test = unsafe { core::ptr::read_volatile(mmconfig_base as *const u32) };
    serial_println!("[pci] ECAM test read={:#x}", test);
    let mut pci_root = PciRoot::new(unsafe { MmioCam::new(mmconfig_base, Cam::Ecam) });
    serial_println!("[pci] PciRoot created, enumerating...");
    for (device_function, info) in pci_root.enumerate_bus(0) {
        serial_println!(
            "[pci] {}:{}.{} vendor={:#06x} device={:#06x}",
            device_function.bus, device_function.device, device_function.function,
            info.vendor_id, info.device_id,
        );
        if let Some(virtio_type) = virtio_device_type(&info) {
            serial_println!("[pci]   VirtIO {:?}", virtio_type);
            pci_root.set_command(
                device_function,
                Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER,
            );

            let transport = PciTransport::new::<tyn_kernel::drivers::virtio::hal::TynHal, _>(
                &mut pci_root, device_function,
            ).unwrap();
            serial_println!("[pci]   transport OK: {:?}", transport.device_type());

            if transport.device_type() == DeviceType::Network {
                virtio_net(transport);
            }
        }
    }
}

fn virtio_net(transport: impl virtio_drivers::transport::Transport) {
    use tyn_kernel::drivers::virtio::hal::TynHal;
    use virtio_drivers::device::net::VirtIONet;

    const QUEUE_SIZE: usize = 16;
    const BUF_LEN: usize = 2048;

    let net = VirtIONet::<TynHal, _, QUEUE_SIZE>::new(transport, BUF_LEN)
        .expect("failed to create net driver");
    serial_println!("[net] MAC={:02x?}", net.mac_address());

    // Build smoltcp stack
    let mut device = tyn_kernel::net::device::VirtioNetDevice::new(net);
    let iface = tyn_kernel::net::interface::build(&mut device);

    serial_println!("[net] IP=10.0.2.15/24 gw=10.0.2.2");

    let mut sockets = smoltcp::iface::SocketSet::new(alloc::vec::Vec::new());
    let mut echo = tyn_kernel::net::tcp_echo::TcpEchoServer::new(&mut sockets);
    serial_println!("[net] echo server ready");

    // Initial poll to move socket to Listen
    let start_tsc = unsafe { core::arch::x86_64::_rdtsc() };
    let mut iface = iface;
    let millis = || -> i64 {
        let tsc = unsafe { core::arch::x86_64::_rdtsc() };
        (tsc.wrapping_sub(start_tsc) / 2_000_000) as i64
    };

    // Poll once to transition
    let now = smoltcp::time::Instant::from_millis(millis());
    iface.poll(now, &mut device, &mut sockets);
    echo.poll(&mut sockets);

    serial_println!("Entering poll loop");
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
    loop { x86_64::instructions::hlt(); }
}
