//! The append-only event log: the single source of truth for watch state.
//!
//! Each device appends to its own `<device>.jsonl` inside the events
//! directory. Because no two devices ever write the same file, syncing the
//! directory with Syncthing, rsync or git can never produce conflicts.
//! State is rebuilt by reading *all* files and replaying events ordered by
//! `(ts, dev, file, line)`. See `docs/events.md` for the format.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{ItemKey, SeriesStatus, WatchState};

/// Current unix time in seconds.
pub fn now() -> i64 {
    unix_secs(std::time::SystemTime::now())
}

/// Seconds since the unix epoch (0 before it, saturating far in the future).
pub fn unix_secs(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// One line of the event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Unix timestamp (seconds).
    pub ts: i64,
    /// Device that recorded the event.
    pub dev: String,
    /// What happened.
    #[serde(flatten)]
    pub body: EventBody,
}

impl Event {
    /// An event from `dev` at `ts`.
    pub fn new(ts: i64, dev: impl Into<String>, body: EventBody) -> Self {
        Self { ts, dev: dev.into(), body }
    }
}

impl EventBody {
    /// The item an event is about, if it is an item event.
    pub fn item(&self) -> Option<&ItemKey> {
        match self {
            Self::Watched { item, .. } | Self::Progress { item, .. } | Self::Unwatched { item, .. } => Some(item),
            _ => None,
        }
    }

    /// The series key a series event names (`None` for merges and unknown kinds).
    pub fn series(&self) -> Option<&str> {
        match self {
            Self::Watched { series, .. }
            | Self::Progress { series, .. }
            | Self::Unwatched { series, .. }
            | Self::SeriesStatus { series, .. }
            | Self::Title { series, .. }
            | Self::Meta { series, .. }
            | Self::Unlink { series } => Some(series),
            Self::Alias { .. } | Self::Unalias { .. } | Self::Unknown => None,
        }
    }

    /// Every series key the event names: both sides of a merge, the key an
    /// unmerge splits off, or [`EventBody::series`].
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        let (a, b) = match self {
            Self::Alias { from, to } => (Some(from.as_str()), Some(to.as_str())),
            Self::Unalias { from } => (Some(from.as_str()), None),
            body => (body.series(), None),
        };
        a.into_iter().chain(b)
    }

    /// A status change made by the user.
    pub fn status(series: impl Into<String>, status: SeriesStatus, note: Option<String>) -> Self {
        Self::SeriesStatus { series: series.into(), status, note, auto: false }
    }

    /// `watched` for an item, without a file name.
    pub fn watched(series: impl Into<String>, item: ItemKey) -> Self {
        Self::Watched { series: series.into(), item, file: None }
    }
}

