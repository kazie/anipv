//! The in-memory library: files from the index joined with replayed state.
//!
//! This is what the TUI and CLI render. It is cheap to rebuild (a few tens of
//! milliseconds for ~15k files), so it is simply rebuilt after every change.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::app::Index;
use crate::config::{Config, RootKind};
use crate::events::{Event, EventBody, Link, Moved, Relabelled, SetBy, State};
use crate::franchise::{Folder, Franchise, Part};
use crate::index::classify::{Classified, Entry, classify, classify_split};
use crate::index::db::{FileRow, MatchResult, SeriesMeta};
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
    /// The entry folder of a franchise folder it is in (see [`Classified::entry`]).
    pub entry: Option<Entry>,
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
    pub files: Vec<Arc<FileRef>>,
    /// Watch state.
    pub state: WatchState,
}

impl Item {
    /// The file to play: present, highest version, most recently added.
    pub fn best_file(&self) -> Option<&FileRef> {
        self.files
            .iter()
            .map(|f| &**f)
            .filter(|f| f.present)
            .max_by_key(|f| (f.version.unwrap_or(1), f.added(), f.size))
    }

    /// True if any file is on disk.
    pub fn present(&self) -> bool {
        self.files.iter().any(|f| f.present)
    }

    /// A file name for this item (the one to play, else any known), recorded in
    /// events as evidence of what was watched.
    pub fn file_name(&self) -> Option<String> {
        self.best_file().or_else(|| self.files.first().map(|f| &**f)).map(FileRef::name)
    }

    /// One of the numbered episodes an episode total counts: a whole number
    /// above zero. An episode `00` prologue or a `12.5` recap is not.
    pub fn counts_toward_total(&self) -> bool {
        self.key.kind == ItemKind::Episode && self.key.ep.is_some_and(|e| e.is_whole() && e.whole() > 0)
    }
}

/// What a series' last match that linked nothing came to, and when (see
/// [`crate::index::db::MatchAttempt`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastMatch {
    /// What it came to.
    pub result: MatchResult,
    /// When it ran.
    pub at: i64,
    /// It ran with the current offline database version. A title search's
    /// answer from an older version still stands, but the newer database is
    /// asked again (see [`crate::meta::needs_match`]).
    pub current: bool,
}

