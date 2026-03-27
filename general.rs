use std::{
    io::{self, BufRead, BufReader},
    thread,
    time::{Duration, Instant},
};

use serialport::SerialPort;

use crate::{Config, Error};

/// NAND flash image dumper holding the U-Boot serial interface.
pub struct Dumper<S: SerialPort> {
    pub(crate) serial: BufReader<S>,
    pub(crate) conf: Config,
    pub(crate) buf_line: String, // temp buffer
}

impl<S: SerialPort> Dumper<S> {
    /// Builds the NAND flash image dumper.
    pub fn build(mut serial: S, conf: Config) -> Result<Self, Error> {
        conf.check()?;
        serial.set_timeout(Duration::from_millis(500))?;
        serial.set_baud_rate(conf.baud_rate())?;
        Ok(Self {
            serial: BufReader::new(serial),
            conf,
            buf_line: String::new(),
        })
    }

    /// Sends spaces for stopping the auto booting procedure of U-boot.
    /// Also checks the existence of the name `U-Boot` in the response of `version` command,
    /// returning `Error::UbootNotFound` if not found.
    pub fn prep_for_power_on(&mut self, dur: Duration) -> Result<(), Error> {
        self.clear_read_buffer()?;
        let t_finish = Instant::now() + dur;
        while Instant::now() < t_finish {
            self.serial().write_all(b" ")?;
            self.serial().flush()?;
            thread::sleep(Duration::from_millis(100));
        }
        self.serial().write_all(b"\n")?;
        self.send_cmd_and_get_reply("version")
            .map_err(|_| Error::UbootNotFound)?
            .find("U-Boot")
            .ok_or(Error::UbootNotFound)?;
        Ok(())
    }

    /// Keep checking the loopback message against the string sent with the U-boot `echo` command.
    /// Returns the total length of the strings checked, or `Error::UnstableConnection` if any difference is found.
    pub fn check_comm(&mut self, dur: Duration) -> Result<usize, Error> {
        const STR_ALPHA_NUMERIC: &str =
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        const CHECK_LEN: usize = 255;
        let t_finish = Instant::now() + dur;
        let mut cnt_checked = 0;
        while Instant::now() < t_finish {
            self.serial().write_all(b"echo ")?;
            let mut string_check = String::new();
            for i in 0..CHECK_LEN {
                let ch = STR_ALPHA_NUMERIC.as_bytes()
                    [getrandom::u32().unwrap() as usize % STR_ALPHA_NUMERIC.len()];
                self.serial().write_all(&[ch])?;
                string_check.push(char::from_u32(ch as u32).unwrap());
                if i % 16 == 0 {
                    self.serial().flush()?;
                }
            }
            self.serial().write_all(b"\n")?;
            self.serial().flush()?;
            self.read_until_header(&string_check)
                .map_err(|_| Error::UnstableConnection)?;
            cnt_checked += CHECK_LEN;
        }
        Ok(cnt_checked)
    }

    /// Gets basic information from U-Boot to be checked manually.
    ///
    /// NOTE: currently, `Dumper` will not use any of these information for future uses.
    pub fn probe_uboot_info(&mut self) -> Result<Vec<(String, String)>, Error> {
        // XXX: more commands may be added
        const PROBE_CMDS: &[&str] = &[
            "version",
            "bdinfo",
            "coninfo",
            "iminfo",
            "mtdparts",
            "nand info",
            "mmc list",
            "mmcinfo",
            "printenv",
            "help",
        ];
        let mut infos = Vec::new();
        for cmd in PROBE_CMDS {
            infos.push((cmd.to_string(), self.send_cmd_and_get_reply(cmd)?));
        }
        Ok(infos)
    }

    /// Gets an info string using the `nand device` command.
    pub fn nand_info(&mut self) -> Result<String, Error> {
        self.select_nand(self.conf.nand_index())
    }

    /// Gets a bad block list string using the `nand bad` command.
    /// TODO: return a list of offset values instead of a `String`.
    pub fn nand_bad_info(&mut self) -> Result<String, Error> {
        self.select_nand(self.conf.nand_index())?;
        self.send_cmd_and_get_reply("nand bad")
    }

    /// Selects the active NAND device number and gets an info string.
    pub(crate) fn select_nand(&mut self, num: u32) -> Result<String, Error> {
        let resp = self
            .send_cmd_and_get_reply(&format!("nand device {num}"))?
            .to_lowercase();

        if resp.contains("no such device") || resp.contains("no device") {
            return Err(Error::InvalidConfig("Bad NAND selection index for U-Boot"));
        };
        self.send_cmd_and_get_reply("nand device")
    }

