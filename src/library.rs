//! The in-memory library: files from the index joined with replayed state.
//!
//! This is what the TUI and CLI render. It is cheap to rebuild (a few tens of
//! milliseconds for ~15k files), so it is simply rebuilt after every change.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::app::Index;
use crate::config::{Config, RootKind};
use crate::events::{Event, EventBody, Link, State};
use crate::index::classify::{Classified, classify};
use crate::index::db::{FileRow, SeriesMeta};
use crate::model::{EpNo, ItemKey, ItemKind, SeriesStatus, WatchState};

/// A concrete file providing an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// Absolute path.
    pub path: PathBuf,
    /// Root name.
    pub root: String,
    /// Path relative to the root.
    pub rel: PathBuf,
    /// Size in bytes.
    pub size: u64,
    /// Still on disk.
    pub present: bool,
    /// When it was first indexed.
    pub first_seen: i64,
    /// Modification time (unix seconds), i.e. when it arrived on disk.
    pub mtime: i64,
    /// Release version.
    pub version: Option<u8>,
}

impl FileRef {
    /// File name component.
    pub fn name(&self) -> String {
        file_name_lossy(&self.path)
    }

    /// When it arrived: its modification time (which survives index rebuilds),
    /// else when anipv first indexed it.
    pub fn added(&self) -> i64 {
        if self.mtime > 0 { self.mtime } else { self.first_seen }
    }
}

/// Last path component as a `String` (empty when there is none).
pub fn file_name_lossy(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One watchable item of a series.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// Identity within the series.
    pub key: ItemKey,
    /// Files providing it (several copies/versions possible).
    pub files: Vec<FileRef>,
    /// Watch state.
    pub state: WatchState,
}

impl Item {
    /// The file to play: present, highest version, most recently added.
    pub fn best_file(&self) -> Option<&FileRef> {
        self.files.iter().filter(|f| f.present).max_by_key(|f| (f.version.unwrap_or(1), f.added(), f.size))
    }

    /// True if any file is on disk.
    pub fn present(&self) -> bool {
        self.files.iter().any(|f| f.present)
    }

    /// A file name for this item (the one to play, else any known), recorded in
    /// events as evidence of what was watched.
    pub fn file_name(&self) -> Option<String> {
        self.best_file().or_else(|| self.files.first()).map(FileRef::name)
    }

    /// One of the numbered episodes an episode total counts: a whole number
    /// above zero. An episode `00` prologue or a `12.5` recap is not.
    pub fn counts_toward_total(&self) -> bool {
        self.key.kind == ItemKind::Episode && self.key.ep.is_some_and(|e| e.is_whole() && e.whole() > 0)
    }
}

/// A series with its items and user state.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    /// Canonical key.
    pub key: String,
    /// Display title.
    pub title: String,
    /// User status.
    pub status: SeriesStatus,
    /// Note attached to the status.
    pub note: Option<String>,
    /// Items sorted by key (episodes first, by number), except that episodes
    /// without a number come after the numbered ones.
    pub items: Vec<Item>,
    /// Cached metadata for the linked anime.
    pub meta: Option<SeriesMeta>,
    /// Matching already ran against the current offline database and found
    /// nothing to link.
    pub match_checked: bool,
    /// `AniList` link chosen by the user (from events).
    pub link: Link,
    /// anipv completed this series automatically before.
    pub auto_completed: bool,
    /// `AniList` ids of this anime's prequels (`None` until fetched).
    pub prequels: Option<Vec<u64>>,
    /// Manual episode total (from events).
    pub episodes_override: Option<u32>,
    /// Other keys merged into this one.
    pub aliases: Vec<String>,
    /// What fuzzy search matches against (see [`Series::search_text`]).
    search: String,
}

