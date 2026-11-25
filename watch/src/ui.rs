//! Drawing. Neutral text, one accent (the Vorynza teal, until Corsia has a hue of its own)
//! for progress, selection and live state. Regions are separated by space and a quiet
//! fill, not boxes. Every status is a glyph plus a word, never colour alone.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use serde_json::Value;

use crate::daemon::{Daemon, Link};
use crate::model::Model;
use crate::runs::{Run, Status, Store, now};

pub struct Theme {
    pub accent: Color,
    pub text: Color,
    pub muted: Color,
    pub faint: Color,
    pub fill: Color,
    pub good: Color,
    pub bad: Color,
    pub warn: Color,
}

impl Theme {
    pub fn dark() -> Theme {
        Theme {
            accent: Color::Rgb(0x2e, 0xc4, 0xcc),
            text: Color::Rgb(0xe6, 0xe8, 0xea),
            muted: Color::Rgb(0x9a, 0xa3, 0xab),
            faint: Color::Rgb(0x78, 0x81, 0x89),
            fill: Color::Rgb(0x1b, 0x20, 0x24),
            good: Color::Rgb(0x4a, 0xd6, 0x8f),
            bad: Color::Rgb(0xf0, 0x6e, 0x6e),
            warn: Color::Rgb(0xe8, 0xb3, 0x4b),
        }
    }

    pub fn light() -> Theme {
        Theme {
            accent: Color::Rgb(0x08, 0x7a, 0x82),
            text: Color::Rgb(0x16, 0x1a, 0x1d),
            muted: Color::Rgb(0x55, 0x5e, 0x66),
            faint: Color::Rgb(0x6a, 0x72, 0x7a),
            fill: Color::Rgb(0xf0, 0xf2, 0xf4),
            good: Color::Rgb(0x16, 0x80, 0x4c),
            bad: Color::Rgb(0xc2, 0x33, 0x33),
            warn: Color::Rgb(0x96, 0x5c, 0x00),
        }
    }

    pub fn plain() -> Theme {
        Theme {
            accent: Color::Reset,
            text: Color::Reset,
            muted: Color::Reset,
            faint: Color::Reset,
            fill: Color::Reset,
            good: Color::Reset,
            bad: Color::Reset,
            warn: Color::Reset,
        }
    }
}

pub struct View {
    pub selected: usize,
    pub finished: bool,
}

fn s(c: Color) -> Style {
    Style::new().fg(c)
}

fn bold(c: Color) -> Style {
    s(c).add_modifier(Modifier::BOLD)
}

pub fn duration(secs: f64) -> String {
    let t = secs.max(0.0).round() as u64;
    match t {
        0..=59 => format!("{t}s"),
        60..=3599 => format!("{}m {:02}s", t / 60, t % 60),
        _ => format!("{}h {:02}m", t / 3600, t % 3600 / 60),
    }
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e12 {
        let g = group(v.abs() as u64);
        if v < 0.0 { format!("-{g}") } else { g }
    } else {
        let digits = if v.abs() >= 100.0 {
            0
        } else if v.abs() >= 10.0 {
            1
        } else {
            2
        };
        let t = format!("{v:.digits$}");
        let t = if t.contains('.') { t.trim_end_matches('0').trim_end_matches('.').to_string() } else { t };
        if v.abs() >= 1000.0 { group(t.parse::<f64>().unwrap_or(v).abs() as u64) } else { t }
    }
}

/// Decimal units, as `corsia bench` prints them.
pub fn bytes(b: f64) -> String {
    let (v, unit) = if b >= 1e12 {
        (b / 1e12, "TB")
    } else if b >= 1e9 {
        (b / 1e9, "GB")
    } else if b >= 1e6 {
        (b / 1e6, "MB")
    } else if b >= 1e3 {
        (b / 1e3, "kB")
    } else {
        return format!("{} B", b as u64);
    };
    if v >= 100.0 { format!("{v:.0} {unit}") } else { format!("{v:.1} {unit}") }
}

pub fn rate(mb_s: f64) -> String {
    if mb_s >= 1000.0 {
        format!("{:.2} GB/s", mb_s / 1000.0)
    } else if mb_s >= 10.0 {
        format!("{mb_s:.0} MB/s")
    } else {
        format!("{mb_s:.1} MB/s")
    }
}

/// Paths in titles and settings shrink to their last part: a/b/model.gguf becomes model.gguf.
fn compact(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            let path = word.starts_with('/') || word.starts_with("~/") || word.starts_with("./");
            if path { word.rsplit('/').next().unwrap_or(word) } else { word }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else if width == 0 {
        String::new()
    } else {
        text.chars().take(width - 1).collect::<String>() + "…"
    }
}

fn pad(text: &str, width: usize) -> String {
    format!("{:<width$}", fit(text, width))
}

fn rpad(text: &str, width: usize) -> String {
    format!("{:>width$}", fit(text, width))
}

fn home(p: &std::path::Path) -> String {
    let full = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && full.starts_with(&h) => format!("~{}", &full[h.len()..]),
        _ => full,
    }
}