/// A series with its items and user state.
#[derive(Debug, Clone)]
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
    /// The last match that linked nothing, by the offline database or by
    /// title, and whether it still stands (see [`LastMatch::current`]).
    pub last_match: Option<LastMatch>,
    /// `AniList` link chosen by the user (from events).
    pub link: Link,
    /// anipv completed this series automatically before.
    pub auto_completed: bool,
    /// anipv followed this series automatically before (see
    /// [`crate::app::Ctx::auto_complete`]).
    pub auto_followed: bool,
    /// Who set the current status.
    pub status_by: SetBy,
    /// How the user set it, when not by hand (imported, caught up).
    pub status_via: Option<crate::events::StatusVia>,
    /// The user's latest status was chosen once the series was finished.
    pub kept: bool,
    /// `AniList` ids of this anime's prequels (`None` until fetched).
    pub prequels: Option<Vec<u64>>,
    /// `AniList` ids of this anime's sequels (`None` until fetched).
    pub sequels: Option<Vec<u64>>,
    /// The franchise folder its files are in.
    pub folder: Option<Folder>,
    /// [`Series::shared_dirs`], worked out when first asked for.
    shared: std::sync::OnceLock<Vec<(String, usize)>>,
    /// When its sequels (and prequels) were last fetched.
    pub relations_at: Option<i64>,
    /// Manual episode total (from events).
    pub episodes_override: Option<u32>,
    /// Other keys merged into this one.
    pub aliases: Vec<String>,
    /// What fuzzy search matches against (see [`Series::search_text`]),
    /// made when first asked for.
    search: std::sync::OnceLock<String>,
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

    /// How many [`Series::new_episodes`] there are.
    pub fn new_count(&self) -> usize {
        self.new_iter().count()
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
        self.anilist_id_with(self.meta.as_ref())
    }

    /// `m` if it can be this series' metadata under its link: never when it
    /// is unlinked, only a row for the linked anime when it is linked by hand.
    /// The filter [`Series::meta`] went through when the library was built,
    /// for rows from elsewhere (e.g. fresh from a sync).
    pub fn usable_meta<'a>(&self, m: Option<&'a SeriesMeta>) -> Option<&'a SeriesMeta> {
        m.filter(|m| link_admits(self.link, m))
    }

    /// [`Series::anilist_id`] with `m` as its metadata: the manual link, else
    /// the id of `m` if it is [usable](Series::usable_meta).
    pub fn anilist_id_with(&self, m: Option<&SeriesMeta>) -> Option<u64> {
        self.link.manual().or_else(|| self.usable_meta(m)?.anilist)
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
        self.status.is_tracked() || self.watched_count() > 0
    }

    /// Items `ordered_items(extras, false)` leaves out that `missing` would
    /// show: those not on disk, unless nothing is (then all are listed).
    pub fn missing_count(&self, extras: bool) -> usize {
        if !self.has_watchable_on_disk() {
            return 0;
        }
        self.items.iter().filter(|i| listed(i, extras, true) && !i.present()).count()
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

    /// Its (automatic) link came from a title search on `AniList`, which the
    /// offline database can't confirm (see [`SeriesMeta::search_linked`]).
    pub fn search_linked(&self) -> bool {
        self.link == Link::Auto && self.meta.as_ref().is_some_and(|m| m.search_linked)
    }

    /// Worth matching to an anime in the offline database: on disk, or with
    /// a status. A series known only from history isn't.
    pub fn worth_matching(&self) -> bool {
        self.present() || self.status != SeriesStatus::Untracked
    }

    /// Worth looking up on `AniList` by title (which sends the title): on
    /// disk, or tracked. Not a show dropped or skipped and gone from disk.
    pub fn worth_searching(&self) -> bool {
        self.present() || self.status.is_tracked()
    }

    /// Done, and not auto-completed before (so a user who set it back to
    /// following isn't overridden), nor given its status by the user while it
    /// was already done (until they change it again). A followed series like
    /// that is completed by [`crate::app::Ctx::auto_complete`].
    pub fn may_auto_complete(&self) -> bool {
        !self.auto_completed && !self.kept && self.is_done()
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
    /// would let a query match across the two copies.) Made once, when first
    /// asked for.
    pub fn search_text(&self) -> &str {
        self.search.get_or_init(|| search_text(&self.title, &self.key, &self.aliases, &self.other_names()))
    }

    /// What its metadata calls it (`AniList`'s romaji, English and native
    /// titles) other than its title, key and the names merged into it.
    pub fn other_names(&self) -> Vec<&str> {
        let title = self.title.to_lowercase();
        let known = |name: &str| {
            let key = crate::identity::series_key(name);
            name.to_lowercase() == title || key == self.key || self.aliases.contains(&key)
        };
        self.meta.iter().flat_map(SeriesMeta::names).filter(|n| !known(n)).collect()
    }

    /// Every key the series goes by: its own, then the merged-away ones.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.key.as_str()).chain(self.aliases.iter().map(String::as_str))
    }

    /// Airing summary from metadata (`ep 9 in 1d 7h`, `finished`, …) and
    /// whether it is "live" (still airing) for highlighting.
    pub fn airing(&self, now: i64) -> Option<(String, bool)> {
        let m = self.meta.as_ref()?;
        if let (Some(e), Some(t)) = (m.next_ep, m.next_airing) {
            return Some((format!("ep {e} {until}", until = crate::fmt::until(t, now)), true));
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

    /// Up to `n` new episodes with the file to play for each, skipping `skip`.
    ///
    /// A multi-episode file (`- 001&002`) is listed once, under its first episode.
    pub fn queue_candidates(&self, n: usize, skip: impl Fn(&ItemKey) -> bool) -> Vec<(ItemKey, PathBuf)> {
        let mut seen = HashSet::new();
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
        self.items.iter().flat_map(|i| &i.files).filter(|f| f.present).map(|f| f.added()).max()
    }

    /// True if any file of the series is on disk.
    pub fn present(&self) -> bool {
        self.items.iter().any(Item::present)
    }

    /// Find an item by key.
    pub fn item(&self, key: &ItemKey) -> Option<&Item> {
        self.items.iter().find(|i| &i.key == key)
    }

    /// Folders inside this series' folder that hold episodes another folder
    /// of it also has on disk: another anime kept in the same folder (see
    /// [`crate::events::EventBody::Split`]), or a second release. Each with
    /// the number of such episodes, the folder with the most files (and
    /// series folders themselves) left out. Only archive roots (see
    /// [`RootKind::Archive`]) have series folders to look in. Worked out
    /// when first asked for (shown for one series at a time).
    pub fn shared_dirs(&self, cfg: &Config) -> &[(String, usize)] {
        self.shared.get_or_init(|| self.shared_episode_dirs(&cfg.root_kinds()))
    }

    fn shared_episode_dirs(&self, kinds: &HashMap<&str, RootKind>) -> Vec<(String, usize)> {
        // A franchise's entry folder is a series of its own already: nothing to split.
        let dir_of = |f: &FileRef| {
            let archive = kinds.get(f.root.as_str()) == Some(&RootKind::Archive) && f.entry.is_none();
            f.rel.parent().filter(|_| archive).map(|p| (f.root.clone(), p.to_path_buf()))
        };
        let mut files_in: HashMap<(String, PathBuf), usize> = HashMap::new();
        for f in self.items.iter().flat_map(|i| &i.files).filter(|f| f.present) {
            if let Some(d) = dir_of(f) {
                *files_in.entry(d).or_default() += 1;
            }
        }
        let mut shared: BTreeMap<(String, PathBuf), usize> = BTreeMap::new();
        for it in self.episodes() {
            let dirs: BTreeSet<(String, PathBuf)> =
                it.files.iter().filter(|f| f.present).filter_map(|f| dir_of(f)).collect();
            // Copies in different roots (downloads and archive) are expected:
            // only folders under one series folder count.
            let top = |d: &(String, PathBuf)| (d.0.clone(), d.1.components().next().map(|c| c.as_os_str().to_owned()));
            let mut per_top: HashMap<_, usize> = HashMap::new();
            for d in &dirs {
                *per_top.entry(top(d)).or_default() += 1;
            }
            for d in dirs.iter().filter(|d| per_top.get(&top(d)).is_some_and(|n| *n > 1)) {
                *shared.entry(d.clone()).or_default() += 1;
            }
        }
        let main = shared
            .keys()
            .max_by_key(|d| (files_in.get(*d).copied().unwrap_or(0), std::cmp::Reverse(d.1.components().count())))
            .cloned();
        shared
            .into_iter()
            // A series folder itself can't be split off its series.
            .filter(|(d, _)| Some(d) != main.as_ref() && d.1.components().count() >= 2)
            .map(|((_, d), n)| (d.to_string_lossy().into_owned(), n))
            .collect()
    }
}

/// True if the metadata row `m` can belong to a series linked by `link`: an
/// unlinked series ignores a link cached before the unlink arrived, and a
/// manual link ignores data cached for another anime (until the next sync
/// replaces it).
fn link_admits(link: Link, m: &SeriesMeta) -> bool {
    match link {
        Link::Off => false,
        Link::Manual(id) => m.is_for(id),
        Link::Auto => true,
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
    pub file: Arc<FileRef>,
    /// Canonical series key it belongs to.
    pub series: String,
    /// Items it provides.
    pub items: Vec<ItemKey>,
    /// The key it had before (see [`Classified::earlier`]).
    pub earlier: Option<String>,
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
    /// Merged-away key → the key it was merged into (as the log settled them).
    aliases: HashMap<String, String>,
    /// Legacy series key → the key it is now (see [`legacy_keys`]).
    legacy: HashMap<String, String>,
    /// The reverse of `legacy`: current key → its legacy spellings, sorted.
    legacy_of: HashMap<String, Vec<String>>,
    /// Mapped legacy keys the event log names (see [`Library::unmerge_events`]).
    legacy_named: HashSet<String>,
    /// Mapped legacy keys the event log merges or unmerges (see [`Library::legacy_aliases`]).
    legacy_decided: HashSet<String>,
    /// Series that belong together, in watch order (see [`crate::franchise`]).
    pub franchises: Vec<Franchise>,
    /// The franchise of each series (by index), and the stop of it you are
    /// at (see [`Library::franchise_of`]).
    franchise_at: Vec<Option<(usize, usize)>>,
    /// Folders split off their series, `(root, dir)` (see
    /// [`crate::events::settled_splits`]).
    pub splits: Vec<(String, String)>,
    /// The offline database version the cached attempts are judged against
    /// (see [`crate::app::MetaCache::db_version`]).
    pub db_version: String,
    /// Problems met while building that did not stop it (e.g. the cache
    /// migration in [`crate::app::Ctx::migrated_library`] failing), for the UI to show.
    pub warnings: Vec<String>,
    /// Part of the event log could not be read (see [`Library::warnings`]).
    /// Nothing is recorded automatically from such a library: the missing
    /// part may hold choices that would be overridden.
    pub partial_history: bool,
}

/// Group the classified files by the series they belong to now (after the
/// merges in `aliases`, see [`crate::events::resolve_alias`]).
fn group(
    aliases: &HashMap<String, String>,
    classified: Vec<(&FileRow, RootKind, &Classified)>,
) -> (HashMap<String, Building>, Vec<IndexedFile>) {
    let mut building: HashMap<String, Building> = HashMap::new();
    let mut indexed = Vec::with_capacity(classified.len());
    for (row, kind, c) in classified {
        let Classified { series, title, parsed, entry, .. } = c;
        let canon = crate::events::resolve_alias(aliases, series);
        let items = c.items();
        let file = Arc::new(FileRef {
            path: row.path.clone(),
            root: row.root.clone(),
            rel: row.rel.clone(),
            size: row.size,
            present: row.present,
            first_seen: row.first_seen,
            mtime: row.mtime,
            version: parsed.version,
            entry: entry.clone(),
        });
        // Most files join a series already started: no key copy for those.
        let b = match building.get_mut(canon) {
            Some(b) => b,
            None => building.entry(canon.to_string()).or_default(),
        };
        // Count its files in each franchise folder (see `Building::folder`).
        if let Some(e) = entry {
            if !b.folders.iter().any(|t| t.root == row.root && t.name == e.franchise) {
                let (root, name) = (row.root.clone(), e.franchise.clone());
                b.folders.push(FolderTally { root, name, present: 0, all: 0, first: e.no });
            }
            if let Some(t) = b.folders.iter_mut().find(|t| t.root == row.root && t.name == e.franchise) {
                t.present += usize::from(row.present);
                t.all += 1;
                t.first = t.first.min(e.no);
            }
        }
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
            match b.items.get_mut(it) {
                Some(files) => files.push(file.clone()),
                None => {
                    b.items.insert(it.clone(), vec![file.clone()]);
                }
            }
        }
        indexed.push(IndexedFile { file, series: canon.to_string(), items, earlier: c.earlier.clone() });
    }
    (building, indexed)
}

/// Group `series` into franchises, with the franchise of each and the stop
/// you are at in it: the series' first stop not watched yet, else its last.
fn franchises(series: &[Series]) -> (Vec<Franchise>, Vec<Option<(usize, usize)>>) {
    let nodes: Vec<crate::franchise::Node<'_>> = series
        .iter()
        .map(|s| crate::franchise::Node {
            key: &s.key,
            title: &s.title,
            anilist: s.anilist_id(),
            prequels: s.prequels.as_deref().unwrap_or_default(),
            sequels: s.sequels.as_deref().unwrap_or_default(),
            folder: s.folder.as_ref(),
            parts: entry_parts(s),
            year: s.meta.as_ref().and_then(|m| m.year),
        })
        .collect();
    let mut franchises = crate::franchise::group(&nodes);
    for f in &mut franchises {
        let labels: Vec<&str> = f.stops.iter().map(|stop| stop_label(series, stop)).collect();
        let short = crate::franchise::short_labels(&f.name, &labels);
        for (stop, short) in f.stops.iter_mut().zip(short) {
            stop.watched = stop_watched(series, stop);
            stop.short = short;
        }
    }
    let mut at: Vec<Option<(usize, usize)>> = vec![None; series.len()];
    for (f, franchise) in franchises.iter().enumerate() {
        for (n, stop) in franchise.stops.iter().enumerate() {
            let place = &mut at[stop.series];
            // The first not watched stays; else the last so far.
            let settled = place.is_some_and(|(_, p)| !franchise.stops[p].watched);
            if !settled {
                *place = Some((f, n));
            }
        }
    }
    (franchises, at)
}

/// What to call a stop: [`Library::stop_label`].
fn stop_label<'a>(series: &'a [Series], stop: &'a crate::franchise::Stop) -> &'a str {
    stop.folder.as_ref().map_or(series[stop.series].title.as_str(), |part| part.name.as_str())
}

/// Every item of the stop is watched (for a whole series: it is completed,
/// or finished and fully watched): [`crate::franchise::Stop::watched`].
fn stop_watched(series: &[Series], stop: &crate::franchise::Stop) -> bool {
    let s = &series[stop.series];
    if stop.items.is_empty() {
        return s.status == SeriesStatus::Completed || s.is_done();
    }
    let watched: HashSet<&ItemKey> = s.items.iter().filter(|i| i.state.is_watched()).map(|i| &i.key).collect();
    stop.items.iter().all(|k| watched.contains(k))
}

/// The items of `s` by the entry folder of its franchise folder they are in
/// (see [`crate::index::classify::entry_dir`]): number, name, items. An item
/// with files in several goes with the lowest number.
fn entry_parts(s: &Series) -> Vec<(Part, Vec<ItemKey>)> {
    let Some(folder) = &s.folder else { return Vec::new() };
    let mut parts: BTreeMap<u32, (&str, Vec<ItemKey>)> = BTreeMap::new();
    let mut elsewhere: Vec<&ItemKey> = Vec::new();
    for it in &s.items {
        // The lowest-numbered entry folder of the series' franchise folder holding a file of it.
        let entry = it
            .files
            .iter()
            .filter(|f| f.root == folder.root)
            .filter_map(|f| f.entry.as_ref())
            .filter(|e| e.franchise == folder.name)
            .min_by_key(|e| e.no);
        match entry {
            Some(e) => parts.entry(e.no).or_insert_with(|| (&e.name, Vec::new())).1.push(it.key.clone()),
            None => elsewhere.push(&it.key),
        }
    }
    // An item with no file in an entry folder (a copy in the downloads, a
    // later episode not on disk) goes with the part it follows: the one
    // holding the item before it, else the first. A part isn't done while
    // one of its series' items is left out.
    for key in elsewhere {
        let follows = parts.iter().rev().find(|(_, (_, items))| items.iter().any(|k| k < key)).map(|(no, _)| *no);
        if let Some(no) = follows.or_else(|| parts.keys().next().copied())
            && let Some((_, items)) = parts.get_mut(&no)
        {
            items.push(key.clone());
        }
    }
    parts.into_iter().map(|(no, (name, items))| (Part { no, name: name.to_string() }, items)).collect()
}

/// How `row` is classified in the innermost of the `splits` it lies in, if any.
fn split_class(cfg: &Config, splits: &[(String, String)], row: &FileRow, kind: RootKind) -> Option<Classified> {
    let depth = splits
        .iter()
        .filter(|(root, dir)| *root == row.root && row.rel.parent().is_some_and(|p| p.starts_with(dir)))
        .map(|(_, dir)| Path::new(dir).components().count())
        .max()?;
    Some(classify_split(cfg, kind, &row.rel, Some(depth)))
}

/// Add the series and items known only from history (files deleted or never
/// indexed here). A key without files whose history moved to other series
/// (see [`moved`]) is not one: its status alone doesn't make it a series.
fn add_history(building: &mut HashMap<String, Building>, state: &State, moved: &Moved) {
    // Keys without files whose history went to other series.
    let vacated: HashSet<String> = moved.keys().filter(|k| !building.contains_key(*k)).cloned().collect();
    for (series, items) in &state.items {
        let b = building.entry(series.clone()).or_default();
        for item in items.keys() {
            b.items.entry(item.clone()).or_default();
        }
    }
    for (series, s) in &state.series {
        if (s.status != SeriesStatus::Untracked || s.title.is_some()) && !vacated.contains(series) {
            building.entry(series.clone()).or_default();
        }
    }
}

/// Item events recorded under a key their file had before, when the file
/// belongs to another series now: by the file name they recorded, the series
/// and items the files of that name with that earlier key give now, when they
/// agree (on disk ones, if any are). Nothing is written; the log keeps the
/// old key.
///
/// A file's earlier keys are the one an older version gave it (see
/// [`Classified::earlier`]), and the one it had while a
/// folder it is in was split off (see [`EventBody::Split`]), for after the
/// split is undone.
fn moved(cfg: &Config, events: &[Event], canonical: &HashMap<String, String>, indexed: &[IndexedFile]) -> Moved {
    let mut moved = Moved::new();
    // Every folder ever split, undone or not.
    let ever_split: BTreeSet<(&str, &Path)> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Split { root, dir } | EventBody::Unsplit { root, dir } => Some((root.as_str(), Path::new(dir))),
            _ => None,
        })
        .collect();
    let mut by_name: Option<HashMap<String, Vec<(&IndexedFile, String)>>> = None;
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    for ev in events {
        let (Some(series), Some(_), Some(file)) = (ev.body.series(), ev.body.item(), ev.body.file()) else {
            continue;
        };
        let key = crate::events::rekey(canonical, series);
        if !seen.insert((key, file)) {
            continue;
        }
        // Files deeper than their series folder whose key changed, by name.
        let by_name = by_name.get_or_insert_with(|| {
            let kinds = cfg.root_kinds();
            let mut m: HashMap<String, Vec<(&IndexedFile, String)>> = HashMap::new();
            for f in indexed {
                let Some(&kind) = kinds.get(f.file.root.as_str()) else { continue };
                let split_keys = ever_split
                    .iter()
                    .filter(|(root, dir)| {
                        *root == f.file.root && f.file.rel.parent().is_some_and(|p| p.starts_with(dir))
                    })
                    .map(|(_, dir)| classify_split(cfg, kind, &f.file.rel, Some(dir.components().count())).series);
                for earlier in f.earlier.iter().cloned().chain(split_keys) {
                    let earlier = crate::events::rekey(canonical, &earlier).to_string();
                    if earlier != f.series {
                        m.entry(f.file.name()).or_default().push((f, earlier));
                    }
                }
            }
            m
        });
        let Some(files) = by_name.get(file) else { continue };
        let named: Vec<&IndexedFile> = files.iter().filter(|(_, earlier)| earlier == key).map(|(f, _)| *f).collect();
        let present: Vec<&IndexedFile> = named.iter().copied().filter(|f| f.file.present).collect();
        let pool = if present.is_empty() { &named } else { &present };
        if let [first, rest @ ..] = &pool[..]
            && rest.iter().all(|f| f.series == first.series && f.items == first.items)
        {
            let target = (first.series.clone(), first.items.clone());
            moved.entry(key.to_string()).or_default().insert(file.to_string(), target);
        }
    }
    moved
}

