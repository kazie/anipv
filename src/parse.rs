//! Filename parsing: video file names → title, episode number and item kind.
//!
//! Handles the naming conventions commonly found in video libraries:
//!
//! * bracketed tags: `[GroupA] Grand Blue S3 - 07 (1080p) [1234ABCD].mkv`
//! * dotted:         `Chainsmoker.Cat.S01E07.1080p.WEB.x264-GRP.mkv`
//! * underscores:    `[GroupB]Powerpuff_Girls_Z_-_09_[0000ABCD].avi`
//! * bare numbers:   `Azumanga Daiou 24.mkv`, `41 - Princess In Peril!.mkv`
//! * extras/specials: `- 20 Next Ep PV`, `NCOP 1`, `OVA - 02`, `SP1`, `Audio Drama 3`
//!
//! The parser is purely lexical; folder context (archive series folders,
//! `Extras/` directories) is applied by the indexer on top of this.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::identity::label_key;
use crate::model::{EpNo, ItemKey, ItemKind};

/// File extensions treated as playable video.
pub const VIDEO_EXTS: &[&str] =
    &["mkv", "mp4", "avi", "webm", "m2ts", "ts", "ogm", "wmv", "m4v", "mov", "mpg", "mpeg", "flv", "rmvb"];

/// True if `name` has a video file extension.
pub fn is_video(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| VIDEO_EXTS.iter().any(|v| v.eq_ignore_ascii_case(ext)))
}

/// `name` without a video extension (other extensions are kept).
pub fn stem(name: &str) -> &str {
    if is_video(name) { name.rsplit_once('.').map_or(name, |(s, _)| s) } else { name }
}

/// Result of parsing a single file or directory name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// Group tag (`[GroupA]`, `-GRP`), if any.
    pub group: Option<String>,
    /// Cleaned human title; may be empty when the name has none (`41 - Foo.mkv`).
    pub title: String,
    /// Season from an `SxxEyy` marker.
    pub season: Option<u32>,
    /// Episode number (for extras: the episode they belong to).
    pub ep: Option<EpNo>,
    /// Last episode for multi-episode files (`09-10`, `001&002`).
    pub ep_end: Option<EpNo>,
    /// Release version (`03v2` → 2).
    pub version: Option<u8>,
    /// Item kind.
    pub kind: ItemKind,
    /// Normalized label for specials/extras (`"ova"`, `"fanart corner"`).
    pub label: String,
    /// CRC32 hash tag, if present.
    pub hash: Option<String>,
}

impl Parsed {
    /// The item key this file maps to within its series.
    pub fn item_key(&self) -> ItemKey {
        let label = match self.kind {
            ItemKind::Episode => String::new(),
            ItemKind::Movie if self.label.is_empty() => label_key(&self.title),
            _ => self.label.clone(),
        };
        ItemKey { kind: self.kind, ep: self.ep, label }
    }
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect("valid regex"));
    };
}

re!(LEADING_GROUP, r"^\s*[\[(]([^\[\]()]+)[\])]\s*");
re!(HASH, r"[\[(]([0-9A-Fa-f]{8})[\])]");
re!(BRACKETS, r"\[[^\[\]]*\]|\(([^()]*)\)|\{[^{}]*\}|【[^】]*】");
re!(YEAR, r"^(?:19[5-9]\d|20[0-3]\d)$");
re!(TRAILING_GROUP, r"-([A-Za-z0-9]+)$");
re!(BATCH_RANGE_DASH, r"(?i)\s[-–]\s\d{1,4}\s?[-~]\s?\d{1,4}\b");
re!(BATCH_WORDS, r"(?i)\b(?:\d{1,4}\s?[-~]\s?\d{1,4}\s+)?(?:batch|complete(?:\s+series)?)\b");
re!(SPACES, r"\s{2,}");
re!(VERSION_TOKEN, r"(?:^|\s)v(\d{1,2})(?:$|\s)");

// Episode patterns, strongest first.
re!(SXXEYY, r"(?i)(?:^|[\s_.\-])S(\d{1,2})\s?E(\d{1,4}(?:\.\d)?)(?:v(\d{1,2}))?(?:-?E?(\d{1,4}))?(?:$|[\s_.\-])");
// `1x05`: season, episode (2-3 digits, so `16x9` and `1920x1080` are not ones),
// with an optional range end (`1x05-06`, `1x05-1x06`).
re!(NXM, r"(?i)(?:^|[\s_.\-])(\d{1,2})x(\d{2,3})(?:v(\d{1,2}))?(?:-(?:\d{1,2}x)?(\d{1,3}))?(?:$|[\s_.\-])");
re!(EP_WORD, r"(?i)(?:^|[\s_])(?:episode|ep)\.?\s?(\d{1,4}(?:\.\d)?)(?:v(\d{1,2}))?(?:$|[\s_])");
re!(DASH_NUM, r"[\s_][-–]\s?(\d{1,4}(?:\.\d)?)(?:v(\d{1,2}))?(?:[-&~+](\d{1,4}))?(?:$|[\s_])");
re!(E_ONLY, r"(?i)(?:^|[\s_.])E(\d{2,4})(?:v(\d{1,2}))?(?:$|[\s_.])");
// `05. Title`; a digit after the dot (`2.5 Title`) is rejected in code.
re!(LEADING_NUM, r"^(\d{1,4})(?:v(\d{1,2}))?(?:\.|\s[-–])\s*");
re!(MOVIE_N, r"(?i)(?:^|\s)Movie\s?(\d{1,2})(?:\s[-–]|\s|$)");
re!(NUMERIC_TAG, r"\[(\d{1,4})(?:v(\d{1,2}))?\]");
re!(ANY_NUM, r"(?:^|[\s_])(\d{1,4}(?:\.\d)?)(?:v(\d{1,2}))?(?:$|[\s_])");

