//! Project-wide drafts for `corsia agent`.
//!
//! The Corsia fork of llama.cpp drafts free tokens (n-gram drafts: no draft model, no extra
//! compute) by finding the last few tokens earlier in the context and proposing what followed
//! them. A coding assistant also writes a lot that exists elsewhere in the project but not in
//! its context: in the history of two real projects, the project's own files gave 42 to 67%
//! more free drafts than the context alone. So `corsia agent` learns which project the
//! assistant works in (assistants name the working directory in their prompt), tokenizes the
//! project's source files with llama-server's own tokenizer, and writes them to the file the
//! fork reads (`CORSIA_DRAFT_CORPUS`): little-endian i32 token ids, -1 between files. It runs
//! in the background, never in the way of a request, and again when the project changes or
//! the last build is ten minutes old.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::json::{self, Value};

/// Files larger than this are data, not code someone writes like.
const MAX_FILE: u64 = 400 << 10;
/// Text tokenized per project at most (about 8 million tokens).
const MAX_TOTAL: u64 = 32 << 20;
const MAX_FILES: usize = 20_000;
/// A build older than this is redone on the next request.
const STALE: Duration = Duration::from_secs(600);

const EXTENSIONS: &[&str] = &[
    "rs", "c", "h", "cc", "cpp", "hpp", "cu", "py", "ts", "tsx", "js", "jsx", "mjs", "go", "java", "kt", "swift", "rb",
    "php", "cs", "scala", "lua", "sh", "zsh", "sql", "css", "scss", "html", "vue", "svelte", "md", "toml", "yaml",
    "yml", "json", "proto", "graphql", "dart", "zig", "ex", "exs", "nix",
];
const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", ".next", "out", "vendor", "__pycache__", ".venv", "venv",
    ".turbo", ".cache", "coverage",
];

#[derive(Default)]
struct Status {
    project: Option<PathBuf>,
    built: Option<Instant>,
    building: bool,
    files: usize,
    tokens: usize,
    note: String,
}

pub struct Corpus {
    /// The file llama-server reads (CORSIA_DRAFT_CORPUS).
    pub path: PathBuf,
    status: Mutex<Status>,
}

impl Corpus {
    pub fn new(path: PathBuf) -> Corpus {
        Corpus { path, status: Mutex::new(Status::default()) }
    }

    /// Called with each chat request (before it is rewritten): starts a build in the
    /// background when the request names a new project or the last build is stale.
    pub fn notice(self: &Arc<Self>, body: &Value, upstream: &str) {
        let Some(dir) = project_dir(body) else { return };
        {
            let mut s = self.status.lock().unwrap();
            let fresh = s.project.as_deref() == Some(dir.as_path()) && s.built.is_some_and(|t| t.elapsed() < STALE);
            if s.building || fresh {
                return;
            }
            s.building = true;
        }
        let me = self.clone();
        let upstream = upstream.to_string();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let result = me.build(&dir, &upstream);
            let mut s = me.status.lock().unwrap();
            s.building = false;
            match result {
                Ok((files, tokens)) => {
                    s.project = Some(dir.clone());
                    s.built = Some(Instant::now());
                    s.files = files;
                    s.tokens = tokens;
                    s.note = String::new();
                    eprintln!(
                        "  Project drafts: {} files, {} tokens from {} in {:.1} s",
                        files,
                        tokens,
                        dir.display(),
                        t0.elapsed().as_secs_f64()
                    );
                }
                Err(e) => {
                    s.note = e.clone();
                    eprintln!("  Project drafts: could not read {}: {e}", dir.display());
                }
            }
        });
    }

    /// For `/api/agent`: `{"file":..,"project":..,"files":..,"tokens":..,"building":..,"note":..}`.
    pub fn status_json(&self) -> String {
        let s = self.status.lock().unwrap();
        let mut out = String::from("{\"file\":");
        json::write_str(&self.path.display().to_string(), &mut out);
        out.push_str(",\"project\":");
        match &s.project {
            Some(p) => json::write_str(&p.display().to_string(), &mut out),
            None => out.push_str("null"),
        }
        out.push_str(&format!(",\"files\":{},\"tokens\":{},\"building\":{},\"note\":", s.files, s.tokens, s.building));
        json::write_str(&s.note, &mut out);
        out.push('}');
        out
    }

    fn build(&self, dir: &Path, upstream: &str) -> Result<(usize, usize), String> {
        let files = source_files(dir);
        if files.is_empty() {
            return Err("no source files found".into());
        }
        let mut bytes: Vec<u8> = Vec::new();
        let (mut n_files, mut n_tokens, mut total) = (0usize, 0usize, 0u64);
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else { continue }; // not text
            total += text.len() as u64;
            if total > MAX_TOTAL {
                break;
            }
            let toks = tokenize(upstream, &text)?;
            for t in &toks {
                bytes.extend_from_slice(&t.to_le_bytes());
            }
            bytes.extend_from_slice(&(-1i32).to_le_bytes());
            n_files += 1;
            n_tokens += toks.len();
        }
        let parent = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        // Kept per project, then swapped into place whole: llama-server never reads half a file.
        let mut h = DefaultHasher::new();
        dir.hash(&mut h);
        let keep = parent.join(format!("{:016x}.tok", h.finish()));
        std::fs::write(&keep, &bytes).map_err(|e| format!("cannot write {}: {e}", keep.display()))?;
        let tmp = self.path.with_extension("tok.tmp");
        std::fs::copy(&keep, &tmp).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("cannot replace {}: {e}", self.path.display()))?;
        Ok((n_files, n_tokens))
    }
}

