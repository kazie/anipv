//! Core domain types shared by the parser, index, event log and TUI.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// An episode number, stored in tenths so `12.5` recap episodes sort correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EpNo(u32);

impl EpNo {
    /// Highest episode number accepted from user input or file names.
    pub const MAX: u32 = 100_000;

    /// Whole episode number (saturating for absurdly large input).
    pub fn new(n: u32) -> Self {
        Self(n.saturating_mul(10))
    }

    /// The raw value in tenths.
    pub fn tenths(self) -> u32 {
        self.0
    }

    /// Whole part, e.g. `12` for `12.5`.
    pub fn whole(self) -> u32 {
        self.0 / 10
    }

    /// True for `x.0` numbers.
    pub fn is_whole(self) -> bool {
        self.0.is_multiple_of(10)
    }
}

impl fmt::Display for EpNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, tenth) = (self.0 / 10, self.0 % 10);
        // `pad` so callers can ask for width/fill (`{ep:0>2}`).
        if self.is_whole() { f.pad(&whole.to_string()) } else { f.pad(&format!("{whole}.{tenth}")) }
    }
}

impl FromStr for EpNo {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (whole, frac) = match s.split_once('.') {
            Some((w, f)) => (w, Some(f)),
            None => (s, None),
        };
        let w: u32 = whole.parse().map_err(|_| format!("bad episode number {s:?}"))?;
        let f: u32 = match frac {
            None => 0,
            Some(f) if f.len() == 1 => f.parse().map_err(|_| format!("bad episode number {s:?}"))?,
            Some(_) => return Err(format!("bad episode number {s:?}")),
        };
        if w > Self::MAX {
            return Err(format!("episode number {s:?} is too large"));
        }
        Ok(Self(w * 10 + f))
    }
}

impl Serialize for EpNo {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EpNo {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// What kind of thing a file is, relative to its series.
///
/// Serialized as its lowercase name. A kind written by a newer version reads
/// as [`ItemKind::Unknown`], so the event carrying it still parses (it stays in
/// the log) and is then ignored; this version never writes `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ItemKind {
    /// A regular numbered episode. Counts towards progress.
    Episode,
    /// OVA / OAD / SP / special: watchable, tracked, but not main progress.
    Special,
    /// Openings, PVs, fanart corners, menus… attached to a series or episode.
    Extra,
    /// Unnumbered single feature (movie, one-shot).
    #[default]
    Movie,
    /// A kind this version does not know (from a newer log). Never produced
    /// by the parser, never shown, never written.
    Unknown,
}

impl ItemKind {
    /// Stable short name used in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Episode => "episode",
            Self::Special => "special",
            Self::Extra => "extra",
            Self::Movie => "movie",
            Self::Unknown => "unknown",
        }
    }

    /// Inverse of [`ItemKind::as_str`]; strict, so the database never holds an
    /// `Unknown` (unlike event files, see [`ItemKind::from_name`]).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "episode" => Self::Episode,
            "special" => Self::Special,
            "extra" => Self::Extra,
            "movie" => Self::Movie,
            _ => return None,
        })
    }

    /// Like [`ItemKind::parse`], but an unrecognised name is [`ItemKind::Unknown`].
    pub fn from_name(s: &str) -> Self {
        Self::parse(s).unwrap_or(Self::Unknown)
    }

    /// False only for [`ItemKind::Unknown`]: kinds this version understands.
    pub fn is_known(self) -> bool {
        self != Self::Unknown
    }
}

impl Serialize for ItemKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if !self.is_known() {
            // The original name is lost, so writing it back would corrupt the log.
            return Err(serde::ser::Error::custom("an unknown item kind is never written"));
        }
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ItemKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = std::borrow::Cow::<str>::deserialize(d)?;
        Ok(Self::from_name(&name))
    }
}

/// Identifies one watchable item inside a series, independent of file paths.
///
/// Different copies/versions of the same episode share an `ItemKey`, so
/// watching `- 03v2` marks episode 3 regardless of which file was played.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ItemKey {
    /// Item kind.
    pub kind: ItemKind,
    /// Episode number; for extras, the episode they are attached to (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ep: Option<EpNo>,
    /// Disambiguating label (`"OVA"`, `"fanart corner"`, normalized title for movies…).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

