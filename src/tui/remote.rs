//! Remote mode: key presses in the TUI forwarded to mpv by name, so the
//! user's own `input.conf` decides what they do. See
//! <https://mpv.io/manual/stable/#key-names>.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// mpv's name for `key` (`SPACE`, `Ctrl+LEFT`, `q`, …), or `None` for a key
/// mpv has no name for.
///
/// A typed character already carries Shift (`Q`, `?`), so Shift is only
/// named for other keys (`Shift+RIGHT`).
pub fn mpv_key(key: KeyEvent) -> Option<String> {
    let name = match key.code {
        KeyCode::Char(' ') => "SPACE".into(),
        KeyCode::Char('#') => "SHARP".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("F{n}"),
        // Shift is part of the key here.
        KeyCode::BackTab => return Some("Shift+TAB".into()),
        code => named(code)?.into(),
    };
    let mods = key.modifiers;
    let prefix = |on: bool, p: &'static str| if on { p } else { "" };
    let shift = !matches!(key.code, KeyCode::Char(_)) && mods.contains(KeyModifiers::SHIFT);
    Some(format!(
        "{}{}{}{name}",
        prefix(shift, "Shift+"),
        prefix(mods.contains(KeyModifiers::CONTROL), "Ctrl+"),
        prefix(mods.contains(KeyModifiers::ALT), "Alt+"),
    ))
}

/// mpv's names for the keys that have one of their own.
fn named(code: KeyCode) -> Option<&'static str> {
    Some(match code {
        KeyCode::Enter => "ENTER",
        KeyCode::Esc => "ESC",
        KeyCode::Backspace => "BS",
        KeyCode::Tab => "TAB",
        KeyCode::Left => "LEFT",
        KeyCode::Right => "RIGHT",
        KeyCode::Up => "UP",
        KeyCode::Down => "DOWN",
        KeyCode::Home => "HOME",
        KeyCode::End => "END",
        KeyCode::PageUp => "PGUP",
        KeyCode::PageDown => "PGDWN",
        KeyCode::Insert => "INS",
        KeyCode::Delete => "DEL",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(code: KeyCode, mods: KeyModifiers) -> Option<String> {
        mpv_key(KeyEvent::new(code, mods))
    }

    #[test]
    fn key_names() {
        let none = KeyModifiers::NONE;
        for (code, mods, want) in [
            (KeyCode::Char(' '), none, "SPACE"),
            (KeyCode::Char('q'), none, "q"),
            (KeyCode::Char('Q'), KeyModifiers::SHIFT, "Q"),
            (KeyCode::Char('#'), KeyModifiers::SHIFT, "SHARP"),
            (KeyCode::Char('>'), KeyModifiers::SHIFT, ">"),
            (KeyCode::Char('s'), KeyModifiers::CONTROL, "Ctrl+s"),
            (KeyCode::Char('x'), KeyModifiers::ALT, "Alt+x"),
            (KeyCode::Enter, none, "ENTER"),
            (KeyCode::Esc, none, "ESC"),
            (KeyCode::Backspace, none, "BS"),
            (KeyCode::Tab, none, "TAB"),
            (KeyCode::BackTab, KeyModifiers::SHIFT, "Shift+TAB"),
            (KeyCode::Left, none, "LEFT"),
            (KeyCode::Right, KeyModifiers::CONTROL, "Ctrl+RIGHT"),
            (KeyCode::Up, KeyModifiers::CONTROL, "Ctrl+UP"),
            (KeyCode::Down, KeyModifiers::SHIFT, "Shift+DOWN"),
            (KeyCode::Left, KeyModifiers::ALT, "Alt+LEFT"),
            (KeyCode::PageUp, none, "PGUP"),
            (KeyCode::PageDown, none, "PGDWN"),
            (KeyCode::Home, none, "HOME"),
            (KeyCode::End, none, "END"),
            (KeyCode::Insert, none, "INS"),
            (KeyCode::Delete, none, "DEL"),
            (KeyCode::F(11), none, "F11"),
            (KeyCode::Right, KeyModifiers::SHIFT | KeyModifiers::CONTROL, "Shift+Ctrl+RIGHT"),
        ] {
            assert_eq!(name(code, mods).as_deref(), Some(want), "{code:?} {mods:?}");
        }
        assert_eq!(name(KeyCode::CapsLock, none), None);
    }
}
