use std::io::Cursor;

use rr_core::Transport;

use super::*;

/// Drives that exist only in memory. `write` records what it was asked to
/// do, reports half the image written, then waits for the gate to open.
struct FakeDrives {
    writes: Mutex<Vec<(String, PathBuf)>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    fail: bool,
}

impl FakeDrives {
    fn new(open: bool) -> Self {
        Self { writes: Mutex::new(Vec::new()), gate: Arc::new((Mutex::new(open), Condvar::new())), fail: false }
    }
}

fn open_gate(gate: &(Mutex<bool>, Condvar)) {
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}

fn drive(name: &str, transport: Transport) -> Drive {
    Drive {
        name: name.into(),
        path: PathBuf::from("/dev").join(name),
        size_bytes: 16 << 30,
        transport,
        removable: transport == Transport::Usb,
        read_only: false,
        vendor: Some("Acme".into()),
        model: Some("Stick".into()),
        partitions: Vec::new(),
        usage: Vec::new(),
    }
}

impl Drives for FakeDrives {
    fn list(&self) -> io::Result<Vec<Drive>> {
        Ok(vec![drive("nvme0n1", Transport::Internal), drive("sdb", Transport::Usb)])
    }

    fn find(&self, name: &str) -> io::Result<Option<Drive>> {
        Ok(self.list()?.into_iter().find(|d| d.name == name.strip_prefix("/dev/").unwrap_or(name)))
    }

    fn write(&self, drive: &Drive, _policy: &Policy, image: &Path, progress: &mut dyn FnMut(Phase, u64)) -> Result<Written, Error> {
        self.writes.lock().unwrap().push((drive.name.clone(), image.to_path_buf()));
        let bytes = fs::read(image)?;
        progress(Phase::Writing, bytes.len() as u64 / 2);
        let (open, opened) = &*self.gate;
        let _open = opened.wait_while(open.lock().unwrap(), |open| !*open).unwrap();
        if self.fail {
            return Err(Error::Io(io::Error::other("the stick was pulled out")));
        }
        progress(Phase::Writing, bytes.len() as u64);
        progress(Phase::Verifying, bytes.len() as u64);
        let sha256 = hash::hash_exact(&mut Cursor::new(&bytes), bytes.len() as u64, &mut |_| {})?;
        Ok(Written { bytes: bytes.len() as u64, sha256, reread: Ok(()) })
    }
}

struct Harness {
    server: Server<FakeDrives>,
    _dir: tempfile::TempDir,
    image: PathBuf,
    next_id: u64,
    notes: Vec<Value>,
}

impl Harness {
    fn with(drives: FakeDrives, options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("disk.img");
        let mut bytes = vec![0u8; 8192];
        bytes[510..512].copy_from_slice(&[0x55, 0xAA]);
        fs::write(&image, bytes).unwrap();
        Self { server: Server::new(drives, options), _dir: dir, image, next_id: 0, notes: Vec::new() }
    }

    fn new(options: Options) -> Self {
        Self::with(FakeDrives::new(true), options)
    }

    fn writable() -> Self {
        Self::new(Options { allow_writes: true, ..Options::default() })
    }

    /// A writable server whose writes hang half-way until the gate opens.
    fn held() -> Self {
        Self::with(FakeDrives::new(false), Options { allow_writes: true, ..Options::default() })
    }

