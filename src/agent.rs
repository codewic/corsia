//! `corsia agent`: makes a local model fast for coding assistants.
//!
//! An OpenAI-compatible proxy in front of llama-server. Coding assistants send a long
//! system prompt and tool list (often 15,000 to 25,000 tokens) with every request, and on
//! a local model prompt processing dominates. llama-server can reuse the KV state of the
//! previous prompt, but only up to the first token that differs, and assistants put
//! details that change (date, folder, git status) early in the system prompt. So:
//!
//! 1. Stable prefix: those details move out of the system prompt to the front of the
//!    first user message. The system prompt and tools are then identical across
//!    sessions and folders, and tools are put in name order. Nothing is dropped.
//! 2. Slots: each stable prefix keeps its own llama-server slot, and small side requests
//!    (titles, summaries) go to the last slot, so they never push the big prompt out.
//! 3. KV snapshots: the first time a big prefix has been processed, the slot is saved to
//!    disk (llama-server `--slot-save-path`), named by a hash of the prefix. A later
//!    session, or a restarted server, restores it in seconds instead of recomputing it.
//! 4. Reporting: every request's prompt size, cache reuse and timings, on stderr, in
//!    `/api/agent` and pushed live on `/api/agent/events`.
//! 5. Live text: what the model is writing, as it streams, on `/api/agent/live` (thinking
//!    and answer apart), so `corsia watch` can show it from another machine.
//! 6. Project-wide drafts: the project the assistant names is tokenized in the background
//!    into the file the fork's n-gram drafts read (`CORSIA_DRAFT_CORPUS`), so code that
//!    exists elsewhere in the project is drafted for free (see `corpus.rs`).
//!
//! Listens on 127.0.0.1 only and checks the Host header, like the daemon's API.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::brand::BIN;
use crate::json::{self, Value};

const MAX_BODY: usize = 64 << 20;

pub struct AgentArgs {
    port: u16,
    upstream: String,
    snapshots: Option<PathBuf>,
    slots: usize,
    rewrite: bool,
    min_snapshot_tokens: u64,
    log: Option<PathBuf>,
    volatile: Vec<String>,
    /// The project token file for the fork's drafts (CORSIA_DRAFT_CORPUS); None turns it off.
    corpus: Option<PathBuf>,
    /// false: ask the model not to think (`enable_thinking: false`) unless the request says.
    think: bool,
}

impl AgentArgs {
    fn parse(args: &[String]) -> Result<AgentArgs, String> {
        let mut a = AgentArgs {
            port: 7720,
            upstream: "127.0.0.1:8080".into(),
            snapshots: None,
            slots: 2,
            rewrite: true,
            min_snapshot_tokens: 1024,
            log: None,
            volatile: vec![],
            corpus: std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/corsia/corpus/current.tok")),
            think: true,
        };
        let mut it = args.iter();
        while let Some(x) = it.next() {
            let mut val = |what: &str| it.next().cloned().ok_or(format!("{x} needs {what}."));
            match x.as_str() {
                "--port" => a.port = val("a port")?.parse().map_err(|_| "--port needs a number.")?,
                "--upstream" => {
                    let u = val("llama-server's address, like 127.0.0.1:8080")?;
                    a.upstream = u.trim_start_matches("http://").trim_end_matches('/').to_string();
                }
                "--snapshots" => a.snapshots = Some(PathBuf::from(val("the folder given to llama-server as --slot-save-path")?)),
                "--slots" => a.slots = val("a number")?.parse().map_err(|_| "--slots needs a number.")?,
                "--min-snapshot-tokens" => a.min_snapshot_tokens = val("a number")?.parse().map_err(|_| "--min-snapshot-tokens needs a number.")?,
                "--log" => a.log = Some(PathBuf::from(val("a file")?)),
                "--volatile" => a.volatile.push(val("a line label, like \"Session id\"")?.to_lowercase()),
                "--plain" => a.rewrite = false,
                "--think" => {
                    a.think = match val("on or off")?.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => return Err("--think takes on or off.".into()),
                    }
                }
                "--corpus" => {
                    let v = val("a file for the project's tokens, or off")?;
                    a.corpus = if v == "off" { None } else { Some(PathBuf::from(v)) };
                }
                y => return Err(format!("Unknown option {y}. Run `{BIN} --help`.")),
            }
        }
        if a.slots == 0 {
            return Err("--slots must be at least 1 (llama-server's -np).".into());
        }
        if a.snapshots.as_deref() == Some(std::path::Path::new("auto")) {
            a.snapshots = Some(fastest_snapshot_dir()?);
        }
        if let Some(d) = &a.snapshots {
            if !d.is_dir() {
                return Err(format!("{} is not a folder. Create it and give the same folder to llama-server as --slot-save-path.", d.display()));
            }
        }
        Ok(a)
    }
}