// Specials: the following number (if any) is the special's episode number.
re!(SPECIAL_KW, r"(?i)(?:^|[\s_\-])(OVA|OAD|SP|Specials?|Recap|Omake)\s?(\d{1,3})?(?:$|[\s_\-])");
// Extras: matched case-insensitively.
re!(
    EXTRA_KW,
    r"(?i)(?:^|[\s_\-])(NC\s?OP\s?\d*|NC\s?ED\s?\d*|clean\s+(?:opening|ending)|creditless\s+(?:op|ed|opening|ending)|fanart\s+corner|next\s+ep(?:isode)?\s+pv|preview|trailer|teaser|PV\s?\d*|menu|audio\s+drama|drama\s+cd|picture\s+drama|making\s+of|interview|bd-box|scene\s+collection|commentary|promo)(?:$|[\s_\-\d]|v\d)"
);
// Weaker extras words, only used when no episode number was found.
re!(
    EXTRA_WEAK,
    r"(?i)(?:^|[\s_\-])(opening|ending|commercials?|bonus|featurette|extras?|music\s+video|promotional|showcase)(?:$|[\s_\-\d]|v\d)"
);
// Case-sensitive short extras that would be too noisy case-insensitively.
re!(EXTRA_UPPER, r"(?:^|[\s_\-])((?:OP|ED|CM)\d*)(?:$|[\s_\-])");

re!(
    QUALITY,
    r"(?i)^(?:\d{3,4}p|\d{3,4}x\d{3,4}|[xh]\.?26[45]|h|hevc|avc|av1|vp9|flac|aac[\d.]*|ac3|e?ac-?3|dts[\w.-]*|ddp?[\d.]*|truehd|atmos|opus|bd|bdrip|bdremux|bd-?rip|blu-?ray|dvd|dvdrip|r2|web|web-?dl|web-?rip|webdl|nf|amzn|cr|dsnp|hulu|atvp|hi10p?|10-?bit|8-?bit|repack|proper|dual|dual-audio|multi|multi-aud|remux|uncensored|jpn|msubs[\w-]*|hdr\d*|hdr10plus|dv|sdr|uhd|2160p|4k)$"
);

/// Make separators uniform: dots and underscores used as spaces become spaces.
fn normalize_separators(stem: &str) -> String {
    let spaces = stem.matches(' ').count();
    let unders = stem.matches('_').count();
    let mut s = stem.to_string();
    if unders > 0 && unders >= spaces {
        s = s.replace('_', " ");
    }
    if spaces == 0 && s.matches('.').count() >= 2 {
        s = s.replace('.', " ");
    }
    s
}

/// Drop bracketed tags, keeping `(2023)`-style years which are part of titles.
fn strip_brackets(s: &str) -> String {
    // Innermost tags first, until none are left (`[1080p[BD]`).
    let mut cur = Cow::Borrowed(s);
    loop {
        let next = BRACKETS.replace_all(&cur, |c: &Captures<'_>| match c.get(1) {
            Some(inner) if YEAR.is_match(inner.as_str().trim()) => {
                format!("\u{1}{year}\u{2}", year = inner.as_str().trim())
            }
            _ => " ".to_string(),
        });
        let Cow::Owned(next) = next else { break };
        cur = Cow::Owned(next);
    }
    cur.chars()
        .map(|c| match c {
            '[' | ']' | '{' | '}' => ' ',
            '\u{1}' => '(',
            '\u{2}' => ')',
            c => c,
        })
        .collect()
}

/// Cut a title at the first technical token (resolution, codec, source).
fn cut_at_quality(title: &str, cut_year: bool) -> &str {
    let mut end = title.len();
    let mut offset = 0;
    for tok in title.split(' ') {
        let t = tok.trim_matches(|c: char| !c.is_alphanumeric());
        if !t.is_empty() && offset > 0 && (QUALITY.is_match(t) || (cut_year && YEAR.is_match(tok))) {
            end = offset;
            break;
        }
        offset += tok.len() + 1;
    }
    &title[..end.min(title.len())]
}

/// A title cut at the first technical token (not at a year) and tidied.
fn clean_title(s: &str) -> String {
    tidy(cut_at_quality(s, false))
}

fn tidy(s: &str) -> String {
    let s = SPACES.replace_all(s, " ");
    s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '–' | '~' | ':' | ',' | '.' | '_' | '|'))
        .to_string()
}

fn label_of(s: &str) -> String {
    label_key(cut_at_quality(s.trim(), false))
}

/// Parse an optional capture group (episode, version, season…).
fn cap<T: std::str::FromStr>(m: Option<regex::Match<'_>>) -> Option<T> {
    m.and_then(|m| m.as_str().parse().ok())
}

/// Split off a leading `[Group]` / `(Group)` tag (not a year or a checksum).
fn take_group(s: &str) -> (Option<String>, &str) {
    match LEADING_GROUP.captures(s) {
        Some(c) if !YEAR.is_match(c[1].trim()) && !HASH.is_match(&c[0]) => {
            (Some(c[1].trim().to_string()), &s[c.get_match().end()..])
        }
        _ => (None, s),
    }
}

/// Drop bracketed tags and batch markers (`1-12 Batch`, and for directory
/// names also `- 01-12`).
fn strip_noise(s: &str, dir: bool) -> String {
    let mut s = strip_brackets(s);
    if dir {
        s = BATCH_RANGE_DASH.replace_all(&s, " ").into_owned();
    }
    BATCH_WORDS.replace_all(&s, " ").into_owned()
}

/// `Show Movie 2 - Subtitle` → the second movie of Show. Returns true if matched.
fn movie_n(text: &str, out: &mut Parsed) -> bool {
    match MOVIE_N.captures(text) {
        Some(c) if c.get(0).is_some_and(|m| m.start() > 0) => {
            let m = c.get_match();
            out.kind = ItemKind::Movie;
            out.label = format!("movie {}", &c[1]);
            out.title = tidy(&text[..m.start()]);
            out.ep = None;
            true
        }
        _ => false,
    }
}

