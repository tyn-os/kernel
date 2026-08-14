//! ENA I/O data path — persistent device + smoltcp `Device` impl (Phase 2B).
//!
//! Owns one RX and one TX I/O queue pair (host placement) plus their DMA
//! buffers, and bridges them to smoltcp. Descriptor/cdesc layouts are from
//! amzn-drivers `ena_eth_io_defs.h`; the SQ doorbell is
//! `writel(sq.tail, bar0 + sq_doorbell_offset)`
//! (`ena_eth_com.h:ena_com_write_{rx,tx}_sq_doorbell`). x86 DMA to
//! write-back memory is cache-coherent, so volatile phase-bit reads suffice.

use super::admin::{self, AdminQueue, SQ_DIR_RX, SQ_DIR_TX};
use crate::serial_println;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

const IO_DEPTH: u16 = 256;
const Q_MASK: u16 = IO_DEPTH - 1;
const RX_DESC_SIZE: usize = 16; // sizeof(ena_eth_io_rx_desc)
const TX_DESC_SIZE: usize = 16; // sizeof(ena_eth_io_tx_desc)
const RX_CDESC_SIZE: usize = 16; // sizeof(ena_eth_io_rx_cdesc_base)
const TX_CDESC_SIZE: usize = 8; // sizeof(ena_eth_io_tx_cdesc)
const BUF_SIZE: usize = 2048;
const N_RX_BUFS: u16 = IO_DEPTH - 1; // keep one slot empty (full/empty disambiguation)
const MTU: usize = 1500;

// rx_desc.ctrl bits.
const RX_CTRL_FIRST: u8 = 1 << 2;
const RX_CTRL_LAST: u8 = 1 << 3;
const RX_CTRL_COMP_REQ: u8 = 1 << 4;
// tx_desc.len_ctrl bits.
const TX_FIRST: u32 = 1 << 26;
const TX_LAST: u32 = 1 << 27;
const TX_COMP_REQ: u32 = 1 << 28;
// rx_cdesc_base.status phase bit.
const RX_CDESC_PHASE_SHIFT: u32 = 24;

pub struct EnaDevice {
    bar0: u64,
    mac: [u8; 6],
    // RX queue
    rx_sq: u64,
    rx_cq: u64,
    rx_db: u32,
    rx_sq_tail: u16,
    rx_sq_phase: u8,
    rx_cq_head: u16,
    rx_cq_phase: u8,
    rx_bufs: u64,
    // TX queue
    tx_sq: u64,
    tx_cq: u64,
    tx_db: u32,
    tx_sq_tail: u16,
    tx_sq_phase: u8,
    tx_cq_head: u16,
    tx_cq_phase: u8,
    tx_bufs: u64,
    tx_next_to_comp: u16,
}

impl EnaDevice {
    /// Create the I/O queues, post RX buffers, and return a ready device.
    pub fn new(aq: &mut AdminQueue, bar0: u64, mac: [u8; 6]) -> Result<Self, &'static str> {
        let rx_cq = admin::dma_alloc(IO_DEPTH as usize * RX_CDESC_SIZE);
        let rx_sq = admin::dma_alloc(IO_DEPTH as usize * RX_DESC_SIZE);
        let tx_cq = admin::dma_alloc(IO_DEPTH as usize * TX_CDESC_SIZE);
        let tx_sq = admin::dma_alloc(IO_DEPTH as usize * TX_DESC_SIZE);
        let rx_bufs = admin::dma_alloc(N_RX_BUFS as usize * BUF_SIZE);
        let tx_bufs = admin::dma_alloc(IO_DEPTH as usize * BUF_SIZE);

        // CQ before SQ (the SQ references the cq index).
        let rx_cq_idx = aq.create_io_cq(IO_DEPTH, rx_cq, RX_CDESC_SIZE as u8)?;
        let (_, rx_db) = aq.create_io_sq(SQ_DIR_RX, IO_DEPTH, rx_sq, rx_cq_idx)?;
        let tx_cq_idx = aq.create_io_cq(IO_DEPTH, tx_cq, TX_CDESC_SIZE as u8)?;
        let (_, tx_db) = aq.create_io_sq(SQ_DIR_TX, IO_DEPTH, tx_sq, tx_cq_idx)?;
        serial_println!(
            "[ena] I/O queues created: rx_cq={} rx_db={:#x} tx_cq={} tx_db={:#x}",
            rx_cq_idx, rx_db, tx_cq_idx, tx_db);