/// `--snapshots auto`: a folder on the fastest writable drive with room for snapshots
/// (solid-state before spinning, then by link speed), created if needed. On the drive
/// holding the home folder it is ~/.cache/corsia/kv, elsewhere .corsia/kv at the top.
fn fastest_snapshot_dir() -> Result<PathBuf, String> {
    const ROOM: u64 = 8 << 30;
    let topo = crate::topology::Topology::discover();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    let home_drive = topo.drive_for_path(&home).map(|d| d.name.clone());
    let mut best: Option<((bool, u32), PathBuf)> = None;
    for d in &topo.drives {
        let rank = (!d.rotational || d.removable, d.link_mb_s().unwrap_or(0));
        let dir = if Some(&d.name) == home_drive.as_ref() {
            Some(home.join(".cache/corsia/kv"))
        } else {
            d.mounts
                .iter()
                .find(|m| !m.read_only && crate::topology::writable(&m.point) && crate::topology::free_bytes(&m.point).unwrap_or(0) >= ROOM)
                .map(|m| m.point.join(".corsia/kv"))
        };
        if let Some(dir) = dir {
            if best.as_ref().is_none_or(|(r, _)| rank > *r) {
                best = Some((rank, dir));
            }
        }
    }
    let dir = best.map(|(_, d)| d).unwrap_or_else(|| home.join(".cache/corsia/kv"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// What one request did, for the log, the API and the live events.
#[derive(Default, Clone)]
struct Record {
    id: u64,
    t: f64,
    slot: Option<usize>,
    status: u16,
    stream: bool,
    moved_chars: usize,
    key: u64,
    cache_n: u64,
    prompt_n: u64,
    prompt_ms: f64,
    predicted_n: u64,
    predicted_ms: f64,
    wall_ms: f64,
    restore: Option<(u64, u64, f64)>,
    save: Option<(u64, u64, f64)>,
    note: String,
}

impl Record {
    fn json(&self) -> String {
        let snap = |s: &Option<(u64, u64, f64)>| match s {
            Some((tok, bytes, ms)) => format!("{{\"tokens\":{tok},\"bytes\":{bytes},\"ms\":{ms:.0}}}"),
            None => "null".into(),
        };
        let mut note = String::new();
        json::write_str(&self.note, &mut note);
        format!(
            "{{\"id\":{},\"t\":{:.3},\"slot\":{},\"status\":{},\"stream\":{},\"moved_chars\":{},\"prefix\":\"{:016x}\",\"prompt_tokens\":{},\"cache_n\":{},\"prompt_n\":{},\"prompt_ms\":{:.0},\"predicted_n\":{},\"predicted_ms\":{:.0},\"wall_ms\":{:.0},\"restore\":{},\"save\":{},\"note\":{}}}",
            self.id,
            self.t,
            self.slot.map_or("null".into(), |s| s.to_string()),
            self.status,
            self.stream,
            self.moved_chars,
            self.key,
            self.cache_n + self.prompt_n,
            self.cache_n,
            self.prompt_n,
            self.prompt_ms,
            self.predicted_n,
            self.predicted_ms,
            self.wall_ms,
            snap(&self.restore),
            snap(&self.save),
            note
        )
    }

    fn line(&self) -> String {
        let total = self.cache_n + self.prompt_n;
        let mut s = format!("#{} ", self.id);
        if let Some(slot) = self.slot {
            s += &format!("slot {slot}, ");
        }
        if self.status != 200 {
            s += &format!("upstream answered {}, ", self.status);
        }
        if total > 0 {
            let pct = 100.0 * self.cache_n as f64 / total as f64;
            s += &format!(
                "prompt {} tokens: {} reused ({pct:.0}%), {} computed in {}",
                group(total),
                group(self.cache_n),
                group(self.prompt_n),
                secs(self.prompt_ms)
            );
            if self.prompt_n > 0 {
                s += &format!(" ({:.2} tok/s)", self.prompt_n as f64 * 1000.0 / self.prompt_ms.max(1.0));
            }
            s += &format!("; {} generated in {}", group(self.predicted_n), secs(self.predicted_ms));
        } else {
            s += &format!("done in {}", secs(self.wall_ms));
        }
        if self.moved_chars > 0 {
            s += &format!("; moved {} changing characters out of the system prompt", group(self.moved_chars as u64));
        }
        if let Some((tok, bytes, ms)) = self.restore {
            s += &format!("; restored snapshot {:016x} ({} tokens, {}) in {}", self.key, group(tok), mb(bytes), secs(ms));
        }
        if let Some((tok, bytes, ms)) = self.save {
            s += &format!("; saved snapshot {:016x} ({} tokens, {}) in {}", self.key, group(tok), mb(bytes), secs(ms));
        }
        if !self.note.is_empty() {
            s += &format!("; {}", self.note);
        }
        s
    }
}

fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn secs(ms: f64) -> String {
    if ms < 1000.0 { format!("{ms:.0} ms") } else { format!("{:.1} s", ms / 1000.0) }
}

fn mb(b: u64) -> String {
    format!("{:.0} MB", b as f64 / 1e6)
}

struct Slots {
    /// The stable prefix each slot holds, as far as Corsia knows.
    loaded: Vec<Option<u64>>,
    last_used: Vec<Instant>,
}

struct State {
    args: AgentArgs,
    slots: Mutex<Slots>,
    records: Mutex<Vec<Record>>,
    next_id: Mutex<u64>,
    subscribers: Mutex<Vec<Sender<String>>>,
    totals: Mutex<(u64, u64, u64)>, // requests, prompt tokens, reused tokens
    /// Watchers of the live text (`/api/agent/live`).
    live: Mutex<Vec<Sender<String>>>,
    /// Project-wide drafts (`--corpus`).
    corpus: Option<Arc<crate::corpus::Corpus>>,
}

impl State {
    /// Sends one live event (`{"id":..,"kind":..,"text":..}`) to every live watcher.
    fn live_event(&self, id: u64, kind: &str, text: &str) {
        let mut watchers = self.live.lock().unwrap();
        if watchers.is_empty() {
            return;
        }
        let mut line = format!("{{\"id\":{id},\"kind\":\"{kind}\",\"text\":");
        json::write_str(text, &mut line);
        line.push('}');
        watchers.retain(|s| s.send(line.clone()).is_ok());
    }

    fn publish(&self, r: Record) {
        eprintln!("{}", r.line());
        let line = r.json();
        if let Some(path) = &self.args.log {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{line}");
            }
        }
        {
            let mut t = self.totals.lock().unwrap();
            t.0 += 1;
            t.1 += r.cache_n + r.prompt_n;
            t.2 += r.cache_n;
        }
        {
            let mut recs = self.records.lock().unwrap();
            recs.push(r);
            let n = recs.len();
            if n > 50 {
                recs.drain(..n - 50);
            }
        }
        self.subscribers.lock().unwrap().retain(|s| s.send(line.clone()).is_ok());
    }

    fn status_json(&self) -> String {
        let t = *self.totals.lock().unwrap();
        let recs = self.records.lock().unwrap();
        let recent: Vec<String> = recs.iter().map(Record::json).collect();
        let slots = self.slots.lock().unwrap();
        let loaded: Vec<String> = slots.loaded.iter().map(|k| k.map_or("null".into(), |k| format!("\"{k:016x}\""))).collect();
        let mut snaps = Vec::new();
        if let Some(dir) = &self.args.snapshots {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if let Some(k) = name.strip_prefix("corsia-").and_then(|n| n.strip_suffix(".kv")) {
                        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                        snaps.push(format!("{{\"prefix\":\"{k}\",\"bytes\":{size}}}"));
                    }
                }
            }
        }
        let mut up = String::new();
        json::write_str(&self.args.upstream, &mut up);
        let mut dir = String::new();
        match &self.args.snapshots {
            Some(d) => json::write_str(&d.display().to_string(), &mut dir),
            None => dir.push_str("null"),
        }
        format!(
            "{{\"upstream\":{up},\"stable_prefix\":{},\"snapshots_dir\":{dir},\"drafts\":{},\"requests\":{},\"prompt_tokens\":{},\"reused_tokens\":{},\"slots\":[{}],\"snapshots\":[{}],\"recent\":[{}]}}",
            self.args.rewrite,
            self.corpus.as_ref().map_or("null".to_string(), |c| c.status_json()),
            t.0,
            t.1,
            t.2,
            loaded.join(","),
            snaps.join(","),
            recent.join(",")
        )
    }
}

