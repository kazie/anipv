//! Interactive terminal UI built on ratatui.
//!
//! * `app` — state, key handling and background work (scan, metadata, mpv)
//! * `ui`  — rendering
//! * `remote` — key names for remote mode (keys sent to mpv)

pub mod app;
mod remote;
pub mod ui;

use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};

use crate::app::Ctx;
pub use app::App;

/// Run the TUI until the user quits.
pub fn run(ctx: Ctx) -> Result<()> {
    let mut app = App::new(ctx)?;
    // Freshen the index (unless `anipv update` did lately) and metadata in
    // the background on start.
    app.scan_unless_fresh();
    app.start_meta(app::STARTUP);

    let mut terminal = ratatui::init();
    // Terminals with the kitty keyboard protocol tell shift-⏎ from ⏎ (it
    // plays from the beginning); others send both as ⏎.
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false)
        && execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
    let res = (|| -> Result<()> {
        // Redraw on input, background messages, and once a second (relative
        // times, message expiry); otherwise stay idle.
        let mut dirty = true;
        let mut last_draw = Instant::now();
        while !app.quit {
            if dirty || last_draw.elapsed() >= Duration::from_secs(1) {
                terminal.draw(|f| ui::draw(f, &mut app))?;
                last_draw = Instant::now();
            }
            dirty = event::poll(Duration::from_millis(100))?
                && match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        app.on_key(k);
                        true
                    }
                    Event::Resize(..) => true,
                    _ => false,
                };
            dirty |= app.pump();
        }
        Ok(())
    })();
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    res
}