fn clock(t: f64) -> String {
    // Local wall time from the system offset, without a timezone database.
    let secs = (t as i64 + local_offset()).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// "17:15" today, "Sat 17:15" within the week, "12 Nov" before that.
fn started(t: f64) -> String {
    let off = local_offset();
    let day = |x: f64| (x as i64 + off).div_euclid(86_400);
    let (d, today) = (day(t), day(now()));
    let c = clock(t);
    let hm = &c[..5];
    if d == today {
        hm.to_string()
    } else if today - d < 7 {
        const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        format!("{} {hm}", DAYS[d.rem_euclid(7) as usize])
    } else {
        // Civil date from days since 1970 (Howard Hinnant's algorithm).
        let z = d + 719_468;
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let dd = doy - (153 * mp + 2) / 5 + 1;
        const MONTHS: [&str; 12] = ["Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec", "Jan", "Feb"];
        format!("{dd} {}", MONTHS[mp as usize])
    }
}

fn local_offset() -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        std::env::var("CORSIA_WATCH_UTC_OFFSET").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
            std::process::Command::new("date")
                .arg("+%z")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|z| {
                    let z = z.trim();
                    let sign = if z.starts_with('-') { -1 } else { 1 };
                    let h: i64 = z.get(1..3)?.parse().ok()?;
                    let m: i64 = z.get(3..5)?.parse().ok()?;
                    Some(sign * (h * 3600 + m * 60))
                })
                .unwrap_or(0)
        })
    })
}

fn glyph<'a>(st: Status, th: &Theme) -> Span<'a> {
    match st {
        Status::Running => Span::styled("●", s(th.accent)),
        Status::Done => Span::styled("✓", s(th.good)),
        Status::Failed => Span::styled("✕", s(th.bad)),
        Status::Stopped => Span::styled("◌", s(th.warn)),
    }
}

fn word(st: Status) -> &'static str {
    match st {
        Status::Running => "Running",
        Status::Done => "Finished",
        Status::Failed => "Failed",
        Status::Stopped => "Stopped",
    }
}

fn bar<'a>(frac: Option<f64>, width: usize, th: &Theme, live: bool) -> Vec<Span<'a>> {
    match frac {
        Some(f) => {
            let full = ((f * width as f64).round() as usize).min(width);
            let color = if live { th.accent } else { th.muted };
            vec![Span::styled("━".repeat(full), s(color)), Span::styled("─".repeat(width - full), s(th.faint))]
        }
        None => vec![Span::styled("╌".repeat(width), s(th.faint))],
    }
}

/// Bars scaled from zero to `top`, newest on the right; the unfilled part is a faint baseline.
fn spark<'a>(values: &[f64], width: usize, top: f64, th: &Theme) -> Vec<Span<'a>> {
    const TICKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let tail = &values[values.len().saturating_sub(width)..];
    let marks: String = tail
        .iter()
        .map(|&v| if top > 0.0 && v > 0.0 { TICKS[((v / top) * 7.0).round().clamp(0.0, 7.0) as usize] } else { '▁' })
        .collect();
    vec![Span::styled("▁".repeat(width - tail.len()), s(th.faint)), Span::styled(marks, s(th.accent))]
}

