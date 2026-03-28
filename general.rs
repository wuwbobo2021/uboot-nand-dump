use std::{
    io::{self, BufRead, Write},
    thread,
    time::{Duration, Instant},
};

use serialport::SerialPort;

use crate::Error;

pub use dumper_struct::Dumper;
/// Seals the private items in the struct to prevent direct access elsewhere.
mod dumper_struct {
    use crate::{Config, Error};
    use serialport::SerialPort;
    use std::{
        io::{self, BufRead, BufReader, Write},
        time::Duration,
    };
    /// NAND flash image dumper holding the U-Boot serial interface.
    pub struct Dumper<S: SerialPort> {
        serial: BufReader<S>,
        conf: Config,
        buf_line: String, // temp buffer
    }

    impl<S: SerialPort> Dumper<S> {
        /// Builds the NAND flash image dumper.
        pub fn build(mut serial: S, conf: Config) -> Result<Self, Error> {
            conf.check()?;
            serial.set_timeout(Duration::from_millis(500))?;
            serial.set_baud_rate(conf.baud_rate())?;
            Ok(Self {
                serial: BufReader::with_capacity(8192, serial),
                conf,
                buf_line: String::new(),
            })
        }

        /// Takes the underlying serial port interface. Do this if no more NAND dump operation is needed.
        pub fn into_inner(mut self) -> S {
            let _ = self.send_interrupt();
            let _ = self.reach_end_of_receiving();
            self.serial.into_inner()
        }

        /// Checks the stored configuration.
        pub fn config(&self) -> &Config {
            &self.conf
        }

        pub(crate) fn reader(&mut self) -> &mut impl BufRead {
            &mut self.serial
        }

        /// Reads one line into the `Dumper`'s line buffer, which stores the last line
        /// read by this function. Returns `Some` of the reference to the `Dumper`'s
        /// line buffer, or `None` if `BufRead::read_line` returns EOF (`Ok(0)`).
        pub(crate) fn read_line(&mut self) -> Result<Option<&str>, io::Error> {
            self.buf_line.clear();
            if self.serial.read_line(&mut self.buf_line)? == 0 {
                Ok(None)
            } else {
                Ok(Some(self.buf_line.as_str()))
            }
        }

        /// Clears read buffer of the internal `serial`.
        pub(crate) fn clear_read_buffer(&mut self) -> Result<(), io::Error> {
            if !self.serial.buffer().is_empty() {
                self.serial.consume(self.serial.buffer().len());
            }
            self.serial
                .get_mut()
                .clear(serialport::ClearBuffer::Input)?;
            Ok(())
        }

        pub(crate) fn writer(&mut self) -> &mut impl Write {
            self.serial.get_mut()
        }
    }
}

impl<S: SerialPort> Dumper<S> {
    /// Sends and flushes a U-boot command, always with an ending `\n`.
    pub(crate) fn send_cmd(&mut self, cmd: &str) -> Result<(), Error> {
        self.send_interrupt()?;
        self.send_cmd_no_pre_intr(cmd)
    }

    /// The same as [Self::send_cmd], except it does not call [Self::send_interrupt] before the command.
    /// This can be used in a function that sends multiple commands (use `send_cmd` for the first).
    pub(crate) fn send_cmd_no_pre_intr(&mut self, cmd: &str) -> Result<(), Error> {
        self.writer().write_all(cmd.as_bytes())?;
        self.writer().write_all(b"\n")?;
        self.writer().flush()?;
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
        while let Ok(len) = self.reader().read_line(&mut resp) {
            if len == 0 {
                break; // XXX: is EOF reliable for ensuring of reaching the end?
            }
        }
        self.clear_read_buffer()?;
        Ok(resp)
    }

    /// Sends CTRL+C character to make sure the shell is in a clean state.
    pub(crate) fn send_interrupt(&mut self) -> Result<(), Error> {
        const CHAR_CONTROL_C: u8 = 0x03;
        const RESP_INTERRUPT: &str = "<INTERRUPT>";
        self.clear_read_buffer()?;
        for _ in 0..3 {
            self.writer().write_all(&[CHAR_CONTROL_C])?;
            self.writer().flush()?;
            if self.read_until_header(RESP_INTERRUPT).is_ok() {
                return Ok(());
            }
        }
        self.read_until_header(RESP_INTERRUPT)?;
        Ok(())
    }

