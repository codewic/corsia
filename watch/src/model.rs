//! The live link to a running `corsia agent`: what the model is writing, as it streams,
//! from `/api/agent/live` (Server-Sent Events: `start`, then `ask` (the question) or `result`
//! (what a tool returned), `think`, `text` and `tool` pieces, `end`).
//! Like the daemon link: a background thread holds the connection, reconnects with a
//! growing pause when nothing answers, and hands each event to the UI thread.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::daemon::{Link, Target};

/// How much of each part the panel keeps (the end of it is what shows).
const KEEP: usize = 8000;

pub enum Msg {
    Event(Value),
    Down(String),
}

/// Runs forever on its own thread. Ends when the receiver is gone.
pub fn follow(target: Target, tx: Sender<Msg>) {
    let mut pause = Duration::from_millis(500);
    loop {
        let started = Instant::now();
        let why = match stream(&target, &tx) {
            Ok(()) => "corsia agent closed the connection".to_string(),
            Err(e) => e,
        };
        if tx.send(Msg::Down(why)).is_err() {
            return;
        }
        if started.elapsed() > Duration::from_secs(10) {
            pause = Duration::from_millis(500);
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_secs(5));
    }
}

fn stream(target: &Target, tx: &Sender<Msg>) -> Result<(), String> {
    let addr = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", target.host))?
        .next()
        .ok_or_else(|| format!("cannot resolve {}", target.host))?;
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "no corsia agent answers there".to_string(),
        std::io::ErrorKind::TimedOut => "no answer within 2 s".to_string(),
        _ => e.to_string(),
    })?;
    // corsia agent sends a keep-alive comment every 15 s while the model is quiet.
    sock.set_read_timeout(Some(Duration::from_secs(25))).map_err(|e| e.to_string())?;
    write!(sock, "GET /api/agent/live HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n", target.label())
        .map_err(|e| e.to_string())?;
    let mut r = BufReader::new(sock);
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| e.to_string())?;
    let code = line.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(match code {
            "403" => "corsia agent refused the request; forward the same port number it listens on".into(),
            "404" => "this corsia agent has no live text yet (an older build)".into(),
            "" => "something answers there, but it is not corsia agent".into(),
            c => format!("corsia agent answered {c}"),
        });
    }
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err("corsia agent stopped sending".into())
            }
            Err(e) => return Err(e.to_string()),
        }
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                if tx.send(Msg::Event(v)).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// What the UI knows about the model's current (or last) answer.
pub struct Model {
    pub target: Target,
    pub link: Link,
    /// The request being answered, 0 before the first.
    pub id: u64,
    /// What this answer responds to: the question, or the tool result it was given.
    pub context: Option<(bool, String)>,
    pub thinking: String,
    pub answer: String,
    /// Set when the answer is complete: tokens, speed, prompt reuse.
    pub done: Option<String>,
    pub started: Option<Instant>,
    /// When the first token of this answer arrived (speed is counted from here, so the
    /// prompt reading before it does not drag the figure down).
    pub first_piece: Option<Instant>,
    pub last_piece: Option<Instant>,
    pub pieces: u64,
}

impl Model {
    pub fn new(target: Target) -> Model {
        Model {
            target,
            link: Link::Connecting,
            id: 0,
            context: None,
            thinking: String::new(),
            answer: String::new(),
            done: None,
            started: None,
            first_piece: None,
            last_piece: None,
            pieces: 0,
        }
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Down(why) => {
                if !matches!(self.link, Link::Down { .. }) {
                    self.link = Link::Down { why, since: Instant::now() };
                }
            }
            Msg::Event(v) => {
                self.link = Link::Live;
                let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
                let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
                let text = v.get("text").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "start" => {
                        self.id = id;
                        self.context = None;
                        self.thinking.clear();
                        self.answer.clear();
                        self.done = None;
                        self.started = Some(Instant::now());
                        self.first_piece = None;
                        self.pieces = 0;
                    }
                    "ask" | "result" if id == self.id => {
                        self.context = Some((kind == "result", text.to_string()));
                    }
                    "think" | "text" | "tool" => {
                        if id != self.id {
                            // Joined mid-answer: start showing from here.
                            self.id = id;
                            self.context = None;
                            self.thinking.clear();
                            self.answer.clear();
                            self.done = None;
                            self.started.get_or_insert_with(Instant::now);
                        }
                        let part = if kind == "think" { &mut self.thinking } else { &mut self.answer };
                        part.push_str(text);
                        if part.len() > KEEP {
                            let mut cut = part.len() - KEEP;
                            while !part.is_char_boundary(cut) {
                                cut += 1;
                            }
                            part.drain(..cut);
                        }
                        self.pieces += 1;
                        let now = Instant::now();
                        self.first_piece.get_or_insert(now);
                        self.last_piece = Some(now);
                    }
                    "end" if id == self.id || self.id == 0 => {
                        self.id = id;
                        self.done = Some(text.to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    /// Tokens per second since the first token (each streamed piece is one token), once
    /// there are enough to say.
    pub fn speed(&self) -> Option<f64> {
        let (a, b) = (self.first_piece?, self.last_piece?);
        let secs = b.duration_since(a).as_secs_f64();
        (self.pieces >= 5 && secs > 0.5).then(|| (self.pieces - 1) as f64 / secs)
    }

    /// True while an answer is streaming.
    pub fn writing(&self) -> bool {
        self.id != 0 && self.done.is_none()
    }
}