/// Event payloads. Unknown kinds (from newer versions) are ignored on replay,
/// and so are item events whose item kind this version does not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum EventBody {
    /// An item was watched to the end (or marked watched).
    Watched {
        /// Series key.
        series: String,
        /// Item within the series.
        item: ItemKey,
        /// File name that was played, informational.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
    /// Partial playback.
    Progress {
        /// Series key.
        series: String,
        /// Item within the series.
        item: ItemKey,
        /// Position in seconds.
        pos: f64,
        /// Duration in seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dur: Option<f64>,
        /// File name that was played, informational.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
    /// Reset an item to unwatched.
    Unwatched {
        /// Series key.
        series: String,
        /// Item within the series.
        item: ItemKey,
        /// A file name of the item, kept as evidence for re-keying.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
    },
    /// Change a series' status.
    SeriesStatus {
        /// Series key.
        series: String,
        /// New status.
        status: SeriesStatus,
        /// Optional free-form note ("dropped after ep 3, too slow").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// Set by anipv itself (e.g. completed after the last episode), not the user.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        auto: bool,
    },
    /// Treat series `from` as part of series `to` (merging two names).
    Alias {
        /// Key being merged away.
        from: String,
        /// Target key.
        to: String,
    },
    /// Undo an alias.
    Unalias {
        /// Key whose alias is removed.
        from: String,
    },
    /// Override a series' display title.
    Title {
        /// Series key.
        series: String,
        /// Title to display.
        title: String,
    },
    /// Link a series to `AniList` and/or set its episode total (whichever is present).
    Meta {
        /// Series key.
        series: String,
        /// `AniList` id to link.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anilist: Option<u64>,
        /// Manual total episode count.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        episodes: Option<u32>,
    },
    /// Remove the `AniList` link and stop matching this series automatically.
    Unlink {
        /// Series key.
        series: String,
    },
    /// An event kind this version does not know.
    #[serde(other)]
    Unknown,
}

/// Writes events for the local device and reads everyone's.
#[derive(Debug, Clone)]
pub struct EventLog {
    dir: PathBuf,
    device: String,
}

impl EventLog {
    /// Open (and create) the events directory for `device`.
    pub fn open(dir: impl Into<PathBuf>, device: impl Into<String>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let device = sanitize_device(&device.into());
        Ok(Self { dir, device })
    }

    /// The device name used for new events.
    pub fn device(&self) -> &str {
        &self.device
    }

    /// The events directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of this device's log file.
    pub fn own_file(&self) -> PathBuf {
        self.dir.join(format!("{}.jsonl", self.device))
    }

    /// Append events stamped with the current time.
    pub fn append(&self, bodies: impl IntoIterator<Item = EventBody>) -> Result<Vec<Event>> {
        let ts = now();
        let events: Vec<Event> = bodies.into_iter().map(|body| Event::new(ts, self.device.as_str(), body)).collect();
        self.append_events(&events)?;
        Ok(events)
    }

    /// Append fully formed events (e.g. imported with historical timestamps).
    pub fn append_events(&self, events: &[Event]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        // Serialize first: an event that can't be written leaves the file alone.
        let mut lines = String::new();
        for e in events {
            lines.push_str(&serde_json::to_string(e)?);
            lines.push('\n');
        }
        let path = self.own_file();
        let mut f = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut buf = String::new();
        // A crash or a sync tool can leave half a line at the end; start on a
        // fresh line so the new events aren't glued onto it and lost with it.
        if !ends_with_newline(&mut f)? {
            buf.push('\n');
        }
        buf.push_str(&lines);
        f.write_all(buf.as_bytes())?;
        f.sync_data()?;
        Ok(())
    }

    /// Read and order all events from every device file.
    pub fn load_all(&self) -> Result<Loaded> {
        load_dir(&self.dir)
    }

    /// [`EventLog::load_all`], kept with what the log files looked like, so
    /// [`EventLog::refresh`] reads them again only when they change.
    pub fn load_cached(&self) -> Result<CachedLog> {
        // Stamped first: a change made while reading is seen by the next refresh.
        let stamps = stamps(&self.dir)?;
        Ok(CachedLog { events: self.load_all()?.events, stamps })
    }

    /// Read the log again if any log file changed since `cache` was read
    /// (another device's events arriving, another anipv process writing).
    /// Returns whether it did.
    pub fn refresh(&self, cache: &mut CachedLog) -> Result<bool> {
        if stamps(&self.dir)? == cache.stamps {
            return Ok(false);
        }
        *cache = self.load_cached()?;
        Ok(true)
    }

    /// Add events just appended by [`EventLog::append`] to `cache`, where
    /// reading the log again would put them, without reading it again.
    pub fn merge(&self, cache: &mut CachedLog, recorded: Vec<Event>) {
        if recorded.is_empty() {
            return;
        }
        // This device's file grew by exactly these lines unless something else
        // wrote to it too; then its stamp stays stale and the next refresh reads it.
        let own = self.own_file();
        let name = own.file_name().unwrap_or_default();
        let written: u64 = recorded.iter().map(|e| serde_json::to_string(e).map_or(0, |s| s.len() as u64 + 1)).sum();
        let now = stamp(&own);
        match (cache.stamps.iter_mut().find(|s| s.0 == name), now) {
            (Some(s), Some(now)) if s.1 + written == now.1 => *s = (name.to_owned(), now.1, now.2),
            (None, Some(now)) if now.1 == written => {
                cache.stamps.push((name.to_owned(), now.1, now.2));
                cache.stamps.sort();
            }
            _ => {}
        }
        add_events(&mut cache.events, recorded);
    }
}

/// The event log as last read (see [`EventLog::load_cached`]).
#[derive(Debug, Clone, Default)]
pub struct CachedLog {
    events: Vec<Event>,
    /// `(file name, length, modification time)` of each log file, by name.
    stamps: Vec<Stamp>,
}

impl CachedLog {
    /// The events, ordered as [`EventLog::load_all`] orders them.
    pub fn events(&self) -> &[Event] {
        &self.events
    }
}

type Stamp = (std::ffi::OsString, u64, Option<std::time::SystemTime>);

/// [`Stamp`]s of the `*.jsonl` files in `dir`, by name.
fn stamps(dir: &Path) -> Result<Vec<Stamp>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut out: Vec<Stamp> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        // A file that can't be stat'ed stamps as empty: once readable, it differs.
        .map(|p| stamp(&p).unwrap_or_else(|| (p.file_name().unwrap_or_default().to_owned(), 0, None)))
        .collect();
    out.sort();
    Ok(out)
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some((path.file_name()?.to_owned(), m.len(), m.modified().ok()))
}

