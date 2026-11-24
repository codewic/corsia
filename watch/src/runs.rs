//! Test runs read from the progress folder, one `<run>.jsonl` file each, written by the
//! scripts in integrations/ (through integrations/progress.sh). The format is the one
//! vi-watch reads: `start`, `step`, `log` and `end` events, one JSON object per line.
//! Corsia adds three optional fields: `unit` on start ("arms"), `better` on the headline
//! ("higher" or "lower") and `current` on a log line (what the run is doing now).
//! Each file is followed from where it was last read, so updates cost only the new lines.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const KEEP_STEPS: usize = 600;
const KEEP_LOGS: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Done,
    Failed,
    Stopped,
}

pub struct Step {
    pub done: u64,
    pub item: Option<String>,
    pub metrics: Vec<(String, f64)>,
}

pub struct Headline {
    pub metric: String,
    pub label: String,
    pub lower_is_better: bool,
}

#[derive(Default)]
pub struct Run {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub unit: Option<String>,
    pub total: Option<u64>,
    pub pid: Option<u32>,
    pub meta: Vec<(String, String)>,
    pub headline: Option<Headline>,
    pub started: f64,
    pub done: u64,
    pub last_t: f64,
    pub current: Option<(String, f64)>,
    pub end: Option<(f64, String, Vec<(String, String)>)>,
    pub steps: VecDeque<Step>,
    pub logs: VecDeque<(f64, String)>,
    /// Metric names in the order the run first reported them.
    pub metric_names: Vec<String>,
    path: PathBuf,
    offset: u64,
    partial: String,
}

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "none".into(),
        other => other.to_string(),
    }
}

fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_object().map_or_else(Vec::new, |o| o.iter().map(|(k, v)| (k.clone(), text(v))).collect())
}

impl Run {
    fn new(path: PathBuf) -> Run {
        let id = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        Run { id: id.clone(), title: id, path, ..Run::default() }
    }

    pub fn status(&self) -> Status {
        match &self.end {
            Some((_, s, _)) if s == "done" => Status::Done,
            Some(_) => Status::Failed,
            None if self.pid.is_some_and(|p| !alive(p)) => Status::Stopped,
            None => Status::Running,
        }
    }

    pub fn fraction(&self) -> Option<f64> {
        self.total.filter(|&t| t > 0).map(|t| (self.done as f64 / t as f64).min(1.0))
    }

    fn until(&self) -> f64 {
        match (&self.end, self.status()) {
            (Some((t, _, _)), _) => *t,
            (None, Status::Stopped) => self.last_t.max(self.started),
            _ => now(),
        }
    }

    pub fn elapsed(&self) -> f64 {
        (self.until() - self.started).max(0.0)
    }

    /// What the run is doing now and for how long, while it runs.
    pub fn now_doing(&self) -> Option<(&str, f64)> {
        if self.status() != Status::Running {
            return None;
        }
        self.current.as_ref().map(|(what, since)| (what.as_str(), (now() - since).max(0.0)))
    }

    pub fn values(&self, metric: &str) -> Vec<(Option<&str>, f64)> {
        self.steps
            .iter()
            .filter_map(|s| s.metrics.iter().find(|(k, _)| k == metric).map(|(_, v)| (s.item.as_deref(), *v)))
            .collect()
    }

    /// The best headline value so far and the step it came from.
    pub fn best(&self) -> Option<(Option<&str>, f64)> {
        let h = self.headline.as_ref()?;
        let vals = self.values(&h.metric);
        let pick = |a: &(Option<&str>, f64), b: &(Option<&str>, f64)| a.1.total_cmp(&b.1);
        if h.lower_is_better { vals.into_iter().min_by(pick) } else { vals.into_iter().max_by(pick) }
    }

    pub fn sort_key(&self) -> (u8, i64) {
        let live = (self.status() == Status::Running) as u8;
        let t = self.end.as_ref().map_or(self.last_t.max(self.started), |e| e.0);
        (1 - live, -(t * 1000.0) as i64)
    }

