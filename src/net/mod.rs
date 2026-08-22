//! Network stack — device abstraction (virtio-net or ENA), smoltcp
//! interface, socket layer.

pub mod device;
pub mod ena;
pub mod interface;
pub mod pci_io;
pub mod socket;
pub mod tcp_echo;

use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, RxToken, TxToken};
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use smoltcp::wire::IpCidr;
use virtio_drivers::transport::pci::PciTransport;

use crate::net::device::{VirtioNetDevice, VirtioRxToken, VirtioTxToken};
use crate::net::ena::device::{EnaDevice, EnaRxToken, EnaTxToken};
use crate::serial_println;
use smoltcp::wire::Ipv4Address;

/// DNS nameservers from the DHCP lease. Exposed to userspace via the synthetic
/// /etc/resolv.conf and read by tyn_boot to configure inet_res (the pure-Erlang
/// resolver). Empty until DHCP configures.
static DNS_SERVERS: spin::Mutex<alloc::vec::Vec<Ipv4Address>> =
    spin::Mutex::new(alloc::vec::Vec::new());

/// Record DHCP-provided nameservers (idempotent replace).
fn set_dns_servers(cfg: &dhcpv4::Config) {
    let mut v = DNS_SERVERS.lock();
    v.clear();
    for ns in cfg.dns_servers.iter() {
        v.push(*ns);
    }
}

/// Synthesize `/etc/resolv.conf` content from the DHCP nameservers.
/// Empty vec → empty string (tyn_boot then skips DNS setup).
pub fn resolv_conf() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    for ns in DNS_SERVERS.lock().iter() {
        let _ = writeln!(s, "nameserver {}", ns);
    }
    s
}

/// The physical NIC backing smoltcp. virtio-net on QEMU, ENA on AWS Nitro.
/// An enum (not `dyn Device`) because smoltcp's `Device` has GAT token
/// types and isn't object-safe; the enum dispatches per call.
pub enum NetDevice {
    Virtio(VirtioNetDevice<PciTransport>),
    Ena(EnaDevice),
}

impl NetDevice {
    /// Free completed TX descriptors before a poll cycle.
    fn drain_tx(&mut self) {
        match self {
            NetDevice::Virtio(d) => d.drain_completed_tx(),
            NetDevice::Ena(d) => d.drain_tx(),
        }
    }
}

pub enum NetRxToken {
    Virtio(VirtioRxToken),
    Ena(EnaRxToken),
}

pub enum NetTxToken<'a> {
    Virtio(VirtioTxToken<'a, PciTransport>),
    Ena(EnaTxToken<'a>),
}

impl Device for NetDevice {
    type RxToken<'a> = NetRxToken;
    type TxToken<'a> = NetTxToken<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        match self {
            NetDevice::Virtio(d) => d.capabilities(),
            NetDevice::Ena(d) => d.capabilities(),
        }
    }

    fn receive(&mut self, t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self {
            NetDevice::Virtio(d) => d
                .receive(t)
                .map(|(r, tx)| (NetRxToken::Virtio(r), NetTxToken::Virtio(tx))),
            NetDevice::Ena(d) => d
                .receive(t)
                .map(|(r, tx)| (NetRxToken::Ena(r), NetTxToken::Ena(tx))),
        }
    }

    fn transmit(&mut self, t: Instant) -> Option<Self::TxToken<'_>> {
        match self {
            NetDevice::Virtio(d) => d.transmit(t).map(NetTxToken::Virtio),
            NetDevice::Ena(d) => d.transmit(t).map(NetTxToken::Ena),
        }
    }
}

impl RxToken for NetRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            NetRxToken::Virtio(t) => t.consume(f),
            NetRxToken::Ena(t) => t.consume(f),
        }
    }
}

impl TxToken for NetTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        match self {
            NetTxToken::Virtio(t) => t.consume(len, f),
            NetTxToken::Ena(t) => t.consume(len, f),
        }
    }
}

/// Global network state — device, interface, and socket set.
pub struct NetState {
    pub sockets: SocketSet<'static>,
    pub iface: Interface,
    pub device: NetDevice,
    start_tsc: u64,
    /// DHCP client handle (ENA/Nitro only; `None` on virtio/static-IP). Kept
    /// in the SocketSet so smoltcp sends RENEW at T1 / REBIND at T2; `poll()`
    /// applies any address change.
    dhcp_handle: Option<SocketHandle>,
}

