//! Interactive terminal UI built on ratatui.
//!
//! * `app` — state, key handling and background work (scan, metadata, mpv)
//! * `ui`  — rendering
//! * `remote` — key names for remote mode (keys sent to mpv)
//! * `query` — the Series view's search: words and `field:value` filters

pub mod app;
mod query;
mod remote;
pub mod ui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};

use crate::app::Ctx;
pub use app::App;

/// The kitty keyboard flags are pushed (see [`run`]).
static KEYBOARD_PUSHED: AtomicBool = AtomicBool::new(false);

/// Pop the kitty keyboard flags, once: a panic (on any thread) and the
/// normal exit both get here, and a second pop would take the shell's own.
fn pop_keyboard() {
    if KEYBOARD_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
}

/// Run the TUI until the user quits.
pub fn run(ctx: Ctx) -> Result<()> {
    let mut app = App::new(ctx)?;
    // Freshen the index (unless `anipv update` did lately) and metadata in
    // the background on start.
    app.scan_unless_fresh();
    app.start_meta(app::STARTUP);

    let mut terminal = ratatui::init();
    // Terminals with the kitty keyboard protocol tell shift-⏎ from ⏎ (it
    // plays from the beginning); others send both as ⏎. Shifted keys come as
    // typed (`P`, `!`), as remote mode names them to mpv.
    let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
    if terminal::supports_keyboard_enhancement().unwrap_or(false)
        && execute!(std::io::stdout(), PushKeyboardEnhancementFlags(flags)).is_ok()
    {
        KEYBOARD_PUSHED.store(true, Ordering::SeqCst);
        // A panic leaves the shell's keys as they were too (ratatui's own
        // hook, which runs next, restores the rest).
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            pop_keyboard();
            hook(info);
        }));
    }
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
    pop_keyboard();
    ratatui::restore();
    res
}
