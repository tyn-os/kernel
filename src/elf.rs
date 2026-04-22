//! Minimal ELF64 loader for static executables.
//!
//! Parses ELF headers, maps PT_LOAD segments into identity-mapped memory,
//! and returns the entry point address. Follows the pattern from Kerla/rCore.

use crate::serial_println;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ET_EXEC: u16 = 2;
const PT_LOAD: u32 = 1;
const ELFCLASS64: u8 = 2;
const EM_X86_64: u16 = 62;

/// ELF64 file header.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

/// ELF64 program header.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

/// Loaded ELF info.
pub struct ElfInfo {
    /// Entry point virtual address.
    pub entry: u64,
    /// Virtual address of program headers (first PT_LOAD vaddr + e_phoff).
    pub phdr_vaddr: u64,
    /// Size of each program header entry.
    pub phentsize: u16,
    /// Number of program header entries.
    pub phnum: u16,
}

/// Load an ELF binary from a byte slice into identity-mapped memory.
///
/// Copies PT_LOAD segments to their specified virtual addresses and
/// zeroes any BSS regions (memsz > filesz). Returns the entry point.
///
/// # Safety
///
/// The caller must ensure:
/// - The target virtual addresses are identity-mapped and writable.
/// - The addresses don't overlap with kernel code/data.
pub unsafe fn load(elf_data: &[u8]) -> Result<ElfInfo, &'static str> {
    if elf_data.len() < core::mem::size_of::<Elf64Header>() {
        return Err("ELF data too small");
    }

    // SAFETY: elf_data is large enough for the header and properly aligned
    // by the linker (embedded in the kernel image).
    let header = unsafe { &*(elf_data.as_ptr() as *const Elf64Header) };

    // Validate magic
    if header.e_ident[..4] != ELF_MAGIC {
        return Err("bad ELF magic");
    }
    if header.e_ident[4] != ELFCLASS64 {
        return Err("not ELF64");
    }
    if header.e_type != ET_EXEC {
        return Err("not ET_EXEC (static executable)");
    }
    if header.e_machine != EM_X86_64 {
        return Err("not x86_64");
    }

    serial_println!("[elf] entry={:#x} phnum={}", header.e_entry, header.e_phnum);

    // Process program headers
    let ph_offset = header.e_phoff as usize;
    let ph_count = header.e_phnum as usize;
    let mut phdr_vaddr: u64 = 0;

    for i in 0..ph_count {
        let offset = ph_offset + i * core::mem::size_of::<Elf64Phdr>();
        if offset + core::mem::size_of::<Elf64Phdr>() > elf_data.len() {
            return Err("program header out of bounds");
        }

        // SAFETY: Offset is within bounds, Elf64Phdr is repr(C).
        let phdr = unsafe { &*(elf_data.as_ptr().add(offset) as *const Elf64Phdr) };

        if phdr.p_type != PT_LOAD {
            continue;
        }

        let src_offset = phdr.p_offset as usize;
        let dst_addr = phdr.p_vaddr as usize;
        let filesz = phdr.p_filesz as usize;
        let memsz = phdr.p_memsz as usize;

        serial_println!(
            "[elf] LOAD: vaddr={:#x} filesz={:#x} memsz={:#x}",
            dst_addr, filesz, memsz
        );

        if src_offset + filesz > elf_data.len() {
            return Err("segment data out of bounds");
        }

        // SAFETY: dst_addr is identity-mapped in our 4 GiB flat address space.
        // The address range doesn't overlap with the kernel (kernel is at 0x200000,
        // user binary is at 0x400000+).
        unsafe {
            // Copy file-backed portion
            core::ptr::copy_nonoverlapping(
                elf_data.as_ptr().add(src_offset),
                dst_addr as *mut u8,
                filesz,
            );
            // Zero BSS (memsz - filesz)
            if memsz > filesz {
                core::ptr::write_bytes((dst_addr + filesz) as *mut u8, 0, memsz - filesz);
            }
        }

        // Check if this PT_LOAD segment contains the program headers
        // (e_phoff falls within [p_offset, p_offset + p_filesz))
        if phdr_vaddr == 0
            && (header.e_phoff >= phdr.p_offset)
            && (header.e_phoff < phdr.p_offset + phdr.p_filesz)
        {
            phdr_vaddr = phdr.p_vaddr + (header.e_phoff - phdr.p_offset);
        }
    }

    serial_println!("[elf] phdr_vaddr={:#x} phentsize={} phnum={}",
        phdr_vaddr, header.e_phentsize, header.e_phnum);

    Ok(ElfInfo {
        entry: header.e_entry,
        phdr_vaddr,
        phentsize: header.e_phentsize,
        phnum: header.e_phnum,
    })
}
