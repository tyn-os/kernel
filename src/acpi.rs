//! Minimal ACPI parser — finds RSDP, RSDT, and MADT to discover CPU Local APIC IDs.
//!
//! Only parses what's needed for SMP: the MADT table's Local APIC entries.
//! No AML interpreter, no DSDT, no power management.

use crate::serial_println;

/// Maximum number of CPUs we support.
pub const MAX_CPUS: usize = 16;

/// Discovered CPU information.
#[derive(Clone, Copy)]
pub struct CpuInfo {
    pub apic_id: u8,
    pub is_bsp: bool,
    pub processor_id: u8,
}

/// I/O APIC information.
#[derive(Clone, Copy)]
pub struct IoApicInfo {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

/// Result of ACPI parsing.
pub struct AcpiInfo {
    pub cpus: [CpuInfo; MAX_CPUS],
    pub num_cpus: usize,
    pub ioapic: Option<IoApicInfo>,
    pub local_apic_addr: u32,
}

/// RSDP signature: "RSD PTR " (8 bytes)
const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";

/// Find RSDP by scanning the BIOS data area.
/// RSDP lives on a 16-byte boundary in 0xE0000-0xFFFFF.
fn find_rsdp() -> Option<u64> {
    let start = 0xE0000u64;
    let end = 0x100000u64;

    let mut addr = start;
    while addr < end {
        let ptr = addr as *const u8;
        // SAFETY: identity-mapped BIOS area, read-only scan.
        let sig = unsafe { core::slice::from_raw_parts(ptr, 8) };
        if sig == RSDP_SIGNATURE {
            // Verify checksum (first 20 bytes must sum to 0 mod 256)
            let rsdp = unsafe { core::slice::from_raw_parts(ptr, 20) };
            let sum: u8 = rsdp.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            if sum == 0 {
                return Some(addr);
            }
        }
        addr += 16; // RSDP is always 16-byte aligned
    }
    None
}

/// Parse the RSDT to find the MADT.
/// RSDT contains an array of 32-bit pointers to other ACPI tables.
fn find_madt(rsdt_addr: u32) -> Option<u64> {
    let rsdt = rsdt_addr as *const u8;
    // SAFETY: identity-mapped physical memory.
    unsafe {
        // RSDT header: signature(4) + length(4) + revision(1) + checksum(1) + ...
        let sig = core::slice::from_raw_parts(rsdt, 4);
        if sig != b"RSDT" {
            serial_println!("[acpi] RSDT signature mismatch");
            return None;
        }
        let length = *(rsdt.add(4) as *const u32);
        // Entries start at offset 36 (after the fixed header)
        let entries_start = 36;
        let num_entries = (length as usize - entries_start) / 4;

        for i in 0..num_entries {
            let entry_addr = *(rsdt.add(entries_start + i * 4) as *const u32);
            let table = entry_addr as *const u8;
            let table_sig = core::slice::from_raw_parts(table, 4);
            if table_sig == b"APIC" {
                return Some(entry_addr as u64);
            }
        }
    }
    None
}

/// Parse the MADT (Multiple APIC Description Table) to discover CPUs.
fn parse_madt(madt_addr: u64) -> AcpiInfo {
    let mut info = AcpiInfo {
        cpus: [CpuInfo { apic_id: 0, is_bsp: false, processor_id: 0 }; MAX_CPUS],
        num_cpus: 0,
        ioapic: None,
        local_apic_addr: 0xFEE0_0000, // default
    };

    let madt = madt_addr as *const u8;
    // SAFETY: identity-mapped physical memory.
    unsafe {
        let length = *(madt.add(4) as *const u32) as usize;
        // Local APIC address at offset 36
        info.local_apic_addr = *(madt.add(36) as *const u32);

        // Entries start at offset 44
        let mut offset = 44usize;
        while offset + 2 <= length {
            let entry_type = *madt.add(offset);
            let entry_len = *madt.add(offset + 1) as usize;
            if entry_len == 0 { break; } // prevent infinite loop

            match entry_type {
                0 => {
                    // Type 0: Local APIC
                    // Bytes: type(1) + len(1) + processor_id(1) + apic_id(1) + flags(4)
                    if entry_len >= 8 && info.num_cpus < MAX_CPUS {
                        let processor_id = *madt.add(offset + 2);
                        let apic_id = *madt.add(offset + 3);
                        let flags = *(madt.add(offset + 4) as *const u32);
                        // Bit 0: Processor Enabled, Bit 1: Online Capable
                        if (flags & 1) != 0 {
                            info.cpus[info.num_cpus] = CpuInfo {
                                apic_id,
                                is_bsp: info.num_cpus == 0, // first enabled = BSP
                                processor_id,
                            };
                            info.num_cpus += 1;
                        }
                    }
                }
                1 => {
                    // Type 1: I/O APIC
                    // Bytes: type(1) + len(1) + id(1) + reserved(1) + address(4) + gsi_base(4)
                    if entry_len >= 12 {
                        let id = *madt.add(offset + 2);
                        let address = *(madt.add(offset + 4) as *const u32);
                        let gsi_base = *(madt.add(offset + 8) as *const u32);
                        info.ioapic = Some(IoApicInfo { id, address, gsi_base });
                    }
                }
                _ => {} // Skip other entry types (NMI, override, etc.)
            }

            offset += entry_len;
        }
    }

    info
}

/// Discover CPUs via ACPI. Returns None if ACPI tables aren't found.
pub fn discover_cpus() -> Option<AcpiInfo> {
    let rsdp_addr = find_rsdp()?;
    serial_println!("[acpi] RSDP at {:#x}", rsdp_addr);

    // RSDP: at offset 16 is the RSDT address (32-bit)
    let rsdt_addr = unsafe { *((rsdp_addr + 16) as *const u32) };
    serial_println!("[acpi] RSDT at {:#x}", rsdt_addr);

    let madt_addr = find_madt(rsdt_addr)?;
    serial_println!("[acpi] MADT at {:#x}", madt_addr);

    let info = parse_madt(madt_addr);
    serial_println!("[acpi] {} CPUs found, Local APIC at {:#x}",
        info.num_cpus, info.local_apic_addr);
    for i in 0..info.num_cpus {
        serial_println!("[acpi]   CPU {}: APIC ID {} {}",
            i, info.cpus[i].apic_id,
            if info.cpus[i].is_bsp { "(BSP)" } else { "" });
    }
    if let Some(ref ioapic) = info.ioapic {
        serial_println!("[acpi]   I/O APIC ID {} at {:#x} GSI base {}",
            ioapic.id, ioapic.address, ioapic.gsi_base);
    }

    Some(info)
}