impl Series {
    /// Regular episodes.
    pub fn episodes(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.key.kind == ItemKind::Episode)
    }

    /// Highest watched episode number.
    pub fn max_watched(&self) -> Option<EpNo> {
        self.episodes().filter(|i| i.state.is_watched()).filter_map(|i| i.key.ep).max()
    }

    /// Unwatched episodes on disk after the furthest watched one, then
    /// unwatched episodes without a number (offered once the numbered run is
    /// done).
    pub fn new_episodes(&self) -> Vec<&Item> {
        self.new_iter().collect()
    }

    fn new_iter(&self) -> impl Iterator<Item = &Item> {
        let after = self.max_watched();
        self.episodes().filter(|i| i.present() && !i.state.is_watched()).filter(move |i| match (after, i.key.ep) {
            (Some(a), Some(e)) => e > a,
            (_, None) | (None, _) => true,
        })
    }

    /// The next episode to watch, if one is on disk.
    pub fn next_up(&self) -> Option<&Item> {
        self.new_iter().next()
    }

    /// Number of watched episodes.
    pub fn watched_count(&self) -> usize {
        self.episodes().filter(|i| i.state.is_watched()).count()
    }

    /// Watchable items on disk (episodes, specials, movies; not extras).
    pub fn on_disk_items(&self) -> usize {
        self.items.iter().filter(|i| i.key.kind != ItemKind::Extra && i.present()).count()
    }

    /// Highest episode number on disk.
    pub fn latest_on_disk(&self) -> Option<EpNo> {
        self.episodes().filter(|i| i.present()).filter_map(|i| i.key.ep).max()
    }

    /// `AniList` id: the manual link, else the automatic match.
    pub fn anilist_id(&self) -> Option<u64> {
        self.link.manual().or_else(|| self.meta.as_ref().and_then(|m| m.anilist))
    }

    /// Total episodes: manual override, then metadata. For a long-running
    /// show the offline database's count lags behind, so a known count is
    /// raised to the episodes aired so far (everything before the next one).
    pub fn total(&self) -> Option<u32> {
        self.episodes_override.or_else(|| {
            let m = self.meta.as_ref()?;
            let aired = m.next_ep.map(|next| next.saturating_sub(1));
            m.episodes.map(|count| aired.map_or(count, |aired| count.max(aired)))
        })
    }

    /// You have a relationship with this series: following, paused or
    /// completed it, or watched at least one episode.
    pub fn engaged(&self) -> bool {
        matches!(self.status, SeriesStatus::Following | SeriesStatus::Paused | SeriesStatus::Completed)
            || self.watched_count() > 0
    }

    /// Items `ordered_items(extras, false)` leaves out that `missing` would show.
    pub fn missing_count(&self, extras: bool) -> usize {
        self.ordered_count(extras, true) - self.ordered_count(extras, false)
    }

    /// Anything besides extras on disk; otherwise hiding what is not on disk
    /// would leave an empty list.
    fn has_watchable_on_disk(&self) -> bool {
        self.items.iter().any(|i| i.present() && i.key.kind != ItemKind::Extra)
    }

    /// Episodes on disk as a compact range: `01–03`, `05`, or empty.
    pub fn disk_range(&self) -> String {
        let eps: Vec<EpNo> = self.episodes().filter(|i| i.present()).filter_map(|i| i.key.ep).collect();
        match (eps.iter().min(), eps.iter().max()) {
            (Some(a), Some(b)) if a == b => format!("{a:0>2}"),
            (Some(a), Some(b)) => format!("{a:0>2}–{b:0>2}"),
            _ => String::new(),
        }
    }

    /// The show has finished airing and every episode has been watched.
    pub fn is_done(&self) -> bool {
        let finished = self.meta.as_ref().is_some_and(SeriesMeta::is_finished);
        finished && self.total().is_some_and(|t| t > 0 && self.whole_watched_count() >= t as usize)
    }

    /// Number of watched episodes that count toward the total (see
    /// [`Item::counts_toward_total`]): what [`Self::progress`] shows and
    /// [`Self::is_done`] compares with the total.
    pub fn whole_watched_count(&self) -> usize {
        self.items.iter().filter(|i| i.counts_toward_total() && i.state.is_watched()).count()
    }

    /// Followed, done, and not auto-completed before (so a user who set it back
    /// to following isn't overridden).
    pub fn should_auto_complete(&self) -> bool {
        self.status == SeriesStatus::Following && !self.auto_completed && self.is_done()
    }

    /// `watched/total`, with `?` for an unknown total. Counts only episodes
    /// that count toward the total, so a finished 12-episode show reads `12/12`
    /// even with an episode `00` and a `12.5` recap watched.
    pub fn progress(&self) -> String {
        format!("{watched}/{total}", watched = self.whole_watched_count(), total = self.total_text())
    }

    /// The episode total, or `?`.
    pub fn total_text(&self) -> String {
        self.total().map_or_else(|| "?".to_string(), |t| t.to_string())
    }

    /// The airing summary alone (see [`Series::airing`]), empty without metadata.
    pub fn airing_text(&self, now: i64) -> String {
        self.airing(now).map(|a| a.0).unwrap_or_default()
    }

    /// What fuzzy search matches against: title and merged-in names, plus the
    /// key when a rename made it differ from the title. (Repeating the title
    /// would let a query match across the two copies.) Computed once when the
    /// library is built.
    pub fn search_text(&self) -> &str {
        &self.search
    }

    /// Airing summary from metadata (`ep 9 in 1d 7h`, `finished`, …) and
    /// whether it is "live" (still airing) for highlighting.
    pub fn airing(&self, now: i64) -> Option<(String, bool)> {
        let m = self.meta.as_ref()?;
        if let (Some(e), Some(t)) = (m.next_ep, m.next_airing) {
            return Some((format!("ep {e} {}", crate::fmt::until(t, now)), true));
        }
        Some(match m.status.as_deref()? {
            "FINISHED" => ("finished".into(), false),
            "RELEASING" => ("airing".into(), true),
            "NOT_YET_RELEASED" => ("upcoming".into(), false),
            other => (other.to_lowercase(), false),
        })
    }

    /// Items in display order: episodes, each followed by its attached extras;
    /// then specials, movies and unattached extras. Extras only when `extras`;
    /// items not on disk (known from history) only when `missing`, or when
    /// nothing is on disk at all.
    pub fn ordered_items(&self, extras: bool, missing: bool) -> Vec<&Item> {
        self.ordered_indices(extras, missing).into_iter().map(|i| &self.items[i]).collect()
    }

    /// [`Series::ordered_items`] as indices into [`Series::items`].
    pub fn ordered_indices(&self, extras: bool, missing: bool) -> Vec<usize> {
        let missing = missing || !self.has_watchable_on_disk();
        let of = move |k: ItemKind| {
            self.items.iter().enumerate().filter(move |(_, i)| i.key.kind == k && listed(i, extras, missing))
        };
        let mut out: Vec<usize> = Vec::with_capacity(self.items.len());
        // Extras by the episode they belong to; the first episode listed with
        // that number takes them.
        let mut attached: HashMap<EpNo, Vec<usize>> = HashMap::new();
        for (n, x) in of(ItemKind::Extra) {
            if let Some(ep) = x.key.ep {
                attached.entry(ep).or_default().push(n);
            }
        }
        for (n, e) in of(ItemKind::Episode) {
            out.push(n);
            out.extend(e.key.ep.and_then(|ep| attached.remove(&ep)).into_iter().flatten());
        }
        out.extend(of(ItemKind::Special).map(|(n, _)| n));
        out.extend(of(ItemKind::Movie).map(|(n, _)| n));
        let unattached = |x: &Item| x.key.ep.is_none_or(|ep| attached.contains_key(&ep));
        out.extend(of(ItemKind::Extra).filter(|(_, x)| unattached(x)).map(|(n, _)| n));
        out
    }

    /// `ordered_items(extras, missing).len()` without building the list.
    pub fn ordered_count(&self, extras: bool, missing: bool) -> usize {
        let missing = missing || !self.has_watchable_on_disk();
        self.items.iter().filter(|i| listed(i, extras, missing)).count()
    }

    /// Up to `n` new episodes with the file to play for each, skipping `skip`.
    ///
    /// A multi-episode file (`- 001&002`) is listed once, under its first episode.
    pub fn queue_candidates(&self, n: usize, skip: impl Fn(&ItemKey) -> bool) -> Vec<(ItemKey, PathBuf)> {
        let mut seen = std::collections::HashSet::new();
        self.new_iter()
            .filter(|i| !skip(&i.key))
            .filter_map(|i| i.best_file().map(|f| (i.key.clone(), f.path.clone())))
            .filter(|(_, path)| seen.insert(path.clone()))
            .take(n)
            .collect()
    }

    /// Unwatched numbered episodes up to and including `upto` ("watched up to
    /// here"). Unnumbered episodes are left alone: one may well be the finale.
    pub fn unwatched_through(&self, upto: EpNo) -> Vec<ItemKey> {
        self.episodes()
            .filter(|i| i.key.ep.is_some_and(|e| e <= upto) && !i.state.is_watched())
            .map(|i| i.key.clone())
            .collect()
    }

    /// Last time anything in the series was played.
    pub fn last_activity(&self) -> Option<i64> {
        self.items.iter().filter_map(|i| i.state.at()).max()
    }

    /// Newest file added.
    pub fn last_added(&self) -> Option<i64> {
        self.items.iter().flat_map(|i| &i.files).filter(|f| f.present).map(FileRef::added).max()
    }

    /// True if any file of the series is on disk.
    pub fn present(&self) -> bool {
        self.items.iter().any(Item::present)
    }

    /// Find an item by key.
    pub fn item(&self, key: &ItemKey) -> Option<&Item> {
        self.items.iter().find(|i| &i.key == key)
    }
}

/// Whether the item list of a series shows `item` (`missing` already
/// accounts for a series with nothing on disk).
fn listed(item: &Item, extras: bool, missing: bool) -> bool {
    (extras || item.key.kind != ItemKind::Extra) && (missing || item.present())
}

/// Why a series is in the inbox (indices into [`Library::series`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    /// A new season of a series you follow, paused, completed or watched.
    NewSeasonOf(usize),
    /// A special or side entry of another series.
    PartOf(usize),
}

impl Hint {
    /// `sequel to Show · following` / `part of Show`.
    pub fn describe(self, lib: &Library) -> String {
        match self {
            Self::NewSeasonOf(j) => {
                let prev = &lib.series[j];
                format!("sequel to {title} · {status}", title = prev.title, status = prev.status)
            }
            Self::PartOf(j) => format!("part of {title}", title = lib.series[j].title),
        }
    }
}

/// One row of [`Library::inbox`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboxEntry {
    /// Index into [`Library::series`].
    pub index: usize,
    /// Relation to another series, if any.
    pub hint: Option<Hint>,
}

/// A file with its classification, for the raw files view.
#[derive(Debug, Clone)]
pub struct IndexedFile {
    /// The file.
    pub file: FileRef,
    /// Canonical series key it belongs to.
    pub series: String,
    /// Items it provides.
    pub items: Vec<ItemKey>,
}

/// Everything the UI needs.
#[derive(Debug, Clone, Default)]
pub struct Library {
    /// All series, sorted by title.
    pub series: Vec<Series>,
    /// All files (present and gone) with classification, sorted by path.
    pub files: Vec<IndexedFile>,
    /// Every key that finds a series (its own, merged-away and legacy keys) → its index.
    by_key: HashMap<String, usize>,
    /// Every merged-away or legacy key → the canonical key it resolves to.
    canonical: HashMap<String, String>,
    aliases: HashMap<String, String>,
    /// Legacy series key → the key it is now (see [`legacy_keys`]).
    legacy: HashMap<String, String>,
    /// The reverse of `legacy`: current key → its legacy spellings, sorted.
    legacy_of: HashMap<String, Vec<String>>,
    /// Mapped legacy keys the event log names (see [`Library::unmerge_events`]).
    legacy_named: HashSet<String>,
    /// Mapped legacy keys the event log merges or unmerges (see [`Library::legacy_aliases`]).
    legacy_decided: HashSet<String>,
    /// Problems met while building that did not stop it (e.g. the cache
    /// migration in [`crate::app::Ctx::migrated_library`] failing), for the UI to show.
    pub warnings: Vec<String>,
}

/// Group the classified files by the series they belong to now (after
/// merges), with the series and items known only from history.
fn group(
    state: &State,
    classified: Vec<(&FileRow, RootKind, &Classified)>,
) -> (HashMap<String, Building>, Vec<IndexedFile>) {
    let mut building: HashMap<String, Building> = HashMap::new();
    let mut indexed = Vec::with_capacity(classified.len());
    for (row, kind, Classified { series, title, parsed, .. }) in classified {
        let canon = state.resolve(series).to_string();
        let items = crate::index::classify::expand_items(parsed);
        let file = FileRef {
            path: row.path.clone(),
            root: row.root.clone(),
            rel: row.rel.clone(),
            size: row.size,
            present: row.present,
            first_seen: row.first_seen,
            mtime: row.mtime,
            version: parsed.version,
        };
        let b = building.entry(canon.clone()).or_default();
        // Archive folder names are the most reliable titles.
        let weight = if kind == RootKind::Archive && row.rel.components().count() > 1 { 1000 } else { 1 };
        let votes = if row.present { &mut b.titles } else { &mut b.gone_titles };
        // Most files vote for a title already counted: no copy for those.
        if let Some(n) = votes.get_mut(title) {
            *n += weight;
        } else {
            votes.insert(title.clone(), weight);
        }
        for it in &items {
            b.items.entry(it.clone()).or_default().push(file.clone());
        }
        indexed.push(IndexedFile { file, series: canon, items });
    }

    // Items/series known only from history (files deleted or never indexed here).
    for (series, items) in &state.items {
        let b = building.entry(series.clone()).or_default();
        for item in items.keys() {
            b.items.entry(item.clone()).or_default();
        }
    }
    for (series, s) in &state.series {
        if s.status != SeriesStatus::Untracked || s.title.is_some() {
            building.entry(series.clone()).or_default();
        }
    }
    (building, indexed)
}

