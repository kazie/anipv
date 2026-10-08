//! Render README screenshots as SVG from the demo library.
#![expect(clippy::print_stdout, clippy::print_stderr, reason = "a command-line tool reporting what it wrote")]
//!
//! ```text
//! cargo run --example screenshots          # writes assets/img/*.svg
//! ```
//!
//! Screens are drawn with ratatui's `TestBackend`, so the images are exactly
//! what the TUI renders, with a fixed clock for reproducible output.

use std::fmt::Write as _;
use std::path::Path;

use anipv::tui::app::NowPlaying;
use anipv::tui::{App, ui};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};

const NOW: i64 = 1_790_000_000;
const W: u16 = 112;

// Palette (Catppuccin Mocha-ish) for the 16 ANSI colors.
#[expect(
    clippy::match_same_arms,
    reason = "normal and light variants share a color; one arm per ANSI color reads best"
)]
fn hex(c: Color, fg: bool) -> Option<&'static str> {
    Some(match c {
        Color::Reset => {
            if fg {
                "#cdd6f4"
            } else {
                return None;
            }
        }
        Color::Black => "#45475a",
        Color::Red => "#f38ba8",
        Color::Green => "#a6e3a1",
        Color::Yellow => "#f9e2af",
        Color::Blue => "#89b4fa",
        Color::Magenta => "#cba6f7",
        Color::Cyan => "#94e2d5",
        Color::Gray => "#bac2de",
        Color::DarkGray => "#6c7086",
        Color::LightRed => "#f38ba8",
        Color::LightGreen => "#a6e3a1",
        Color::LightYellow => "#f9e2af",
        Color::LightBlue => "#89b4fa",
        Color::LightMagenta => "#f5c2e7",
        Color::LightCyan => "#94e2d5",
        Color::White => "#f5f5f5",
        Color::Indexed(53) => "#45305a",
        Color::Indexed(235) => "#262637",
        Color::Indexed(236) => "#313244",
        _ => "#cdd6f4",
    })
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Convert a buffer to an SVG "terminal window".
fn svg(buf: &Buffer, title: &str) -> String {
    let (cw, lh, pad, top) = (8.4_f64, 19.0_f64, 16.0_f64, 34.0_f64);
    let width = f64::from(buf.area.width) * cw + 2.0 * pad;
    let height = f64::from(buf.area.height) * lh + top + pad;
    let mut out = String::new();
    let _ = write!(
        out,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0}" height="{height:.0}" viewBox="0 0 {width:.0} {height:.0}" font-family="'JetBrains Mono','Cascadia Code','Fira Code',Menlo,Consolas,monospace" font-size="14">
<rect width="100%" height="100%" rx="10" fill="#1e1e2e"/>
<circle cx="20" cy="17" r="6" fill="#f38ba8"/><circle cx="40" cy="17" r="6" fill="#f9e2af"/><circle cx="60" cy="17" r="6" fill="#a6e3a1"/>
<text x="{:.0}" y="22" fill="#6c7086" text-anchor="middle" font-size="12">{}</text>
"##,
        width / 2.0,
        esc(title)
    );
    for y in 0..buf.area.height {
        let mut x = 0;
        while x < buf.area.width {
            let cell = &buf[(x, y)];
            let style = (cell.fg, cell.bg, cell.modifier);
            let start = x;
            let mut text = String::new();
            while x < buf.area.width {
                let c = &buf[(x, y)];
                if (c.fg, c.bg, c.modifier) != style {
                    break;
                }
                text.push_str(c.symbol());
                x += 1;
            }
            let n = f64::from(x - start);
            let px = pad + f64::from(start) * cw;
            let py = top + f64::from(y) * lh;
            let reversed = style.2.contains(Modifier::REVERSED);
            let (fg, bg) = if reversed { (style.1, style.0) } else { (style.0, style.1) };
            if let Some(b) = hex(bg, false) {
                let _ = writeln!(
                    out,
                    r#"<rect x="{px:.1}" y="{:.1}" width="{:.1}" height="{lh}" fill="{b}"/>"#,
                    py - 1.0,
                    n * cw
                );
            }
            if text.trim().is_empty() {
                continue;
            }
            let mut attrs = format!(r#"fill="{}""#, hex(fg, true).unwrap_or("#cdd6f4"));
            if style.2.contains(Modifier::BOLD) {
                attrs.push_str(r#" font-weight="bold""#);
            }
            if style.2.contains(Modifier::DIM) {
                attrs.push_str(r#" opacity="0.6""#);
            }
            if style.2.contains(Modifier::UNDERLINED) {
                attrs.push_str(r#" text-decoration="underline""#);
            }
            let _ = writeln!(
                out,
                r#"<text x="{px:.1}" y="{:.1}" {attrs} xml:space="preserve" textLength="{:.1}" lengthAdjust="spacingAndGlyphs">{}</text>"#,
                py + 13.0,
                n * cw,
                esc(&text)
            );
        }
    }
    out.push_str("</svg>\n");
    out
}

fn shot(app: &mut App, height: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(W, height)).expect("terminal");
    t.draw(|f| ui::draw(f, app)).expect("draw");
    t.backend().buffer().clone()
}

/// Parse SGR-colored text (e.g. clap's `--help`) into a ratatui buffer.
fn ansi_buffer(s: &str, width: u16) -> Buffer {
    let mut lines = Vec::new();
    for raw in s.lines() {
        let mut spans = Vec::new();
        let mut style = Style::new();
        let mut rest = raw;
        while let Some(i) = rest.find("\x1b[") {
            if i > 0 {
                spans.push(Span::styled(rest[..i].to_string(), style));
            }
            let after = &rest[i + 2..];
            let end = after.find('m').unwrap_or(after.len());
            for code in after[..end].split(';') {
                style = match code {
                    "" | "0" => Style::new(),
                    "1" => style.add_modifier(Modifier::BOLD),
                    "2" => style.add_modifier(Modifier::DIM),
                    "4" => style.add_modifier(Modifier::UNDERLINED),
                    "31" | "91" => style.fg(Color::Red),
                    "32" | "92" => style.fg(Color::Green),
                    "33" | "93" => style.fg(Color::Yellow),
                    "34" | "94" => style.fg(Color::Blue),
                    "35" | "95" => style.fg(Color::Magenta),
                    "36" | "96" => style.fg(Color::Cyan),
                    "39" => style.fg(Color::Reset),
                    _ => style,
                };
            }
            rest = &after[(end + 1).min(after.len())..];
        }
        if !rest.is_empty() {
            spans.push(Span::styled(rest.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    let h = u16::try_from(lines.len()).unwrap_or(u16::MAX).saturating_add(1);
    let area = Rect::new(0, 0, width, h);
    let mut buf = Buffer::empty(area);
    Paragraph::new(Text::from(lines)).render(Rect::new(1, 0, width - 1, h), &mut buf);
    buf
}

fn main() -> anyhow::Result<()> {
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/img");
    std::fs::create_dir_all(&out_dir)?;
    let home = tempfile::tempdir()?;
    let write = |name: &str, buf: &Buffer, title: &str| -> anyhow::Result<()> {
        std::fs::write(out_dir.join(name), svg(buf, title))?;
        println!("wrote assets/img/{name}");
        Ok(())
    };

    let fresh = |home: &Path| -> anyhow::Result<App> {
        let ctx = anipv::demo::setup(home, NOW)?;
        let mut app = App::new(ctx)?;
        app.clock = Some(NOW);
        Ok(app)
    };

    // Up next with two episodes queued and one playing.
    let mut app = fresh(home.path())?;
    app.press("j  ");
    app.playing = Some(NowPlaying {
        path: None,
        label: "Sousou no Frieren 29".into(),
        pos: 802.0,
        dur: Some(1420.0),
        total: 3,
        done: 0,
    });
    write("up-next.svg", &shot(&mut app, 14), "anipv — up next")?;
    app.playing = None;

    app.press("5");
    write("queue.svg", &shot(&mut app, 10), "anipv — queue")?;

    let home2 = tempfile::tempdir()?;
    let mut app = fresh(home2.path())?;
    app.press("2");
    write("inbox.svg", &shot(&mut app, 12), "anipv — inbox")?;
    app.press("3");
    write("series.svg", &shot(&mut app, 21), "anipv — series")?;

    app.press("/meitantei\n\nx");
    write("detail.svg", &shot(&mut app, 16), "anipv — episodes")?;

    app.press("s");
    write("status.svg", &shot(&mut app, 21), "anipv — set status")?;

    // Colored --help, like rich-click.
    let bin = env!("CARGO_MANIFEST_DIR").to_string() + "/target/debug/anipv";
    let help = std::process::Command::new(&bin).arg("--help").env("CLICOLOR_FORCE", "1").env("COLUMNS", "100").output();
    match help {
        Ok(o) if o.status.success() => {
            write("help.svg", &ansi_buffer(&String::from_utf8_lossy(&o.stdout), 100), "anipv --help")?;
        }
        _ => eprintln!("skipping help.svg: build the binary first (cargo build)"),
    }
    Ok(())
}
