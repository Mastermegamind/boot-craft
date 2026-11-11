//! MCP server: lets AI agents use rust-rufus over stdio (JSON-RPC 2.0,
//! one message per line).
//!
//! Agents get the safety rules people get, never more:
//! - Without `allow_writes` the server is read-only and the write tools
//!   aren't listed at all.
//! - A write takes two calls. `prepare_write` runs every check the CLI runs
//!   and hands back a summary and a one-time token; `confirm_write` needs that
//!   token *and* the drive's name again. A token works once, for one image
//!   and one drive, within [`Options::token_ttl`].
//! - [`Policy`] is the CLI's: no option here unlocks the system disk or an
//!   internal disk.
//!
//! Writing a large ISO takes minutes, longer than many clients let one tool
//! call run. So `confirm_write` starts the write in the background and returns
//! at once; `write_status` reports progress and can wait up to
//! [`MAX_WAIT`] for it to finish. Calls that carry a `progressToken` also get
//! `notifications/progress` while they work. If the input closes mid-write,
//! the server finishes the write before exiting, so a drive is never left
//! half-written by a client going away.
//!
//! stdout carries protocol messages only; nothing else may print to it.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use rr_core::hash::{self, Sha256Digest, to_hex};
use rr_core::image;
use rr_core::write::{self, Target, Written};
use rr_core::{Drive, Error, Policy, platform};
use serde_json::{Value, json};

/// Newest first; a client asking for another gets the newest.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// The longest `write_status` waits in one call, below common client time limits.
pub const MAX_WAIT: Duration = Duration::from_secs(30);
/// Progress notifications are sent at most this often.
const NOTIFY_EVERY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct Options {
    /// Offer `prepare_write`, `confirm_write` and `write_status`.
    pub allow_writes: bool,
    /// Treat loop devices as writable (for tests).
    pub allow_loop: bool,
    /// How long a `prepare_write` token stays valid.
    pub token_ttl: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self { allow_writes: false, allow_loop: false, token_ttl: Duration::from_secs(120) }
    }
}

/// The stage a write is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Writing,
    Verifying,
}

/// Where drives come from. [`SystemDrives`] is the real thing; tests use a fake.
pub trait Drives: Send + Sync + 'static {
    fn list(&self) -> io::Result<Vec<Drive>>;
    fn find(&self, name: &str) -> io::Result<Option<Drive>>;
    /// Erases `drive`, writes `image`, and verifies it, telling `progress`
    /// the phase and the bytes done in it.
    fn write(&self, drive: &Drive, policy: &Policy, image: &Path, progress: &mut dyn FnMut(Phase, u64)) -> Result<Written, Error>;
}

pub struct SystemDrives;

impl Drives for SystemDrives {
    fn list(&self) -> io::Result<Vec<Drive>> {
        platform::list_drives()
    }

    fn find(&self, name: &str) -> io::Result<Option<Drive>> {
        platform::find_drive(name)
    }

    fn write(&self, drive: &Drive, policy: &Policy, image: &Path, progress: &mut dyn FnMut(Phase, u64)) -> Result<Written, Error> {
        let mut target = Target::open(drive, policy)?;
        let written = target.write_image(image, &mut |done| progress(Phase::Writing, done))?;
        target.verify(&written, &mut |done| progress(Phase::Verifying, done))?;
        Ok(written)
    }
}

/// Serves the real drives on `input`/`output` until the input ends, then
/// lets any write still running finish.
pub fn serve<R: BufRead, W: Write>(input: R, output: W, options: Options) -> io::Result<()> {
    Server::new(SystemDrives, options).run(input, output)
}

/// A write that passed every check, waiting for `confirm_write`.
struct Pending {
    image: PathBuf,
    drive: Drive,
    sha256: Sha256Digest,
    len: u64,
    modified: Option<SystemTime>,
    created: Instant,
}

/// How a background write is getting on.
#[derive(Debug, Clone)]
struct JobState {
    phase: Phase,
    done: u64,
    /// `Some` once the write has finished, well or not.
    outcome: Option<Result<Value, String>>,
}

