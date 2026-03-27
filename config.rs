use crate::Error;

const CONF_FILE_IDENTIFIER: &str = "uboot-nand-dump";
const CONF_VERSION: u32 = 1;

/// Specific config for NAND parameters and U-boot operation settings.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// It must be `uboot-nand-dump`.
    pub conf_file_ident: String,
    /// It must be `1` for the current version.
    pub conf_version: u32,
    /// Defaults to 115,200 if not given.
    pub baud_rate: Option<u32>,
    /// Known NAND chip parameters needed here.
    pub nand_conf: NandConfig,
    /// Index of the target NAND selected using `nand device` command, defaults to `0`.
    pub nand_index: Option<u32>,
    /// A string that should be found in the response of the `nand device` command.
    pub expected_nand_info: Option<String>,
    /// Start offset of a target RAM space given to this utility.
    /// The space must be enough for 1 NAND page with OOB.
    pub page_buf_ram_offset: Option<u64>,
}

impl Config {
    /// Returns an error if the config is significantly malformed.
    pub fn check(&self) -> Result<(), Error> {
        if self.conf_file_ident.as_str() != CONF_FILE_IDENTIFIER {
            return Err(Error::InvalidConfig("not a config for this utility"));
        }
        if self.conf_version != CONF_VERSION {
            return Err(Error::InvalidConfig("config format version mismatch"));
        }
        self.nand_conf.check()?;
        if self.baud_rate() < 110 || self.baud_rate() > 2_000_000 {
            return Err(Error::InvalidConfig("unusual baud rate"));
        }
        Ok(())
    }

    /// Defaults to 115,200 if it is not set.
    pub fn baud_rate(&self) -> u32 {
        self.baud_rate.unwrap_or(115_200)
    }

    /// Defaults to 0 if it is not set.
    pub fn nand_index(&self) -> u32 {
        self.nand_index.unwrap_or(0)
    }
}

impl Default for Config {
    /// Defaults to page size 2048, 64 pages each block, 128MiB flash size.
    /// The default flash size is likely wrong for your target.
    fn default() -> Self {
        Self {
            conf_file_ident: CONF_FILE_IDENTIFIER.to_string(),
            conf_version: 1,
            baud_rate: None,
            nand_conf: NandConfig::default(),
            nand_index: None,
            expected_nand_info: None,
            page_buf_ram_offset: None,
        }
    }
}

/// Specifies critical parameters of the NAND flash.
///
/// These parameters are not parsed from U-Boot output because of U-Boot version issues.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NandConfig {
    pub page_size: usize,
    pub page_oob_size: usize,
    pub erase_size: usize,
    pub flash_size: usize,
}

impl Default for NandConfig {
    /// Defaults to page size 2048, 64 pages each block, 128MiB flash size.
    /// The default flash size is likely wrong for your target.
    fn default() -> Self {
        Self {
            page_size: 2048,
            page_oob_size: 64,
            erase_size: 64 * 2048,
            flash_size: 128 * 1024 * 1024,
        }
    }
}

impl NandConfig {
    /// Returns an error if the config is significantly malformed.
    pub fn check(&self) -> Result<(), Error> {
        if self.page_size < 512 {
            return Err(Error::InvalidConfig("invalid page size"));
        }
        if self.page_oob_size < 16 {
            return Err(Error::InvalidConfig("invalid page OOB size"));
        }
        if self.erase_size == 0 || !self.erase_size.is_multiple_of(self.page_size) {
            return Err(Error::InvalidConfig("invalid erase size"));
        }
        if self.flash_size == 0 || !self.flash_size.is_multiple_of(self.erase_size) {
            return Err(Error::InvalidConfig("invalid flash size"));
        }
        Ok(())
    }
}

/// Specifies whether or not to dump the main/OOB data, or to dump both regions of the page.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DumpMode {
    MainOnly,
    OobOnly,
    Both,
}

impl DumpMode {
    pub fn has_main(&self) -> bool {
        self == &DumpMode::Both || self == &DumpMode::MainOnly
    }

    pub fn has_oob(&self) -> bool {
        self == &DumpMode::Both || self == &DumpMode::OobOnly
    }
}