        let mut dev = EnaDevice {
            bar0, mac,
            rx_sq, rx_cq, rx_db,
            rx_sq_tail: 0, rx_sq_phase: 1, rx_cq_head: 0, rx_cq_phase: 1, rx_bufs,
            tx_sq, tx_cq, tx_db,
            tx_sq_tail: 0, tx_sq_phase: 1, tx_cq_head: 0, tx_cq_phase: 1, tx_bufs,
            tx_next_to_comp: 0,
        };

        // Post the initial RX buffers (req_id == buffer index).
        for i in 0..N_RX_BUFS {
            dev.post_rx_desc(i);
        }
        dev.ring_rx_doorbell();
        serial_println!("[ena] posted {} RX buffers", N_RX_BUFS);

        Ok(dev)
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Write an RX descriptor at the current SQ tail pointing at buffer
    /// `req_id`, and advance the tail (does not ring the doorbell).
    fn post_rx_desc(&mut self, req_id: u16) {
        let slot = super::ring::slot(self.rx_sq_tail, Q_MASK) as u64;
        let buf = self.rx_bufs + req_id as u64 * BUF_SIZE as u64;
        let desc = self.rx_sq + slot * RX_DESC_SIZE as u64;
        // SAFETY: slot < IO_DEPTH within rx_sq; buf within rx_bufs.
        unsafe {
            write_volatile(desc as *mut u16, BUF_SIZE as u16); // length
            write_volatile((desc + 2) as *mut u8, 0); // reserved2
            write_volatile((desc + 3) as *mut u8,
                RX_CTRL_FIRST | RX_CTRL_LAST | RX_CTRL_COMP_REQ | (self.rx_sq_phase & 1)); // ctrl
            write_volatile((desc + 4) as *mut u16, req_id);
            write_volatile((desc + 6) as *mut u16, 0); // reserved6
            write_volatile((desc + 8) as *mut u32, buf as u32); // buff_addr_lo
            write_volatile((desc + 12) as *mut u16, (buf >> 32) as u16); // buff_addr_hi
            write_volatile((desc + 14) as *mut u16, 0); // reserved16_w3
        }
        let (t, p) = super::ring::sq_advance(self.rx_sq_tail, self.rx_sq_phase, Q_MASK);
        self.rx_sq_tail = t;
        self.rx_sq_phase = p;
    }

    fn ring_rx_doorbell(&self) {
        // SAFETY: rx_db is a BAR0-relative MMIO doorbell offset from CREATE_SQ.
        unsafe { write_volatile((self.bar0 + self.rx_db as u64) as *mut u32, self.rx_sq_tail as u32); }
    }

    fn ring_tx_doorbell(&self) {
        // SAFETY: tx_db is a BAR0-relative MMIO doorbell offset from CREATE_SQ.
        unsafe { write_volatile((self.bar0 + self.tx_db as u64) as *mut u32, self.tx_sq_tail as u32); }
    }

    /// Reap TX completions so descriptor/buffer slots can be reused.
    pub fn drain_tx(&mut self) {
        loop {
            let cdesc = self.tx_cq + self.tx_cq_head as u64 * TX_CDESC_SIZE as u64;
            // tx_cdesc.flags is byte 3; phase is bit 0.
            let flags = unsafe { read_volatile((cdesc + 3) as *const u8) };
            if !super::ring::entry_ready(flags, self.tx_cq_phase) {
                break;
            }
            self.tx_next_to_comp = self.tx_next_to_comp.wrapping_add(1);
            let (h, p) = super::ring::cq_advance(self.tx_cq_head, self.tx_cq_phase, IO_DEPTH);
            self.tx_cq_head = h;
            self.tx_cq_phase = p;
        }
    }