    fn line(&mut self, line: &str) -> Option<Value> {
        let notes = &mut self.notes;
        self.server.handle_line(line, &mut |note| notes.push(note))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let line = json!({ "jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params }).to_string();
        let reply = self.line(&line).expect("a request gets a reply");
        assert_eq!(reply["id"], self.next_id);
        reply
    }

    /// A tool call's result: (is_error, structured content or the error text).
    fn call(&mut self, tool: &str, args: Value) -> (bool, Value) {
        self.call_with(tool, args, json!({}))
    }

    fn call_with(&mut self, tool: &str, args: Value, meta: Value) -> (bool, Value) {
        let reply = self.request("tools/call", json!({ "name": tool, "arguments": args, "_meta": meta }));
        let result = &reply["result"];
        let is_error = result["isError"].as_bool().unwrap();
        (is_error, if is_error { result["content"][0]["text"].clone() } else { result["structuredContent"].clone() })
    }

    fn image(&self) -> String {
        self.image.display().to_string()
    }

    fn prepare(&mut self) -> String {
        let image = self.image();
        let (is_error, data) = self.call("prepare_write", json!({ "image": image, "drive": "sdb" }));
        assert!(!is_error, "{data}");
        data["token"].as_str().unwrap().to_string()
    }

    /// prepare_write + confirm_write; returns the job id.
    fn start(&mut self) -> String {
        let token = self.prepare();
        let (is_error, data) = self.call("confirm_write", json!({ "token": token, "drive": "sdb" }));
        assert!(!is_error, "{data}");
        data["job_id"].as_str().unwrap().to_string()
    }

    fn status(&mut self, job: &str, wait: u64) -> Value {
        let (is_error, data) = self.call("write_status", json!({ "job_id": job, "wait_seconds": wait }));
        assert!(!is_error, "{data}");
        data
    }

    /// Waits for a held write's background thread to report half-way.
    fn halfway(&mut self, job: &str) -> Value {
        let began = Instant::now();
        loop {
            let status = self.status(job, 0);
            if status["done_bytes"] != 0 || began.elapsed() > Duration::from_secs(5) {
                return status;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn open_gate(&self) {
        open_gate(&self.server.drives.gate);
    }

    fn writes(&self) -> usize {
        self.server.drives.writes.lock().unwrap().len()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.open_gate();
        self.server.finish_jobs();
    }
}

fn tool_names(harness: &mut Harness) -> Vec<String> {
    let reply = harness.request("tools/list", json!({}));
    reply["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

#[test]
fn the_handshake_agrees_on_a_protocol_version() {
    let mut h = Harness::new(Options::default());
    let reply = h.request(
        "initialize",
        json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }),
    );
    assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(reply["result"]["serverInfo"]["name"], "rust-rufus");
    assert!(reply["result"]["capabilities"]["tools"].is_object());
    let reply = h.request("initialize", json!({ "protocolVersion": "1999-01-01" }));
    assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    assert_eq!(h.request("ping", json!({}))["result"], json!({}));
}

#[test]
fn notifications_get_no_reply_and_bad_messages_get_errors() {
    let mut h = Harness::new(Options::default());
    assert_eq!(h.line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
    assert_eq!(h.line(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#), None);
    assert_eq!(h.line("not json").unwrap()["error"]["code"], -32700);
    assert_eq!(h.line("[1]").unwrap()["error"]["code"], -32600);
    assert_eq!(h.request("resources/list", json!({}))["error"]["code"], -32601);
    assert_eq!(h.request("tools/call", json!({ "name": "format_c" }))["error"]["code"], -32602);
}

#[test]
fn read_only_unless_writes_are_allowed() {
    let mut h = Harness::new(Options::default());
    assert_eq!(tool_names(&mut h), ["list_drives", "inspect_image", "hash_file"]);
    let image = h.image();
    for (tool, args) in [
        ("prepare_write", json!({ "image": image, "drive": "sdb" })),
        ("confirm_write", json!({ "token": "x", "drive": "sdb" })),
        ("write_status", json!({ "job_id": "x" })),
    ] {
        let (is_error, text) = h.call(tool, args);
        assert!(is_error && text.as_str().unwrap().contains("--allow-writes"), "{tool}: {text}");
    }
    assert_eq!(h.writes(), 0);

    let mut h = Harness::writable();
    assert_eq!(tool_names(&mut h), ["list_drives", "inspect_image", "hash_file", "prepare_write", "confirm_write", "write_status"]);
}

#[test]
fn only_confirm_write_is_marked_destructive() {
    let mut h = Harness::writable();
    let reply = h.request("tools/list", json!({}));
    for tool in reply["result"]["tools"].as_array().unwrap() {
        let destructive = tool["annotations"]["destructiveHint"].as_bool().unwrap_or(false);
        assert_eq!(destructive, tool["name"] == "confirm_write", "{}", tool["name"]);
        assert_eq!(tool["annotations"]["readOnlyHint"], tool["name"] != "confirm_write");
    }
}

#[test]
fn list_drives_says_which_can_be_written_and_why_not() {
    let mut h = Harness::new(Options::default());
    let (_, data) = h.call("list_drives", json!({}));
    let drives = data["drives"].as_array().unwrap();
    assert_eq!(drives[0]["name"], "nvme0n1");
    assert_eq!(drives[0]["writable"], false);
    assert!(drives[0]["refusal"].as_str().unwrap().contains("internal disk"));
    assert_eq!(drives[1]["writable"], true);
    assert_eq!(drives[1]["refusal"], Value::Null);
}

#[test]
fn inspect_and_hash_an_image() {
    let mut h = Harness::new(Options::default());
    let image = h.image();
    let (_, data) = h.call("inspect_image", json!({ "path": image }));
    assert_eq!((data["boots_when_written_raw"].as_bool(), data["mbr"].as_bool()), (Some(true), Some(true)));
    let (_, data) = h.call("hash_file", json!({ "path": image }));
    assert_eq!(data["sha256"].as_str().unwrap().len(), 64);
    let (is_error, _) = h.call("hash_file", json!({ "path": "/nonexistent/x.iso" }));
    assert!(is_error);
}

#[test]
fn a_write_takes_prepare_then_confirm_and_runs_in_the_background() {
    let mut h = Harness::writable();
    let token = h.prepare();
    assert_eq!(h.writes(), 0, "prepare_write changes nothing");
    let (is_error, data) = h.call("confirm_write", json!({ "token": token, "drive": "/dev/sdb" }));
    assert!(!is_error, "{data}");
    assert_eq!(data["status"], "writing");
    let job = data["job_id"].as_str().unwrap().to_string();

    let status = h.status(&job, 5);
    assert_eq!(status["status"], "done", "{status}");
    assert_eq!(status["result"]["verified"], true);
    assert_eq!(status["result"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(h.server.drives.writes.lock().unwrap()[0], ("sdb".to_string(), h.image.clone()));
    // Asking again gives the same answer.
    assert_eq!(h.status(&job, 0)["status"], "done");
}

#[test]
fn confirm_write_returns_before_the_write_ends_and_status_shows_progress() {
    let mut h = Harness::held();
    let job = h.start();
    let status = h.halfway(&job);
    assert_eq!((status["status"].as_str(), status["percent"].as_u64()), (Some("writing"), Some(50)), "{status}");
    assert_eq!(status["done_bytes"], 4096);

    // Waiting gives up at the deadline while the write is still going.
    let started = Instant::now();
    assert_eq!(h.status(&job, 1)["status"], "writing");
    assert!(started.elapsed() >= Duration::from_secs(1));

    h.open_gate();
    assert_eq!(h.status(&job, 5)["status"], "done");
}

#[test]
fn write_status_sends_progress_when_asked() {
    let mut h = Harness::held();
    let job = h.start();
    h.halfway(&job);
    h.notes.clear();
    let (is_error, _) = h.call_with("write_status", json!({ "job_id": job, "wait_seconds": 1 }), json!({ "progressToken": "p1" }));
    assert!(!is_error);
    assert!(!h.notes.is_empty());
    let note = &h.notes[0];
    assert_eq!(note["method"], "notifications/progress");
    assert_eq!(note["params"]["progressToken"], "p1");
    assert_eq!((note["params"]["progress"].as_u64(), note["params"]["total"].as_u64()), (Some(4096), Some(16384)));

    h.notes.clear();
    h.call("write_status", json!({ "job_id": job }));
    assert!(h.notes.is_empty(), "no token, no notifications");
}

#[test]
fn hashing_sends_progress_when_asked() {
    let mut h = Harness::new(Options::default());
    let image = h.image();
    h.call_with("hash_file", json!({ "path": image }), json!({ "progressToken": 7 }));
    let last = h.notes.last().expect("a progress notification");
    assert_eq!((last["params"]["progressToken"].as_u64(), last["params"]["progress"].as_u64()), (Some(7), Some(8192)));
}

#[test]
fn a_drive_being_written_cannot_be_prepared_again() {
    let mut h = Harness::held();
    let job = h.start();
    let image = h.image();
    let (is_error, text) = h.call("prepare_write", json!({ "image": image, "drive": "sdb" }));
    assert!(is_error && text.as_str().unwrap().contains("being written"), "{text}");
    let (_, data) = h.call("list_drives", json!({}));
    assert_eq!(data["drives"][1]["name"], "sdb");
    h.open_gate();
    assert_eq!(h.status(&job, 5)["status"], "done");
    let (is_error, _) = h.call("prepare_write", json!({ "image": image, "drive": "sdb" }));
    assert!(!is_error, "free again once the write ended");
}

#[test]
fn a_failed_write_is_reported() {
    let mut drives = FakeDrives::new(true);
    drives.fail = true;
    let mut h = Harness::with(drives, Options { allow_writes: true, ..Options::default() });
    let job = h.start();
    let status = h.status(&job, 5);
    assert_eq!(status["status"], "failed");
    assert!(status["error"].as_str().unwrap().contains("pulled out"), "{status}");
}

#[test]
fn the_server_finishes_a_write_before_it_exits() {
    let mut h = Harness::held();
    let job = h.start();
    let gate = Arc::clone(&h.server.drives.gate);
    let opener = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        open_gate(&gate);
    });
    h.server.finish_jobs();
    assert!(!h.server.jobs[&job].running(), "finish_jobs returned before the write ended");
    opener.join().unwrap();
}

#[test]
fn a_token_works_once_and_only_for_its_drive() {
    let mut h = Harness::writable();
    let token = h.prepare();
    let (is_error, text) = h.call("confirm_write", json!({ "token": token, "drive": "nvme0n1" }));
    assert!(is_error && text.as_str().unwrap().contains("made for sdb"), "{text}");
    // The failed attempt used the token up.
    let (is_error, text) = h.call("confirm_write", json!({ "token": token, "drive": "sdb" }));
    assert!(is_error && text.as_str().unwrap().contains("already used"), "{text}");
    let (is_error, _) = h.call("confirm_write", json!({ "token": "made-up", "drive": "sdb" }));
    assert!(is_error);
    let (is_error, _) = h.call("write_status", json!({ "job_id": "job-made-up" }));
    assert!(is_error);
    assert_eq!(h.writes(), 0);
}

#[test]
fn an_expired_token_is_refused() {
    let mut h = Harness::new(Options { allow_writes: true, token_ttl: Duration::ZERO, ..Options::default() });
    let token = h.prepare();
    let (is_error, text) = h.call("confirm_write", json!({ "token": token, "drive": "sdb" }));
    assert!(is_error && text.as_str().unwrap().contains("expired"), "{text}");
    assert_eq!(h.writes(), 0);
}

#[test]
fn an_image_changed_after_prepare_is_not_written() {
    let mut h = Harness::writable();
    let token = h.prepare();
    let mut bytes = fs::read(&h.image).unwrap();
    bytes.extend([1, 2, 3]);
    fs::write(&h.image, bytes).unwrap();
    let (is_error, text) = h.call("confirm_write", json!({ "token": token, "drive": "sdb" }));
    assert!(is_error && text.as_str().unwrap().contains("changed"), "{text}");
    assert_eq!(h.writes(), 0);
}

#[test]
fn prepare_runs_every_check_the_cli_runs() {
    let mut h = Harness::writable();
    let image = h.image();
    let refused = |h: &mut Harness, args: Value, needle: &str| {
        let (is_error, text) = h.call("prepare_write", args);
        assert!(is_error && text.as_str().unwrap().contains(needle), "wanted {needle:?}, got {text}");
    };
    refused(&mut h, json!({ "image": image, "drive": "nvme0n1" }), "internal disk");
    refused(&mut h, json!({ "image": image, "drive": "sdz" }), "no drive named sdz");
    refused(&mut h, json!({ "image": image, "drive": "sdb", "sha256": "0".repeat(64) }), "SHA-256");
    refused(&mut h, json!({ "image": image, "drive": "sdb", "sha256": "abc" }), "64 hex digits");
    refused(&mut h, json!({ "drive": "sdb" }), "missing argument `image`");

    let junk = h._dir.path().join("junk.bin");
    fs::write(&junk, vec![7u8; 4096]).unwrap();
    let junk = junk.display().to_string();
    refused(&mut h, json!({ "image": junk, "drive": "sdb" }), "force");
    let (is_error, _) = h.call("prepare_write", json!({ "image": junk, "drive": "sdb", "force": true }));
    assert!(!is_error);
    assert_eq!(h.writes(), 0);
}

#[test]
fn serves_line_by_line_over_stdio() {
    let input = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let mut output = Vec::new();
    Server::new(FakeDrives::new(true), Options::default()).run(Cursor::new(input), &mut output).unwrap();
    let replies: Vec<Value> = String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.len(), 2, "the notification gets no reply");
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 3);
}
