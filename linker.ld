OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

/* Kernel at 240 MiB — above ALL ERTS memory (segments, fixed mmaps, stack).
   ERTS loads at 0x400000..0xE56000 with fixed mmaps up to ~0x1000000.
   The mmap bump allocator starts at 128 MiB. User stack at 224 MiB.
   Kernel must be above all of these. */
BASE_ADDRESS = 0xF000000;

SECTIONS
{
    . = BASE_ADDRESS;
    start = .;

    .text : {
        stext = .;
        *(.text.entry)
        *(.text .text.*)
        . = ALIGN(4K);
        etext = .;
    }

    .rodata : {
        srodata = .;
        *(.rodata .rodata.*)
        . = ALIGN(4K);
        erodata = .;
    }

    .data : {
        sdata = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
        *(.got .got.*)
        edata = .;
    }

    .stack : {
        *(.bss.stack)
    }

    .bss : {
        sbss = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        ebss = .;
    }

    . = ALIGN(4K);
    PROVIDE(dma_region = .);

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note*)
        *(.comment*)
    }
}