    /// Waits and discards every line received before the timeout of `serial` is reached.
    /// Clears read buffer of `serial`.
    pub(crate) fn reach_end_of_receiving(&mut self) -> Result<(), Error> {
        self.clear_read_buffer()?;
        while self.read_line_no_eof().is_ok() {}
        self.clear_read_buffer()?;
        Ok(())
    }

    /// Does [Self::read_line], returns `UnexpectedEof` error on EOF.
    pub(crate) fn read_line_no_eof(&mut self) -> Result<&str, io::Error> {
        self.read_line()?
            .ok_or(io::Error::new(io::ErrorKind::UnexpectedEof, "check serial"))
    }

    /// Read lines until one line that contains the `header` and returns that line.
    pub(crate) fn read_until_header(&mut self, header: &str) -> Result<String, Error> {
        loop {
            let line = self
                .read_line()
                .map_err(|e| {
                    if let io::ErrorKind::TimedOut = e.kind() {
                        Error::Shell(format!("did not receive expected `{header}`"))
                    } else {
                        e.into()
                    }
                })?
                .unwrap_or("");
            if line.contains(header) {
                return Ok(String::from(line));
            }
        }
    }

    /// Reads and fill pure hex strings of bytes into `out_buf`, seperated by white spaces.
    pub(crate) fn read_bytes_from_hex(&mut self, out_buf: &mut [u8]) -> Result<(), Error> {
        let mut cnt_read = 0;
        while cnt_read < out_buf.len() {
            let line = self.read_line_no_eof()?;
            for hex in line.split_whitespace() {
                let val = u8::from_str_radix(hex, 16).map_err(|e| {
                    Error::Shell(format!(
                        "unable to parse hex in line '{}' which should be a hex line: {e}",
                        line
                    ))
                })?;
                out_buf[cnt_read] = val;
                cnt_read += 1;
                if cnt_read == out_buf.len() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

impl<S: SerialPort> Dumper<S> {
    /// Sends spaces for stopping the auto booting procedure of U-boot.
    /// Also checks the existence of the name `U-Boot` in the response of `version` command,
    /// returning `Error::UbootNotFound` if not found.
    pub fn prep_for_power_on(&mut self, dur: Duration) -> Result<(), Error> {
        self.clear_read_buffer()?;
        let t_finish = Instant::now() + dur;
        while Instant::now() < t_finish {
            self.writer().write_all(b" ")?;
            self.writer().flush()?;
            thread::sleep(Duration::from_millis(100));
        }
        self.send_interrupt()?;
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
        self.send_interrupt()?;
        let t_finish = Instant::now() + dur;
        let mut cnt_checked = 0;
        while Instant::now() < t_finish {
            self.writer().write_all(b"echo ")?;
            let mut string_check = String::new();
            for i in 0..CHECK_LEN {
                let ch = STR_ALPHA_NUMERIC.as_bytes()
                    [getrandom::u32().unwrap() as usize % STR_ALPHA_NUMERIC.len()];
                self.writer().write_all(&[ch])?;
                string_check.push(char::from_u32(ch as u32).unwrap());
                if i % 16 == 0 {
                    self.writer().flush()?;
                }
            }
            self.writer().write_all(b"\n")?;
            self.writer().flush()?;
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
        self.select_nand(self.config().nand_index())
    }

    /// Gets a bad block list string using the `nand bad` command.
    /// TODO: return a list of offset values instead of a `String`.
    pub fn nand_bad_info(&mut self) -> Result<String, Error> {
        self.select_nand(self.config().nand_index())?;
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
        self.send_interrupt()?;
        self.dump_memory_no_pre_intr(offset, out_buf)
    }

    /// The same as [Self::dump_memory], except it does not call [Self::send_interrupt] before the command.
    /// This can be used in a function that sends multiple commands (send the interrupt for the first).
    pub(crate) fn dump_memory_no_pre_intr(
        &mut self,
        offset: u64,
        out_buf: &mut [u8],
    ) -> Result<(), Error> {
        if out_buf.is_empty() {
            return Ok(());
        }
        self.clear_read_buffer()?;
        self.send_cmd_no_pre_intr(&format!(
            "md.l {:#x} {:#x}",
            offset,
            out_buf.len().div_ceil(4)
        ))?;
        let mut cnt_read = 0;
        while cnt_read < out_buf.len() {
            let line = self.read_line_no_eof()?;
            for (i, hex) in line.split_whitespace().skip(1).enumerate() {
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
        }
        self.clear_read_buffer()?;
        Ok(())
    }
}
