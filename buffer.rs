use std::fs::File;
use std::io::Write;
use std::ops::Range;
use std::path::Path;

use crate::config::DumpMode;
use crate::{Error, NandConfig};

/// Stores the data of a page.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Page {
    page_size: usize,
    oob_size: usize,
    data: Option<Vec<u8>>,
    oob: Option<Vec<u8>>,
}

impl Page {
    pub(crate) fn new(nand_conf: &NandConfig) -> Self {
        Page {
            page_size: nand_conf.page_size,
            oob_size: nand_conf.page_oob_size,
            data: None,
            oob: None,
        }
    }

    pub(crate) fn init_data_buf(&mut self) -> &mut [u8] {
        self.data.replace(vec![0u8; self.page_size]);
        self.data_mut().unwrap()
    }

    pub(crate) fn init_oob_buf(&mut self) -> &mut [u8] {
        self.oob.replace(vec![0u8; self.oob_size]);
        self.oob_mut().unwrap()
    }

    /// The main data size.
    pub fn size(&self) -> usize {
        self.page_size
    }

    /// The page OOB area size.
    pub fn oob_size(&self) -> usize {
        self.oob_size
    }

    /// Gets the main data, if available.
    pub fn data(&self) -> Option<&[u8]> {
        self.data.as_ref().map(|v| &v[..])
    }

    /// Gets the OOB data, if available.
    pub fn oob(&self) -> Option<&[u8]> {
        self.oob.as_ref().map(|v| &v[..])
    }

    /// Modifies the main data buffer, if available.
    pub fn data_mut(&mut self) -> Option<&mut [u8]> {
        self.data.as_mut().map(|v| &mut v[..])
    }

    /// Modifies the OOB data buffer, if available.
    pub fn oob_mut(&mut self) -> Option<&mut [u8]> {
        self.oob.as_mut().map(|v| &mut v[..])
    }
}

/// Stores the dumped NAND data.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DumpBuf {
    nand_conf: NandConfig,
    dump_mode: DumpMode,
    range: Range<usize>,
    pages: Vec<Page>,
}

impl DumpBuf {
    /// Creates an empty [DumpBuf] with a given start offset.
    pub fn build(
        nand_conf: &NandConfig,
        dump_mode: DumpMode,
        start_offset: usize,
    ) -> Result<DumpBuf, Error> {
        nand_conf.check()?;
        if start_offset > nand_conf.flash_size {
            return Err(Error::OutOfRange);
        }
        if !start_offset.is_multiple_of(nand_conf.page_size) {
            return Err(Error::InvalidRange(start_offset..start_offset));
        }
        Ok(DumpBuf {
            nand_conf: nand_conf.clone(),
            dump_mode,
            range: start_offset..start_offset,
            pages: Vec::new(),
        })
    }

    /// Increases the dump range by one page, which is appended as the last page.
    pub fn push_page(&mut self, page: Page) -> Result<(), Error> {
        if self.nand_conf.page_size != page.size() || self.nand_conf.page_oob_size != page.oob_size
        {
            return Err(Error::InvalidPage("Page size mismatch"));
        }
        if self.dump_mode().has_main() != page.data().is_some()
            || self.dump_mode().has_oob() != page.oob().is_some()
        {
            return Err(Error::InvalidPage("Page dump mode mismatch"));
        }
        let new_range_end = self.range.end + self.nand_conf.page_size;
        if new_range_end > self.nand_conf.flash_size {
            return Err(Error::OutOfRange);
        }
        self.pages.push(page);
        self.range.end = new_range_end;
        Ok(())
    }

    /// Appends data into this buffer according to the dump mode; if dump mode is `Both`,
    /// `data` will be treated as data+OOB interleaved data for each page.
    /// Length of `raw` data must be a multiple of [Self::page_dump_size].
    pub fn append(&mut self, raw: &[u8]) -> Result<(), Error> {
        if !raw.len().is_multiple_of(self.page_dump_size()) {
            return Err(Error::InvalidRange(0..raw.len()));
        }
        for mut raw_page in raw.chunks(self.page_dump_size()) {
            let mut page = Page::new(&self.nand_conf);
            if self.dump_mode().has_main() {
                page.init_data_buf()
                    .copy_from_slice(&raw_page[..self.nand_conf.page_size]);
                raw_page = &raw_page[self.nand_conf.page_size..];
            }
            if self.dump_mode().has_oob() {
                page.init_oob_buf().copy_from_slice(raw_page);
            }
            self.push_page(page)?;
        }
        Ok(())
    }

