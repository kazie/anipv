//! The Series view's search: words fuzzy-match a series' names, and
//! `field:value` tokens filter on its `AniList` data.
//!
//! * `studio:toho` — a main studio's name contains it
//! * `year:2023`, `year:2020..2023`, `year>=2020` — the year it started
//! * `season:fall` — the season it started (`autumn` too)
//! * `score>80`, `score<=60`, `score:70..80` — the audience score
//! * `#kusuriya` — one of its official hashtags contains it
//!
//! `"…"` quotes a value or words with spaces (`studio:"toho animation"`,
//! `"one piece"`). Any other token, an unknown `field:` included (`Re:Zero`),
//! is a word; a quoted token or one after `\` is always a word (`"#Compass"`,
//! `\#compass`). A known field without a usable value (`studio:`, `score>`,
//! `season:autum`) is left out, rather than hiding everything while it's
//! typed, and the search box says what the field takes ([`SeriesQuery::hint`]).

use crate::library::Series;

/// The fields, for the search box.
const FIELDS: &str = "studio:  year:  season:  score>  #tag";

/// What the search box asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SeriesQuery {
    /// The words, for the fuzzy match on names (empty: no fuzzy match), with
    /// the spaces of a quoted phrase escaped for the matcher.
    pub words: String,
    filters: Vec<Filter>,
    /// The first token left out, as typed, and what its field takes.
    ignored: Option<(String, &'static str)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Filter {
    /// Lowercase part of a studio's name.
    Studio(String),
    /// Start year in this range (inclusive).
    Year(i32, i32),
    /// `WINTER`, `SPRING`, `SUMMER`, `FALL`.
    Season(&'static str),
    /// Score in this range (inclusive).
    Score(i32, i32),
    /// Lowercase part of a hashtag.
    Hashtag(String),
}

/// What a token says.
enum Token {
    /// Words for the matcher.
    Word(String),
    Filter(Filter),
    /// A known field without a usable value (yet), and what the field takes.
    Incomplete(&'static str),
}

const STUDIO: &str = "studio: part of a studio's name";
const SEASON: &str = "season: winter spring summer fall";
const YEAR: &str = "year: 2023  2020..2023  >=2020  <2000";
const SCORE: &str = "score: >80  <=60  70..80";
const HASHTAG: &str = "#: part of a hashtag (\\# for a title)";

impl SeriesQuery {
    /// Read the search box's text.
    pub fn parse(text: &str) -> Self {
        let mut q = Self::default();
        let mut words: Vec<String> = Vec::new();
        for (raw, unquoted) in tokens(text) {
            match token(&raw, &unquoted) {
                Token::Word(w) => words.push(w),
                Token::Filter(f) => q.filters.push(f),
                Token::Incomplete(help) => {
                    q.ignored.get_or_insert((raw, help));
                }
            }
        }
        q.words = words.join(" ");
        q
    }

    /// For the search box: what a token left out needs (`warn`), else the fields.
    pub fn hint(&self) -> (String, bool) {
        match &self.ignored {
            Some((raw, help)) => (format!("ignored {raw} · {help}"), true),
            None => (FIELDS.to_string(), false),
        }
    }

    /// True if it filters on metadata.
    pub fn has_filters(&self) -> bool {
        !self.filters.is_empty()
    }

    /// True if `s` passes every filter (a series without metadata passes none).
    pub fn accepts(&self, s: &Series) -> bool {
        if self.filters.is_empty() {
            return true;
        }
        let Some(m) = &s.meta else { return false };
        let has = |list: &[String], part: &str| list.iter().any(|v| v.to_lowercase().contains(part));
        self.filters.iter().all(|f| match f {
            Filter::Studio(part) => has(&m.studios, part),
            Filter::Year(lo, hi) => m.year.is_some_and(|y| (*lo..=*hi).contains(&y)),
            Filter::Season(season) => m.season.as_deref() == Some(season),
            Filter::Score(lo, hi) => m.score.is_some_and(|sc| (*lo..=*hi).contains(&i32::from(sc))),
            Filter::Hashtag(part) => has(&m.hashtags, part),
        })
    }
}

/// Split on spaces outside `"…"`: each token as typed and without its quotes.
fn tokens(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (mut raw, mut unquoted, mut quoted) = (String::new(), String::new(), false);
    for c in text.chars() {
        if c.is_whitespace() && !quoted {
            if !raw.is_empty() {
                out.push((std::mem::take(&mut raw), std::mem::take(&mut unquoted)));
            }
            continue;
        }
        raw.push(c);
        if c == '"' {
            quoted = !quoted;
        } else {
            unquoted.push(c);
        }
    }
    if !raw.is_empty() {
        out.push((raw, unquoted));
    }
    out
}

/// What the token `raw` (`t` without its quotes) says.
fn token(raw: &str, t: &str) -> Token {
    if raw.starts_with('"') {
        return Token::Word(matcher_words(t));
    }
    if let Some(rest) = t.strip_prefix('\\') {
        return Token::Word(matcher_words(rest));
    }
    if let Some(tag) = t.strip_prefix('#') {
        return text_filter(tag, Filter::Hashtag, HASHTAG);
    }
    let lower = t.to_lowercase();
    if let Some(v) = lower.strip_prefix("studio:") {
        return text_filter(v, Filter::Studio, STUDIO);
    }
    if let Some(v) = lower.strip_prefix("season:") {
        let season = match v {
            "winter" => "WINTER",
            "spring" => "SPRING",
            "summer" => "SUMMER",
            "fall" | "autumn" => "FALL",
            _ => return Token::Incomplete(SEASON),
        };
        return Token::Filter(Filter::Season(season));
    }
    number_field(&lower, "year", Filter::Year, YEAR)
        .or_else(|| number_field(&lower, "score", Filter::Score, SCORE))
        .unwrap_or_else(|| Token::Word(matcher_words(t)))
}

/// Words as the fuzzy matcher reads them: spaces inside a phrase escaped,
/// so `one piece` stays one part.
fn matcher_words(t: &str) -> String {
    t.split(' ').collect::<Vec<_>>().join("\\ ")
}

/// A text filter, or nothing while its value is empty.
fn text_filter(value: &str, filter: fn(String) -> Filter, help: &'static str) -> Token {
    if value.is_empty() { Token::Incomplete(help) } else { Token::Filter(filter(value.to_lowercase())) }
}

/// `field:N`, `field:A..B`, `field>N`, `field>=N`, `field<N`, `field<=N` as a
/// filter on an inclusive range; `None` if `t` isn't about `field`.
fn number_field(t: &str, field: &str, filter: fn(i32, i32) -> Filter, help: &'static str) -> Option<Token> {
    let rest = t.strip_prefix(field)?;
    let num = |s: &str| s.parse::<i32>().ok();
    let range = if let Some(v) = rest.strip_prefix(">=") {
        num(v).map(|n| (n, i32::MAX))
    } else if let Some(v) = rest.strip_prefix("<=") {
        num(v).map(|n| (i32::MIN, n))
    } else if let Some(v) = rest.strip_prefix('>') {
        num(v).and_then(|n| n.checked_add(1)).map(|n| (n, i32::MAX))
    } else if let Some(v) = rest.strip_prefix('<') {
        num(v).and_then(|n| n.checked_sub(1)).map(|n| (i32::MIN, n))
    } else {
        // Else `years`, `scored`: another word that starts with the field's name.
        let v = rest.strip_prefix(':')?;
        match v.split_once("..") {
            Some((lo, hi)) => num(lo).zip(num(hi)),
            None => num(v).map(|n| (n, n)),
        }
    };
    // A backwards range (`2023..2020`) is a mistake, not "nothing".
    Some(match range {
        Some((lo, hi)) if lo <= hi => Token::Filter(filter(lo, hi)),
        _ => Token::Incomplete(help),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters(text: &str) -> Vec<Filter> {
        SeriesQuery::parse(text).filters
    }

    #[test]
    fn words_and_fields() {
        let q = SeriesQuery::parse("apothecary studio:Toho diaries");
        assert_eq!(q.words, "apothecary diaries");
        assert_eq!(q.filters, [Filter::Studio("toho".into())]);
        assert!(!SeriesQuery::parse("frieren").has_filters());
    }

    #[test]
    fn years_and_scores() {
        assert_eq!(filters("year:2023"), [Filter::Year(2023, 2023)]);
        assert_eq!(filters("year:2020..2023"), [Filter::Year(2020, 2023)]);
        assert_eq!(filters("year>=2020"), [Filter::Year(2020, i32::MAX)]);
        assert_eq!(filters("year<2000"), [Filter::Year(i32::MIN, 1999)]);
        assert_eq!(filters("score>80"), [Filter::Score(81, i32::MAX)]);
        assert_eq!(filters("score<=60"), [Filter::Score(i32::MIN, 60)]);
        assert_eq!(filters("SCORE:70..80"), [Filter::Score(70, 80)]);
    }

    #[test]
    fn seasons_and_hashtags() {
        assert_eq!(filters("season:fall season:Autumn"), [Filter::Season("FALL"), Filter::Season("FALL")]);
        assert_eq!(filters("season:winter"), [Filter::Season("WINTER")]);
        assert_eq!(filters("#Kusuriya"), [Filter::Hashtag("kusuriya".into())]);
        assert_eq!(filters("#薬屋"), [Filter::Hashtag("薬屋".into())]);
    }

    #[test]
    fn quotes_hold_spaces() {
        let q = SeriesQuery::parse(r#"studio:"toho animation" "one piece""#);
        assert_eq!(q.filters, [Filter::Studio("toho animation".into())]);
        assert_eq!(q.words, r"one\ piece", "one part for the matcher");
        // Unclosed while typing: the same, so far.
        assert_eq!(SeriesQuery::parse(r#""one pie"#).words, r"one\ pie");
    }

    /// A title that starts with `#` is found quoted or after `\`.
    #[test]
    fn hashtag_like_titles_are_words_when_escaped() {
        for text in [r##""#Compass""##, r"\#Compass"] {
            let q = SeriesQuery::parse(text);
            assert_eq!((q.words.as_str(), q.has_filters()), ("#Compass", false), "{text}");
        }
        assert!(SeriesQuery::parse("#compass").has_filters());
        assert_eq!(SeriesQuery::parse(r#""studio:x""#).words, "studio:x", "quoted: a word");
    }

    /// Unknown fields and words that start like a field are words.
    #[test]
    fn words_that_look_like_fields() {
        for text in ["Re:Zero", "years", "scored"] {
            let q = SeriesQuery::parse(text);
            assert_eq!((q.words.as_str(), q.has_filters()), (text, false), "{text}");
        }
    }

    /// A known field without a usable value hides nothing, and the hint says
    /// what the field takes; otherwise the hint lists the fields.
    #[test]
    fn unusable_values_are_left_out_and_explained() {
        for (typing, help) in [
            ("studio:", STUDIO),
            ("score>", SCORE),
            ("year:20..", YEAR),
            ("year:2023..2020", YEAR),
            ("season:autum", SEASON),
            ("score>abc", SCORE),
            ("#", HASHTAG),
        ] {
            let q = SeriesQuery::parse(&format!("frieren {typing}"));
            assert_eq!((q.words.as_str(), q.has_filters()), ("frieren", false), "{typing}");
            assert_eq!(q.hint(), (format!("ignored {typing} · {help}"), true));
        }
        assert_eq!(SeriesQuery::parse("frieren season:fall").hint(), (FIELDS.to_string(), false));
        let first = SeriesQuery::parse("studio: season:x").hint().0;
        assert!(first.starts_with("ignored studio:"), "the first one: {first}");
    }
}