/// A write running (or finished) in the background.
struct Job {
    drive: String,
    image: PathBuf,
    total: u64,
    started: Instant,
    shared: Arc<(Mutex<JobState>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Job {
    fn state(&self) -> MutexGuard<'_, JobState> {
        self.shared.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn running(&self) -> bool {
        self.state().outcome.is_none()
    }
}

/// Sends `notifications/progress` for one request, if it asked for them.
struct Notifier<'a> {
    token: Option<Value>,
    notify: &'a mut dyn FnMut(Value),
    last: Option<Instant>,
}

impl Notifier<'_> {
    fn report(&mut self, progress: u64, total: u64, message: &str) {
        let Some(token) = &self.token else { return };
        let now = Instant::now();
        if progress < total && self.last.is_some_and(|last| now - last < NOTIFY_EVERY) {
            return;
        }
        self.last = Some(now);
        (self.notify)(json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": { "progressToken": token, "progress": progress, "total": total, "message": message },
        }));
    }
}

pub struct Server<D: Drives> {
    drives: Arc<D>,
    options: Options,
    pending: HashMap<String, Pending>,
    jobs: HashMap<String, Job>,
    issued: u64,
}

type ToolResult = Result<(String, Value), String>;
type RpcError = (i64, String);

impl<D: Drives> Server<D> {
    pub fn new(drives: D, options: Options) -> Self {
        Self { drives: Arc::new(drives), options, pending: HashMap::new(), jobs: HashMap::new(), issued: 0 }
    }