/// Whether the text after an episode number marks an extra (`- 20 Next Ep PV`).
///
/// The text is an extra only when it is nothing but the keyword (and a number):
/// `- 20 Next Ep PV`, `- 03 - PV`. An episode title that merely contains one
/// (`- 03 - Interview with the Vampire`, `- 05 The Preview Night`) is not.
fn is_extra_tail(tail: &str) -> bool {
    let core = tail.trim_start_matches(['-', '–']).trim_start();
    let text = format!(" {core}");
    let Some(c) = extra_kw(&text) else { return false };
    let kw = kw(&c);
    kw.start() == 1 && only_number_after(&text[kw.end()..])
}

/// The first extras keyword in `s` (the case-insensitive words, else the
/// short upper-case ones like `OP`); group 1 is the keyword.
fn extra_kw(s: &str) -> Option<Captures<'_>> {
    EXTRA_KW.captures(s).or_else(|| EXTRA_UPPER.captures(s))
}

/// Whether `rest` (what follows an extras keyword) is at most a number,
/// version or technical tags.
fn only_number_after(rest: &str) -> bool {
    cut_at_quality(rest, false).chars().all(|ch| ch.is_whitespace() || ch.is_ascii_digit() || ch == 'v')
}

struct Hit {
    start: usize,
    end: usize,
    ep: Option<EpNo>,
    ep_end: Option<EpNo>,
    version: Option<u8>,
    season: Option<u32>,
}

impl Hit {
    /// Episode `ep` (no range, no season) found at `m`.
    fn new(m: regex::Match<'_>, ep: Option<EpNo>, version: Option<u8>) -> Self {
        Self { start: m.start(), end: m.end(), ep, ep_end: None, version, season: None }
    }
}

/// The keyword (group 1) of an extras keyword match.
fn kw<'h>(c: &Captures<'h>) -> regex::Match<'h> {
    c.get(1).expect("keyword group")
}

/// The end of a short episode range (`09-10`, `001&002`, `S01E01-E02`):
/// after `start` and at most four episodes on.
fn range_end(start: Option<EpNo>, end: Option<regex::Match<'_>>) -> Option<EpNo> {
    cap::<EpNo>(end).filter(|e| start.is_some_and(|s| *e > s && e.whole() - s.whole() <= 4))
}

/// A season marker match (`SXXEYY`, `NXM`): season, episode, version, range end.
fn season_hit(c: &Captures<'_>) -> Hit {
    let ep = cap(c.get(2));
    Hit { season: cap(c.get(1)), ep_end: range_end(ep, c.get(4)), ..Hit::new(c.get_match(), ep, cap(c.get(3))) }
}

fn strong_episode(s: &str) -> Option<Hit> {
    if let Some(c) = SXXEYY.captures(s) {
        return Some(season_hit(&c));
    }
    for (re, dash) in [(&*EP_WORD, false), (&*DASH_NUM, true), (&*E_ONLY, false)] {
        // `Movie Title - 2024` is a year, not episode 2024 (`- 1180` is an episode).
        let year = |c: &Captures<'_>| {
            dash && YEAR.is_match(&c[1])
                && c.get(2).is_none()
                && c.get(3).is_none()
                && !tidy(&s[..c.get_match().start()]).is_empty()
        };
        if let Some(c) = re.captures_iter(s).find(|c| !year(c)) {
            let ep: Option<EpNo> = cap(c.get(1));
            let ep_end = range_end(ep, c.get(3));
            return Some(Hit { ep_end, ..Hit::new(c.get_match(), ep, cap(c.get(2))) });
        }
    }
    if let Some(c) = NXM.captures(s) {
        return Some(season_hit(&c));
    }
    // `05. Kino`, but not `2.5 Jigen no Ririsa` or `3.0+1.0`.
    let decimal = |c: &Captures<'_>| {
        let n_end = c.get(2).or_else(|| c.get(1)).map_or(0, |m| m.end());
        s[n_end..].strip_prefix('.').is_some_and(|r| r.starts_with(|ch: char| ch.is_ascii_digit()))
    };
    if let Some(c) = LEADING_NUM.captures(s).filter(|c| !decimal(c)) {
        // Anchored at the start of `s`.
        return Some(Hit::new(c.get_match(), cap(c.get(1)), cap(c.get(2))));
    }
    None
}

/// A decimal other than a half (`3.0`, `2.1`) reads as a version or title
/// number (`Evangelion 3.0 You Can (Not) Redo`), not as an episode.
fn is_title_decimal(n: &str) -> bool {
    n.split_once('.').is_some_and(|(_, frac)| frac != "5")
}

/// Last bare number, preferring one followed by ` - ` or end of string; skips
/// years and version-like decimals.
fn weak_episode(s: &str) -> Option<Hit> {
    let mut best: Option<Hit> = None;
    let mut pos = 0;
    while let Some(c) = ANY_NUM.captures_at(s, pos) {
        let whole = c.get_match();
        let n = c.get(1).expect("number group");
        pos = n.end();
        if YEAR.is_match(n.as_str()) || is_title_decimal(n.as_str()) {
            continue;
        }
        // The last preferred number wins, else the first one.
        let after = s[whole.end()..].trim_start();
        let preferred = after.is_empty() || after.starts_with(['-', '–']);
        if preferred || best.is_none() {
            best = Some(Hit::new(whole, cap(Some(n)), cap(c.get(2))));
        }
    }
    best
}

/// Clean a directory name into a title without guessing episode numbers.
///
/// ```
/// use anipv::parse::clean_dir_name;
/// assert_eq!(clean_dir_name("[GroupA] Super no Ura de Yani Suu Futari (01-12) (1080p) [Batch]"), "Super no Ura de Yani Suu Futari");
/// assert_eq!(clean_dir_name("Blue Seed 2"), "Blue Seed 2");
/// ```
pub fn clean_dir_name(name: &str) -> String {
    let normalized = normalize_separators(name);
    let s = strip_noise(take_group(&normalized).1, true);
    let s = SPACES.replace_all(s.trim(), " ");
    let t = clean_title(&s);
    if t.is_empty() { tidy(name) } else { t }
}

