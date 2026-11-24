//! `corsia watch --snapshot WxH [--html]`: draw one frame off-screen and print it, as plain
//! text or as an HTML page with the real colours. Used to review the design and in tests.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

use crate::daemon::Daemon;
use crate::runs::Store;
use crate::ui::{self, Theme, View};

fn css(c: Color, fallback: &str) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => fallback.to_string(),
    }
}

pub fn render(store: &Store, daemon: &Daemon, view: &View, theme: &Theme, size: &str, html: bool, bg: &str, fg: &str) -> std::io::Result<String> {
    let (w, h) = size.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((120, 36));
    let mut term = Terminal::new(TestBackend::new(w, h)).map_err(std::io::Error::other)?;
    term.draw(|f| ui::draw(f, store, daemon, None, view, theme)).map_err(std::io::Error::other)?;
    let buf = term.backend().buffer();
    let mut out = String::new();
    if html {
        out.push_str(&format!(
            "<!doctype html><meta charset=utf-8><body style=\"margin:0;background:{bg}\"><pre style=\"margin:0;padding:24px;font:15px/1.35 'Geist Mono','JetBrains Mono',monospace;color:{fg}\">"
        ));
    }
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            let cell = &buf[(x, y)];
            let sym = cell.symbol();
            if !html {
                line.push_str(sym);
                continue;
            }
            let esc = match sym {
                "<" => "&lt;".to_string(),
                ">" => "&gt;".to_string(),
                "&" => "&amp;".to_string(),
                s => s.to_string(),
            };
            let bold = if cell.modifier.contains(Modifier::BOLD) { "font-weight:600;" } else { "" };
            line.push_str(&format!(
                "<span style=\"display:inline-block;height:1.35em;color:{};background:{};{bold}\">{esc}</span>",
                css(cell.fg, "inherit"),
                css(cell.bg, "transparent")
            ));
        }
        if html {
            out.push_str(&line);
        } else {
            out.push_str(line.trim_end());
        }
        out.push('\n');
    }
    if html {
        out.push_str("</pre>");
    }
    Ok(out)
}