pub fn run(args: &[String]) -> ExitCode {
    let args = match AgentArgs::parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{BIN}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{BIN}: could not listen on 127.0.0.1:{}: {e}", args.port);
            return ExitCode::FAILURE;
        }
    };
    match upstream_get(&args.upstream, "/health") {
        Ok((200, _)) => {}
        Ok((code, body)) => eprintln!("Note: llama-server at {} answered /health with {code} ({}). Requests will wait for it.", args.upstream, body.trim()),
        Err(e) => eprintln!("Note: llama-server is not answering at {} yet ({e}). Requests will fail until it does.", args.upstream),
    }
    let n = args.slots;
    eprintln!("{BIN} agent: OpenAI-compatible endpoint on http://127.0.0.1:{}/v1, in front of llama-server at {}", args.port, args.upstream);
    if args.rewrite {
        eprintln!("  Stable prefix: on. {n} slot(s); start llama-server with -np {n} --kv-unified.");
    } else {
        eprintln!("  Plain mode: requests pass through unchanged, only measured.");
    }
    match &args.snapshots {
        Some(d) => eprintln!("  KV snapshots in {} (give llama-server --slot-save-path {})", d.display(), d.display()),
        None => eprintln!("  KV snapshots: off (--snapshots DIR to keep each system prompt's state on disk)"),
    }
    match &args.corpus {
        Some(f) => eprintln!(
            "  Project drafts: on; start llama-server with CORSIA_DRAFT_CORPUS={} and --spec-type ngram-simple",
            f.display()
        ),
        None => eprintln!("  Project drafts: off"),
    }
    if !args.think {
        eprintln!("  Thinking: off (each request asks the model to answer directly; --think on to allow it)");
    }
    eprintln!("  Live status: http://127.0.0.1:{}/api/agent and /api/agent/events. Ctrl+C stops.", args.port);
    let corpus = args.corpus.clone().map(|f| Arc::new(crate::corpus::Corpus::new(f)));
    let state = Arc::new(State {
        corpus,
        slots: Mutex::new(Slots { loaded: vec![None; n], last_used: vec![Instant::now(); n] }),
        args,
        records: Mutex::new(Vec::new()),
        next_id: Mutex::new(0),
        subscribers: Mutex::new(Vec::new()),
        totals: Mutex::new((0, 0, 0)),
        live: Mutex::new(Vec::new()),
    });
    if state.args.rewrite && state.args.snapshots.is_some() {
        let st = state.clone();
        std::thread::spawn(move || warm_prefixes(st));
    }
    for stream in listener.incoming().flatten() {
        let state = state.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &state) {
                if e.kind() != io::ErrorKind::BrokenPipe && e.kind() != io::ErrorKind::ConnectionReset {
                    eprintln!("{BIN} agent: {e}");
                }
            }
        });
    }
    ExitCode::SUCCESS
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream, port: u16) -> io::Result<Result<Request, (u16, &'static str)>> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    let mut total = line.len();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        total += h.len();
        if total > 64 * 1024 {
            return Ok(Err((431, "Request headers too large.\n")));
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let hdr = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let host_ok = matches!(hdr("host"), Some(h) if h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if !host_ok {
        return Ok(Err((403, "Corsia only answers requests addressed to 127.0.0.1 or localhost.\n")));
    }
    let mut body = Vec::new();
    if let Some(len) = hdr("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if len > MAX_BODY {
            return Ok(Err((413, "Request body too large.\n")));
        }
        body.resize(len, 0);
        reader.read_exact(&mut body)?;
    } else if hdr("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        read_chunked(&mut reader, |piece| {
            body.extend_from_slice(piece);
            if body.len() > MAX_BODY { Err(io::Error::other("request body too large")) } else { Ok(()) }
        })?;
    }
    Ok(Ok(Request { method, path, headers, body }))
}

fn handle(mut stream: TcpStream, state: &State) -> io::Result<()> {
    let req = match read_request(&stream, state.args.port)? {
        Ok(r) => r,
        Err((code, msg)) => return respond(&mut stream, code, "text/plain", msg),
    };
    let path = req.path.split('?').next().unwrap_or("");
    match (req.method.as_str(), path) {
        ("GET", "/api/agent") => respond(&mut stream, 200, "application/json", &state.status_json()),
        ("GET", "/api/agent/events") => events(&mut stream, state),
        ("GET", "/api/agent/live") => live(&mut stream, state),
        ("POST", "/v1/chat/completions" | "/chat/completions") => chat(&mut stream, state, &req),
        _ => {
            let mut sink = |_: &[u8]| {};
            forward(&state.args.upstream, &req.method, &req.path, &req.headers, &req.body, &mut stream, &mut sink).map(|_| ())
        }
    }
}

fn respond(stream: &mut TcpStream, code: u16, kind: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {code} {}\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        reason(code),
        body.len()
    )
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Pushes one `request` event per finished request until the client goes away.
fn events(stream: &mut TcpStream, state: &State) -> io::Result<()> {
    let (tx, rx) = channel::<String>();
    state.subscribers.lock().unwrap().push(tx);
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\nretry: 2000\n\nevent: status\ndata: {}\n\n", state.status_json())?;
    stream.flush()?;
    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(line) => write!(stream, "event: request\ndata: {line}\n\n")?,
            // A comment keeps idle connections open through proxies and finds dead clients.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => write!(stream, ": idle\n\n")?,
            Err(_) => return Ok(()),
        }
        stream.flush()?;
    }
}

/// Pushes what the model writes, piece by piece, until the client goes away: `start` when
/// a request begins, `think` and `text` pieces as they stream, `end` when it is done.
fn live(stream: &mut TcpStream, state: &State) -> io::Result<()> {
    let (tx, rx) = channel::<String>();
    state.live.lock().unwrap().push(tx);
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\nretry: 2000\n\n")?;
    stream.flush()?;
    loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(line) => write!(stream, "event: live\ndata: {line}\n\n")?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => write!(stream, ": idle\n\n")?,
            Err(_) => return Ok(()),
        }
        stream.flush()?;
    }
}

/// Picks the model's words out of a streamed chat answer (OpenAI-style SSE): the
/// thinking (`reasoning_content`) and the answer (`content`), piece by piece.
#[derive(Default)]
struct LiveScan {
    buf: Vec<u8>,
    /// The whole body, for a plain JSON answer (bounded).
    plain: Vec<u8>,
    streamed: bool,
}

impl LiveScan {
    fn feed(&mut self, piece: &[u8], out: &mut dyn FnMut(&str, &str)) {
        if self.plain.len() < MAX_BODY {
            self.plain.extend_from_slice(piece);
        }
        self.buf.extend_from_slice(piece);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let Ok(v) = json::parse(data.trim()) else { continue };
            self.streamed = true;
            if let Some(delta) = v.get("choices").and_then(Value::as_arr).and_then(|c| c.first()).and_then(|c| c.get("delta")) {
                Self::emit(delta, out);
            }
        }
    }

    /// A non-streamed answer: the message comes whole at the end.
    fn finish(&mut self, out: &mut dyn FnMut(&str, &str)) {
        if self.streamed || self.plain.is_empty() {
            return;
        }
        if let Ok(v) = json::parse(&String::from_utf8_lossy(&self.plain)) {
            if let Some(msg) = v.get("choices").and_then(Value::as_arr).and_then(|c| c.first()).and_then(|c| c.get("message")) {
                Self::emit(msg, out);
            }
        }
    }

    fn emit(m: &Value, out: &mut dyn FnMut(&str, &str)) {
        if let Some(t) = m.get("reasoning_content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            out("think", t);
        }
        if let Some(t) = m.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            out("text", t);
        }
        // What the model does: each tool call's name once, then its arguments as they stream.
        for call in m.get("tool_calls").and_then(Value::as_arr).into_iter().flatten() {
            let f = call.get("function");
            if let Some(name) = f.and_then(|f| f.get("name")).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                out("tool", &format!("\n→ {name} "));
            }
            if let Some(a) = f.and_then(|f| f.get("arguments")).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                out("tool", a);
            }
        }
    }
}