/// The mapped legacy keys `events` name, and those they merge or unmerge.
fn legacy_mentions(events: &[Event], legacy: &HashMap<String, String>) -> (HashSet<String>, HashSet<String>) {
    let (mut named, mut decided) = (HashSet::new(), HashSet::new());
    if legacy.is_empty() {
        return (named, decided);
    }
    for ev in events {
        if let EventBody::Alias { from, .. } | EventBody::Unalias { from } = &ev.body
            && legacy.contains_key(from)
        {
            decided.insert(from.clone());
        }
        named.extend(ev.body.keys().filter(|k| legacy.contains_key(*k)).map(str::to_string));
    }
    (named, decided)
}

/// What [`Lookups::finish_series`] looks a series up in.
struct Lookups<'a> {
    cfg: &'a Config,
    state: &'a State,
    index: &'a Index,
    legacy_of: &'a HashMap<String, Vec<String>>,
    /// Cached metadata rows by series key.
    meta_by: HashMap<&'a str, &'a SeriesMeta>,
    /// Series keys with a "no match" result.
    attempted: HashSet<&'a str>,
    /// Canonical key → the keys merged into it.
    aliases_of: HashMap<&'a str, Vec<String>>,
}

impl<'a> Lookups<'a> {
    fn new(cfg: &'a Config, state: &'a State, index: &'a Index, legacy_of: &'a HashMap<String, Vec<String>>) -> Self {
        let meta_by = index.meta.rows.iter().map(|m| (m.series.as_str(), m)).collect();
        let attempted = index.meta.attempts.iter().map(|a| a.series.as_str()).collect();
        let mut aliases_of: HashMap<&str, Vec<String>> = HashMap::new();
        for from in state.aliases.keys() {
            // On an alias cycle the canonical key is itself a merged-away key:
            // it is the series, not one of its other names.
            let to = state.resolve(from);
            if to != from {
                aliases_of.entry(to).or_default().push(from.clone());
            }
        }
        Self { cfg, state, index, legacy_of, meta_by, attempted, aliases_of }
    }

    /// The series `key`, from its files and items (`b`), history and caches.
    fn finish_series(&self, key: String, b: Building) -> Series {
        let state = self.state;
        let st = state.series.get(&key).cloned().unwrap_or_default();
        let title = st.title.clone().unwrap_or_else(|| {
            let votes = if b.titles.is_empty() { &b.gone_titles } else { &b.titles };
            votes
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(t, _)| t.clone())
                .or_else(|| {
                    let f = state.file_hints.get(&key)?;
                    Some(classify(self.cfg, RootKind::Ongoing, Path::new(f)).title).filter(|t| !t.is_empty())
                })
                .unwrap_or_else(|| title_case(&key))
        });
        let states = state.items.get(&key);
        let mut items: Vec<Item> = b
            .items
            .into_iter()
            .map(|(k, files)| {
                let state = states.and_then(|m| m.get(&k)).copied().unwrap_or_default();
                Item { key: k, files, state }
            })
            .collect();
        // Unnumbered episodes after the numbered ones (stable: by label).
        items.sort_by_key(|i| (i.key.kind, i.key.kind == ItemKind::Episode && i.key.ep.is_none()));
        let mut aliases = self.aliases_of.get(key.as_str()).cloned().unwrap_or_default();
        aliases.sort();
        // Caches written under a legacy key still belong to the series.
        // An unlinked series ignores a link cached before the unlink
        // arrived, and a manual link ignores data cached for another
        // anime (until the next sync replaces it).
        // The first usable row wins: a row for another anime under
        // one key doesn't hide a matching row under the other.
        let meta = spellings(self.legacy_of, &key)
            .find_map(|k| {
                self.meta_by.get(k).filter(|m| match st.link {
                    Link::Off => false,
                    Link::Manual(id) => m.is_for(id),
                    Link::Auto => true,
                })
            })
            .map(|m| SeriesMeta { series: key.clone(), ..(*m).clone() });
        // A "no match" for the old key doesn't count: the new key is a
        // different title to match (`Ctx::migrated_library` drops those rows).
        let match_checked = self.attempted.contains(key.as_str());
        let mut s = Series {
            meta,
            match_checked,
            key,
            title,
            status: st.status,
            note: st.note,
            items,
            link: st.link,
            auto_completed: st.auto_completed,
            prequels: None,
            episodes_override: st.episodes,
            aliases,
            search: String::new(),
        };
        s.prequels = s.anilist_id().and_then(|id| self.index.meta.prequels.get(&id).cloned());
        s.search = search_text(&s.title, &s.key, &s.aliases);
        s
    }
}

#[derive(Default)]
struct Building {
    /// Title votes from files on disk, and from gone files (used only when
    /// nothing is on disk, so a deleted file's old name can't win a tie).
    titles: HashMap<String, usize>,
    gone_titles: HashMap<String, usize>,
    items: BTreeMap<ItemKey, Vec<FileRef>>,
}

impl Library {
    /// Build from the event log and the cached index (files, metadata, match
    /// attempts, prequels).
    ///
    /// The events are replayed here, after the files are classified, because a
    /// series recorded under a key from before kana voiced marks were kept
    /// (`バカ` was `ハカ`) is re-keyed to the key of the series on disk it
    /// belongs to (see `legacy_keys`).
    pub fn build(cfg: &Config, events: &[Event], index: &Index) -> Self {
        let classified: Vec<(&FileRow, RootKind, &Classified)> = index.classified().collect();
        let legacy = legacy_keys(classified.iter().map(|(_, _, c)| (c.series.as_str(), c.legacy_series.as_deref())));
        let state = &State::replay_with(events, &legacy);
        let mut legacy_of: HashMap<String, Vec<String>> = HashMap::new();
        for (old, new) in &legacy {
            legacy_of.entry(new.clone()).or_default().push(old.clone());
        }
        for olds in legacy_of.values_mut() {
            olds.sort_unstable();
        }

        let (building, mut indexed) = group(state, classified);
        let lookups = Lookups::new(cfg, state, index, &legacy_of);
        let mut series: Vec<Series> = building.into_iter().map(|(key, b)| lookups.finish_series(key, b)).collect();
        series.sort_by_cached_key(|s| (s.title.to_lowercase(), s.key.clone()));
        indexed.sort_by(|a, b| a.file.path.cmp(&b.file.path));

        let (legacy_named, legacy_decided) = legacy_mentions(events, &legacy);
        // Resolved once here, so lookups by any key are a single lookup.
        let canonical: HashMap<String, String> = legacy
            .iter()
            .map(|(old, new)| (old, state.resolve(new)))
            .chain(state.aliases.keys().map(|from| (from, state.resolve(from))))
            .filter(|(k, to)| k.as_str() != *to)
            .map(|(k, to)| (k.clone(), to.to_string()))
            .collect();
        let mut by_key: HashMap<String, usize> = series.iter().enumerate().map(|(i, s)| (s.key.clone(), i)).collect();
        for (k, to) in &canonical {
            if let Some(&i) = by_key.get(to) {
                by_key.entry(k.clone()).or_insert(i);
            }
        }
        Self {
            series,
            files: indexed,
            by_key,
            canonical,
            aliases: state.aliases.clone(),
            legacy_named,
            legacy_decided,
            legacy,
            legacy_of,
            warnings: Vec::new(),
        }
    }

    /// `Title 07,08` for messages about played items.
    pub fn label(&self, series: &str, items: &[ItemKey]) -> String {
        let title = self.get(series).map_or(series, |s| s.title.as_str());
        let eps: Vec<String> = items.iter().map(ItemKey::describe).collect();
        format!("{title} {}", eps.join(","))
    }

    /// Canonical key for a (possibly merged-away) series key.
    pub fn resolve<'a>(&'a self, key: &'a str) -> &'a str {
        self.canonical.get(key).map_or(key, String::as_str)
    }