/// Items recorded under a label no file gives any more, when a newer version
/// labels their file differently (a cleaner extras label): the file name an
/// event recorded finds the file in the same series, and its item of the same
/// kind and number is the one that file gives now. Nothing is written; the
/// log keeps the old label.
fn relabelled(
    events: &[Event],
    canonical: &HashMap<String, String>,
    building: &HashMap<String, Building>,
) -> Relabelled {
    let mut relabelled = Relabelled::new();
    for ev in events {
        let (Some(series), Some(item), Some(file)) = (ev.body.series(), ev.body.item(), ev.body.file()) else {
            continue;
        };
        // An episode has no label to change.
        if item.kind == ItemKind::Episode {
            continue;
        }
        let key = crate::events::rekey(canonical, series);
        let Some(b) = building.get(key) else { continue };
        if b.items.contains_key(item) || relabelled.get(key).is_some_and(|m| m.contains_key(item)) {
            continue;
        }
        // Only the label changed: one item of the same kind and number that
        // a file of that name gives (recorded as `FileRef::name` gives it).
        let first = ItemKey { kind: item.kind, ep: item.ep, label: String::new() };
        let named = |f: &Arc<FileRef>| f.rel.file_name().is_some_and(|n| n.to_string_lossy() == file);
        let mut now = b
            .items
            .range(first..)
            .take_while(|(k, _)| k.kind == item.kind && k.ep == item.ep)
            .filter(|(_, files)| files.iter().any(named))
            .map(|(k, _)| k);
        if let (Some(to), None) = (now.next(), now.next()) {
            relabelled.entry(key.to_string()).or_default().insert(item.clone(), to.clone());
        }
    }
    relabelled
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
    /// Series keys with a match on record that linked nothing (see [`LastMatch`]).
    attempted: HashMap<&'a str, LastMatch>,
    /// Canonical key → the keys merged into it.
    aliases_of: HashMap<&'a str, Vec<String>>,
}

