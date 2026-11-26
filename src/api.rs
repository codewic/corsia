//! The daemon's local API, for the management UI and scripts.
//!
//! - `GET /api/status`: everything as JSON
//! - `GET /api/events`: Server-Sent Events, a `status` event every second with per-drive
//!   MB/s since the last one and the latest notes
//! - `GET /api/notes`: the latest notes as JSON
//! - `POST /api/notes`: adds a note (the body, one line, up to 500 characters), for whoever
//!   sets the machine up to say what they are doing; `corsia watch` shows them. The request
//!   must carry an `X-Corsia` header, which a web page cannot add without asking first, so a
//!   page open in a browser cannot write notes.
//! - Every `status` event also carries `progress`: the job running now and how far along it
//!   is, read from `~/.cache/corsia/progress` (see `progress_json`), or null.
//!
//! Listens on 127.0.0.1 only and checks the Host header, so a web page elsewhere cannot
//! reach it through DNS rebinding.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::Engine;

// as many as a watcher can show when it gives the notes its whole body
const NOTES_SHOWN: usize = 40;

fn notes_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/corsia/notes.log"))
}

/// The latest notes, oldest first, as a JSON array of strings.
fn notes_json() -> String {
    let text = notes_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(NOTES_SHOWN);
    let items: Vec<String> = lines[start..].iter().map(|l| crate::js(l)).collect();
    format!("[{}]", items.join(","))
}

/// What is running right now, as one line in `~/.cache/corsia/progress`: optionally a
/// percentage first ("42% merging the model"), then the text. Whoever runs a long job
/// rewrites it as the job moves and empties it at the end; a line nobody has touched for
/// 15 minutes is dropped. JSON: {"text":..,"pct":..|null,"age_s":..} or null.
fn progress_json() -> String {
    let Some(path) = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/corsia/progress")) else {
        return "null".into();
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return "null".into();
    };
    let age = meta.modified().ok().and_then(|m| m.elapsed().ok()).map_or(0, |d| d.as_secs());
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let line: String = text.lines().next().unwrap_or("").trim().chars().filter(|c| !c.is_control()).take(300).collect();
    if line.is_empty() || age > 15 * 60 {
        return "null".into();
    }
    let (pct, rest) = match line.split_once(' ') {
        Some((head, rest)) if head.ends_with('%') => match head.trim_end_matches('%').parse::<f64>() {
            Ok(p) => (format!("{}", p.clamp(0.0, 100.0)), rest.trim()),
            Err(_) => ("null".into(), line.as_str()),
        },
        _ => ("null".into(), line.as_str()),
    };
    format!("{{\"text\":{},\"pct\":{pct},\"age_s\":{age}}}", crate::js(rest))
}

/// Appends a note with the local time in front; keeps the file to its last 200 lines.
fn add_note(body: &str) -> io::Result<()> {
    let path = notes_path().ok_or_else(|| io::Error::other("no home folder"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let line: String = body.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).take(500).collect();
    if line.is_empty() {
        return Ok(());
    }
    let now = std::process::Command::new("date").arg("+%H:%M").output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).unwrap_or_default();
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    text.push_str(&format!("{}  {line}\n", now.trim()));
    let lines: Vec<&str> = text.lines().collect();
    let keep = lines[lines.len().saturating_sub(200)..].join("\n") + "\n";
    std::fs::write(&path, keep)
}

pub fn start(engine: Arc<Engine>, port: u16) -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let engine = engine.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &engine, port);
            });
        }
    });
    Ok(port)
}

