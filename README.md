# uboot-nand-dump

Dumps NAND flash image via U-Boot serial interface.

This is definitely not a hack tool, because it merely uses the U-Boot UART interface
already exposed by the vendor.

Use `default-features = false` when using this as a dependency.

## Reading

This reads the NAND flash data in a slow but generic manner (command `nand dump`
or `nand read.raw` plus `md.l`), trying to maintain compatibility with different U-Boot versions,
making NAND dump within U-Boot builds without `usb` and `mmc` (SD) subsystems possible.

However, the speed of this method is the slowest: about 1.7 KB/s (`nand dump` without CRC check),
or around 2.3KB/s (`nand read.raw` and `md` with CRC check), under baud rate 115,200.

The dump result can be checked by `crc32` command if a start offset of a target RAM space is
given to this utility. The size of this space must be enough for one NAND page with OOB.
Note that this is more reliable; it is also faster than reading with `nand dump`.

## File convertion

Convertion between the data+OOB interleaved dump file (for Linux `nanddump` or some NAND chip
programmers) and the seperated data dump file plus OOB dump file (for U-Boot operation)
is supported here.

## TODO

One who implements any of these features may become the owner of this crate:

- Support erasing, programming and verifying.
- Support using `saves` command (if available) to perform faster reads.
