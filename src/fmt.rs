//! Small formatting helpers for terminal output.

use std::io::IsTerminal;
use std::sync::OnceLock;

fn color_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| !no_color(std::env::var_os("NO_COLOR").as_deref()) && std::io::stdout().is_terminal())
}

/// Whether a `NO_COLOR` of `value` turns colors off: only when it is not
/// empty (no-color.org).
fn no_color(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

/// Bold text.
pub fn bold(s: &str) -> String {
    paint("1", s)
}
/// Dimmed text.
pub fn dim(s: &str) -> String {
    paint("2", s)
}
/// Green text.
pub fn green(s: &str) -> String {
    paint("32", s)
}
/// Yellow text.
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
/// Cyan text.
pub fn cyan(s: &str) -> String {
    paint("36", s)
}
/// Magenta text.
pub fn magenta(s: &str) -> String {
    paint("35", s)
}
/// Red text.
pub fn red(s: &str) -> String {
    paint("31", s)
}

/// Compact relative time in the past: `now`, `5m`, `3h`, `2d`, `6w`, `1y`.
pub fn ago(ts: i64, now: i64) -> String {
    let d = now.saturating_sub(ts).max(0);
    match d {
        0..60 => "now".into(),
        60..3_600 => format!("{m}m", m = d / 60),
        3_600..86_400 => format!("{h}h", h = d / 3_600),
        86_400..1_209_600 => format!("{days}d", days = d / 86_400),
        1_209_600..31_536_000 => format!("{w}w", w = d / 604_800),
        _ => format!("{y}y", y = d / 31_536_000),
    }
}

/// [`ago`] for an optional timestamp; empty when there is none.
pub fn ago_opt(ts: Option<i64>, now: i64) -> String {
    ts.map(|t| ago(t, now)).unwrap_or_default()
}

/// Compact relative time in the future: `in 3h`, `in 2d 4h`, `aired`.
pub fn until(ts: i64, now: i64) -> String {
    let d = ts.saturating_sub(now);
    if d <= 0 {
        return "aired".into();
    }
    let (days, hours, mins) = (d / 86_400, (d % 86_400) / 3_600, (d % 3_600) / 60);
    match (days, hours) {
        (0, 0) => format!("in {m}m", m = mins.max(1)),
        (0, h) => format!("in {h}h"),
        (dd, 0) => format!("in {dd}d"),
        (dd, h) => format!("in {dd}d {h}h"),
    }
}

/// Whole seconds of `d`, rounded up: a wait of 59.99 s is 60 s, and the last
/// fraction of one is 1 s, not 0 s.
pub fn secs_up(d: std::time::Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

/// Seconds as `m:ss` or `h:mm:ss`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to >= 0; whole seconds for display"
)]
pub fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
        format!("{h}:{m:02}:{sec:02}")
    } else {
        let (m, sec) = (s / 60, s % 60);
        format!("{m}:{sec:02}")
    }
}

/// Truncate to `width` display columns (char-based), adding `…` (nothing at
/// all for width 0, which has no room for it).
pub fn trunc(s: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    // The characters' widths add up to a first cut. Measured as a string, like
    // `pad` and the terminal do, it then moves on while the next character
    // still fits, or back until it fits: a character can change how wide the
    // one before it is (`☺` + U+FE0F is an emoji, two wide; `👨` + a skin
    // tone is one emoji, not two).
    let fits = |end: usize| s[..end].width() < width;
    let mut end = 0;
    let mut sum = 0;
    for (i, c) in s.char_indices() {
        sum += c.width().unwrap_or(0);
        if sum >= width {
            break;
        }
        end = i + c.len_utf8();
    }
    if fits(end) {
        while let Some(c) = s[end..].chars().next()
            && fits(end + c.len_utf8())
        {
            end += c.len_utf8();
        }
    } else {
        end = s[..end].char_indices().rev().map(|(i, _)| i).find(|&i| fits(i)).unwrap_or(0);
    }
    format!("{cut}…", cut = &s[..end])
}

/// Pad (or truncate) to exactly `width` display columns.
pub fn pad(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let t = trunc(s, width);
    let w = t.width();
    format!("{t}{fill}", fill = " ".repeat(width.saturating_sub(w)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/anilist.md R18: the last fraction of a second shows as 1 s.
    #[test]
    fn secs_round_up() {
        let ms = std::time::Duration::from_millis;
        assert_eq!([ms(0), ms(1), ms(999), ms(1000), ms(59_999)].map(secs_up), [0, 1, 1, 1, 60]);
    }

    #[test]
    fn an_empty_no_color_keeps_colors() {
        assert!(no_color(Some("1".as_ref())));
        assert!(!no_color(Some("".as_ref())) && !no_color(None));
    }

    /// Truncating measures the string as a whole, like padding does: an
    /// emoji presentation selector makes the character before it two wide.
    #[test]
    fn truncated_text_fits_its_width() {
        use unicode_width::UnicodeWidthStr;
        let s = "ab\u{263A}\u{FE0F}cdefgh";
        for width in 1..12 {
            assert_eq!(pad(s, width).width(), width, "{width}");
        }
        assert_eq!(trunc(s, 4), "ab\u{263A}…", "the selector would make it five wide");
        assert_eq!(trunc(s, 5), "ab\u{263A}\u{FE0F}…");
    }

    proptest::proptest! {
        /// Whatever the text (emoji sequences, selectors, control characters),
        /// the cut fits and is the start of the text.
        #[test]
        fn truncated_text_always_fits(
            v in proptest::collection::vec(proptest::sample::select(vec![
                'a', '日', '\u{263A}', '\u{FE0F}', '\u{FE0E}', '\u{301}', '\u{200D}', '\u{1F468}', '\u{1F3FB}',
                '\u{1F1F8}', '\u{1F44D}', '\u{20E3}', '\u{1}', '\t',
            ]), 0..16),
            width in 0usize..22,
        ) {
            use unicode_width::UnicodeWidthStr;
            let s: String = v.into_iter().collect();
            let t = trunc(&s, width);
            proptest::prop_assert!(t.width() <= width, "{s:?} at {width}: {t:?}");
            proptest::prop_assert!(s.starts_with(t.trim_end_matches('…')));
        }
    }

    #[test]
    fn relative_times() {
        assert_eq!(ago(100, 100), "now");
        assert_eq!(ago(0, 3 * 3600), "3h");
        assert_eq!(ago(0, 3 * 86_400), "3d");
        assert_eq!(ago(0, 30 * 86_400), "4w");
        assert_eq!(ago(0, 800 * 86_400), "2y");
        assert_eq!(until(0, 10), "aired");
        assert_eq!(until(2 * 86_400 + 4 * 3600 + 5, 0), "in 2d 4h");
        assert_eq!(until(3 * 3600, 0), "in 3h");
        assert_eq!(until(300, 0), "in 5m");
        assert_eq!(until(30, 0), "in 1m", "under a minute rounds up, not to `in 0m`");
    }

    #[test]
    fn clocks() {
        assert_eq!(clock(65.0), "1:05");
        assert_eq!(clock(3725.0), "1:02:05");
    }

    #[test]
    fn width_aware() {
        assert_eq!(trunc("abcdef", 4), "abc…");
        assert_eq!(trunc("abc", 4), "abc");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("日本語テキスト", 6), "日本… ");
        // Regression: width 0 gave "…", one column too wide.
        assert_eq!(trunc("abc", 0), "");
        assert_eq!(pad("abc", 0), "");
        assert_eq!(trunc("abc", 1), "…");
    }
}
