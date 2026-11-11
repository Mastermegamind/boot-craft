//! `rust-rufus`: the command line over rr-core.

use std::fs::File;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use rr_core::hash::{self, Sha256Digest, to_hex};
use rr_core::image;
use rr_core::write::{self, Target};
use rr_core::{Drive, Policy, platform};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "rust-rufus", version, about = "Make bootable USB drives from ISO and disk images")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List drives, and whether each one may be written
    List {
        /// Offer loop devices too (for testing)
        #[arg(long)]
        allow_loop: bool,
    },
    /// Show what an image is and whether a raw write will boot
    Info { image: PathBuf },
    /// Print a file's SHA-256
    Hash { file: PathBuf },
    /// Erase a drive and write an image to it, then verify
    Write(WriteArgs),
    /// Serve AI agents over MCP on stdin/stdout (read-only unless --allow-writes)
    Mcp {
        /// Offer the two-step write tools (prepare_write, confirm_write)
        #[arg(long)]
        allow_writes: bool,
        /// Treat loop devices as writable (for testing)
        #[arg(long)]
        allow_loop: bool,
    },
}

#[derive(Args)]
struct WriteArgs {
    image: PathBuf,
    /// The drive, as `list` shows it: sdb or /dev/sdb
    drive: String,
    /// The image's expected SHA-256, checked before anything is erased
    #[arg(long, value_name = "HEX")]
    sha256: Option<String>,
    /// Don't read the drive back afterwards
    #[arg(long)]
    no_verify: bool,
    /// Write even if the image won't boot when written raw
    #[arg(long)]
    force: bool,
    /// Allow loop devices (for testing)
    #[arg(long)]
    allow_loop: bool,
    /// Don't ask for confirmation
    #[arg(long, short)]
    yes: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result {
    match cli.command {
        Command::List { allow_loop } => list(&Policy { allow_loop }),
        Command::Info { image } => info(&image),
        Command::Hash { file } => {
            let digest = hash_file(&file)?;
            println!("{}  {}", to_hex(&digest), file.display());
            Ok(())
        }
        Command::Write(args) => write_image(&args),
        Command::Mcp { allow_writes, allow_loop } => {
            let options = rr_mcp::Options { allow_writes, allow_loop, ..rr_mcp::Options::default() };
            Ok(rr_mcp::serve(io::stdin().lock(), io::stdout().lock(), options)?)
        }
    }
}

fn list(policy: &Policy) -> Result {
    let drives = platform::list_drives()?;
    if drives.is_empty() {
        println!("No drives found.");
        return Ok(());
    }
    println!("{:<10} {:>10}  {:<8}  {:<28}  STATUS", "DRIVE", "SIZE", "TYPE", "MODEL");
    for drive in &drives {
        let status = match drive.check(policy) {
            Ok(()) => "can be written".to_string(),
            Err(reason) => format!("refused: {reason}"),
        };
        println!("{:<10} {:>10}  {:<8}  {:<28}  {status}", drive.name, size(drive.size_bytes), drive.transport, clip(&drive.label(), 28));
    }
    Ok(())
}

fn info(path: &Path) -> Result {
    let info = image::inspect(path)?;
    let kind = info.kind();
    println!("File:      {}", path.display());
    println!("Size:      {} ({} bytes)", size(info.size), info.size);
    println!("Kind:      {}", kind.describe());
    if let Some(iso) = &info.iso {
        println!("Label:     {}", iso.volume_id);
        println!("El Torito: {}", yes_no(iso.el_torito));
    }
    println!("MBR:       {}", yes_no(info.mbr));
    println!("GPT:       {}", yes_no(info.gpt));
    println!();
    println!("{}", kind.advice());
    Ok(())
}

fn write_image(args: &WriteArgs) -> Result {
    let policy = Policy { allow_loop: args.allow_loop };

    // Everything that can go wrong without touching the drive comes first.
    let info = image::inspect(&args.image)?;
    let kind = info.kind();
    if !kind.boots_when_written_raw() && !args.force {
        return Err(format!("{} is {}.\n{}\nPass --force to write it anyway.", args.image.display(), kind.describe(), kind.advice()).into());
    }
    if let Some(expected) = &args.sha256 {
        let expected = hash::parse_hex(expected).ok_or("--sha256 takes 64 hex digits")?;
        let actual = hash_file(&args.image)?;
        if actual != expected {
            return Err(format!(
                "the image's SHA-256 is {}, not {}: the download is damaged or it's a different file. Nothing was written.",
                to_hex(&actual),
                to_hex(&expected)
            )
            .into());
        }
        eprintln!("SHA-256 matches.");
    }
    let drive = platform::find_drive(&args.drive)?.ok_or_else(|| format!("no drive named {}; see `rust-rufus list`", args.drive))?;
    drive.check(&policy).map_err(|reason| format!("{} can't be written: {reason}", drive.name))?;
    write::fits(info.size, drive.size_bytes)?;
    confirm(&drive, &args.image, args.yes)?;

    let mut target = Target::open(&drive, &policy)?;
    let mut meter = Meter::new("Writing  ", info.size);
    let written = target.write_image(&args.image, &mut |done| meter.update(done))?;
    if let Err(e) = &written.reread {
        eprintln!("note: the system didn't reread the new partition table ({e}); unplug and replug the drive to see it");
    }
    if !args.no_verify {
        let mut meter = Meter::new("Verifying", written.bytes);
        target.verify(&written, &mut |done| meter.update(done))?;
        eprintln!("Verified: the drive matches the image (SHA-256 {}).", to_hex(&written.sha256));
    }
    eprintln!("Done. {} can be removed.", drive.name);
    Ok(())
}

/// Says what will be erased and, unless `yes`, has the user type the drive's name.
fn confirm(drive: &Drive, image: &Path, yes: bool) -> Result {
    eprintln!("This erases everything on {} ({}, {}):", drive.path.display(), drive.label(), size(drive.size_bytes));
    for partition in &drive.partitions {
        eprintln!("  {}  {}", partition.name, size(partition.size_bytes));
    }
    eprintln!("and writes {} to it.", image.display());
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err("input isn't a terminal, so nothing can be confirmed; pass --yes to go ahead".into());
    }
    eprint!("Type {} to continue: ", drive.name);
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    if line.trim() == drive.name { Ok(()) } else { Err("not confirmed; nothing was written".into()) }
}