pub fn draw(f: &mut Frame, store: &Store, daemon: &Daemon, model: Option<&Model>, view: &View, th: &Theme) {
    let area = f.area().inner(Margin { horizontal: 2, vertical: 1 });
    let panel_h = daemon_height(daemon).min(area.height.saturating_sub(6));
    // The model panel takes a third of what is left once something has been written.
    let model_h = match model {
        Some(m) if m.id != 0 || matches!(m.link, Link::Live) => (area.height.saturating_sub(panel_h + 8) / 3).clamp(4, 14),
        Some(_) => 2,
        None => 0,
    };
    let notes_h = if daemon.notes.is_empty() { 0 } else { (daemon.notes.len() as u16 + 1).min(area.height.saturating_sub(panel_h + model_h + 10)) };
    let [header, _, body, _, mpanel, _, npanel, _, panel, _, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(if model_h > 0 { 1 } else { 0 }),
        Constraint::Length(model_h),
        Constraint::Length(if notes_h > 0 { 1 } else { 0 }),
        Constraint::Length(notes_h),
        Constraint::Length(1),
        Constraint::Length(panel_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_notes(f, npanel, daemon, th);

    draw_header(f, header, store, daemon, th);
    draw_footer(f, footer, store, daemon, view, th);
    draw_daemon(f, panel, daemon, th);
    if let Some(m) = model {
        draw_model(f, mpanel, m, th);
    }

    let runs = store.ordered(view.finished);
    if runs.is_empty() {
        empty(f, body, store, view, th);
        return;
    }
    let sel = view.selected.min(runs.len() - 1);
    if body.width >= 100 {
        let [list, _, detail] = Layout::horizontal([Constraint::Length(38), Constraint::Length(4), Constraint::Min(40)]).areas(body);
        draw_list(f, list, &runs, sel, th);
        draw_detail(f, detail, runs[sel], th);
    } else {
        let rows = (runs.len() as u16 * 2).min(4).min(body.height / 4 * 2).max(2);
        let [list, _, detail] = Layout::vertical([Constraint::Length(rows), Constraint::Length(1), Constraint::Min(4)]).areas(body);
        draw_list(f, list, &runs, sel, th);
        draw_detail(f, detail, runs[sel], th);
    }
}

/// Notes from whoever is setting the machine up, newest last.
fn draw_notes(f: &mut Frame, area: Rect, d: &Daemon, th: &Theme) {
    if area.height == 0 {
        return;
    }
    let w = area.width as usize;
    let mut out = vec![Line::from(vec![Span::styled("Notes", bold(th.text)), Span::styled("  what is being done on this machine", s(th.muted))])];
    let room = area.height.saturating_sub(1) as usize;
    let start = d.notes.len().saturating_sub(room);
    for n in &d.notes[start..] {
        let (time, text) = n.split_once("  ").unwrap_or(("", n.as_str()));
        let text: String = text.chars().take(w.saturating_sub(time.len() + 2)).collect();
        out.push(Line::from(vec![Span::styled(format!("{time}  "), s(th.faint)), Span::styled(text, s(th.text))]));
    }
    f.render_widget(Paragraph::new(out), area);
}

/// What the model is writing: its thinking (dim) and its answer, the latest lines.
fn draw_model(f: &mut Frame, area: Rect, m: &Model, th: &Theme) {
    if area.height == 0 {
        return;
    }
    let w = area.width as usize;
    let mut title = vec![Span::styled("Model", bold(th.text)), Span::styled("  ", s(th.muted))];
    match (&m.link, m.writing(), &m.done) {
        (Link::Down { why, .. }, _, _) if m.id == 0 => {
            title.push(Span::styled(format!("no live text from {}: {why}", m.target.label()), s(th.muted)));
        }
        (Link::Connecting, _, _) if m.id == 0 => {
            title.push(Span::styled(format!("connecting to corsia agent at {}", m.target.label()), s(th.muted)));
        }
        (_, true, _) => {
            let secs = m.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
            title.push(Span::styled("● ", s(th.accent)));
            let what = match (m.pieces, m.speed()) {
                (0, _) => format!("answer #{}: reading the prompt, {}", m.id, duration(secs)),
                (n, Some(v)) => format!("writing answer #{}: {n} tokens, {v:.1} tok/s, {}", m.id, duration(secs)),
                (n, None) => format!("writing answer #{}: {n} tokens, {}", m.id, duration(secs)),
            };
            title.push(Span::styled(what, s(th.muted)));
        }
        (_, false, Some(done)) => {
            title.push(Span::styled(format!("answer #{} done: {done}", m.id), s(th.muted)));
        }
        _ => title.push(Span::styled(format!("waiting for a request at {}", m.target.label()), s(th.muted))),
    }
    let mut lines: Vec<Line> = Vec::new();
    let push = |text: &str, style: Style, lines: &mut Vec<Line>| {
        for raw in text.split('\n') {
            let chars: Vec<char> = raw.chars().collect();
            if chars.is_empty() {
                lines.push(Line::from(""));
                continue;
            }
            for chunk in chars.chunks(w.max(1)) {
                lines.push(Line::from(Span::styled(chunk.iter().collect::<String>(), style)));
            }
        }
    };
    if let Some((tool, text)) = &m.context {
        let label = if *tool { "tool result: " } else { "asked: " };
        push(&format!("{label}{}", text.replace('\n', " ")), s(th.muted), &mut lines);
    }
    if !m.thinking.is_empty() {
        push(&format!("thinking: {}", m.thinking.trim_start()), s(th.muted), &mut lines);
    }
    if !m.answer.is_empty() {
        push(m.answer.trim_start(), s(th.text), &mut lines);
    }
    let room = area.height.saturating_sub(1) as usize;
    let start = lines.len().saturating_sub(room);
    let mut out = vec![Line::from(title)];
    out.extend(lines.into_iter().skip(start));
    f.render_widget(Paragraph::new(out), area);
}

fn draw_header(f: &mut Frame, area: Rect, store: &Store, daemon: &Daemon, th: &Theme) {
    let left = Line::from(vec![
        Span::styled("◆ ", s(th.accent)),
        Span::styled("Corsia", bold(th.text)),
        Span::styled("  watch", s(th.muted)),
    ]);
    let (live, _) = store.counts();
    let mut right = Vec::new();
    if live > 0 {
        right.push(Span::styled("● ", s(th.accent)));
        right.push(Span::styled(format!("{live} running"), s(th.text)));
    } else {
        right.push(Span::styled("No test running", s(th.muted)));
    }
    right.push(Span::styled("   ", s(th.muted)));
    match (&daemon.link, daemon.stale()) {
        (Link::Live, None) => {
            right.push(Span::styled("● ", s(th.accent)));
            let now_rate = daemon.rate_now();
            let what = if now_rate < 0.05 { "Daemon idle".to_string() } else { format!("Daemon {}", rate(now_rate)) };
            right.push(Span::styled(what, s(th.text)));
        }
        (Link::Live, Some(_)) => {
            right.push(Span::styled("◌ ", s(th.warn)));
            right.push(Span::styled("Daemon quiet", s(th.text)));
        }
        (Link::Connecting, _) => right.push(Span::styled("◌ Connecting", s(th.muted))),
        (Link::Down { .. }, _) => right.push(Span::styled("○ No daemon", s(th.muted))),
    }
    // Narrowing drops the clock first, then the word "watch"; the mark always stays.
    let wide = area.width >= 80;
    if wide {
        right.push(Span::styled(format!("   {}", clock(now())), s(th.muted)));
    }
    let left = if area.width >= 64 { left } else { Line::from(left.spans[..2].to_vec()) };
    f.render_widget(Paragraph::new(left), area);
    f.render_widget(Paragraph::new(Line::from(right).right_aligned()), area);
}

fn draw_footer(f: &mut Frame, area: Rect, store: &Store, daemon: &Daemon, view: &View, th: &Theme) {
    let keys = Line::from(vec![
        Span::styled("↑↓", s(th.text)),
        Span::styled(" select   ", s(th.muted)),
        Span::styled("f", s(th.text)),
        Span::styled(if view.finished { " hide finished   " } else { " show finished   " }, s(th.muted)),
        Span::styled("q", s(th.text)),
        Span::styled(" quit", s(th.muted)),
    ]);
    f.render_widget(Paragraph::new(keys), area);
    let src = format!("Runs from {}   daemon at {}", home(&store.dir), daemon.target.label());
    if area.width as usize > 46 + src.chars().count() {
        f.render_widget(Paragraph::new(Line::from(Span::styled(src, s(th.faint))).right_aligned()), area);
    }
}

fn empty(f: &mut Frame, area: Rect, store: &Store, view: &View, th: &Theme) {
    let (live, finished) = store.counts();
    let mut lines = vec![Line::from(Span::styled("◇", s(th.accent))).centered(), Line::default()];
    if live == 0 && finished > 0 && !view.finished {
        lines.push(Line::from(Span::styled("No test running right now", bold(th.text))).centered());
        lines.push(Line::from(Span::styled(format!("{finished} finished runs are hidden. Press f to show them."), s(th.muted))).centered());
    } else {
        lines.push(Line::from(Span::styled("No test runs yet", bold(th.text))).centered());
        lines.push(
            Line::from(Span::styled("The test scripts report here the moment they start, one line per arm as it finishes.", s(th.muted)))
                .centered(),
        );
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("integrations/llama.cpp/stream-on-40gb.sh MODEL.gguf CAP_GIB", s(th.text))).centered());
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(format!("Watching {}", home(&store.dir)), s(th.faint))).centered());
    let h = lines.len() as u16;
    let y = area.y + area.height.saturating_sub(h) / 2;
    f.render_widget(Paragraph::new(lines), Rect { y, height: h.min(area.height), ..area });
}