impl<'a> Lookups<'a> {
    fn new(
        cfg: &'a Config,
        state: &'a State,
        index: &'a Index,
        legacy_of: &'a HashMap<String, Vec<String>>,
        canonical: &'a HashMap<String, String>,
    ) -> Self {
        let meta_by = index.meta.rows.iter().map(|m| (m.series.as_str(), m)).collect();
        let attempted = index
            .meta
            .attempts
            .iter()
            .map(|a| {
                let current = index.meta.db_version == a.db_version;
                (a.series.as_str(), LastMatch { result: a.result, at: a.at, current })
            })
            .collect();
        // A series' merged-away names: the merged-away keys that lead to it
        // (on an alias cycle the canonical key is itself merged away; it is
        // the series, not one of its other names, and not in `canonical`).
        let mut aliases_of: HashMap<&str, Vec<String>> = HashMap::new();
        for (from, to) in canonical.iter().filter(|(from, _)| state.aliases.contains_key(*from)) {
            aliases_of.entry(to).or_default().push(from.clone());
        }
        Self { cfg, state, index, legacy_of, meta_by, attempted, aliases_of }
    }

    /// The series `key`, from its files and items (`b`), history and caches.
    fn finish_series(&self, key: String, b: Building) -> Series {
        let folder = b.folder();
        let state = self.state;
        let st = state.series.get(&key).cloned().unwrap_or_default();
        // The title set by the user, else the files', else the last played
        // file's (a series known only from history), else the key's.
        let title = st.title.clone().or_else(|| b.voted_title()).unwrap_or_else(|| {
            let played = state.file_hints.get(&key).map(|f| classify(self.cfg, RootKind::Ongoing, Path::new(f)).title);
            played.filter(|t| !t.is_empty()).unwrap_or_else(|| title_case(&key))
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
        // Caches written under a legacy key still belong to the series. The
        // first usable row (see `link_admits`) wins: a row for another anime
        // under one key doesn't hide a matching row under the other.
        let meta = spellings(self.legacy_of, &key)
            .find_map(|k| self.meta_by.get(k).filter(|m| link_admits(st.link, m)))
            .map(|m| SeriesMeta { series: key.clone(), ..(*m).clone() });
        // A "no match" for the old key doesn't count: the new key is a
        // different title to match (`Ctx::migrated_library` drops those rows).
        let last_match = self.attempted.get(key.as_str()).copied();
        let mut s = Series {
            meta,
            last_match,
            key,
            title,
            status: st.status,
            note: st.note,
            items,
            link: st.link,
            auto_completed: st.auto_completed,
            auto_followed: st.auto_followed,
            status_by: st.status_by,
            status_via: st.status_via,
            kept: st.kept,
            prequels: None,
            sequels: None,
            relations_at: None,
            folder,
            shared: std::sync::OnceLock::new(),
            episodes_override: st.episodes,
            aliases,
            search: std::sync::OnceLock::new(),
        };
        if let Some(id) = s.anilist_id() {
            s.prequels = self.index.meta.prequels.get(&id).cloned();
            if let Some((sequels, at)) = self.index.meta.sequels.get(&id) {
                (s.sequels, s.relations_at) = (Some(sequels.clone()), Some(*at));
            }
        }
        s
    }
}

/// A series' files in one franchise folder.
struct FolderTally {
    root: String,
    name: String,
    /// Files on disk, and all of them.
    present: usize,
    all: usize,
    /// The lowest entry number among them.
    first: u32,
}

#[derive(Default)]
struct Building {
    /// Title votes from files on disk, and from gone files (used only when
    /// nothing is on disk, so a deleted file's old name can't win a tie).
    titles: HashMap<String, usize>,
    gone_titles: HashMap<String, usize>,
    items: BTreeMap<ItemKey, Vec<Arc<FileRef>>>,
    /// The franchise folders its files are in (see [`Building::folder`]).
    folders: Vec<FolderTally>,
}

impl Building {
    /// The franchise folder (see [`Series::folder`]): the one holding most
    /// of its files on disk (else of all its files), a tie going to the
    /// lowest entry number.
    fn folder(&self) -> Option<Folder> {
        let best = self.folders.iter().max_by_key(|t| (t.present, t.all, std::cmp::Reverse(t.first)))?;
        Some(Folder { root: best.root.clone(), name: best.name.clone(), first: best.first })
    }

    /// The title most files vote for (those on disk, or else gone ones);
    /// a tie goes to the smallest.
    fn voted_title(&self) -> Option<String> {
        let votes = if self.titles.is_empty() { &self.gone_titles } else { &self.titles };
        votes.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))).map(|(t, _)| t.clone())
    }
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
        let splits = crate::events::settled_splits(events);
        // Files in a split folder are classified again, as their own series.
        let resplit: HashMap<usize, Classified> = if splits.is_empty() {
            HashMap::new()
        } else {
            let split = |(i, (row, kind, _)): (usize, (&FileRow, RootKind, &Classified))| {
                Some((i, split_class(cfg, &splits, row, kind)?))
            };
            index.classified().enumerate().filter_map(split).collect()
        };
        let classified: Vec<(&FileRow, RootKind, &Classified)> = index
            .classified()
            .enumerate()
            .map(|(i, (row, kind, c))| (row, kind, resplit.get(&i).unwrap_or(c)))
            .collect();
        let legacy = legacy_keys(classified.iter().map(|(_, _, c)| (c.series.as_str(), c.legacy_series.as_deref())));
        let aliases = crate::events::settled_aliases(events, &legacy);
        // Resolved once, so lookups by any key are a single lookup.
        let canonical = crate::events::canonical_keys(&legacy, &aliases);
        let (mut building, mut indexed) = group(&aliases, classified);
        let relabelled = relabelled(events, &canonical, &building);
        let moved = moved(cfg, events, &canonical, &indexed);
        let state = State::replay_settled(events, aliases, &canonical, &relabelled, &moved);
        add_history(&mut building, &state, &moved);
        let mut legacy_of: HashMap<String, Vec<String>> = HashMap::new();
        for (old, new) in &legacy {
            legacy_of.entry(new.clone()).or_default().push(old.clone());
        }
        for olds in legacy_of.values_mut() {
            olds.sort_unstable();
        }

        let lookups = Lookups::new(cfg, &state, index, &legacy_of, &canonical);
        let mut series: Vec<Series> = building.into_iter().map(|(key, b)| lookups.finish_series(key, b)).collect();
        series.sort_by_cached_key(|s| (s.title.to_lowercase(), s.key.clone()));
        indexed.sort_by(|a, b| path_order(&a.file.path, &b.file.path));

        let (franchises, franchise_at) = franchises(&series);
        let (legacy_named, legacy_decided) = legacy_mentions(events, &legacy);
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
            franchises,
            franchise_at,
            splits,
            db_version: index.meta.db_version.clone(),
            warnings: Vec::new(),
            partial_history: false,
        }
    }

    /// Folders of the series `s` that [`crate::events::EventBody::Split`]
    /// could split off: `(root, dir)` of every folder below an archive series
    /// folder that holds its files, sorted (not ones split already). Only
    /// archive roots (see [`RootKind::Archive`]) have series folders.
    pub fn splittable(&self, cfg: &Config, s: &Series) -> Vec<(String, String)> {
        let kinds = cfg.root_kinds();
        // Folders, with how many of the series' files are in them.
        let mut dirs: BTreeMap<(String, String), usize> = BTreeMap::new();
        let files: Vec<&Arc<FileRef>> = s.items.iter().flat_map(|i| &i.files).collect();
        for f in &files {
            // A franchise's entry folder is a series of its own already.
            if kinds.get(f.root.as_str()) != Some(&RootKind::Archive) || f.entry.is_some() {
                continue;
            }
            let Some(parent) = f.rel.parent() else { continue };
            for dir in parent.ancestors().filter(|d| d.components().count() >= 2) {
                // A season or extras folder is part of the show, not another anime.
                if dir.file_name().is_some_and(|n| crate::index::classify::is_generic_dir(cfg, &n.to_string_lossy())) {
                    continue;
                }
                *dirs.entry((f.root.clone(), dir.to_string_lossy().into_owned())).or_default() += 1;
            }
        }
        // A folder with all of the series' files in it leaves nothing to split it from.
        dirs.into_iter().filter(|(d, n)| *n < files.len() && !self.splits.contains(d)).map(|(d, _)| d).collect()
    }

    /// Split folders (see [`Library::splittable`]) whose files are in `s`,
    /// or whose series folder is one of `s`'s: what an unsplit of `s` can name.
    pub fn splits_of(&self, s: &Series) -> Vec<(String, String)> {
        let tops: HashSet<(&str, std::path::Component<'_>)> = s
            .items
            .iter()
            .flat_map(|i| &i.files)
            .filter_map(|f| Some((f.root.as_str(), f.rel.components().next()?)))
            .collect();
        self.splits
            .iter()
            .filter(|(root, dir)| {
                Path::new(dir).components().next().is_some_and(|c| tops.contains(&(root.as_str(), c)))
            })
            .cloned()
            .collect()
    }

    /// The index (into [`Library::franchises`]) of the franchise series `i`
    /// (an index into [`Library::series`]) is in.
    pub fn franchise_index(&self, i: usize) -> Option<usize> {
        self.franchise_at.get(i)?.map(|(f, _)| f)
    }

    /// The franchise series `i` (an index into [`Library::series`]) is in,
    /// and the stop of it you are at (0-based): its first stop not watched
    /// yet, else its last.
    pub fn franchise_pos(&self, i: usize) -> Option<(&Franchise, usize)> {
        let (f, pos) = (*self.franchise_at.get(i)?)?;
        Some((&self.franchises[f], pos))
    }

    /// [`Library::franchise_pos`] by key.
    pub fn franchise_of(&self, key: &str) -> Option<(&Franchise, usize)> {
        self.franchise_pos(self.index_of(key)?)
    }

    /// What to call a stop: its entry folder's name (`Kabukimonogatari`),
    /// else the series' title.
    pub fn stop_label<'a>(&'a self, stop: &'a crate::franchise::Stop) -> &'a str {
        stop_label(&self.series, stop)
    }

    /// The short name ([`crate::franchise::Stop::short`]) of series `i`
    /// in its franchise, if it is one stop; a series over several entry
    /// folders has none (its title says more than its first arc's name).
    pub fn short_series_label(&self, i: usize) -> Option<&str> {
        let (f, _) = self.franchise_pos(i)?;
        let mut stops = f.stops_of(i);
        let (first, more) = (stops.next()?, stops.next());
        more.is_none().then(|| f.stops[first].short.as_str())
    }

    /// The series (indices) before and after series `i` in its franchise's
    /// watch order (see [`Franchise::neighbours`]).
    pub fn franchise_neighbours(&self, i: usize) -> (Option<usize>, Option<usize>) {
        self.franchise_pos(i).map_or((None, None), |(f, _)| f.neighbours(i))
    }

    /// The franchise stop whose entry folder is called `name` (compared as
    /// series keys), if exactly one is.
    pub fn stop_named(&self, name: &str) -> Option<(&Franchise, usize)> {
        let key = crate::identity::series_key(name);
        let mut hits = self.franchises.iter().flat_map(|f| {
            let key = &key;
            f.stops.iter().enumerate().filter_map(move |(n, stop)| {
                let part = stop.folder.as_ref()?;
                (crate::identity::series_key(&part.name) == *key).then_some((f, n))
            })
        });
        let hit = hits.next()?;
        hits.next().is_none().then_some(hit)
    }

    /// `Monogatari Series 13/16`: the franchise of `key` and where you are in it.
    pub fn franchise_place(&self, key: &str) -> Option<String> {
        let (f, pos) = self.franchise_of(key)?;
        Some(format!("{name} {at}/{stops}", name = f.name, at = pos + 1, stops = f.stops.len()))
    }

    /// `Title 07,08` for messages about played items.
    pub fn label(&self, series: &str, items: &[ItemKey]) -> String {
        let title = self.title_of(series);
        let eps: Vec<String> = items.iter().map(ItemKey::describe).collect();
        format!("{title} {eps}", eps = eps.join(","))
    }

    /// The title of the series `key`, or the key itself without one.
    pub fn title_of<'a>(&'a self, key: &'a str) -> &'a str {
        self.get(key).map_or(key, |s| s.title.as_str())
    }

    /// Canonical key for a series key (possibly merged away, or a legacy spelling).
    pub fn resolve<'a>(&'a self, key: &'a str) -> &'a str {
        crate::events::rekey(&self.canonical, key)
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
        let mut by_base: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, s) in self.series.iter().enumerate() {
            by_base.entry(crate::identity::base_key(&s.key)).or_default().push(i);
        }
        let mut entries: Vec<InboxEntry> = self
            .series
            .iter()
            .enumerate()
            .filter(|(_, s)| Self::in_inbox(s, &ongoing))
            .map(|(i, s)| InboxEntry { index: i, hint: self.inbox_hint(i, s, &by_anilist, &by_base) })
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

    /// `by_base` holds the indices of the series with each base key, in order
    /// (made once per inbox).
    fn inbox_hint(
        &self,
        i: usize,
        s: &Series,
        by_anilist: &HashMap<u64, usize>,
        by_base: &HashMap<&str, Vec<usize>>,
    ) -> Option<Hint> {
        // A prequel you're watching (AniList), or the same base title with a
        // season marker (`kusuriya no hitorigoto s3` after `kusuriya no hitorigoto`).
        let prequel = s.prequels.iter().flatten().filter_map(|id| by_anilist.get(id)).copied();
        let base = crate::identity::base_key(&s.key);
        // Only a key with a season marker (base differs) looks for its other seasons.
        let others = if base == s.key { None } else { by_base.get(base) };
        let same_title = others.into_iter().flatten().copied();
        if let Some(j) = prequel.chain(same_title).find(|&j| j != i && self.series[j].engaged()) {
            return Some(Hint::NewSeasonOf(j));
        }
        // `pocket monsters 2023 pokemon go` belongs to `pocket monsters 2023`:
        // the longest key of another series that the key continues with a word.
        s.key.rmatch_indices(' ').find_map(|(at, _)| {
            let prefix = &s.key[..at];
            self.index_of(prefix).filter(|&j| j != i && self.series[j].key == prefix).map(Hint::PartOf)
        })
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
        let i = self.files.binary_search_by(|f| path_order(&f.file.path, path)).ok()?;
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
    // Almost always: no key has a legacy spelling.
    if series.clone().all(|(_, legacy)| legacy.is_none()) {
        return HashMap::new();
    }
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

/// The order of paths as [`Path`] compares them (component by component),
/// for the normalized paths a scan finds, but without splitting them into
/// components: bytes, with the separator first. Sorting a library's files
/// by `Path::cmp` takes a third of a build.
fn path_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    use std::os::unix::ffi::OsStrExt;
    fn bytes(p: &Path) -> impl Iterator<Item = u8> + '_ {
        p.as_os_str().as_bytes().iter().map(|&c| if c == b'/' { 0 } else { c })
    }
    bytes(a).cmp(bytes(b))
}

