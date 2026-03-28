use std::ops::Range;

use serialport::SerialPort;

use crate::{DumpBuf, Dumper, Error, NandConfig, Page, config::DumpMode};

impl<S: SerialPort> Dumper<S> {
    const DUMP_DATA_HEAD: &str = "dump:";
    const DUMP_OOB_HEAD: &str = "OOB:";

    /// Reads the data in the given range, which must be page-aligned.
    pub fn read(&mut self, range: Range<usize>, mode: DumpMode) -> Result<DumpBuf, Error> {
        self.nand_conf().check()?;
        if range.is_empty()
            || !range.start.is_multiple_of(self.nand_conf().page_size)
            || !range.end.is_multiple_of(self.nand_conf().page_size)
        {
            return Err(Error::InvalidRange(range));
        }

        let cnt_pages = range.len() / self.nand_conf().page_size;
        let mut dump_buf = self.init_read(range.start, mode)?;
        for _ in 0..cnt_pages {
            self.read_next_page(&mut dump_buf)?;
        }

        Ok(dump_buf)
    }

    /// Creates an empty [DumpBuf] to be used with [Self::read_next_page].
    pub fn init_read(&mut self, start_offset: usize, mode: DumpMode) -> Result<DumpBuf, Error> {
        let info = self.select_nand(self.config().nand_index())?;
        if let Some(expected) = self.config().expected_nand_info.clone()
            && !info.contains(&expected)
        {
            return Err(Error::UnexpectedNandInfo(info));
        }
        DumpBuf::build(self.nand_conf(), mode, start_offset)
    }

    /// Reads the next page and extend the range of `dump_buf` by one page.
    pub fn read_next_page(&mut self, dump_buf: &mut DumpBuf) -> Result<(), Error> {
        let nand_offset = dump_buf.range().end;
        let new_range_end = nand_offset + self.nand_conf().page_size;
        if new_range_end > self.nand_conf().flash_size {
            return Err(Error::OutOfRange);
        }
        let mut last_err = None;
        for _ in 0..5 {
            match self.read_page(
                nand_offset / self.nand_conf().page_size,
                dump_buf.dump_mode(),
            ) {
                Ok(page) => {
                    dump_buf.push_page(page)?;
                    return Ok(());
                }
                Err(e) => {
                    let _ = last_err.replace(e);
                    self.reach_end_of_receiving()?;
                }
            }
        }
        Err(last_err.unwrap())
    }

    // NOTE: it is possible to use `nand dump` (with `read.raw` for `crc32`) here, because
    // the output of `md` uses a longer output format that might lead to slower read speed.
    // While testing with that method, somehow it is slower than the `read.raw` method.
    // There is another problem with that method: bit flips occuring in the page would be
    // treated like `UnstableConnection`; this is because 2 reads are performed.
    /// If the page buffer RAM region is available, does `nand read` and `crc32`;
    /// else, does `Self::dump_page_without_crc_check`.
    fn read_page(&mut self, i_page: usize, mode: DumpMode) -> Result<Page, Error> {
        let Some(&ram_offset) = self.config().page_buf_ram_offset.as_ref() else {
            let page = self.read_page_without_crc_check(i_page, mode)?;
            return Ok(page);
        };

        // NOTE: `nand read.raw` reads a page (with OOB) by default, since:
        // <https://patchwork.ozlabs.org/project/uboot/patch/1316785390-17006-1-git-send-email-marek.vasut@gmail.com>
        let nand_offset = i_page * self.nand_conf().page_size;
        self.clear_read_buffer()?;
        self.send_cmd(&format!(
            "nand read.raw {:#x} {:#x}\n",
            ram_offset, nand_offset
        ))?;
        self.read_until_header("OK")?;

        let mut page = Page::new(self.nand_conf());
        if mode.has_main() {
            let page_data = page.init_data_buf();
            self.dump_memory_no_pre_intr(ram_offset, page_data)?;
        }
        if mode.has_oob() {
            let page_oob = page.init_oob_buf();
            self.dump_memory_no_pre_intr(ram_offset + self.nand_conf().page_size as u64, page_oob)?;
        }

        if mode.has_main() {
            let crc32 = Self::crc32(page.data().unwrap());
            let uboot_crc32 = self.uboot_crc32(ram_offset, self.nand_conf().page_size)?;
            if uboot_crc32 != crc32 {
                return Err(Error::UnstableConnection);
            }
        }

        if mode.has_oob() {
            let crc32 = Self::crc32(page.oob().unwrap());
            let uboot_crc32 = self.uboot_crc32(
                ram_offset + self.nand_conf().page_size as u64,
                self.nand_conf().page_oob_size,
            )?;
            if uboot_crc32 != crc32 {
                return Err(Error::UnstableConnection);
            }
        }

        Ok(page)
    }

    /// Uses the CRC32 algorithm used by U-boot.
    fn crc32(data: &[u8]) -> u32 {
        // <https://elixir.u-boot.org/u-boot/v2013.04/source/lib/crc32.c>
        // <https://elixir.u-boot.org/u-boot/v2026.04-rc5/source/lib/crc32.c>
        const ALG: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
        ALG.checksum(data)
    }

    /// Uses the U-boot `crc32` command.
    fn uboot_crc32(&mut self, address: u64, count: usize) -> Result<u32, Error> {
        // NOTE: returning format of `crc32`: `CRC32 for %08lx ... %08lx ==> %08lx\n`.
        self.send_cmd_no_pre_intr(&format!("crc32 {:#x} {:#x}\n", address, count))?;
        loop {
            let line = self.read_until_header("CRC32")?;
            if line.find(&format!("{address:x}")).is_none()
                || line
                    .find(&format!("{:x}", address + count as u64 - 1))
                    .is_none()
            {
                continue;
            }
            let crc_hex = line
                .split_whitespace()
                .last()
                .ok_or_else(|| Error::Shell(format!("invalid crc32 command response: {line}")))?;
            let val = u32::from_str_radix(crc_hex, 16)
                .map_err(|_| Error::Shell(format!("invalid crc32 command response: {line}")))?;
            return Ok(val);
        }
    }

    /// Dumps exactly one page with `nand dump`, without retry on any failure.
    fn read_page_without_crc_check(
        &mut self,
        i_page: usize,
        mode: DumpMode,
    ) -> Result<Page, Error> {
        let mut page = Page::new(self.nand_conf());

        let nand_offset = i_page * self.nand_conf().page_size;

        if mode != DumpMode::OobOnly {
            // NOTE: this reads the data in raw mode.
            self.send_cmd(&format!("nand dump {:#x}\n", nand_offset))?;
        } else {
            self.send_cmd(&format!("nand dump.oob {:#x}\n", nand_offset))?;
        }
        self.read_until_header(&format!("{nand_offset:x}"))?;

        if mode.has_main() {
            self.read_until_header(Self::DUMP_DATA_HEAD)?;
            let page_data = page.init_data_buf();
            self.read_bytes_from_hex(page_data)?;
        }

        if mode.has_oob() {
            self.read_until_header(Self::DUMP_OOB_HEAD)?;
            let page_oob = page.init_oob_buf();
            self.read_bytes_from_hex(page_oob)?;
        }

        Ok(page)
    }

    fn nand_conf(&self) -> &NandConfig {
        &self.config().nand_conf
    }
}
