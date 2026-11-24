//! corsia-watch: a live terminal view of Corsia's test runs and of the running daemon.
//! `corsia watch` runs this binary; it is a crate of its own so Corsia's main build stays
//! light.
//!
//!   corsia watch                     runs from ~/.cache/corsia-build/progress, daemon on 7714
//!   corsia watch <folder>            another progress folder
//!   corsia watch --url 127.0.0.1:7715   another daemon (a remote one through ssh -L)
//!   corsia watch --light             colours for a light terminal (NO_COLOR turns colour off)
//!   corsia watch --snapshot 120x36 [--html]   print one frame and exit
//!
//! Runs update when their file changes (inotify through `notify`). The daemon pushes a
//! status event every second over Server-Sent Events. Nothing is polled; the short wait
//! for keys only moves clocks.

mod daemon;
mod model;
mod runs;
mod snapshot;
mod ui;

use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use daemon::{Daemon, Target};
use runs::Store;
use ui::{Theme, View};

const HELP: &str = "corsia watch [FOLDER] [--url HOST:PORT] [--agent HOST:PORT] [--light] [--snapshot WxH [--html]]

Live view of Corsia's test runs and the running daemon.

  FOLDER      progress folder the test scripts write to
              (default $CORSIA_PROGRESS, else ~/.cache/corsia-build/progress)
  --url       the daemon's API (default 127.0.0.1:7714). For a daemon on another
              machine, forward the same port: ssh -L 7714:127.0.0.1:7714 HOST
  --agent     corsia agent, for what the model is writing (default 127.0.0.1:7717);
              forward it the same way: ssh -L 7717:127.0.0.1:7717 HOST
  --light     colours for a light terminal; NO_COLOR=1 turns colour off
  --snapshot  draw one frame of the given size and exit (--html for the colours);
              waits up to --wait SECONDS (default 3) for the daemon first

Keys: up and down (or j and k) choose a run, f shows or hides finished runs, q quits.
";

fn progress_dir(arg: Option<String>) -> PathBuf {
    if let Some(a) = arg {
        return PathBuf::from(a);
    }
    if let Some(p) = std::env::var_os("CORSIA_PROGRESS").filter(|p| !p.is_empty() && p != "off") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".cache/corsia-build/progress")
}

fn value_of(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn main() -> io::Result<()> {
    // `corsia watch ...` passes "watch" through; drop it.
    let args: Vec<String> = std::env::args().skip(1).skip_while(|a| a == "watch").collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return Ok(());
    }
    let light = args.iter().any(|a| a == "--light");
    let theme = if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        Theme::plain()
    } else if light {
        Theme::light()
    } else {
        Theme::dark()
    };
    let snapshot = value_of(&args, "--snapshot");
    let html = args.iter().any(|a| a == "--html");
    let url = value_of(&args, "--url").unwrap_or_else(|| "127.0.0.1:7714".into());
    let wait: f64 = value_of(&args, "--wait").and_then(|w| w.parse().ok()).unwrap_or(3.0);
    let agent_url = value_of(&args, "--agent").unwrap_or_else(|| "127.0.0.1:7717".into());
    let with_values = ["--snapshot", "--url", "--wait", "--agent"];
    let mut positional = None;
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
        } else if with_values.contains(&a.as_str()) {
            skip = true;
        } else if a.starts_with("--") {
            if !["--light", "--html"].contains(&a.as_str()) {
                eprintln!("corsia watch: unknown option {a}. Run `corsia watch --help`.");
                std::process::exit(2);
            }
        } else if positional.is_none() {
            positional = Some(a.clone());
        }
    }
    let target = match Target::parse(&url) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("corsia watch: --url {url}: {e}");
            std::process::exit(2);
        }
    };
    let dir = progress_dir(positional);
    std::fs::create_dir_all(&dir)?;

    let (dtx, drx) = mpsc::channel();
    {
        let target = target.clone();
        std::thread::spawn(move || daemon::follow(target, dtx));
    }
    let mut daemon = Daemon::new(target);

    if let Some(size) = snapshot {
        let mut store = Store::new(dir);
        store.scan();
        // Gather a few seconds of events so the graphs have something to show.
        let until = Instant::now() + Duration::from_secs_f64(wait.max(0.0));
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match drx.recv_timeout(left) {
                Ok(msg) => {
                    let down = matches!(msg, daemon::Msg::Down(_));
                    daemon.apply(msg);
                    if down {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let (bg, fg) = if light { ("#ffffff", "#161a1d") } else { ("#0c0c0e", "#e6e8ea") };
        let frame = snapshot::render(&store, &daemon, &View { selected: 0, finished: true }, &theme, &size, html, bg, fg)?;
        print!("{frame}");
        return Ok(());
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        // Only changes count: reading a file to show it raises access events, and reacting
        // to those made the watcher re-read forever (a core busy while idle).
        if let Ok(ev) = res {
            if matches!(ev.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)) {
                let _ = tx.send(ev.paths);
            }
        }
    })
    .map_err(io::Error::other)?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).map_err(io::Error::other)?;

    let mut store = Store::new(dir);
    store.scan();
    let mut view = View { selected: 0, finished: true };

    // What the model is writing, from corsia agent.
    let agent = match Target::parse(&agent_url) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("corsia watch: --agent {agent_url}: {e}");
            std::process::exit(2);
        }
    };
    let (mtx, mrx) = mpsc::channel();
    {
        let agent = agent.clone();
        std::thread::spawn(move || model::follow(agent, mtx));
    }
    let mut model = model::Model::new(agent);

    ratatui::run(|term| -> io::Result<()> {
        // Draw only when something changed (a progress file, a daemon event, a key, a
        // resize) and once a second for the clocks: redrawing every frame kept a core busy.
        let mut dirty = true;
        let mut last_draw = Instant::now();
        loop {
            while let Ok(paths) = rx.try_recv() {
                for p in paths {
                    store.touch(&p);
                }
                dirty = true;
            }
            while let Ok(msg) = drx.try_recv() {
                daemon.apply(msg);
                dirty = true;
            }
            while let Ok(msg) = mrx.try_recv() {
                model.apply(msg);
                dirty = true;
            }
            if dirty || last_draw.elapsed() >= Duration::from_secs(1) {
                term.draw(|f| ui::draw(f, &store, &daemon, Some(&model), &view, &theme))?;
                dirty = false;
                last_draw = Instant::now();
            }
            let waited = Instant::now();
            if !event::poll(Duration::from_millis(250))? {
                // Pace the loop even where poll returns at once (no terminal, odd pty).
                if let Some(rest) = Duration::from_millis(250).checked_sub(waited.elapsed()) {
                    std::thread::sleep(rest);
                }
                continue;
            }
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Resize(..) => {
                    dirty = true;
                    std::thread::sleep(Duration::from_millis(30)); // a burst of resizes draws once
                    continue;
                }
                _ => {
                    // Some terminals send a steady stream of events we ignore (mouse, focus);
                    // without a pause the loop spins on them.
                    std::thread::sleep(Duration::from_millis(30));
                    continue;
                }
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            dirty = true;
            let count = store.ordered(view.finished).len();
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => view.selected = view.selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => view.selected = (view.selected + 1).min(count.saturating_sub(1)),
                KeyCode::Home | KeyCode::Char('g') => view.selected = 0,
                KeyCode::End | KeyCode::Char('G') => view.selected = count.saturating_sub(1),
                KeyCode::Char('f') => {
                    view.finished = !view.finished;
                    view.selected = 0;
                }
                KeyCode::Char('r') => store.scan(),
                _ => {}
            }
        }
    })
}