/// Title and merged-in names, plus the key when it differs from the title's own.
fn search_text(title: &str, key: &str, aliases: &[String], others: &[&str]) -> String {
    let mut text = title.to_string();
    if crate::identity::series_key(title) != key {
        text.push(' ');
        text.push_str(key);
    }
    for alias in aliases {
        text.push(' ');
        text.push_str(alias);
    }
    for name in others {
        text.push(' ');
        text.push_str(name);
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

    /// The cheap path order is `Path`'s, for the paths a scan finds.
    #[test]
    fn path_order_is_paths_order() {
        let paths =
            ["/m/Show/x.mkv", "/m/Show 2/x.mkv", "/m/Show-2/a.mkv", "/m/Show/a/b.mkv", "/m/Showa.mkv", "/m/S", "/m"];
        for a in paths {
            for b in paths {
                assert_eq!(path_order(Path::new(a), Path::new(b)), Path::new(a).cmp(Path::new(b)), "{a} vs {b}");
            }
        }
    }
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
        for extras in [false, true] {
            let listed = |missing| s.ordered_items(extras, missing).len();
            assert_eq!(s.missing_count(extras), listed(true) - listed(false));
        }

        // Nothing on disk any more: hiding it all would leave an empty list.
        let gone: Vec<FileRow> = files.iter().map(|f| FileRow { present: false, ..f.clone() }).collect();
        let lib = build(&gone, &events, &[]);
        let s = lib.get("show").unwrap();
        assert_eq!(s.ordered_items(false, false).len(), 5);
        assert_eq!(s.missing_count(false), 0);
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
            assert_eq!(s.ordered_items(false, false).len(), 3);
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

    /// History under a key no file has any more (the files are classified
    /// into other series now) follows the file names it recorded.
    #[test]
    fn history_of_a_vanished_key_follows_its_files() {
        let owari = "[G] Owarimonogatari S1 - 13 [ABCD0013].mkv";
        let second = "[G] Monogatari Series Second Season - 01 [ABCD0005].mkv";
        let files = [
            row("anime", &format!("Monogatari (BD)/13 - Owarimonogatari S1/{owari}"), true, 1),
            row("anime", &format!("Monogatari (BD)/05 - Nekomonogatari (White)/{second}"), true, 1),
            row("anime", "Other/Other - 01.mkv", true, 1),
        ];
        let watched = |ts, series: &str, n, file: &str| {
            ev(ts, EventBody::Watched { series: series.into(), item: ep(n), file: Some(file.into()) })
        };
        let state = |lib: &Library, key: &str, n| lib.get(key).and_then(|s| s.item(&ep(n))).map(|i| i.state);
        let events = [
            // Recorded when the whole folder was one series `monogatari`.
            watched(5, "monogatari", 13, owari),
            watched(6, "monogatari", 1, second),
            ev(7, EventBody::status("monogatari", SeriesStatus::Following, None)),
            // A series that still has files keeps what it recorded.
            watched(8, "other", 2, owari),
        ];
        let lib = build(&files, &events, &[]);
        assert_eq!(state(&lib, "owarimonogatari", 13), Some(WatchState::Watched { at: 5 }));
        assert_eq!(state(&lib, "monogatari series second season", 1), Some(WatchState::Watched { at: 6 }));
        assert_eq!(state(&lib, "other", 2), Some(WatchState::Watched { at: 8 }));
        assert!(lib.get("monogatari").is_none(), "no row is left for the vanished key");

        // An event without a file name has nothing to follow: it stays.
        let bare = ev(9, EventBody::watched("monogatari", ep(3)));
        let lib = build(&files, &[events[0].clone(), bare], &[]);
        assert_eq!(state(&lib, "monogatari", 3), Some(WatchState::Watched { at: 9 }));
        assert_eq!(state(&lib, "owarimonogatari", 13), Some(WatchState::Watched { at: 5 }));

        // Only files the key was given to before count: a file of the same
        // name in another series folder never was `monogatari`'s.
        let twice = [files[0].clone(), row("anime", &format!("Elsewhere/{owari}"), true, 1)];
        let lib = build(&twice, &events[..1], &[]);
        assert_eq!(state(&lib, "owarimonogatari", 13), Some(WatchState::Watched { at: 5 }));
        assert_eq!(state(&lib, "elsewhere", 13), Some(WatchState::Unwatched));
        let lib = build(&twice[1..], &events[..1], &[]);
        assert_eq!(state(&lib, "monogatari", 13), Some(WatchState::Watched { at: 5 }));
    }

    /// An episode outside the franchise folder (still in the downloads) counts
    /// with its series' entry: the entry isn't watched until it is.
    #[test]
    fn episodes_outside_the_entry_folder_belong_to_its_stop() {
        let files = [
            row("anime", "Mono/01 - Alpha/[G] Alpha - 01.mkv", true, 1),
            row("anime", "Mono/02 - Beta/[G] Beta - 01.mkv", true, 1),
            row("dl", "[G] Alpha - 02.mkv", true, 1),
        ];
        let lib = build(&files, &[ev(1, EventBody::watched("alpha", ep(1)))], &[]);
        let (f, pos) = lib.franchise_of("alpha").unwrap();
        assert_eq!((f.stops.len(), pos), (2, 0));
        assert_eq!(f.stops[0].items, [ep(1), ep(2)]);
        assert!(!f.stops[0].watched, "episode 2 is still to watch");
    }

    /// A series' franchise folder is the one its files on disk are in, not
    /// one a file was deleted from; and an entry folder isn't split off again.
    #[test]
    fn the_franchise_folder_is_the_one_on_disk() {
        let files = [
            row("anime", "Old/01 - Alpha/[G] Alpha - 01.mkv", false, 1),
            row("anime", "Mono/05 - Alpha/[G] Alpha - 02.mkv", true, 1),
            row("anime", "Mono/06 - Beta/[G] Beta - 01.mkv", true, 1),
        ];
        let lib = build(&files, &[], &[]);
        let alpha = lib.get("alpha").unwrap();
        assert_eq!(alpha.folder, Some(Folder { root: "anime".into(), name: "Mono".into(), first: 5 }));
        assert_eq!(lib.franchise_of("alpha").unwrap().0.name, "Mono");
        assert!(lib.splittable(&cfg(), alpha).is_empty());
    }

    /// A folder split off its series is a series of its own, and takes the
    /// history of its files along; an unsplit puts both back.
    #[test]
    fn a_split_folder_is_its_own_series() {
        let mini = "[C] Carole & Tuesday Mini Series - 03 [1080p].mkv";
        let files = [
            row("anime", "Carole & Tuesday/Carole & Tuesday - 03.mkv", true, 1),
            row("anime", &format!("Carole & Tuesday/Carole & Tuesday Mini Series/{mini}"), true, 1),
        ];
        let dir = "Carole & Tuesday/Carole & Tuesday Mini Series";
        let split = ev(2, EventBody::Split { root: "anime".into(), dir: dir.into() });
        let watched =
            ev(3, EventBody::Watched { series: "carole and tuesday".into(), item: ep(3), file: Some(mini.into()) });
        let status = ev(4, EventBody::status("carole and tuesday", SeriesStatus::Following, None));
        let state = |lib: &Library, key: &str| lib.get(key).and_then(|s| s.item(&ep(3))).map(|i| i.state);

        let lib = build(&files, &[watched.clone(), status.clone()], &[]);
        assert_eq!(lib.series.len(), 1, "one series, its episode 3 from two files");
        let carole = lib.get("carole and tuesday").unwrap();
        assert_eq!(lib.splittable(&cfg(), carole), [("anime".to_string(), dir.to_string())]);
        // A download folder has no series folders to split off.
        let dl = build(&[row("dl", "Show/Sub/[G] Show - 01.mkv", true, 1)], &[], &[]);
        assert!(dl.splittable(&cfg(), &dl.series[0]).is_empty());
        assert_eq!(state(&lib, "carole and tuesday"), Some(WatchState::Watched { at: 3 }));

        let lib = build(&files, &[split.clone(), watched.clone(), status.clone()], &[]);
        assert_eq!(lib.splits, vec![("anime".to_string(), dir.to_string())]);
        assert_eq!(state(&lib, "carole and tuesday mini series"), Some(WatchState::Watched { at: 3 }));
        let main = lib.get("carole and tuesday").unwrap();
        assert_eq!(
            (main.item(&ep(3)).map(|i| i.state), main.status),
            (Some(WatchState::Unwatched), SeriesStatus::Following)
        );
        assert_eq!(
            lib.file(Path::new(&format!("/anime/{dir}/{mini}"))).unwrap().series,
            "carole and tuesday mini series"
        );

        let unsplit = ev(5, EventBody::Unsplit { root: "anime".into(), dir: dir.into() });
        let lib = build(&files, &[split.clone(), watched, status.clone(), unsplit.clone()], &[]);
        assert_eq!((lib.series.len(), lib.splits.len()), (1, 0));
        assert_eq!(state(&lib, "carole and tuesday"), Some(WatchState::Watched { at: 3 }));

        // Watched while split (under the split-off key): back with the show
        // once the split is undone, and no row is left for the split-off key.
        let during = ev(
            4,
            EventBody::Watched {
                series: "carole and tuesday mini series".into(),
                item: ep(3),
                file: Some(mini.into()),
            },
        );
        let lib = build(&files, &[split, during, status, unsplit], &[]);
        assert_eq!(lib.series.len(), 1);
        assert_eq!(state(&lib, "carole and tuesday"), Some(WatchState::Watched { at: 4 }));
    }

    #[test]
    fn a_relabelled_file_keeps_its_watch_state() {
        let name = "[GroupA] Grand Blue (Creditless ED 01) [BDRip 1080p][ABCD0001].mkv";
        let files = [
            row("anime", &format!("Grand Blue/Extras/{name}"), true, 1),
            row("anime", "Grand Blue/Grand Blue - 01.mkv", true, 1),
        ];
        // The label an older version gave the file: its whole name.
        let old =
            ItemKey { kind: ItemKind::Extra, ep: None, label: crate::identity::label_key(crate::parse::stem(name)) };
        let new = ItemKey { label: "creditless ed 01".into(), ..old };
        let file = || Some(name.to_string());
        let watched = |ts, series: &str, item: &ItemKey| {
            ev(ts, EventBody::Watched { series: series.into(), item: item.clone(), file: file() })
        };
        let states = |events: &[Event]| -> Vec<(ItemKey, WatchState)> {
            let lib = build(&files, events, &[]);
            lib.get("grand blue").unwrap().items.iter().map(|i| (i.key.clone(), i.state)).collect()
        };
        let with_new = |state| [(ep(1), WatchState::Unwatched), (new.clone(), state)];

        // The state moves to the item the file gives now; no item is left under the old label.
        assert_eq!(states(&[watched(5, "grand blue", &old)]), with_new(WatchState::Watched { at: 5 }));
        // Events under either label replay as one item, in log order: the
        // newest wins, and playing a watched item again doesn't un-watch it.
        let unwatched = ev(7, EventBody::Unwatched { series: "grand blue".into(), item: new.clone(), file: file() });
        assert_eq!(states(&[watched(5, "grand blue", &old), unwatched]), with_new(WatchState::Unwatched));
        let progress =
            EventBody::Progress { series: "grand blue".into(), item: new.clone(), pos: 1.0, dur: None, file: file() };
        assert_eq!(states(&[watched(5, "grand blue", &old), ev(7, progress)]), with_new(WatchState::Watched { at: 5 }));
        // Nor is an item of another kind (an episode watched elsewhere as a
        // file of this name), or one that a file still gives.
        assert_eq!(
            states(&[watched(5, "grand blue", &ep(2))]),
            [
                (ep(1), WatchState::Unwatched),
                (ep(2), WatchState::Watched { at: 5 }),
                (new.clone(), WatchState::Unwatched)
            ]
        );
        assert_eq!(
            states(&[watched(5, "grand blue", &ep(1))]),
            [(ep(1), WatchState::Watched { at: 5 }), (new.clone(), WatchState::Unwatched)]
        );
        // Nor is history in another series with the same file name.
        let lib = build(&files, &[watched(5, "other show", &old)], &[]);
        assert_eq!(lib.get("other show").unwrap().items[0].key, old);
        assert!(lib.get("grand blue").unwrap().items.iter().all(|i| i.state == WatchState::Unwatched));
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
        let attempt = MatchAttempt { series: "zzz".into(), db_version: "v1".into(), at: 1, result: MatchResult::None };
        let index =
            index(&files, MetaCache { attempts: vec![attempt], db_version: "v1".into(), ..MetaCache::default() });
        let lib = Library::build(&cfg(), &[], &index);
        assert!(lib.get("zzz").unwrap().last_match.is_some());
        assert!(lib.get("yyy").unwrap().last_match.is_none());
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

    /// Every title AniList has for a series is searchable, each once.
    #[test]
    fn search_text_has_the_anilist_titles() {
        let files = [row("dl", "[A] Kusuriya no Hitorigoto - 01.mkv", true, 1)];
        let meta = SeriesMeta {
            series: "kusuriya no hitorigoto".into(),
            anilist: Some(5),
            title: Some("Kusuriya no Hitorigoto".into()),
            titles: Some(vec![
                "Kusuriya no Hitorigoto".into(),
                "The Apothecary Diaries".into(),
                "薬屋のひとりごと".into(),
            ]),
            ..SeriesMeta::default()
        };
        let lib = build(&files, &[], &[meta]);
        let s = lib.get("kusuriya no hitorigoto").unwrap();
        assert_eq!(s.search_text(), "Kusuriya no Hitorigoto The Apothecary Diaries 薬屋のひとりごと");
        assert_eq!(s.other_names(), ["The Apothecary Diaries", "薬屋のひとりごと"]);
    }

    /// An AniList title that is already the key or a merged-away name isn't
    /// added again.
    #[test]
    fn search_text_has_each_name_once() {
        let files =
            [row("dl", "[A] Kusuriya - 01.mkv", true, 1), row("dl", "[A] Apothecary Diaries - 01.mkv", true, 1)];
        let events = vec![
            ev(1, EventBody::Title { series: "kusuriya".into(), title: "Kusuriya no Hitorigoto".into() }),
            ev(2, EventBody::Alias { from: "apothecary diaries".into(), to: "kusuriya".into() }),
        ];
        let meta = SeriesMeta {
            series: "kusuriya".into(),
            anilist: Some(5),
            titles: Some(vec!["Kusuriya".into(), "Apothecary Diaries".into(), "薬屋のひとりごと".into()]),
            ..SeriesMeta::default()
        };
        let lib = build(&files, &events, &[meta]);
        let s = lib.get("kusuriya").unwrap();
        assert_eq!(s.search_text(), "Kusuriya no Hitorigoto kusuriya apothecary diaries 薬屋のひとりごと");
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
        let attempt =
            MatchAttempt { series: "ハカ".into(), db_version: "v1".into(), at: 1, result: MatchResult::None };
        let cache = MetaCache { attempts: vec![attempt], db_version: "v1".into(), ..rows(&[meta("ハカ", 1)]) };
        let mut index = index(&baka_files(), cache);
        let s = |index: &Index| Library::build(&cfg(), &[], index).get("バカ").unwrap().clone();
        let old = s(&index);
        assert_eq!(old.meta.as_ref().map(|m| (m.series.as_str(), m.anilist)), Some(("バカ", Some(1))), "re-keyed");
        assert_eq!(old.anilist_id(), Some(1));
        assert!(old.last_match.is_none(), "a \"no match\" for the old key is not one for the new key");
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