impl ItemKey {
    /// A regular episode.
    pub fn episode(n: EpNo) -> Self {
        Self { kind: ItemKind::Episode, ep: Some(n), label: String::new() }
    }

    /// Short human description, e.g. `07`, `OVA 2`, `20 · fanart corner`.
    pub fn describe(&self) -> String {
        let ep = self.ep.map(|e| format!("{e:0>2}"));
        match (self.kind, ep) {
            (ItemKind::Episode, Some(e)) => e,
            (ItemKind::Episode, None) => "?".into(),
            (_, Some(e)) if self.label.is_empty() => e,
            (ItemKind::Special, Some(e)) => format!("{} {e}", self.label),
            (_, Some(e)) => format!("{e} · {}", self.label),
            (ItemKind::Movie, None) if self.label.is_empty() => "movie".into(),
            (_, None) => self.label.clone(),
        }
    }
}

/// User-chosen relationship with a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SeriesStatus {
    // `help` texts are shown in --help and shell completions; keep them short
    // and free of quotes (the fish completion generator doesn't escape them).
    /// Exists on disk, no decision made. Also what an unknown status (written by
    /// a newer version) reads as, so the event still applies.
    #[default]
    #[value(help = "On disk, no decision made")]
    Untracked,
    /// Actively watching; shows up in "Up next".
    #[value(help = "Watching; shows up in Up next")]
    Following,
    /// Stopped for now, may resume ("dropped too early").
    #[value(help = "Stopped for now, may resume")]
    Paused,
    /// Not interested; hidden by default.
    #[value(help = "Not interested; hidden by default")]
    Dropped,
    /// Finished.
    #[value(help = "Finished")]
    Completed,
    /// Never started and not interested (kept out of the inbox).
    #[value(help = "Never started, not interested")]
    Skipped,
}

impl SeriesStatus {
    /// All statuses in display order.
    pub const ALL: [Self; 6] =
        [Self::Following, Self::Paused, Self::Untracked, Self::Completed, Self::Dropped, Self::Skipped];

    /// Following, paused or completed: series the user cares about (not
    /// untracked, dropped or skipped).
    pub fn is_tracked(self) -> bool {
        !matches!(self, Self::Untracked | Self::Dropped | Self::Skipped)
    }

    /// Lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untracked => "untracked",
            Self::Following => "following",
            Self::Paused => "paused",
            Self::Dropped => "dropped",
            Self::Skipped => "skipped",
            Self::Completed => "completed",
        }
    }

    /// Parse a status name or its first letter.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.to_ascii_lowercase();
        Self::ALL.into_iter().find(|st| st.as_str() == s || (s.len() == 1 && st.as_str().starts_with(&s)))
    }
}

impl<'de> Deserialize<'de> for SeriesStatus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::ALL.into_iter().find(|st| st.as_str() == s).unwrap_or_default())
    }
}

impl fmt::Display for SeriesStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Watch state of a single item.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum WatchState {
    /// Never played (or explicitly reset).
    #[default]
    Unwatched,
    /// Played partially; resume position and duration in seconds.
    Started {
        /// Last known position (s).
        pos: f64,
        /// Duration (s), if known.
        dur: Option<f64>,
        /// When it was last played (unix seconds).
        at: i64,
    },
    /// Fully watched at the given unix time.
    Watched {
        /// When it was finished (unix seconds).
        at: i64,
    },
}