    fn apply(&mut self, ev: &Value) {
        let t = ev["t"].as_f64().unwrap_or(0.0);
        self.last_t = self.last_t.max(t);
        match ev["event"].as_str() {
            Some("start") => {
                self.title = ev["title"].as_str().unwrap_or(&self.id).to_string();
                self.kind = ev["kind"].as_str().unwrap_or("run").to_string();
                self.unit = ev["unit"].as_str().map(str::to_string);
                self.total = ev["total"].as_u64();
                self.pid = ev["pid"].as_u64().map(|p| p as u32);
                self.meta = pairs(&ev["meta"]);
                self.headline = ev["headline"]["metric"].as_str().map(|m| Headline {
                    metric: m.to_string(),
                    label: ev["headline"]["label"].as_str().unwrap_or(m).to_string(),
                    lower_is_better: ev["headline"]["better"].as_str() == Some("lower"),
                });
                self.started = t;
            }
            Some("step") => {
                self.done = ev["done"].as_u64().unwrap_or(self.done + 1);
                let metrics: Vec<(String, f64)> = ev["metrics"]
                    .as_object()
                    .map_or_else(Vec::new, |o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect());
                for (name, _) in &metrics {
                    if !self.metric_names.contains(name) {
                        self.metric_names.push(name.clone());
                    }
                }
                let item = ev["item"].as_str().map(str::to_string);
                if self.current.as_ref().is_some_and(|(c, _)| Some(c) == item.as_ref()) {
                    self.current = None;
                }
                self.steps.push_back(Step { done: self.done, item, metrics });
                if self.steps.len() > KEEP_STEPS {
                    self.steps.pop_front();
                }
            }
            Some("log") => {
                if let Some(c) = ev["current"].as_str() {
                    self.current = Some((c.to_string(), t));
                }
                if let Some(msg) = ev.get("msg") {
                    self.logs.push_back((t, text(msg)));
                    if self.logs.len() > KEEP_LOGS {
                        self.logs.pop_front();
                    }
                }
            }
            Some("end") => {
                self.current = None;
                self.end = Some((t, ev["status"].as_str().unwrap_or("done").to_string(), pairs(&ev["summary"])));
            }
            _ => {}
        }
    }

    /// Read whatever was appended since the last call. A file that shrank was restarted.
    fn refresh(&mut self) {
        let Ok(mut f) = File::open(&self.path) else { return };
        let len = f.metadata().map_or(0, |m| m.len());
        if len < self.offset {
            *self = Run::new(self.path.clone());
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return;
        }
        self.offset += buf.len() as u64;
        self.partial.push_str(&buf);
        let Some(cut) = self.partial.rfind('\n') else { return };
        let complete: String = self.partial.drain(..=cut).collect();
        for line in complete.lines() {
            if let Ok(ev) = serde_json::from_str::<Value>(line) {
                self.apply(&ev);
            }
        }
    }
}

fn alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") { Path::new(&format!("/proc/{pid}")).exists() } else { true }
}

pub struct Store {
    pub dir: PathBuf,
    runs: BTreeMap<PathBuf, Run>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir, runs: BTreeMap::new() }
    }

    pub fn scan(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let present: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        self.runs.retain(|p, _| present.contains(p));
        for p in present {
            self.touch(&p);
        }
    }

    pub fn touch(&mut self, path: &Path) {
        if path.extension().is_none_or(|x| x != "jsonl") {
            return;
        }
        if !path.exists() {
            self.runs.remove(path);
            return;
        }
        self.runs.entry(path.to_path_buf()).or_insert_with(|| Run::new(path.to_path_buf())).refresh();
    }

    /// Running first, then the most recently finished.
    pub fn ordered(&self, finished: bool) -> Vec<&Run> {
        let mut v: Vec<&Run> = self.runs.values().filter(|r| finished || r.status() == Status::Running).collect();
        v.sort_by_key(|r| r.sort_key());
        v
    }

    pub fn counts(&self) -> (usize, usize) {
        let live = self.runs.values().filter(|r| r.status() == Status::Running).count();
        (live, self.runs.len() - live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_from(lines: &[&str]) -> Run {
        let mut r = Run::new(PathBuf::from("/x/stream-1.jsonl"));
        for l in lines {
            r.apply(&serde_json::from_str(l).unwrap());
        }
        r
    }

    #[test]
    fn arms_current_and_best() {
        let r = run_from(&[
            r#"{"event":"start","t":1,"title":"T","total":2,"unit":"arms","headline":{"metric":"tok/s","label":"Generating","better":"higher"}}"#,
            r#"{"event":"log","t":2,"msg":"ssd: starting","current":"ssd"}"#,
            r#"{"event":"step","t":3,"done":1,"item":"ssd","metrics":{"tok/s":0.05,"s/token":18.8}}"#,
            r#"{"event":"log","t":4,"msg":"stream: starting","current":"stream"}"#,
            r#"{"event":"step","t":5,"done":2,"item":"stream","metrics":{"tok/s":1.47,"s/token":0.68}}"#,
        ]);
        assert_eq!(r.metric_names, vec!["tok/s", "s/token"]);
        assert!(r.current.is_none());
        assert_eq!(r.best(), Some((Some("stream"), 1.47)));
        assert_eq!(r.unit.as_deref(), Some("arms"));
    }

    #[test]
    fn lower_is_better() {
        let r = run_from(&[
            r#"{"event":"start","t":1,"headline":{"metric":"load s","better":"lower"}}"#,
            r#"{"event":"step","t":2,"item":"direct 1","metrics":{"load s":40}}"#,
            r#"{"event":"step","t":3,"item":"corsia 1","metrics":{"load s":12.5}}"#,
        ]);
        assert_eq!(r.best(), Some((Some("corsia 1"), 12.5)));
        assert_eq!(r.done, 2);
    }
}