/// Fill in `out` for a name `s` with a clear episode marker (`hit`): an
/// episode, or a special or extra that the marker's number belongs to.
fn with_episode(s: &str, hit: &Hit, mut out: Parsed) -> Parsed {
    let mut title = s[..hit.start].to_string();
    out.ep = hit.ep;
    out.ep_end = hit.ep_end;
    out.season = hit.season;
    out.version = hit.version.or(out.version);
    out.kind = ItemKind::Episode;

    // `Show S00E05` / `Show 0x05` → special #5 of Show (season 0 is specials).
    if out.season == Some(0) {
        out.season = None;
        out.kind = ItemKind::Special;
        out.label = label_key("special");
    }
    // `Show OVA - 02` → special #2 of Show.
    if let Some(c) = SPECIAL_KW.captures(&title).filter(|c| c.get(0).is_some_and(|m| m.start() > 0))
        && c.get(2).is_none()
    {
        out.kind = ItemKind::Special;
        out.label = c[1].to_lowercase();
        title.truncate(c.get_match().start());
    }
    if out.kind == ItemKind::Special {
        out.title = clean_title(&title);
        return out;
    }
    if movie_n(&s[..hit.end], &mut out) {
        return out;
    }
    let rest_trim = cut_at_quality(s[hit.end..].trim(), false);
    if let Some(c) = EXTRA_KW.captures(&title).filter(|c| c.get(1).is_some_and(|m| m.start() > 0)) {
        // `Show - Drama CD 3 - 1` → extra, the number is part of the extra.
        let kw = kw(&c);
        let ep = out.ep.take().map(|e| e.to_string()).unwrap_or_default();
        out.kind = ItemKind::Extra;
        out.label = label_of(&format!("{} {ep}", &title[kw.start()..]));
        title.truncate(kw.start());
    } else if is_extra_tail(rest_trim) {
        // `Show - 20 Next Ep PV` → extra attached to episode 20.
        out.kind = ItemKind::Extra;
        out.label = label_of(rest_trim);
    } else if let Some(c) = SPECIAL_KW.captures(&format!(" {rest_trim} "))
        && c.get(0).is_some_and(|m| m.start() == 0)
    {
        out.kind = ItemKind::Special;
        out.label = c[1].to_lowercase();
    }
    out.title = clean_title(&title);
    out
}

/// Parse a file (or directory) name.
///
/// # Panics
///
/// Never: the `expect`s guard capture groups that every match of their pattern has.
///
/// ```
/// use anipv::parse::parse;
/// use anipv::model::{EpNo, ItemKind};
/// let p = parse("[GroupA] Grand Blue S3 - 07 (1080p) [1234ABCD].mkv");
/// assert_eq!(p.title, "Grand Blue S3");
/// assert_eq!(p.ep, Some(EpNo::new(7)));
/// assert_eq!(p.kind, ItemKind::Episode);
/// ```
pub fn parse(name: &str) -> Parsed {
    let stem = stem(name);
    let hash = HASH.captures_iter(stem).last().map(|c| c[1].to_uppercase());

    let normalized = normalize_separators(stem);
    let (mut group, rest) = take_group(&normalized);
    if group.is_none() && !stem.contains(' ') {
        group = TRAILING_GROUP.captures(stem).map(|c| c[1].to_string()).filter(|g| !QUALITY.is_match(g));
    }

    let mut s = strip_noise(rest, !is_video(name));
    let mut version = None;
    if let Some(c) = VERSION_TOKEN.captures(&s) {
        version = cap(c.get(1));
        s = VERSION_TOKEN.replace(&s, " ").into_owned();
    }
    s = SPACES.replace_all(s.trim(), " ").into_owned();

    let mut out = Parsed { group, version, hash, ..Parsed::default() };

    if let Some(hit) = strong_episode(&s) {
        return with_episode(&s, &hit, out);
    }

    // No clear episode: drop technical tail (`1080p WEB DDP5 1 …`) so its
    // numbers aren't mistaken for episodes, then look for extras / specials.
    let full = s;
    let s = cut_at_quality(&full, false).trim();
    if movie_n(s, &mut out) {
        return out;
    }
    let extra = extra_kw(&full).or_else(|| EXTRA_WEAK.captures(&full));
    // After a number the keyword must end the name (`Show 05 PV`,
    // `Show 5 Go Go! NCOP`), so `Show 03 - Interview with the Vampire` stays an episode.
    let weak = weak_episode(s);
    let extra = extra.filter(|c| {
        let kw = kw(c);
        weak.as_ref().is_none_or(|w| kw.start() < w.end || only_number_after(&full[kw.end()..]))
    });
    let special = SPECIAL_KW.captures(s).filter(|c| c.get(1).is_some_and(|m| m.start() > 0));
    let pick_special = match (&extra, &special) {
        (Some(e), Some(sp)) => sp.get(0).map(|m| m.start()) < e.get(0).map(|m| m.start()),
        (None, Some(_)) => true,
        _ => false,
    };
    if pick_special {
        let c = special.expect("checked");
        let kw = kw(&c);
        out.kind = ItemKind::Special;
        out.label = kw.as_str().to_lowercase();
        out.ep = cap(c.get(2));
        if out.ep.is_none() {
            out.label = label_of(&s[kw.start()..]);
        }
        out.title = clean_title(&s[..kw.start()]);
        return out;
    }
    if let Some(c) = extra {
        let kw = kw(&c);
        out.kind = ItemKind::Extra;
        out.label = label_key(&full[kw.start()..]);
        out.title = clean_title(&full[..kw.start()]);
        return out;
    }

    if let Some(hit) = weak {
        out.kind = ItemKind::Episode;
        out.ep = hit.ep;
        out.version = hit.version.or(out.version);
        // May leave the title empty ("86 Eighty-Six"); the folder supplies it then.
        out.title = clean_title(&s[..hit.start]);
        return out;
    }

    out.kind = ItemKind::Movie;
    out.title = tidy(cut_at_quality(s, true));
    // Everything was bracketed (`[group][Title][02][BD]`): use the tags.
    if out.title.is_empty() {
        let tags = stem.replace(['[', ']', '(', ')', '_'], " ");
        if let Some(c) = extra_kw(&tags) {
            let kw = kw(&c);
            out.kind = ItemKind::Extra;
            out.label = label_key(kw.as_str());
            out.title = clean_title(tags[..kw.start()].trim());
            return out;
        }
    }
    if out.title.is_empty()
        && let Some(c) = NUMERIC_TAG.captures(stem)
    {
        out.kind = ItemKind::Episode;
        out.ep = cap(c.get(1));
        out.version = cap(c.get(2)).or(out.version);
    }
    out
}