impl NetState {
    /// Poll the smoltcp interface — processes incoming/outgoing packets.
    pub fn poll(&mut self) {
        self.device.drain_tx();
        let now = self.now();
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        // DHCP lease renewal: smoltcp issues RENEW/REBIND on its own timers as
        // long as the socket is polled. Apply a re-config if the lease (or IP)
        // changes. On AWS the IP is stable across renewals, so this normally
        // just refreshes the lease and keeps the instance reachable past it.
        if let Some(h) = self.dhcp_handle {
            match self.sockets.get_mut::<dhcpv4::Socket>(h).poll() {
                Some(dhcpv4::Event::Configured(config)) => {
                    serial_println!("[net] DHCP renewed: ip={}", config.address);
                    self.iface.update_ip_addrs(|addrs| {
                        addrs.clear();
                        let _ = addrs.push(IpCidr::Ipv4(config.address));
                    });
                    if let Some(router) = config.router {
                        let _ = self.iface.routes_mut().add_default_ipv4_route(router);
                    }
                    set_dns_servers(&config);
                }
                Some(dhcpv4::Event::Deconfigured) => {
                    serial_println!("[net] DHCP lease lost");
                }
                None => {}
            }
        }
        socket::gc_closed_handles(self);

        // [diag] BUG-8 recovery instrumentation (TEMPORARY — remove with the fix).
        // Every ~2s: free heap + a TCP-state histogram over the whole SocketSet,
        // to trace after a connection flood whether (a) the heap recovers or stays
        // pinned (fix strands refused connections / teardown bug), (b) the listener
        // pool returns to Listen or stays stuck, and (c) the kernel keeps logging
        // while HTTP won't serve (BEAM-level wedge). now_ms() is TSC-based (no lock,
        // no syscall), so this is safe in the poll path.
        {
            use core::sync::atomic::{AtomicU64, Ordering};
            static LAST_MS: AtomicU64 = AtomicU64::new(0);
            let ms = now_ms(self.start_tsc);
            // Gated behind the VERBOSE debug flag (default OFF) so it's silent in
            // production; enable with set_verbose(true) to trace a running node.
            // Prints via serial_println_always! (below) to survive post-boot QUIET.
            if crate::serial::verbose()
                && ms.wrapping_sub(LAST_MS.load(Ordering::Relaxed)) >= 2000 {
                LAST_MS.store(ms, Ordering::Relaxed);
                let (mut total, mut listen, mut estab, mut closewait, mut closing, mut closed) =
                    (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
                for (_h, s) in self.sockets.iter() {
                    if let smoltcp::socket::Socket::Tcp(t) = s {
                        total += 1;
                        match t.state() {
                            smoltcp::socket::tcp::State::Listen => listen += 1,
                            smoltcp::socket::tcp::State::Established => estab += 1,
                            smoltcp::socket::tcp::State::CloseWait => closewait += 1,
                            smoltcp::socket::tcp::State::Closed => closed += 1,
                            _ => closing += 1,
                        }
                    }
                }
                // _always: bypass QUIET (set true post-boot), else these are
                // dropped exactly when we need them — during the flood/recovery.
                crate::serial_println_always!(
                    "[diag] t={}s heap_free={}KiB tcp[total={} listen={} estab={} closewait={} closing={} closed={}]",
                    ms / 1000,
                    crate::memory::heap::free_bytes() / 1024,
                    total, listen, estab, closewait, closing, closed
                );
            }
        }
    }

    fn now(&self) -> Instant {
        Instant::from_millis(now_ms(self.start_tsc) as i64)
    }

    /// Milliseconds since boot (TSC-based, coarse). Used by the BUG-8 teardown
    /// reaper to age sockets stranded in a half-closed state.
    pub(crate) fn uptime_ms(&self) -> u64 {
        now_ms(self.start_tsc)
    }
}

/// Milliseconds since `start_tsc` (TSC at ~2 GHz, matching the rest of the
/// stack's coarse timebase — exact rate isn't critical for smoltcp timers).
fn now_ms(start_tsc: u64) -> u64 {
    let tsc = unsafe { core::arch::x86_64::_rdtsc() };
    tsc.wrapping_sub(start_tsc) / 2_000_000
}

static mut NET_STATE: Option<NetState> = None;
static NET_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Initialize networking with a virtio-net PCI transport (QEMU dev path).
pub fn init_with_transport(transport: PciTransport) {
    use crate::drivers::virtio::hal::TynHal;
    use smoltcp::wire::IpAddress;
    use virtio_drivers::device::net::VirtIONet;

    const QUEUE_SIZE: usize = 64;
    const BUF_LEN: usize = 2048;

    let net = VirtIONet::<TynHal, _, QUEUE_SIZE>::new(transport, BUF_LEN)
        .expect("VirtIONet::new failed");
    let vdev = VirtioNetDevice::new(net);
    let mac = vdev.mac_address();
    serial_println!("[net] virtio-net MAC={:02x?}", mac);

    let mut device = NetDevice::Virtio(vdev);
    let mut iface = interface::build(&mut device, mac);
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(IpAddress::Ipv4(interface::KERNEL_IP), interface::PREFIX_LEN))
            .expect("adding kernel IP failed");
    });
    iface
        .routes_mut()
        .add_default_ipv4_route(interface::GATEWAY_IP)
        .expect("adding default route failed");

    // The virtio dev path is static-IP (no DHCP to learn a nameserver). QEMU's
    // SLIRP user-net always runs a DNS forwarder at 10.0.2.3 (gateway .2), so
    // seed it — this makes /tyn/resolv.conf non-empty and outbound DNS behave
    // under QEMU exactly as it does from a DHCP lease on Nitro.
    DNS_SERVERS.lock().push(Ipv4Address::new(10, 0, 2, 3));

    let sockets = SocketSet::new(alloc::vec::Vec::new());
    let start_tsc = unsafe { core::arch::x86_64::_rdtsc() };
    unsafe {
        NET_STATE = Some(NetState {
            sockets,
            iface,
            device,
            start_tsc,
            dhcp_handle: None,
        });
    }
    serial_println!("[net] initialized (virtio), IP={}", interface::KERNEL_IP);
}