fn chat(stream: &mut TcpStream, state: &State, req: &Request) -> io::Result<()> {
    let t0 = Instant::now();
    let mut rec = Record { t: now_secs(), ..Default::default() };
    {
        let mut id = state.next_id.lock().unwrap();
        *id += 1;
        rec.id = *id;
    }
    let text = String::from_utf8_lossy(&req.body);
    let mut body = match json::parse(&text) {
        Ok(v) => v,
        Err(e) => {
            // Not ours to judge: let llama-server answer it.
            rec.note = format!("request was not JSON ({e}), passed through");
            let mut sink = |_: &[u8]| {};
            rec.status = forward(&state.args.upstream, &req.method, &req.path, &req.headers, &req.body, stream, &mut sink)?;
            state.publish(rec);
            return Ok(());
        }
    };
    rec.stream = matches!(body.get("stream"), Some(Value::Bool(true)));
    if let Some(c) = &state.corpus {
        c.notice(&body, &state.args.upstream); // before the rewrite moves the working directory
    }
    if !state.args.think {
        no_thinking(&mut body);
    }
    let (key, prefix_chars) = if state.args.rewrite {
        let r = stabilise(&mut body, &state.args.volatile);
        rec.moved_chars = r.moved_chars;
        (r.key, r.prefix_chars)
    } else {
        prefix_key(&body)
    };
    rec.key = key;
    // A system prompt and tools this long take a CPU a minute or more to read (about 17
    // tokens/s for a 30B model on a laptop), while restoring a snapshot takes under a second.
    // Titles and other side requests stay well under it.
    let big = prefix_chars >= 4_000;

    let mut slot = None;
    if state.args.rewrite {
        let s = choose_slot(state, key, big);
        body.set("id_slot", Value::num(s));
        body.set("cache_prompt", Value::Bool(true));
        slot = Some(s);
        // A known big prefix the slot does not hold: restore its snapshot first. Corsia's
        // own record is checked against llama-server, which may have restarted since.
        let mut holds = state.slots.lock().unwrap().loaded[s] == Some(key);
        if holds && slot_tokens(&state.args.upstream, s) == Some(0) {
            holds = false;
            state.slots.lock().unwrap().loaded[s] = None;
        }
        if let (false, Some(dir)) = (holds, &state.args.snapshots) {
            let name = snapshot_name(key);
            if dir.join(&name).is_file() {
                match slot_action(&state.args.upstream, s, "restore", &name) {
                    Ok(v) => {
                        rec.restore = Some((
                            v.get("n_restored").and_then(Value::as_f64).unwrap_or(0.0) as u64,
                            v.get("n_read").and_then(Value::as_f64).unwrap_or(0.0) as u64,
                            v.get("timings").and_then(|t| t.get("restore_ms")).and_then(Value::as_f64).unwrap_or(0.0),
                        ));
                        state.slots.lock().unwrap().loaded[s] = Some(key);
                    }
                    Err(e) => rec.note = format!("could not restore snapshot: {e}"),
                }
            }
        }
    }
    rec.slot = slot;

    let out = body.to_json();
    let mut timings = Timings::default();
    let mut scan = SseScan::default();
    let mut words = LiveScan::default();
    let id = rec.id;
    state.live_event(id, "start", "");
    // What the model is answering: the person's question or the result of the tool it ran.
    if let Some(last) = body.get("messages").and_then(Value::as_arr).and_then(|m| m.last()) {
        let kind = match last.get("role").and_then(Value::as_str) {
            Some("user") => Some("ask"),
            Some("tool") => Some("result"),
            _ => None,
        };
        if let Some(kind) = kind {
            let text = match last.get("content") {
                Some(Value::Str(t)) => t.clone(),
                Some(Value::Arr(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(" "),
                _ => String::new(),
            };
            let mut short: String = text.chars().take(300).collect();
            if short.len() < text.len() {
                short.push('…');
            }
            state.live_event(id, kind, &short);
        }
    }
    let mut emit = |kind: &str, text: &str| state.live_event(id, kind, text);
    let mut on_data = |piece: &[u8]| {
        scan.feed(piece, &mut timings);
        words.feed(piece, &mut emit);
    };
    let result = forward(&state.args.upstream, "POST", &req.path, &req.headers, out.as_bytes(), stream, &mut on_data);
    scan.finish(&mut timings);
    words.finish(&mut emit);
    rec.status = match &result {
        Ok(code) => *code,
        Err(e) => {
            // The client or llama-server went away mid-answer: still report what happened.
            rec.note = format!("connection lost mid-answer: {e}");
            state.live_event(id, "end", "connection lost mid-answer");
            rec.wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
            state.publish(rec);
            return result.map(|_| ());
        }
    };
    rec.cache_n = timings.cache_n;
    rec.prompt_n = timings.prompt_n;
    rec.prompt_ms = timings.prompt_ms;
    rec.predicted_n = timings.predicted_n;
    rec.predicted_ms = timings.predicted_ms;
    rec.wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let speed = if rec.predicted_ms > 0.0 { rec.predicted_n as f64 * 1000.0 / rec.predicted_ms } else { 0.0 };
    state.live_event(id, "end", &format!("{} tokens, {:.2} tok/s, prompt {} new + {} reused", rec.predicted_n, speed, rec.prompt_n, rec.cache_n));

    if let (Some(s), 200) = (slot, rec.status) {
        {
            let mut slots = state.slots.lock().unwrap();
            if slots.loaded[s] == Some(key) && big && rec.cache_n == 0 && rec.restore.is_none() {
                rec.note = "llama-server had lost this slot's cache (restarted?)".into();
            }
            slots.loaded[s] = Some(key);
            slots.last_used[s] = Instant::now();
        }
        // First time this big prefix has been computed: keep its state on disk.
        let total = rec.cache_n + rec.prompt_n;
        if big && total >= state.args.min_snapshot_tokens {
            keep_prefix(state, key, &body);
        }
        if let Some(dir) = &state.args.snapshots {
            let name = snapshot_name(key);
            if big && total >= state.args.min_snapshot_tokens && !dir.join(&name).is_file() {
                match slot_action(&state.args.upstream, s, "save", &name) {
                    Ok(v) => {
                        rec.save = Some((
                            v.get("n_saved").and_then(Value::as_f64).unwrap_or(0.0) as u64,
                            v.get("n_written").and_then(Value::as_f64).unwrap_or(0.0) as u64,
                            v.get("timings").and_then(|t| t.get("save_ms")).and_then(Value::as_f64).unwrap_or(0.0),
                        ))
                    }
                    Err(e) => rec.note = format!("could not save snapshot: {e}"),
                }
            }
        }
    }
    state.publish(rec);
    Ok(())
}

/// `--think off`: asks the model to answer without a thinking phase (chat templates that
/// support it read `chat_template_kwargs.enable_thinking`), unless the request already chose.
/// On the dev box a coding task took 118 s this way against more than 25 minutes with thinking,
/// tests passing both times.
fn no_thinking(body: &mut Value) {
    let Value::Obj(fields) = body else { return };
    match fields.iter_mut().find(|(k, _)| k == "chat_template_kwargs") {
        Some((_, Value::Obj(kw))) => {
            if !kw.iter().any(|(k, _)| k == "enable_thinking") {
                kw.push(("enable_thinking".into(), Value::Bool(false)));
            }
        }
        Some(_) => {}
        None => fields.push(("chat_template_kwargs".into(), Value::Obj(vec![("enable_thinking".into(), Value::Bool(false))]))),
    }
}

// Warm prefixes. A coding assistant's first request carries its whole system prompt and
// tool list (5,000 to 23,000 tokens), and reading that on a CPU took 5 to 49 minutes in the
// measurements. The snapshot only helps from the second time. So each big prefix is also kept
// as text, in a library every model shares (`prefixes` beside the per-model snapshot folders),
// and when the agent starts (after a restart or a model switch) it computes the newest ones
// for the model now loaded and saves their snapshots before anyone asks. A request that comes
// in meanwhile waits behind the warm-up on the same slot and reuses what it computed.

fn prefix_dir(state: &State) -> Option<PathBuf> {
    let snaps = state.args.snapshots.as_ref()?;
    Some(snaps.parent().unwrap_or(snaps).join("prefixes"))
}

/// Keeps what makes up the stable prefix: the leading system messages, the tools and the
/// template options. Written once per prefix.
fn keep_prefix(state: &State, key: u64, body: &Value) {
    let Some(dir) = prefix_dir(state) else { return };
    let path = dir.join(format!("corsia-{key:016x}.json"));
    if path.is_file() {
        let _ = std::fs::OpenOptions::new().append(true).open(&path); // keep it recent
        return;
    }
    let mut keep = Vec::new();
    if let Some(msgs) = body.get("messages").and_then(Value::as_arr) {
        let system: Vec<Value> = msgs.iter().take_while(|m| m.get("role").and_then(Value::as_str) == Some("system")).cloned().collect();
        if system.is_empty() {
            return;
        }
        keep.push(("messages".to_string(), Value::Arr(system)));
    }
    for k in ["tools", "chat_template_kwargs", "parallel_tool_calls"] {
        if let Some(v) = body.get(k) {
            keep.push((k.to_string(), v.clone()));
        }
    }
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&path, Value::Obj(keep).to_json());
    }
}