    /// The current key for `key`: itself, unless it is a mapped legacy key
    /// (see [`Library::legacy_keys`]). Aliases are not followed.
    pub fn current_key<'a>(&'a self, key: &'a str) -> &'a str {
        crate::events::rekey(&self.legacy, key)
    }

    /// Every spelling of the current key `key` that history or caches may use:
    /// the key itself, then its mapped legacy keys.
    pub fn spellings<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> {
        spellings(&self.legacy_of, key)
    }

    /// Legacy series keys (from before kana voiced marks were kept) that this
    /// library reads as current keys: legacy → current (see `docs/events.md`).
    pub fn legacy_keys(&self) -> &HashMap<String, String> {
        &self.legacy
    }

    /// True if `key` (as recorded, after legacy mapping) is merged into another series.
    pub fn is_merged_away(&self, key: &str) -> bool {
        self.aliases.contains_key(self.current_key(key))
    }

    /// Alias events that make the legacy key mapping permanent: `legacy → key`
    /// for each mapped legacy key that the events it was built from name,
    /// unless they already have an alias or unalias *from* that legacy key
    /// (written by this, or the user's own decision about it, which is left alone).
    ///
    /// Once written, older versions and libraries where the mapping no longer
    /// applies (the series gone from disk, or a real series with the old key
    /// appearing) still read the history as the series'. Pure: the caller
    /// appends them ([`crate::app::Ctx::migrated_library`] does).
    pub fn legacy_aliases(&self) -> Vec<EventBody> {
        let mut pairs: Vec<(&String, &String)> = self
            .legacy
            .iter()
            .filter(|(old, _)| self.legacy_named.contains(*old) && !self.legacy_decided.contains(*old))
            .collect();
        pairs.sort_unstable();
        pairs.into_iter().map(|(old, new)| EventBody::Alias { from: old.clone(), to: new.clone() }).collect()
    }

    /// Events that split the merged-away key `from` off its series again.
    ///
    /// Besides `from` itself, every legacy spelling of it that the log names
    /// is unmerged too, so readers that take the old key as recorded agree,
    /// and is then pointed back at `from` (see [`Library::legacy_aliases`]),
    /// so its history stays with it. Which spellings the log names was noted
    /// when the library was built, so the log is not read again.
    pub fn unmerge_events(&self, from: &str) -> Vec<EventBody> {
        let from = self.current_key(from);
        let mut out = vec![EventBody::Unalias { from: from.to_string() }];
        for old in self.spellings(from).skip(1) {
            if self.legacy_named.contains(old) {
                out.push(EventBody::Unalias { from: old.to_string() });
                out.push(EventBody::Alias { from: old.to_string(), to: from.to_string() });
            }
        }
        out
    }

    /// Series and items for any path: indexed files directly, other files by
    /// classifying them (relative to a root when inside one).
    pub fn identify(&self, cfg: &Config, path: &Path) -> (String, Vec<ItemKey>) {
        if let Some(f) = self.file(path) {
            return (f.series.clone(), f.items.clone());
        }
        let c = cfg
            .roots
            .iter()
            .find_map(|r| path.strip_prefix(r.resolved()).ok().map(|rel| classify(cfg, r.kind, rel)))
            .unwrap_or_else(|| {
                classify(cfg, RootKind::Ongoing, Path::new(path.file_name().unwrap_or(path.as_os_str())))
            });
        (self.resolve(&c.series).to_string(), c.items())
    }

    /// Untracked series with files in an ongoing root (e.g. Downloads): shows
    /// waiting to be followed or skipped. New seasons of shows you watch come
    /// first, then everything else newest first, then side entries of other
    /// series (specials, spin-offs) at the end.
    pub fn inbox(&self, cfg: &Config) -> Vec<InboxEntry> {
        let ongoing = cfg.ongoing_roots();
        let by_anilist: HashMap<u64, usize> =
            self.series.iter().enumerate().filter_map(|(i, s)| s.anilist_id().map(|id| (id, i))).collect();
        let bases: Vec<&str> = self.series.iter().map(|s| crate::identity::base_key(&s.key)).collect();
        let mut entries: Vec<InboxEntry> = self
            .series
            .iter()
            .enumerate()
            .filter(|(_, s)| Self::in_inbox(s, &ongoing))
            .map(|(i, s)| InboxEntry { index: i, hint: self.inbox_hint(i, s, &by_anilist, &bases) })
            .collect();
        entries.sort_by_cached_key(|e| {
            let rank = match e.hint {
                Some(Hint::NewSeasonOf(_)) => 0,
                None => 1,
                Some(Hint::PartOf(_)) => 2,
            };
            let s = &self.series[e.index];
            (rank, std::cmp::Reverse(s.last_added()), s.title.to_lowercase())
        });
        entries
    }

    /// True if `s` belongs in the [`Library::inbox`]: untracked, with a file
    /// present in one of the `ongoing` roots ([`Config::ongoing_roots`]).
    pub fn in_inbox(s: &Series, ongoing: &HashSet<&str>) -> bool {
        s.status == SeriesStatus::Untracked
            && s.items.iter().flat_map(|i| &i.files).any(|f| f.present && ongoing.contains(f.root.as_str()))
    }

    /// `bases[j]` is the base key of `self.series[j]` (computed once per inbox).
    fn inbox_hint(&self, i: usize, s: &Series, by_anilist: &HashMap<u64, usize>, bases: &[&str]) -> Option<Hint> {
        // A prequel you're watching (AniList), or the same base title with a
        // season marker (`kusuriya no hitorigoto s3` after `kusuriya no hitorigoto`).
        let prequel = s.prequels.iter().flatten().filter_map(|id| by_anilist.get(id)).copied();
        let base = bases[i];
        // Only a key with a season marker (base differs) looks for its other seasons.
        let others = if base == s.key { 0..0 } else { 0..self.series.len() };
        let same_title = others.filter(|&j| j != i && bases[j] == base);
        if let Some(j) = prequel.chain(same_title).find(|&j| j != i && self.series[j].engaged()) {
            return Some(Hint::NewSeasonOf(j));
        }
        // `pocket monsters 2023 pokemon go` belongs to `pocket monsters 2023`.
        self.series
            .iter()
            .enumerate()
            .filter(|&(j, o)| j != i && s.key.len() > o.key.len() && s.key.starts_with(&o.key))
            .filter(|(_, o)| s.key.as_bytes()[o.key.len()] == b' ')
            .max_by_key(|(_, o)| o.key.len())
            .map(|(j, _)| Hint::PartOf(j))
    }

    /// Position of a series in [`Library::series`].
    pub fn index_of(&self, key: &str) -> Option<usize> {
        self.by_key.get(key).copied()
    }

    /// Look up a series by key. A key merged into another series finds that
    /// series, so keys held across a merge (queue, Detail, requests) stay valid.
    pub fn get(&self, key: &str) -> Option<&Series> {
        self.index_of(key).map(|i| &self.series[i])
    }

    /// Classification of an indexed path.
    pub fn file(&self, path: &Path) -> Option<&IndexedFile> {
        let i = self.files.binary_search_by(|f| f.file.path.as_path().cmp(path)).ok()?;
        Some(&self.files[i])
    }

    /// Followed (and optionally paused) series with something new on disk,
    /// most recently active first.
    pub fn up_next(&self, include_paused: bool) -> Vec<&Series> {
        let mut v: Vec<&Series> = self
            .series
            .iter()
            .filter(|s| s.status == SeriesStatus::Following || (include_paused && s.status == SeriesStatus::Paused))
            .collect();
        v.sort_by_cached_key(|s| {
            (
                s.next_up().is_none(),
                s.status != SeriesStatus::Following,
                std::cmp::Reverse(s.last_activity().max(s.last_added())),
            )
        });
        v
    }
}

/// `key`, then its legacy spellings in `legacy_of` (current → legacy keys).
fn spellings<'a>(legacy_of: &'a HashMap<String, Vec<String>>, key: &'a str) -> impl Iterator<Item = &'a str> {
    std::iter::once(key).chain(legacy_of.get(key).into_iter().flatten().map(String::as_str))
}