/// Initialize networking with an ENA device (AWS Nitro). Runs a DHCP client
/// to obtain the VPC address before handing off to the socket layer. The
/// DHCP exchange also exercises the RX path (the OFFER/ACK are unicast to
/// our MAC).
pub fn init_with_ena(dev: EnaDevice) {
    let mac = dev.mac_address();
    let mut device = NetDevice::Ena(dev);
    let mut iface = interface::build(&mut device, mac);

    let start_tsc = unsafe { core::arch::x86_64::_rdtsc() };
    let mut sockets = SocketSet::new(alloc::vec::Vec::new());
    let dhcp_handle = sockets.add(dhcpv4::Socket::new());

    serial_println!("[net] ENA up, starting DHCP...");
    // A single 20s attempt was fragile: a slow/dropped first DISCOVER (ARP
    // resolution, DHCP-server load) left Phoenix listening on an unconfigured
    // interface (~1/32 boots). Retry up to 5 times with exponential backoff,
    // resetting the socket to force a fresh DISCOVER each attempt.
    const MAX_DHCP_ATTEMPTS: u32 = 5;
    const ATTEMPT_TIMEOUT_MS: u64 = 10_000;
    let mut configured = false;

    'attempts: for attempt in 1..=MAX_DHCP_ATTEMPTS {
        let attempt_start = now_ms(start_tsc);
        loop {
            let now = Instant::from_millis(now_ms(start_tsc) as i64);
            iface.poll(now, &mut device, &mut sockets);

            match sockets.get_mut::<dhcpv4::Socket>(dhcp_handle).poll() {
                Some(dhcpv4::Event::Configured(config)) => {
                    serial_println!(
                        "[net] DHCP configured on attempt {}: ip={} router={:?}",
                        attempt, config.address, config.router);
                    iface.update_ip_addrs(|addrs| {
                        addrs.clear();
                        let _ = addrs.push(IpCidr::Ipv4(config.address));
                    });
                    if let Some(router) = config.router {
                        let _ = iface.routes_mut().add_default_ipv4_route(router);
                    }
                    set_dns_servers(&config);
                    configured = true;
                    break 'attempts;
                }
                Some(dhcpv4::Event::Deconfigured) => {}
                None => {}
            }

            if now_ms(start_tsc).wrapping_sub(attempt_start) > ATTEMPT_TIMEOUT_MS {
                break; // this attempt timed out
            }
            core::hint::spin_loop();
        }

        if attempt < MAX_DHCP_ATTEMPTS {
            serial_println!("[net] DHCP attempt {}/{} timed out, retrying...", attempt, MAX_DHCP_ATTEMPTS);
            // Force a fresh DISCOVER on the next attempt.
            sockets.get_mut::<dhcpv4::Socket>(dhcp_handle).reset();
            // Exponential backoff (0.5s, 1s, 2s, 4s), still polling the
            // interface so RX/ARP and the new DISCOVER keep flowing.
            let backoff_ms = 500u64 << (attempt - 1).min(3);
            let backoff_start = now_ms(start_tsc);
            while now_ms(start_tsc).wrapping_sub(backoff_start) < backoff_ms {
                let now = Instant::from_millis(now_ms(start_tsc) as i64);
                iface.poll(now, &mut device, &mut sockets);
                core::hint::spin_loop();
            }
        }
    }

    if !configured {
        serial_println!(
            "[net] DHCP failed after {} attempts — networking not configured",
            MAX_DHCP_ATTEMPTS);
    }

    // Keep the DHCP socket in the SocketSet so smoltcp renews the lease
    // (RENEW at T1, REBIND at T2); NetState::poll applies any address change.
    unsafe {
        NET_STATE = Some(NetState {
            sockets,
            iface,
            device,
            start_tsc,
            dhcp_handle: Some(dhcp_handle),
        });
    }
    if configured {
        serial_println!("[net] initialized (ENA) via DHCP (lease renewal active)");
    }
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