#[cfg(test)]
mod tests {
    //! Example names below are made up: fictional group tags (`[GroupA]`,
    //! `-GRP`), neutral technical tags and obviously fake checksums. Only the
    //! *shapes* matter: spacing, brackets, separators, versions and ranges.

    use super::*;

    #[expect(clippy::unnecessary_wraps, reason = "compared against `Parsed::ep`, which is an Option")]
    fn ep(n: &str) -> Option<EpNo> {
        Some(n.parse().unwrap())
    }

    #[track_caller]
    fn check(name: &str, title: &str, e: Option<&str>, kind: ItemKind) -> Parsed {
        let p = parse(name);
        assert_eq!(p.title, title, "title of {name:?}: {p:?}");
        assert_eq!(p.ep, e.and_then(ep), "episode of {name:?}: {p:?}");
        assert_eq!(p.kind, kind, "kind of {name:?}: {p:?}");
        p
    }

    use ItemKind::*;

    #[test]
    fn bracketed_dash_number() {
        let p = check("[GroupA] Grand Blue S3 - 07 (1080p) [1234ABCD].mkv", "Grand Blue S3", Some("7"), Episode);
        assert_eq!(p.group.as_deref(), Some("GroupA"));
        assert_eq!(p.hash.as_deref(), Some("1234ABCD"));
        check("[GroupA] One Piece - 1180 (1080p) [0000F00D].mkv", "One Piece", Some("1180"), Episode);
        check(
            "[GroupB] Super no Ura de Yani Suu Futari - 01.mkv",
            "Super no Ura de Yani Suu Futari",
            Some("1"),
            Episode,
        );
        check("[GroupC] The Ghost in the Shell - 06 [CAFE0006].mkv", "The Ghost in the Shell", Some("6"), Episode);
        check("Aria - The Natural - 15 [DVD 960x720 x264 AC3].mkv", "Aria - The Natural", Some("15"), Episode);
        check("[GroupD] Aikatsu! - 047v2 (BD 1920x1080 x265 FLAC) [ABCD0047].mkv", "Aikatsu!", Some("47"), Episode);
        check("Meitantei Holmes  - 07 [1080p[BD].mkv", "Meitantei Holmes", Some("7"), Episode);
        check("[GroupE]Yes! Pretty Cure 5 - 21[BD][1080p][0000BEEF].mkv", "Yes! Pretty Cure 5", Some("21"), Episode);
        check(
            "Lupin III S2 - 066 - Shooting Orders!! [GroupF][720p][12340066].mkv",
            "Lupin III S2",
            Some("66"),
            Episode,
        );
        check(
            "[GroupA] Log Horizon - 21 - The Two of Us Shall Waltz (BD 1080p AAC) [ABCD0021].mkv",
            "Log Horizon",
            Some("21"),
            Episode,
        );
    }

    #[test]
    fn versions() {
        let p =
            check("[GroupA] Ghost Meets Gal! - 03v2 (1080p) [0000AAAA].mkv", "Ghost Meets Gal!", Some("3"), Episode);
        assert_eq!(p.version, Some(2));
        let p = check(
            "[GroupB] Pocket Monsters (2023) 111 (1080p HEVC 10-bit) v2 [11111111].mkv",
            "Pocket Monsters (2023)",
            Some("111"),
            Episode,
        );
        assert_eq!(p.version, Some(2));
        check(
            "[GroupA] Cardfight!! Vanguard - Divinez Genma Seisen-hen - 01v2 (1080p) [AAAA0001].mkv",
            "Cardfight!! Vanguard - Divinez Genma Seisen-hen",
            Some("1"),
            Episode,
        );
        check(
            "[GroupA] Umayuru - Full Gate! - 01v2 (1080p) [BBBB0001].mkv",
            "Umayuru - Full Gate!",
            Some("1"),
            Episode,
        );
    }

    #[test]
    fn dotted_season_episode() {
        let p = check("Chainsmoker.Cat.S01E07.1080p.WEB.AAC2.0.H.264-GRP.mkv", "Chainsmoker Cat", Some("7"), Episode);
        assert_eq!(p.season, Some(1));
        assert_eq!(p.group.as_deref(), Some("GRP"));
        let p = check(
            "Ranma.1-2.2024.S03E01.Training.Meals.1080p.WEB.DUAL.AAC2.0.H.264-GRP.mkv",
            "Ranma 1-2 2024",
            Some("1"),
            Episode,
        );
        assert_eq!(p.season, Some(3));
        check(
            "JoJos.Bizarre.Adventure.S06E02.The.Sheriffs.Request.1080p.WEB.DUAL.AAC2.0.H.264-GRP.mkv",
            "JoJos Bizarre Adventure",
            Some("2"),
            Episode,
        );
        check(
            "[GroupA] Dungeons & Television - S01E01 [WEB 1080P AVC, Opus MULTi-AUD, MULTi][DDDD0001].mkv",
            "Dungeons & Television",
            Some("1"),
            Episode,
        );
        check("Lupin III - S02E145 - [Japanese] Albatross, The Wings of Death.mkv", "Lupin III", Some("145"), Episode);
        check("Mobile.Fighter.G.Gundam.S01E37.mkv", "Mobile Fighter G Gundam", Some("37"), Episode);
        check(
            "New.PANTY.and.STOCKING.with.GARTERBELT.S01E02.2.3.1080p.WEB.DUAL.DDP5.1.H.264-GRP.mkv",
            "New PANTY and STOCKING with GARTERBELT",
            Some("2"),
            Episode,
        );
        check("Gundam.Wing.Ep.32.mkv", "Gundam Wing", Some("32"), Episode);
    }