/// Add events just appended to this device's log to `events` (ordered like
/// [`EventLog::load_all`]) where loading the log again would put them: after
/// everything with the same time and device.
fn add_events(events: &mut Vec<Event>, recorded: Vec<Event>) {
    if recorded.is_empty() {
        return;
    }
    events.extend(recorded);
    // Stable, and nearly sorted already.
    events.sort_by(|a, b| (a.ts, &a.dev).cmp(&(b.ts, &b.dev)));
}

/// True for an empty file or one whose last byte is `\n`.
fn ends_with_newline(f: &mut std::fs::File) -> std::io::Result<bool> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    if f.metadata()?.len() == 0 {
        return Ok(true);
    }
    f.seek(SeekFrom::End(-1))?;
    let mut last = [0u8];
    f.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// True for the characters a device name may use as-is in its event log's
/// file name: letters, digits, `-` and `_`.
pub fn is_device_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

fn sanitize_device(d: &str) -> String {
    let s: String = d.chars().map(|c| if is_device_char(c) { c } else { '_' }).collect();
    if s.is_empty() { "device".into() } else { s }
}

/// Events loaded from disk, plus lines that could not be parsed.
#[derive(Debug, Default)]
pub struct Loaded {
    /// Ordered events.
    pub events: Vec<Event>,
    /// `file:line: error` for each malformed line.
    pub errors: Vec<String>,
}

/// Read every `*.jsonl` in `dir` and order events deterministically.
pub fn load_dir(dir: &Path) -> Result<Loaded> {
    let mut out = Loaded::default();
    // (file index in path order, line number, event); sorted by
    // (ts, dev, file, line) below. Line numbers restart in every file, and two
    // files can hold the same `dev` (a Syncthing conflict copy, two machines
    // with one name), so the file keeps each file's events together.
    let mut tagged: Vec<(usize, usize, Event)> = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    for (fi, path) in files.into_iter().enumerate() {
        let f = std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        // Read raw bytes: serde validates UTF-8, so a line cut off mid-character
        // (e.g. by a sync tool) is reported and skipped like any malformed line.
        let mut reader = BufReader::new(f);
        let mut line = Vec::new();
        for i in 0.. {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.trim_ascii().is_empty() {
                continue;
            }
            match serde_json::from_slice::<Event>(&line) {
                Ok(ev) => tagged.push((fi, i, ev)),
                Err(e) => out.errors.push(format!("{file}:{line}: {e}", file = path.display(), line = i + 1)),
            }
        }
    }
    tagged.sort_by(|(fa, la, a), (fb, lb, b)| (a.ts, &a.dev, fa, la).cmp(&(b.ts, &b.dev, fb, lb)));
    out.events = tagged.into_iter().map(|t| t.2).collect();
    Ok(out)
}

/// How a series is linked to `AniList`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Link {
    /// Not set by the user: matched automatically.
    #[default]
    Auto,
    /// Linked by hand to this `AniList` id.
    Manual(u64),
    /// The user removed the link: never auto-match.
    Off,
}

impl Link {
    /// The manually linked id, if any.
    pub fn manual(self) -> Option<u64> {
        match self {
            Self::Manual(id) => Some(id),
            _ => None,
        }
    }
}

/// Series-level state derived from events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesState {
    /// Current status.
    pub status: SeriesStatus,
    /// Note attached to the last status change.
    pub note: Option<String>,
    /// Display title override.
    pub title: Option<String>,
    /// `AniList` link.
    pub link: Link,
    /// anipv has completed this series automatically before (so it won't
    /// again after the user changes the status back).
    pub auto_completed: bool,
    /// Manual episode total.
    pub episodes: Option<u32>,
}

/// `key` read through `map` (old spelling → current key, see
/// [`State::replay_with`]): the key it maps to, or itself.
pub fn rekey<'a>(map: &'a HashMap<String, String>, key: &'a str) -> &'a str {
    map.get(key).map_or(key, String::as_str)
}