fn draw_list(f: &mut Frame, area: Rect, runs: &[&Run], sel: usize, th: &Theme) {
    let per = 2u16;
    let visible = (area.height / per).max(1) as usize;
    let first = sel.saturating_sub(visible - 1);
    let w = area.width as usize;
    for (k, run) in runs.iter().enumerate().skip(first).take(visible) {
        let y = area.y + (k - first) as u16 * per;
        if y >= area.bottom() {
            break;
        }
        let rect = Rect { x: area.x, y, width: area.width, height: per.min(area.bottom() - y) };
        let st = run.status();
        let when = started(run.started);
        let short = run.title.split(':').next().unwrap_or(&run.title).trim();
        let title = fit(short, w.saturating_sub(4 + when.chars().count()));
        let gap = w.saturating_sub(2 + title.chars().count() + when.chars().count());
        let count = match run.total {
            Some(t) => format!("{}/{}", run.done, t),
            None => run.done.to_string(),
        };
        let tail = match (run.now_doing(), run.best(), &run.headline) {
            (Some((what, _)), _, _) => format!("  {what}"),
            (None, _, _) if st == Status::Failed => "  failed".into(),
            (None, Some((_, v)), Some(h)) => format!("  {} {}", number(v), h.metric),
            _ => format!("  {}", duration(run.elapsed())),
        };
        let bar_w = w.saturating_sub(3 + count.len() + tail.chars().count());
        let mut second = vec![Span::raw("  ")];
        second.extend(bar(run.fraction(), bar_w, th, st == Status::Running));
        second.push(Span::styled(format!(" {count}"), s(th.text)));
        second.push(Span::styled(tail, s(th.muted)));
        let lines = vec![
            Line::from(vec![
                glyph(st, th),
                Span::raw(" "),
                Span::styled(title, bold(th.text)),
                Span::raw(" ".repeat(gap)),
                Span::styled(when, s(th.muted)),
            ]),
            Line::from(second),
        ];
        let style = if k == sel { Style::new().bg(th.fill) } else { Style::new() };
        f.render_widget(Paragraph::new(lines).style(style), rect);
    }
}