    #[test]
    fn season_x_episode() {
        let p = check("Show 1x05.mkv", "Show", Some("5"), Episode);
        assert_eq!(p.season, Some(1));
        let p = check("Show - 2x13 - Title.mkv", "Show", Some("13"), Episode);
        assert_eq!(p.season, Some(2));
        let p = check("Some.Show.10x101.720p.mkv", "Some Show", Some("101"), Episode);
        assert_eq!(p.season, Some(10));
        // Resolutions and aspect ratios are not episodes.
        for name in ["Show 1920x1080.mkv", "Show 720x480.mkv", "Show 16x9.mkv", "Show 1x5.mkv"] {
            assert_eq!(parse(name).season, None, "{name}");
        }
        check("Show 1920x1080.mkv", "Show", None, Movie);
        check("Show 720x480 DVD.mkv", "Show", None, Movie);
        check("Show - 03 - 1x05 Recut.mkv", "Show", Some("3"), Episode);
        // Ranges, like `S01E05-E06`.
        for name in ["Show 1x05-06.mkv", "Show 1x05-1x06.mkv", "Show.1x05-06.720p.mkv"] {
            let p = check(name, "Show", Some("5"), Episode);
            assert_eq!((p.season, p.ep_end), (Some(1), ep("6")), "{name}");
        }
    }

    #[test]
    fn season_zero_is_specials() {
        for name in ["Show S00E05.mkv", "Show 0x05.mkv", "Show.S00E05.1080p.WEB-GRP.mkv", "Show - 0x05 - Title.mkv"] {
            let p = check(name, "Show", Some("5"), Special);
            assert_eq!((p.season, p.label.as_str()), (None, "special"), "{name}");
        }
    }

    #[test]
    fn leading_decimals_are_titles() {
        check("2.5 Jigen no Ririsa 01.mkv", "2.5 Jigen no Ririsa", Some("1"), Episode);
        check("2.5 Jigen no Ririsa - 03 [1080p].mkv", "2.5 Jigen no Ririsa", Some("3"), Episode);
        check("3.0+1.0 Thrice Upon a Time.mkv", "3.0+1.0 Thrice Upon a Time", None, Movie);
        check("05.Kino.mkv", "", Some("5"), Episode);
    }

    #[test]
    fn years_after_a_dash_are_not_episodes() {
        let p = check("Movie Title - 2024.mkv", "Movie Title", None, Movie);
        assert_eq!(p.item_key().label, "movie title");
        check("Movie Title - 2024 [1080p].mkv", "Movie Title", None, Movie);
        check("Show - 2024 - 05.mkv", "Show - 2024", Some("5"), Episode);
        check("Show - 2024v2.mkv", "Show", Some("2024"), Episode);
        check("[GroupA] One Piece - 1180 (1080p).mkv", "One Piece", Some("1180"), Episode);
        check("[GroupA] Show - 2051 (1080p).mkv", "Show", Some("2051"), Episode);
    }

    #[test]
    fn version_like_decimals_are_not_episodes() {
        let p = check("Evangelion 3.0 You Can (Not) Redo.mkv", "Evangelion 3.0 You Can Redo", None, Movie);
        assert_eq!(p.item_key().label, "evangelion 3 0 you can redo");
        check("Show 2.0.mkv", "Show 2.0", None, Movie);
        check("Show 2.1.mkv", "Show 2.1", None, Movie);
        // Half episodes keep working, marked or not.
        check("Show 12.5.mkv", "Show", Some("12.5"), Episode);
        check("Show - 12.5.mkv", "Show", Some("12.5"), Episode);
        check("Show - 3.0.mkv", "Show", Some("3.0"), Episode);
        check("Show Episode 12.5.mkv", "Show", Some("12.5"), Episode);
        check("Show.E12.mkv", "Show", Some("12"), Episode);
        // A real marker later in the name still wins over the decimal.
        check("Show 2.0 05.mkv", "Show 2.0", Some("5"), Episode);
    }

    #[test]
    fn underscores() {
        check("[GroupB]Powerpuff_Girls_Z_-_09_[0000ABCD].avi", "Powerpuff Girls Z", Some("9"), Episode);
        check("[Group-C]_Gokinjo_Monogatari_-_36_[ABCD0036].mkv", "Gokinjo Monogatari", Some("36"), Episode);
        check(
            "[GroupD]_Mahou_no_Star_Magical_Emi_23_[h264&AAC]_[ABCD0023].mkv",
            "Mahou no Star Magical Emi",
            Some("23"),
            Episode,
        );
        check("Steam_Detectives _Ep.23[h.264-AAC][GroupE][ABCD0024].mkv", "Steam Detectives", Some("23"), Episode);
        check(
            "[GroupF]_Jojo's_Bizarre_Adventure_23_(1920x1080_BD_FLAC)_[ABCD0025].mkv",
            "Jojo's Bizarre Adventure",
            Some("23"),
            Episode,
        );
    }