/// Which old series keys stand for which current ones.
///
/// `series` yields `(key, legacy key if different)` for every classified file. Where a file's
/// legacy key differs from its key, history recorded under the legacy key
/// belongs to the key, unless the legacy key is itself the key of another
/// series on disk (then the exact-key owner keeps it) or several different keys
/// share one legacy key (nothing says which one it was). Returns legacy → key.
fn legacy_keys<'a>(series: impl Iterator<Item = (&'a str, Option<&'a str>)> + Clone) -> HashMap<String, String> {
    let on_disk: HashSet<&str> = series.clone().map(|(key, _)| key).collect();
    let mut map: HashMap<&str, Option<&str>> = HashMap::new();
    let differing = series.filter_map(|(key, legacy)| Some((key, legacy?)));
    for (key, legacy) in differing.filter(|(_, legacy)| !on_disk.contains(legacy)) {
        let slot = map.entry(legacy).or_insert(Some(key));
        if *slot != Some(key) {
            *slot = None;
        }
    }
    map.into_iter().filter_map(|(legacy, key)| Some((legacy.to_string(), key?.to_string()))).collect()
}

/// Title and merged-in names, plus the key when it differs from the title's own.
fn search_text(title: &str, key: &str, aliases: &[String]) -> String {
    let mut text = title.to_string();
    if crate::identity::series_key(title) != key {
        text.push(' ');
        text.push_str(key);
    }
    for alias in aliases {
        text.push(' ');
        text.push_str(alias);
    }
    text
}

