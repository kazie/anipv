//! Series identity: turning messy titles into stable, comparable keys.
//!
//! A *series key* is the canonical identifier used everywhere (index, events,
//! metadata cache). It is derived deterministically from a title so that two
//! devices that discover the same series independently agree on its key
//! without any coordination.

use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

static SEASON_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:season\s*(\d{1,2})|(\d{1,2})(?:st|nd|rd|th)\s+season|s0?(\d{1,2}))\b").expect("valid regex")
});

/// Normalize a human title into a series key.
///
/// * Unicode is NFKD-decomposed and diacritics are dropped (`Pokémon` → `pokemon`).
///   The exception is the kana voiced sound marks (dakuten `゛` and handakuten `゜`):
///   they change the letter, so they stay and the kana is recomposed
///   (`バカ` ≠ `ハカ`, `フレンズ` stays `フレンズ`).
/// * Everything is lowercased; apostrophes vanish (`Let's` → `lets`), `&` becomes `and`.
/// * Any other non-alphanumeric run becomes a single space.
/// * Season markers (`Season 2`, `2nd Season`, `S02`) collapse to `s2`; `s1` is dropped.
///
/// Keys written before the voiced marks were kept are available from
/// [`legacy_series_key`].
///
/// ```
/// use anipv::identity::series_key;
/// assert_eq!(series_key("Grand Blue Season 3"), "grand blue s3");
/// assert_eq!(series_key("Healin' Good♥Precure"), "healin good precure");
/// assert_ne!(series_key("バカ"), series_key("ハカ"));
/// ```
pub fn series_key(title: &str) -> String {
    fold(title, true)
}

/// The series key as it was before kana voiced marks were kept: `バカ` and
/// `ハカ` both give `ハカ`. Events in old logs are recorded under these keys;
/// the library maps them to [`series_key`] (see `docs/events.md`).
///
/// ```
/// use anipv::identity::{legacy_series_key, series_key};
/// assert_eq!(legacy_series_key("フレンズ"), "フレンス");
/// assert_eq!(legacy_series_key("One Piece"), series_key("One Piece"));
/// ```
pub fn legacy_series_key(title: &str) -> String {
    fold(title, false)
}

/// Key-style normalization for item labels (`ova`, `fanart corner`, a movie's
/// title). A label is part of an item's identity in the event log, so it keeps
/// the folding it was recorded with and does not follow [`series_key`].
pub fn label_key(text: &str) -> String {
    legacy_series_key(text)
}

const DAKUTEN: char = '\u{3099}';
const HANDAKUTEN: char = '\u{309A}';

fn is_voiced_mark(c: char) -> bool {
    matches!(c, DAKUTEN | HANDAKUTEN)
}

/// A kana letter, the only thing a voiced mark can belong to.
fn is_kana(c: char) -> bool {
    ('\u{3041}'..='\u{30FF}').contains(&c) && c.is_alphabetic()
}

/// Shared by [`series_key`] and [`legacy_series_key`]; `voiced` keeps (and
/// recomposes) voiced marks that follow a kana.
fn fold(title: &str, voiced: bool) -> String {
    let mut prev = '\0';
    let folded: String = title
        .nfkd()
        .filter(|&c| {
            let keep = if is_voiced_mark(c) { voiced && is_kana(prev) } else { !is_combining_mark(c) };
            prev = c;
            keep
        })
        .collect::<String>()
        .to_lowercase()
        .replace(['\'', '’', '`'], "")
        .replace('&', " and ");

    let seasoned = SEASON_WORDS.replace_all(&folded, |caps: &regex::Captures<'_>| {
        let n: u32 = caps.iter().skip(1).flatten().next().and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
        if n <= 1 { " ".to_string() } else { format!(" s{n} ") }
    });

    let seasoned = seasoned.trim_start();
    let seasoned = seasoned.strip_prefix("the ").unwrap_or(seasoned);
    let mut out = String::with_capacity(seasoned.len());
    let mut pending_space = false;
    for c in seasoned.chars() {
        if is_voiced_mark(c) {
            // Kept only straight after its kana: put them together (NFC).
            if !pending_space && let Some(base) = out.pop() {
                out.extend([base, c].into_iter().nfc());
            }
        } else if c.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        } else {
            pending_space = true;
        }
    }
    out
}

/// Key and display title for a title with an explicit season (e.g. from `S03E05`).
///
/// Season 1 (or none) leaves both alone; later seasons get an `sN` suffix on
/// the key and ` SN` on the title, unless the title already carries one.
pub fn series_name(title: &str, season: Option<u32>) -> (String, String) {
    name_with(title, season, series_key)
}

/// [`series_name`] with [`legacy_series_key`].
pub fn legacy_series_name(title: &str, season: Option<u32>) -> (String, String) {
    name_with(title, season, legacy_series_key)
}