    pub fn run<R: BufRead, W: Write>(&mut self, input: R, mut output: W) -> io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let mut failed = None;
            let reply = {
                let mut notify = |message: Value| {
                    if let Err(e) = send(&mut output, &message) {
                        failed.get_or_insert(e);
                    }
                };
                self.handle_line(&line, &mut notify)
            };
            if let Some(e) = failed {
                return Err(e);
            }
            if let Some(reply) = reply {
                send(&mut output, &reply)?;
            }
        }
        self.finish_jobs();
        Ok(())
    }

    /// Waits for every background write to end. A drive must never be left
    /// half-written because the client went away.
    pub fn finish_jobs(&mut self) {
        for job in self.jobs.values_mut() {
            if let Some(thread) = job.thread.take() {
                if !thread.is_finished() {
                    eprintln!("rust-rufus mcp: finishing the write to {} before exiting", job.drive);
                }
                let _ = thread.join();
            }
        }
    }

    /// The reply to one message, if it needs one (notifications don't).
    /// `notify` sends messages ahead of the reply, such as progress.
    pub fn handle_line(&mut self, line: &str, notify: &mut dyn FnMut(Value)) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(e) => return Some(error_reply(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if !message.is_object() {
            return Some(error_reply(Value::Null, -32600, "expected one JSON-RPC message object"));
        }
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to us (we send no requests) or junk with an id.
            return match id {
                Some(id) if message.get("result").is_none() && message.get("error").is_none() => {
                    Some(error_reply(id, -32600, "missing method"))
                }
                _ => None,
            };
        };
        let id = id?; // a notification
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let token = params.pointer("/_meta/progressToken").cloned();
        let mut notifier = Notifier { token, notify, last: None };
        Some(match self.request(method, &params, &mut notifier) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, text)) => error_reply(id, code, &text),
        })
    }

    fn request(&mut self, method: &str, params: &Value, notifier: &mut Notifier<'_>) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str);
                let version = asked.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "rust-rufus", "title": "rust-rufus", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": self.instructions(),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "tools/call needs a tool name".to_string()))?;
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                self.call(name, &args, notifier)
            }
            other => Err((-32601, format!("unknown method {other}"))),
        }
    }

    fn instructions(&self) -> String {
        let mut text = String::from(
            "rust-rufus makes bootable USB drives from ISO and disk images. Use list_drives to see drives and whether each \
             may be written, inspect_image to see whether an image boots when written raw, and hash_file to check a download.",
        );
        if self.options.allow_writes {
            text.push_str(
                " Writing ERASES a drive and takes two steps: prepare_write checks everything and returns a summary and a \
                 one-time token; show the summary to the person and get their explicit go-ahead before calling \
                 confirm_write with the token and the drive name. confirm_write starts the write in the background; call \
                 write_status (with wait_seconds up to 30) until it reports done or failed, and tell the person the result.",
            );
        } else {
            text.push_str(" This server is read-only: the person has to restart it with --allow-writes before drives can be written.");
        }
        text
    }

    fn tools(&self) -> Vec<Value> {
        let read_only = json!({ "readOnlyHint": true, "openWorldHint": false });
        let no_args = json!({ "type": "object", "properties": {}, "additionalProperties": false });
        let path_arg = |what: &str| {
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": what } },
                "required": ["path"],
                "additionalProperties": false,
            })
        };
        let mut tools = vec![
            tool(
                "list_drives",
                "List drives",
                "Every drive on this computer with its size, type (usb, sd card, internal, virtual), model and \
                 partitions, and whether it may be written; if not, why (system disk, mounted, internal...).",
                no_args,
                read_only.clone(),
            ),
            tool(
                "inspect_image",
                "Inspect an image",
                "What an image file is (hybrid ISO, disc-only ISO, disk image), its ISO label, and whether writing it raw \
                 to a USB drive makes it bootable.",
                path_arg("Absolute path of the image file"),
                read_only.clone(),
            ),
            tool(
                "hash_file",
                "SHA-256 of a file",
                "The SHA-256 of a file, to compare with the checksum its publisher gives.",
                path_arg("Absolute path of the file"),
                read_only.clone(),
            ),
        ];
        if self.options.allow_writes {
            tools.push(tool(
                "prepare_write",
                "Prepare to write an image",
                "Checks that `image` can be written to `drive` (the image boots raw, its SHA-256 matches if given, the drive \
                 may be erased and is big enough) without changing anything. Returns what will be erased and a one-time \
                 token for confirm_write. Show the summary to the person before confirming.",
                json!({
                    "type": "object",
                    "properties": {
                        "image": { "type": "string", "description": "Absolute path of the image file" },
                        "drive": { "type": "string", "description": "Drive name as list_drives gives it, e.g. sdb" },
                        "sha256": { "type": "string", "description": "Expected SHA-256 of the image (64 hex digits)" },
                        "force": { "type": "boolean", "description": "Allow an image that won't boot when written raw" },
                    },
                    "required": ["image", "drive"],
                    "additionalProperties": false,
                }),
                read_only.clone(),
            ));
            tools.push(tool(
                "confirm_write",
                "Erase the drive and write",
                "ERASES the drive prepared by prepare_write, then writes the image and verifies it by reading it back, in \
                 the background. Needs the token from prepare_write and the same drive name. Only call this after the \
                 person has agreed. Returns a job_id for write_status.",
                json!({
                    "type": "object",
                    "properties": {
                        "token": { "type": "string", "description": "The token prepare_write returned" },
                        "drive": { "type": "string", "description": "The drive name again, as a second check" },
                    },
                    "required": ["token", "drive"],
                    "additionalProperties": false,
                }),
                json!({ "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }),
            ));
            tools.push(tool(
                "write_status",
                "Progress of a write",
                "How a write started by confirm_write is going: phase (writing, verifying), bytes done, percent, speed; \
                 and once it ends, done (with the verified SHA-256) or failed (with why). wait_seconds (0 to 30) waits \
                 for it to end before answering.",
                json!({
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "The job_id confirm_write returned" },
                        "wait_seconds": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT.as_secs(), "description": "Wait up to this long for the write to end" },
                    },
                    "required": ["job_id"],
                    "additionalProperties": false,
                }),
                read_only,
            ));
        }
        tools
    }

    fn call(&mut self, name: &str, args: &Value, notifier: &mut Notifier<'_>) -> Result<Value, RpcError> {
        let outcome = match name {
            "list_drives" => self.list_drives(),
            "inspect_image" => inspect_image(args),
            "hash_file" => hash_file(args, notifier),
            "prepare_write" | "confirm_write" | "write_status" if !self.options.allow_writes => {
                Err("writing is turned off: the person has to restart this server with --allow-writes".into())
            }
            "prepare_write" => self.prepare_write(args, notifier),
            "confirm_write" => self.confirm_write(args),
            "write_status" => self.write_status(args, notifier),
            other => return Err((-32602, format!("unknown tool {other}"))),
        };
        Ok(match outcome {
            Ok((text, data)) => json!({
                "content": [{ "type": "text", "text": format!("{text}\n\n{}", serde_json::to_string_pretty(&data).unwrap_or_default()) }],
                "structuredContent": data,
                "isError": false,
            }),
            Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
        })
    }

    fn policy(&self) -> Policy {
        Policy { allow_loop: self.options.allow_loop }
    }

    fn list_drives(&self) -> ToolResult {
        let drives = self.drives.list().map_err(|e| format!("couldn't list drives: {e}"))?;
        let policy = self.policy();
        let lines: Vec<String> = drives
            .iter()
            .map(|d| {
                let status = match self.busy_job(&d.name) {
                    Some(job_id) => format!("being written ({job_id})"),
                    None => d.check(&policy).map_or_else(|r| format!("refused: {r}"), |()| "can be written".into()),
                };
                format!("{} ({}, {}, {}): {status}", d.name, size(d.size_bytes), d.transport, d.label())
            })
            .collect();
        let text = if lines.is_empty() { "No drives found.".to_string() } else { lines.join("\n") };
        Ok((text, json!({ "drives": drives.iter().map(|d| drive_json(d, &policy)).collect::<Vec<_>>() })))
    }

    /// The job writing `drive` right now, if any.
    fn busy_job(&self, drive: &str) -> Option<&str> {
        self.jobs.iter().find(|(_, job)| job.drive == drive && job.running()).map(|(id, _)| id.as_str())
    }

    fn prepare_write(&mut self, args: &Value, notifier: &mut Notifier<'_>) -> ToolResult {
        let image = PathBuf::from(arg(args, "image")?);
        let drive_name = arg(args, "drive")?;
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        let policy = self.policy();

        let info = image::inspect(&image).map_err(|e| format!("can't read {}: {e}", image.display()))?;
        let kind = info.kind();
        if !kind.boots_when_written_raw() && !force {
            return Err(format!("{} is {}. {} Pass force: true to write it anyway.", image.display(), kind.describe(), kind.advice()));
        }
        let drive = self
            .drives
            .find(drive_name)
            .map_err(|e| format!("couldn't look up {drive_name}: {e}"))?
            .ok_or_else(|| format!("no drive named {drive_name}; call list_drives"))?;
        if let Some(job_id) = self.busy_job(&drive.name) {
            return Err(format!("{} is being written right now ({job_id}); wait for write_status to report the end", drive.name));
        }
        drive.check(&policy).map_err(|reason| format!("{} can't be written: {reason}", drive.name))?;
        write::fits(info.size, drive.size_bytes).map_err(|e| e.to_string())?;

        let (sha256, len, modified) = hash_with_stamp(&image, notifier)?;
        if let Some(expected) = args.get("sha256").and_then(Value::as_str) {
            let expected = hash::parse_hex(expected).ok_or("sha256 must be 64 hex digits")?;
            if expected != sha256 {
                return Err(format!(
                    "the image's SHA-256 is {}, not {}: the download is damaged or it's a different file. Nothing was written.",
                    to_hex(&sha256),
                    to_hex(&expected)
                ));
            }
        }

        let ttl = self.options.token_ttl;
        self.pending.retain(|_, p| p.created.elapsed() < ttl);
        let token = self.new_id("");
        let partitions = if drive.partitions.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = drive.partitions.iter().map(|p| format!("{} {}", p.name, size(p.size_bytes))).collect();
            format!(", partitions {}", list.join(", "))
        };
        let text = format!(
            "Ready. This will ERASE everything on {} ({}, {}{partitions}) and write {} ({}, {}). Show this to the person and \
             get their go-ahead, then call confirm_write with this token and drive \"{}\". The token works once, within {} seconds.",
            drive.path.display(),
            drive.label(),
            size(drive.size_bytes),
            image.display(),
            kind.describe(),
            size(len),
            drive.name,
            ttl.as_secs(),
        );
        let data = json!({
            "token": token,
            "expires_in_seconds": ttl.as_secs(),
            "drive": drive_json(&drive, &policy),
            "image": { "path": image, "size_bytes": len, "sha256": to_hex(&sha256), "kind": kind.describe() },
        });
        self.pending.insert(token, Pending { image, drive, sha256, len, modified, created: Instant::now() });
        Ok((text, data))
    }

    fn confirm_write(&mut self, args: &Value) -> ToolResult {
        let token = arg(args, "token")?;
        let drive_name = arg(args, "drive")?;
        // Single use: the token is gone whatever happens next.
        let pending = self.pending.remove(token).ok_or("unknown or already used token; call prepare_write again")?;
        if pending.created.elapsed() >= self.options.token_ttl {
            return Err(format!(
                "the token expired; call prepare_write again and confirm within {} seconds",
                self.options.token_ttl.as_secs()
            ));
        }
        let wanted = drive_name.strip_prefix("/dev/").unwrap_or(drive_name);
        if wanted != pending.drive.name {
            return Err(format!("the token was made for {}, not {drive_name}; nothing was written", pending.drive.name));
        }
        if let Some(job_id) = self.busy_job(&pending.drive.name) {
            return Err(format!("{} is being written right now ({job_id}); nothing new was started", pending.drive.name));
        }
        let meta = fs::metadata(&pending.image).map_err(|e| format!("can't read {}: {e}", pending.image.display()))?;
        if meta.len() != pending.len || meta.modified().ok() != pending.modified {
            return Err("the image changed since prepare_write; nothing was written. Call prepare_write again.".into());
        }

        let job_id = self.new_id("job-");
        let shared = Arc::new((Mutex::new(JobState { phase: Phase::Writing, done: 0, outcome: None }), Condvar::new()));
        let thread = {
            let (drives, shared, policy) = (Arc::clone(&self.drives), Arc::clone(&shared), self.policy());
            let Pending { drive, image, sha256, .. } = &pending;
            let (drive, image, expected) = (drive.clone(), image.clone(), *sha256);
            thread::spawn(move || {
                let (state, changed) = &*shared;
                let mut progress = |phase, done| {
                    let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
                    (state.phase, state.done) = (phase, done);
                };
                let outcome = match drives.write(&drive, &policy, &image, &mut progress) {
                    Ok(written) if written.sha256 != expected => Err(format!(
                        "the image changed while it was being written, so {} holds something other than what was \
                         prepared. Call prepare_write again.",
                        drive.name
                    )),
                    Ok(written) => Ok(json!({
                        "bytes": written.bytes,
                        "sha256": to_hex(&written.sha256),
                        "verified": true,
                        "partition_table_reread": written.reread.is_ok(),
                    })),
                    Err(e) => Err(e.to_string()),
                };
                state.lock().unwrap_or_else(PoisonError::into_inner).outcome = Some(outcome);
                changed.notify_all();
            })
        };
        self.jobs.insert(
            job_id.clone(),
            Job {
                drive: pending.drive.name.clone(),
                image: pending.image.clone(),
                total: pending.len,
                started: Instant::now(),
                shared,
                thread: Some(thread),
            },
        );
        let text = format!(
            "Started erasing {} and writing {}. Call write_status with job_id \"{job_id}\" (wait_seconds up to {}) until it \
             reports done or failed.",
            pending.drive.name,
            pending.image.display(),
            MAX_WAIT.as_secs()
        );
        Ok((text, json!({ "job_id": job_id, "drive": pending.drive.name, "status": "writing", "total_bytes": pending.len })))
    }

    fn write_status(&mut self, args: &Value, notifier: &mut Notifier<'_>) -> ToolResult {
        let job_id = arg(args, "job_id")?;
        let wait = Duration::from_secs(args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(0)).min(MAX_WAIT);
        let job = self.jobs.get(job_id).ok_or_else(|| format!("no write job {job_id}"))?;
        let deadline = Instant::now() + wait;
        // Progress over both phases, so it only ever goes up.
        let overall = |state: &JobState| match state.phase {
            Phase::Writing => state.done,
            Phase::Verifying => job.total + state.done,
        };
        let state = {
            let (lock, changed) = &*job.shared;
            let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                let message = format!("{} {}", phase_name(state.phase), job.drive);
                notifier.report(overall(&state), job.total * 2, &message);
                let now = Instant::now();
                if state.outcome.is_some() || now >= deadline {
                    break state.clone();
                }
                let pause = (deadline - now).min(Duration::from_secs(1));
                state = changed.wait_timeout(state, pause).unwrap_or_else(PoisonError::into_inner).0;
            }
        };

        let elapsed = job.started.elapsed();
        let mut data = json!({
            "job_id": job_id,
            "drive": job.drive,
            "image": job.image,
            "total_bytes": job.total,
            "elapsed_seconds": elapsed.as_secs(),
        });
        let text = match &state.outcome {
            None => {
                let percent = (state.done * 100).checked_div(job.total).unwrap_or(100);
                let rate = (overall(&state) as f64 / elapsed.as_secs_f64().max(0.001)) as u64;
                data["status"] = json!(phase_name(state.phase));
                data["done_bytes"] = json!(state.done);
                data["percent"] = json!(percent);
                data["bytes_per_second"] = json!(rate);
                format!(
                    "{} {}: {percent}% ({} of {}), {}/s. Call write_status again.",
                    capitalized(phase_name(state.phase)),
                    job.drive,
                    size(state.done),
                    size(job.total),
                    size(rate)
                )
            }
            Some(Ok(result)) => {
                data["status"] = json!("done");
                data["result"] = result.clone();
                format!(
                    "Done: wrote {} to {} and verified it by reading it back (SHA-256 {}). The drive can be removed.",
                    job.image.display(),
                    job.drive,
                    result["sha256"].as_str().unwrap_or("?")
                )
            }
            Some(Err(error)) => {
                data["status"] = json!("failed");
                data["error"] = json!(error);
                format!("Writing {} failed: {error}. The drive may be unusable until it is written again.", job.drive)
            }
        };
        Ok((text, data))
    }

    fn new_id(&mut self, prefix: &str) -> String {
        self.issued += 1;
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(self.issued);
        format!("{prefix}{:016x}{:04x}", hasher.finish(), self.issued)
    }
}

