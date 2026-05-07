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

    /// Verifies the target NAND region against the given `dump_buf` quickly by checking CRC32 values of
    /// current page data read from U-Boot and corresponding values calculated from page data in `dump_buf`.
    /// Returns start offsets (within NAND main address space) of pages where the target NAND data
    /// differ from `dump_buf`.
    pub fn quick_verify(&mut self, dump_buf: &DumpBuf) -> Result<Vec<usize>, Error> {
        let Some(&ram_offset) = self.config().page_buf_ram_offset.as_ref() else {
            return Err(Error::InvalidConfig(
                "quick verify is impossible because `page_buf_ram_offset` isn't provided",
            ));
        };

        let mut pages_found = Vec::new();
        for (page, page_offset) in dump_buf
            .pages()
            .iter()
            .zip(dump_buf.range().step_by(self.nand_conf().page_size))
        {
            let i_page_abs = page_offset / self.nand_conf().page_size;
            self.read_page_into_ram(i_page_abs, ram_offset, dump_buf.dump_mode())?;
            if dump_buf.dump_mode().has_main() {
                let crc32 = Self::crc32(page.data().unwrap());
                let uboot_crc32 = self.uboot_crc32(ram_offset, self.nand_conf().page_size)?;
                if uboot_crc32 != crc32 {
                    pages_found.push(page_offset);
                    continue;
                }
            }
            if dump_buf.dump_mode().has_oob() {
                let ram_oob_offset = ram_offset + self.nand_conf().page_size as u64;
                let crc32 = Self::crc32(page.oob().unwrap());
                let uboot_crc32 =
                    self.uboot_crc32(ram_oob_offset, self.nand_conf().page_oob_size)?;
                if uboot_crc32 != crc32 {
                    pages_found.push(page_offset);
                }
            }
        }
        Ok(pages_found)
    }

    /// Does [Self::quick_verify], reads the "dirty" pages found and replaces the corresponding
    /// data in `dump_buf`.
    pub fn quick_sync(&mut self, dump_buf: &mut DumpBuf) -> Result<Vec<usize>, Error> {
        let pages_found = self.quick_verify(dump_buf)?;
        for page_offset in &pages_found {
            let i_page_abs = page_offset / self.nand_conf().page_size;
            let i_page_rel = (page_offset - dump_buf.range().start) / self.nand_conf().page_size;
            let page = self.read_page(i_page_abs, dump_buf.dump_mode())?;
            dump_buf.pages_mut()[i_page_rel] = page;
        }
        Ok(pages_found)
    }

    // NOTE: it is possible to use `nand dump` (with `read.raw` for `crc32`) here, because
    // the output of `md` uses a longer output format that might lead to slower read speed.
    // While testing with that method, somehow it is slower than the `read.raw` method.
    // There is another problem with that method: bit flips occuring in the page would be
    // treated like `UnstableConnection`; this is because 2 reads are performed.
    /// If the page buffer RAM region is available, does `nand read` and `crc32`;
    /// else, does `Self::dump_page_without_crc_check`.
    ///
    /// `i_page_abs` is the index of the page (starting from 0) within the NAND size.
    fn read_page(&mut self, i_page_abs: usize, mode: DumpMode) -> Result<Page, Error> {
        let Some(&ram_offset) = self.config().page_buf_ram_offset.as_ref() else {
            let page = self.read_page_without_crc_check(i_page_abs, mode)?;
            return Ok(page);
        };

        self.read_page_into_ram(i_page_abs, ram_offset, mode)?;
        let ram_oob_offset = ram_offset + self.nand_conf().page_size as u64;

        let mut page = Page::new(self.nand_conf());
        if mode.has_main() {
            let page_data = page.init_data_buf();
            if self.config().fast_empty_check()
                && self.empty_data_check(ram_offset, page_data.len())?
            {
                page_data.fill(0xFF);
            } else {
                self.dump_memory_no_pre_intr(ram_offset, page_data)?;
            }
            if Self::crc32(page_data) != self.uboot_crc32(ram_offset, page_data.len())? {
                return Err(Error::UnstableConnection);
            }
        }
        if mode.has_oob() {
            let page_oob = page.init_oob_buf();
            if self.config().fast_empty_check()
                && self.empty_data_check(ram_oob_offset, page_oob.len())?
            {
                page_oob.fill(0xFF);
            } else {
                self.dump_memory_no_pre_intr(ram_oob_offset, page_oob)?;
            }
            if Self::crc32(page_oob) != self.uboot_crc32(ram_oob_offset, page_oob.len())? {
                return Err(Error::UnstableConnection);
            }
        }
        Ok(page)
    }

    /// Reads a page into the target RAM at `ram_offset`; however, the OOB data (if read)
    /// will start at offset of `ram_offset` + page size even if the main data isn't read.
    /// `i_page_abs` is the index of the page (starting from 0) within the NAND size.
    fn read_page_into_ram(
        &mut self,
        i_page_abs: usize,
        ram_offset: u64,
        mode: DumpMode,
    ) -> Result<(), Error> {
        let nand_offset = i_page_abs * self.nand_conf().page_size;

        let mut read_raw_needed = true;
        self.clear_read_buffer()?;
        if self.config().enable_uboot_ecc() {
            read_raw_needed = false;
            if mode.has_main() {
                self.send_cmd(&format!(
                    "nand read {:#x} {:#x} {:#x}\n",
                    ram_offset,
                    nand_offset,
                    self.nand_conf().page_size
                ))?;
                // "Skipping bad block" string can be found in:
                // <https://elixir.u-boot.org/u-boot/v2011.03/source/drivers/mtd/nand/nand_util.c#L630>
                // <https://elixir.u-boot.org/u-boot/v2013.04/source/drivers/mtd/nand/nand_util.c#L729>
                // <https://elixir.u-boot.org/u-boot/v2026.04/source/drivers/mtd/nand/raw/nand_util.c#L758>
                if let Err(err) = self.read_until_header("OK", Some("Skipping bad block")) {
                    if let Error::Shell(msg) = &err
                        && msg.contains("Skipping")
                    {
                        read_raw_needed = true;
                    } else {
                        return Err(err);
                    }
                }
            }
            if !read_raw_needed && mode.has_oob() {
                let ram_oob_offset = ram_offset + self.nand_conf().page_size as u64;
                self.send_cmd(&format!(
                    "nand read.oob {:#x} {:#x} {:#x}\n",
                    ram_oob_offset,
                    nand_offset,
                    self.nand_conf().page_oob_size
                ))?;
                self.read_until_header("OK", None)?;
            }
        }

        if read_raw_needed {
            // NOTE: `nand read.raw` reads one page with OOB by default, since:
            // <https://patchwork.ozlabs.org/project/uboot/patch/1316785390-17006-1-git-send-email-marek.vasut@gmail.com>
            self.send_cmd(&format!(
                "nand read.raw {:#x} {:#x}\n",
                ram_offset, nand_offset
            ))?;
            self.read_until_header("OK", None)?;
        }

        Ok(())
    }

    /// Checks if a block of data in target RAM starting from `ram_offset` contains only `0xFF`s.
    /// `data_size` must not exceed the main data size of a page.
    fn empty_data_check(&mut self, ram_offset: u64, data_size: usize) -> Result<bool, Error> {
        if data_size == 0 {
            return Ok(true);
        } else if data_size > self.nand_conf().page_size {
            return Err(Error::InvalidConfig(
                "wrong `empty_data_check` usage: `data_size` exceeds page main data size",
            ));
        }
        let empty_buf_ram_offset = self.init_target_empty_buf()?;
        self.clear_read_buffer()?;
        self.send_cmd_no_pre_intr(&format!(
            "cmp.b {:#x} {:#x} {:#x}",
            empty_buf_ram_offset, ram_offset, data_size
        ))?;
        // <https://elixir.u-boot.org/u-boot/v2011.03/source/common/cmd_mem.c#L346>
        // <https://elixir.u-boot.org/u-boot/v2026.04/source/cmd/mem.c#L308>
        let ret = self.read_until_header("Total of", None)?;
        if ret.contains(&format!("Total of {data_size}")) && ret.contains("same") {
            Ok(true)
        } else {
            Ok(false)
        }
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
            let line = self.read_until_header("CRC32", None)?;
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
        i_page_abs: usize,
        mode: DumpMode,
    ) -> Result<Page, Error> {
        let mut page = Page::new(self.nand_conf());

        let nand_offset = i_page_abs * self.nand_conf().page_size;

        self.clear_read_buffer()?;
        if mode != DumpMode::OobOnly {
            // NOTE: this reads the data in raw mode.
            self.send_cmd(&format!("nand dump {:#x}\n", nand_offset))?;
        } else {
            self.send_cmd(&format!("nand dump.oob {:#x}\n", nand_offset))?;
        }
        self.read_until_header(&format!("{nand_offset:x}"), None)?;

        if mode.has_main() {
            self.read_until_header(Self::DUMP_DATA_HEAD, None)?;
            let page_data = page.init_data_buf();
            self.read_bytes_from_hex(page_data)?;
        }

        if mode.has_oob() {
            self.read_until_header(Self::DUMP_OOB_HEAD, None)?;
            let page_oob = page.init_oob_buf();
            self.read_bytes_from_hex(page_oob)?;
        }

        Ok(page)
    }

    fn nand_conf(&self) -> &NandConfig {
        &self.config().nand_conf
    }
}
