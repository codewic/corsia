//! Corsia: spreads model weights across every drive in the machine and streams them at
//! the combined speed. This first release ships the lane finder and the lane bench.

mod agent;
mod api;
mod bench;
mod brand;
mod corpus;
mod engine;
mod fusefs;
mod hint;
mod hintclient;
mod json;
mod layout;
mod predict;
mod replay;
mod store;
mod topology;
mod trace;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use bench::{Measure, Target, SOLO, STOP, TOGETHER};
use brand::{BENCH_DIR, BIN, NAME};
use topology::{short, Bus, Drive, Topology};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    match args.first().map(String::as_str) {
        Some("lanes") => lanes(json),
        Some("bench") => match BenchArgs::parse(&args[1..]) {
            Ok(b) => bench_cmd(b, json),
            Err(e) => fail(&e),
        },
        Some("trace") => trace_cmd(&args[1..]),
        Some("predict") => predict_cmd(&args[1..]),
        Some("hint-bench") => hint_bench_cmd(&args[1..]),
        Some("hint-prefetch") => hint_prefetch_cmd(&args[1..]),
        Some("serve") => match ServeArgs::parse(&args[1..]) {
            Ok(s) => serve_cmd(s),
            Err(e) => fail(&e),
        },
        Some("replay") => match ReplayArgs::parse(&args[1..]) {
            Ok(r) => replay_cmd(r, json),
            Err(e) => fail(&e),
        },
        Some("watch") => watch_cmd(&args[1..]),
        Some("agent") => agent::run(&args[1..]),
        Some("--version" | "-V") => {
            println!("{BIN} {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        None | Some("--help" | "-h" | "help") => {
            print!("{}", help());
            ExitCode::SUCCESS
        }
        Some(other) => fail(&format!("Unknown command \"{other}\". Run `{BIN} --help` to see the commands.")),
    }
}