fn draw_detail(f: &mut Frame, area: Rect, run: &Run, th: &Theme) {
    let st = run.status();
    let w = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(fit(&compact(&run.title), w), bold(th.text))));
    let mut sub: Vec<String> = run.meta.iter().map(|(k, v)| format!("{k} {}", compact(v))).collect();
    sub.push(run.id.clone());
    lines.push(Line::from(Span::styled(fit(&sub.join("  ·  "), w), s(th.muted))));
    lines.push(Line::default());

    let plural = run.unit.as_deref().unwrap_or("steps");
    let unit = if run.total == Some(1) { plural.strip_suffix('s').unwrap_or(plural) } else { plural };
    let count = match run.total {
        Some(t) => format!("  {} of {} {unit}", run.done, t),
        None => format!("  {} {unit}", run.done),
    };
    let mut gauge = bar(run.fraction(), w.saturating_sub(count.chars().count()), th, st == Status::Running);
    gauge.push(Span::styled(count, s(th.text)));
    lines.push(Line::from(gauge));
    lines.push(Line::default());

    let mut stats: Vec<(String, String)> = vec![("Status".into(), word(st).into()), ("Elapsed".into(), duration(run.elapsed()))];
    match run.now_doing() {
        Some((what, secs)) => stats.push(("Now".into(), format!("{what}, {}", duration(secs)))),
        None if st == Status::Running => stats.push(("Now".into(), "starting".into())),
        None => {}
    }
    if let Some(h) = &run.headline {
        let best = run.best().map_or_else(
            || "no result yet".to_string(),
            |(item, v)| format!("{}{}", cell(&h.metric, v), item.map_or_else(String::new, |i| format!(", {i}"))),
        );
        stats.push((format!("Best ({})", h.label.to_lowercase()), best));
    }
    if w < 70 {
        let mut first = vec![glyph(st, th), Span::styled(format!(" {}", stats[0].1), s(th.text))];
        for (k, v) in stats.iter().skip(1) {
            let sep = if k == "Now" { "  ·  now " } else if k.starts_with("Best") { "" } else { "  ·  " };
            if k.starts_with("Best") {
                continue;
            }
            first.push(Span::styled(sep, s(th.muted)));
            first.push(Span::styled(v.clone(), s(th.text)));
        }
        lines.push(Line::from(first));
        if let Some((k, v)) = stats.iter().find(|(k, _)| k.starts_with("Best")) {
            lines.push(Line::from(vec![Span::styled(format!("{k}  "), s(th.muted)), Span::styled(v.clone(), s(th.text))]));
        }
    }
    let col = 22usize;
    let per_row = (w / col).max(1);
    for (r, chunk) in stats.chunks(per_row).enumerate().filter(|_| w >= 70) {
        if r > 0 {
            lines.push(Line::default());
        }
        let mut label = Vec::new();
        let mut value = Vec::new();
        for (i, (k, v)) in chunk.iter().enumerate() {
            label.push(Span::styled(pad(k, col), s(th.muted)));
            if r == 0 && i == 0 {
                value.push(glyph(st, th));
                value.push(Span::styled(format!(" {}", pad(v, col - 2)), s(th.text)));
            } else {
                value.push(Span::styled(pad(v, col), s(th.text)));
            }
        }
        lines.push(Line::from(label));
        lines.push(Line::from(value));
    }

    if let Some((t, status, summary)) = &run.end {
        lines.push(Line::default());
        let mut msg = format!("{} at {}", if status == "done" { "Finished" } else { "Failed" }, clock(*t));
        if !summary.is_empty() {
            msg += &format!("  ·  {}", summary.iter().map(|(k, v)| format!("{k} {}", compact(v))).collect::<Vec<_>>().join("  ·  "));
        }
        let (g, c) = if status == "done" { ("✓ ", th.good) } else { ("✕ ", th.bad) };
        lines.push(Line::from(vec![Span::styled(g, s(c)), Span::styled(fit(&msg, w.saturating_sub(2)), s(th.text))]));
    } else if st == Status::Stopped {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("◌ ", s(th.warn)),
            Span::styled(fit("The script ended without saying it finished. It may have been stopped or crashed.", w.saturating_sub(2)), s(th.text)),
        ]));
    }

    results(&mut lines, run, w, area.height as usize, th);

    let room = area.height as usize;
    if !run.logs.is_empty() && room > lines.len() + 3 {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("Log", bold(th.text))));
        let n = room.saturating_sub(lines.len()).min(12);
        let recent: Vec<_> = run.logs.iter().rev().take(n).collect();
        for (t, msg) in recent.into_iter().rev() {
            lines.push(Line::from(vec![
                Span::styled(format!("{}  ", clock(*t)), s(th.faint)),
                Span::styled(fit(msg, w.saturating_sub(10)), s(th.text)),
            ]));
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// A metric named "time s" or "load s" is seconds: shown as "41.2s" or "3m 41s".
fn cell(name: &str, v: f64) -> String {
    if name.ends_with(" s") {
        if v >= 60.0 { duration(v) } else { format!("{}s", number(v)) }
    } else if name.contains('/') || name.contains(' ') {
        format!("{} {name}", number(v))
    } else {
        number(v)
    }
}

fn header(name: &str) -> &str {
    name.strip_suffix(" s").unwrap_or(name)
}

fn value(name: &str, v: f64) -> String {
    if name.ends_with(" s") { cell(name, v) } else { number(v) }
}

/// One row per finished step in the order they ran, the headline compared with the first
/// row, and a row for the step that is running now.
fn results(lines: &mut Vec<Line>, run: &Run, w: usize, room: usize, th: &Theme) {
    if run.steps.is_empty() && run.now_doing().is_none() {
        return;
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Results", bold(th.text))));

    let item_w = run.steps.iter().filter_map(|s| s.item.as_ref()).map(|i| i.chars().count()).max().unwrap_or(4).clamp(6, 16) + 3;
    let base = run.headline.as_ref().and_then(|h| {
        let first = run.steps.front()?;
        let v = first.metrics.iter().find(|(k, _)| *k == h.metric)?.1;
        (v > 0.0 && run.values(&h.metric).len() >= 2).then(|| (first.item.clone().unwrap_or_else(|| "first".into()), v))
    });
    let mut cols: Vec<(String, usize)> = Vec::new();
    let mut used = item_w;
    let ratio_w = base.as_ref().map_or(0, |(i, _)| i.chars().count().max(4) + 6);
    for name in &run.metric_names {
        let widest = run
            .steps
            .iter()
            .filter_map(|s| s.metrics.iter().find(|(k, _)| k == name))
            .map(|(_, v)| value(name, *v).chars().count())
            .max()
            .unwrap_or(1);
        let cw = header(name).chars().count().max(widest) + 3;
        if used + cw + ratio_w > w {
            break;
        }
        used += cw;
        cols.push((name.clone(), cw));
    }

    let mut head = vec![Span::styled(pad("", item_w), s(th.muted))];
    head.extend(cols.iter().map(|(n, cw)| Span::styled(rpad(header(n), *cw), s(th.muted))));
    if let Some((item, _)) = &base {
        head.push(Span::styled(rpad(&format!("vs {item}"), ratio_w), s(th.muted)));
    }
    if !cols.is_empty() {
        lines.push(Line::from(head));
    }

    let best = run.best().map(|(_, v)| v);
    let reserve = run.now_doing().is_some() as usize;
    let left = room.saturating_sub(lines.len() + reserve).max(1);
    let skip = run.steps.len().saturating_sub(left);
    for step in run.steps.iter().skip(skip) {
        let item = step.item.clone().unwrap_or_else(|| format!("#{}", step.done));
        let mut row = vec![Span::styled(pad(&item, item_w), s(th.text))];
        for (name, cw) in &cols {
            let v = step.metrics.iter().find(|(k, _)| k == name).map(|(_, v)| *v);
            let is_best = run.headline.as_ref().is_some_and(|h| &h.metric == name) && v.is_some() && v == best;
            let cell = v.map_or_else(|| "none".to_string(), |v| value(name, v));
            let style = if is_best { bold(th.accent) } else if v.is_none() { s(th.faint) } else { s(th.text) };
            row.push(Span::styled(rpad(&cell, *cw), style));
        }
        if let (Some((_, b)), Some(h)) = (&base, &run.headline) {
            let cell = match step.metrics.iter().find(|(k, _)| *k == h.metric) {
                Some((_, v)) if *v > 0.0 => {
                    let r = if h.lower_is_better { b / v } else { v / b };
                    if r >= 10.0 { format!("{r:.0}x") } else { format!("{r:.1}x") }
                }
                _ => "none".into(),
            };
            row.push(Span::styled(rpad(&cell, ratio_w), s(th.text)));
        }
        lines.push(Line::from(row));
    }
    if let Some((what, secs)) = run.now_doing() {
        lines.push(Line::from(vec![
            Span::styled(pad(what, item_w), s(th.text)),
            Span::styled("● ", s(th.accent)),
            Span::styled(format!("running for {}", duration(secs)), s(th.muted)),
        ]));
    }
}

fn daemon_height(d: &Daemon) -> u16 {
    match (&d.link, &d.last) {
        (Link::Live, Some(_)) => 8 + d.drives.len() as u16,
        _ => 4,
    }
}

fn draw_daemon(f: &mut Frame, area: Rect, d: &Daemon, th: &Theme) {
    if area.height == 0 {
        return;
    }
    f.render_widget(Paragraph::new("").style(Style::new().bg(th.fill)), area);
    let inner = area.inner(Margin { horizontal: 2, vertical: 1 });
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    let title = Span::styled("Daemon   ", bold(th.text));

    let v = match (&d.link, &d.last) {
        (Link::Live, Some(v)) => v,
        (Link::Connecting, _) => {
            lines.push(Line::from(vec![
                title,
                Span::styled("◌ ", s(th.muted)),
                Span::styled(format!("Connecting to {}…", d.target.label()), s(th.text)),
            ]));
            f.render_widget(Paragraph::new(lines).style(Style::new().bg(th.fill)), inner);
            return;
        }
        (Link::Down { why, since }, last) => {
            let state = if last.is_some() { format!("Stopped {} ago", duration(since.elapsed().as_secs_f64())) } else { "Not running".into() };
            lines.push(Line::from(vec![
                title,
                Span::styled("○ ", s(th.muted)),
                Span::styled(state, s(th.text)),
                Span::styled(fit(&format!("   {}: {why}", d.target.label()), w.saturating_sub(30)), s(th.muted)),
            ]));
            let local = d.target.host == "127.0.0.1" || d.target.host == "localhost";
            let mut hint = vec![Span::styled("Start it with ", s(th.muted)), Span::styled("corsia serve MODEL --mount DIR", s(th.text))];
            if local {
                hint.push(Span::styled(", or watch another machine with ", s(th.muted)));
                hint.push(Span::styled(format!("ssh -L {p}:127.0.0.1:{p} HOST", p = d.target.port), s(th.text)));
            }
            lines.push(Line::from(hint));
            f.render_widget(Paragraph::new(lines).style(Style::new().bg(th.fill)), inner);
            return;
        }
        (Link::Live, None) => return,
    };

    // Title row: state on the left, the model on the right.
    let mut left = vec![title];
    match d.stale() {
        None => {
            left.push(Span::styled("● ", s(th.accent)));
            left.push(Span::styled("Live", s(th.text)));
        }
        Some(secs) => {
            left.push(Span::styled("◌ ", s(th.warn)));
            left.push(Span::styled(format!("No update for {secs}s"), s(th.text)));
        }
    }
    left.push(Span::styled(format!("   {}", d.target.label()), s(th.muted)));
    let model = format!(
        "{}   {}   up {}",
        v["model"]["name"].as_str().unwrap_or("model"),
        bytes(v["model"]["bytes"].as_f64().unwrap_or(0.0)),
        duration(v["uptime_s"].as_f64().unwrap_or(0.0))
    );
    let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
    lines.push(Line::default());
    lines.push(Line::default());

    // Reading now: the total, then each drive on the same scale.
    let wide = w >= 100;
    let mid = w >= 72;
    let label_w = 11;
    let role_w = 12;
    let rate_w = 11;
    let tail_w = if wide { 50 } else { 0 };
    let spark_w = w.saturating_sub(label_w + if mid { role_w } else { 0 } + rate_w + tail_w + 2).clamp(4, 90);
    let total: Vec<f64> = d.total.iter().copied().collect();
    let total_peak = d.total_peak();
    let mut row = vec![Span::styled(pad("Reading", label_w), s(th.muted))];
    if mid {
        row.push(Span::styled(pad("all drives", role_w), s(th.muted)));
    }
    row.extend(spark(&total, spark_w, total_peak, th));
    row.push(Span::styled(rpad(&rate(d.rate_now()), rate_w), bold(th.text)));
    if wide {
        row.push(Span::styled(format!("   peak {}", rate(total_peak)), s(th.muted)));
    }
    lines.push(Line::from(row));

    let sources = v["sources"].as_array().cloned().unwrap_or_default();
    let model_chunks = v["model"]["chunks"].as_f64().unwrap_or(0.0);
    for drive in &d.drives {
        let src = sources.iter().find(|s| s["drive"].as_str() == Some(drive.name.as_str())).cloned().unwrap_or(Value::Null);
        let rates: Vec<f64> = drive.rates.iter().copied().collect();
        let now_rate = rates.last().copied().unwrap_or(0.0);
        let mut row = vec![Span::styled(pad(&format!("  {}", drive.name), label_w), s(th.text))];
        if mid {
            let role = match src["role"].as_str() {
                Some("copy") if model_chunks > 0.0 => {
                    let pct = (src["chunks"].as_f64().unwrap_or(0.0) / model_chunks * 100.0).floor();
                    format!("copy {pct:.0}%")
                }
                Some(r) => r.to_string(),
                None => String::new(),
            };
            row.push(Span::styled(pad(&role, role_w), s(th.muted)));
        }
        row.extend(spark(&rates, spark_w, d.peak, th));
        row.push(Span::styled(rpad(&rate(now_rate), rate_w), s(th.text)));
        if wide {
            row.push(Span::styled(
                format!("   {:>8} served  {:>9} reads   ", bytes(src["bytes_served"].as_f64().unwrap_or(0.0)), group(src["reads"].as_u64().unwrap_or(0))),
                s(th.muted),
            ));
            let errors = src["errors"].as_u64().unwrap_or(0);
            if src["online"].as_bool() == Some(false) {
                row.push(Span::styled("✕ ", s(th.bad)));
                row.push(Span::styled("offline", s(th.text)));
            } else if errors > 0 {
                row.push(Span::styled("! ", s(th.warn)));
                row.push(Span::styled(format!("{} errors", group(errors)), s(th.text)));
            } else {
                row.push(Span::styled("✓ ", s(th.good)));
                row.push(Span::styled("no errors", s(th.muted)));
            }
        } else if src["online"].as_bool() == Some(false) {
            row.push(Span::styled("  ✕ offline", s(th.bad)));
        }
        lines.push(Line::from(row));
    }
    lines.push(Line::default());

    // Prefetch and cache, then the copier.
    let p = &v["prefetch"];
    let mut pre = vec![
        Span::styled(pad("Prefetch", label_w), s(th.muted)),
        Span::styled(bytes(p["stored_bytes"].as_f64().unwrap_or(0.0)), s(th.text)),
        Span::styled(" in memory, ", s(th.muted)),
        Span::styled(bytes(p["already_resident_bytes"].as_f64().unwrap_or(0.0)), s(th.text)),
        Span::styled(" already there", s(th.muted)),
    ];
    if mid {
        pre.truncate(1);
        pre.extend([
            Span::styled(bytes(p["stored_bytes"].as_f64().unwrap_or(0.0)), s(th.text)),
            Span::styled(" put in memory   ", s(th.muted)),
            Span::styled(bytes(p["already_resident_bytes"].as_f64().unwrap_or(0.0)), s(th.text)),
            Span::styled(" already there   ", s(th.muted)),
            Span::styled(bytes(p["joined_bytes"].as_f64().unwrap_or(0.0)), s(th.text)),
            Span::styled(" joined in flight", s(th.muted)),
        ]);
    }
    if wide {
        pre.push(Span::styled("   ", s(th.muted)));
        pre.push(Span::styled(group(p["async_dropped"].as_u64().unwrap_or(0)), s(th.text)));
        pre.push(Span::styled(" guesses dropped", s(th.muted)));
    }
    lines.push(Line::from(pre));

    let c = &v["cache"];
    let (hits, misses) = (c["hits"].as_f64().unwrap_or(0.0), c["misses"].as_f64().unwrap_or(0.0));
    let hit = if hits + misses > 0.0 { format!(", {:.0}% hits", hits / (hits + misses) * 100.0) } else { ", no reads yet".into() };
    let cache = format!("{} of {} blocks{hit}", group(c["chunks"].as_u64().unwrap_or(0)), group(c["capacity"].as_u64().unwrap_or(0)));
    let copied = bytes(v["copier"]["copied_bytes"].as_f64().unwrap_or(0.0));
    let (cg, cc, cw) = match v["copier"]["state"].as_str().unwrap_or("") {
        "copying" => ("● ", th.accent, format!("copying, {copied} so far")),
        "waiting for idle" => ("◌ ", th.muted, format!("waiting for the drives to go quiet, {copied} copied")),
        "done" => ("✓ ", th.good, format!("done, {copied} copied this session")),
        _ => ("○ ", th.muted, "none (add --copy-to DIR for a speed copy)".into()),
    };
    let cache = if mid { cache } else { hit.trim_start_matches(", ").to_string() };
    let cache_w = label_w + cache.chars().count() + 6;
    lines.push(Line::from(vec![
        Span::styled(pad("Cache", label_w), s(th.muted)),
        Span::styled(format!("{cache}      "), s(th.text)),
        Span::styled("Copies  ", s(th.muted)),
        Span::styled(cg, s(cc)),
        Span::styled(fit(&cw, w.saturating_sub(cache_w + 10)), s(th.text)),
    ]));

    f.render_widget(Paragraph::new(lines).style(Style::new().bg(th.fill)), inner);
    let row = inner_row(inner, 0);
    f.render_widget(Paragraph::new(Line::from(left)), row);
    if w > left_w + 12 {
        let m = fit(&model, w - left_w - 3);
        f.render_widget(Paragraph::new(Line::from(Span::styled(m, s(th.muted))).right_aligned()), row);
    }
}

fn inner_row(r: Rect, n: u16) -> Rect {
    Rect { y: r.y + n, height: 1.min(r.height.saturating_sub(n)), ..r }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_only_shrinks_paths() {
        assert_eq!(compact("log /home/me/.cache/run.log"), "log run.log");
        assert_eq!(compact("mount, 0.07 tok/s"), "mount, 0.07 tok/s");
    }

    #[test]
    fn numbers_and_units() {
        assert_eq!(number(3637.0), "3,637");
        assert_eq!(number(0.054), "0.05");
        assert_eq!(number(18.84), "18.8");
        assert_eq!(cell("time s", 221.0), "3m 41s");
        assert_eq!(cell("load s", 12.5), "12.5s");
        assert_eq!(rate(1234.0), "1.23 GB/s");
        assert_eq!(bytes(11_814_277_696.0), "11.8 GB");
    }
}