fn send<W: Write>(output: &mut W, message: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, message)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Writing => "writing",
        Phase::Verifying => "verifying",
    }
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn tool(name: &str, title: &str, description: &str, input_schema: Value, annotations: Value) -> Value {
    let mut annotations = annotations;
    annotations["title"] = json!(title);
    json!({ "name": name, "title": title, "description": description, "inputSchema": input_schema, "annotations": annotations })
}

fn inspect_image(args: &Value) -> ToolResult {
    let path = arg(args, "path")?;
    let info = image::inspect(Path::new(path)).map_err(|e| format!("can't read {path}: {e}"))?;
    let kind = info.kind();
    let text = format!("{path} is {}. {}", kind.describe(), kind.advice());
    let data = json!({
        "path": path,
        "size_bytes": info.size,
        "kind": kind.describe(),
        "boots_when_written_raw": kind.boots_when_written_raw(),
        "iso_label": info.iso.as_ref().map(|i| i.volume_id.clone()),
        "el_torito": info.iso.as_ref().is_some_and(|i| i.el_torito),
        "mbr": info.mbr,
        "gpt": info.gpt,
        "advice": kind.advice(),
    });
    Ok((text, data))
}

fn hash_file(args: &Value, notifier: &mut Notifier<'_>) -> ToolResult {
    let path = arg(args, "path")?;
    let (digest, len, _) = hash_with_stamp(Path::new(path), notifier)?;
    Ok((format!("SHA-256 of {path}: {}", to_hex(&digest)), json!({ "path": path, "size_bytes": len, "sha256": to_hex(&digest) })))
}