    #[test]
    fn bare_numbers() {
        check("Azumanga Daiou 24 [ABCD0124].mkv", "Azumanga Daiou", Some("24"), Episode);
        check("Danball Senki Wars 37.mp4", "Danball Senki Wars", Some("37"), Episode);
        check(
            "[GroupA] Bakusou Kyoudai Let's & Go!! WGP 10.mkv",
            "Bakusou Kyoudai Let's & Go!! WGP",
            Some("10"),
            Episode,
        );
        check(
            "[GroupB] Pocket Monsters (2023) 116 (1080p HEVC 10-bit) [11111116].mkv",
            "Pocket Monsters (2023)",
            Some("116"),
            Episode,
        );
        check(
            "[GroupC] Sailor Moon SuperS 162 (DVD.H264.AC3) [ABCD0162].mkv",
            "Sailor Moon SuperS",
            Some("162"),
            Episode,
        );
        check(
            "[GroupB] POKÉTOON 21 - The Summer That Goes On (1080p VP9 Opus) [ABCD0221].mkv",
            "POKÉTOON",
            Some("21"),
            Episode,
        );
        check(
            "[GroupD]_K-On!_B-Side_Theater_-_Uraon_05_[BD][ABCD0005].mkv",
            "K-On! B-Side Theater - Uraon",
            Some("5"),
            Episode,
        );
        check(
            "Transformable Shinkansen Robot Shinkalion Z 17 [ABCD0017].mkv",
            "Transformable Shinkansen Robot Shinkalion Z",
            Some("17"),
            Episode,
        );
    }

    #[test]
    fn leading_numbers() {
        check("41 - Princess In Peril! The Pilfered Carafe!!.mkv", "", Some("41"), Episode);
        check("05. Kino no Tabi - the Beautiful World [Hi10p AAC][GroupA].mkv", "", Some("5"), Episode);
        check("19 - I Want To Be Stronger!.mkv", "", Some("19"), Episode);
    }

    #[test]
    fn episode_word() {
        check("Yuusha Exkaiser - Episode 06(GroupA)(VHS Audio)[ABCD0006].mkv", "Yuusha Exkaiser", Some("6"), Episode);
        let p = check(
            "Nekketsu Saikyo Go-Saurer - Episode 24v0[ABCD0024].mkv",
            "Nekketsu Saikyo Go-Saurer",
            Some("24"),
            Episode,
        );
        assert_eq!(p.version, Some(0));
    }

    #[test]
    fn extras_attached_to_episode() {
        let p =
            check("[GroupA] Meitantei Precure! - 20 Next Ep PV (1080p).mkv", "Meitantei Precure!", Some("20"), Extra);
        assert_eq!(p.label, "next ep pv");
        let p = check(
            "[GroupA] Meitantei Precure! - 19 Fanart Corner (1080p).mkv",
            "Meitantei Precure!",
            Some("19"),
            Extra,
        );
        assert_eq!(p.label, "fanart corner");
        check("[GroupA] Meitantei Precure! - 06v2 Fanart Corner (1080p).mkv", "Meitantei Precure!", Some("6"), Extra);
    }

    /// An episode title that merely contains an extras word is still an episode.
    #[test]
    fn episode_titles_with_extras_words_stay_episodes() {
        check("[GroupA] Show - 03 - Interview with the Vampire (1080p).mkv", "Show", Some("3"), Episode);
        check("[GroupA] Show - 01 - Episode Title With Preview (1080p).mkv", "Show", Some("1"), Episode);
        check("[GroupA] Show - 04 - PV (1080p).mkv", "Show", Some("4"), Extra);
        // Without a dash between number and title.
        check("[GroupA] Show - 05 The Preview Night (1080p).mkv", "Show", Some("5"), Episode);
        check("Show 03 - Interview with the Vampire.mkv", "Show", Some("3"), Episode);
        check("Show 03 Interview with the Vampire.mkv", "Show", Some("3"), Episode);
        check("Show - 05 Trailer 2 (1080p).mkv", "Show", Some("5"), Extra);
        check("Show 05 PV.mkv", "Show 05", None, Extra);
    }

    #[test]
    fn standalone_extras() {
        let p = check(
            "[GroupC] Senki Zesshou Symphogear - NCOP 1 [BD 720p AAC] [ABCD00C1].mkv",
            "Senki Zesshou Symphogear",
            None,
            Extra,
        );
        assert_eq!(p.label, "ncop 1");
        check("[GroupD] Shirobako - NCED1 [BD 720p] [ABCD00E1].mkv", "Shirobako", None, Extra);
        check("[GroupE] Yes! Precure 5 Go Go! NCOP (BD 720p).mkv", "Yes! Precure 5 Go Go!", None, Extra);
        let p = check("Clean Opening B.mkv", "", None, Extra);
        assert_eq!(p.label, "clean opening b");
        check("[GroupF] Houkago no Pleiades - Trailer 2 [BD 1080p][ABCD0002].mkv", "Houkago no Pleiades", None, Extra);
        let p = check(
            "[GroupA] Sayonara Lara Audio Drama 3 - Lara to Mari to Sugosu Shiki ~Aki~.mkv",
            "Sayonara Lara",
            None,
            Extra,
        );
        assert!(p.label.starts_with("audio drama 3"));
        check("[GroupB] My Dress-Up Darling - NCED [BD 1080p HEVC FLAC].mkv", "My Dress-Up Darling", None, Extra);
        check(
            "[GroupC] Girls und Panzer - das Finale 5 PV [WEB 1080p][ABCD0005].mkv",
            "Girls und Panzer - das Finale 5",
            None,
            Extra,
        );
        check("BOX3 Disc 8 Menu.mkv", "BOX3 Disc 8", None, Extra);
    }

