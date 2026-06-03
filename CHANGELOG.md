
# Changelog

## 0.1.4
- Enforces command echoback checking in `read_page_into_ram`. This may introduce compatibility problem,
  but this is important for ensuring data correctness.

## 0.1.3
- (!!!) Ensures the base address of target RAM operations is 0x0. Previous versions of this tool may
  produce incorrect result if this opearation base address is changed before using this tool.
- `Dumper::dump_memory` now checks for the start address in `md.l` output to make sure it is not
  receiving previously buffered data from the serial port.
- Adds the option of having an extra region beyond the minimum size of the given target RAM region,
  it is to be filled by 0xFF, so that empty pages can be found by `cmp`ing with that region, speeding
  up dumping process. This doubles the size of the used target RAM region.
- Adds the option of using `nand read` commands without `.raw` suffix, passing through the ECC. 
  Note: only `nand read.raw` is used previously because it is consistent with `nand dump`.
- Supports quick verifying of a given image; CRC32 values of current page data read from U-Boot can be
  checked against that image, and only the differences need to be dumped for creating a new image.
- Exposes `DumpMode::MainOnly` option in the command-line interface.

## 0.1.2
* Adds empty page check feature for `Dumpbuf`.
* Adds bad block check feature for `Dumpbuf`.
* Adds corresponding `check` subcommand.

## 0.1.1
* Fixes read buffer clearing.
* Sends CTRL+C before sending U-boot shell commands.
* Adds progress bar for `check-comm` subcommand.
* Adds `Dumper::into_inner` and `Dumper::config`.

## 0.1.0
* Initial release.