fn hash_file(path: &Path) -> Result<Sha256Digest> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut meter = Meter::new("Hashing  ", len);
    Ok(hash::hash_exact(&mut file, len, &mut |done| meter.update(done))?)
}

/// A progress line on stderr, redrawn at most five times a second.
struct Meter {
    label: &'static str,
    total: u64,
    start: Instant,
    last: Option<Instant>,
    shown: bool,
}

impl Meter {
    fn new(label: &'static str, total: u64) -> Self {
        Self { label, total, start: Instant::now(), last: None, shown: io::stderr().is_terminal() }
    }

    fn update(&mut self, done: u64) {
        if !self.shown {
            return;
        }
        let now = Instant::now();
        let finished = done >= self.total;
        if !finished && self.last.is_some_and(|last| now - last < Duration::from_millis(200)) {
            return;
        }
        self.last = Some(now);
        let seconds = (now - self.start).as_secs_f64().max(0.001);
        let percent = (done * 100).checked_div(self.total).unwrap_or(100);
        eprint!("\r{} {percent:>3}%  {} / {}  {}/s   ", self.label, size(done), size(self.total), size((done as f64 / seconds) as u64));
        if finished {
            eprintln!();
        }
    }
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.to_string() } else { text.chars().take(max - 1).chain(['…']).collect() }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(32 << 30), "32.0 GiB");
    }

    #[test]
    fn long_models_are_clipped() {
        assert_eq!(clip("short", 28), "short");
        assert_eq!(clip("abcdef", 4), "abc…");
    }

    #[test]
    fn the_cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