    fn tx_free_slots(&self) -> u16 {
        super::ring::free_slots(self.tx_sq_tail, self.tx_next_to_comp, IO_DEPTH)
    }
}

impl Device for EnaDevice {
    type RxToken<'a> = EnaRxToken;
    type TxToken<'a> = EnaTxToken<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = MTU;
        caps.max_burst_size = Some(1);
        caps.medium = Medium::Ethernet;
        caps
    }

    fn receive(&mut self, _t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let cdesc = self.rx_cq + self.rx_cq_head as u64 * RX_CDESC_SIZE as u64;
        let status = unsafe { read_volatile(cdesc as *const u32) };
        if !super::ring::entry_ready(((status >> RX_CDESC_PHASE_SHIFT) & 1) as u8, self.rx_cq_phase) {
            return None;
        }
        let len = unsafe { read_volatile((cdesc + 4) as *const u16) } as usize;
        let req_id = unsafe { read_volatile((cdesc + 6) as *const u16) };

        // Copy the frame out of the DMA buffer so the buffer can be re-posted.
        let mut packet = Vec::new();
        if req_id < N_RX_BUFS && len <= BUF_SIZE {
            let src = (self.rx_bufs + req_id as u64 * BUF_SIZE as u64) as *const u8;
            packet.resize(len, 0);
            // SAFETY: src is buffer `req_id`, len <= BUF_SIZE; coherent DMA.
            unsafe { core::ptr::copy_nonoverlapping(src, packet.as_mut_ptr(), len); }
        }

        // Advance the CQ.
        let (h, p) = super::ring::cq_advance(self.rx_cq_head, self.rx_cq_phase, IO_DEPTH);
        self.rx_cq_head = h;
        self.rx_cq_phase = p;

        // Re-post the same buffer for future receives.
        if req_id < N_RX_BUFS {
            self.post_rx_desc(req_id);
            self.ring_rx_doorbell();
        }

        Some((EnaRxToken { packet }, EnaTxToken { dev: self }))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Self::TxToken<'_>> {
        self.drain_tx();
        Some(EnaTxToken { dev: self })
    }
}

pub struct EnaRxToken {
    packet: Vec<u8>,
}

impl RxToken for EnaRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.packet)
    }
}

pub struct EnaTxToken<'a> {
    dev: &'a mut EnaDevice,
}

impl TxToken for EnaTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let dev = self.dev;
        dev.drain_tx();

        let slot = super::ring::slot(dev.tx_sq_tail, Q_MASK) as u64;
        let buf = dev.tx_bufs + slot * BUF_SIZE as u64;
        let req_id = super::ring::slot(dev.tx_sq_tail, Q_MASK) as u32;

        // Let smoltcp build the frame directly in the (coherent) DMA buffer.
        // SAFETY: buf is TX buffer `slot`, len <= MTU < BUF_SIZE.
        let result = {
            let frame = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len.min(BUF_SIZE)) };
            f(frame)
        };

        if dev.tx_free_slots() == 0 {
            serial_println!("[ena] TX ring full, dropping frame");
            return result;
        }

        let desc = dev.tx_sq + slot * TX_DESC_SIZE as u64;
        let len_ctrl = (len as u32 & 0xffff)
            | ((dev.tx_sq_phase as u32) << 24)
            | TX_FIRST | TX_LAST | TX_COMP_REQ;
        // SAFETY: slot < IO_DEPTH within tx_sq.
        unsafe {
            write_volatile(desc as *mut u32, len_ctrl);
            write_volatile((desc + 4) as *mut u32, (req_id & 0x3ff) << 22); // meta_ctrl: req_id_lo
            write_volatile((desc + 8) as *mut u32, buf as u32); // buff_addr_lo
            write_volatile((desc + 12) as *mut u32, (buf >> 32) as u32 & 0xffff); // addr_hi, hdr_sz=0
        }
        let (t, p) = super::ring::sq_advance(dev.tx_sq_tail, dev.tx_sq_phase, Q_MASK);
        dev.tx_sq_tail = t;
        dev.tx_sq_phase = p;
        dev.ring_tx_doorbell();

        result
    }
}