fn help() -> String {
    format!(
        "{NAME} {ver}: stream model weights from every drive you have, at their combined speed.
Usage:
  {BIN} agent [--upstream HOST:PORT] [--port N] [--slots N] [--snapshots DIR]
               [--min-snapshot-tokens N] [--volatile LABEL]... [--log FILE] [--plain]
               [--corpus FILE|off]
      OpenAI-compatible endpoint (http://127.0.0.1:7720/v1) in front of llama-server
      (default 127.0.0.1:8080) that makes coding assistants fast on a local model: moves
      the date, folder and git status out of the system prompt so its long prefix is
      reused, keeps side requests off the main slot, and saves each system prompt's KV
      state to DIR (llama-server's --slot-save-path; auto picks a folder on the fastest
      drive and prints it) to restore it in later sessions.
      Reports cache reuse per request; live on /api/agent and /api/agent/events.
      Project-wide drafts: the project the assistant names is tokenized into FILE
      (default ~/.cache/corsia/corpus/current.tok) for the fork's free n-gram drafts;
      give llama-server CORSIA_DRAFT_CORPUS=FILE and --spec-type ngram-simple.

  {BIN} serve MODEL --mount DIR [--copy-to DIR]... [--copy-gib N] [--cache-mib N]
                     [--copy-layout experts|file] [--copy-order TRACE]
                     [--port N] [--threads N]
      Mount MODEL read-only at DIR/<name> for any engine to open. Reads come from the
      original or from speed copies on other drives, whichever will finish first. Copies
      fill while nothing is reading, busiest chunks first. --copy-layout experts stores
      each expert's slices side by side (one read per expert instead of three), with the
      experts that fire together in TRACE as neighbours; such a copy may sit on the
      original's own drive, which is what makes a lone hard disk fast. Reads go to the drives in 1 MiB
      blocks kept in RAM (--cache-mib, default 256). Live status on
      http://127.0.0.1:PORT/api/status and /api/events (default port 7714).

  {BIN} watch [FOLDER] [--url HOST:PORT] [--light]
      Live terminal view of the test runs (integrations/*.sh report to
      ~/.cache/corsia-build/progress) and of the running daemon's drives, prefetch,
      cache and copies. Runs the separate corsia-watch binary (built from watch/).

  {BIN} hint-bench SOCKET [--threads N] [--batch N] [--verify ORIGINAL]
      Read the served model through the engine socket (hinted mode): every 4 MiB range
      once, in random order, N ranges per request. --verify checks every byte against
      the original file, with odd offsets and lengths.

  {BIN} hint-prefetch SOCKET [--async] OFFSET:LEN...
      Put byte ranges of the served model into the page cache of the mounted file, the
      way an engine that memory-maps the mount asks for the experts it is about to use.

  {BIN} lanes [--json]
      Show each drive, the controller (lane) it hangs off, and how to place drives for speed.

  {BIN} bench [DIR...] [--size MIB] [--seconds N] [--keep] [--json]
      Measure every drive alone, then all at once. Without DIRs it picks one writable
      folder per drive. Writes one test file per drive (default 1024 MiB) and deletes it
      afterwards unless --keep. Needs no root.

  {BIN} trace synth FILE [--tokens N] [--layers N] [--experts N] [--topk N]
                         [--skew X] [--repeat X] [--affinity X] [--seed N]
      Write a synthetic expert routing trace (default: shaped like Qwen3-30B-A3B).
      --affinity is the chance a slot follows the previous layer's usual successors.

  {BIN} predict TRACE [--ahead N] [--pick N] [--warmup N]
      How well Corsia's predictor names the next layers' experts in advance, learning
      as it goes. No drives involved.

  {BIN} replay TRACE [DIR...] [--expert-kib N] [--cache-mib N] [--tokens N]
                     [--warmup N] [--span-mib N] [--ahead N] [--pick N]
                     [--compute-ms N] [--only KEY,KEY] [--keep] [--json]
      Play a routing trace against your drives: a RAM cache decides what must be read,
      then each placement strategy reads it from the real drives, layer by layer.
      --compute-ms adds CPU time per token (spread over the layers, the PC sleeps), so
      prefetching can overlap with it.
      --disk-model [--model GGUF] [--order-from TRACE] [--order-tokens N]
      [--seek-ms X] [--disk-mb-s X] [--measured-s X]: no drives; counts the reads and
      seeks per token on a modelled hard disk for the file's layout and for expert-major
      copies (id order and co-firing order, learned from the warm-up tokens unless
      --order-from). --model takes the real expert offsets from a GGUF. --measured-s fits
      the seek cost to a measured s/token for the file layout. Strategy keys:
      one, sliced, whole, scheduled, ahead1, ahead3, one_ahead_token, ahead_token,
      predicted.

Drives on the same lane share its bandwidth, so speed adds up per lane, not per drive.
",
        ver = env!("CARGO_PKG_VERSION")
    )
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("{BIN}: {msg}");
    ExitCode::FAILURE
}

/// `corsia watch`: runs corsia-watch, a crate of its own (watch/) so the terminal UI's
/// dependencies stay out of this build. Looks next to this binary, then in the source
/// tree's watch/target, then on PATH.
fn watch_cmd(args: &[String]) -> ExitCode {
    let name = format!("{BIN}-watch");
    let exe = std::env::current_exe().ok();
    let dir = exe.as_deref().and_then(Path::parent);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(d) = dir {
        candidates.push(d.join(&name));
        // target/release/corsia -> watch/target/release/corsia-watch
        if let Some(root) = d.parent().and_then(Path::parent) {
            candidates.push(root.join("watch/target/release").join(&name));
        }
    }
    let found = candidates.into_iter().find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from(&name));
    match std::process::Command::new(&found).args(args).status() {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fail(&format!(
            "{name} is not built yet. Build it with: cargo build --release --manifest-path watch/Cargo.toml"
        )),
        Err(e) => fail(&format!("could not start {name}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// lanes

fn lanes(json: bool) -> ExitCode {
    let topo = Topology::discover();
    if json {
        println!("{}", lanes_json(&topo));
        return ExitCode::SUCCESS;
    }
    println!(
        "{NAME} found {} drive{} on {} lane{}\n",
        topo.drives.len(),
        plural(topo.drives.len()),
        topo.lanes.len(),
        plural(topo.lanes.len())
    );
    for lane in &topo.lanes {
        let limit = match (&lane.kind, lane.shared_mb_s) {
            (_, Some(mb)) => format!("about {mb} MB/s shared"),
            (topology::LaneKind::Sata, None) => "each port has its own link".into(),
            (topology::LaneKind::Nvme, None) => "one drive, its own PCIe link".into(),
            _ => "depends on the card".into(),
        };
        let uplink = lane.uplink.as_ref().map(|u| format!(", PCIe {u}")).unwrap_or_default();
        println!(
            "{}  {}  {}  {limit}{uplink}",
            pad(lane.kind.label(), 8),
            pad(&short(&format!("{} {}", lane.vendor, lane.device), 46), 46),
            pad(lane.id.trim_start_matches("0000:"), 7),
        );
        let on = topo.drives_on(&lane.id);
        if on.is_empty() {
            println!("{}free", " ".repeat(10));
        }
        for d in on {
            println!("{}{}  {}", " ".repeat(10), pad(&d.name, 8), drive_line(d));
        }
    }
    let orphans: Vec<&Drive> = topo.drives.iter().filter(|d| d.lane.as_ref().is_none_or(|l| topo.lane(l).is_none())).collect();
    if !orphans.is_empty() {
        println!("Other");
        for d in orphans {
            println!("{}{}  {}", " ".repeat(10), pad(&d.name, 8), drive_line(d));
        }
    }
    let tips = topo.advice();
    if !tips.is_empty() {
        println!("\nAdvice");
        for t in tips {
            println!("  • {t}");
        }
    }
    ExitCode::SUCCESS
}

fn drive_line(d: &Drive) -> String {
    let mut bits = vec![short(&d.model, 28), format!("{} {}", human_size(d.size_bytes), d.kind_word())];
    match &d.bus {
        Bus::Sata { link: Some(l) } => bits.push(format!("link {l}")),
        Bus::Usb { port, speed_mbps, uas, .. } => {
            bits.push(format!("port {port} at {}", usb_speed(*speed_mbps)));
            bits.push(if *uas { "UAS".into() } else { "Bulk-Only".into() });
        }
        Bus::Nvme { link: Some(l) } => bits.push(format!("PCIe {l}")),
        _ => {}
    }
    if let Some(mb) = d.link_mb_s() {
        bits.push(format!("up to about {mb} MB/s"));
    }
    bits.join(" · ")
}

fn lanes_json(topo: &Topology) -> String {
    let lanes: Vec<String> = topo
        .lanes
        .iter()
        .map(|l| {
            let drives: Vec<String> = topo.drives_on(&l.id).iter().map(|d| js(&d.name)).collect();
            format!(
                "{{\"id\":{},\"kind\":{},\"vendor\":{},\"device\":{},\"shared_mb_s\":{},\"uplink\":{},\"drives\":[{}]}}",
                js(&l.id),
                js(l.kind.label()),
                js(&l.vendor),
                js(&l.device),
                opt_num(l.shared_mb_s),
                l.uplink.as_deref().map_or("null".into(), js),
                drives.join(",")
            )
        })
        .collect();
    let drives: Vec<String> = topo
        .drives
        .iter()
        .map(|d| {
            let mounts: Vec<String> = d
                .mounts
                .iter()
                .map(|m| format!("{{\"point\":{},\"fstype\":{},\"read_only\":{}}}", js(&m.point.to_string_lossy()), js(&m.fstype), m.read_only))
                .collect();
            format!(
                "{{\"name\":{},\"model\":{},\"size_bytes\":{},\"kind\":{},\"lane\":{},\"link_mb_s\":{},\"mounts\":[{}]}}",
                js(&d.name),
                js(&d.model),
                d.size_bytes,
                js(d.kind_word()),
                d.lane.as_deref().map_or("null".into(), js),
                opt_num(d.link_mb_s()),
                mounts.join(",")
            )
        })
        .collect();
    let advice: Vec<String> = topo.advice().iter().map(|t| js(t)).collect();
    format!("{{\"lanes\":[{}],\"drives\":[{}],\"advice\":[{}]}}", lanes.join(","), drives.join(","), advice.join(","))
}

// ---------------------------------------------------------------------------
// bench

struct BenchArgs {
    dirs: Vec<PathBuf>,
    mib: u64,
    secs: f64,
    keep: bool,
}

impl BenchArgs {
    fn parse(args: &[String]) -> Result<BenchArgs, String> {
        let mut b = BenchArgs { dirs: vec![], mib: 1024, secs: 4.0, keep: false };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--json" => {}
                "--keep" => b.keep = true,
                "--size" => {
                    b.mib = it.next().and_then(|v| v.parse().ok()).filter(|v| *v >= 64 && v % 4 == 0).ok_or("--size takes MiB: a multiple of 4, at least 64.")?
                }
                "--seconds" => b.secs = it.next().and_then(|v| v.parse().ok()).filter(|v: &f64| *v >= 1.0 && *v <= 120.0).ok_or("--seconds takes a number from 1 to 120.")?,
                s if s.starts_with('-') => return Err(format!("Unknown option {s}.")),
                s => b.dirs.push(PathBuf::from(s)),
            }
        }
        Ok(b)
    }
}

extern "C" fn on_interrupt(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
    // A second Ctrl+C ends the program at once.
    unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
}

fn bench_cmd(args: BenchArgs, json: bool) -> ExitCode {
    unsafe { libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t) };
    let topo = Topology::discover();
    let plan = match plan_dirs(&topo, &args.dirs).and_then(|p| check_space(p, args.mib)) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };

    let say = |s: String| {
        if !json {
            println!("{s}");
        }
    };
    say(format!("{NAME} lane bench"));
    say(format!("{} MiB test file per drive · {} s per test · reads bypass the page cache\n", args.mib, args.secs));
    for (name, dir) in &plan {
        let d = topo.drive(name);
        let lane = d.and_then(|d| d.lane.as_deref()).and_then(|l| topo.lane(l));
        let lane_s = lane.map_or("unknown lane".into(), |l| format!("{} {} {}", l.kind.label(), l.vendor, l.id.trim_start_matches("0000:")));
        say(format!("  {}  {}  {}  {}", pad(name, 8), pad(&short(&d.map_or(String::new(), |d| d.model.clone()), 20), 20), pad(&lane_s, 26), dir.display()));
    }

    say("\nWriting test files".into());
    let mut targets: Vec<Target> = Vec::new();
    let mut failed = None;
    for (name, dir) in &plan {
        let t0 = Instant::now();
        match Target::prepare(name, dir.clone(), args.mib) {
            Ok(t) => {
                say(format!("  {}  {} MiB in {:.1} s{}", pad(name, 8), args.mib, t0.elapsed().as_secs_f64(), if t.direct { "" } else { " (no direct reads on this filesystem)" }));
                targets.push(t);
            }
            Err(e) => {
                failed = Some(format!("Couldn't write the test file on {name} at {}: {e}", dir.display()));
                break;
            }
        }
        if STOP.load(Ordering::Relaxed) {
            break;
        }
    }

    let result = if let Some(msg) = failed {
        Err(msg)
    } else if STOP.load(Ordering::Relaxed) {
        Err("Stopped.".to_string())
    } else {
        measure(&targets, &args, &topo, json)
    };

    for t in &targets {
        if args.keep {
            say(format!("Kept {}", t.file.display()));
        } else {
            t.remove();
        }
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn measure(targets: &[Target], args: &BenchArgs, topo: &Topology, json: bool) -> Result<(), String> {
    let stopped = || STOP.load(Ordering::Relaxed);
    let mut solo: Vec<Vec<Measure>> = Vec::new();
    for (pi, p) in SOLO.iter().enumerate() {
        let mut row = Vec::new();
        for (ti, t) in targets.iter().enumerate() {
            if stopped() {
                return Err("Stopped.".into());
            }
            let m = bench::run(t, *p, args.secs, 0x5EED + (pi * 31 + ti) as u64).map_err(|e| format!("Reading from {} failed: {e}", t.drive))?;
            row.push(m);
        }
        solo.push(row);
    }
    let together: Option<Vec<Measure>> = if targets.len() > 1 && !stopped() {
        let mut out = Vec::new();
        for (t, r) in targets.iter().zip(bench::run_together(targets, TOGETHER, args.secs)) {
            out.push(r.map_err(|e| format!("Reading from {} with the others failed: {e}", t.drive))?);
        }
        Some(out)
    } else {
        None
    };
    if stopped() {
        return Err("Stopped.".into());
    }

    let together_idx = SOLO.iter().position(|p| p.name == TOGETHER.name).unwrap_or(0);
    if json {
        println!("{}", bench_json(targets, args, &solo, together.as_deref(), together_idx));
        return Ok(());
    }

    let w = 16;
    let mut head = format!("\n{}", pad("Each drive alone", 30));
    for t in targets {
        head.push_str(&pad(&t.drive, w));
    }
    println!("{head}");
    for (p, row) in SOLO.iter().zip(&solo) {
        let mut line = format!("  {}", pad(p.name, 28));
        for m in row {
            line.push_str(&pad(&format!("{:.0} MB/s", m.mb_s), w));
        }
        println!("{line}");
    }
    let lat_idx = SOLO.iter().position(|p| p.depth == 1).unwrap_or(0);
    let mut line = format!("  {}", pad("1 MiB wait, p50/p99", 28));
    for m in &solo[lat_idx] {
        line.push_str(&pad(&format!("{}/{}", ms(m.p50_us), ms(m.p99_us)), w));
    }
    println!("{line}");

    if let Some(tg) = &together {
        println!("\nAll drives at once: {}", TOGETHER.name);
        let mut total = 0.0;
        for (i, (t, m)) in targets.iter().zip(tg).enumerate() {
            let alone = solo[together_idx][i].mb_s.max(1e-9);
            total += m.mb_s;
            println!("  {}  {}  {:.0}% of its speed alone", pad(&t.drive, 8), pad(&format!("{:.0} MB/s", m.mb_s), 12), m.mb_s / alone * 100.0);
        }
        let best = solo[together_idx].iter().map(|m| m.mb_s).fold(0.0, f64::max).max(1e-9);
        println!("  {}  {}  {:.2}x the fastest drive alone", pad("Total", 8), pad(&format!("{total:.0} MB/s"), 12), total / best);
        let shares: Vec<String> = targets.iter().zip(tg).map(|(t, m)| format!("{} {:.0}%", t.drive, m.mb_s / total.max(1e-9) * 100.0)).collect();
        println!("\nHow {NAME} would split each read so every drive finishes together");
        println!("  {}", shares.join("  "));
        let slow: Vec<String> = targets
            .iter()
            .zip(tg)
            .enumerate()
            .filter(|(i, (_, m))| m.mb_s < solo[together_idx][*i].mb_s * 0.85)
            .map(|(_, (t, _))| t.drive.clone())
            .collect();
        if !slow.is_empty() {
            println!("  {} slowed down with the others running, so they likely share a lane or the controller's uplink.", slow.join(" and "));
        }
    }

    let mut notes = Vec::new();
    if targets.iter().any(|t| topo.drive(&t.drive).is_some_and(|d| d.rotational)) {
        notes.push(format!("Random reads stay inside each {} MiB test file, so a hard disk seeks less here than across a whole model.", args.mib));
    }
    for t in targets.iter().filter(|t| !t.direct) {
        notes.push(format!("{}'s filesystem refused direct reads, so its numbers may include some RAM caching.", t.drive));
    }
    let tips = topo.advice();
    if !notes.is_empty() || !tips.is_empty() {
        println!("\nNotes");
        for n in notes.iter().chain(&tips) {
            println!("  • {n}");
        }
    }
    println!();
    Ok(())
}

fn bench_json(targets: &[Target], args: &BenchArgs, solo: &[Vec<Measure>], together: Option<&[Measure]>, together_idx: usize) -> String {
    let m_json = |m: &Measure| format!("{{\"mb_s\":{:.1},\"p50_us\":{},\"p99_us\":{},\"reads\":{}}}", m.mb_s, m.p50_us, m.p99_us, m.reads);
    let tj: Vec<String> = targets
        .iter()
        .map(|t| format!("{{\"drive\":{},\"dir\":{},\"direct\":{}}}", js(&t.drive), js(&t.dir.to_string_lossy()), t.direct))
        .collect();
    let sj: Vec<String> = SOLO
        .iter()
        .zip(solo)
        .map(|(p, row)| {
            let cells: Vec<String> = targets.iter().zip(row).map(|(t, m)| format!("{}:{}", js(&t.drive), m_json(m))).collect();
            format!("{}:{{{}}}", js(p.name), cells.join(","))
        })
        .collect();
    let tg = together.map_or("null".into(), |tg| {
        let cells: Vec<String> = targets.iter().zip(tg).map(|(t, m)| format!("{}:{}", js(&t.drive), m_json(m))).collect();
        let total: f64 = tg.iter().map(|m| m.mb_s).sum();
        format!("{{\"pattern\":{},\"drives\":{{{}}},\"total_mb_s\":{:.1}}}", js(TOGETHER.name), cells.join(","), total)
    });
    let _ = together_idx;
    format!(
        "{{\"version\":{},\"size_mib\":{},\"seconds\":{},\"targets\":[{}],\"alone\":{{{}}},\"together\":{}}}",
        js(env!("CARGO_PKG_VERSION")),
        args.mib,
        args.secs,
        tj.join(","),
        sj.join(","),
        tg
    )
}

// ---------------------------------------------------------------------------
// serve

struct ServeArgs {
    model: PathBuf,
    mount: PathBuf,
    copies: Vec<PathBuf>,
    copy_gib: Option<u64>,
    /// Lay copies out expert by expert (one read per expert) instead of like the file.
    experts_layout: bool,
    /// Routing trace whose co-firing orders each layer's experts in a new expert copy.
    copy_order: Option<PathBuf>,
    port: u16,
    threads: usize,
    cache_mib: u64,
    socket: Option<PathBuf>,
}

impl ServeArgs {
    fn parse(args: &[String]) -> Result<ServeArgs, String> {
        let mut model = None;
        let mut mount = None;
        let mut s = ServeArgs { model: PathBuf::new(), mount: PathBuf::new(), copies: vec![], copy_gib: None, experts_layout: false, copy_order: None, port: 7714, threads: 8, cache_mib: 256, socket: None };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--mount" => mount = Some(PathBuf::from(it.next().ok_or("--mount needs a folder.")?)),
                "--copy-to" => s.copies.push(PathBuf::from(it.next().ok_or("--copy-to needs a folder on another drive.")?)),
                "--copy-gib" => s.copy_gib = Some(num(&mut it, a)?),
                "--copy-layout" => match it.next().map(String::as_str) {
                    Some("experts") => s.experts_layout = true,
                    Some("file") => s.experts_layout = false,
                    _ => return Err("--copy-layout takes experts or file.".into()),
                },
                "--copy-order" => s.copy_order = Some(PathBuf::from(it.next().ok_or("--copy-order needs a routing trace.")?)),
                "--port" => s.port = num(&mut it, a)?,
                "--threads" => s.threads = num(&mut it, a)?,
                "--cache-mib" => s.cache_mib = num(&mut it, a)?,
                "--socket" => s.socket = Some(PathBuf::from(it.next().ok_or("--socket needs a path.")?)),
                x if x.starts_with('-') => return Err(format!("Unknown option {x}.")),
                x => model = Some(PathBuf::from(x)),
            }
        }
        s.model = model.ok_or(format!("Name the model file. Example: {BIN} serve ~/models/qwen3.gguf --mount ~/corsia"))?;
        s.mount = mount.ok_or("Choose where to mount it with --mount DIR (an empty folder).")?;
        if s.copy_order.is_some() && !s.experts_layout {
            return Err("--copy-order only applies with --copy-layout experts.".into());
        }
        if !(1..=64).contains(&s.threads) {
            return Err("--threads must be 1 to 64.".into());
        }
        if s.cache_mib < 16 {
            return Err("--cache-mib must be at least 16.".into());
        }
        Ok(s)
    }
}

fn serve_cmd(args: ServeArgs) -> ExitCode {
    unsafe { libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t) };
    unsafe { libc::signal(libc::SIGTERM, on_interrupt as *const () as libc::sighandler_t) };
    let model = match std::fs::canonicalize(&args.model) {
        Ok(p) if p.is_file() => p,
        _ => return fail(&format!("{} is not a file.", args.model.display())),
    };
    let size = std::fs::metadata(&model).map(|m| m.len()).unwrap_or(0);
    if size < 16 * store::CHUNK {
        return fail("The model must be at least 64 MiB.");
    }
    let name = model.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let topo = Topology::discover();
    let primary = topo.drive_for_path(&model);
    let primary_name = primary.map_or("original".to_string(), |d| d.name.clone());

    // Expert-major copies need to know where the experts are.
    let expert_map = if args.experts_layout {
        match layout::ExpertMap::from_gguf(&model) {
            Ok(m) => Some(m),
            Err(e) => return fail(&format!("--copy-layout experts: {e}")),
        }
    } else {
        None
    };
    let order_trace = match &args.copy_order {
        Some(p) => match trace::load(p) {
            Ok(t) => Some(t),
            Err(e) => return fail(&e),
        },
        None => None,
    };

    // Speed copies, one per extra drive. An expert-major copy may also sit beside the
    // original: on a hard disk, one read per expert beats three even on the same drive.
    let mut copies = Vec::new();
    let mut used = vec![primary_name.clone()];
    for dir in &args.copies {
        let Some(d) = topo.drive_for_path(dir) else {
            return fail(&format!("Can't tell which drive {} is on. Pass a folder on a mounted drive.", dir.display()));
        };
        let beside_original = d.name == primary_name && used.iter().filter(|u| **u == d.name).count() == 1;
        if used.contains(&d.name) && !(args.experts_layout && beside_original) {
            return fail(&format!("{} is on {}, which already holds a copy or the original. A second copy on one drive adds no speed.", dir.display(), d.name));
        }
        used.push(d.name.clone());
        let free = topology::free_bytes(dir).unwrap_or(0).saturating_sub(2 << 30);
        let budget = args.copy_gib.map_or(free, |g| (g << 30).min(free));
        let max_chunks = budget / store::CHUNK;
        let opened = match &expert_map {
            Some(map) => store::CopyStore::open_experts(&d.name, dir, &name, map, max_chunks, || match &order_trace {
                Some(t) if t.layers == map.layers.len() && t.experts == map.experts() => layout::cofire_order(t, 0..t.tokens.len(), map.experts()),
                Some(_) => {
                    eprintln!("{BIN}: the trace's layers or experts don't match the model; laying experts out in id order.");
                    layout::identity_order(map)
                }
                None => layout::identity_order(map),
            })
            .map(|(c, kept)| {
                if kept && order_trace.is_some() {
                    println!("The copy on {} keeps the expert order it was started with (--copy-order applies to a new copy).", d.name);
                }
                c
            }),
            None => store::CopyStore::open(&d.name, dir, &name, size, max_chunks),
        };
        match opened {
            Ok(c) => copies.push((Arc::new(c), prior_for(d))),
            Err(e) => return fail(&format!("Couldn't set up the copy on {} at {}: {e}", d.name, dir.display())),
        }
    }

    let prior = primary.map_or(replay::DriveModel { name: primary_name.clone(), mb_s: 200.0, per_read_ms: 1.0 }, prior_for);
    let engine = match engine::Engine::new(model.clone(), &primary_name, prior, copies, ((args.cache_mib << 20) / engine::BLOCK) as usize) {
        Ok(e) => Arc::new(e),
        Err(e) => return fail(&format!("Couldn't open {}: {e}", model.display())),
    };
    println!("{NAME} is measuring the drives (read-only, a few seconds)…");
    for i in 0..engine.sources.len() {
        let _ = engine.measure(i);
    }

    if let Err(e) = std::fs::create_dir_all(&args.mount) {
        return fail(&format!("Couldn't create {}: {e}", args.mount.display()));
    }
    if std::fs::read_dir(&args.mount).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return fail(&format!("{} is not an empty folder. Pick an empty one to mount into.", args.mount.display()));
    }
    let socket = args.socket.clone().unwrap_or_else(|| hint::default_socket(&name));
    let port = match api::start(engine.clone(), args.port) {
        Ok(p) => p,
        Err(e) => return fail(&format!("Couldn't open the status API on port {}: {e}. Try --port.", args.port)),
    };
    let copier = {
        let e = engine.clone();
        std::thread::spawn(move || e.run_copier())
    };
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::NoAtime,
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::FSName(format!("corsia:{name}")),
        fuser::MountOption::Subtype("corsia".into()),
    ];
    config.n_threads = Some(args.threads);
    config.clone_fd = true;
    let session = match fuser::spawn_mount(fusefs::ModelFs::new(engine.clone()), &args.mount, &config) {
        Ok(s) => s,
        Err(e) => {
            engine.stop.store(true, Ordering::Relaxed);
            return fail(&format!("Couldn't mount at {}: {e}. Is fusermount3 installed?", args.mount.display()));
        }
    };
    // The engine socket starts after the mount so PREFETCH can store into its page cache.
    if let Err(e) = hint::start(engine.clone(), &socket, Some(session.notifier()), Some(args.mount.join(&name))) {
        engine.stop.store(true, Ordering::Relaxed);
        return fail(&format!("Couldn't open the engine socket at {}: {e}. Try --socket.", socket.display()));
    }

    println!("\n{NAME} is serving {name} ({}, {} chunks of 4 MiB)", human_size(size), engine.chunks);
    println!("  Open it from: {}", args.mount.join(&name).display());
    println!("  Engines that speak Corsia: {}  (hinted mode, no FUSE)", socket.display());
    println!("  Live status:  http://127.0.0.1:{port}/api/status  (events: /api/events)");
    for s in &engine.sources {
        let m = s.model.lock().map(|m| m.clone()).ok();
        let speed = m.map_or(String::new(), |m| format!("{:.0} MB/s, {:.1} ms per read{}", m.mb_s, m.per_read_ms, if s.measured.load(Ordering::Relaxed) { "" } else { " (estimate)" }));
        match &s.copy {
            None => println!("  {}  original · {speed}", pad(&s.drive, 6)),
            Some(c) => println!(
                "  {}  copy at {} · {} of {} chunks · {speed}",
                pad(&s.drive, 6),
                c.dir.display(),
                c.present(),
                c.max_chunks
            ),
        }
    }
    if let Some((knob, kb)) = fuse_readahead(&args.mount) {
        if kb < 1024 {
            // Measured 2026-11-22: 4 MiB readahead made the kernel send 1 MiB requests instead
            // of 128 KiB and lifted throughput 10 to 25%, with no over-fetch.
            println!("  Faster: let the kernel send 1 MiB requests (lasts until unmount, needs root):");
            println!("    echo 4096 | sudo tee {}", knob.display());
        }
    }
    println!("Press Ctrl+C to stop and unmount.\n");

    let mut last_report = std::time::Instant::now();
    let mut last_copied = 0;
    while !STOP.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if last_report.elapsed().as_secs() >= 10 {
            last_report = std::time::Instant::now();
            let copied = engine.copied_bytes.load(Ordering::Relaxed);
            if copied != last_copied {
                last_copied = copied;
                let parts: Vec<String> = engine.sources.iter().filter_map(|s| s.copy.as_ref().map(|c| format!("{} {}/{}", s.drive, c.present(), c.max_chunks))).collect();
                println!("  copies: {}", parts.join(", "));
            }
        }
    }
    println!("\nStopping…");
    engine.stop.store(true, Ordering::Relaxed);
    let _ = std::fs::remove_file(&socket);
    hint::release_resident();
    let unmounted = session.umount_and_join();
    let _ = copier.join();
    match unmounted {
        Ok(()) => {
            println!("Unmounted {}. Copies stay on their drives and are reused next time.", args.mount.display());
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("Unmounting failed: {e}. Run `fusermount3 -u {}` once the engine closes the file.", args.mount.display())),
    }
}

/// The mount's readahead setting: (its sysfs path, current KiB). FUSE mounts get their own
/// backing device named after the mount's device number.
fn fuse_readahead(mount: &Path) -> Option<(PathBuf, u64)> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(mount).ok()?.dev();
    let knob = PathBuf::from(format!("/sys/class/bdi/{}:{}/read_ahead_kb", libc::major(dev), libc::minor(dev)));
    let kb = std::fs::read_to_string(&knob).ok()?.trim().parse().ok()?;
    Some((knob, kb))
}

fn hint_prefetch_cmd(args: &[String]) -> ExitCode {
    let Some(socket) = args.first() else { return fail("Usage: corsia hint-prefetch SOCKET [--async] OFFSET:LEN...") };
    let mut ranges = Vec::new();
    let background = args.iter().any(|a| a == "--async");
    for r in args[1..].iter().filter(|a| *a != "--async") {
        let parsed = r.split_once(':').and_then(|(o, l)| Some((o.parse::<u64>().ok()?, l.parse::<u32>().ok()?)));
        match parsed {
            Some(x) => ranges.push(x),
            None => return fail(&format!("\"{r}\" is not OFFSET:LEN in bytes.")),
        }
    }
    if ranges.is_empty() {
        return fail("Give at least one OFFSET:LEN range.");
    }
    let t0 = Instant::now();
    if background {
        return match hintclient::Client::connect(Path::new(socket), 4096).and_then(|mut c| {
            c.prefetch_async(&ranges)?;
            // Stay connected briefly: closing the connection ends its background worker.
            std::thread::sleep(std::time::Duration::from_millis(500));
            Ok(())
        }) {
            Ok(()) => {
                println!("Asked in the background; returned after {:.1} ms (connection held 500 ms)", t0.elapsed().as_secs_f64() * 1e3 - 500.0);
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("Prefetch failed: {e}")),
        };
    }
    match hintclient::Client::connect(Path::new(socket), 4096).and_then(|mut c| c.prefetch(&ranges)) {
        Ok((blocks, bytes)) => {
            println!("{:.2} MiB stored in the page cache in {blocks} pieces, {:.1} ms", bytes as f64 / 1048576.0, t0.elapsed().as_secs_f64() * 1e3);
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("Prefetch failed: {e}")),
    }
}

fn hint_bench_cmd(args: &[String]) -> ExitCode {
    let (mut threads, mut batch) = (1usize, 1usize);
    let mut socket = None;
    let mut verify: Option<PathBuf> = None;
    let mut it = args.iter();
    let parsed: Result<(), String> = (|| {
        while let Some(a) = it.next() {
            match a.as_str() {
                "--threads" => threads = num(&mut it, a)?,
                "--batch" => batch = num(&mut it, a)?,
                "--verify" => verify = Some(PathBuf::from(it.next().ok_or("--verify needs the original model file.")?)),
                x if x.starts_with('-') => return Err(format!("Unknown option {x}.")),
                x => socket = Some(PathBuf::from(x)),
            }
        }
        Ok(())
    })();
    if let Err(e) = parsed {
        return fail(&e);
    }
    let Some(socket) = socket else { return fail("Name the engine socket that `corsia serve` printed.") };
    if !(1..=64).contains(&threads) || !(1..=64).contains(&batch) {
        return fail("--threads and --batch must be 1 to 64.");
    }
    match hintclient::bench(&socket, threads, batch, verify.as_deref()) {
        Ok(r) => {
            println!(
                "{:.0} MB/s ({} MiB in {:.1} s, {} ranges, {} per request, {} reader{}, {}% started on a copy)",
                r.mb_s,
                r.bytes >> 20,
                r.secs,
                r.ranges,
                batch,
                threads,
                plural(threads),
                r.from_copies * 100 / r.ranges.max(1)
            );
            if verify.is_some() {
                if r.mismatches == 0 {
                    println!("All {} ranges matched the original byte for byte.", r.ranges);
                } else {
                    return fail(&format!("{} of {} ranges did not match the original.", r.mismatches, r.ranges));
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("Hinted read failed: {e}")),
    }
}

/// A starting guess for a drive's speed, until it has been measured.
fn prior_for(d: &Drive) -> replay::DriveModel {
    let link = d.link_mb_s().unwrap_or(200) as f64;
    let (mb_s, per_read_ms) = match d.kind_word() {
        "hard disk" => (link.min(110.0), 6.0),
        "flash drive" | "memory card" => (link.min(30.0), 1.5),
        "NVMe SSD" => (link, 0.1),
        _ => (link, 0.3),
    };
    replay::DriveModel { name: d.name.clone(), mb_s, per_read_ms }
}

/// One test folder per drive: the folders given, or one picked on every mounted drive.
fn plan_dirs(topo: &Topology, dirs: &[PathBuf]) -> Result<Vec<(String, PathBuf)>, String> {
    let mut plan: Vec<(String, PathBuf)> = Vec::new();
    if dirs.is_empty() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        for d in &topo.drives {
            if let Some(dir) = pick_dir(d, home.as_deref()) {
                plan.push((d.name.clone(), dir));
            }
        }
    } else {
        for dir in dirs {
            let d = topo
                .drive_for_path(dir)
                .ok_or_else(|| format!("Can't tell which drive {} is on. Pass a folder on a mounted drive.", dir.display()))?;
            if plan.iter().any(|(n, _)| n == &d.name) {
                return Err(format!("Two folders are on {}. Pass one folder per drive.", d.name));
            }
            plan.push((d.name.clone(), dir.join(BENCH_DIR)));
        }
    }
    if plan.is_empty() {
        return Err("No writable folder found on any drive. Mount a drive or pass a folder.".into());
    }
    Ok(plan)
}

fn check_space(plan: Vec<(String, PathBuf)>, mib: u64) -> Result<Vec<(String, PathBuf)>, String> {
    let need = (mib << 20) + (64 << 20);
    for (name, dir) in &plan {
        let probe = existing_parent(dir);
        // A test file left from an earlier --keep run is reused, so it needs no new space.
        let reuse = dir.join(format!("corsia-test-{mib}m.bin")).metadata().map(|m| m.len() == mib << 20).unwrap_or(false);
        if !reuse && topology::free_bytes(&probe).unwrap_or(0) < need {
            return Err(format!("{name} needs {} MiB free at {} for its test file. Free some space or pick a smaller size.", need >> 20, probe.display()));
        }
    }
    Ok(plan)
}

fn num<T: std::str::FromStr>(it: &mut std::slice::Iter<String>, name: &str) -> Result<T, String> {
    it.next().and_then(|v| v.parse().ok()).ok_or_else(|| format!("{name} needs a number."))
}

// ---------------------------------------------------------------------------
// trace

fn trace_cmd(args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) != Some("synth") {
        return fail(&format!("Usage: {BIN} trace synth FILE [options]. Run `{BIN} --help` for the options."));
    }
    let mut p = trace::SynthParams::default();
    let mut out: Option<PathBuf> = None;
    let mut it = args[1..].iter();
    let parsed: Result<(), String> = (|| {
        while let Some(a) = it.next() {
            match a.as_str() {
                "--tokens" => p.tokens = num(&mut it, a)?,
                "--layers" => p.layers = num(&mut it, a)?,
                "--experts" => p.experts = num(&mut it, a)?,
                "--topk" => p.topk = num(&mut it, a)?,
                "--skew" => p.skew = num(&mut it, a)?,
                "--repeat" => p.repeat = num(&mut it, a)?,
                "--affinity" => p.affinity = num(&mut it, a)?,
                "--seed" => p.seed = num(&mut it, a)?,
                s if s.starts_with('-') => return Err(format!("Unknown option {s}.")),
                s => out = Some(PathBuf::from(s)),
            }
        }
        Ok(())
    })();
    if let Err(e) = parsed {
        return fail(&e);
    }
    let Some(out) = out else { return fail("Name the file to write, for example: corsia trace synth qwen3-30b.trace") };
    if p.tokens == 0 || p.layers == 0 || p.experts == 0 || p.experts > u16::MAX as usize || p.topk == 0 || !(0.0..=1.0).contains(&p.repeat) || !(0.0..=1.0).contains(&p.affinity) {
        return fail("Tokens, layers, experts and topk must be above 0, experts at most 65535, and --repeat and --affinity between 0 and 1.");
    }
    let t = trace::synth(&p);
    if let Err(e) = trace::save(&t, &out) {
        return fail(&format!("Couldn't write {}: {e}", out.display()));
    }
    println!(
        "Wrote {} tokens to {}: {} layers × {} experts, top {}, popularity skew {}, repeat {}, affinity {} (synthetic).",
        t.tokens.len(),
        out.display(),
        t.layers,
        t.experts,
        t.topk,
        p.skew,
        p.repeat,
        p.affinity
    );
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// predict

fn predict_cmd(args: &[String]) -> ExitCode {
    let (mut ahead, mut pick, mut warmup) = (3usize, 8usize, 64usize);
    let mut path: Option<PathBuf> = None;
    let mut it = args.iter();
    let parsed: Result<(), String> = (|| {
        while let Some(a) = it.next() {
            match a.as_str() {
                "--ahead" => ahead = num(&mut it, a)?,
                "--pick" => pick = num(&mut it, a)?,
                "--warmup" => warmup = num(&mut it, a)?,
                s if s.starts_with('-') => return Err(format!("Unknown option {s}.")),
                s => path = Some(PathBuf::from(s)),
            }
        }
        Ok(())
    })();
    if let Err(e) = parsed {
        return fail(&e);
    }
    let Some(path) = path else { return fail(&format!("Name a trace file. Make one with `{BIN} trace synth FILE`.")) };
    if !(1..=8).contains(&ahead) || pick == 0 {
        return fail("--ahead must be 1 to 8 and --pick above 0.");
    }
    let trace = match trace::load(&path) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };
    if trace.tokens.len() <= warmup {
        return fail(&format!("The trace has {} tokens, not more than the {warmup} warm-up tokens.", trace.tokens.len()));
    }
    let results = predict::evaluate(&trace, ahead, pick, warmup);
    println!("{NAME} predictor · {} ({} tokens, {} after warm-up)", trace.source, trace.tokens.len(), trace.tokens.len() - warmup);
    println!("Share of each layer's {} experts named in advance among {pick} guesses\n", trace.topk);
    let mut head = format!("  {}", pad("", 18));
    for d in 1..=ahead {
        head.push_str(&pad(&format!("{d} ahead"), 10));
    }
    println!("{head}");
    for r in &results {
        let mut line = format!("  {}", pad(r.name, 18));
        for v in &r.recall {
            line.push_str(&pad(&format!("{:.0}%", v * 100.0), 10));
        }
        println!("{line}");
    }
    let chance = pick as f64 / trace.experts as f64 * 100.0;
    println!("\n  Random guessing would score about {chance:.0}%.");
    if trace.source.starts_with("synthetic") {
        println!("  The trace is synthetic: its affinity setting decides how much \"Affinity only\" can find. Real routing needs a real trace.");
    }
    println!();
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// replay

struct ReplayArgs {
    trace: PathBuf,
    dirs: Vec<PathBuf>,
    expert_kib: u64,
    cache_mib: u64,
    tokens: usize,
    warmup: usize,
    span_mib: u64,
    ahead: usize,
    pick: usize,
    only: Option<Vec<String>>,
    compute_ms: f64,
    keep: bool,
    /// Disk model instead of real drives: seeks per token for each copy layout.
    disk_model: bool,
    /// A GGUF to take the real expert offsets from (implies the disk model).
    model: Option<PathBuf>,
    /// Trace the co-firing order is learned from, and how many of its tokens.
    order_from: Option<PathBuf>,
    order_tokens: Option<usize>,
    seek_ms: f64,
    disk_mb_s: f64,
    /// A measured s/token for the file layout, to fit the seek cost to.
    measured_s: Option<f64>,
}

impl ReplayArgs {
    fn parse(args: &[String]) -> Result<ReplayArgs, String> {
        // Qwen3-30B-A3B at Q4_K_M: about 17.7 GB of experts over 48 × 128 of them.
        let mut r = ReplayArgs {
            trace: PathBuf::new(),
            dirs: vec![],
            expert_kib: 2816,
            cache_mib: 4096,
            tokens: 24,
            warmup: 64,
            span_mib: 2048,
            ahead: 2,
            pick: 8,
            only: None,
            compute_ms: 0.0,
            keep: false,
            disk_model: false,
            model: None,
            order_from: None,
            order_tokens: None,
            seek_ms: 10.0,
            disk_mb_s: 100.0,
            measured_s: None,
        };
        let mut it = args.iter();
        let mut positional = Vec::new();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--json" => {}
                "--keep" => r.keep = true,
                "--expert-kib" => r.expert_kib = num(&mut it, a)?,
                "--cache-mib" => r.cache_mib = num(&mut it, a)?,
                "--tokens" => r.tokens = num(&mut it, a)?,
                "--warmup" => r.warmup = num(&mut it, a)?,
                "--span-mib" => r.span_mib = num(&mut it, a)?,
                "--ahead" => r.ahead = num(&mut it, a)?,
                "--pick" => r.pick = num(&mut it, a)?,
                "--compute-ms" => r.compute_ms = num(&mut it, a)?,
                "--disk-model" => r.disk_model = true,
                "--model" => {
                    r.model = Some(PathBuf::from(it.next().ok_or("--model needs a GGUF file.")?));
                    r.disk_model = true;
                }
                "--order-from" => r.order_from = Some(PathBuf::from(it.next().ok_or("--order-from needs a routing trace.")?)),
                "--order-tokens" => r.order_tokens = Some(num(&mut it, a)?),
                "--seek-ms" => r.seek_ms = num(&mut it, a)?,
                "--disk-mb-s" => r.disk_mb_s = num(&mut it, a)?,
                "--measured-s" => r.measured_s = Some(num(&mut it, a)?),
                "--only" => r.only = Some(it.next().ok_or("--only needs strategy keys, for example one,predicted.")?.split(',').map(|s| s.trim().to_string()).collect()),
                s if s.starts_with('-') => return Err(format!("Unknown option {s}.")),
                s => positional.push(PathBuf::from(s)),
            }
        }
        if positional.is_empty() {
            return Err(format!("Name a trace file. Make one with `{BIN} trace synth FILE`."));
        }
        r.trace = positional.remove(0);
        r.dirs = positional;
        if r.expert_kib < 4 || r.tokens == 0 || r.span_mib < 64 || r.span_mib % 4 != 0 {
            return Err("--expert-kib must be at least 4, --tokens above 0, --span-mib a multiple of 4 and at least 64.".into());
        }
        if r.span_mib << 10 < r.expert_kib * 4 {
            return Err("--span-mib must hold at least four experts.".into());
        }
        if !(1..=8).contains(&r.ahead) || r.pick == 0 || r.pick > 255 {
            return Err("--ahead must be 1 to 8 and --pick 1 to 255.".into());
        }
        if !(r.seek_ms >= 0.0 && r.disk_mb_s > 0.0) {
            return Err("--seek-ms must be 0 or more and --disk-mb-s above 0.".into());
        }
        if !(0.0..=60000.0).contains(&r.compute_ms) {
            return Err("--compute-ms takes milliseconds of CPU time per token, 0 to 60000.".into());
        }
        Ok(r)
    }
}

fn replay_cmd(args: ReplayArgs, json: bool) -> ExitCode {
    unsafe { libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t) };
    let trace = match trace::load(&args.trace) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };
    if trace.tokens.len() <= args.warmup {
        return fail(&format!("The trace has {} tokens, not more than the {} warm-up tokens. Use a longer trace or --warmup.", trace.tokens.len(), args.warmup));
    }
    if args.disk_model {
        return match seek_model_cmd(&args, &trace, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&e),
        };
    }
    let topo = Topology::discover();
    let plan = match plan_dirs(&topo, &args.dirs).and_then(|p| check_space(p, args.span_mib)) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let say = |s: String| {
        if !json {
            println!("{s}");
        }
    };
    let expert_bytes = args.expert_kib << 10;
    let total_experts = trace.layers * trace.experts;
    let cache_experts = ((args.cache_mib << 20) / replay::align_up(expert_bytes)) as usize;

    say(format!("{NAME} replay bench"));
    say(format!(
        "Trace: {} layers × {} experts, top {} · {} ({} tokens)",
        trace.layers,
        trace.experts,
        trace.topk,
        trace.source,
        trace.tokens.len()
    ));
    say(format!(
        "Expert {:.2} MiB · all experts {:.1} GiB · RAM cache {} MiB ({} experts, {:.0}% of them)\n",
        expert_bytes as f64 / 1048576.0,
        (expert_bytes * total_experts as u64) as f64 / 1073741824.0,
        args.cache_mib,
        cache_experts.min(total_experts),
        cache_experts.min(total_experts) as f64 / total_experts as f64 * 100.0
    ));

    say("Preparing drives".into());
    let mut targets: Vec<Target> = Vec::new();
    for (name, dir) in &plan {
        let t0 = Instant::now();
        match Target::prepare(name, dir.clone(), args.span_mib) {
            Ok(t) if !t.direct => {
                t.remove();
                cleanup(&targets, args.keep);
                return fail(&format!("{name}'s filesystem refuses direct reads, so its numbers would include RAM caching. Pick a folder on another filesystem."));
            }
            Ok(t) => {
                say(format!("  {}  {} MiB span file ready in {:.1} s · {}", pad(name, 8), args.span_mib, t0.elapsed().as_secs_f64(), dir.display()));
                targets.push(t);
            }
            Err(e) => {
                cleanup(&targets, args.keep);
                return fail(&format!("Couldn't write the span file on {name} at {}: {e}", dir.display()));
            }
        }
        if STOP.load(Ordering::Relaxed) {
            cleanup(&targets, args.keep);
            return fail("Stopped.");
        }
    }
    let result = run_replay(&targets, &trace, &args, expert_bytes, cache_experts, json);
    cleanup(&targets, args.keep);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// `corsia replay TRACE --disk-model`: seeks and reads per token for the file's layout and
/// for an expert-major copy (in id order and in co-firing order), on a hard disk modelled
/// as a fixed seek plus bytes over bandwidth.
fn seek_model_cmd(args: &ReplayArgs, trace: &trace::Trace, json: bool) -> Result<(), String> {
    let map = match &args.model {
        Some(p) => layout::ExpertMap::from_gguf(p)?,
        None => layout::ExpertMap::synthetic(trace.layers, trace.experts as u64, (args.expert_kib << 10) / 3),
    };
    if map.layers.len() != trace.layers || map.experts() != trace.experts {
        return Err(format!(
            "The trace has {} layers of {} experts, the model {} of {}.",
            trace.layers,
            trace.experts,
            map.layers.len(),
            map.experts()
        ));
    }
    let avg_expert = (0..map.layers.len()).map(|l| map.expert_bytes(l)).sum::<u64>() / map.layers.len().max(1) as u64;
    let cache_experts = ((args.cache_mib << 20) / avg_expert.max(1)) as usize;
    let misses = replay::simulate_cache(trace, cache_experts, args.warmup, args.tokens);
    if misses.per_token.is_empty() {
        return Err("No tokens left to replay after the warm-up.".into());
    }
    let replayed = args.warmup..args.warmup + misses.per_token.len();

    // The co-firing order: by default learned from the warm-up tokens only, so the
    // replayed tokens are ones the order has not seen.
    let (order_trace, order_label) = match &args.order_from {
        Some(p) => (trace::load(p)?, p.display().to_string()),
        None => (trace::load(&args.trace)?, format!("{} (warm-up tokens)", args.trace.display())),
    };
    let order_n = args.order_tokens.unwrap_or(if args.order_from.is_some() { order_trace.tokens.len() } else { args.warmup }).min(order_trace.tokens.len());
    if order_n == 0 {
        return Err("No tokens to learn the expert order from: raise --warmup or pass --order-from.".into());
    }
    if order_trace.layers != trace.layers || order_trace.experts != trace.experts {
        return Err("The order trace has a different shape from the replayed one.".into());
    }
    let id = layout::identity_order(&map);
    let co = layout::cofire_order(&order_trace, 0..order_n, trace.experts);
    let id_lay = layout::ExpertLayout::new(&map, id.clone())?;
    let co_lay = layout::ExpertLayout::new(&map, co.clone())?;

    let disk = replay::DriveModel { name: "hard disk".into(), mb_s: args.disk_mb_s, per_read_ms: args.seek_ms };
    let run = |disk: &replay::DriveModel| {
        vec![
            replay::seek_replay("File layout (today)", "file", &misses, trace.experts, &map, None, disk),
            replay::seek_replay("Expert-major copy, id order", "experts_id", &misses, trace.experts, &map, Some(&id_lay), disk),
            replay::seek_replay("Expert-major copy, co-firing order", "experts_cofire", &misses, trace.experts, &map, Some(&co_lay), disk),
        ]
    };
    let results = run(&disk);
    // Fit the seek cost so the file layout matches a measured s/token, when given.
    // The read-through gap depends on the seek cost, so refit a few times until it settles.
    let fitted = args.measured_s.and_then(|m| {
        let mut f = results[0].clone();
        let mut d = None;
        for _ in 0..6 {
            let seek_ms = (m - f.transfer_s) / f.reads.max(1e-9) * 1e3;
            if seek_ms <= 0.0 {
                return None;
            }
            let model = replay::DriveModel { name: "fitted".into(), mb_s: args.disk_mb_s, per_read_ms: seek_ms };
            f = replay::seek_replay("", "file", &misses, trace.experts, &map, None, &model);
            d = Some(model);
        }
        d
    });
    let fitted_results = fitted.as_ref().map(run);
    let per_layer = trace.layers.max(1) as f64;
    let adj_id = layout::adjacency(trace, replayed.clone(), &id) / per_layer;
    let adj_co = layout::adjacency(trace, replayed.clone(), &co) / per_layer;

    if json {
        let row = |r: &replay::SeekResult| {
            format!(
                "{{\"key\":{},\"experts\":{:.2},\"requests\":{:.2},\"reads\":{:.2},\"mb\":{:.2},\"seek_s\":{:.3},\"transfer_s\":{:.3},\"s_per_token\":{:.3}}}",
                js(&r.key),
                r.experts,
                r.requests,
                r.reads,
                r.mb,
                r.seek_s,
                r.transfer_s,
                r.s_per_token()
            )
        };
        let rows: Vec<String> = results.iter().map(row).collect();
        let fit = match (&fitted, &fitted_results) {
            (Some(d), Some(rs)) => format!("{{\"seek_ms\":{:.3},\"layouts\":[{}]}}", d.per_read_ms, rs.iter().map(row).collect::<Vec<_>>().join(",")),
            _ => "null".into(),
        };
        println!(
            "{{\"trace\":{},\"tokens\":{},\"cache_experts\":{},\"hit_rate\":{:.4},\"disk\":{{\"seek_ms\":{},\"mb_s\":{},\"read_through\":{}}},\"order\":{{\"from\":{},\"tokens\":{},\"adjacent_pairs_id\":{:.3},\"adjacent_pairs_cofire\":{:.3}}},\"copy_bytes\":{},\"model_bytes\":{},\"layouts\":[{}],\"fitted\":{}}}",
            js(&args.trace.display().to_string()),
            misses.per_token.len(),
            misses.cache_experts,
            misses.hit_rate,
            args.seek_ms,
            args.disk_mb_s,
            replay::read_through(&disk),
            js(&order_label),
            order_n,
            adj_id,
            adj_co,
            co_lay.space,
            map.file_size,
            rows.join(","),
            fit
        );
        return Ok(());
    }

    println!("{NAME} replay: disk model");
    println!("Trace: {} layers × {} experts, top {} · {} · replaying tokens {}..{}", trace.layers, trace.experts, trace.topk, trace.source, replayed.start, replayed.end);
    println!(
        "Model: {} · {:.2} MiB per expert on average · RAM cache {} MiB ({} experts, {:.0}% hits)",
        args.model.as_ref().map_or("synthetic (from --expert-kib)".to_string(), |p| p.display().to_string()),
        avg_expert as f64 / 1048576.0,
        args.cache_mib,
        misses.cache_experts,
        misses.hit_rate * 100.0
    );
    println!(
        "Disk: {} ms per seek, {} MB/s, gaps under {} KiB read through",
        args.seek_ms,
        args.disk_mb_s,
        replay::read_through(&disk) >> 10
    );
    println!(
        "Expert order learned from {order_label}, {order_n} tokens · of a token's experts in a layer, {adj_co:.2} pairs sit side by side (id order {adj_id:.2})"
    );
    println!(
        "Copy size: {:.2} GiB for a {:.2} GiB model (pages two slices share are stored twice)\n",
        co_lay.space as f64 / 1073741824.0,
        map.file_size as f64 / 1073741824.0
    );
    let table = |rs: &[replay::SeekResult]| {
        println!("{:<38} {:>8} {:>8} {:>7} {:>8} {:>7} {:>9} {:>9}", "Layout", "experts", "pieces", "reads", "MB", "seek s", "transfer", "s/token");
        for r in rs {
            println!(
                "{:<38} {:>8.1} {:>8.1} {:>7.1} {:>8.1} {:>7.2} {:>9.2} {:>9.2}",
                r.label,
                r.experts,
                r.requests,
                r.reads,
                r.mb,
                r.seek_s,
                r.transfer_s,
                r.s_per_token()
            );
        }
        let base = rs[0].s_per_token();
        let best = rs.iter().map(|r| r.s_per_token()).fold(f64::INFINITY, f64::min);
        println!("Best: {:.2}x faster than the file layout.", base / best.max(1e-9));
    };
    println!("Per token (experts fetched, pieces asked for, reads after merging):");
    table(&results);
    match (&fitted, &fitted_results, args.measured_s) {
        (Some(d), Some(rs), Some(m)) => {
            println!("\nFitted to the measured {m} s/token for the file layout: {:.2} ms per read at {} MB/s", d.per_read_ms, args.disk_mb_s);
            table(rs);
        }
        (_, _, Some(m)) => println!(
            "\nThe measured {m} s/token is less than the transfer time alone ({:.2} s) at {} MB/s: the disk must be faster than that. Raise --disk-mb-s.",
            results[0].transfer_s, args.disk_mb_s
        ),
        _ => {}
    }
    println!("\nThe disk model counts reads and bytes; real disks also queue and reorder reads, so treat the ratios as the result and the seconds as a guide.");
    Ok(())
}

fn cleanup(targets: &[Target], keep: bool) {
    for t in targets {
        if keep {
            println!("Kept {}", t.file.display());
        } else {
            t.remove();
        }
    }
}

fn run_replay(targets: &[Target], trace: &trace::Trace, args: &ReplayArgs, expert_bytes: u64, cache_experts: usize, json: bool) -> Result<(), String> {
    let say = |s: String| {
        if !json {
            println!("{s}");
        }
    };
    say("\nMeasuring each drive for expert reads".into());
    let mut models: Vec<(replay::DriveModel, usize)> = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let m = replay::measure_drive(t, 2.0).map_err(|e| format!("Measuring {} failed: {e}", t.drive))?;
        say(format!("  {}  {:.0} MB/s · {:.1} ms fixed cost per read", pad(&m.name, 8), m.mb_s, m.per_read_ms));
        models.push((m, i));
    }
    // Fastest first: the one-drive strategy uses drive 0.
    models.sort_by(|a, b| b.0.mb_s.total_cmp(&a.0.mb_s));
    let files: Vec<std::fs::File> = models.iter().map(|(_, i)| targets[*i].open()).collect::<Result<_, _>>().map_err(|e| format!("Opening a span file failed: {e}"))?;
    let spans: Vec<u64> = models.iter().map(|(_, i)| targets[*i].file_bytes).collect();
    let models: Vec<replay::DriveModel> = models.into_iter().map(|(m, _)| m).collect();

    let misses = replay::simulate_cache(trace, cache_experts, args.warmup, args.tokens);
    let miss_per_token = misses.per_token.iter().map(|t| t.iter().map(|l| l.len()).sum::<usize>()).sum::<usize>() as f64 / misses.per_token.len().max(1) as f64;
    say(format!(
        "\nRAM cache after {} warm-up tokens: {:.0}% of expert uses hit · {:.0} of {:.0} experts per token come from the drives ({:.0} MiB)",
        args.warmup,
        misses.hit_rate * 100.0,
        miss_per_token,
        misses.needed_per_token,
        miss_per_token * replay::align_up(expert_bytes) as f64 / 1048576.0
    ));

    let predicted = replay::Strategy::Predicted { ahead: args.ahead as u8, pick: args.pick as u8 };
    let mut strategies: Vec<replay::Strategy> =
        if files.len() == 1 { vec![replay::Strategy::One, replay::Strategy::OneAhead(u8::MAX)] } else { replay::Strategy::ALL.to_vec() };
    strategies.push(predicted);
    if let Some(only) = &args.only {
        let unknown: Vec<&String> = only.iter().filter(|k| !strategies.iter().any(|s| &s.key() == *k)).collect();
        if !unknown.is_empty() {
            return Err(format!("Unknown strategy {}. Keys: {}.", unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "), strategies.iter().map(|s| s.key()).collect::<Vec<_>>().join(", ")));
        }
        strategies.retain(|s| only.contains(&s.key()));
    }
    let fastest = models[0].name.clone();
    let mut results = Vec::new();
    let mut predict_stats = None;
    let per_layer = args.compute_ms / trace.layers.max(1) as f64;
    if args.compute_ms > 0.0 {
        say(format!("\nReplaying {} tokens per strategy, with {} ms of compute per token", misses.per_token.len(), args.compute_ms));
    } else {
        say(format!("\nReplaying {} tokens per strategy, drives only (no compute)", misses.per_token.len()));
    }
    say(format!("  {}{}{}{}", pad("Strategy", 34), pad("Tokens/s", 10), pad("MB/s", 8), "Per token"));
    for s in &strategies {
        let run = if *s == predicted {
            replay::run_predicted(&files, &spans, &models, expert_bytes, trace, cache_experts, args.warmup, args.tokens, args.ahead, args.pick, per_layer).map(|(r, st)| {
                predict_stats = Some(st);
                r
            })
        } else {
            replay::run_strategy(*s, &files, &spans, &models, expert_bytes, &misses, per_layer)
        };
        let r = run.map_err(|e| format!("Replaying \"{}\" failed: {e}", s.label(&fastest)))?;
        let share: Vec<String> = models
            .iter()
            .zip(&r.drive_bytes)
            .filter(|(_, b)| **b > 0)
            .map(|(m, b)| format!("{} {:.0}%", m.name, *b as f64 / r.bytes.max(1) as f64 * 100.0))
            .collect();
        say(format!(
            "  {}{}{}{}  {}",
            pad(&s.label(&fastest), 34),
            pad(&format!("{:.2}", r.tokens_per_s()), 10),
            pad(&format!("{:.0}", r.mb_s()), 8),
            pad(&format!("{:.0} ms", r.p50_token_ms), 9),
            share.join(", ")
        ));
        results.push(r);
    }
    if let Some(st) = &predict_stats {
        say(format!(
            "\nPredicted prefetch: {} reads guessed ahead, {} of them used ({:.0}%), {} arrived late; {} reads on demand; cache hits {:.0}%",
            st.prefetched,
            st.useful,
            st.useful as f64 / st.prefetched.max(1) as f64 * 100.0,
            st.late,
            st.demand_reads,
            st.hit_rate * 100.0
        ));
    }

    if json {
        let rows: Vec<String> = results
            .iter()
            .map(|r| {
                let drives: Vec<String> = models.iter().zip(&r.drive_bytes).map(|(m, b)| format!("{}:{}", js(&m.name), b)).collect();
                format!(
                    "{{\"strategy\":{},\"tokens\":{},\"seconds\":{:.3},\"tokens_per_s\":{:.3},\"mb_s\":{:.1},\"p50_token_ms\":{:.1},\"reads\":{},\"drive_bytes\":{{{}}}}}",
                    js(&r.strategy.key()),
                    r.tokens,
                    r.secs,
                    r.tokens_per_s(),
                    r.mb_s(),
                    r.p50_token_ms,
                    r.reads,
                    drives.join(",")
                )
            })
            .collect();
        let drives: Vec<String> = models.iter().map(|m| format!("{{\"drive\":{},\"mb_s\":{:.1},\"per_read_ms\":{:.2}}}", js(&m.name), m.mb_s, m.per_read_ms)).collect();
        println!(
            "{{\"version\":{},\"trace\":{{\"source\":{},\"layers\":{},\"experts\":{},\"topk\":{}}},\"expert_bytes\":{},\"cache_experts\":{},\"hit_rate\":{:.4},\"experts_read_per_token\":{:.1},\"drives\":[{}],\"results\":[{}]}}",
            js(env!("CARGO_PKG_VERSION")),
            js(&trace.source),
            trace.layers,
            trace.experts,
            trace.topk,
            expert_bytes,
            misses.cache_experts,
            misses.hit_rate,
            miss_per_token,
            drives.join(","),
            rows.join(",")
        );
        return Ok(());
    }

    let find = |s: replay::Strategy| results.iter().find(|r| r.strategy == s).map(|r| r.tokens_per_s());
    use replay::Strategy as S;
    if let (Some(one), Some(sched)) = (find(S::One), find(S::Scheduled)) {
        println!("\nWithout prediction: best multi-drive {:.2} tokens/s vs one drive {:.2} ({:.2}x).", sched.max(find(S::Whole).unwrap_or(0.0)).max(find(S::Sliced).unwrap_or(0.0)), one, sched.max(find(S::Whole).unwrap_or(0.0)).max(find(S::Sliced).unwrap_or(0.0)) / one.max(1e-9));
        if let (Some(one_t), Some(multi_t)) = (find(S::OneAhead(u8::MAX)), find(S::Ahead(u8::MAX))) {
            println!("Whole token known: {} drives {:.2} tokens/s vs one drive {:.2} ({:.2}x). Prediction alone: {:.2}x.", models.len(), multi_t, one_t, multi_t / one_t.max(1e-9), one_t / one.max(1e-9));
        }
        println!("Strategies that pick the soonest drive need a copy of every expert on every drive.");
    }
    println!("\nNotes");
    if trace.source.starts_with("synthetic") {
        println!("  • The trace is synthetic. Real routing may hit the cache more or less often; record one on the target machine to be sure.");
    }
    if args.compute_ms > 0.0 {
        println!("  • Compute is simulated ({} ms per token). Only the predicted strategy overlaps reads with it; the others read, then compute.", args.compute_ms);
    } else {
        println!("  • Drives only: compute time is not included, so these are ceilings. Reads for a layer start only when the layer before is done, as in an engine without prediction.");
    }
    println!("  • Experts sit in a {} MiB span per drive, so a hard disk seeks less than it would across a whole model.", args.span_mib);
    println!();
    Ok(())
}

/// A writable folder on this drive: under the home folder when home lives here, else the
/// roomiest writable mount.
fn pick_dir(d: &Drive, home: Option<&Path>) -> Option<PathBuf> {
    let usable: Vec<&topology::Mount> = d
        .mounts
        .iter()
        .filter(|m| !m.read_only)
        .filter(|m| {
            let p = m.point.to_string_lossy();
            !["/boot", "/efi", "/snap", "/var/lib/docker", "/var/snap"].iter().any(|x| p.starts_with(x))
        })
        .collect();
    if let Some(home) = home {
        if let Some(m) = usable.iter().filter(|m| home.starts_with(&m.point)).max_by_key(|m| m.point.as_os_str().len()) {
            let _ = m;
            return Some(home.join(".cache").join("corsia-bench"));
        }
    }
    usable
        .into_iter()
        .filter(|m| topology::writable(&m.point))
        .max_by_key(|m| topology::free_bytes(&m.point).unwrap_or(0))
        .map(|m| m.point.join(BENCH_DIR))
}

fn existing_parent(p: &Path) -> PathBuf {
    let mut p = p.to_path_buf();
    while !p.exists() {
        if !p.pop() {
            return PathBuf::from("/");
        }
    }
    p
}

// ---------------------------------------------------------------------------
// Formatting

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { format!("{s} ") } else { format!("{s}{}", " ".repeat(w - n)) }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn human_size(b: u64) -> String {
    let gb = b as f64 / 1e9;
    if gb >= 1000.0 { format!("{:.1} TB", gb / 1000.0) } else if gb >= 10.0 { format!("{gb:.0} GB") } else { format!("{gb:.1} GB") }
}

fn usb_speed(mbps: u32) -> String {
    if mbps >= 1000 { format!("{} Gb/s", mbps / 1000) } else { format!("{mbps} Mb/s") }
}

fn ms(us: u32) -> String {
    format!("{:.1} ms", us as f64 / 1000.0)
}

fn opt_num(v: Option<u32>) -> String {
    v.map_or("null".into(), |n| n.to_string())
}

fn js(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
