#![deny(unsafe_code)]

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use clap::Parser;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use serialport::{SerialPort, SerialPortType};

use uboot_nand_dump::{DumpBuf, DumpMode, Dumper};

const PARTIAL_FILE_SUFFIX: &str = ".partial";

#[derive(clap::Parser)]
#[command(name = "uboot-nand-dump")]
#[command(version = "0.1.0")]
#[command(
    about = "Dumps NAND flash image via U-Boot serial interface",
    long_about = "<https://crates.io/crates/uboot-nand-dump/0.1.0>"
)]
struct Cli {
    #[arg(help = "specific config for NAND parameters and U-boot operation settings")]
    conf: PathBuf,
    #[arg(long, help = "system name of the serial port")]
    port: Option<String>,
    #[arg(
        long,
        short,
        help = "sends spaces to stop auto boot; check if U-boot exists"
    )]
    prep_power_on: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Create new configuration, which is needed for all other functions
    NewConf,
    /// Check for stable serial connection
    CheckComm(DurationArg),
    /// Probe some U-Boot info
    UbootInfo,
    /// Read a range of NAND data
    Read(NandRwArgs),
    /// Merge main-only and OOB-only dump files
    Merge(MergeArgs),
    /// Split main+OOB interleaved dump file into main-only and OOB-only files
    Split(SplitArgs),
}

#[derive(clap::Args)]
struct DurationArg {
    #[arg(help = "duration in seconds")]
    duration: u32,
}

#[derive(clap::Args)]
struct NandRwArgs {
    #[arg(
        long,
        help = "only read OOB (fast), if the main data is read elsewhere; offset/size still refer to the main data"
    )]
    oob_only: bool,
    #[arg(help = "name for the dump file")]
    name: String,
    #[arg(long, help = "it must be page-aligned; defaults to 0")]
    offset: Option<String>,
    #[arg(long, help = "defaults to the remaining flash size after `offset`")]
    size: Option<String>,
    #[arg(long, help = "interleave main+OOB for each page, like `nanddump`")]
    interleave: bool,
    #[arg(long, help = "save partial record if offset+size < flash size")]
    partial: bool,
}

#[derive(clap::Args)]
struct MergeArgs {
    main: PathBuf,
    oob: PathBuf,
    output: PathBuf,
}

#[derive(clap::Args)]
struct SplitArgs {
    input: PathBuf,
    main: PathBuf,
    oob: PathBuf,
}

fn conf_input(
    mut serial: impl SerialPort,
) -> Result<uboot_nand_dump::Config, Box<dyn std::error::Error>> {
    let mut conf = uboot_nand_dump::Config::default();

    conf.baud_rate = ask_value_or_notify("baud rate (optional)", conf.baud_rate());

    serial.set_baud_rate(conf.baud_rate())?;
    let mut dumper = Dumper::build(serial, conf.clone())?;
    let infos = dumper.probe_uboot_info()?;
    print_uboot_info(&infos, false);
    println!("\nPlease determine input parameters with the info printed above.\n");

    conf.nand_conf.page_size = ask_value_or_default("page size", conf.nand_conf.page_size);
    conf.nand_conf.page_oob_size =
        ask_value_or_default("page OOB size", conf.nand_conf.page_oob_size);
    conf.nand_conf.flash_size = ask_value_or_default("flash size (B)", conf.nand_conf.flash_size);
    // TODO: ask block size (erase size) when adding erasing/writing functions.

    conf.nand_index = ask_value_or_notify("NAND index in U-Boot (optional)", conf.nand_index());
    conf.page_buf_ram_offset = ask_value(
        "\
Give a start offset of a free target RAM space given to this utility if the target \
RAM address space is known, the space must be *enough* for 1 NAND page with OOB:\
    ",
    );

    println!("Input a string if it should be found in the response of the `nand device` command:");
    conf.expected_nand_info = input_line().map(|s| s.trim().to_string());

    conf.check()?;
    Ok(conf)
}

fn conf_load(path: &Path) -> Result<uboot_nand_dump::Config, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let conf: uboot_nand_dump::Config = serde_json::from_reader(file)?;
    conf.check()?;
    Ok(conf)
}

fn conf_save(
    conf: &uboot_nand_dump::Config,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(file, conf)?;
    println!("Saved `{}`.", path.to_string_lossy());
    Ok(())
}

fn buf_load_partial(path: &Path) -> Result<DumpBuf, Box<dyn std::error::Error>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(postcard::from_bytes(&buf)?)
}

fn buf_save_partial(buf: &DumpBuf, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let file = postcard::to_io(buf, file)?;
    file.sync_all()?;
    println!("Saved `{}`.", path.to_string_lossy());
    Ok(())
}