    #[test]
    fn specials() {
        let p = check("[GroupD] Fortune Quest OVA 4.mkv", "Fortune Quest", Some("4"), Special);
        assert_eq!(p.label, "ova");
        let p = check(
            "[GroupE] Aria the Avvenire OVA - 02 [BD 1080p HEVC][FLAC][ABCD0002].mkv",
            "Aria the Avvenire",
            Some("2"),
            Special,
        );
        assert_eq!(p.label, "ova");
        let p = check(
            "[GroupC] Girls und Panzer - MLLSD - SP1 [WEB 1080p][ABCD00F1].mkv",
            "Girls und Panzer - MLLSD",
            Some("1"),
            Special,
        );
        assert_eq!(p.label, "sp");
        check(
            "[GroupB] Pocket Monsters (2023) - Pokémon Unite Special Anime (Post-141 Extra) (1080p VP9 Opus) [ABCD0141].mkv",
            "Pocket Monsters (2023) - Pokémon Unite",
            None,
            Special,
        );
    }

    #[test]
    fn movies() {
        let p = check("Kuramerukagari (2024) - 1080p WEB x264 -GroupA (JP).mkv", "Kuramerukagari (2024)", None, Movie);
        assert_eq!(p.item_key().label, "kuramerukagari 2024");
        check("[GroupB] Garden of Remembrance (1080p) [ABCD1080].mkv", "Garden of Remembrance", None, Movie);
        check("THE.RIBBON.HERO.2026.1080p.WEB.DUAL.DDP5.1.H.264-GRP.mkv", "THE RIBBON HERO", None, Movie);
        check(
            "MOBILE.SUIT.GUNDAM.HATHAWAY.The.Sorcery.Of.Nymph.Circe.2026.REPACK.1080p.WEB.DDP2.0.H.264-GRP.mkv",
            "MOBILE SUIT GUNDAM HATHAWAY The Sorcery Of Nymph Circe",
            None,
            Movie,
        );
        check(
            "Laid-Back Camp The Movie (2022) (BD Remux 1080p AVC TrueHD) [ABCD2022] [GroupC].mkv",
            "Laid-Back Camp The Movie (2022)",
            None,
            Movie,
        );
        check(
            "[GroupA] Detective Conan - A Hanamaru Answer (1080p).mkv",
            "Detective Conan - A Hanamaru Answer",
            None,
            Movie,
        );
    }

    #[test]
    fn numbered_movies_and_bracket_only() {
        let p = check(
            "[GroupA] Tensei shitara Slime Datta Ken Movie 2 - Soukai no Namida-hen (1080p) [ABCD0002].mkv",
            "Tensei shitara Slime Datta Ken",
            None,
            Movie,
        );
        assert_eq!(p.label, "movie 2");
        assert_eq!(p.item_key().label, "movie 2");
        let p =
            check("[GroupC] Girls und Panzer - Drama CD 3 - 1 [1080p][ABCD0031].mkv", "Girls und Panzer", None, Extra);
        assert_eq!(p.label, "drama cd 3 1");
        check("[字幕组][Z.O.E Dolores,i][02][BD X264 AAC 720P].mkv", "", Some("2"), Episode);
        let p = check("[Yes! Precure 5 Go Go!][NCED2][BD][1080P][H264_FLAC].mkv", "Yes! Precure 5 Go Go!", None, Extra);
        assert_eq!(p.label, "nced2");
    }

    #[test]
    fn multi_episode_and_e_only() {
        let p = check("[GroupD] Mahou Shoujotai - 09-10 [ABCD0910].avi", "Mahou Shoujotai", Some("9"), Episode);
        assert_eq!(p.ep_end, ep("10"));
        let p = check(
            "[GroupE] Urusei Yatsura - 001&002 [BD 1280x960 x264 Hi10P FLAC].mkv",
            "Urusei Yatsura",
            Some("1"),
            Episode,
        );
        assert_eq!(p.ep_end, ep("2"));
        let p = check("Lupin III S2 - 066 - Shooting Orders!!.mkv", "Lupin III S2", Some("66"), Episode);
        assert_eq!(p.ep_end, None);
        check("Grisaia.no.Kajitsu.E05.1080p.BluRay.x264-GRP.mkv", "Grisaia no Kajitsu", Some("5"), Episode);
        let p = check("Show.S01E01-E02.1080p.WEB.x264-GRP.mkv", "Show", Some("1"), Episode);
        assert_eq!(p.ep_end, ep("2"));
        let p = check("Show.S02E07E08.mkv", "Show", Some("7"), Episode);
        assert_eq!(p.ep_end, ep("8"));
        assert_eq!(p.season, Some(2));
    }

    #[test]
    fn weak_extras() {
        check("[GroupF] Planet With - NCOPv1 [BD 1080p][ABCD0001].mkv", "Planet With", None, Extra);
        check("[GroupA]_Onmyou_Taisenki_-_Opening2_v3_(DVD_h264_AC3)_[ABCD0002].mkv", "Onmyou Taisenki", None, Extra);
        check("Mahou no Yousei Persia - Bonus.mkv", "Mahou no Yousei Persia", None, Extra);
        check("Commercials.mkv", "", None, Extra);
        check("Astro Boy (Tetsuwan Atom) 2003 DVD 13 EXTRAS.mkv", "Astro Boy 2003", None, Extra);
    }

    #[test]
    fn directory_names() {
        check("[GroupA] Miru - Watashi no Mirai (01-05) (1080p) [Batch]", "Miru - Watashi no Mirai", None, Movie);
        check("[GroupA] Meitantei Precure! - 1-12 Batch (1080p)", "Meitantei Precure!", None, Movie);
        let p = parse("BEASTARS.S03.1080p.WEB.DUAL.DDP5.1.H.264-GRP");
        assert_eq!(p.title, "BEASTARS S03");
        check("One Piece", "One Piece", None, Movie);
    }

    #[test]
    fn video_ext() {
        assert!(is_video("a.MKV"));
        assert!(is_video("a.b.mp4"));
        assert!(!is_video("a.flac"));
        assert!(!is_video("mpvpipe"));
    }
}