impl WatchState {
    /// One-character marker: `✓` watched, `◐` started, `·` unwatched.
    pub fn glyph(&self) -> &'static str {
        match self {
            Self::Watched { .. } => "✓",
            Self::Started { .. } => "◐",
            Self::Unwatched => "·",
        }
    }

    /// True if watched.
    pub fn is_watched(&self) -> bool {
        matches!(self, Self::Watched { .. })
    }

    /// Fraction played for started items.
    pub fn fraction(&self) -> Option<f64> {
        match self {
            Self::Started { pos, dur: Some(d), .. } => Some((pos / d).clamp(0.0, 1.0)),
            _ => None,
        }
    }

    /// `40%` for started items.
    pub fn percent(&self) -> Option<String> {
        self.fraction().map(|f| format!("{pct:.0}%", pct = f * 100.0))
    }

    /// Last interaction time, if any.
    pub fn at(&self) -> Option<i64> {
        match self {
            Self::Unwatched => None,
            Self::Started { at, .. } | Self::Watched { at } => Some(*at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epno_roundtrip() {
        for s in ["1", "12", "12.5", "1180", "0"] {
            let e: EpNo = s.parse().unwrap();
            assert_eq!(e.to_string(), s);
        }
        assert!("1.25".parse::<EpNo>().is_err());
        assert!("x".parse::<EpNo>().is_err());
        assert!("500000000".parse::<EpNo>().is_err(), "overflow is an error, not a panic");
        assert_eq!(EpNo::new(u32::MAX).tenths(), u32::MAX);
        assert!(EpNo::new(12) < "12.5".parse().unwrap());
    }

    #[test]
    fn epno_serde() {
        let k = ItemKey::episode(EpNo::new(7));
        let j = serde_json::to_string(&k).unwrap();
        assert_eq!(j, r#"{"kind":"episode","ep":"7"}"#);
        let back: ItemKey = serde_json::from_str(&j).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn item_kind_serde_is_forward_compatible() {
        for k in [ItemKind::Episode, ItemKind::Special, ItemKind::Extra, ItemKind::Movie] {
            assert!(k.is_known());
            let j = serde_json::to_string(&k).unwrap();
            assert_eq!(j, format!("\"{}\"", k.as_str()));
            assert_eq!(serde_json::from_str::<ItemKind>(&j).unwrap(), k);
        }
        // A kind from a newer version reads as unknown instead of failing…
        let k: ItemKind = serde_json::from_str(r#""ova2""#).unwrap();
        assert_eq!(k, ItemKind::Unknown);
        assert!(!k.is_known());
        // …in a whole item too, with its other fields intact…
        let key: ItemKey = serde_json::from_str(r#"{"kind":"ova2","ep":"3","label":"x","more":1}"#).unwrap();
        assert_eq!((key.kind, key.ep, key.label.as_str()), (ItemKind::Unknown, Some(EpNo::new(3)), "x"));
        // …but is never written, and the database parser stays strict.
        assert!(serde_json::to_string(&ItemKind::Unknown).is_err());
        assert!(serde_json::to_string(&key).is_err());
        assert_eq!(ItemKind::parse("ova2"), None);
        assert_eq!(ItemKind::parse("unknown"), None);
    }

    #[test]
    fn describe() {
        assert_eq!(ItemKey::episode(EpNo::new(7)).describe(), "07");
        assert_eq!(ItemKey::episode(EpNo::new(1180)).describe(), "1180");
        let sp = ItemKey { kind: ItemKind::Special, ep: Some(EpNo::new(2)), label: "OVA".into() };
        assert_eq!(sp.describe(), "OVA 02");
        let ex = ItemKey { kind: ItemKind::Extra, ep: Some(EpNo::new(20)), label: "fanart corner".into() };
        assert_eq!(ex.describe(), "20 · fanart corner");
    }

    #[test]
    fn status_serde_is_forward_compatible() {
        assert_eq!(serde_json::to_string(&SeriesStatus::Skipped).unwrap(), r#""skipped""#);
        assert_eq!(serde_json::from_str::<SeriesStatus>(r#""skipped""#).unwrap(), SeriesStatus::Skipped);
        // A status from a newer version reads as untracked instead of failing the event.
        assert_eq!(serde_json::from_str::<SeriesStatus>(r#""something_new""#).unwrap(), SeriesStatus::Untracked);
    }

    #[test]
    fn status_parse() {
        assert_eq!(SeriesStatus::parse("f"), Some(SeriesStatus::Following));
        assert_eq!(SeriesStatus::parse("Dropped"), Some(SeriesStatus::Dropped));
        assert_eq!(SeriesStatus::parse("zzz"), None);
    }

    #[test]
    fn fraction() {
        let s = WatchState::Started { pos: 50.0, dur: Some(100.0), at: 0 };
        assert_eq!(s.fraction(), Some(0.5));
        assert!(!s.is_watched());
    }
}