    /// (Dangerous) Dumps a memory range using the U-boot `md` command.
    /// This might be dangerous because a bad address range may trigger some bug
    /// in the target U-boot version.
    pub fn dump_memory(&mut self, offset: u64, out_buf: &mut [u8]) -> Result<(), Error> {
        if out_buf.is_empty() {
            return Ok(());
        }
        self.send_cmd(&format!(
            "md.l {:#x} {:#x}",
            offset,
            out_buf.len().div_ceil(4)
        ))?;
        self.buf_line.clear();
        let mut cnt_read = 0;
        while cnt_read < out_buf.len() {
            let sz = self.serial.read_line(&mut self.buf_line)?;
            if sz == 0 {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "check serial",
                )));
            }
            for (i, hex) in self.buf_line.split_whitespace().skip(1).enumerate() {
                let Ok(val) = u32::from_str_radix(hex, 16) else {
                    break;
                };
                for j in 0..4 {
                    out_buf[cnt_read] = ((val >> (j * 8)) & 0xFF) as u8;
                    cnt_read += 1;
                    if cnt_read == out_buf.len() {
                        return Ok(());
                    }
                }
                if i == 4 - 1 {
                    break;
                }
            }
            self.buf_line.clear();
        }
        self.clear_read_buffer()?;
        Ok(())
    }
}

impl<S: SerialPort> Dumper<S> {
    pub(crate) fn serial(&mut self) -> &mut S {
        self.serial.get_mut()
    }

    /// Sends and flushes a U-boot command, always with an ending `\n`.
    pub(crate) fn send_cmd(&mut self, cmd: &str) -> Result<(), Error> {
        self.serial().write_all(cmd.as_bytes())?;
        self.serial().write_all(b"\n")?;
        self.serial().flush()?;
        self.serial().clear(serialport::ClearBuffer::Output)?;
        Ok(())
    }

    /// Sends and flushes a U-boot command, receives all lines of the response.
    /// No more line may be received if the timeout of `serial` is reached.
    ///
    /// This is not fast enough for frequently repeated commands.
    pub(crate) fn send_cmd_and_get_reply(&mut self, cmd: &str) -> Result<String, Error> {
        self.clear_read_buffer()?;
        self.send_cmd(cmd)?;
        let mut resp = String::new();
        let mut reader = io::BufReader::new(&mut self.serial);
        while reader.read_line(&mut resp).is_ok() {}
        Ok(resp)
    }

    /// Reads exact one line. Use this if it is known that a command's response is only 1 line.
    #[allow(unused)]
    pub(crate) fn read_line(&mut self) -> Result<String, Error> {
        self.buf_line.clear();
        self.serial.read_line(&mut self.buf_line)?;
        let line = self.buf_line.clone();
        self.buf_line.clear();
        Ok(line)
    }

    /// Waits and discards every line received before the timeout of `serial` is reached.
    /// Clears read buffer of `serial`.
    pub(crate) fn reach_end_of_receiving(&mut self) -> Result<(), Error> {
        self.clear_read_buffer()?;
        let mut reader = io::BufReader::new(&mut self.serial);
        while let Ok(cnt) = reader.read_line(&mut self.buf_line) {
            self.buf_line.clear();
            // XXX: is this reliable for expecting no more lines?
            if cnt == 0 {
                break;
            }
        }
        self.buf_line.clear();
        self.clear_read_buffer()
    }

    /// Clears read buffer of the `serial`.
    pub(crate) fn clear_read_buffer(&mut self) -> Result<(), Error> {
        self.serial().clear(serialport::ClearBuffer::Input)?;
        Ok(())
    }

    /// Read lines until one line that contains the `header` and returns that line.
    pub(crate) fn read_until_header(&mut self, header: &str) -> Result<String, Error> {
        loop {
            self.buf_line.clear();
            self.serial.read_line(&mut self.buf_line).map_err(|e| {
                if let io::ErrorKind::TimedOut = e.kind() {
                    Error::Shell(format!("did not receive expected `{header}`"))
                } else {
                    e.into()
                }
            })?;
            if self.buf_line.contains(header) {
                let line = self.buf_line.clone();
                self.buf_line.clear();
                return Ok(line);
            }
        }
    }

    /// Reads and fill pure hex strings of bytes into `out_buf`, seperated by white spaces.
    pub(crate) fn read_bytes_from_hex(&mut self, out_buf: &mut [u8]) -> Result<(), Error> {
        self.buf_line.clear();
        let mut cnt_read = 0;
        while cnt_read < out_buf.len() {
            let sz = self.serial.read_line(&mut self.buf_line)?;
            if sz == 0 {
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "check serial",
                )));
            }
            for hex in self.buf_line.split_whitespace() {
                let val = u8::from_str_radix(hex, 16).map_err(|e| {
                    Error::Shell(format!(
                        "unable to parse hex in line '{}' which should be a hex line: {e}",
                        self.buf_line
                    ))
                })?;
                out_buf[cnt_read] = val;
                cnt_read += 1;
                if cnt_read == out_buf.len() {
                    return Ok(());
                }
            }
            self.buf_line.clear();
        }
        Ok(())
    }
}