/// Follow alias chains from `key` to its canonical key.
///
/// Two devices can merge in opposite directions before syncing (`a→b` and
/// `b→a`). Every key on such a cycle resolves to the cycle's smallest key, so
/// they all agree on one series instead of swapping places.
pub fn resolve_alias<'a>(aliases: &'a HashMap<String, String>, key: &'a str) -> &'a str {
    // Most keys are not merged anywhere.
    let Some(mut next) = aliases.get(key).map(String::as_str) else { return key };
    let mut path: Vec<&'a str> = vec![key];
    loop {
        if let Some(i) = path.iter().position(|k| *k == next) {
            return path[i..].iter().copied().min().unwrap_or(next);
        }
        path.push(next);
        match aliases.get(next) {
            Some(n) => next = n,
            None => return next,
        }
    }
}

/// Result of replaying the event log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// Watch state per resolved series key and item.
    pub items: HashMap<String, HashMap<ItemKey, WatchState>>,
    /// Per-series state, keyed by resolved series key.
    pub series: HashMap<String, SeriesState>,
    /// Alias map: merged-away key → target key (unresolved, as recorded).
    pub aliases: HashMap<String, String>,
    /// Last file name recorded per series, used as a title hint when the
    /// files themselves are gone.
    pub file_hints: HashMap<String, String>,
}

impl State {
    /// Follow alias chains (cycle-safe) to the canonical series key.
    pub fn resolve<'a>(&'a self, key: &'a str) -> &'a str {
        resolve_alias(&self.aliases, key)
    }

    /// Watch state of an item (unwatched if unknown).
    #[cfg(test)]
    pub fn item(&self, series: &str, item: &ItemKey) -> WatchState {
        self.items.get(self.resolve(series)).and_then(|m| m.get(item)).copied().unwrap_or_default()
    }

    /// [`State::replay_with`] without a key map.
    #[cfg(test)]
    pub fn replay(events: &[Event]) -> Self {
        Self::replay_with(events, &HashMap::new())
    }

    /// Replay ordered events into state.
    ///
    /// Merges are settled first; then every event applies, in log order, to its
    /// series' canonical key. "Newest wins" therefore holds for every field,
    /// including items and links recorded under a name that was merged later.
    ///
    /// Every series key an event names (in any event kind, merges included)
    /// is read through `map` (see [`rekey`]) first: keys recorded under an old
    /// spelling (see [`crate::identity::legacy_series_key`]) are treated as
    /// the current key they map to.
    pub fn replay_with(events: &[Event], map: &HashMap<String, String>) -> Self {
        let mut aliases = HashMap::new();
        for ev in events {
            match &ev.body {
                EventBody::Alias { from, to } => {
                    let (from, to) = (rekey(map, from), rekey(map, to));
                    if from != to {
                        aliases.insert(from.to_string(), to.to_string());
                    }
                }
                EventBody::Unalias { from } => {
                    aliases.remove(rekey(map, from));
                }
                _ => {}
            }
        }

        let mut state = Self::default();
        let canon = |series: &str| resolve_alias(&aliases, rekey(map, series)).to_string();
        for ev in events {
            // An item of a kind from a newer version: the line stays in the
            // log, but means nothing here (no item, no watch state).
            if ev.body.item().is_some_and(|i| !i.kind.is_known()) {
                continue;
            }
            match &ev.body {
                EventBody::Watched { series, item, file } => {
                    let key = canon(series);
                    if let Some(f) = file {
                        state.file_hints.insert(key.clone(), f.clone());
                    }
                    state.items.entry(key).or_default().insert(item.clone(), WatchState::Watched { at: ev.ts });
                }
                EventBody::Progress { series, item, pos, dur, .. } => {
                    let slot = state.items.entry(canon(series)).or_default().entry(item.clone()).or_default();
                    // Re-watching doesn't un-watch.
                    if !slot.is_watched() {
                        // Older logs may carry a duration of 0 for "unknown".
                        let dur = dur.filter(|d| *d > 0.0);
                        *slot = WatchState::Started { pos: *pos, dur, at: ev.ts };
                    }
                }
                EventBody::Unwatched { series, item, .. } => {
                    state.items.entry(canon(series)).or_default().insert(item.clone(), WatchState::Unwatched);
                }
                EventBody::SeriesStatus { series, status, note, auto } => {
                    let s = state.series.entry(canon(series)).or_default();
                    s.status = *status;
                    s.note.clone_from(note);
                    s.auto_completed |= *auto && *status == SeriesStatus::Completed;
                }
                EventBody::Title { series, title } => {
                    state.series.entry(canon(series)).or_default().title = Some(title.clone());
                }
                EventBody::Meta { series, anilist, episodes } => {
                    let s = state.series.entry(canon(series)).or_default();
                    if let Some(id) = anilist {
                        s.link = Link::Manual(*id);
                    }
                    if episodes.is_some() {
                        s.episodes = *episodes;
                    }
                }
                EventBody::Unlink { series } => {
                    state.series.entry(canon(series)).or_default().link = Link::Off;
                }
                EventBody::Alias { .. } | EventBody::Unalias { .. } | EventBody::Unknown => {}
            }
        }
        state.aliases = aliases;
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EpNo, ItemKind};
    use proptest::prelude::*;