fn name_with(title: &str, season: Option<u32>, key_of: fn(&str) -> String) -> (String, String) {
    let (key, added) = name_key(title, season, key_of);
    let shown = match added {
        Some(n) => format!("{title} S{n}"),
        None => title.to_string(),
    };
    (key, shown)
}

/// The legacy key of `title` (as [`legacy_series_name`] gives it), given its
/// current `key` (as [`series_name`] gives it), or `None` when it is the same.
///
/// Only a voiced mark kept after a kana makes the two differ, so a key without
/// one skips the second fold. The title is folded rather than the key: a key
/// is not always its own key's input (`The The バカ` is keyed `the バカ`).
pub fn legacy_name_key(key: &str, title: &str, season: Option<u32>) -> Option<String> {
    has_voiced_mark(key).then(|| name_key(title, season, legacy_series_key).0)
}

/// True if a key holds a kana voiced mark (on its own or composed into a
/// kana, `バ`): exactly the keys whose legacy key differs.
fn has_voiced_mark(key: &str) -> bool {
    !key.is_ascii() && key.nfd().any(is_voiced_mark)
}

/// The key of `title` with `key_of`, plus the season suffix it was given (if any).
fn name_key(title: &str, season: Option<u32>, key_of: fn(&str) -> String) -> (String, Option<u32>) {
    let base = key_of(title);
    match season {
        Some(n) if n >= 2 && !base.split(' ').any(|w| w == format!("s{n}")) => {
            let key = if base.is_empty() { format!("s{n}") } else { format!("{base} s{n}") };
            (key, Some(n))
        }
        _ => (base, None),
    }
}

