/* Raspberry Pi Pico 2 W: RP2350A with 4 MiB of QSPI flash at 0x10000000.
 *
 * Flash layout (keep in sync with src/main.rs CONFIG_OFFSET, firmware/tools/ and the
 * README):
 *
 *   0x10000000 - 0x103EFFFF  firmware image (FLASH below, 4032 KiB)
 *   0x103F0000 - 0x103FDFFF  reserved for future node data (56 KiB, unused)
 *   0x103FE000 - 0x103FFFFF  config region: two 4 KiB slots (Wi-Fi, hostname)
 *
 * The linker can't place anything past FLASH, and a UF2 only rewrites the sectors it
 * carries, so flashing a new firmware.uf2 leaves the config region alone.
 */
MEMORY {
    FLASH : ORIGIN = 0x10000000, LENGTH = 4032K
    CONFIG : ORIGIN = 0x103FE000, LENGTH = 8K
    /* SRAM0-SRAM7, striped. */
    RAM : ORIGIN = 0x20000000, LENGTH = 512K
    SRAM8 : ORIGIN = 0x20080000, LENGTH = 4K
    SRAM9 : ORIGIN = 0x20081000, LENGTH = 4K
}

SECTIONS {
    /* ### Boot ROM info
     *
     * Goes after .vector_table, to keep it in the first 4K of flash
     * where the Boot ROM (and picotool) can find it
     */
    .start_block : ALIGN(4)
    {
        __start_block_addr = .;
        KEEP(*(.start_block));
        KEEP(*(.boot_info));
    } > FLASH

} INSERT AFTER .vector_table;

/* move .text to start /after/ the boot info (8-aligned, as .text requires) */
_stext = ALIGN(ADDR(.start_block) + SIZEOF(.start_block), 8);

SECTIONS {
    /* ### Picotool 'Binary Info' Entries
     *
     * Picotool looks through this block (as we have pointers to it in our
     * header) to find interesting information.
     */
    .bi_entries : ALIGN(4)
    {
        /* We put this in the header */
        __bi_entries_start = .;
        /* Here are the entries */
        KEEP(*(.bi_entries));
        /* Keep this block a nice round size */
        . = ALIGN(4);
        /* We put this in the header */
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;

SECTIONS {
    /* ### Boot ROM extra info
     *
     * Goes after everything in our program, so it can contain a signature.
     */
    .end_block : ALIGN(4)
    {
        __end_block_addr = .;
        KEEP(*(.end_block));
    } > FLASH

} INSERT AFTER .uninit;

PROVIDE(start_to_end = __end_block_addr - __start_block_addr);
PROVIDE(end_to_start = __start_block_addr - __end_block_addr);

/* The config region must stay exactly where the tools write it. */
ASSERT(ORIGIN(CONFIG) == 0x103FE000, "config region moved");
ASSERT(ORIGIN(FLASH) + LENGTH(FLASH) <= ORIGIN(CONFIG), "firmware overlaps the config region");