fn handle(mut stream: TcpStream, engine: &Engine, port: u16) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut host_ok = false;
    let mut marked = false;
    let mut length = 0usize;
    let mut total = line.len();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        total += h.len();
        if total > 16 * 1024 {
            return respond(&mut stream, 431, "text/plain", "Request headers too large.\n");
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("host") {
                let v = v.trim();
                host_ok = v == format!("127.0.0.1:{port}") || v == format!("localhost:{port}");
            } else if k.eq_ignore_ascii_case("x-corsia") {
                marked = true;
            } else if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if !host_ok {
        return respond(&mut stream, 403, "text/plain", "Corsia only answers requests addressed to 127.0.0.1 or localhost.\n");
    }
    if method == "POST" && (path == "/api/notes" || path == "/api/models/use") {
        if !marked {
            return respond(&mut stream, 403, "text/plain", "This needs an X-Corsia header.\n");
        }
        let mut body = vec![0u8; length.min(4096)];
        std::io::Read::read_exact(&mut reader, &mut body)?;
        let body = String::from_utf8_lossy(&body);
        if path == "/api/notes" {
            add_note(&body)?;
            return respond(&mut stream, 200, "application/json", &notes_json());
        }
        return match switch_model(body.trim()) {
            Ok(name) => respond(&mut stream, 202, "application/json", &format!("{{\"switching\":{}}}", crate::js(&name))),
            Err(why) => respond(&mut stream, 400, "application/json", &format!("{{\"error\":{}}}", crate::js(&why))),
        };
    }
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", "Only GET is supported (and POST to /api/notes and /api/models/use).\n");
    }
    match path.split('?').next().unwrap_or("") {
        "/api/status" => respond(&mut stream, 200, "application/json", &engine.status_json()),
        "/api/notes" => respond(&mut stream, 200, "application/json", &notes_json()),
        "/api/events" => events(&mut stream, engine),
        "/api/models" => respond(&mut stream, 200, "application/json", &models_json(engine)),
        "/api/agent/events" | "/api/agent/live" | "/api/agent" => agent_proxy(&mut stream, path),
        "/" | "/index.html" => respond(&mut stream, 200, "text/html", UI_PAGE),
        "/ui/geist.woff2" => respond_bytes(&mut stream, "font/woff2", UI_FONT),
        _ => respond(&mut stream, 404, "text/plain", "Not found. The management page is at /, the API at /api/status and /api/events.\n"),
    }
}

/// The management page and its font, built into the binary so the daemon serves them with
/// nothing else installed (`ui/README.md`).
const UI_PAGE: &str = include_str!("../ui/index.html");
const UI_FONT: &[u8] = include_bytes!("../ui/geist.woff2");

fn respond_bytes(stream: &mut TcpStream, kind: &str, body: &[u8]) -> io::Result<()> {
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: max-age=86400\r\nConnection: close\r\n\r\n", body.len())?;
    stream.write_all(body)
}

/// The folder models are picked from: CORSIA_MODELS_DIR, else ~/models, else ~/models/gguf
/// (the same choice as local-agent.sh).
fn models_dir() -> std::path::PathBuf {
    if let Some(d) = std::env::var_os("CORSIA_MODELS_DIR") {
        return d.into();
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let d = home.join("models");
    if d.is_dir() { d } else { home.join("models/gguf") }
}

/// One file per model, as `local-agent.sh models` lists them: split parts, tuned -cpu.gguf
/// copies and unfinished copies left out. Sorted by name.
fn model_files() -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = std::fs::read_dir(models_dir())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let split = name.contains("-of-0000") && name.contains("-0000");
                    if !name.ends_with(".gguf") || name.ends_with("-cpu.gguf") || split {
                        return None;
                    }
                    Some((name, e.metadata().ok()?.len()))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn meminfo_mib(key: &str) -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| t.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .map_or(0, |kib| kib / 1024)
}