    use Event as E;

    fn ev(ts: i64, dev: &str, body: EventBody) -> Event {
        E::new(ts, dev, body)
    }

    fn watched(series: &str, n: u32) -> EventBody {
        EventBody::watched(series, ItemKey::episode(EpNo::new(n)))
    }

    #[test]
    fn json_shape() {
        let e = ev(10, "desk", watched("one piece", 1180));
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(
            j,
            r#"{"ts":10,"dev":"desk","e":"watched","series":"one piece","item":{"kind":"episode","ep":"1180"}}"#
        );
        assert_eq!(serde_json::from_str::<Event>(&j).unwrap(), e);
    }

    #[test]
    fn unknown_events_and_fields_are_tolerated() {
        let e: Event = serde_json::from_str(r#"{"ts":1,"dev":"x","e":"from_the_future","foo":1}"#).unwrap();
        assert_eq!(e.body, EventBody::Unknown);
        // Newer versions may add fields to existing kinds.
        let e: Event = serde_json::from_str(
            r#"{"ts":1,"dev":"x","e":"watched","series":"s","item":{"kind":"episode","ep":"1","future":true},"rating":5}"#,
        )
        .unwrap();
        assert_eq!(e.body, watched("s", 1));
    }

    /// An item kind from a newer version doesn't fail its line: the event is
    /// read and stays in the log, but builds no state, and its neighbours apply.
    #[test]
    fn unknown_item_kinds_are_kept_but_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::open(dir.path(), "desk").unwrap();
        let file = dir.path().join("desk.jsonl");
        let future = r#"{"ts":2,"dev":"desk","e":"watched","series":"x","item":{"kind":"ova2","ep":"1","tags":["a"]},"file":"x.mkv","rating":5}"#;
        let progress = r#"{"ts":3,"dev":"desk","e":"progress","series":"x","item":{"kind":"ova2","ep":"2"},"pos":5.0}"#;
        let text = format!(
            "{}\n{future}\n{progress}\n{}\n",
            r#"{"ts":1,"dev":"desk","e":"watched","series":"x","item":{"kind":"episode","ep":"1"}}"#,
            r#"{"ts":4,"dev":"desk","e":"watched","series":"x","item":{"kind":"episode","ep":"3"}}"#,
        );
        std::fs::write(&file, &text).unwrap();

        let loaded = log.load_all().unwrap();
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.events.len(), 4, "the unknown-kind events are read, not dropped");
        assert_eq!(loaded.events[1].body.item().unwrap().kind, ItemKind::Unknown);

        let state = State::replay(&loaded.events);
        let ep = |n| ItemKey::episode(EpNo::new(n));
        assert!(state.item("x", &ep(1)).is_watched());
        assert!(state.item("x", &ep(3)).is_watched(), "lines after it still apply");
        assert_eq!(state.items["x"].len(), 2, "no item, no watch state for the unknown kind");
        assert!(state.items["x"].keys().all(|i| i.kind.is_known()));
        assert!(state.file_hints.is_empty(), "not even a file hint: {:?}", state.file_hints);

