/* Kernel memory layout. The base address must match bootinfo::KERNEL_BASE. */

ENTRY(_start)

/* One segment per permission set, so eliboot can map each with exactly the
   access it needs (W^X). */
PHDRS
{
    text   PT_LOAD FLAGS(5); /* r-x */
    rodata PT_LOAD FLAGS(4); /* r-- */
    data   PT_LOAD FLAGS(6); /* rw- */
}

SECTIONS
{
    . = 0xFFFFFFFF80000000;
    __kernel_start = .;

    .text : ALIGN(4K) {
        *(.text .text.*)
    } :text

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
        *(.data.rel.ro .data.rel.ro.*)
    } :rodata

    /* Test cases registered by ktest! (empty outside test builds). */
    .kernel_tests : ALIGN(8) {
        __kernel_tests_start = .;
        KEEP(*(.kernel_tests))
        __kernel_tests_end = .;
    } :rodata

    .data : ALIGN(4K) {
        *(.data .data.*)
        *(.got .got.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    . = ALIGN(4K);
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