/// {"dir":..,"chosen":..,"serving":..,"mem_total_mib":..,"models":[{"name","bytes","fits"}]}
fn models_json(engine: &Engine) -> String {
    let total = meminfo_mib("MemTotal:");
    let chosen = std::env::var_os("HOME")
        .and_then(|h| std::fs::read_to_string(std::path::PathBuf::from(h).join(".config/corsia/model")).ok())
        .and_then(|p| std::path::Path::new(p.trim()).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let serving = engine.path.file_name().map(|n| n.to_string_lossy().replace("-cpu.gguf", ".gguf")).unwrap_or_default();
    let items: Vec<String> = model_files()
        .iter()
        .map(|(name, bytes)| {
            // what local-agent.sh asks for: the file, the KV cache and working room, within 90% of memory
            let fits = (bytes >> 20) + 3 * 1024 <= total * 9 / 10;
            format!("{{\"name\":{},\"bytes\":{bytes},\"fits\":{fits}}}", crate::js(name))
        })
        .collect();
    // where the chat page (llama-server's own) and the coding-assistant endpoint listen, as
    // local-agent.sh sets them for the service; null when this daemon runs on its own
    let port = |v: &str| std::env::var(v).ok().and_then(|p| p.parse::<u16>().ok()).map_or("null".to_string(), |p| p.to_string());
    format!(
        "{{\"dir\":{},\"chosen\":{},\"serving\":{},\"mem_total_mib\":{total},\"chat_port\":{},\"agent_port\":{},\"models\":[{}]}}",
        crate::js(&models_dir().to_string_lossy()),
        crate::js(&chosen),
        crate::js(&serving),
        port("LLAMA_PORT"),
        port("PORT"),
        items.join(",")
    )
}

/// Switches to another model by running `local-agent.sh use NAME` as its own short-lived
/// systemd unit: the switch stops the service this daemon runs in, so it cannot be a child.
/// The settings the service was started with (drafts, thinking, threads...) go along.
fn switch_model(name: &str) -> Result<String, String> {
    if !model_files().iter().any(|(n, _)| n == name) {
        return Err(format!("There is no model called {name} in {}.", models_dir().display()));
    }
    let script = std::env::var_os("CORSIA_LOCAL_AGENT").map(std::path::PathBuf::from).or_else(|| {
        // target/release/corsia -> integrations/llama.cpp/local-agent.sh
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.parent()?.parent()?.join("integrations/llama.cpp/local-agent.sh"))
    });
    let Some(script) = script.filter(|p| p.is_file()) else {
        return Err("Corsia cannot find local-agent.sh to switch models with (set CORSIA_LOCAL_AGENT).".into());
    };
    let unit = format!("corsia-switch-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    let mut cmd = std::process::Command::new("systemd-run");
    cmd.args(["--user", "--collect", "--quiet", "--unit", &unit]);
    // drafts are left out: each model gets its own choice (local-agent.sh DRAFTS=auto)
    for v in ["THINK", "THREADS", "CTX", "PROJECT_DRAFTS", "CORSIA_REPACK"] {
        if let Ok(val) = std::env::var(v) {
            if !val.is_empty() {
                cmd.arg(format!("--setenv={v}={val}"));
            }
        }
    }
    cmd.arg(&script).args(["use", name]);
    match cmd.status() {
        Ok(s) if s.success() => Ok(name.to_string()),
        Ok(s) => Err(format!("systemd-run failed ({s}).")),
        Err(e) => Err(format!("systemd-run could not start: {e}.")),
    }
}

/// The coding-assistant endpoint's own events (one per finished request) and live text,
/// passed through so the page needs one origin. The agent port is PORT (as local-agent.sh
/// sets it for the service), else 7720.
fn agent_proxy(stream: &mut TcpStream, path: &str) -> io::Result<()> {
    let port = std::env::var("PORT").ok().and_then(|p| p.parse::<u16>().ok()).unwrap_or(7720);
    let mut up = match TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(2)) {
        Ok(s) => s,
        Err(_) => return respond(stream, 503, "text/plain", "No coding-assistant endpoint is running.\n"),
    };
    write!(up, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n")?;
    stream.set_read_timeout(None)?;
    io::copy(&mut up, stream).map(|_| ())
}

fn respond(stream: &mut TcpStream, code: u16, kind: &str, body: &str) -> io::Result<()> {
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Request Header Fields Too Large",
    };
    write!(stream, "HTTP/1.1 {code} {reason}\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len())
}

/// Pushes a status event every second until the client goes away or the daemon stops.
fn events(stream: &mut TcpStream, engine: &Engine) -> io::Result<()> {
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\nretry: 2000\n\n")?;
    stream.flush()?;
    let mut last: Vec<u64> = engine.sources.iter().map(|s| s.bytes.load(Ordering::Relaxed)).collect();
    let mut last_t = Instant::now();
    while !engine.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        let secs = last_t.elapsed().as_secs_f64().max(1e-3);
        last_t = Instant::now();
        let rates: Vec<String> = engine
            .sources
            .iter()
            .zip(last.iter_mut())
            .map(|(s, prev)| {
                let now = s.bytes.load(Ordering::Relaxed);
                let rate = (now - *prev) as f64 / secs / 1e6;
                *prev = now;
                format!("{}:{rate:.1}", crate::js(&s.drive))
            })
            .collect();
        let status = engine.status_json();
        // Splice the live rates into the status object.
        let load1 = std::fs::read_to_string("/proc/loadavg").ok().and_then(|l| l.split_whitespace().next().map(str::to_string)).unwrap_or_else(|| "0".into());
        let machine = format!("{{\"mem_total_mib\":{},\"mem_avail_mib\":{},\"load1\":{load1}}}", meminfo_mib("MemTotal:"), meminfo_mib("MemAvailable:"));
        let body = format!("{},\"rate_mb_s\":{{{}}},\"notes\":{},\"progress\":{},\"machine\":{machine}}}", &status[..status.len() - 1], rates.join(","), notes_json(), progress_json());
        write!(stream, "event: status\ndata: {body}\n\n")?;
        stream.flush()?;
    }
    Ok(())
}