/// The franchise part of a series key: a trailing season (`s3`) or year
/// (`2024`) is dropped, so different seasons of a show share a base key.
///
/// ```
/// use anipv::identity::base_key;
/// assert_eq!(base_key("kusuriya no hitorigoto s3"), "kusuriya no hitorigoto");
/// assert_eq!(base_key("ranma 1 2 2024 s3"), "ranma 1 2");
/// assert_eq!(base_key("one piece"), "one piece");
/// ```
pub fn base_key(key: &str) -> &str {
    let mut base = key;
    loop {
        let Some((head, last)) = base.rsplit_once(' ') else { return base };
        let season = last.strip_prefix('s').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        let year = last.len() == 4
            && (last.starts_with("19") || last.starts_with("20"))
            && last.bytes().all(|b| b.is_ascii_digit());
        if !(season || year) {
            return base;
        }
        base = head;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn basic_normalization() {
        assert_eq!(series_key("One Piece"), "one piece");
        assert_eq!(series_key("  One   Piece!! "), "one piece");
        assert_eq!(series_key("Bakusou Kyoudai Let's & Go!!"), "bakusou kyoudai lets and go");
        assert_eq!(series_key("Kiratto Pri☆Chan"), "kiratto pri chan");
        assert_eq!(series_key("Kiratto Pri☆chan"), "kiratto pri chan");
        assert_eq!(series_key("POKÉTOON"), "poketoon");
        assert_eq!(series_key("JoJo’s Bizarre Adventure"), "jojos bizarre adventure");
        assert_eq!(series_key("The Ghost in the Shell"), "ghost in the shell");
        assert_eq!(series_key("Theater"), "theater");
    }

    #[test]
    fn kana_voiced_marks_tell_keys_apart() {
        assert_ne!(series_key("バカ"), series_key("ハカ"));
        assert_ne!(series_key("ぱん"), series_key("はん"));
        assert_eq!(series_key("けものフレンズV 1stLIVE"), "けものフレンズv 1stlive");
        assert_eq!(series_key("バカとテストと召喚獣 S2"), "バカとテストと召喚獣 s2");
        // The old behaviour is still available, for keys already in logs.
        assert_eq!(legacy_series_key("バカ"), "ハカ");
        assert_eq!(legacy_series_key("バカ"), legacy_series_key("ハカ"));
        assert_eq!(legacy_series_key("けものフレンズV"), "けものフレンスv");
    }

    #[test]
    fn kana_keys_are_nfc_whatever_the_input_form() {
        let key = series_key("バカ ヴ パン");
        assert_eq!(key, "バカ ヴ パン");
        assert_eq!(series_key("ハ\u{3099}カ ウ\u{3099} ハ\u{309A}ン"), key, "decomposed");
        assert_eq!(series_key("ﾊﾞｶ ｳﾞ ﾊﾟﾝ"), key, "half-width");
        assert_eq!(series_key(&key), key, "a key is its own key");
        assert!(key.chars().all(|c| !is_voiced_mark(c)), "composed wherever a kana exists");
    }

    #[test]
    fn stray_voiced_marks_are_dropped_as_before() {
        for t in ["A\u{3099}B", "\u{3099}x", "x \u{309A}y", "\u{309B}\u{309C}x", "e\u{301}\u{3099}"] {
            assert_eq!(series_key(t), legacy_series_key(t), "{t:?}");
        }
        // A spacing mark (゛) is a separator, not part of the preceding kana.
        assert_eq!(series_key("ハ゛カ"), legacy_series_key("ハ゛カ"));
    }

    #[test]
    fn labels_keep_the_legacy_folding() {
        assert_eq!(label_key("バカ ova"), "ハカ ova");
        assert_eq!(label_key("Fanart Corner"), series_key("Fanart Corner"));
    }

    #[test]
    fn legacy_names_follow_the_same_season_rules() {
        assert_eq!(legacy_series_name("バカ", Some(2)), ("ハカ s2".into(), "バカ S2".into()));
        assert_eq!(series_name("バカ", Some(2)), ("バカ s2".into(), "バカ S2".into()));
        assert_eq!(legacy_name_key("バカ s2", "バカ", Some(2)).as_deref(), Some("ハカ s2"));
        assert_eq!(legacy_name_key("one piece", "One Piece", None), None);
        // A raw mark after a kana that has no composed form is kept too.
        assert_eq!(legacy_name_key(&series_key("ア\u{3099}"), "ア\u{3099}", None).as_deref(), Some("ア"));
        // Folding the key again is not the same: `the` is stripped twice.
        let title = "The The バカ";
        assert_eq!(legacy_series_key(&series_key(title)), "ハカ");
        assert_eq!(legacy_name_key(&series_key(title), title, None).as_deref(), Some("the ハカ"));
    }

    proptest! {
        /// Without kana voiced marks nothing changed: existing keys are the same.
        #[test]
        fn keys_without_voiced_marks_are_unchanged(t in "[a-zA-Z0-9 &'’.!☆éÉñÅ字幕한글ー・あいうえおかきくけこアイウエオカキクケコ]{0,40}") {
            prop_assert_eq!(series_key(&t), legacy_series_key(&t));
        }

        /// Only a key with a voiced mark has a different legacy key
        /// (classification relies on this to skip the second fold).
        #[test]
        fn only_keys_with_voiced_marks_have_another_legacy_key(
            t in "([a-zA-Z0-9 &'.!éÅ\u{3099}\u{309A}゛゜ﾞﾟハカバアぱ]|\\PC){0,40}",
            season in proptest::option::of(0u32..30),
        ) {
            let (key, _) = series_name(&t, season);
            let legacy = legacy_series_name(&t, season).0;
            prop_assert_eq!(legacy_name_key(&key, &t, season), (legacy != key).then_some(legacy));
        }

        /// A legacy key is what the new fold gives once marks are dropped,
        /// which is what lets old history be mapped onto the new key.
        #[test]
        fn legacy_key_of_a_key_is_the_legacy_key_of_the_title(t in "[a-zA-Z0-9 &'.!ぁ-ゖァ-ヺ\u{3099}\u{309A}\u{FF66}-\u{FF9F}]{0,40}") {
            prop_assert_eq!(legacy_series_key(&series_key(&t)), legacy_series_key(&t));
        }

        #[test]
        fn keys_are_stable(t in "\\PC{0,30}") {
            let k = series_key(&t);
            prop_assert_eq!(series_key(&k), k);
        }
    }

    #[test]
    fn seasons_collapse() {
        assert_eq!(series_key("Grand Blue S3"), "grand blue s3");
        assert_eq!(series_key("Grand Blue Season 3"), "grand blue s3");
        assert_eq!(series_key("Grand Blue 3rd Season"), "grand blue s3");
        assert_eq!(series_key("Youjo Senki S02"), "youjo senki s2");
        assert_eq!(series_key("Something Season 1"), "something");
        assert_eq!(series_key("Nurse Witch S1"), "nurse witch");
    }

    #[test]
    fn season_suffix() {
        let name = |t: &str, s| series_name(t, s);
        assert_eq!(name("BEASTARS", Some(3)), ("beastars s3".into(), "BEASTARS S3".into()));
        assert_eq!(name("Beastars S3", Some(3)), ("beastars s3".into(), "Beastars S3".into()));
        assert_eq!(name("Chainsmoker Cat", Some(1)), ("chainsmoker cat".into(), "Chainsmoker Cat".into()));
        assert_eq!(name("Chainsmoker Cat", None), ("chainsmoker cat".into(), "Chainsmoker Cat".into()));
    }

    #[test]
    fn does_not_eat_words_starting_with_s() {
        assert_eq!(series_key("Sailor Moon SuperS"), "sailor moon supers");
        assert_eq!(series_key("S2 Sakura"), "s2 sakura");
    }

    #[test]
    fn base_keys() {
        assert_eq!(base_key("grand blue s3"), "grand blue");
        assert_eq!(base_key("pocket monsters 2023"), "pocket monsters");
        assert_eq!(base_key("dandadan s2"), "dandadan");
        assert_eq!(base_key("s2"), "s2", "a key that is only a marker stays");
        assert_eq!(base_key("mobile suit gundam 0083"), "mobile suit gundam 0083");
        assert_eq!(base_key("yes pretty cure 5"), "yes pretty cure 5");
    }
}