fn buf_save(
    buf: &DumpBuf,
    path: &Path,
    interleave: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if interleave && buf.dump_mode() == DumpMode::Both {
        let mut dump_path = PathBuf::from(path);
        dump_path.set_extension("bin");
        buf.save(&dump_path)?;
        println!("Saved `{}`.", dump_path.to_string_lossy());
    } else {
        let mut data_path = PathBuf::from(path);
        if buf.dump_mode().has_main() {
            data_path.set_extension("bin");
            buf.save_data(&data_path)?;
            println!("Saved `{}`.", data_path.to_string_lossy());
        }
        if buf.dump_mode().has_oob() {
            let mut oobs_path = data_path.clone();
            oobs_path.set_extension("oob");
            oobs_path.add_extension("bin");
            buf.save_oobs(&oobs_path)?;
            println!("Saved `{}`.", oobs_path.to_string_lossy());
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if let Commands::NewConf = cli.command {
        let port_name = if let Some(name) = cli.port {
            name
        } else {
            ask_port_selection()?
        };
        let port = serialport::new(&port_name, 115_200).open_native()?;
        let conf = conf_input(port)?;
        let mut path = cli.conf.clone();
        path.set_extension("json");
        conf_save(&conf, &path)?;
        return Ok(());
    }

    let conf = conf_load(&cli.conf)?;

    match cli.command {
        Commands::Merge(MergeArgs { main, oob, output }) => {
            let mut buf = DumpBuf::build(&conf.nand_conf, DumpMode::MainOnly, 0)?;
            buf.append(&std::fs::read(main)?)?;
            buf.merge_oobs(&std::fs::read(oob)?)?;
            buf.save(&output)?;
            return Ok(());
        }
        Commands::Split(SplitArgs { input, main, oob }) => {
            let mut buf = DumpBuf::build(&conf.nand_conf, DumpMode::Both, 0)?;
            buf.append(&std::fs::read(input)?)?;
            buf.save_data(&main)?;
            buf.save_oobs(&oob)?;
            return Ok(());
        }
        _ => (),
    }

    let port_name = if let Some(name) = cli.port {
        name
    } else {
        ask_port_selection()?
    };
    let port = serialport::new(&port_name, conf.baud_rate()).open_native()?;
    let mut dumper = Dumper::build(port, conf.clone())?;
    if cli.prep_power_on {
        println!("preparing for the device's power on event for 5 secs...");
        dumper.prep_for_power_on(Duration::from_secs(5))?;
    }

    match cli.command {
        Commands::CheckComm(DurationArg { duration }) => {
            let cnt_checked = dumper.check_comm(Duration::from_secs(duration as u64))?;
            println!("{cnt_checked} characters of echo loopback checked OK");
        }
        Commands::UbootInfo => {
            let infos = dumper.probe_uboot_info()?;
            print_uboot_info(&infos, true);
        }
        Commands::Read(read_params) => {
            if conf.page_buf_ram_offset.is_none() {
                println!(
                    "*** WARNING ***: `page_buf_ram_offset` is not set, CRC32 check will not be performed."
                );
            }
            let (info, bad_info) = (dumper.nand_info()?, dumper.nand_bad_info()?);
            println!(
                "Please check the page size {} and page OOB size {} against the U-boot info below:",
                conf.nand_conf.page_size, conf.nand_conf.page_oob_size
            );
            println!("{info}\n{bad_info}\n");

            let dump_mode = if read_params.oob_only {
                DumpMode::OobOnly
            } else {
                DumpMode::Both
            };

            // The range is always for main data, even in OOB-only dump mode.
            let read_range = {
                let param_offset = read_params.offset.and_then(|s| parse_hex_or_dec_value(&s));
                let param_size: Option<usize> =
                    read_params.size.and_then(|s| parse_hex_or_dec_value(&s));
                let off_start = param_offset.unwrap_or(0);
                let off_end = param_size
                    .map(|s| off_start + s)
                    .unwrap_or(conf.nand_conf.flash_size);
                let off_end = off_end.div_ceil(conf.nand_conf.page_size) * conf.nand_conf.page_size;
                off_start..off_end
            };

            let partial_file_path = PathBuf::from(read_params.name.clone() + PARTIAL_FILE_SUFFIX);
            let unfinished_result = buf_load_partial(&partial_file_path).ok().and_then(|res| {
                if res.nand_config() == &conf.nand_conf
                    && res.range().start == read_range.start
                    && res.dump_mode() == dump_mode
                    && ask_yes_no("Resume the partial dump? If so, make sure the OS haven't booted since that operation.")
                        .unwrap_or(false)
                {
                    Some(res)
                } else {
                    None
                }
            });

            let mut result =
                unfinished_result.unwrap_or(dumper.init_read(read_range.start, dump_mode)?);

            println!(
                "Reading within range [{:#x}, {:#x}) of NAND device {}",
                read_range.start,
                read_range.end,
                conf.nand_index()
            );

            let total_size = result.page_dump_size() * read_range.len() / conf.nand_conf.page_size;
            let progress = ProgressBar::new(total_size as u64);
            progress.set_style(
                ProgressStyle::with_template(
                    "[{elapsed_precise}] {bar:40.cyan/blue} {msg} {percent_precise}% {bytes_per_sec}",
                )
                .unwrap()
                .progress_chars("##-"),
            );
            let progress = progress.with_position(result.data_size() as u64); // recover from saved partial

            // NOTE: this is the time-consuming loop
            let mut read_error = None;
            while result.range().end < read_range.end {
                if let Err(e) = dumper.read_next_page(&mut result) {
                    read_error.replace(e);
                    break;
                }
                progress.inc(result.page_dump_size() as u64);
                progress.set_message(HumanBytes(result.data_size() as u64).to_string());
            }

            if (read_params.partial && read_range.end < conf.nand_conf.flash_size)
                || read_error.is_some()
            {
                buf_save_partial(&result, &partial_file_path)?;
            } else {
                buf_save(
                    &result,
                    &PathBuf::from(read_params.name),
                    read_params.interleave,
                )?;
                let _ = std::fs::remove_file(&partial_file_path);
            }

            if let Some(e) = read_error {
                return Err(e.into());
            }
        }
        _ => {
            unreachable!()
        }
    }

    Ok(())
}

fn input_line() -> Option<String> {
    io::stdin().lines().next()?.ok()
}

fn ask_yes_no(question: &str) -> Option<bool> {
    print!("{} (Y/n) ", question);
    io::stdout().flush().ok()?;
    let input = input_line()?;
    match input.trim().chars().next()? {
        'Y' | 'y' => Some(true),
        'n' | 'N' => Some(false),
        _ => None,
    }
}

fn ask_value<T: FromStr + TryFrom<u64>>(param_name: &str) -> Option<T> {
    print!("Input {} (use `0x` prefix for hex): ", param_name);
    io::stdout().flush().ok()?;
    let input = input_line()?;
    parse_hex_or_dec_value(input.trim())
}

fn parse_hex_or_dec_value<T: FromStr + TryFrom<u64>>(input: &str) -> Option<T> {
    if input.starts_with("0x") || input.starts_with("0X") {
        let hex = input.get(2..)?;
        u64::from_str_radix(hex, 16).ok()?.try_into().ok()
    } else {
        input.parse().ok()
    }
}

fn ask_value_or_notify<T: FromStr + TryFrom<u64> + std::fmt::Display, U: std::fmt::Display>(
    param_name: &str,
    notify_default: U,
) -> Option<T> {
    let res = ask_value(param_name);
    if res.is_none() {
        println!("Using default value {notify_default}.");
    }
    res
}

fn ask_value_or_default<T: FromStr + TryFrom<u64> + std::fmt::Display>(
    param_name: &str,
    default_val: T,
) -> T {
    ask_value(param_name).unwrap_or_else(|| {
        println!("Using default value {default_val}.");
        default_val
    })
}

fn ask_port_selection() -> Result<String, Box<dyn std::error::Error>> {
    println!("Available ports:");
    let mut names = Vec::new();
    for (i, info) in serialport::available_ports()?.iter().enumerate() {
        let desc = match info.port_type.clone() {
            SerialPortType::BluetoothPort => "Bluetooth serial",
            SerialPortType::PciPort => "PCI permanent port",
            SerialPortType::UsbPort(info) => &format!(
                "USB serial, {}, {}",
                info.manufacturer
                    .unwrap_or_else(|| format!("VID: 0x{:04X}", info.vid)),
                info.product
                    .unwrap_or_else(|| format!("PID: 0x{:04X}", info.pid))
            ),
            SerialPortType::Unknown => "Unknown type",
        };
        println!("{}.\t{}\t{}\n", i + 1, info.port_name.trim(), desc);
        names.push(info.port_name.clone());
    }
    if names.is_empty() {
        return Err("No available ports".into());
    }
    let i: usize =
        ask_value("one of the index number printed here").ok_or("Serial port not selected")?;
    Ok(names.into_iter().nth(i - 1).ok_or("bad number entered")?)
}

fn print_uboot_info(infos: &Vec<(String, String)>, print_long: bool) {
    for (cmd, info) in infos {
        if print_long
            || ["version", "bdinfo", "iminfo", "mtdparts", "nand info"].contains(&cmd.as_str())
        {
            if !print_long {
                println!("Response of command `{cmd}`:");
            } else {
                println!(
                    "----------------------- Response of command `{cmd}` -----------------------"
                );
            }
            println!("{info}\n");
        }
    }
}