/// The configured IPv4 formatted as an ERTS longname `n@a.b.c.d`, or None if no
/// IPv4 is set yet. Formats via the address's `Display` (avoids depending on a
/// particular smoltcp octet accessor). Used at boot to inject a dynamic `-name`
/// so the node comes up distributed on the address it actually got.
pub fn dist_node_name() -> Option<alloc::string::String> {
    if !is_initialized() {
        return None;
    }
    with_net(|net| {
        net.iface.ip_addrs().iter().find_map(|cidr| match cidr {
            IpCidr::Ipv4(v4) => Some(alloc::format!("n@{}", v4.address())),
            _ => None,
        })
    })
}

/// Pump the interface (drives the DHCP DISCOVER→ACK exchange) until an IPv4
/// lease is configured or `timeout_ms` elapses, then return the `n@<ip>`
/// longname. Bounded so a NIC-less or DHCP-timeout boot never hangs — returns
/// None and the node boots non-distributed (unchanged behavior). Called ONCE at
/// boot, before the ERTS argv is built, so the address is known in time for
/// `-name`. On virtio (static IP) this returns immediately; on ENA/Nitro it
/// spins the poll loop until DHCP completes (~1–2 s typical).
pub fn wait_for_dist_name(timeout_ms: u64) -> Option<alloc::string::String> {
    if !is_initialized() {
        crate::serial_println!("[dist] net not initialized — non-distributed boot");
        return None;
    }
    crate::serial_println!("[dist] waiting up to {}ms for an IPv4 (DHCP)…", timeout_ms);
    let deadline =
        crate::syscall::monotonic_ns().saturating_add(timeout_ms.saturating_mul(1_000_000));
    let mut polls: u64 = 0;
    loop {
        poll();
        polls += 1;
        if let Some(name) = dist_node_name() {
            crate::serial_println!("[dist] got {} after {} polls", name, polls);
            return Some(name);
        }
        if crate::syscall::monotonic_ns() >= deadline {
            crate::serial_println!("[dist] no IPv4 after {}ms / {} polls — non-distributed", timeout_ms, polls);
            return None;
        }
        core::hint::spin_loop();
    }
}
