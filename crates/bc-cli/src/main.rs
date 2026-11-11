//! `boot-craft`: the command line over bc-core.

use std::fs::File;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bc_core::detect::Mode;
use bc_core::hash::{self, Sha256Digest, to_hex};
use bc_core::layout::Scheme;
use bc_core::write::{self, Target, WriteProgress};
use bc_core::{Drive, Error, Policy, checksums, image, platform};
use bc_core::{unattend, winusb};
use clap::{Args, Parser, Subcommand};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(name = "boot-craft", version, about = "Make bootable USB drives from ISO and disk images")]
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
        /// Keep watching, and list again whenever a drive comes, goes or changes
        #[arg(long)]
        watch: bool,
    },
    /// Show what an image is and whether a raw write will boot
    Info { image: PathBuf },
    /// Print a file's SHA-256
    Hash { file: PathBuf },
    /// List the files inside an ISO image
    Ls {
        image: PathBuf,
        /// Only this directory and what's under it
        #[arg(default_value = "")]
        path: String,
        /// List at most this many entries
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
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
    /// The image's expected SHA-256, checked before anything is erased.
    /// Without it, a SHA256SUMS (or similar) file next to the image is used.
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
    /// Unmount the drive's volumes first instead of refusing (never the system's)
    #[arg(long)]
    unmount: bool,
    /// Don't ask for confirmation
    #[arg(long, short)]
    yes: bool,
    #[command(flatten)]
    windows: WindowsArgs,
}

/// How Windows install drives are made (Windows ISOs only).
#[derive(Args, Default)]
#[command(next_help_heading = "Windows install drives")]
struct WindowsArgs {
    /// Partition table: gpt (UEFI, the default) or mbr (UEFI on older firmware)
    #[arg(long, value_enum, default_value_t = SchemeArg::Gpt)]
    scheme: SchemeArg,
    /// Windows 11: install without TPM 2.0, Secure Boot or 4 GB of RAM
    #[arg(long)]
    bypass_requirements: bool,
    /// Make this local account instead of signing in to a Microsoft account (no internet needed)
    #[arg(long, value_name = "NAME")]
    local_account: Option<String>,
    /// Answer the privacy questions with no
    #[arg(long)]
    skip_privacy: bool,
    /// Language and region for setup and Windows, such as en-US or fr-FR
    #[arg(long, value_name = "LOCALE")]
    locale: Option<String>,
    /// Don't let Windows turn on BitLocker device encryption by itself
    #[arg(long)]
    no_bitlocker: bool,
    /// Where to split an install.wim over 4 GiB (needs that much free space)
    #[arg(long, value_name = "DIR")]
    temp_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
enum SchemeArg {
    #[default]
    Gpt,
    Mbr,
}

impl WindowsArgs {
    fn options(&self) -> winusb::Options {
        let unattend = unattend::Options {
            bypass_requirements: self.bypass_requirements,
            local_account: self.local_account.clone(),
            skip_privacy: self.skip_privacy,
            locale: self.locale.clone(),
            no_bitlocker: self.no_bitlocker,
        };
        let scheme = match self.scheme {
            SchemeArg::Gpt => Scheme::Gpt,
            SchemeArg::Mbr => Scheme::Mbr,
        };
        winusb::Options { scheme, unattend, temp_dir: self.temp_dir.clone(), ..winusb::Options::default() }
    }

    /// Whether any option that only Windows images use was given.
    fn any(&self) -> bool {
        self.options().unattend.any() || self.scheme != SchemeArg::Gpt || self.temp_dir.is_some()
    }
}

/// The user stopped the command (Ctrl-C); exits with 130 like an interrupted program.
#[derive(Debug)]
struct Stopped(String);

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Stopped {}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is::<Stopped>() => {
            eprintln!("{e}");
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A flag Ctrl-C sets, so long work can stop cleanly; a second Ctrl-C quits at once.
fn stop_on_ctrl_c() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let installed = ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
        eprintln!("\nStopping... (Ctrl-C again to quit at once)");
    });
    if let Err(e) = installed {
        eprintln!("note: Ctrl-C won't stop cleanly ({e})");
    }
    stop
}

fn run(cli: Cli) -> Result {
    match cli.command {
        Command::List { allow_loop, watch } => {
            let policy = Policy { allow_loop, ..Policy::default() };
            if watch { watch_drives(&policy) } else { list(&platform::list_drives()?, &policy) }
        }
        Command::Info { image } => info(&image),
        Command::Hash { file } => {
            let digest = hash_file(&file, &stop_on_ctrl_c())?;
            println!("{}  {}", to_hex(&digest), file.display());
            Ok(())
        }
        Command::Ls { image, path, limit } => list_files(&image, &path, limit),
        Command::Write(args) => write_image(&args),
        Command::Mcp { allow_writes, allow_loop } => {
            let options = bc_mcp::Options { allow_writes, allow_loop, ..bc_mcp::Options::default() };
            Ok(bc_mcp::serve(io::stdin().lock(), io::stdout().lock(), options)?)
        }
    }
}