/// The file's SHA-256, its length, and when it was last modified.
fn hash_with_stamp(path: &Path, notifier: &mut Notifier<'_>) -> Result<(Sha256Digest, u64, Option<SystemTime>), String> {
    let fail = |e: io::Error| format!("can't read {}: {e}", path.display());
    let mut file = File::open(path).map_err(fail)?;
    let meta = file.metadata().map_err(fail)?;
    let message = format!("hashing {}", path.display());
    let digest = hash::hash_exact(&mut file, meta.len(), &mut |done| notifier.report(done, meta.len(), &message)).map_err(fail)?;
    Ok((digest, meta.len(), meta.modified().ok()))
}

fn drive_json(drive: &Drive, policy: &Policy) -> Value {
    let check = drive.check(policy);
    json!({
        "name": drive.name,
        "path": drive.path,
        "size_bytes": drive.size_bytes,
        "size": size(drive.size_bytes),
        "transport": drive.transport.to_string(),
        "model": drive.label(),
        "removable": drive.removable,
        "partitions": drive.partitions.iter().map(|p| json!({ "name": p.name, "size_bytes": p.size_bytes })).collect::<Vec<_>>(),
        "writable": check.is_ok(),
        "refusal": check.err().map(|r| r.to_string()),
    })
}

fn arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| format!("missing argument `{name}`"))
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
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

#[cfg(test)]
mod tests;