/// `one piece` → `One Piece`, used when no better title is known.
pub fn title_case(key: &str) -> String {
    key.split(' ')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::MetaCache;
    use crate::config::Root;
    use crate::events::{Event, EventBody};
    use crate::index::db::{FileRow, MatchAttempt};

    fn cfg() -> Config {
        Config {
            roots: vec![Root::test("dl", "/dl", RootKind::Ongoing), Root::test("anime", "/anime", RootKind::Archive)],
            ..Config::default()
        }
    }

    fn row(root: &str, rel: &str, present: bool, seen: i64) -> FileRow {
        FileRow { present, first_seen: seen, ..FileRow::test(root, rel) }
    }

    fn index(files: &[FileRow], meta: MetaCache) -> Index {
        Index::new(&cfg(), files.to_vec(), meta)
    }

    fn rows(meta: &[SeriesMeta]) -> MetaCache {
        MetaCache { rows: meta.to_vec(), ..MetaCache::default() }
    }

    fn build(files: &[FileRow], events: &[Event], meta: &[SeriesMeta]) -> Library {
        Library::build(&cfg(), events, &index(files, rows(meta)))
    }

    fn ev(ts: i64, body: EventBody) -> Event {
        Event::new(ts, "t", body)
    }

    fn ep(n: u32) -> ItemKey {
        ItemKey::episode(EpNo::new(n))
    }

    #[test]
    fn up_next_flow() {
        let files = vec![
            row("anime", "One Piece/[GroupA] One Piece - 1179 (1080p) [A].mkv", true, 1),
            row("anime", "One Piece/[GroupA] One Piece - 1180 (1080p) [B].mkv", true, 2),
            row("dl", "[GroupA] One Piece - 1181 (1080p) [C].mkv", true, 3),
            row("dl", "[GroupA] One Piece - 1182 (1080p) [D].mkv", true, 4),
            row("dl", "[GroupA] Grand Blue S3 - 01 (1080p) [E].mkv", true, 5),
            row("dl", "[GroupA] Grand Blue S3 - 01v2 (1080p) [F].mkv", true, 6),
        ];
        let events = vec![
            ev(10, EventBody::status("one piece", SeriesStatus::Following, None)),
            ev(11, EventBody::Watched { series: "one piece".into(), item: ep(1180), file: None }),
        ];
        let lib = build(&files, &events, &[]);
        let op = lib.get("one piece").unwrap();
        assert_eq!(op.title, "One Piece");
        assert_eq!(op.next_up().unwrap().key, ep(1181));
        assert_eq!(op.new_episodes().len(), 2);
        assert_eq!(op.on_disk_items(), 4);
        assert_eq!(op.latest_on_disk(), Some(EpNo::new(1182)));

        let gb = lib.get("grand blue s3").unwrap();
        assert_eq!(gb.items.len(), 1, "versions collapse into one item");
        assert_eq!(gb.items[0].best_file().unwrap().version, Some(2));
        assert_eq!(gb.status, SeriesStatus::Untracked);

        let up = lib.up_next(false);
        assert_eq!(up.len(), 1);
        assert_eq!(up[0].key, "one piece");
        assert!(lib.file(Path::new("/dl/[GroupA] One Piece - 1181 (1080p) [C].mkv")).is_some());
    }

    #[test]
    fn history_without_files_and_aliases() {
        let events = vec![
            ev(1, EventBody::Watched { series: "old show".into(), item: ep(3), file: None }),
            ev(
                2,
                EventBody::Watched { series: "even the student council has its holes".into(), item: ep(1), file: None },
            ),
            ev(
                3,
                EventBody::Alias {
                    from: "even the student council has its holes".into(),
                    to: "seitokai ni mo ana wa aru".into(),
                },
            ),
            ev(
                4,
                EventBody::Title { series: "seitokai ni mo ana wa aru".into(), title: "Student Council Holes".into() },
            ),
        ];
        let files = vec![
            row("dl", "Even.the.Student.Council.Has.Its.Holes.S01E01.1080p.WEB-X.mkv", true, 1),
            row("dl", "[GroupB] Seitokai ni mo Ana wa Aru! - 02 [1080p].mkv", true, 1),
            row("dl", "gone - 01.mkv", false, 1),
        ];
        let lib = build(&files, &events, &[]);
        let old = lib.get("old show").unwrap();
        assert_eq!(old.title, "Old Show");
        assert!(!old.present());
        let s = lib.get("seitokai ni mo ana wa aru").unwrap();
        assert_eq!(s.title, "Student Council Holes");
        assert_eq!(s.aliases, vec!["even the student council has its holes"]);
        assert_eq!(s.watched_count(), 1);
        assert_eq!(s.next_up().unwrap().key, ep(2));
        assert_eq!(lib.series.iter().filter(|s| s.key.starts_with("even the")).count(), 0, "no series of its own");
        assert_eq!(lib.get("even the student council has its holes").unwrap().key, s.key, "the old key finds it");
        assert!(!lib.get("gone").unwrap().present());
    }

    #[test]
    fn unconfigured_roots_are_skipped() {
        let files = vec![row("other", "x - 01.mkv", true, 1)];
        let lib = build(&files, &[], &[]);
        assert!(lib.series.is_empty());
    }

    #[test]
    fn multi_episode_files_are_queued_once() {
        let files = vec![
            row("anime", "Urusei Yatsura/[G] Urusei Yatsura - 001&002 [BD].mkv", true, 1),
            row("anime", "Urusei Yatsura/[G] Urusei Yatsura - 003&004 [BD].mkv", true, 1),
        ];
        let lib = build(&files, &[], &[]);
        let s = lib.get("urusei yatsura").unwrap();
        let q = s.queue_candidates(10, |_| false);
        assert_eq!(q.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), vec![ep(1), ep(3)]);
    }

    #[test]
    fn recaps_do_not_count_toward_completion() {
        let mut files: Vec<FileRow> =
            (1..=12).map(|n| row("anime", &format!("Show/Show - {n:02}.mkv"), true, 1)).collect();
        files.push(row("anime", "Show/Show - 11.5.mkv", true, 1));
        let recap: ItemKey = ItemKey::episode("11.5".parse().unwrap());
        let mut events: Vec<Event> = (1..=11u32).map(|n| ev(i64::from(n), EventBody::watched("show", ep(n)))).collect();
        events.push(ev(20, EventBody::watched("show", recap)));
        let meta = SeriesMeta {
            series: "show".into(),
            anilist: Some(1),
            episodes: Some(12),
            status: Some("FINISHED".into()),
            ..SeriesMeta::default()
        };
        let lib = build(&files, &events, std::slice::from_ref(&meta));
        assert!(!lib.get("show").unwrap().is_done(), "episode 12 is still unwatched");
        assert_eq!(lib.get("show").unwrap().progress(), "11/12", "the recap is not counted");
        events.push(ev(21, EventBody::watched("show", ep(12))));
        let lib = build(&files, &events, std::slice::from_ref(&meta));
        assert!(lib.get("show").unwrap().is_done());

        // An episode `00` prologue doesn't stand in for an unwatched episode 12.
        files.push(row("anime", "Show/Show - 00.mkv", true, 1));
        let mut events: Vec<Event> = (0..=11u32).map(|n| ev(i64::from(n), EventBody::watched("show", ep(n)))).collect();
        assert!(!build(&files, &events, std::slice::from_ref(&meta)).get("show").unwrap().is_done());
        events.push(ev(20, EventBody::watched("show", ep(12))));
        events.push(ev(21, EventBody::watched("show", ItemKey::episode("11.5".parse().unwrap()))));
        let lib = build(&files, &events, &[meta]);
        assert!(lib.get("show").unwrap().is_done());
        assert_eq!(lib.get("show").unwrap().progress(), "12/12", "neither 00 nor 11.5 counts");
    }

    /// A deleted file's old name doesn't compete with names on disk, whatever
    /// order the rows come in.
    #[test]
    fn titles_come_from_files_on_disk() {
        let files = [row("dl", "[A] Another Name - 01.mkv", false, 1), row("dl", "[B] Show - 02.mkv", true, 2)];
        let merge = [ev(1, EventBody::Alias { from: "another name".into(), to: "show".into() })];
        let lib = build(&files, &merge, &[]);
        assert_eq!(lib.get("show").unwrap().title, "Show");
        let gone = [row("dl", "[A] Another Name - 01.mkv", false, 1)];
        let lib = build(&gone, &[], &[]);
        assert_eq!(
            lib.get("another name").unwrap().title,
            "Another Name",
            "nothing on disk: the old name is all there is"
        );
    }

    #[test]
    fn items_not_on_disk_are_listed_on_request() {
        let files: Vec<FileRow> = (4..=5).map(|n| row("anime", &format!("Show/Show - {n:02}.mkv"), true, 1)).collect();
        let events: Vec<Event> = (1..=3u32).map(|n| ev(i64::from(n), EventBody::watched("show", ep(n)))).collect();
        let lib = build(&files, &events, &[]);
        let s = lib.get("show").unwrap();
        let eps = |missing| s.ordered_items(false, missing).iter().map(|i| i.key.clone()).collect::<Vec<_>>();
        assert_eq!(eps(false), [ep(4), ep(5)]);
        assert_eq!(eps(true), (1..=5).map(ep).collect::<Vec<_>>());
        assert_eq!(s.missing_count(false), 3);
        for (extras, missing) in [(false, false), (false, true), (true, false), (true, true)] {
            assert_eq!(s.ordered_count(extras, missing), s.ordered_items(extras, missing).len());
        }

        // Nothing on disk any more: hiding it all would leave an empty list.
        let gone: Vec<FileRow> = files.iter().map(|f| FileRow { present: false, ..f.clone() }).collect();
        let lib = build(&gone, &events, &[]);
        let s = lib.get("show").unwrap();
        assert_eq!(s.ordered_items(false, false).len(), 5);
        assert_eq!(s.missing_count(false), 0);
        assert_eq!(s.ordered_count(false, false), 5);
    }

    #[test]
    fn total_counts_episodes_aired_since_the_offline_count() {
        let files = [row("anime", "Long Show/Long Show - 1100.mkv", true, 1)];
        let meta = |episodes, next_ep| SeriesMeta {
            series: "long show".into(),
            anilist: Some(1),
            episodes,
            next_ep,
            ..SeriesMeta::default()
        };
        let total = |m: SeriesMeta| build(&files, &[], &[m]).get("long show")?.total();
        assert_eq!(total(meta(Some(1168), Some(1181))), Some(1180), "stale count, ep 1181 next");
        assert_eq!(total(meta(Some(12), Some(5))), Some(12), "a season's announced length stays");
        assert_eq!(total(meta(None, Some(5))), None, "length unknown: aired so far isn't a total");
        assert_eq!(total(meta(Some(24), None)), Some(24));
    }

    #[test]
    fn a_manual_link_ignores_metadata_cached_for_another_anime() {
        let files = [row("anime", "Show/Show - 01.mkv", true, 1)];
        let cached = SeriesMeta { series: "show".into(), anilist: Some(1), ..SeriesMeta::default() };
        let link = |id| ev(1, EventBody::Meta { series: "show".into(), anilist: Some(id), episodes: None });
        let build = |id| build(&files, &[link(id)], std::slice::from_ref(&cached));
        assert!(build(2).get("show").unwrap().meta.is_none(), "cached for anime 1, linked to 2");
        assert!(build(1).get("show").unwrap().meta.is_some());
    }

    #[test]
    fn watched_through_skips_unnumbered_episodes() {
        let files: Vec<FileRow> = (1..=4).map(|n| row("anime", &format!("Show/Show - {n:02}.mkv"), true, 1)).collect();
        // An episode item without a number (here only known from history).
        let unnumbered = ItemKey { kind: ItemKind::Episode, ep: None, label: String::new() };
        let events = vec![ev(1, EventBody::Unwatched { series: "show".into(), item: unnumbered, file: None })];
        let lib = build(&files, &events, &[]);
        assert_eq!(lib.get("show").unwrap().unwatched_through(EpNo::new(3)), vec![ep(1), ep(2), ep(3)]);
    }

    /// An episode without a number goes after the numbered ones: listed
    /// there, and next up only once they are watched.
    #[test]
    fn unnumbered_episodes_come_after_numbered_ones() {
        let files: Vec<FileRow> = (1..=2).map(|n| row("anime", &format!("Show/Show - {n:02}.mkv"), true, 1)).collect();
        let unnumbered = ItemKey { kind: ItemKind::Episode, ep: None, label: String::new() };
        let known = ev(1, EventBody::Unwatched { series: "show".into(), item: unnumbered.clone(), file: None });
        let watched = [1, 2].map(|n| ev(i64::from(n) + 1, EventBody::watched("show", ep(n))));
        let keys = |s: &Series| s.ordered_items(false, true).iter().map(|i| i.key.clone()).collect::<Vec<_>>();
        for events in [vec![known.clone()], [vec![known], watched.to_vec()].concat()] {
            let mut lib = build(&files, &events, &[]);
            let s = &mut lib.series[0];
            // Put the unnumbered episode on disk too (no file name parses as one).
            let file = s.items[0].files.clone();
            s.items.iter_mut().find(|i| i.key == unnumbered).unwrap().files = file;
            assert_eq!(keys(s), [ep(1), ep(2), unnumbered.clone()]);
            assert_eq!(s.ordered_count(false, false), 3);
            assert_eq!(s.disk_range(), "01–02");
            let queued: Vec<ItemKey> = s.queue_candidates(5, |_| false).into_iter().map(|(k, _)| k).collect();
            if s.watched_count() == 0 {
                assert_eq!(s.next_up().unwrap().key, ep(1));
                // The unnumbered one shares episode 1's file here, so it is listed once.
                assert_eq!(queued, [ep(1), ep(2)]);
            } else {
                assert_eq!(s.next_up().unwrap().key, unnumbered, "offered once the numbered run is done");
                assert_eq!(queued, std::slice::from_ref(&unnumbered));
            }
        }
    }

    /// Extras follow the first episode with their number; the others (no
    /// number, or no such episode) come last, in their own order.
    #[test]
    fn extras_are_listed_after_their_episode() {
        let files: Vec<FileRow> = (1..=2).map(|n| row("anime", &format!("Show/Show - {n:02}.mkv"), true, 1)).collect();
        let mut lib = build(&files, &[], &[]);
        let s = &mut lib.series[0];
        let item = |kind, ep: Option<u32>, label: &str| Item {
            key: ItemKey { kind, ep: ep.map(EpNo::new), label: label.into() },
            files: s.items[0].files.clone(),
            state: WatchState::default(),
        };
        let added = [
            item(ItemKind::Extra, Some(3), "x3"),
            item(ItemKind::Extra, Some(1), "x1"),
            item(ItemKind::Special, Some(1), "special"),
            item(ItemKind::Extra, None, "loose"),
            item(ItemKind::Extra, Some(1), "x1b"),
        ];
        s.items.extend(added);
        let order: Vec<String> = s.ordered_items(true, false).iter().map(|i| i.key.describe()).collect();
        // ep 1, x1, x1b, ep 2, special, x3, loose
        let expected = [0, 3, 6, 1, 4, 2, 5].map(|i| s.items[i].key.describe());
        assert_eq!(order, expected);
        assert_eq!(s.ordered_count(true, false), order.len());
    }

    #[test]
    fn inbox_membership_and_order() {
        let files = vec![
            row("dl", "[A] Fresh Show - 01.mkv", true, 50),
            row("dl", "[A] Older Show - 01.mkv", true, 10),
            row("dl", "[A] Watched Show S2 - 01.mkv", true, 5),
            row("dl", "[A] Watched Show Extra Story - 01.mkv", true, 60),
            row("dl", "[A] Followed Show - 02.mkv", true, 70),
            row("dl", "[A] Skipped Show - 01.mkv", true, 80),
            row("dl", "[A] Gone Show - 01.mkv", false, 90),
            row("anime", "Archive Show/Archive Show - 01.mkv", true, 99),
            row("anime", "Watched Show/Watched Show - 01.mkv", true, 1),
        ];
        let events = vec![
            ev(1, EventBody::watched("watched show", ep(1))),
            ev(2, EventBody::status("followed show", SeriesStatus::Following, None)),
            ev(3, EventBody::status("skipped show", SeriesStatus::Skipped, None)),
        ];
        let lib = build(&files, &events, &[]);
        let inbox: Vec<(&str, Option<Hint>)> =
            lib.inbox(&cfg()).iter().map(|e| (lib.series[e.index].key.as_str(), e.hint)).collect();
        let watched = lib.index_of("watched show").unwrap();
        assert_eq!(
            inbox,
            vec![
                // A new season of something you watched comes first…
                ("watched show s2", Some(Hint::NewSeasonOf(watched))),
                // …then new shows, newest first (by file time)…
                ("fresh show", None),
                ("older show", None),
                // …and side entries of other series last.
                ("watched show extra story", Some(Hint::PartOf(watched))),
            ],
            "followed, skipped, gone and archive-only series stay out"
        );
    }

    #[test]
    fn anilist_prequels_mark_sequels_without_matching_titles() {
        use crate::index::db::SeriesMeta;
        let files = vec![
            row("dl", "[A] Totally Different Name - 01.mkv", true, 1),
            row("anime", "Old Name/Old Name - 01.mkv", true, 1),
        ];
        let meta = |series: &str, id| SeriesMeta { series: series.into(), anilist: Some(id), ..SeriesMeta::default() };
        let events = [ev(1, EventBody::status("old name", SeriesStatus::Completed, None))];
        let metas = [meta("totally different name", 2), meta("old name", 1)];
        let lib = build(&files, &events, &metas);
        assert_eq!(lib.inbox(&cfg())[0].hint, None);
        let index = index(&files, MetaCache { prequels: HashMap::from([(2, vec![1])]), ..rows(&metas) });
        let lib = Library::build(&cfg(), &events, &index);
        let old = lib.index_of("old name").unwrap();
        assert_eq!(lib.inbox(&cfg())[0].hint, Some(Hint::NewSeasonOf(old)));
        assert_eq!(lib.get("old name").unwrap().prequels, None, "only the sequel's anime has prequels");
    }

    #[test]
    fn match_attempts_mark_series_as_checked() {
        let files = [row("dl", "[A] Zzz - 01.mkv", true, 1), row("dl", "[A] Yyy - 01.mkv", true, 1)];
        let attempt = MatchAttempt { series: "zzz".into(), db_version: "v1".into(), at: 1, result: "none".into() };
        let index = index(&files, MetaCache { attempts: vec![attempt], ..MetaCache::default() });
        let lib = Library::build(&cfg(), &[], &index);
        assert!(lib.get("zzz").unwrap().match_checked);
        assert!(!lib.get("yyy").unwrap().match_checked);
    }

    #[test]
    fn search_text_is_computed_once_and_follows_renames_and_merges() {
        let files = [row("dl", "[A] Kusuriya - 01.mkv", true, 1), row("dl", "[A] Apothecary - 01.mkv", true, 1)];
        let plain = build(&files, &[], &[]);
        assert_eq!(plain.get("kusuriya").unwrap().search_text(), "Kusuriya", "key equals the title's key");
        let events = vec![
            ev(1, EventBody::Title { series: "kusuriya".into(), title: "The Apothecary Diaries".into() }),
            ev(2, EventBody::Alias { from: "apothecary".into(), to: "kusuriya".into() }),
        ];
        let lib = build(&files, &events, &[]);
        let s = lib.get("kusuriya").unwrap();
        assert_eq!(s.search_text(), "The Apothecary Diaries kusuriya apothecary");
    }

    // ---- keys from before kana voiced marks were kept (`バカ` was `ハカ`) ----

    fn baka_files() -> Vec<FileRow> {
        vec![row("anime", "バカ/バカ - 01.mkv", true, 1), row("anime", "バカ/バカ - 02.mkv", true, 1)]
    }

    #[test]
    fn history_under_a_legacy_key_follows_the_series_on_disk() {
        let (legacy, key) = ("ハカ", "バカ");
        let link = EventBody::Meta { series: legacy.into(), anilist: Some(7), episodes: Some(12) };
        let events = vec![
            ev(1, EventBody::status(legacy, SeriesStatus::Following, Some("good".into()))),
            ev(2, EventBody::watched(legacy, ep(1))),
            ev(3, EventBody::Title { series: legacy.into(), title: "Baka!".into() }),
            ev(4, link),
            ev(5, EventBody::watched(legacy, ep(2))),
            // Newer events are written under the new key and win over older ones.
            ev(6, EventBody::Unwatched { series: key.into(), item: ep(2), file: None }),
        ];
        let lib = build(&baka_files(), &events, &[]);
        assert_eq!(lib.series.len(), 1, "one series, not a legacy one beside it");
        let s = lib.get(key).unwrap();
        assert_eq!((s.key.as_str(), s.title.as_str()), (key, "Baka!"));
        assert_eq!((s.status, s.note.as_deref()), (SeriesStatus::Following, Some("good")));
        assert_eq!((s.link, s.episodes_override), (Link::Manual(7), Some(12)));
        assert!(s.item(&ep(1)).unwrap().state.is_watched());
        assert!(!s.item(&ep(2)).unwrap().state.is_watched());
        assert_eq!(s.next_up().unwrap().key, ep(2));
        assert_eq!(lib.get(legacy).unwrap().key, key, "the old key still finds it");
        assert!(s.aliases.is_empty(), "the old key is not shown as a merged name");
    }

    #[test]
    fn merges_under_a_legacy_key_move_with_the_series() {
        let events = vec![
            ev(1, EventBody::watched("ハカ", ep(1))),
            ev(2, EventBody::Alias { from: "ハカ".into(), to: "silly show".into() }),
            ev(3, EventBody::Alias { from: "other name".into(), to: "ハカ".into() }),
            ev(4, EventBody::watched("other name", ep(2))),
        ];
        let lib = build(&baka_files(), &events, &[]);
        let s = lib.get("バカ").unwrap();
        assert_eq!(s.key, "silly show", "the user merged it into another series");
        assert_eq!(s.watched_count(), 2);
        assert_eq!(s.aliases, vec!["other name", "バカ"]);
        let unmerge = [events, vec![ev(5, EventBody::Unalias { from: "ハカ".into() })]].concat();
        let lib = build(&baka_files(), &unmerge, &[]);
        assert_eq!(lib.get("バカ").unwrap().key, "バカ", "unmerging works on the legacy name too");
    }

    #[test]
    fn the_exact_key_owner_keeps_its_history() {
        // `ハカ` really is a series on disk, so its history is its own.
        let mut files = baka_files();
        files.push(row("anime", "ハカ/ハカ - 01.mkv", true, 1));
        let events = vec![ev(1, EventBody::watched("ハカ", ep(1)))];
        let lib = build(&files, &events, &[]);
        assert_eq!(lib.series.len(), 2);
        assert_eq!(lib.get("ハカ").unwrap().watched_count(), 1);
        assert_eq!(lib.get("バカ").unwrap().watched_count(), 0, "not aliased onto the lookalike");
    }

    #[test]
    fn a_legacy_key_shared_by_several_series_is_not_guessed() {
        // `バカ` and `ハガ` both used to be `ハカ`: nothing says which one it was.
        let mut files = baka_files();
        files.push(row("anime", "ハガ/ハガ - 01.mkv", true, 1));
        let events = vec![ev(1, EventBody::watched("ハカ", ep(1)))];
        let lib = build(&files, &events, &[]);
        assert_eq!(lib.get("バカ").unwrap().watched_count(), 0);
        assert_eq!(lib.get("ハガ").unwrap().watched_count(), 0);
        assert_eq!(lib.get("ハカ").unwrap().watched_count(), 1, "stays as the history-only series it was");
    }

    #[test]
    fn history_only_series_keep_their_legacy_key() {
        let events = vec![ev(1, EventBody::watched("ハカ", ep(1)))];
        let lib = build(&[], &events, &[]);
        assert_eq!(lib.series.len(), 1);
        assert_eq!(lib.series[0].key, "ハカ");
    }

    #[test]
    fn caches_under_a_legacy_key_are_found() {
        let meta = |series: &str, anilist| SeriesMeta {
            series: series.into(),
            anilist: Some(anilist),
            ..SeriesMeta::default()
        };
        let attempt = MatchAttempt { series: "ハカ".into(), db_version: "v1".into(), at: 1, result: "none".into() };
        let mut index = index(&baka_files(), MetaCache { attempts: vec![attempt], ..rows(&[meta("ハカ", 1)]) });
        let s = |index: &Index| Library::build(&cfg(), &[], index).get("バカ").unwrap().clone();
        let old = s(&index);
        assert_eq!(old.meta.as_ref().map(|m| (m.series.as_str(), m.anilist)), Some(("バカ", Some(1))), "re-keyed");
        assert_eq!(old.anilist_id(), Some(1));
        assert!(!old.match_checked, "a \"no match\" for the old key is not one for the new key");
        // A row under the new key is preferred.
        index.meta.rows.push(meta("バカ", 2));
        assert_eq!(s(&index).anilist_id(), Some(2));
    }

    #[test]
    fn a_manual_link_finds_its_row_under_either_key() {
        let meta = |series: &str, anilist| SeriesMeta {
            series: series.into(),
            anilist: Some(anilist),
            ..SeriesMeta::default()
        };
        let events = [ev(1, EventBody::Meta { series: "バカ".into(), anilist: Some(2), episodes: None })];
        // The new key's row is for another anime; the legacy row is the linked one.
        let lib = build(&baka_files(), &events, &[meta("バカ", 1), meta("ハカ", 2)]);
        assert_eq!(lib.get("バカ").unwrap().meta.as_ref().and_then(|m| m.anilist), Some(2));
    }

    fn alias(from: &str, to: &str) -> EventBody {
        EventBody::Alias { from: from.into(), to: to.into() }
    }

    #[test]
    fn legacy_aliases_are_proposed_once_for_keys_with_history() {
        let mut files = baka_files();
        files.push(row("anime", "フレンズ/フレンズ - 01.mkv", true, 1));
        let watched = vec![ev(1, EventBody::watched("ハカ", ep(1)))];
        let lib = build(&files, &watched, &[]);
        assert_eq!(lib.legacy_keys().len(), 2, "both series have a legacy key");
        // `フレンス` has no history: nothing to make permanent.
        assert_eq!(lib.legacy_aliases(), vec![alias("ハカ", "バカ")]);
        assert!(build(&files, &[], &[]).legacy_aliases().is_empty());

        // Written once: the alias itself is in the log next time.
        let with = [watched.clone(), vec![ev(2, alias("ハカ", "バカ"))]].concat();
        let again = build(&files, &with, &[]);
        assert!(again.legacy_aliases().is_empty());
        // It changes nothing while the mapping applies.
        let (a, b) = (lib.get("バカ").unwrap(), again.get("バカ").unwrap());
        assert_eq!((a.watched_count(), &a.aliases), (b.watched_count(), &b.aliases));
        assert_eq!(again.series.len(), lib.series.len());

        // The user's own merge or unmerge of the old key is left alone.
        for decided in [alias("ハカ", "silly show"), EventBody::Unalias { from: "ハカ".into() }] {
            let log = [watched.clone(), vec![ev(2, decided)]].concat();
            assert!(build(&files, &log, &[]).legacy_aliases().is_empty());
        }
        // A merge naming it counts as history.
        let merged = vec![ev(1, alias("other name", "ハカ"))];
        assert_eq!(build(&files, &merged, &[]).legacy_aliases(), vec![alias("ハカ", "バカ")]);
    }

    /// Once the alias is written, a real `ハカ` appearing on disk later does not
    /// take the history: the alias merges it into `バカ` (accepted trade-off,
    /// see `docs/events.md`; it can be unmerged).
    #[test]
    fn a_later_lookalike_does_not_steal_migrated_history() {
        let events = vec![
            ev(1, EventBody::watched("ハカ", ep(1))),
            ev(2, EventBody::status("ハカ", SeriesStatus::Following, None)),
            ev(3, alias("ハカ", "バカ")),
        ];
        let mut files = baka_files();
        files.push(row("anime", "ハカ/ハカ - 03.mkv", true, 1));
        let lib = build(&files, &events, &[]);
        assert!(lib.legacy_keys().is_empty(), "the exact key is on disk: no mapping");
        assert!(lib.legacy_aliases().is_empty());
        let s = lib.get("バカ").unwrap();
        assert_eq!((s.key.as_str(), s.status), ("バカ", SeriesStatus::Following));
        assert!(s.item(&ep(1)).unwrap().state.is_watched());
        assert_eq!(lib.get("ハカ").unwrap().key, "バカ", "merged into the series that had the history");
        assert_eq!(lib.series.len(), 1);
        // Without the alias the lookalike would own the old history.
        let lib = build(&files, &events[..2], &[]);
        assert_eq!(lib.get("ハカ").unwrap().watched_count(), 1);
        assert_eq!(lib.get("バカ").unwrap().watched_count(), 0);

        // Also when the series is gone from disk, or for a reader without the mapping.
        let lib = build(&[], &events, &[]);
        assert_eq!(lib.get("ハカ").unwrap().key, "バカ");
        assert_eq!(lib.get("バカ").unwrap().watched_count(), 1);
        assert_eq!(State::replay(&events).resolve("ハカ"), "バカ");
    }

    #[test]
    fn unmerging_covers_the_legacy_spelling() {
        // Merged into another series by a version that keyed it `ハカ`.
        let events = vec![ev(1, EventBody::watched("ハカ", ep(1))), ev(2, alias("ハカ", "silly show"))];
        let files = [baka_files(), vec![row("anime", "Silly Show/Silly Show - 01.mkv", true, 1)]].concat();
        let lib = build(&files, &events, &[]);
        assert_eq!(lib.get("バカ").unwrap().key, "silly show");
        assert!(lib.is_merged_away("バカ") && lib.is_merged_away("ハカ"));
        assert!(!lib.is_merged_away("silly show"));
        let bodies = lib.unmerge_events("ハカ");
        assert_eq!(bodies, lib.unmerge_events("バカ"), "either spelling");
        assert_eq!(
            bodies,
            vec![
                EventBody::Unalias { from: "バカ".into() },
                EventBody::Unalias { from: "ハカ".into() },
                alias("ハカ", "バカ")
            ]
        );
        let after: Vec<Event> = events.iter().cloned().chain(bodies.into_iter().map(|b| ev(3, b))).collect();
        let lib = build(&files, &after, &[]);
        let s = lib.get("バカ").unwrap();
        assert_eq!((s.key.as_str(), s.watched_count()), ("バカ", 1), "split off, with its history");
        assert_eq!(lib.series.len(), 2);
        // A reader without the mapping agrees.
        assert_eq!(State::replay(&after).resolve("ハカ"), "バカ");
        // Nothing to point back when the legacy key has no history.
        let lib = build(&files, &[ev(1, alias("バカ", "silly show"))], &[]);
        assert_eq!(lib.unmerge_events("バカ"), vec![EventBody::Unalias { from: "バカ".into() }]);
    }

    /// The keys resolved when the library is built resolve as following the
    /// legacy mapping and alias chains (cycles included) would.
    #[test]
    fn every_key_resolves_like_following_the_aliases() {
        let events = vec![
            ev(1, EventBody::watched("ハカ", ep(1))),
            ev(2, alias("a", "b")),
            ev(3, alias("b", "c")),
            ev(4, alias("x", "y")),
            ev(5, alias("y", "x")),
            ev(6, alias("c", "ハカ")),
            ev(7, EventBody::watched("y", ep(1))),
        ];
        let lib = build(&baka_files(), &events, &[]);
        for k in ["ハカ", "バカ", "a", "b", "c", "x", "y", "z"] {
            let slow = crate::events::resolve_alias(&lib.aliases, lib.current_key(k));
            assert_eq!(lib.resolve(k), slow, "{k}");
            assert_eq!(lib.index_of(k), lib.series.iter().position(|s| s.key == slow), "{k}");
        }
        assert_eq!(lib.get("a").unwrap().key, "バカ");
        assert_eq!(lib.get("y").unwrap().key, "x");
    }

    #[test]
    fn disk_ranges() {
        let files = vec![
            row("dl", "[A] X - 03.mkv", true, 1),
            row("dl", "[A] X - 01.mkv", true, 1),
            row("dl", "[A] Y - 07.mkv", true, 1),
        ];
        let lib = build(&files, &[], &[]);
        assert_eq!(lib.get("x").unwrap().disk_range(), "01–03");
        assert_eq!(lib.get("y").unwrap().disk_range(), "07");
    }

    #[test]
    fn title_casing() {
        assert_eq!(title_case("one piece"), "One Piece");
        assert_eq!(title_case(""), "");
    }
}
