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
    if method == "POST" && path == "/api/notes" {
        if !marked {
            return respond(&mut stream, 403, "text/plain", "Notes need an X-Corsia header.\n");
        }
        let mut body = vec![0u8; length.min(4096)];
        std::io::Read::read_exact(&mut reader, &mut body)?;
        add_note(&String::from_utf8_lossy(&body))?;
        return respond(&mut stream, 200, "application/json", &notes_json());
    }
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", "Only GET is supported (and POST to /api/notes).\n");
    }
    match path {
        "/api/status" => respond(&mut stream, 200, "application/json", &engine.status_json()),
        "/api/notes" => respond(&mut stream, 200, "application/json", &notes_json()),
        "/api/events" => events(&mut stream, engine),
        "/" => respond(&mut stream, 200, "text/plain", "Corsia is running.\nStatus: /api/status\nLive events: /api/events\n"),
        _ => respond(&mut stream, 404, "text/plain", "Not found. Try /api/status or /api/events.\n"),
    }
}

fn respond(stream: &mut TcpStream, code: u16, kind: &str, body: &str) -> io::Result<()> {
    let reason = match code {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
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
        let body = format!("{},\"rate_mb_s\":{{{}}},\"notes\":{},\"progress\":{}}}", &status[..status.len() - 1], rates.join(","), notes_json(), progress_json());
        write!(stream, "event: status\ndata: {body}\n\n")?;
        stream.flush()?;
    }
    Ok(())
}