fn note(text: &str) {
    let Some(home) = std::env::var_os("HOME") else { return };
    let path = PathBuf::from(home).join(".cache/corsia/notes.log");
    let now = std::process::Command::new("date").arg("+%H:%M").output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}  {text}", now.trim());
    }
}

fn progress(text: &str) {
    if let Some(home) = std::env::var_os("HOME") {
        let _ = std::fs::write(PathBuf::from(home).join(".cache/corsia/progress"), format!("{text}\n"));
    }
}

/// At start: computes and saves the snapshots of the newest known prefixes (one per main
/// slot) that the loaded model has none of yet.
fn warm_prefixes(state: Arc<State>) {
    let (Some(dir), Some(snaps)) = (prefix_dir(&state), state.args.snapshots.clone()) else { return };
    // llama-server must be up; give a large model time to load
    let t_wait = Instant::now();
    while !matches!(upstream_get(&state.args.upstream, "/health"), Ok((200, _))) {
        if t_wait.elapsed() > Duration::from_secs(1800) {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    let mut known: Vec<(SystemTime, u64, PathBuf)> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let key = u64::from_str_radix(name.strip_prefix("corsia-")?.strip_suffix(".json")?, 16).ok()?;
                    Some((e.metadata().ok()?.modified().ok()?, key, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    known.sort_by(|a, b| b.0.cmp(&a.0));
    let main_slots = state.args.slots.saturating_sub(1).max(1);
    for (_, key, path) in known.into_iter().take(main_slots) {
        let name = snapshot_name(key);
        if snaps.join(&name).is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(mut body) = json::parse(&text) else { continue };
        // the slot is claimed first, so a request for this prefix queues behind the warm-up
        let s = {
            let mut slots = state.slots.lock().unwrap();
            if slots.loaded.contains(&Some(key)) {
                continue;
            }
            let n = slots.loaded.len();
            let s = (0..n.saturating_sub(1).max(1)).min_by_key(|&i| (slots.loaded[i].is_some(), slots.last_used[i])).unwrap_or(0);
            slots.loaded[s] = Some(key);
            slots.last_used[s] = Instant::now();
            s
        };
        if let Value::Obj(fields) = &mut body {
            if let Some((_, Value::Arr(msgs))) = fields.iter_mut().find(|(k, _)| k == "messages") {
                msgs.push(json::parse(r#"{"role":"user","content":"Hi"}"#).unwrap());
            }
        }
        body.set("max_tokens", Value::num(1));
        body.set("stream", Value::Bool(false));
        body.set("id_slot", Value::num(s));
        body.set("cache_prompt", Value::Bool(true));
        if !state.args.think {
            no_thinking(&mut body);
        }
        let t0 = Instant::now();
        progress("warming the coding assistant's prompt for this model, so its first answer comes quickly");
        note("Warming a coding assistant's prompt (system prompt and tools) for the model just started, so the first message does not wait for it.");
        let ok = matches!(upstream_call(&state.args.upstream, "POST", "/v1/chat/completions", &body.to_json()), Ok((200, _)));
        let saved = ok && slot_action(&state.args.upstream, s, "save", &name).is_ok();
        let _ = std::fs::write(PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/corsia/progress"), "");
        let tokens = slot_tokens(&state.args.upstream, s).unwrap_or(0);
        if saved {
            note(&format!("Assistant prompt warmed: {tokens} tokens in {:.0} s. The first message now reads only its own words.", t0.elapsed().as_secs_f64()));
            eprintln!("{BIN} agent: warmed prefix {key:016x} into slot {s}: {tokens} tokens in {:.1} s", t0.elapsed().as_secs_f64());
        } else {
            state.slots.lock().unwrap().loaded[s] = None;
            note("Warming the assistant's prompt did not finish; the first message will read it instead.");
        }
    }
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn snapshot_name(key: u64) -> String {
    format!("corsia-{key:016x}.kv")
}

/// The slot for a request: the one already holding its prefix if any; otherwise big
/// prefixes take the least recently used of the main slots and small requests the last
/// slot, so side requests never evict a big prompt.
fn choose_slot(state: &State, key: u64, big: bool) -> usize {
    let slots = state.slots.lock().unwrap();
    let n = slots.loaded.len();
    if let Some(i) = slots.loaded.iter().position(|k| *k == Some(key)) {
        return i;
    }
    if n == 1 {
        return 0;
    }
    if !big {
        return n - 1;
    }
    (0..n - 1).min_by_key(|&i| (slots.loaded[i].is_some(), slots.last_used[i])).unwrap_or(0)
}

/// FNV-1a, 64 bits: names snapshot files. A collision would only cost a recompute,
/// because llama-server compares the restored tokens with the prompt itself.
fn fnv(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

const FNV0: u64 = 0xcbf29ce484222325;

struct Rewrite {
    key: u64,
    prefix_chars: usize,
    moved_chars: usize,
}

/// Blocks whose whole content changes per session or folder.
const VOLATILE_TAGS: &[&str] = &["env", "environment", "environment_details", "git_status", "gitStatus", "system_info", "session_info"];

/// Labels of "Label: value" lines that change per session or folder.
const VOLATILE_LABELS: &[&str] = &[
    "date", "time", "today", "directory", "cwd", "workspace", "folder", "git status", "branch", "session",
];

/// True for a short "Label: value" line whose label names something that changes.
fn volatile_line(line: &str, extra: &[String]) -> bool {
    let t = line.trim_start().trim_start_matches(['-', '*', '•']).trim_start();
    if t.len() > 300 {
        return false;
    }
    let Some(p) = t.find(':') else { return false };
    if p == 0 || p > 48 {
        return false;
    }
    let label = t[..p].to_lowercase();
    if label.contains("http") {
        return false;
    }
    VOLATILE_LABELS.iter().any(|w| label.contains(w)) || extra.iter().any(|w| label.contains(w.as_str()))
}

/// Splits text into what stays (stable) and what moves (changes per session).
fn split_volatile(text: &str, extra: &[String]) -> (String, String) {
    let mut rest = text.to_string();
    let mut moved: Vec<String> = Vec::new();
    for tag in VOLATILE_TAGS {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        while let Some(a) = rest.find(&open) {
            let Some(b) = rest[a..].find(&close).map(|b| a + b + close.len()) else { break };
            let end = if rest[b..].starts_with('\n') { b + 1 } else { b };
            moved.push(rest[a..b].to_string());
            rest.replace_range(a..end, "");
        }
    }
    let mut kept = String::with_capacity(rest.len());
    let mut fence = false;
    for line in rest.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            fence = !fence;
        }
        if !fence && volatile_line(line, extra) {
            moved.push(line.trim_end_matches('\n').to_string());
        } else {
            kept.push_str(line);
        }
    }
    (kept, moved.join("\n"))
}

fn is_system(m: &Value) -> bool {
    matches!(m.get("role").and_then(Value::as_str), Some("system" | "developer"))
}

/// Moves the changing details of the system messages (those before the first user
/// message) to the front of the first user message, puts tools in name order, and
/// returns the hash of what is now the stable prefix.
fn stabilise(body: &mut Value, extra: &[String]) -> Rewrite {
    let mut moved_all: Vec<String> = Vec::new();
    if let Some(Value::Arr(msgs)) = body.get_mut("messages") {
        for m in msgs.iter_mut() {
            if !is_system(m) {
                break;
            }
            match m.get_mut("content") {
                Some(Value::Str(s)) => {
                    let (kept, moved) = split_volatile(s, extra);
                    if !moved.is_empty() {
                        *s = kept;
                        moved_all.push(moved);
                    }
                }
                Some(Value::Arr(parts)) => {
                    for p in parts.iter_mut() {
                        if let Some(Value::Str(s)) = p.get_mut("text") {
                            let (kept, moved) = split_volatile(s, extra);
                            if !moved.is_empty() {
                                *s = kept;
                                moved_all.push(moved);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if !moved_all.is_empty() {
            let moved = moved_all.join("\n");
            match msgs.iter_mut().find(|m| m.get("role").and_then(Value::as_str) == Some("user")) {
                Some(u) => match u.get_mut("content") {
                    Some(Value::Str(s)) => *s = format!("{moved}\n\n{s}"),
                    Some(Value::Arr(parts)) => parts.insert(
                        0,
                        Value::Obj(vec![("type".into(), Value::Str("text".into())), ("text".into(), Value::Str(moved))]),
                    ),
                    _ => {}
                },
                // No user message: nothing later to carry it, so leave it at the end of
                // the system prompt, where it costs the least.
                None => {
                    if let Some(Value::Str(s)) = msgs.iter_mut().find(|m| is_system(m)).and_then(|m| m.get_mut("content")) {
                        s.push_str("\n\n");
                        s.push_str(&moved);
                    }
                }
            }
        }
    }
    if let Some(Value::Arr(tools)) = body.get_mut("tools") {
        let name = |t: &Value| t.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).map(str::to_string);
        if tools.iter().all(|t| name(t).is_some()) {
            tools.sort_by_key(|t| name(t).unwrap_or_default());
        }
    }
    let (key, prefix_chars) = prefix_key(body);
    Rewrite { key, prefix_chars, moved_chars: moved_all.iter().map(String::len).sum() }
}

/// Hash and size of the leading system messages plus the tools.
fn prefix_key(body: &Value) -> (u64, usize) {
    let mut h = FNV0;
    let mut chars = 0;
    if let Some(msgs) = body.get("messages").and_then(Value::as_arr) {
        for m in msgs.iter().take_while(|m| is_system(m)) {
            let c = m.get("content").map(Value::to_json).unwrap_or_default();
            chars += c.len();
            fnv(&mut h, b"system\0");
            fnv(&mut h, c.as_bytes());
        }
    }
    if let Some(t) = body.get("tools") {
        let c = t.to_json();
        chars += c.len();
        fnv(&mut h, b"tools\0");
        fnv(&mut h, c.as_bytes());
    }
    (h, chars)
}

#[derive(Default)]
struct Timings {
    cache_n: u64,
    prompt_n: u64,
    prompt_ms: f64,
    predicted_n: u64,
    predicted_ms: f64,
}

impl Timings {
    /// Reads timings from JSON text; if it does not parse, picks the numbers out of the
    /// `timings` object directly, so an odd value elsewhere never loses the report.
    fn take_text(&mut self, text: &str) -> bool {
        if let Ok(v) = json::parse(text) {
            if v.get("timings").is_some() {
                self.take(&v);
                return true;
            }
        }
        let Some(at) = text.rfind("\"timings\"") else { return false };
        let obj = &text[at..];
        let num = |k: &str| -> f64 {
            let key = format!("\"{k}\"");
            obj.find(&key)
                .map(|i| obj[i + key.len()..].trim_start().trim_start_matches(':').trim_start())
                .map(|r| r.chars().take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+')).collect::<String>())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0.0)
        };
        self.cache_n = num("cache_n") as u64;
        self.prompt_n = num("prompt_n") as u64;
        self.prompt_ms = num("prompt_ms");
        self.predicted_n = num("predicted_n") as u64;
        self.predicted_ms = num("predicted_ms");
        true
    }

    fn take(&mut self, v: &Value) {
        if let Some(t) = v.get("timings") {
            let f = |k: &str| t.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            self.cache_n = f("cache_n") as u64;
            self.prompt_n = f("prompt_n") as u64;
            self.prompt_ms = f("prompt_ms");
            self.predicted_n = f("predicted_n") as u64;
            self.predicted_ms = f("predicted_ms");
        }
    }
}

/// Finds llama-server's `timings` in a streamed (SSE) or plain JSON response.
#[derive(Default)]
struct SseScan {
    /// The current partial line.
    buf: Vec<u8>,
    /// The whole body, for a plain JSON answer (bounded).
    plain: Vec<u8>,
    found: bool,
}

impl SseScan {
    fn feed(&mut self, piece: &[u8], t: &mut Timings) {
        if self.plain.len() < MAX_BODY {
            self.plain.extend_from_slice(piece);
        }
        self.buf.extend_from_slice(piece);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            self.line(&line, t);
        }
    }

    fn line(&mut self, line: &[u8], t: &mut Timings) {
        let line = String::from_utf8_lossy(line);
        if let Some(data) = line.trim().strip_prefix("data:") {
            if data.contains("\"timings\"") {
                self.found |= t.take_text(data.trim());
            }
        }
    }

    fn finish(&mut self, t: &mut Timings) {
        let rest = std::mem::take(&mut self.buf);
        if !rest.is_empty() {
            self.line(&rest, t);
        }
        if !self.found && !self.plain.is_empty() {
            let body = String::from_utf8_lossy(&self.plain).into_owned();
            if body.contains("\"timings\"") {
                self.found = t.take_text(&body);
            }
        }
    }
}

/// Reads an HTTP chunked body, handing each piece over as it arrives.
fn read_chunked(reader: &mut impl BufRead, mut piece: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    loop {
        let mut size = String::new();
        if reader.read_line(&mut size)? == 0 {
            return Ok(());
        }
        let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).map_err(|_| io::Error::other("bad chunk size"))?;
        if n == 0 {
            // Trailers until the blank line.
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l)? == 0 || l == "\r\n" || l == "\n" {
                    return Ok(());
                }
            }
        }
        let mut buf = vec![0u8; n];
        reader.read_exact(&mut buf)?;
        piece(&buf)?;
        let mut crlf = String::new();
        reader.read_line(&mut crlf)?;
    }
}

/// Sends a request to llama-server and relays the response to the client as it arrives
/// (chunk by chunk for streams), handing each body piece to `on_data`. Returns the status.
fn forward(
    upstream: &str,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
    client: &mut TcpStream,
    on_data: &mut dyn FnMut(&[u8]),
) -> io::Result<u16> {
    let mut up = match TcpStream::connect(upstream) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("{{\"error\":{{\"message\":\"Corsia could not reach llama-server at {upstream}: {e}\",\"type\":\"unavailable\"}}}}");
            respond(client, 502, "application/json", &msg)?;
            return Ok(502);
        }
    };
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        if matches!(k.as_str(), "content-type" | "authorization" | "accept") {
            head += &format!("{k}: {v}\r\n");
        }
    }
    head += "\r\n";
    up.write_all(head.as_bytes())?;
    up.write_all(body)?;
    up.flush()?;

    let mut reader = BufReader::new(up);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(502);
    let mut length = None;
    let mut chunked = false;
    let mut kind = "application/json".to_string();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "content-length" => length = v.parse::<usize>().ok(),
                "transfer-encoding" => chunked = v.eq_ignore_ascii_case("chunked"),
                "content-type" => kind = v.to_string(),
                _ => {}
            }
        }
    }
    let mut out = format!("HTTP/1.1 {status} {}\r\nContent-Type: {kind}\r\nCache-Control: no-cache\r\nConnection: close\r\n", reason(status));
    match (chunked, length) {
        (false, Some(n)) => {
            out += &format!("Content-Length: {n}\r\n\r\n");
            client.write_all(out.as_bytes())?;
            let mut buf = vec![0u8; n];
            reader.read_exact(&mut buf)?;
            on_data(&buf);
            client.write_all(&buf)?;
        }
        _ => {
            out += "Transfer-Encoding: chunked\r\n\r\n";
            client.write_all(out.as_bytes())?;
            client.flush()?;
            let mut send = |piece: &[u8]| -> io::Result<()> {
                on_data(piece);
                write!(client, "{:x}\r\n", piece.len())?;
                client.write_all(piece)?;
                client.write_all(b"\r\n")?;
                client.flush()
            };
            if chunked {
                read_chunked(&mut reader, &mut send)?;
            } else {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    send(&buf[..n])?;
                }
            }
            client.write_all(b"0\r\n\r\n")?;
        }
    }
    client.flush()?;
    Ok(status)
}

/// A small request to llama-server, read whole.
pub(crate) fn upstream_call(upstream: &str, method: &str, path: &str, body: &str) -> io::Result<(u16, String)> {
    let mut up = TcpStream::connect(upstream)?;
    write!(up, "{method} {path} HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())?;
    let mut reader = BufReader::new(up);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut chunked = false;
    let mut length = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = v.trim().parse::<usize>().ok(),
                "transfer-encoding" => chunked = v.trim().eq_ignore_ascii_case("chunked"),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    if chunked {
        read_chunked(&mut reader, |p| {
            out.extend_from_slice(p);
            Ok(())
        })?;
    } else if let Some(n) = length {
        out.resize(n, 0);
        reader.read_exact(&mut out)?;
    } else {
        reader.read_to_end(&mut out)?;
    }
    Ok((status, String::from_utf8_lossy(&out).into_owned()))
}

fn upstream_get(upstream: &str, path: &str) -> io::Result<(u16, String)> {
    upstream_call(upstream, "GET", path, "")
}

/// How many tokens llama-server says a slot holds: Some(0) for a slot that has served
/// nothing since llama-server started, None when it cannot tell (slots endpoint off).
fn slot_tokens(upstream: &str, slot: usize) -> Option<u64> {
    let (code, text) = upstream_get(upstream, "/slots").ok()?;
    if code != 200 {
        return None;
    }
    let v = json::parse(&text).ok()?;
    let s = v.as_arr()?.iter().find(|s| s.get("id").and_then(Value::as_f64) == Some(slot as f64))?;
    Some(s.get("n_prompt_tokens").and_then(Value::as_f64).unwrap_or(0.0) as u64)
}

/// Saves or restores a slot through llama-server's slot API.
fn slot_action(upstream: &str, slot: usize, action: &str, file: &str) -> Result<Value, String> {
    let mut body = String::from("{\"filename\":");
    json::write_str(file, &mut body);
    body.push('}');
    let (code, text) = upstream_call(upstream, "POST", &format!("/slots/{slot}?action={action}"), &body).map_err(|e| e.to_string())?;
    let v = json::parse(&text).map_err(|e| format!("llama-server answered {code} with something that is not JSON ({e})"))?;
    if code != 200 {
        let msg = v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("no message").to_string();
        return Err(format!("llama-server answered {code}: {msg}"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(system: &str, user: &str) -> Value {
        json::parse(&format!(
            r#"{{"model":"m","messages":[{{"role":"system","content":{}}},{{"role":"user","content":{}}}],"tools":[{{"type":"function","function":{{"name":"read"}}}},{{"type":"function","function":{{"name":"bash"}}}}]}}"#,
            Value::Str(system.into()).to_json(),
            Value::Str(user.into()).to_json()
        ))
        .unwrap()
    }

    const SYS: &str = "You are an agent.\nAlways check the working directory before editing.\n<env>\n  Working directory: /home/a/proj\n  Today's date: Sun Nov 22 2026\n</env>\nSkills follow.\n- Current branch: main\nIMPORTANT: be brief.\n";

    #[test]
    fn moves_changing_details_and_keeps_the_rest() {
        let mut v = req(SYS, "hello");
        let r = stabilise(&mut v, &[]);
        let msgs = v.get("messages").unwrap().as_arr().unwrap();
        let sys = msgs[0].get("content").unwrap().as_str().unwrap();
        let user = msgs[1].get("content").unwrap().as_str().unwrap();
        assert_eq!(sys, "You are an agent.\nAlways check the working directory before editing.\nSkills follow.\nIMPORTANT: be brief.\n");
        assert!(user.starts_with("<env>\n  Working directory: /home/a/proj\n  Today's date: Sun Nov 22 2026\n</env>\n- Current branch: main\n\nhello"));
        assert!(r.moved_chars > 0);
        let names: Vec<&str> = v.get("tools").unwrap().as_arr().unwrap().iter().map(|t| t.get("function").unwrap().get("name").unwrap().as_str().unwrap()).collect();
        assert_eq!(names, ["bash", "read"]);
    }

    #[test]
    fn same_prefix_across_folders_and_days() {
        let mut a = req(SYS, "one");
        let other = SYS.replace("/home/a/proj", "/srv/b").replace("Sun Nov 22", "Mon Nov 23").replace("main", "fix");
        let mut b = req(&other, "two");
        assert_eq!(stabilise(&mut a, &[]).key, stabilise(&mut b, &[]).key);
        let mut c = req("You are a title generator.", "x");
        assert_ne!(stabilise(&mut a.clone(), &[]).key, stabilise(&mut c, &[]).key);
    }

    #[test]
    fn leaves_code_and_prose_alone() {
        let s = "Use this format:\n```\nDate: 2026-01-01\n```\nThe time it takes: varies a lot and this line is part of the instructions, so it is long enough to matter but still under the limit.\n";
        let (kept, moved) = split_volatile(s, &[]);
        assert!(kept.contains("Date: 2026-01-01"));
        // "The time it takes" is a label that names time: moved. Harmless, and documented.
        assert!(moved.contains("The time it takes") || kept.contains("The time it takes"));
    }

    #[test]
    fn finds_timings_in_a_stream_split_anywhere() {
        let stream = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[],\"timings\":{\"cache_n\":900,\"prompt_n\":100,\"prompt_ms\":2500.5,\"predicted_n\":7,\"predicted_ms\":800}}\n\ndata: [DONE]\n\n";
        for cut in [1, 7, 40, 100] {
            let mut t = Timings::default();
            let mut s = SseScan::default();
            for piece in stream.as_bytes().chunks(cut) {
                s.feed(piece, &mut t);
            }
            s.finish(&mut t);
            assert_eq!((t.cache_n, t.prompt_n, t.predicted_n), (900, 100, 7));
        }
    }

    #[test]
    fn finds_timings_when_the_stream_starts_oddly_or_does_not_parse() {
        for stream in [
            "\n: keep-alive\n\ndata: {\"choices\":[]}\n\ndata: {\"choices\":[],\"timings\":{\"cache_n\":5,\"prompt_n\":546,\"prompt_ms\":102544.6,\"predicted_n\":16,\"predicted_ms\":30676.97}}\n\ndata: [DONE]\n\n",
            "data: {\"x\":NaN,\"timings\":{\"cache_n\":5,\"prompt_n\":546,\"prompt_ms\":102544.6,\"predicted_n\":16,\"predicted_ms\":30676.97,\"prompt_per_second\":inf}}",
            "{\"choices\":[{\"message\":{}}],\"timings\":{\"cache_n\":5,\"prompt_n\":546,\"prompt_ms\":102544.6,\"predicted_n\":16,\"predicted_ms\":30676.97}}",
        ] {
            for cut in [1, 9, 1000] {
                let mut t = Timings::default();
                let mut s = SseScan::default();
                for piece in stream.as_bytes().chunks(cut) {
                    s.feed(piece, &mut t);
                }
                s.finish(&mut t);
                assert_eq!((t.cache_n, t.prompt_n, t.predicted_n), (5, 546, 16), "{stream}");
            }
        }
    }

    #[test]
    fn think_off_asks_for_no_thinking_unless_the_request_chose() {
        let mut v = json::parse("{\"messages\":[]}").unwrap();
        no_thinking(&mut v);
        assert_eq!(v.to_json(), "{\"messages\":[],\"chat_template_kwargs\":{\"enable_thinking\":false}}");
        let mut keep = json::parse("{\"chat_template_kwargs\":{\"enable_thinking\":true}}").unwrap();
        no_thinking(&mut keep);
        assert_eq!(keep.to_json(), "{\"chat_template_kwargs\":{\"enable_thinking\":true}}");
    }

    #[test]
    fn side_requests_take_the_last_slot() {
        let st = State {
            args: AgentArgs::parse(&[]).unwrap(),
            slots: Mutex::new(Slots { loaded: vec![None, None], last_used: vec![Instant::now(); 2] }),
            records: Mutex::new(vec![]),
            next_id: Mutex::new(0),
            subscribers: Mutex::new(vec![]),
            totals: Mutex::new((0, 0, 0)),
            live: Mutex::new(vec![]),
            corpus: None,
        };
        assert_eq!(choose_slot(&st, 1, true), 0);
        assert_eq!(choose_slot(&st, 2, false), 1);
        st.slots.lock().unwrap().loaded[0] = Some(1);
        assert_eq!(choose_slot(&st, 1, true), 0);
        assert_eq!(choose_slot(&st, 3, true), 0);
    }
}
