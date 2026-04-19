# Bootstrapping from 32-bit with the Multiboot specification.
# See https://www.gnu.org/software/grub/manual/multiboot/multiboot.html

.section .text.entry
.code32
.global _start
_start:
    mov     ecx, {mb_magic}
    cmp     ecx, eax
    jnz     1f
    mov     edi, ebx        # arg1: multiboot info
    jmp     entry32
1:  hlt
    jmp     1b

.balign 4
.type multiboot_header, STT_OBJECT
multiboot_header:
    .int    {mb_hdr_magic}                      # magic: 0x1BADB002
    .int    {mb_hdr_flags}                      # flags
    .int    -({mb_hdr_magic} + {mb_hdr_flags})  # checksum
    .int    multiboot_header - {offset}         # header_addr
    .int    start - {offset}                    # load_addr
    .int    edata - {offset}                    # load_end
    .int    ebss - {offset}                     # bss_end_addr
    .int    _start - {offset}                   # entry_addr

.code32
entry32:
    lgdt    [.Ltmp_gdt_desc - {offset}]

    # set data segment selectors
    mov     ax, 0x18
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax

    # set PAE, PGE bit in CR4
    mov     eax, {cr4}
    mov     cr4, eax

    # load the temporary page table
    lea     eax, [.Ltmp_pml4 - {offset}]
    mov     cr3, eax

    # set LME, NXE bit in IA32_EFER
    mov     ecx, {efer_msr}
    mov     edx, 0
    mov     eax, {efer}
    wrmsr

    # set protected mode, write protect, paging bit in CR0
    mov     eax, {cr0}
    mov     cr0, eax

    ljmp    0x10, offset entry64 - {offset}

.code64
entry64:
    # clear segment selectors
    xor     ax, ax
    mov     ss, ax
    mov     ds, ax
    mov     es, ax
    mov     fs, ax
    mov     gs, ax

    # set RSP to boot stack top
    movabs  rsp, offset boot_stack_top

    # call main(mbi)
    movabs  rax, offset {entry}
    call    rax
    jmp     .Lhlt

.Lhlt:
    hlt
    jmp     .Lhlt

.section .rodata
.balign 8
.Ltmp_gdt_desc:
    .short  .Ltmp_gdt_end - .Ltmp_gdt - 1
    .long   .Ltmp_gdt - {offset}

.section .data
.balign 16
.Ltmp_gdt:
    .quad 0x0000000000000000    # 0x00: null
    .quad 0x00cf9b000000ffff    # 0x08: code32
    .quad 0x00af9b000000ffff    # 0x10: code64
    .quad 0x00cf93000000ffff    # 0x18: data
.Ltmp_gdt_end:

.balign 4096
.Ltmp_pml4:
    .quad .Ltmp_pdpt_low - {offset} + 0x3   # PRESENT | WRITABLE
    .zero 8 * 511

.Ltmp_pdpt_low:
    .quad 0x0000 | 0x83         # 0-1GiB identity
    .quad 0x40000000 | 0x83     # 1-2GiB identity
    .quad 0x80000000 | 0x83     # 2-3GiB identity
    .quad 0xc0000000 | 0x83     # 3-4GiB identity
    .zero 8 * 508

.section .bss.stack
.balign 4096
.boot_stack:
    .space {boot_stack_size}
boot_stack_top:
