//! The daemon's local API, for the management UI and scripts.
//!
//! - `GET /api/status`: everything as JSON
//! - `GET /api/events`: Server-Sent Events, a `status` event every second with per-drive
//!   MB/s since the last one
//!
//! Listens on 127.0.0.1 only and checks the Host header, so a web page elsewhere cannot
//! reach it through DNS rebinding.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::Engine;

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
            }
        }
    }
    if !host_ok {
        return respond(&mut stream, 403, "text/plain", "Corsia only answers requests addressed to 127.0.0.1 or localhost.\n");
    }
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", "Only GET is supported.\n");
    }
    match path {
        "/api/status" => respond(&mut stream, 200, "application/json", &engine.status_json()),
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
        let body = format!("{},\"rate_mb_s\":{{{}}}}}", &status[..status.len() - 1], rates.join(","));
        write!(stream, "event: status\ndata: {body}\n\n")?;
        stream.flush()?;
    }
    Ok(())
}