/// The working directory the assistant names in its prompt ("Working directory: /path",
/// as opencode and others write it), looked for in the messages before the first answer.
pub fn project_dir(body: &Value) -> Option<PathBuf> {
    let msgs = body.get("messages").and_then(Value::as_arr)?;
    for m in msgs {
        if m.get("role").and_then(Value::as_str) == Some("assistant") {
            break;
        }
        let texts: Vec<&str> = match m.get("content") {
            Some(Value::Str(s)) => vec![s.as_str()],
            Some(Value::Arr(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect(),
            _ => vec![],
        };
        for t in texts {
            for line in t.lines() {
                let l = line.trim().trim_start_matches(['-', '*', ' ']);
                let lower = l.to_ascii_lowercase();
                for label in ["working directory:", "current working directory:", "cwd:"] {
                    if let Some(rest) = lower.strip_prefix(label) {
                        let path = l[l.len() - rest.len()..].trim().trim_matches(['`', '"', '\'']);
                        let p = PathBuf::from(path);
                        if p.is_absolute() && p.is_dir() {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    None
}

/// The project's source files: what git tracks (and untracked files it does not ignore)
/// when it is a repository, otherwise a walk that skips build and dependency folders.
fn source_files(dir: &Path) -> Vec<PathBuf> {
    let wanted = |p: &Path| {
        p.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            && std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_FILE)
    };
    if dir.join(".git").exists() {
        if let Ok(out) = Command::new("git").arg("-C").arg(dir).args(["ls-files", "-co", "--exclude-standard"]).output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(|l| dir.join(l))
                    .filter(|p| wanted(p))
                    .take(MAX_FILES)
                    .collect();
            }
        }
    }
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref()) {
                    stack.push(p);
                }
            } else if kind.is_file() && wanted(&p) {
                found.push(p);
                if found.len() >= MAX_FILES {
                    return found;
                }
            }
        }
    }
    found.sort();
    found
}

/// Token ids for `text` from llama-server's own tokenizer, so they match what it generates.
fn tokenize(upstream: &str, text: &str) -> Result<Vec<i32>, String> {
    let mut body = String::from("{\"content\":");
    json::write_str(text, &mut body);
    body.push_str(",\"add_special\":false}");
    let (code, reply) = crate::agent::upstream_call(upstream, "POST", "/tokenize", &body).map_err(|e| format!("llama-server: {e}"))?;
    if code != 200 {
        return Err(format!("llama-server answered /tokenize with {code}"));
    }
    let v = json::parse(&reply).map_err(|e| format!("llama-server's /tokenize reply: {e}"))?;
    let toks = v.get("tokens").and_then(Value::as_arr).ok_or("llama-server's /tokenize reply has no tokens")?;
    Ok(toks.iter().filter_map(|t| t.as_f64()).map(|f| f as i32).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_working_directory() {
        let dir = std::env::temp_dir();
        let text = format!("You are an agent.\n<env>\n  Working directory: {}\n  Today's date: x\n</env>\n", dir.display());
        let mut body = String::from("{\"messages\":[{\"role\":\"system\",\"content\":");
        json::write_str(&text, &mut body);
        body.push_str("},{\"role\":\"user\",\"content\":\"hi\"}]}");
        let v = json::parse(&body).unwrap();
        assert_eq!(project_dir(&v), Some(dir));
        let none = json::parse("{\"messages\":[{\"role\":\"user\",\"content\":\"Working directory: relative/path\"}]}").unwrap();
        assert_eq!(project_dir(&none), None);
    }
}
