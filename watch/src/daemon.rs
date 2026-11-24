//! The live link to a running `corsia serve`: its Server-Sent Events at /api/events, one
//! `status` event a second. A background thread holds the connection, reconnects with a
//! growing pause when the daemon is not there, and hands each event to the UI thread.
//! Plain HTTP/1.1 over a TcpStream: the daemon only listens on loopback.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::Value;

const HISTORY: usize = 180;
/// No event for this long means the link is stale (the daemon sends one a second).
pub const STALE_AFTER: Duration = Duration::from_secs(4);

pub enum Msg {
    Status(Value),
    Down(String),
}

#[derive(Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

impl Target {
    /// Accepts `http://127.0.0.1:7714`, `127.0.0.1:7714`, `localhost:7715/api/events` or `7715`.
    pub fn parse(s: &str) -> Result<Target, String> {
        let rest = s.trim().trim_start_matches("http://");
        if rest.starts_with("https://") {
            return Err("Corsia's API is plain HTTP on this machine; use http://".into());
        }
        let authority = rest.split('/').next().unwrap_or("");
        if let Ok(port) = authority.parse::<u16>() {
            return Ok(Target { host: "127.0.0.1".into(), port });
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| format!("\"{p}\" is not a port number"))?),
            None => (authority, 7714),
        };
        if host.is_empty() {
            return Err(format!("no host in \"{s}\""));
        }
        Ok(Target { host: host.to_string(), port })
    }

    pub fn label(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Runs forever on its own thread. Ends when the receiver is gone.
pub fn follow(target: Target, tx: Sender<Msg>) {
    let mut pause = Duration::from_millis(500);
    loop {
        let started = Instant::now();
        let why = match stream(&target, &tx) {
            Ok(()) => "the daemon closed the connection".to_string(),
            Err(e) => e,
        };
        if tx.send(Msg::Down(why)).is_err() {
            return;
        }
        // A link that lasted a while starts its retries short again.
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
        std::io::ErrorKind::ConnectionRefused => "nothing answers there".to_string(),
        std::io::ErrorKind::TimedOut => "no answer within 2 s".to_string(),
        _ => e.to_string(),
    })?;
    sock.set_read_timeout(Some(STALE_AFTER + Duration::from_secs(2))).map_err(|e| e.to_string())?;
    write!(sock, "GET /api/events HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n", target.label())
        .map_err(|e| e.to_string())?;
    let mut r = BufReader::new(sock);
    let mut line = String::new();
    r.read_line(&mut line).map_err(read_err)?;
    let code = line.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(match code {
            "403" => "the daemon refused the request; forward the same port number it listens on".into(),
            "404" => "something answers there, but it is not Corsia (no /api/events)".into(),
            "" => "something answers there, but it is not Corsia".into(),
            c => format!("the daemon answered {c}"),
        });
    }
    loop {
        line.clear();
        if r.read_line(&mut line).map_err(read_err)? == 0 {
            return Ok(());
        }
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                if tx.send(Msg::Status(v)).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

fn read_err(e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "the daemon stopped sending updates".into(),
        _ => e.to_string(),
    }
}

pub enum Link {
    Connecting,
    Live,
    Down { why: String, since: Instant },
}

pub struct Drive {
    pub name: String,
    pub rates: VecDeque<f64>,
}

/// What the UI knows about the daemon.
pub struct Daemon {
    pub target: Target,
    pub link: Link,
    pub last: Option<Value>,
    pub last_at: Option<Instant>,
    pub drives: Vec<Drive>,
    pub total: VecDeque<f64>,
    pub peak: f64,
}

impl Daemon {
    pub fn new(target: Target) -> Daemon {
        Daemon { target, link: Link::Connecting, last: None, last_at: None, drives: Vec::new(), total: VecDeque::new(), peak: 0.0 }
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Status(v) => self.status(v),
            Msg::Down(why) => {
                if !matches!(self.link, Link::Down { .. }) {
                    self.link = Link::Down { why, since: Instant::now() };
                } else if let Link::Down { why: w, .. } = &mut self.link {
                    *w = why;
                }
            }
        }
    }

    fn status(&mut self, v: Value) {
        // A different model or a restarted daemon starts the graphs afresh.
        let restarted = self.last.as_ref().is_some_and(|old| {
            old["model"]["path"] != v["model"]["path"] || v["uptime_s"].as_f64() < old["uptime_s"].as_f64()
        });
        if restarted || !matches!(self.link, Link::Live) && self.last.as_ref().is_none_or(|o| o["model"]["path"] != v["model"]["path"]) {
            self.drives.clear();
            self.total.clear();
            self.peak = 0.0;
        }
        let mut sum = 0.0;
        for s in v["sources"].as_array().into_iter().flatten() {
            let name = s["drive"].as_str().unwrap_or("?").to_string();
            let rate = v["rate_mb_s"][&name].as_f64().unwrap_or(0.0);
            sum += rate;
            let d = match self.drives.iter().position(|d| d.name == name) {
                Some(i) => &mut self.drives[i],
                None => {
                    self.drives.push(Drive { name, rates: VecDeque::new() });
                    self.drives.last_mut().unwrap()
                }
            };
            d.rates.push_back(rate);
            if d.rates.len() > HISTORY {
                d.rates.pop_front();
            }
            self.peak = self.peak.max(rate);
        }
        self.total.push_back(sum);
        if self.total.len() > HISTORY {
            self.total.pop_front();
        }
        self.last = Some(v);
        self.last_at = Some(Instant::now());
        self.link = Link::Live;
    }

    /// Seconds since the last event, when that is long enough to say so.
    pub fn stale(&self) -> Option<u64> {
        let age = self.last_at?.elapsed();
        (matches!(self.link, Link::Live) && age > STALE_AFTER).then(|| age.as_secs())
    }

    pub fn rate_now(&self) -> f64 {
        self.total.back().copied().unwrap_or(0.0)
    }

    pub fn total_peak(&self) -> f64 {
        self.total.iter().copied().fold(0.0, f64::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let t = Target::parse("http://127.0.0.1:7714/api/events").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("127.0.0.1", 7714));
        assert_eq!(Target::parse("7715").unwrap().port, 7715);
        assert_eq!(Target::parse("localhost").unwrap().port, 7714);
        assert!(Target::parse("https://x").is_err());
    }

    #[test]
    fn rates_per_drive() {
        let mut d = Daemon::new(Target::parse("7714").unwrap());
        let v: Value = serde_json::from_str(
            r#"{"model":{"path":"/m"},"uptime_s":3,"sources":[{"drive":"sda"},{"drive":"sdc"}],"rate_mb_s":{"sda":400.0,"sdc":30.5}}"#,
        )
        .unwrap();
        d.apply(Msg::Status(v));
        assert_eq!(d.drives.len(), 2);
        assert_eq!(d.rate_now(), 430.5);
        assert!(matches!(d.link, Link::Live));
        d.apply(Msg::Down("nothing answers there".into()));
        assert!(matches!(d.link, Link::Down { .. }));
    }
}