    /// Turns the dump mode of this buffer into `Both` with the given `raw` data
    /// of page dumps with only **main data**, if the previous mode is `OobOnly`.
    /// Length of `raw` must match the existing OOB data exactly.
    pub fn merge_data(&mut self, raw: &[u8]) -> Result<(), Error> {
        if self.dump_mode() != DumpMode::OobOnly {
            return Err(Error::InvalidPage("dump mode mismatch"));
        }
        if raw.len() != self.range().len() {
            return Err(Error::InvalidRange(0..raw.len()));
        }
        for (i, raw_data) in raw.chunks(self.nand_conf.page_size).enumerate() {
            self.pages[i].init_data_buf().copy_from_slice(raw_data);
        }
        Ok(())
    }

    /// Turns the dump mode of this buffer into `Both` with the given `raw` data
    /// of page dumps with only **OOB data**, if the previous mode is `MainOnly`.
    /// Length of `raw` must match the existing main data exactly.
    pub fn merge_oobs(&mut self, raw: &[u8]) -> Result<(), Error> {
        if self.dump_mode() != DumpMode::MainOnly {
            return Err(Error::InvalidPage("dump mode mismatch"));
        }
        if raw.len() != self.pages().len() * self.nand_conf.page_oob_size {
            return Err(Error::InvalidRange(0..raw.len()));
        }
        for (i, raw_oob) in raw.chunks(self.nand_conf.page_oob_size).enumerate() {
            self.pages[i].init_oob_buf().copy_from_slice(raw_oob);
        }
        Ok(())
    }

    /// Returns the fixed dump mode.
    pub fn dump_mode(&self) -> DumpMode {
        self.dump_mode
    }

    /// The nand config used while dumping.
    pub fn nand_config(&self) -> &NandConfig {
        &self.nand_conf
    }

    /// Indicates the dump range within the NAND size (always main data range, excluding OOB).
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Returns the dumped data size for each page under the current **dump mode**.
    pub fn page_dump_size(&self) -> usize {
        let mut size = 0;
        if self.dump_mode().has_main() {
            size += self.nand_conf.page_size;
        }
        if self.dump_mode().has_oob() {
            size += self.nand_conf.page_oob_size;
        }
        size
    }

    /// Returns the size of all data currently held by this buffer.
    pub fn data_size(&self) -> usize {
        self.page_dump_size() * self.pages().len()
    }

    /// Gets available pages.
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// Modifies available pages.
    pub fn pages_mut(&mut self) -> &mut [Page] {
        &mut self.pages
    }

    /// Saves the dumped data, interleaving main data and OOB data for each page if both are dumped.
    pub fn save(&self, dump_path: &Path) -> Result<(), Error> {
        let mut file = File::create(dump_path)?;
        for page in &self.pages {
            if let Some(data) = page.data() {
                file.write_all(data)?;
            }
            if let Some(oob) = page.oob() {
                file.write_all(oob)?;
            }
        }
        file.sync_all()?;
        Ok(())
    }

    /// Saves the dumped main data without OOB.
    pub fn save_data(&self, dump_path: &Path) -> Result<(), Error> {
        if !self.dump_mode().has_main() {
            return Err(Error::InvalidPage("main data not dumped"));
        }
        let mut file = File::create(dump_path)?;
        for page in &self.pages {
            let data = page.data().unwrap();
            file.write_all(data)?;
        }
        file.sync_all()?;
        Ok(())
    }

    /// Saves the dumped OOB data.
    pub fn save_oobs(&self, dump_path: &Path) -> Result<(), Error> {
        if !self.dump_mode().has_oob() {
            return Err(Error::InvalidPage("OOB not dumped"));
        }
        let mut file = File::create(dump_path)?;
        for page in &self.pages {
            let oob = page.oob().unwrap();
            file.write_all(oob)?;
        }
        file.sync_all()?;
        Ok(())
    }
}
