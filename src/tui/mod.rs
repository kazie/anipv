//! Interactive terminal UI built on ratatui.
//!
//! * `app` — state, key handling and background work (scan, metadata, mpv)
//! * `ui`  — rendering

pub mod app;
pub mod ui;

use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::app::Ctx;
pub use app::App;

/// Run the TUI until the user quits.
pub fn run(ctx: Ctx) -> Result<()> {
    let mut app = App::new(ctx)?;
    // Freshen the index and metadata in the background on start.
    app.start_scan();
    app.start_meta(app::STARTUP);

    let mut terminal = ratatui::init();
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
    ratatui::restore();
    res
}