/// Lists the drives, then again each time they change, until Ctrl-C.
fn watch_drives(policy: &Policy) -> Result {
    let mut shown: Option<Vec<Drive>> = None;
    loop {
        let drives = platform::list_drives()?;
        if shown.as_ref() != Some(&drives) {
            if shown.is_some() {
                println!();
            }
            println!("Drives (watching for changes; Ctrl-C to stop):");
            list(&drives, policy)?;
            shown = Some(drives);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn list(drives: &[Drive], policy: &Policy) -> Result {
    if drives.is_empty() {
        println!("No drives found.");
        return Ok(());
    }
    println!("{:<10} {:>10}  {:<8}  {:<28}  STATUS", "DRIVE", "SIZE", "TYPE", "MODEL");
    for drive in drives {
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
    let (kind, source, header) = (info.kind(), &info.source, &info.header);
    println!("File:      {} ({})", path.display(), size(source.file_size));
    println!("Stored:    {}", source.describe());
    match source.size {
        Some(bytes) => println!("Image:     {} ({bytes} bytes)", size(bytes)),
        None => println!("Image:     size known only once decompressed"),
    }
    let found = &info.detected;
    println!("OS:        {}", found.os);
    if found.summary() != found.os.to_string() {
        println!("Name:      {}", found.summary());
    }
    if let Some(format) = found.format {
        println!("Files:     {}", format.label());
    }
    if !found.evidence.is_empty() {
        println!("Found by:  {}", found.evidence.join("; "));
    }
    println!("Kind:      {}", kind.describe());
    if let Some(iso) = &header.iso {
        println!("Label:     {}", iso.volume_id);
        println!("El Torito: {}", yes_no(iso.el_torito));
    }
    println!("MBR:       {}", yes_no(header.mbr));
    println!("GPT:       {}", yes_no(header.gpt));
    match checksums::find(path) {
        Some(published) => println!("Checksum:  listed in {} (checked by `write`)", published.from.display()),
        None => println!("Checksum:  no SHA256SUMS or .sha256 file next to it"),
    }
    for (i, edition) in found.editions.iter().enumerate() {
        println!("{}{edition}", if i == 0 { "Editions:  " } else { "           " });
    }
    println!("Writes as: {}", found.mode.describe());
    if found.mode == Mode::WindowsFiles {
        match winusb::plan(&info.source, found, &winusb::Options::default()) {
            Ok(plan) => println!("Plan:      {}", plan.describe()),
            Err(e) => println!("Plan:      can't be made: {e}"),
        }
    }
    println!();
    if found.mode.supported() {
        println!("It can be written with `boot-craft write`.");
    } else {
        println!("`boot-craft write` can't make this bootable yet; --force writes it raw, which won't boot.");
    }
    Ok(())
}

fn list_files(image: &Path, path: &str, limit: usize) -> Result {
    let tree = bc_core::tree::Tree::open(File::open(image)?)?;
    let mut tree = tree.ok_or("there's no ISO 9660 or UDF file system in it (compressed images must be decompressed first)")?;
    let start = tree.find(path)?.ok_or_else(|| format!("no {path} in the image"))?;
    let prefix = if path.trim_matches('/').is_empty() { String::new() } else { format!("{}/", path.trim_matches('/')) };
    let entries = if start.is_dir { tree.walk_from(&start, limit)? } else { vec![(String::new(), start)] };
    eprintln!("{} ({}):", image.display(), tree.format().label());
    for (sub, entry) in &entries {
        let shown = format!("{prefix}{sub}");
        if entry.is_dir {
            println!("{:>12}  {shown}/", "");
        } else {
            println!("{:>12}  {shown}", size(entry.size));
        }
    }
    if entries.len() >= limit {
        eprintln!("(stopped at {limit} entries; --limit lists more)");
    }
    Ok(())
}

fn write_image(args: &WriteArgs) -> Result {
    let policy = Policy { allow_loop: args.allow_loop, unmount: args.unmount };
    let stop = stop_on_ctrl_c();

    // Everything that can go wrong without touching the drive comes first.
    let info = image::inspect(&args.image)?;
    let mode = info.detected.mode;
    let windows = mode == Mode::WindowsFiles && !args.force;
    if args.windows.any() && !windows {
        return Err("--scheme, --temp-dir and the Windows setup options are for Windows install images only".into());
    }
    let plan = if windows { Some(winusb::plan(&info.source, &info.detected, &args.windows.options())?) } else { None };
    if !mode.supported() && !args.force {
        return Err(format!(
            "{} is {}: it needs {}.\nWritten raw it won't boot; pass --force to write it raw anyway.",
            args.image.display(),
            info.detected.summary(),
            mode.describe()
        )
        .into());
    }
    let expected = match &args.sha256 {
        Some(hex) => Some((hash::parse_hex(hex).ok_or("--sha256 takes 64 hex digits")?, "--sha256".to_string())),
        None => checksums::find(&args.image).map(|p| (p.sha256, p.from.display().to_string())),
    };
    if let Some((expected, from)) = expected {
        let actual = hash_file(&args.image, &stop).map_err(|e| stopped_or(e, "Stopped; nothing was written."))?;
        if actual != expected {
            return Err(format!(
                "the image's SHA-256 is {}, but {from} says {}: the download is damaged or it's a different file. Nothing was written.",
                to_hex(&actual),
                to_hex(&expected)
            )
            .into());
        }
        eprintln!("SHA-256 matches {from}.");
    }
    let drive = platform::find_drive(&args.drive)?.ok_or_else(|| format!("no drive named {}; see `boot-craft list`", args.drive))?;
    drive.check(&policy).map_err(|reason| format!("{} can't be written: {reason}", drive.name))?;
    if let Some(plan) = &plan {
        plan.fits(drive.size_bytes)?;
    } else if let Some(bytes) = info.source.size {
        write::fits(bytes, drive.size_bytes)?;
    }
    let what = match &plan {
        Some(plan) => {
            let mut text = format!(
                "makes a Windows install drive from {} ({}):
  {}",
                args.image.display(),
                info.detected.summary(),
                plan.describe()
            );
            for option in &plan.unattend_options {
                text.push_str(&format!("\n  setup will {option}"));
            }
            text
        }
        None => format!("writes {} to it.", args.image.display()),
    };
    confirm(&drive, &what, policy.unmount, args.yes)?;
    if stop.load(Ordering::SeqCst) {
        return Err(Stopped("Stopped; nothing was written.".into()).into());
    }
    if let Some(plan) = &plan {
        return write_windows(args, &info.source, plan, &drive, &policy, &stop);
    }

    let mut target = Target::open(&drive, &policy)?;
    target.cancel_on(Arc::clone(&stop));
    let (total, file_size) = (info.source.size, info.source.file_size.max(1));
    let mut meter = Meter::new("Writing  ", total);
    let written = target
        .write_image(&info.source, &mut |p: WriteProgress| {
            let ratio = match total {
                Some(total) => p.written as f64 / total.max(1) as f64,
                None => p.file_read as f64 / file_size as f64,
            };
            meter.update(p.written, ratio);
        })
        .map_err(|e| {
            let partly = format!("Stopped. {} is only partly written and won't boot; write it again or format it.", drive.name);
            stopped_or(e, &partly)
        })?;
    meter.finish(written.bytes);
    if let Err(e) = &written.reread {
        eprintln!("note: the system didn't reread the new partition table ({e}); unplug and replug the drive to see it");
    }
    if !args.no_verify {
        let mut meter = Meter::new("Verifying", Some(written.bytes));
        target
            .verify(&written, &mut |done| meter.update(done, done as f64 / written.bytes as f64))
            .map_err(|e| stopped_or(e, "Stopped while verifying: the image was written in full, but not checked."))?;
        meter.finish(written.bytes);
        eprintln!("Verified: the drive matches the image (SHA-256 {}).", to_hex(&written.sha256));
    }
    eprintln!("Done. {} can be removed.", drive.name);
    Ok(())
}

/// Splits install.wim if need be (before anything is erased), then makes the stick.
fn write_windows(
    args: &WriteArgs,
    source: &bc_core::source::Source,
    plan: &winusb::Plan,
    drive: &Drive,
    policy: &Policy,
    stop: &Arc<AtomicBool>,
) -> Result {
    let mut meters = Meters::default();
    let prepared = winusb::prepare(source, plan, Some(stop), &mut |step, done, total| meters.update(step, done, total))
        .map_err(|e| stopped_or(e, "Stopped; nothing was written."))?;
    meters.done();

    let mut target = Target::open(drive, policy)?;
    target.cancel_on(Arc::clone(stop));
    let written = target
        .write_windows(source, plan, &prepared, !args.no_verify, &mut |step, done, total| meters.update(step, done, total))
        .map_err(|e| {
            if meters.step == Some(winusb::Step::Verifying) {
                stopped_or(e, "Stopped while verifying: every file was written, but not all were checked.")
            } else {
                stopped_or(e, &format!("Stopped. {} is only partly written and won't boot; write it again or format it.", drive.name))
            }
        })?;
    meters.done();
    if let Err(e) = &written.reread {
        eprintln!("note: the system didn't reread the new partition table ({e}); unplug and replug the drive to see it");
    }
    let count = written.built.files.len();
    if written.verified {
        eprintln!("Verified: all {count} files read back from the drive match ({}).", size(written.built.bytes));
    } else {
        eprintln!("Copied {count} files ({}); not read back (--no-verify).", size(written.built.bytes));
    }
    eprintln!("Done. {} can be removed. Boot from it in UEFI mode to install Windows.", drive.name);
    Ok(())
}

/// One progress line per step of making a Windows stick.
#[derive(Default)]
struct Meters {
    step: Option<winusb::Step>,
    meter: Option<Meter>,
    last: u64,
}

impl Meters {
    fn update(&mut self, step: winusb::Step, done: u64, total: u64) {
        if self.step != Some(step) {
            self.done();
            let label = match step {
                winusb::Step::Extracting => "Extracting",
                winusb::Step::Splitting => "Splitting install.wim for FAT32 (this takes a few minutes)...",
                winusb::Step::Copying => "Copying  ",
                winusb::Step::Verifying => "Verifying",
            };
            if step == winusb::Step::Splitting {
                eprintln!("{label}");
                self.meter = None;
            } else {
                self.meter = Some(Meter::new(label, Some(total)));
            }
            self.step = Some(step);
        }
        self.last = done;
        if let Some(meter) = &mut self.meter {
            meter.update(done, done as f64 / total.max(1) as f64);
        }
    }

    fn done(&mut self) {
        if let Some(mut meter) = self.meter.take() {
            meter.finish(self.last);
        }
    }
}

/// `message` as a [`Stopped`] if `e` is a cancellation, else `e` itself.
fn stopped_or(e: impl Into<Box<dyn std::error::Error>>, message: &str) -> Box<dyn std::error::Error> {
    let e = e.into();
    let cancelled = matches!(e.downcast_ref::<Error>(), Some(Error::Cancelled))
        || e.downcast_ref::<io::Error>().is_some_and(hash::is_cancelled)
        || e.is::<Stopped>();
    if cancelled { Box::new(Stopped(message.into())) } else { e }
}

/// Says what will be erased and, unless `yes`, has the user type the drive's name.
fn confirm(drive: &Drive, what: &str, unmount: bool, yes: bool) -> Result {
    eprintln!("This erases everything on {} ({}, {}):", drive.path.display(), drive.label(), size(drive.size_bytes));
    for partition in &drive.partitions {
        eprintln!("  {}  {}", partition.name, size(partition.size_bytes));
    }
    if unmount && drive.mounted() {
        let mounted: Vec<String> = drive
            .usage
            .iter()
            .filter_map(|u| match &u.kind {
                bc_core::UsageKind::Mounted(at) => Some(at.display().to_string()),
                _ => None,
            })
            .collect();
        eprintln!("unmounting {} first,", mounted.join(", "));
    }
    eprintln!("and {what}");
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

/// The SHA-256 of the file itself, as publishers list it (of the `.xz`, not
/// of what's inside).
fn hash_file(path: &Path, stop: &AtomicBool) -> Result<Sha256Digest> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut meter = Meter::new("Hashing  ", Some(len));
    let digest = hash::hash_exact_until(&mut file, len, Some(stop), &mut |done| meter.update(done, done as f64 / len.max(1) as f64))
        .map_err(|e| stopped_or(e, "Stopped."))?;
    meter.finish(len);
    Ok(digest)
}

/// A progress line on stderr, redrawn at most five times a second.
struct Meter {
    label: &'static str,
    total: Option<u64>,
    start: Instant,
    last: Option<Instant>,
    shown: bool,
}

impl Meter {
    /// `total` is `None` when the size isn't known in advance (compressed images).
    fn new(label: &'static str, total: Option<u64>) -> Self {
        Self { label, total, start: Instant::now(), last: None, shown: io::stderr().is_terminal() }
    }

    /// `ratio` is how far along the work is, from 0 to 1.
    fn update(&mut self, done: u64, ratio: f64) {
        let now = Instant::now();
        if self.last.is_some_and(|last| now - last < Duration::from_millis(200)) {
            return;
        }
        self.last = Some(now);
        self.draw(done, ratio);
    }

    fn finish(&mut self, done: u64) {
        if self.shown && self.last.is_some() {
            self.draw(done, 1.0);
            eprintln!();
        }
    }

    fn draw(&self, done: u64, ratio: f64) {
        if !self.shown {
            return;
        }
        let seconds = self.start.elapsed().as_secs_f64().max(0.001);
        let percent = (ratio.clamp(0.0, 1.0) * 100.0) as u32;
        let amount = match self.total {
            Some(total) => format!("{} / {}", size(done), size(total)),
            None => size(done),
        };
        eprint!("\r{} {percent:>3}%  {amount}  {}/s   ", self.label, size((done as f64 / seconds) as u64));
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