        // Appending leaves what is already in the file as it was.
        log.append_events(&[ev(5, "desk", watched("x", 4))]).unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().starts_with(&text));
    }

    /// This version never writes an item of a kind it doesn't know.
    #[test]
    fn unknown_item_kinds_are_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::open(dir.path(), "desk").unwrap();
        let unknown = ItemKey { kind: ItemKind::Unknown, ep: Some(EpNo::new(1)), label: String::new() };
        let e = ev(1, "desk", EventBody::watched("x", unknown));
        assert!(log.append_events(&[e]).is_err());
        assert!(!log.own_file().exists(), "nothing is created for a refused event");
    }

    #[test]
    fn keys_an_event_names() {
        let keys = |b: EventBody| b.keys().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(keys(EventBody::Alias { from: "a".into(), to: "b".into() }), ["a", "b"]);
        assert_eq!(keys(EventBody::Unalias { from: "a".into() }), ["a"]);
        assert_eq!(keys(EventBody::Unlink { series: "s".into() }), ["s"]);
        assert!(keys(EventBody::Unknown).is_empty());
    }

    /// Re-keying covers every event kind (items, status, merges, titles, links),
    /// and the result is what replaying the log written under the new key gives.
    #[test]
    fn replay_with_rekey_reads_old_keys_as_the_new_one() {
        let rekey = HashMap::from([("old".to_string(), "new".to_string())]);
        let ep = ItemKey::episode(EpNo::new(1));
        let bodies = |k: &str| {
            vec![
                watched(k, 1),
                EventBody::Progress {
                    series: k.into(),
                    item: ItemKey::episode(EpNo::new(2)),
                    pos: 3.0,
                    dur: None,
                    file: None,
                },
                EventBody::Unwatched {
                    series: k.into(),
                    item: ItemKey::episode(EpNo::new(3)),
                    file: Some("f.mkv".into()),
                },
                EventBody::status(k, SeriesStatus::Paused, Some("n".into())),
                EventBody::Title { series: k.into(), title: "T".into() },
                EventBody::Meta { series: k.into(), anilist: Some(4), episodes: Some(5) },
                EventBody::Alias { from: "x".into(), to: k.into() },
                EventBody::Alias { from: k.into(), to: "y".into() },
                EventBody::Unalias { from: "x".into() },
                EventBody::Unlink { series: k.into() },
            ]
        };
        let log = |k: &str| -> Vec<Event> {
            bodies(k).into_iter().enumerate().map(|(i, b)| ev(i64::try_from(i).unwrap(), "a", b)).collect()
        };
        let migrated = State::replay_with(&log("old"), &rekey);
        assert_eq!(migrated, State::replay(&log("new")));
        assert!(migrated.item("new", &ep).is_watched());
        assert!(migrated.items.keys().all(|k| k == "y") && migrated.series.keys().all(|k| k == "y"), "merged into y");
        assert_eq!(migrated.aliases.get("new").map(String::as_str), Some("y"));
    }

    #[test]
    fn replay_basic() {
        let ep = ItemKey::episode(EpNo::new(3));
        let events = vec![
            ev(
                1,
                "a",
                EventBody::Progress { series: "x".into(), item: ep.clone(), pos: 10.0, dur: Some(100.0), file: None },
            ),
            ev(2, "a", watched("x", 3)),
            ev(
                3,
                "a",
                EventBody::Progress { series: "x".into(), item: ep.clone(), pos: 5.0, dur: Some(100.0), file: None },
            ),
        ];
        let s = State::replay(&events);
        assert_eq!(s.item("x", &ep), WatchState::Watched { at: 2 }, "rewatch progress keeps watched");
        let mut events = events;
        events.push(ev(4, "a", EventBody::Unwatched { series: "x".into(), item: ep.clone(), file: None }));
        assert_eq!(State::replay(&events).item("x", &ep), WatchState::Unwatched);
    }

    #[test]
    fn logged_zero_duration_reads_as_unknown() {
        let ep = ItemKey::episode(EpNo::new(1));
        let events = vec![ev(
            1,
            "a",
            EventBody::Progress { series: "x".into(), item: ep.clone(), pos: 40.0, dur: Some(0.0), file: None },
        )];
        let st = State::replay(&events).item("x", &ep);
        assert_eq!(st, WatchState::Started { pos: 40.0, dur: None, at: 1 });
        assert_eq!(st.fraction(), None);
    }

    #[test]
    fn aliases_merge_history() {
        let events = vec![
            ev(1, "a", watched("seitokai ni mo ana wa aru", 1)),
            ev(2, "a", watched("even the student council has its holes", 2)),
            ev(3, "a", EventBody::status("seitokai ni mo ana wa aru", SeriesStatus::Following, None)),
            ev(
                4,
                "a",
                EventBody::Alias {
                    from: "even the student council has its holes".into(),
                    to: "seitokai ni mo ana wa aru".into(),
                },
            ),
        ];
        let s = State::replay(&events);
        let canon = "seitokai ni mo ana wa aru";
        assert!(s.item(canon, &ItemKey::episode(EpNo::new(2))).is_watched());
        assert!(s.item("even the student council has its holes", &ItemKey::episode(EpNo::new(1))).is_watched());
        assert_eq!(s.series[canon].status, SeriesStatus::Following);
    }

    #[test]
    fn unwatching_after_a_merge_sticks() {
        let ep2 = ItemKey::episode(EpNo::new(2));
        let events = vec![
            ev(2, "a", watched("old name", 2)),
            ev(3, "a", EventBody::Alias { from: "old name".into(), to: "new name".into() }),
            ev(100, "a", EventBody::Unwatched { series: "new name".into(), item: ep2.clone(), file: None }),
        ];
        assert_eq!(State::replay(&events).item("new name", &ep2), WatchState::Unwatched);
    }

    #[test]
    fn unlink_is_remembered_and_wins_across_merges() {
        let events = vec![
            ev(1, "a", EventBody::Meta { series: "old name".into(), anilist: Some(5), episodes: None }),
            ev(2, "a", EventBody::Alias { from: "old name".into(), to: "new name".into() }),
            ev(3, "a", EventBody::Unlink { series: "new name".into() }),
            ev(4, "a", EventBody::Meta { series: "new name".into(), anilist: None, episodes: Some(12) }),
        ];
        let s = &State::replay(&events).series["new name"];
        assert_eq!(s.link, Link::Off);
        assert_eq!(s.episodes, Some(12), "an episodes-only event leaves the link alone");
    }

    #[test]
    fn alias_cycles_terminate() {
        let events = vec![
            ev(1, "a", EventBody::Alias { from: "a".into(), to: "b".into() }),
            ev(2, "a", EventBody::Alias { from: "b".into(), to: "a".into() }),
        ];
        let s = State::replay(&events);
        // Both keys end up as one series, not swapped.
        assert_eq!(s.resolve("a"), "a");
        assert_eq!(s.resolve("b"), "a");
        // A longer chain leading into a cycle resolves to the same key.
        let mut aliases: HashMap<String, String> = HashMap::new();
        for (f, t) in [("x", "c"), ("c", "d"), ("d", "e"), ("e", "c")] {
            aliases.insert(f.into(), t.into());
        }
        for k in ["x", "c", "d", "e"] {
            assert_eq!(resolve_alias(&aliases, k), "c");
        }
    }

    #[test]
    fn log_roundtrip_multi_device() {
        let dir = tempfile::tempdir().unwrap();
        let desk = EventLog::open(dir.path(), "desk").unwrap();
        let laptop = EventLog::open(dir.path(), "lap top").unwrap();
        assert_eq!(laptop.device(), "lap_top");
        desk.append_events(&[ev(5, "desk", watched("x", 2))]).unwrap();
        laptop.append_events(&[ev(3, "lap_top", watched("x", 1))]).unwrap();
        std::fs::write(dir.path().join("broken.jsonl"), b"not json\n\n{\"ts\":1,\"dev\":\"x\xff\"}\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "whatever").unwrap();
        let loaded = desk.load_all().unwrap();
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.events[0].ts, 3);
        assert_eq!(loaded.errors.len(), 2, "bad JSON and invalid UTF-8 are both reported: {:?}", loaded.errors);
    }

    /// Two files can carry the same `dev` (a Syncthing conflict copy, two
    /// machines with one name). Line numbers restart per file, so events with
    /// equal `(ts, dev)` stay grouped by file (in path order), not interleaved.
    #[test]
    fn same_device_in_two_files_does_not_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, series: [&str; 2]| {
            let text: String =
                series.iter().map(|s| serde_json::to_string(&ev(1, "desk", watched(s, 1))).unwrap() + "\n").collect();
            std::fs::write(dir.path().join(name), text).unwrap();
        };
        write("desk.jsonl", ["a1", "a2"]);
        write("desk.sync-conflict-20260101-000000-ABCDEF.jsonl", ["b1", "b2"]);
        let loaded = load_dir(dir.path()).unwrap();
        let order: Vec<&str> = loaded.events.iter().filter_map(|e| e.body.series()).collect();
        assert_eq!(order, ["a1", "a2", "b1", "b2"]);
    }

    /// A half-written last line (crash, full disk) costs only itself: the
    /// next append starts on a new line.
    #[test]
    fn append_after_a_truncated_line_keeps_new_events() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::open(dir.path(), "desk").unwrap();
        log.append_events(&[ev(1, "desk", watched("x", 1))]).unwrap();
        let file = dir.path().join("desk.jsonl");
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str(r#"{"ts":2,"dev":"de"#);
        std::fs::write(&file, text).unwrap();
        log.append_events(&[ev(3, "desk", watched("x", 2))]).unwrap();
        let loaded = log.load_all().unwrap();
        assert_eq!(loaded.events.iter().map(|e| e.ts).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
    }

    /// A cached log takes this device's appends without reading again, and
    /// reads again when any log file changes otherwise.
    #[test]
    fn cached_logs_are_read_again_only_after_outside_changes() {
        let dir = tempfile::tempdir().unwrap();
        let desk = EventLog::open(dir.path(), "desk").unwrap();
        let mut cache = desk.load_cached().unwrap();
        assert!(!desk.refresh(&mut cache).unwrap(), "nothing changed");
        for n in 1..=2 {
            let recorded = desk.append([watched("x", n)]).unwrap();
            desk.merge(&mut cache, recorded);
            assert!(!desk.refresh(&mut cache).unwrap(), "own append {n}");
        }
        assert_eq!(cache.events(), desk.load_all().unwrap().events);

        // Another device's file, then another writer of this device's file.
        EventLog::open(dir.path(), "laptop").unwrap().append([watched("y", 1)]).unwrap();
        assert!(desk.refresh(&mut cache).unwrap());
        assert_eq!(cache.events().len(), 3);
        desk.append([watched("x", 3)]).unwrap();
        let recorded = desk.append([watched("x", 4)]).unwrap();
        desk.merge(&mut cache, recorded);
        assert!(desk.refresh(&mut cache).unwrap(), "the other writer's line is read");
        assert_eq!(cache.events(), desk.load_all().unwrap().events);
    }

    #[test]
    fn missing_dir_is_empty() {
        let loaded = load_dir(Path::new("/nonexistent/anipv/events")).unwrap();
        assert!(loaded.events.is_empty());
    }

    fn arb_body() -> impl Strategy<Value = EventBody> {
        let series = prop::sample::select(vec!["a", "b", "c"]);
        let ep = 1u32..4;
        prop_oneof![
            (series.clone(), ep.clone()).prop_map(|(s, n)| watched(s, n)),
            (series.clone(), ep.clone(), 0.0f64..100.0).prop_map(|(s, n, p)| EventBody::Progress {
                series: s.into(),
                item: ItemKey::episode(EpNo::new(n)),
                pos: p,
                dur: Some(100.0),
                file: None
            }),
            (series.clone(), ep).prop_map(|(s, n)| EventBody::Unwatched {
                series: s.into(),
                item: ItemKey::episode(EpNo::new(n)),
                file: None
            }),
            (series.clone(), prop::sample::select(SeriesStatus::ALL.to_vec()))
                .prop_map(|(s, st)| EventBody::status(s, st, None)),
            (series.clone(), series).prop_map(|(a, b)| EventBody::Alias { from: a.into(), to: b.into() }),
        ]
    }

    proptest! {
        /// Splitting events across device files and reading them back in any
        /// file order yields the same state.
        #[test]
        fn replay_is_independent_of_file_layout(bodies in prop::collection::vec((arb_body(), 0usize..3), 0..40)) {
            let devs = ["desk", "laptop", "htpc"];
            let events: Vec<Event> = bodies.iter().enumerate()
                .map(|(i, (b, d))| ev(i64::try_from(i).unwrap() / 3, devs[*d], b.clone()))
                .collect();
            let dir = tempfile::tempdir().unwrap();
            for d in devs {
                let log = EventLog::open(dir.path(), d).unwrap();
                let mine: Vec<Event> = events.iter().filter(|e| e.dev == d).cloned().collect();
                log.append_events(&mine).unwrap();
            }
            let a = State::replay(&load_dir(dir.path()).unwrap().events);
            // Same events, written in reverse device order to a different dir.
            let dir2 = tempfile::tempdir().unwrap();
            for d in devs.iter().rev() {
                let log = EventLog::open(dir2.path(), *d).unwrap();
                let mine: Vec<Event> = events.iter().filter(|e| e.dev == *d).cloned().collect();
                log.append_events(&mine).unwrap();
            }
            let b = State::replay(&load_dir(dir2.path()).unwrap().events);
            prop_assert_eq!(a, b);
        }

        /// Replaying a log twice over (duplicated sync) changes nothing.
        #[test]
        fn replay_is_idempotent_under_duplication(bodies in prop::collection::vec(arb_body(), 0..30)) {
            let events: Vec<Event> = bodies.into_iter().enumerate().map(|(i, b)| ev(i64::try_from(i).unwrap(), "desk", b)).collect();
            let once = State::replay(&events);
            let mut doubled: Vec<Event> = events.iter().flat_map(|e| [e.clone(), e.clone()]).collect();
            doubled.sort_by_key(|e| e.ts);
            prop_assert_eq!(once, State::replay(&doubled));
        }
    }
}
