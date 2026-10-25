//! TUI state and input handling (rendering lives in `ui.rs`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use super::query::SeriesQuery;
use crate::app::{Ctx, Index, QueueEntry, fuzzy_rank, queue_new};
use crate::events::{CachedLog, EventBody, Recorded, now};
use crate::folders::{Dir, Entry, Folders};
use crate::index::db::Changes;
use crate::library::{Hint, InboxEntry, Item, Library, Series};
use crate::model::{ItemKey, ItemKind, SeriesStatus};
use crate::mpv::{Outcome, PlayerEvent};

/// Top-level screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Followed series with new episodes.
    UpNext,
    /// New untracked shows in ongoing roots, to follow or skip.
    Inbox,
    /// All series.
    Series,
    /// Files in ongoing roots, newest first.
    Files,
    /// The play queue.
    Queue,
    /// One series' episodes.
    Detail,
}

impl View {
    /// Tabs in display order (Detail is reached via Enter).
    pub const TABS: [Self; 5] = [Self::UpNext, Self::Inbox, Self::Series, Self::Files, Self::Queue];

    /// Every view, tabs first.
    const ALL: [Self; 6] = [Self::UpNext, Self::Inbox, Self::Series, Self::Files, Self::Queue, Self::Detail];

    /// The next (or previous) tab, wrapping.
    fn cycle(self, forward: bool) -> Self {
        let n = Self::TABS.len();
        let i = Self::TABS.iter().position(|v| *v == self).unwrap_or(0);
        Self::TABS[if forward { (i + 1) % n } else { (i + n - 1) % n }]
    }

    /// Tab label.
    pub fn title(self) -> &'static str {
        match self {
            Self::UpNext => "Up next",
            Self::Inbox => "Inbox",
            Self::Series | Self::Detail => "Series",
            Self::Files => "Files",
            Self::Queue => "Queue",
        }
    }
}

/// Which series the Series view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesFilter {
    /// Everything with files on disk, except dropped and skipped series.
    Active,
    /// Everything, including history-only series.
    All,
    /// One status.
    Only(SeriesStatus),
}

impl SeriesFilter {
    const CYCLE: [Self; 8] = [
        Self::Active,
        Self::Only(SeriesStatus::Following),
        Self::Only(SeriesStatus::Paused),
        Self::Only(SeriesStatus::Untracked),
        Self::Only(SeriesStatus::Completed),
        Self::Only(SeriesStatus::Dropped),
        Self::Only(SeriesStatus::Skipped),
        Self::All,
    ];

    /// Label for the title bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "on disk",
            Self::All => "all",
            Self::Only(s) => s.as_str(),
        }
    }

    fn accepts(self, s: &Series) -> bool {
        match self {
            Self::Active => s.present() && !matches!(s.status, SeriesStatus::Dropped | SeriesStatus::Skipped),
            Self::All => true,
            Self::Only(st) => s.status == st,
        }
    }

    fn next(self) -> Self {
        let i = Self::CYCLE.iter().position(|f| *f == self).unwrap_or(0);
        Self::CYCLE[(i + 1) % Self::CYCLE.len()]
    }
}

/// What the Files view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesMode {
    /// Files in the download folders, newest first.
    Recent,
    /// Every root as folders, one folder at a time.
    Tree,
}

/// What a text input about a series is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputPurpose {
    /// Note for a status change to this status.
    Note(SeriesStatus),
    /// New display title.
    Rename,
    /// `AniList` id to link.
    Link,
}

impl InputPurpose {
    /// What the input box asks for.
    pub fn prompt(self) -> String {
        match self {
            Self::Note(status) => format!("{status} — note (optional)"),
            Self::Rename => "title".into(),
            Self::Link => "AniList id (empty to unlink)".into(),
        }
    }
}

/// Modal overlays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Popup {
    /// Key help, scrolled down this many lines (clamped when drawn).
    Help {
        /// First line shown.
        scroll: u16,
        /// It all fit on the screen when last drawn: then any key closes it.
        fits: bool,
    },
    /// Choose a status for a series.
    Status {
        /// Series key.
        series: String,
        /// Highlighted row.
        idx: usize,
    },
    /// Pick a series to merge into.
    Merge(MergePicker),
    /// The Series view's search box: edits [`App::query`] as it is typed.
    Search,
    /// Single-line text input about a series.
    Input {
        /// Series key.
        series: String,
        /// What it's for.
        purpose: InputPurpose,
        /// Current text.
        text: String,
    },
    /// Quit anipv? (Warns that tracking is lost while mpv is still running.)
    ConfirmQuit,
    /// Split off every name merged into a series (`U`)?
    Unmerge {
        /// Series key.
        series: String,
        /// The names merged into it.
        names: Vec<String>,
    },
    /// Catch up to a series (see [`crate::app::catch_up`])?
    CatchUp {
        /// Series key.
        series: String,
        /// What it would do.
        prompt: String,
    },
}

/// State of the merge picker: the filter text, the candidates it matches
/// (computed when the text changes, not on every frame or key) and the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePicker {
    /// Series being merged away.
    pub from: String,
    /// Filter text.
    pub query: String,
    /// Keys of the candidates for `query`, best match first.
    pub targets: Vec<String>,
    /// Highlighted row and scroll offset.
    pub state: TableState,
}

impl MergePicker {
    fn new(app: &App, from: &str) -> Self {
        let from = from.to_string();
        let mut picker = Self { from, query: String::new(), targets: Vec::new(), state: TableState::default() };
        picker.refresh(app, None);
        picker
    }

    /// Recompute the candidates, with the cursor on `keep` if it is still
    /// listed, otherwise back on the top row.
    fn refresh(&mut self, app: &App, keep: Option<&str>) {
        self.targets = app.merge_targets(&self.from, &self.query).iter().map(|s| s.key.clone()).collect();
        let kept = keep.and_then(|k| self.targets.iter().position(|t| t == k));
        if kept.is_none() {
            *self.state.offset_mut() = 0;
        }
        self.state.select(Some(kept.unwrap_or(0)));
        clamp_cursor(&mut self.state, self.targets.len());
    }

    /// Move the cursor by `delta` rows, staying inside the list.
    fn step(&mut self, delta: isize) {
        step_cursor(&mut self.state, self.targets.len(), delta);
    }

    /// Key of the highlighted candidate.
    fn selected(&self) -> Option<&str> {
        self.state.selected().and_then(|i| self.targets.get(i)).map(String::as_str)
    }
}

/// `ix` (indices of series sorted by title) with the entries of each
/// franchise together, in watch order, where the franchise's name sorts; with
/// `fold`, only the entry you're at (the first not completed, dropped or
/// skipped, else the last) of each, and any other you follow.
fn franchise_order(lib: &Library, ix: impl Iterator<Item = usize>, fold: bool) -> Vec<usize> {
    let mut by_franchise: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut rows: Vec<(String, Option<usize>, usize)> = Vec::new();
    for i in ix {
        let s = &lib.series[i];
        match (lib.franchise_index(i), lib.franchise_pos(i)) {
            (Some(f_ix), Some((_, pos))) => by_franchise.entry(f_ix).or_default().push(pos),
            _ => rows.push((s.title.to_lowercase(), None, i)),
        }
    }
    for (f_ix, mut positions) in by_franchise {
        let f = &lib.franchises[f_ix];
        positions.sort_unstable();
        if fold {
            let done = |p: &usize| {
                matches!(
                    lib.series[f.stops[*p].series].status,
                    SeriesStatus::Completed | SeriesStatus::Dropped | SeriesStatus::Skipped
                )
            };
            let at = positions.iter().find(|p| !done(p)).or_else(|| positions.last()).copied();
            let following = |p: &usize| lib.series[f.stops[*p].series].status == SeriesStatus::Following;
            positions.retain(|p| Some(*p) == at || following(p));
        }
        rows.extend(positions.into_iter().map(|p| (f.name.to_lowercase(), Some(p), f.stops[p].series)));
    }
    rows.sort();
    rows.into_iter().map(|(_, _, i)| i).collect()
}

/// A row of the Detail list: an item of the series shown, or a link to the
/// season before (`▲`) or after (`▼`) it in its franchise.
#[derive(Debug, Clone, Copy)]
pub enum DetailRow<'a> {
    /// An item of the series shown.
    Item(&'a Item),
    /// The season before (or, `next`, after) it.
    Season {
        /// That season.
        series: &'a Series,
        /// Its index (see [`Library::entry`]).
        index: usize,
        /// After, rather than before.
        next: bool,
    },
    /// An entry related to it on `AniList` (a side story, a film…).
    Related {
        /// That entry.
        series: &'a Series,
        /// Its index (see [`Library::entry`]).
        index: usize,
        /// How it relates (`side_story`, see
        /// [`crate::meta::anilist::Relations::related`]).
        relation: &'a str,
    },
    /// A divider naming the rows after it.
    Divider(Divider),
}

/// What a Detail divider row introduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Divider {
    /// The related entries.
    Related,
    /// The episodes of part `n` of the series' split (see [`crate::split`]).
    Part(usize),
}

impl<'a> DetailRow<'a> {
    /// The item, unless this is a link to another entry or a divider.
    pub fn item(self) -> Option<&'a Item> {
        match self {
            Self::Item(it) => Some(it),
            Self::Season { .. } | Self::Related { .. } | Self::Divider(_) => None,
        }
    }
}

/// What a Detail row is: an index into the series' items, or the index (see
/// [`Library::entry`]) of the season it links to (`next`: after, rather
/// than before) or of a related entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailAt {
    Item(usize),
    Season { series: usize, next: bool },
    Related { series: usize, n: usize },
    Divider(Divider),
}

impl DetailAt {
    /// The index of its item, unless it is a link to another entry.
    fn item_ix(self) -> Option<usize> {
        match self {
            Self::Item(i) => Some(i),
            Self::Season { .. } | Self::Related { .. } | Self::Divider(_) => None,
        }
    }

    /// The entry it links to, if it is a link.
    fn linked(self) -> Option<usize> {
        match self {
            Self::Item(_) | Self::Divider(_) => None,
            Self::Season { series, .. } | Self::Related { series, .. } => Some(series),
        }
    }
}

/// What a list row stands for, to find it again after a reload.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowId {
    /// A series, by key.
    Series(String),
    /// A file (or queue entry), by path.
    File(PathBuf),
    /// An item of the series in Detail.
    Item(ItemKey),
    /// A folder in the Files view's tree.
    Dir(Dir),
    /// A season link in Detail: the series it opens, and which of the two
    /// (both can open the same one: a series split around this one).
    Season {
        /// Series key.
        key: String,
        /// The `▼` link, not the `▲` one.
        next: bool,
    },
    /// A related entry's link in Detail, by the key of the entry it opens.
    Related(String),
    /// A divider in Detail.
    Divider(Divider),
}

/// Live playback info.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NowPlaying {
    /// Display label for the current file.
    pub label: String,
    /// The current file, once it has started playing.
    pub path: Option<PathBuf>,
    /// Position (s).
    pub pos: f64,
    /// Duration (s).
    pub dur: Option<f64>,
    /// Files in this mpv session.
    pub total: usize,
    /// Files finished so far.
    pub done: usize,
    /// mpv is paused.
    pub paused: bool,
    /// Sends keys to mpv, once connected.
    pub remote: Option<crate::mpv::Remote>,
    /// Remote mode: keys go to mpv (see [`App::remote_active`]).
    pub remote_on: bool,
}

impl NowPlaying {
    /// A session of `total` files that has not started playing yet.
    fn starting(total: usize) -> Self {
        Self { label: "starting mpv…".into(), total, ..Self::default() }
    }

    /// Keys can reach mpv: it has connected.
    pub fn connected(&self) -> bool {
        self.remote.is_some()
    }
}

/// Messages from background threads.
#[derive(Debug)]
pub enum Msg {
    /// Scan progress for a root.
    ScanProgress(String, usize),
    /// Scan finished.
    ScanDone(Result<ScanSummary, String>),
    /// mpv activity.
    Player(PlayerEvent),
    /// What a background metadata update is doing now (header text).
    MetaProgress(&'static str),
    /// A background metadata update waits for `AniList`'s rate limit until then.
    AniListWait(Instant),
    /// Metadata sync finished; the number is the link epoch it started under.
    Meta(crate::meta::SyncResult, u64),
    /// Metadata error.
    MetaError(String),
}

/// What a finished scan reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSummary {
    /// Changes per root, for the status line.
    pub text: String,
    /// Roots that could not be scanned (unmounted, say).
    pub offline_roots: Vec<String>,
    /// A root that could be reached failed to scan in full.
    pub failed: bool,
}

/// The metadata update running now.
#[derive(Debug)]
struct MetaRun {
    /// What it was asked to update.
    request: MetaRequest,
    /// What it is doing, for the header.
    doing: &'static str,
    /// Until when it waits for `AniList`'s rate limit.
    wait_until: Option<Instant>,
}

impl MetaRun {
    fn new(request: MetaRequest) -> Self {
        Self { request, doing: "metadata", wait_until: None }
    }
}

/// Opens a URL; fails if no opener could be started.
pub type Opener = Box<dyn FnMut(&str) -> Result<()>>;

/// The whole TUI state.
#[expect(clippy::struct_excessive_bools, reason = "independent view switches")]
pub struct App {
    /// Shared context.
    pub ctx: Ctx,
    /// Current library snapshot.
    pub lib: Arc<Library>,
    /// Active view.
    pub view: View,
    /// View to return to from Detail.
    back: View,
    /// Selections per view.
    pub upnext_state: TableState,
    /// Inbox view selection.
    pub inbox_state: TableState,
    /// Series view selection.
    pub series_state: TableState,
    /// Files view selection (newest first).
    pub files_state: TableState,
    /// What the Files view shows: the newest files, or the folder tree.
    pub files_mode: FilesMode,
    /// The folder tree of the files on disk (see [`crate::folders`]).
    pub folders: Folders,
    /// The folder the tree shows.
    pub tree_at: Dir,
    /// Applied fuzzy query for the tree: it lists matching folders.
    pub tree_query: String,
    /// Tree rows: the listing of `tree_at`, or the folders found.
    tree_rows: Vec<Entry>,
    /// Files view selection in the tree.
    pub tree_state: TableState,
    /// Queue view selection.
    pub queue_state: TableState,
    /// Detail view selection.
    pub detail_state: TableState,
    /// Series filter.
    pub filter: SeriesFilter,
    /// The Series view lists one entry per franchise, the one you're at.
    pub fold_franchises: bool,
    /// Applied fuzzy query for the Series view.
    pub query: String,
    /// Include paused series in Up next.
    pub include_paused: bool,
    /// What the episode list shows besides episodes on disk.
    pub episodes_shown: EpisodeFilter,
    /// Series shown in Detail.
    pub detail_key: Option<String>,
    /// Sections of the series in Detail you collapsed (`true`) or expanded
    /// by hand, over the default (see [`App::collapsed`]); forgotten when
    /// another series is opened.
    folds: HashMap<Divider, bool>,
    /// The item a first Backspace marked to reset (series key, item): a
    /// second resets it, any other key forgets it.
    pub pending_reset: Option<(String, ItemKey)>,
    /// Play queue.
    pub queue: Vec<QueueEntry>,
    /// Open popup.
    pub popup: Option<Popup>,
    /// Recent status line messages, oldest first (see [`App::current_message`]).
    messages: Vec<Message>,
    /// Active scan description.
    pub scanning: Option<String>,
    /// Bumped on every manual link change. Each sync carries the epoch it
    /// started under, so its results for a series relinked since are dropped
    /// instead of overwriting the new link.
    meta_epoch: u64,
    /// Series whose link changed by hand, with the epoch of the change.
    relinked: HashMap<String, u64>,
    /// The metadata update running now.
    meta_running: Option<MetaRun>,
    /// A metadata update requested while one ran: it starts when that ends.
    meta_pending: Option<MetaRequest>,
    /// Playback state while mpv runs.
    pub playing: Option<NowPlaying>,
    /// Set to leave the main loop.
    pub quit: bool,
    /// Files and metadata from the cache, re-read only after scans and metadata updates.
    index: Index,
    /// The event log, re-read only when its files change (or after a scan).
    events: CachedLog,
    /// A reload is due once the pending background messages are handled.
    reload_due: bool,
    /// The offline anime database, loaded once for all metadata updates.
    offline: Arc<crate::meta::offline::OfflineCache>,
    /// Fixed clock for tests/screenshots; `None` uses the real time.
    pub clock: Option<i64>,
    /// Roots the last scan could not reach (unmounted, say); none are known
    /// before the first scan (see [`App::start_scan`]).
    pub offline_roots: Vec<String>,
    // Row caches, rebuilt by `refresh_rows`: indices into `lib.series` /
    // `lib.files`, the inbox entries, and the Up next count.
    upnext_ix: Vec<usize>,
    /// Up next rows with a new episode on disk (the tab's count).
    upnext_ready: usize,
    inbox: Vec<InboxEntry>,
    /// Width of the longest Inbox hint, over every row (so the column does
    /// not jump while scrolling).
    inbox_hint_width: usize,
    series_ix: Vec<usize>,
    files_ix: Vec<usize>,
    /// Detail rows: the season before, the items of the series shown, the
    /// season after.
    detail_ix: Vec<DetailAt>,
    /// When the last scan progress message was taken (they come in faster than they can be drawn).
    last_progress: Option<Instant>,
    /// What the cache held at the last reload, to notice what another process
    /// (`anipv update`) stored since (see [`App::pick_up_changes`]).
    changes: Option<Changes>,
    /// When `changes` was last compared.
    changes_checked: Instant,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Opens a URL in the browser (`o`); tests swap in a recorder.
    pub opener: Opener,
}

impl App {
    /// Create the app and load the library.
    pub fn new(ctx: Ctx) -> Result<Self> {
        // Taken before reading, so what is stored meanwhile counts as new.
        let changes = ctx.db.changes().ok();
        let mut index = ctx.index()?;
        let mut events = ctx.log.load_cached()?;
        // Like every reload: a series finished while anipv was closed (watched
        // elsewhere, metadata updated) is completed right away.
        let (lib, completed) = ctx.library_completed(&mut index, &mut events)?;
        let lib = Arc::new(lib);
        let (tx, rx) = channel();
        let mut app = Self {
            ctx,
            lib,
            view: View::UpNext,
            back: View::UpNext,
            upnext_state: TableState::default().with_selected(0),
            inbox_state: TableState::default().with_selected(0),
            series_state: TableState::default().with_selected(0),
            files_state: TableState::default().with_selected(0),
            files_mode: FilesMode::Recent,
            folders: Folders::default(),
            tree_at: Dir::top(),
            tree_query: String::new(),
            tree_rows: Vec::new(),
            tree_state: TableState::default().with_selected(0),
            queue_state: TableState::default().with_selected(0),
            detail_state: TableState::default().with_selected(0),
            filter: SeriesFilter::Active,
            fold_franchises: true,
            query: String::new(),
            include_paused: false,
            episodes_shown: EpisodeFilter::default(),
            detail_key: None,
            folds: HashMap::new(),
            pending_reset: None,
            queue: Vec::new(),
            popup: None,
            messages: Vec::new(),
            scanning: None,
            meta_epoch: 0,
            relinked: HashMap::new(),
            meta_running: None,
            meta_pending: None,
            playing: None,
            quit: false,
            clock: None,
            index,
            events,
            reload_due: false,
            offline: Arc::default(),
            offline_roots: Vec::new(),
            upnext_ix: Vec::new(),
            upnext_ready: 0,
            inbox: Vec::new(),
            inbox_hint_width: 0,
            series_ix: Vec::new(),
            files_ix: Vec::new(),
            detail_ix: Vec::new(),
            last_progress: None,
            changes,
            changes_checked: Instant::now(),
            tx,
            rx,
            opener: Box::new(open_url),
        };
        app.warn_all();
        if !completed.is_empty() {
            app.say(completed.summary());
        }
        app.refresh_rows();
        if app.upnext_ix.is_empty() {
            app.view = if app.inbox.is_empty() { View::Series } else { View::Inbox };
        }
        Ok(app)
    }

    /// The tab shown as active: Detail belongs to the tab it was opened from.
    pub fn tab(&self) -> View {
        if self.view == View::Detail { self.back } else { self.view }
    }

    /// Current time for display purposes.
    pub fn now(&self) -> i64 {
        self.clock.unwrap_or_else(now)
    }

    /// Show a transient message.
    pub fn say(&mut self, s: impl Into<String>) {
        self.post(Severity::Info, s.into());
    }

    /// Show a transient error: it stays in the status line over later
    /// [`App::say`] messages until it times out.
    pub fn fail(&mut self, s: impl Into<String>) {
        self.post(Severity::Error, s.into());
    }

    fn post(&mut self, severity: Severity, text: String) {
        self.messages.retain(Message::is_fresh);
        self.messages.push(Message { text, severity, at: Instant::now() });
    }

    /// Show the library's warnings (all of them, as one error).
    fn warn_all(&mut self) {
        if !self.lib.warnings.is_empty() {
            let text = self.lib.warnings.join("; ");
            self.fail(text);
        }
    }

    /// The message to show: the newest fresh error, else the newest fresh message.
    pub fn current(&self) -> Option<&Message> {
        self.messages.iter().filter(|m| m.is_fresh()).max_by_key(|m| m.severity)
    }

    /// The text of [`App::current`].
    pub fn current_message(&self) -> Option<&str> {
        self.current().map(|m| m.text.as_str())
    }

    /// Re-read the indexed files from the cache database (after a scan: files
    /// known before keep their classification), then reload.
    pub fn reload_index(&mut self) {
        // Taken first, so what is stored meanwhile counts as new; kept only
        // if the read works, so a failed one is tried again.
        let changes = self.ctx.db.changes().ok();
        match self.ctx.index_from(&self.index) {
            Ok(index) => {
                self.index = index;
                // Files and metadata were both read again.
                self.changes = changes;
            }
            Err(e) => self.fail(format!("reading the index failed: {e:#}")),
        }
        self.reload();
    }

    /// [`App::reload_index`], telling of shows that arrived in the inbox.
    fn reload_index_with_news(&mut self) -> Option<String> {
        let before: HashSet<String> = self.inbox_rows().iter().map(|(s, _)| s.key.clone()).collect();
        self.reload_index();
        let arrived = self.inbox_rows().iter().filter(|(s, _)| !before.contains(&s.key)).count();
        (arrived > 0).then(|| {
            format!("{arrived} new show{plural} in your inbox (press 2)", plural = if arrived == 1 { "" } else { "s" })
        })
    }

    /// Reload what another process (`anipv update`, from a timer) stored in
    /// the cache since the last reload: scans, or else metadata. Not while a
    /// scan or update of this TUI runs, whose own reload follows. True if
    /// anything was reloaded.
    pub fn pick_up_changes(&mut self) -> bool {
        self.changes_checked = Instant::now();
        if self.scanning.is_some() || self.meta_running.is_some() {
            return false;
        }
        let Ok(now) = self.ctx.db.changes() else { return false };
        match &self.changes {
            Some(seen) if seen.files != now.files => {
                match crate::app::offline_roots(&self.ctx.cfg, &self.ctx.db) {
                    Ok(roots) => self.offline_roots = roots,
                    Err(e) => self.fail(format!("reading the index failed: {e:#}")),
                }
                if let Some(news) = self.reload_index_with_news() {
                    self.say(news);
                }
                true
            }
            Some(seen) if seen.meta != now.meta => {
                self.reload_meta();
                true
            }
            Some(_) => false,
            None => {
                self.changes = Some(now);
                false
            }
        }
    }

    /// On start: scan, unless every root was looked at lately (see
    /// [`crate::app::index_fresh`]), by `anipv update` say.
    pub fn scan_unless_fresh(&mut self) {
        // The clock scans are recorded by, not a test or demo clock.
        let now = now();
        match crate::app::index_fresh(&self.ctx.cfg, &self.ctx.db, now) {
            Ok(Some(fresh)) => {
                self.offline_roots = fresh.offline_roots;
                let when = match crate::fmt::ago(fresh.oldest, now).as_str() {
                    "now" => "just now".to_string(),
                    ago => format!("{ago} ago"),
                };
                self.say(format!("index scanned {when} · r rescans"));
            }
            Ok(None) | Err(_) => self.start_scan(),
        }
    }

    /// Re-read only the cached metadata (the files are unchanged), then reload.
    fn reload_meta(&mut self) {
        let changes = self.ctx.db.changes();
        match self.ctx.meta_cache() {
            Ok(meta) => {
                self.index.meta = meta;
                // Only metadata was read again: a scan stored elsewhere stays new.
                if let (Some(seen), Ok(now)) = (&mut self.changes, changes) {
                    seen.meta = now.meta;
                }
            }
            Err(e) => self.fail(format!("reading the cached metadata failed: {e:#}")),
        }
        self.reload();
    }

    /// Replay the event log (read again only if its files changed) and
    /// rebuild the library, keeping selections on the same series where possible.
    pub fn reload(&mut self) {
        self.reload_due = false;
        let keep = self.selections();
        if let Err(e) = self.ctx.log.refresh(&mut self.events) {
            self.fail(format!("reading the event log failed: {e:#}"));
        }
        match self.ctx.library_completed(&mut self.index, &mut self.events) {
            Ok((lib, done)) => {
                // Warnings that stay (a read-only events folder, say) show when
                // they appear, not on every reload, where they would hide what
                // each action reports.
                let changed = lib.warnings != self.lib.warnings;
                self.lib = Arc::new(lib);
                if changed {
                    self.warn_all();
                }
                if !done.is_empty() {
                    self.say(done.summary());
                }
            }
            Err(e) => self.fail(format!("reload failed: {e:#}")),
        }
        self.resolve_held_keys();
        self.refresh_rows();
        self.restore_selections(&keep);
        // The merge picker lists series of the old library: keep its cursor on
        // the same target. Other popups stay open as they are.
        match self.popup.take() {
            Some(Popup::Merge(mut picker)) => {
                let keep = picker.selected().map(str::to_owned);
                picker.refresh(self, keep.as_deref());
                self.popup = Some(Popup::Merge(picker));
            }
            other => self.popup = other,
        }
        debug_assert!(
            self.lib.series.iter().all(|s| s.items.iter().all(|i| i.key.kind.is_known())),
            "items of unknown kinds never reach the library"
        );
    }

    /// Number of rows in `view`.
    pub fn rows_len(&self, view: View) -> usize {
        match view {
            View::UpNext => self.upnext_ix.len(),
            View::Inbox => self.inbox.len(),
            View::Series => self.series_ix.len(),
            View::Files if self.files_mode == FilesMode::Tree => self.tree_rows.len(),
            View::Files => self.files_ix.len(),
            View::Queue => self.queue.len(),
            View::Detail => self.detail_ix.len(),
        }
    }

    fn state_mut(&mut self, view: View) -> &mut TableState {
        match view {
            View::UpNext => &mut self.upnext_state,
            View::Inbox => &mut self.inbox_state,
            View::Series => &mut self.series_state,
            View::Files if self.files_mode == FilesMode::Tree => &mut self.tree_state,
            View::Files => &mut self.files_state,
            View::Queue => &mut self.queue_state,
            View::Detail => &mut self.detail_state,
        }
    }

    fn state(&self, view: View) -> &TableState {
        match view {
            View::UpNext => &self.upnext_state,
            View::Inbox => &self.inbox_state,
            View::Series => &self.series_state,
            View::Files if self.files_mode == FilesMode::Tree => &self.tree_state,
            View::Files => &self.files_state,
            View::Queue => &self.queue_state,
            View::Detail => &self.detail_state,
        }
    }

    /// The row under each view's cursor, to put the cursors back on after the
    /// lists change.
    fn selections(&self) -> Vec<(View, RowId)> {
        View::ALL.iter().filter_map(|&v| Some((v, self.selected_id(v)?))).collect()
    }

    /// Put each cursor back on the row it was on, where that row is still listed.
    fn restore_selections(&mut self, keep: &[(View, RowId)]) {
        for (view, id) in keep {
            self.select_id(*view, id);
        }
    }

    fn clamp_selections(&mut self) {
        for view in View::ALL {
            let len = self.rows_len(view);
            clamp_cursor(self.state_mut(view), len);
        }
        // The Files view's other mode keeps its cursor too.
        let (len, state) = match self.files_mode {
            FilesMode::Tree => (self.files_ix.len(), &mut self.files_state),
            FilesMode::Recent => (self.tree_rows.len(), &mut self.tree_state),
        };
        clamp_cursor(state, len);
    }

    /// What the highlighted row of `view` stands for.
    fn selected_id(&self, view: View) -> Option<RowId> {
        let i = self.state(view).selected()?;
        let series = |ix: usize| RowId::Series(self.lib.series[ix].key.clone());
        Some(match view {
            View::UpNext => series(*self.upnext_ix.get(i)?),
            View::Inbox => series(self.inbox.get(i)?.index),
            View::Series => series(*self.series_ix.get(i)?),
            View::Files if self.files_mode == FilesMode::Tree => match self.tree_rows.get(i)? {
                Entry::Folder(dir) => RowId::Dir(dir.clone()),
                Entry::File(f) => RowId::File(self.lib.files.get(*f)?.file.path.clone()),
            },
            View::Files => RowId::File(self.lib.files[*self.files_ix.get(i)?].file.path.clone()),
            View::Queue => RowId::File(self.queue.get(i)?.file.clone()),
            View::Detail => match *self.detail_ix.get(i)? {
                DetailAt::Item(n) => RowId::Item(self.detail_series()?.items.get(n)?.key.clone()),
                DetailAt::Season { series, next } => {
                    RowId::Season { key: self.lib.get_entry(series)?.key.clone(), next }
                }
                DetailAt::Related { series, .. } => RowId::Related(self.lib.get_entry(series)?.key.clone()),
                DetailAt::Divider(d) => RowId::Divider(d),
            },
        })
    }

    /// Put the cursor of `view` on the row for `id`, if it is listed.
    fn select_id(&mut self, view: View, id: &RowId) {
        let lib = &self.lib;
        // A key held from before a merge finds the series it was merged into.
        let series_at = |ix: &[usize], k: &str| {
            let at = lib.index_of(k)?;
            ix.iter().position(|&i| i == at)
        };
        let found = match (view, id) {
            (View::UpNext, RowId::Series(k)) => series_at(&self.upnext_ix, k),
            (View::Inbox, RowId::Series(k)) => {
                let at = lib.index_of(k);
                self.inbox.iter().position(|e| Some(e.index) == at)
            }
            (View::Series, RowId::Series(k)) => series_at(&self.series_ix, k),
            (View::Files, id) if self.files_mode == FilesMode::Tree => {
                self.tree_rows.iter().position(|e| match (e, id) {
                    (Entry::Folder(dir), RowId::Dir(d)) => dir == d,
                    (Entry::File(i), RowId::File(p)) => lib.files.get(*i).is_some_and(|f| f.file.path == *p),
                    _ => false,
                })
            }
            (View::Files, RowId::File(p)) => self.files_ix.iter().position(|&i| lib.files[i].file.path == *p),
            (View::Queue, RowId::File(p)) => self.queue_index_of(p),
            (View::Detail, id) => self.detail_row_of(id),
            _ => None,
        };
        if found.is_some() {
            self.state_mut(view).select(found);
        }
    }

    /// Recompute the cached row lists (after a reload, or a filter/search change).
    fn refresh_rows(&mut self) {
        self.refresh_series_rows();
        self.refresh_detail_rows();
        let lib = &self.lib;
        let upnext: Vec<usize> = lib.up_next(self.include_paused).iter().filter_map(|s| lib.index_of(&s.key)).collect();
        let ongoing = self.ctx.cfg.ongoing_roots();
        let mut files: Vec<usize> = (0..lib.files.len())
            .filter(|&i| lib.files[i].file.present && ongoing.contains(lib.files[i].file.root.as_str()))
            .collect();
        files.sort_by(|&a, &b| {
            let (a, b) = (&lib.files[a].file, &lib.files[b].file);
            b.added().cmp(&a.added()).then_with(|| a.path.cmp(&b.path))
        });
        let inbox = lib.inbox(&self.ctx.cfg);
        self.inbox_hint_width = inbox
            .iter()
            .filter_map(|e| e.hint)
            .map(|h| unicode_width::UnicodeWidthStr::width(h.describe(lib).as_str()))
            .max()
            .unwrap_or(0);
        self.upnext_ready = upnext.iter().filter(|&&i| lib.series[i].next_up().is_some()).count();
        (self.upnext_ix, self.inbox, self.files_ix) = (upnext, inbox, files);
        // The folder tree only while it is shown (`t` builds it).
        if self.files_mode == FilesMode::Tree {
            self.rebuild_folders();
        } else {
            self.folders = Folders::default();
        }
        self.clamp_selections();
    }

    /// Build the folder tree from the library, then its rows.
    fn rebuild_folders(&mut self) {
        let roots: Vec<String> = self.ctx.cfg.roots.iter().map(|r| r.name.clone()).collect();
        self.folders = Folders::build(&self.lib, &roots);
        self.refresh_tree_rows();
    }

    /// Recompute the tree rows: the listing of the folder shown, or, during
    /// a search, the folders and files below it that match (see
    /// [`Folders::search`]). A folder gone from disk (its files moved or
    /// deleted) gives way to the nearest one above it that still has files.
    fn refresh_tree_rows(&mut self) {
        while !self.tree_at.is_top() && self.folders.stats(&self.tree_at).is_none() {
            self.tree_at = self.tree_at.parent().unwrap_or_default();
        }
        self.tree_rows = if self.tree_query.is_empty() {
            self.folders.list(&self.tree_at)
        } else {
            self.folders.search(&self.tree_at, &self.tree_query)
        };
        clamp_cursor(&mut self.tree_state, self.tree_rows.len());
    }

    /// End the folder search: the folder's own listing again.
    fn end_tree_search(&mut self) {
        self.tree_query.clear();
        self.tree_state.select(Some(0));
        self.refresh_tree_rows();
    }

    /// Show `dir` in the tree (a search then covers what is below it), with
    /// the cursor on `cursor` if it is listed, else on the first row.
    fn open_dir(&mut self, dir: Dir, cursor: Option<RowId>) {
        self.tree_at = dir;
        self.tree_state.select(Some(0));
        self.refresh_tree_rows();
        if let Some(id) = cursor {
            self.select_id(View::Files, &id);
        }
    }

    /// Recompute only the Series list: all a filter or search change affects,
    /// so typing in the search box stays cheap on a big library.
    fn refresh_series_rows(&mut self) {
        let lib = &self.lib;
        // The status filter and the search's words and filters work together.
        let query = SeriesQuery::parse(&self.query);
        let accepts = |s: &Series| self.filter.accepts(s) && query.accepts(s);
        let candidates: Vec<(usize, &Series)> = lib.series.iter().enumerate().filter(|(_, s)| accepts(s)).collect();
        self.series_ix = if query.words.is_empty() {
            // Filters alone list every match, franchises unfolded.
            let fold = self.fold_franchises && !query.has_filters();
            franchise_order(lib, candidates.iter().map(|c| c.0), fold)
        } else {
            let mut hits: Vec<usize> = fuzzy_rank(candidates.iter(), &query.words, |(_, s)| s.search_text().into())
                .into_iter()
                .map(|(c, _)| c.0)
                .collect();
            // Then every entry of a franchise the words name.
            // As typed: a quoted phrase's spaces are escaped for the matcher.
            let q = query.words.replace("\\ ", " ").to_lowercase();
            for f in lib.franchises.iter().filter(|f| f.name.to_lowercase().contains(&q)) {
                // Series only: entries only on AniList aren't listed.
                for i in f.series().into_iter().filter(|&i| i < lib.series.len()) {
                    if !hits.contains(&i) && accepts(&lib.series[i]) {
                        hits.push(i);
                    }
                }
            }
            hits
        };
        clamp_cursor(&mut self.series_state, self.series_ix.len());
    }

    /// Recompute the Detail rows (after a reload, or when the series shown
    /// or the episode list's filters change).
    fn refresh_detail_rows(&mut self) {
        let EpisodeFilter { extras, missing } = self.episodes_shown;
        let Some((at, s)) =
            self.detail_key.as_deref().and_then(|k| Some((self.lib.index_any(k)?, self.lib.get_any(k)?)))
        else {
            self.detail_ix = Vec::new();
            return;
        };
        let (prev, next) = self.lib.franchise_neighbours(at);
        let season = |i: Option<usize>, next| i.map(|series| DetailAt::Season { series, next });
        // A split series part by part: each part's heading (even when none
        // of its episodes is listed), then its episodes unless it is
        // collapsed; then the other items. The entries of a collapsed
        // Related are left out too; its heading stays.
        let listed = s.ordered_indices(extras, missing);
        let mut rows = Vec::new();
        match &s.split {
            Some(split) => {
                let part_of = |i: usize| s.part_of_item(&s.items[i]);
                for n in 0..split.parts.len() {
                    rows.push(DetailAt::Divider(Divider::Part(n)));
                    if !self.collapsed(s, Divider::Part(n)) {
                        rows.extend(listed.iter().copied().filter(|&i| part_of(i) == Some(n)).map(DetailAt::Item));
                    }
                }
                rows.extend(listed.iter().copied().filter(|&i| part_of(i).is_none()).map(DetailAt::Item));
            }
            None => rows.extend(listed.into_iter().map(DetailAt::Item)),
        }
        // Related entries are listed whatever the episode filters hide.
        let heading = (!s.related.is_empty()).then_some(DetailAt::Divider(Divider::Related));
        let shown = !self.collapsed(s, Divider::Related);
        let related =
            s.related.iter().enumerate().filter(|_| shown).map(|(n, &(_, series))| DetailAt::Related { series, n });
        self.detail_ix = season(prev, false)
            .into_iter()
            .chain(rows)
            .chain(season(next, true))
            .chain(heading)
            .chain(related)
            .collect();
    }

    /// Whether section `d` of series `s` in Detail is collapsed: as you left
    /// it, else by default every part of a split series but the one holding
    /// the next episode (the last when none is left), and Related open.
    fn collapsed(&self, s: &Series, d: Divider) -> bool {
        self.folds.get(&d).copied().unwrap_or_else(|| match (d, &s.split) {
            (Divider::Part(n), Some(split)) => {
                let home = s.next_up().and_then(|it| s.part_of_item(it)).unwrap_or(split.parts.len() - 1);
                n != home
            }
            _ => false,
        })
    }

    /// Whether section `d` of the series in Detail is collapsed (see `collapsed`).
    pub fn detail_collapsed(&self, d: Divider) -> bool {
        self.detail_series().is_some_and(|s| self.collapsed(s, d))
    }

    /// The part of the series in Detail the cursor is in, for a split
    /// series: an episode's part (another item: the part of the nearest
    /// episode above it), a part's divider's; on ▲, the first part, and on
    /// ▼ or a related entry, the last.
    pub fn detail_part(&self) -> Option<usize> {
        let s = self.detail_series()?;
        let split = s.split.as_ref()?;
        let at = self.detail_state.selected()?;
        let last = split.parts.len() - 1;
        self.detail_ix.get(..=at)?.iter().rev().find_map(|r| match *r {
            DetailAt::Item(i) => s.part_of_item(s.items.get(i)?),
            DetailAt::Divider(Divider::Part(n)) => Some(n),
            DetailAt::Season { next: false, .. } => Some(0),
            DetailAt::Season { next: true, .. } | DetailAt::Related { .. } | DetailAt::Divider(Divider::Related) => {
                Some(last)
            }
        })
    }

    /// Step to part `n` of the split series in Detail: open it if it is
    /// collapsed, and put the cursor on its first episode (its divider when
    /// none is listed).
    fn step_to_part(&mut self, n: usize) -> bool {
        self.folds.insert(Divider::Part(n), false);
        self.refresh_detail_rows();
        let Some(at) = self.detail_ix.iter().position(|r| *r == DetailAt::Divider(Divider::Part(n))) else {
            return false;
        };
        let first = self.detail_ix.get(at + 1).filter(|r| r.item_ix().is_some()).map_or(at, |_| at + 1);
        self.detail_state.select(Some(first));
        true
    }

    /// Back to unwatched: item `it` of `series`, watched or started (its
    /// resume point goes too).
    fn reset_item(&mut self, series: &str, it: &Item) {
        let Some(s) = self.lib.get(series) else { return };
        let label = it.key.describe();
        if matches!(it.state, crate::model::WatchState::Unwatched) {
            return self.say(format!("{label} isn't started"));
        }
        // The first press asks (the row says so too), the second resets.
        let this = (series.to_string(), it.key.clone());
        if self.pending_reset.as_ref() != Some(&this) {
            self.pending_reset = Some(this);
            return self.say(format!("Backspace again to reset {label} to unwatched"));
        }
        self.pending_reset = None;
        let res = self.ctx.mark(s, std::slice::from_ref(&it.key), false);
        let ok = res.is_ok();
        self.after_write(res);
        if ok {
            self.say(format!("{label} reset to unwatched"));
        }
    }

    /// Record that episode `it` of `series` is the first of its linked
    /// `AniList` entry (see [`EventBody::Numbering`]): the episodes before it
    /// go to the entries before. On episode 1: no lead-in.
    fn number_from(&mut self, series: &str, it: &Item) {
        let Some(s) = self.lib.get(series) else { return };
        let Some(title) = s.meta.as_ref().filter(|_| s.anilist_id().is_some()).and_then(|m| m.title.clone()) else {
            return self.fail("not linked to AniList (L links it)");
        };
        let Some(first) = it.key.ep.map(crate::model::EpNo::whole) else { return };
        let label = it.key.describe();
        let anilist = s.anilist_id();
        self.record([EventBody::Numbering { series: series.to_string(), first, anilist }]);
        self.say(format!("{label} is now {title} 01"));
    }

    /// `]` (`next`) or `[` in Detail: in a split series, its next or
    /// previous part as listed; past its last or first, the franchise's
    /// entry after or before it, opened (at that part when it is split).
    /// Without a franchise, or at either end, a message.
    fn step_season(&mut self, next: bool) {
        let lib = Arc::clone(&self.lib);
        let nothing = if next { "nothing after this one" } else { "nothing before this one" };
        let Some(at) = self.detail_key.as_deref().and_then(|k| lib.index_any(k)) else { return };
        // Within a split series, its parts as listed (an empty one too); the
        // part left collapses, so the one stepped to is what shows.
        let parts = lib.entry(at).split.as_ref().map_or(0, |sp| sp.parts.len());
        if let Some(p) = self.detail_part() {
            let to = if next { Some(p + 1) } else { p.checked_sub(1) };
            if let Some(n) = to.filter(|&n| n < parts) {
                self.folds.insert(Divider::Part(p), true);
                self.step_to_part(n);
                return;
            }
        }
        // Past its first or last: the franchise's entry before or after it.
        let Some((f, _)) = lib.franchise_pos(at) else { return self.say(nothing) };
        let mine: Vec<usize> = f.stops_of(at).collect();
        let from = if next { mine.last() } else { mine.first() };
        let to = from.and_then(|&n| if next { n.checked_add(1) } else { n.checked_sub(1) });
        let Some(stop) = to.and_then(|n| f.stops.get(n)).filter(|st| st.series != at) else { return self.say(nothing) };
        self.open_detail(lib.entry(stop.series).key.clone());
        if let Some(n) = lib.stop_part(stop) {
            self.step_to_part(n);
        }
    }

    /// Collapse or expand section `d` of the series in Detail, keeping the
    /// cursor on its divider.
    fn toggle_fold(&mut self, d: Divider) {
        let Some(s) = self.detail_series() else { return };
        let now = !self.collapsed(s, d);
        self.folds.insert(d, now);
        self.reselect_detail(Some(&RowId::Divider(d)));
    }

    /// Whether the series in Detail has a season before or after it.
    pub fn detail_has_seasons(&self) -> bool {
        self.detail_ix.iter().any(|r| matches!(r, DetailAt::Season { .. }))
    }

    /// The first Detail row that is an item, else the first row.
    fn first_item_row(&self) -> usize {
        self.detail_ix.iter().position(|r| r.item_ix().is_some()).unwrap_or(0)
    }

    /// The last Detail row that is an item, else the last row.
    fn last_item_row(&self) -> usize {
        let last = self.detail_ix.len().saturating_sub(1);
        self.detail_ix.iter().rposition(|r| r.item_ix().is_some()).unwrap_or(last)
    }

    /// The Detail row for `id`: an item by its key, a season link by its
    /// series (a key held from before a merge finds the series it went into).
    fn detail_row_of(&self, id: &RowId) -> Option<usize> {
        match id {
            RowId::Item(k) => {
                let s = self.detail_series()?;
                self.detail_ix
                    .iter()
                    .position(|r| matches!(*r, DetailAt::Item(n) if s.items.get(n).is_some_and(|it| it.key == *k)))
            }
            RowId::Season { key, next } => {
                let at = self.lib.index_any(key)?;
                let link = DetailAt::Season { series: at, next: *next };
                self.detail_ix.iter().position(|r| *r == link)
            }
            RowId::Divider(d) => self.detail_ix.iter().position(|r| *r == DetailAt::Divider(*d)),
            RowId::Related(key) => {
                let at = self.lib.index_any(key)?;
                self.detail_ix.iter().position(|r| matches!(*r, DetailAt::Related { series, .. } if series == at))
            }
            _ => None,
        }
    }

    /// Whether a metadata update is running.
    pub fn meta_busy(&self) -> bool {
        self.meta_running.is_some()
    }

    /// What a running metadata update is doing, for the header, with the
    /// seconds left while it waits for `AniList`'s rate limit.
    pub fn meta_status(&self) -> Option<String> {
        let MetaRun { doing, wait_until, .. } = self.meta_running.as_ref()?;
        let left = wait_until.and_then(|t| t.checked_duration_since(Instant::now())).filter(|d| !d.is_zero());
        Some(match left {
            Some(d) => format!("{doing} · AniList rate limit {s}s", s = crate::fmt::secs_up(d)),
            None => doing.to_string(),
        })
    }

    /// Counts for the tab bar: Up next rows with something new, inbox size.
    pub fn tab_counts(&self) -> (usize, usize) {
        (self.upnext_ready, self.inbox.len())
    }

    // ---- rows -------------------------------------------------------------

    /// Series in Up next, rows `range` only.
    pub fn upnext_rows_in(&self, range: Range<usize>) -> Vec<&Series> {
        self.series_in(&self.upnext_ix, range)
    }

    /// Series in the Series view (filtered, searched), rows `range` only.
    pub fn series_rows_in(&self, range: Range<usize>) -> Vec<&Series> {
        self.series_in(&self.series_ix, range)
    }

    /// The series at rows `range` of a list of indices into `lib.series`.
    fn series_in(&self, ix: &[usize], range: Range<usize>) -> Vec<&Series> {
        ix[clip(&range, ix.len())].iter().map(|&i| &self.lib.series[i]).collect()
    }

    /// Files in ongoing roots, newest first, rows `range` only.
    pub fn file_rows_in(&self, range: Range<usize>) -> Vec<&crate::library::IndexedFile> {
        self.files_ix[clip(&range, self.files_ix.len())].iter().map(|&i| &self.lib.files[i]).collect()
    }

    /// Tree rows `range` only.
    pub fn tree_rows_in(&self, range: Range<usize>) -> &[Entry] {
        &self.tree_rows[clip(&range, self.tree_rows.len())]
    }

    fn selected_file(&self) -> Option<&crate::library::IndexedFile> {
        let i = match self.files_mode {
            FilesMode::Recent => *self.files_state.selected().and_then(|i| self.files_ix.get(i))?,
            FilesMode::Tree => match self.selected_entry()? {
                Entry::File(i) => *i,
                Entry::Folder { .. } => return None,
            },
        };
        self.lib.files.get(i)
    }

    /// The tree row under the cursor.
    fn selected_entry(&self) -> Option<&Entry> {
        self.tree_state.selected().and_then(|i| self.tree_rows.get(i))
    }

    /// The query the search box edits: the tree's in the Files view, else the
    /// Series view's.
    pub fn search_query(&self) -> &str {
        if self.view == View::Files { &self.tree_query } else { &self.query }
    }

    /// Map series keys held across a reload (Detail, the queue, an open popup,
    /// metadata requests) to the keys they are now (after a merge, or a
    /// legacy key), so events written for them name the current series and
    /// queue checks compare current keys. This is the one place held keys
    /// are resolved: everything else takes them as current, and looks up
    /// rows kept by key (see [`App::select_id`]) with [`Library::index_of`].
    fn resolve_held_keys(&mut self) {
        let lib = Arc::clone(&self.lib);
        let resolve = |k: &mut String| {
            let now = lib.resolve(k);
            if now != k {
                *k = now.to_string();
            }
        };
        self.detail_key.iter_mut().for_each(&resolve);
        // A queued item whose series' key changed (a season folded into its
        // series) is the item its file is now: its number may have moved.
        let cfg = &self.ctx.cfg;
        for q in &mut self.queue {
            let before = q.series.clone();
            resolve(&mut q.series);
            if q.series != before {
                let (series, items) = lib.identify(cfg, &q.file);
                if series == q.series
                    && let Some(item) = items.into_iter().next()
                {
                    q.item = item;
                }
            }
        }
        match &mut self.popup {
            Some(
                Popup::Status { series, .. }
                | Popup::Input { series, .. }
                | Popup::CatchUp { series, .. }
                | Popup::Unmerge { series, .. },
            ) => {
                resolve(series);
            }
            Some(Popup::Merge(picker)) => resolve(&mut picker.from),
            Some(Popup::Help { .. } | Popup::ConfirmQuit | Popup::Search) | None => {}
        }
        let running = self.meta_running.as_mut().map(|run| &mut run.request);
        for request in running.into_iter().chain(&mut self.meta_pending) {
            if let MetaRequest::Series(key) = request {
                resolve(key);
            }
        }
    }

    /// The series shown in Detail.
    pub fn detail_series(&self) -> Option<&Series> {
        self.detail_key.as_deref().and_then(|k| self.lib.get_any(k))
    }

    /// Detail rows: the season before, the items (extras right after their
    /// episode), the season after.
    pub fn detail_rows(&self) -> Vec<DetailRow<'_>> {
        self.detail_rows_in(0..self.rows_len(View::Detail))
    }

    /// Detail rows `range` only.
    pub fn detail_rows_in(&self, range: Range<usize>) -> Vec<DetailRow<'_>> {
        let Some(s) = self.detail_series() else { return Vec::new() };
        self.detail_ix[clip(&range, self.detail_ix.len())]
            .iter()
            .filter_map(|r| match *r {
                DetailAt::Item(i) => s.items.get(i).map(DetailRow::Item),
                DetailAt::Season { series, next } => {
                    self.lib.get_entry(series).map(|s| DetailRow::Season { series: s, index: series, next })
                }
                DetailAt::Divider(d) => Some(DetailRow::Divider(d)),
                DetailAt::Related { series: index, n } => {
                    let relation = s.related.get(n)?.0.as_str();
                    self.lib.get_entry(index).map(|series| DetailRow::Related { series, index, relation })
                }
            })
            .collect()
    }

    /// Inbox rows: (series, relation hint).
    pub fn inbox_rows(&self) -> Vec<(&Series, Option<Hint>)> {
        self.inbox_rows_in(0..self.inbox.len())
    }

    /// Inbox rows `range` only.
    pub fn inbox_rows_in(&self, range: Range<usize>) -> Vec<(&Series, Option<Hint>)> {
        self.inbox[clip(&range, self.inbox.len())].iter().map(|e| (&self.lib.series[e.index], e.hint)).collect()
    }

    /// Width in cells of the longest Inbox hint.
    pub fn inbox_hint_width(&self) -> usize {
        self.inbox_hint_width
    }

    /// Highlighted series in the Inbox.
    pub fn selected_inbox(&self) -> Option<&Series> {
        self.inbox_state.selected().and_then(|i| self.inbox.get(i)).map(|e| &self.lib.series[e.index])
    }

    /// Highlighted series in Up next.
    fn selected_upnext(&self) -> Option<&Series> {
        self.upnext_state.selected().and_then(|i| self.upnext_ix.get(i)).map(|&i| &self.lib.series[i])
    }

    /// Highlighted series in the Series view.
    fn selected_series(&self) -> Option<&Series> {
        self.series_state.selected().and_then(|i| self.series_ix.get(i)).map(|&i| &self.lib.series[i])
    }

    /// Series under the cursor in whatever view is active.
    fn focused_series(&self) -> Option<&Series> {
        match self.view {
            View::UpNext => self.selected_upnext(),
            View::Inbox => self.selected_inbox(),
            View::Series => self.selected_series(),
            View::Detail => self.detail_series(),
            View::Files => {
                if let Some(f) = self.selected_file() {
                    return self.lib.get(&f.series);
                }
                // A folder of one series stands for it.
                let series = &self.folders.stats(self.selected_entry()?.dir()?)?.series;
                if series.len() == 1 { self.lib.get(series.first()?) } else { None }
            }
            View::Queue => {
                self.queue_state.selected().and_then(|i| self.queue.get(i)).and_then(|q| self.lib.get(&q.series))
            }
        }
    }

    /// True if the item is queued.
    pub fn is_queued(&self, series: &str, item: &ItemKey) -> bool {
        self.queue.iter().any(|q| q.series == series && &q.item == item)
    }

    /// True if the file is queued.
    fn is_file_queued(&self, path: &Path) -> bool {
        self.queue_index_of(path).is_some()
    }

    /// Where the file is in the queue.
    fn queue_index_of(&self, path: &Path) -> Option<usize> {
        self.queue.iter().position(|q| q.file == path)
    }

    /// The file mpv is playing, once it has started.
    pub fn playing_file(&self) -> Option<&Path> {
        self.playing.as_ref()?.path.as_deref()
    }

    // ---- actions ----------------------------------------------------------

    fn toggle_queue(&mut self, series: &str, item: &Item) {
        if let Some(i) = self.queue.iter().position(|q| q.series == series && q.item == item.key) {
            self.queue.remove(i);
        } else if let Some(entry) = entry_for(series, item) {
            self.queue.push(entry);
        } else {
            self.say("not on disk");
        }
    }

    /// Queue the next new episode(s) of a series that aren't queued yet;
    /// true if it has more left to queue.
    fn queue_next_of(&mut self, key: &str, all: bool) -> bool {
        let Some(s) = self.lib.get(key) else { return false };
        let title = s.title.clone();
        // A multi-episode file is queued once: skip items whose file already is.
        let queued_file =
            |item: &ItemKey| s.item(item).and_then(Item::best_file).is_some_and(|f| self.is_file_queued(&f.path));
        // One more than wanted tells whether any are left.
        let want = if all { usize::MAX } else { 1 };
        let mut entries = queue_new(s, want.saturating_add(1), |item| self.is_queued(key, item) || queued_file(item));
        let more = entries.len() > want;
        entries.truncate(want);
        if entries.is_empty() {
            self.say(format!("nothing new to queue for {title}"));
            return false;
        }
        let n = entries.len();
        self.queue.extend(entries);
        self.say(format!("queued {n} from {title} ({queue} in queue)", queue = self.queue.len()));
        more
    }

    /// Keep events just recorded (without reading the log again), or report
    /// that writing them failed, then reload to show the new state.
    fn after_write(&mut self, res: Result<Recorded>) {
        match res {
            Ok(recorded) => self.ctx.log.merge(&mut self.events, recorded),
            Err(e) => self.fail(format!("{failed}: {e:#}", failed = crate::app::WRITE_FAILED)),
        }
        self.reload();
    }

    fn record(&mut self, bodies: impl IntoIterator<Item = EventBody>) {
        let res = self.ctx.record(bodies);
        self.after_write(res);
    }

    /// Mark items watched, or unwatched if the first one already is.
    fn toggle_watched(&mut self, series: &str, items: &[Item]) {
        let (Some(first), Some(s)) = (items.first(), self.lib.get(series)) else { return };
        let keys: Vec<ItemKey> = items.iter().map(|i| i.key.clone()).collect();
        let res = self.ctx.mark(s, &keys, !first.state.is_watched());
        self.after_write(res);
    }

    /// Mark every episode up to and including `upto` watched.
    fn watched_through(&mut self, series: &str, upto: &ItemKey) {
        let lib = Arc::clone(&self.lib);
        let Some(s) = lib.get(series) else { return };
        let Some(upto) = upto.ep else { return };
        let keys = s.unwatched_through(upto);
        self.say(format!("marked {keys} episode(s) watched", keys = keys.len()));
        let res = self.ctx.mark(s, &keys, true);
        self.after_write(res);
    }

    /// `p`: play the queue, or, with nothing queued, the next episode of the
    /// series under the cursor; resuming started files, or (`P`) from the
    /// beginning.
    fn play_key(&mut self, resume: bool) {
        let before = self.queue.len();
        if self.queue.is_empty() && self.playing.is_none() {
            match self.focused_series().map(|s| s.key.clone()) {
                Some(key) => _ = self.queue_next_of(&key, false),
                None => self.say("queue is empty"),
            }
            if self.queue.is_empty() {
                return;
            }
        }
        if !self.play_queue(resume) {
            // mpv did not start: take back what this press queued.
            self.queue.truncate(before);
        }
    }

    /// Start mpv on the whole queue, in order, resuming started files (or
    /// not). True if it started.
    pub fn play_queue(&mut self, resume: bool) -> bool {
        if self.queue.is_empty() && self.playing.is_none() {
            self.say("queue is empty");
            return false;
        }
        let files = self.ctx.play_files(&self.lib, &self.queue, resume);
        self.launch(&files)
    }

    /// Start mpv on just this item, leaving the queue as it is.
    fn play_item(&mut self, series: &str, item: &Item, resume: bool) {
        let Some(entry) = entry_for(series, item) else {
            self.say("not on disk");
            return;
        };
        let files = self.ctx.play_files(&self.lib, std::slice::from_ref(&entry), resume);
        self.launch(&files);
    }

    /// Spawn mpv on `files` and start tracking it, unless it is already
    /// running. True if it started.
    fn launch(&mut self, files: &[crate::mpv::PlayFile]) -> bool {
        if self.playing.is_some() {
            self.say("mpv is already running");
            return false;
        }
        let tx = self.tx.clone();
        match crate::mpv::spawn_tracked(&self.ctx.cfg, self.ctx.socket_path(), files, move |e| {
            let _ = tx.send(Msg::Player(e));
        }) {
            Ok(()) => {
                self.playing = Some(NowPlaying::starting(files.len()));
                // Entries leave the queue once mpv has played each file through
                // (see `on_player`), so quitting early, skipping a file, or a
                // file that fails to open loses nothing.
                true
            }
            Err(e) => {
                self.fail(format!("{e:#}"));
                false
            }
        }
    }

    /// `q` / Ctrl-C: ask first (asked already: quit).
    fn request_quit(&mut self) {
        if matches!(self.popup, Some(Popup::ConfirmQuit)) {
            self.popup = None;
            self.quit = true;
        } else {
            self.popup = Some(Popup::ConfirmQuit);
        }
    }

    /// Open the focused series' `AniList` page in the browser (or show its
    /// URL); in Detail, on a link to another entry, that entry's.
    fn open_anilist(&mut self) {
        let linked = (self.view == View::Detail)
            .then(|| self.detail_state.selected().and_then(|i| self.detail_ix.get(i)?.linked()))
            .flatten()
            .and_then(|i| self.lib.get_entry(i));
        let Some(s) = linked.or_else(|| self.focused_series()) else { return self.say("no series under the cursor") };
        // In a split series, the part under the cursor's.
        let part = (linked.is_none() && self.view == View::Detail)
            .then(|| Some(s.split.as_ref()?.parts.get(self.detail_part()?)?.id()))
            .flatten();
        let Some(id) = part.or_else(|| s.anilist_id()) else {
            return self.fail(format!("{title}: not linked to AniList (L links it)", title = s.title));
        };
        let url = format!("https://anilist.co/anime/{id}");
        match (self.opener)(&url) {
            Ok(()) => self.say(format!("opened {url}")),
            Err(_) => self.say(url),
        }
    }

    /// Copy the mpv command for the queue to the clipboard (or show it).
    fn yank(&mut self) {
        if self.queue.is_empty() {
            self.say("queue is empty");
            return;
        }
        let files = self.ctx.play_files(&self.lib, &self.queue, true);
        let cmd = crate::mpv::command_line(&self.ctx.cfg, &files);
        match copy_to_clipboard(&cmd) {
            Ok(()) => self.say("mpv command copied to clipboard"),
            Err(_) => self.say(String::from_utf8_lossy(&cmd).into_owned()),
        }
    }

    /// Handle a background message. Returns true if the screen needs redrawing.
    pub fn on_msg(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::ScanProgress(root, n) => {
                // Counts arrive far faster than they can be drawn: always keep
                // the latest, but redraw at most once per interval (or right
                // away when the scan moves on to another root).
                let prefix = format!("scanning {root}… ");
                let text = format!("{prefix}{n}");
                let same_root = self.scanning.as_deref().is_some_and(|s| s.starts_with(&prefix));
                if self.scanning.as_deref() == Some(text.as_str()) {
                    return false;
                }
                self.scanning = Some(text);
                if same_root && self.last_progress.is_some_and(|t| t.elapsed() < PROGRESS_INTERVAL) {
                    return false;
                }
                self.last_progress = Some(Instant::now());
            }
            Msg::ScanDone(res) => {
                self.scanning = None;
                if let Ok(done) = &res {
                    self.offline_roots.clone_from(&done.offline_roots);
                }
                let inbox = self.reload_index_with_news();
                match res {
                    // A root that failed is not left out for the inbox news.
                    Ok(done) if done.failed => {
                        self.fail(inbox.into_iter().chain([done.text]).collect::<Vec<_>>().join(" · "));
                    }
                    Ok(done) => self.say(inbox.unwrap_or(done.text)),
                    Err(e) => self.fail(format!("scan failed: {e}")),
                }
            }
            Msg::Player(ev) => self.on_player(ev),
            Msg::MetaProgress(what) => {
                if let Some(run) = &mut self.meta_running {
                    run.doing = what;
                }
            }
            Msg::AniListWait(until) => {
                if let Some(run) = &mut self.meta_running {
                    run.wait_until = Some(until);
                }
            }
            Msg::Meta(mut res, epoch) => {
                let request = self.meta_running.take().map_or(STARTUP, |run| run.request);
                // Keep everything except what this sync found for series
                // relinked while it ran; their own refresh is queued.
                let stale = |series: &String| self.relinked.get(series).is_some_and(|&at| at > epoch);
                res.rows.retain(|m| !stale(&m.series));
                res.attempts.retain(|a| !stale(&a.series));
                self.relinked.retain(|_, at| *at > epoch);
                if let Err(e) = self.ctx.store_sync(&res) {
                    self.fail(format!("could not cache metadata: {e:#}"));
                }
                // Even a sync that found nothing may have stored a new
                // offline database version, which every attempt is judged by.
                self.reload_meta();
                if let Some(msg) = self.meta_summary(&request, &res) {
                    self.say(msg);
                }
                // All in one message, after the summary and as important as an
                // error from the same sync, so none of them is hidden.
                if !res.notes.is_empty() {
                    self.fail(res.notes.iter().map(|n| format!("! {n}")).collect::<Vec<_>>().join(" · "));
                }
                if let Some(next) = self.meta_pending.take() {
                    self.start_meta(next);
                }
            }
            // Always followed by `Meta`, which ends the busy state.
            Msg::MetaError(e) => self.fail(e),
        }
        // Whatever the message changed (a reload, say), each cursor stays on a row.
        self.clamp_selections();
        true
    }

    fn on_player(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Connected(remote) => {
                if let Some(p) = &mut self.playing {
                    p.remote = Some(remote);
                    p.remote_on = self.ctx.cfg.remote_auto;
                }
            }
            PlayerEvent::Paused(paused) => {
                if let Some(p) = &mut self.playing {
                    p.paused = paused;
                }
            }
            PlayerEvent::Started { path } => {
                let (series, items) = self.lib.identify(&self.ctx.cfg, &path);
                let label = self.lib.label(&series, &items);
                if let Some(p) = &mut self.playing {
                    p.label = label;
                    p.path = Some(path);
                    p.pos = 0.0;
                    p.dur = None;
                }
            }
            PlayerEvent::Progress { pos, dur, .. } => {
                if let Some(p) = &mut self.playing {
                    p.pos = pos;
                    p.dur = dur;
                }
            }
            PlayerEvent::Ended { path, pos, furthest, dur, eof } => {
                let outcome = self.ctx.outcome(pos, furthest, dur, eof);
                match self.ctx.record_playback(&self.lib, &mut self.events, &path, outcome) {
                    Ok(label) => match outcome {
                        Outcome::Watched => {
                            // The queue cursor stays on its row as the rows above it go.
                            let at = self.selected_id(View::Queue);
                            self.queue.retain(|q| q.file != path);
                            if let Some(id) = at {
                                self.select_id(View::Queue, &id);
                            }
                            self.say(format!("✓ watched {label}"));
                        }
                        Outcome::Partial { pos, .. } => {
                            self.say(format!("◐ {label} stopped at {clock}", clock = crate::fmt::clock(pos)));
                        }
                        Outcome::Nothing => {}
                    },
                    Err(e) => {
                        // Not recorded as watched, so not taken out of the queue either.
                        let kept = if self.is_file_queued(&path) { "; it stays queued" } else { "" };
                        self.fail(format!("could not record playback: {e:#}{kept}"));
                    }
                }
                if let Some(p) = &mut self.playing {
                    p.done += 1;
                    p.path = None;
                }
                // mpv exiting usually follows at once: one reload for both.
                self.reload_due = true;
            }
            PlayerEvent::Exited { played, error } => {
                self.playing = None;
                match (played, error) {
                    (false, error) => {
                        let why = error.as_deref().unwrap_or("mpv exited before playing anything");
                        if self.queue.is_empty() {
                            self.fail(why);
                        } else {
                            self.fail(format!("{why}; the queue is unchanged"));
                        }
                    }
                    (true, Some(e)) => self.fail(e),
                    (true, None) => {}
                }
                self.reload_due = true;
            }
            PlayerEvent::Error(e) => self.fail(e),
        }
    }

    /// Drain pending background messages (then reload, if any of them asked
    /// for it); true if there were any.
    pub fn pump(&mut self) -> bool {
        let mut any = false;
        while let Ok(m) = self.rx.try_recv() {
            any |= self.on_msg(m);
        }
        if self.reload_due {
            self.reload();
            any = true;
        }
        if self.changes_checked.elapsed() >= CHANGES_INTERVAL {
            any |= self.pick_up_changes();
        }
        any
    }

    /// Wait up to `timeout` for one background message.
    #[doc(hidden)]
    pub fn wait_msg(&mut self, timeout: Duration) {
        if let Ok(m) = self.rx.recv_timeout(timeout) {
            self.on_msg(m);
            let _ = self.pump();
        }
    }

    /// Handle background messages until no metadata update is running, for
    /// at most `timeout`. True if none is.
    #[doc(hidden)]
    pub fn wait_for_meta(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.meta_busy() && Instant::now() < deadline {
            self.wait_msg(Duration::from_millis(50));
        }
        !self.meta_busy()
    }

    // ---- input ------------------------------------------------------------

    /// Feed a string of key presses (`\n` Enter, `\x1b` Esc, `\t` Tab, `\x7f` Backspace), for
    /// tests and scripted screenshots.
    #[doc(hidden)]
    pub fn press(&mut self, keys: &str) {
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                '\t' => KeyCode::Tab,
                '\x7f' => KeyCode::Backspace,
                c => KeyCode::Char(c),
            };
            self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }

    /// Handle a key press.
    pub fn on_key(&mut self, key: KeyEvent) {
        // A key for mpv changes nothing here.
        if self.remote_active() {
            self.on_remote_key(key);
            return;
        }
        self.handle_key(key);
        // Whatever the key changed (the queue, a list), each cursor stays on a row.
        self.clamp_selections();
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match ctrl_letter(key) {
                // Like `q`, but a second Ctrl-C at the prompt quits anyway. Any
                // other popup is closed first, like Esc (a draft is not lost
                // to a quit prompt).
                Some('c') => {
                    if matches!(self.popup, None | Some(Popup::ConfirmQuit)) {
                        self.request_quit();
                    } else {
                        self.on_popup_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                    }
                }
                Some('r') => self.start_meta(MetaRequest::All),
                _ => {}
            }
            return;
        }
        if self.popup.is_some() {
            self.on_popup_key(key);
            return;
        }
        // A reset waits for its second Backspace, and only that.
        if key.code != KeyCode::Backspace {
            self.pending_reset = None;
        }
        if let Some(why) = self.refused_on_remote(key) {
            self.say(why);
            return;
        }
        if self.on_global_key(key) {
            return;
        }
        match self.view {
            View::UpNext => self.on_upnext_key(key),
            View::Inbox => self.on_inbox_key(key),
            View::Series => self.on_series_key(key),
            View::Files => self.on_files_key(key),
            View::Queue => self.on_queue_key(key),
            View::Detail => self.on_detail_key(key),
        }
    }

    /// Why `key` does nothing in the Detail of an entry only on `AniList`
    /// (nothing on disk, no history): there is nothing to play, mark or
    /// queue, and it isn't a series to rename, link, merge or refresh yet.
    fn refused_on_remote(&self, key: KeyEvent) -> Option<&'static str> {
        // Keys that only look, move or open (and `s`/`C`, which record the
        // link with the status) work; any other is refused, so a key added
        // later can't record events for an entry that isn't a series.
        const SAFE: &str = "jkgGhl[]sCopPyv?qr123456dxf ";
        let remote = self.view == View::Detail && self.detail_series().is_some_and(|s| s.remote);
        match key.code {
            _ if !remote => None,
            KeyCode::Char('M') => Some("only on AniList: refreshed with its franchise's relations"),
            KeyCode::Char('m' | 'L' | 'R' | 'U') => Some("only on AniList: s sets a status to make it a series"),
            KeyCode::Char(c) if !SAFE.contains(c) => Some("only on AniList: nothing on disk or watched"),
            _ => None,
        }
    }

    /// Remote mode is on: keys go to mpv. An open popup (opened while mpv
    /// was starting, or the quit prompt) keeps its keys until it closes.
    pub fn remote_active(&self) -> bool {
        self.popup.is_none() && self.playing.as_ref().is_some_and(|p| p.remote_on && p.connected())
    }

    /// A key in remote mode: Ctrl-C hands the keys back to anipv, anything
    /// else is pressed in mpv.
    fn on_remote_key(&mut self, key: KeyEvent) {
        let Some(p) = &mut self.playing else { return };
        if ctrl_letter(key) == Some('c') {
            p.remote_on = false;
            return;
        }
        let (Some(remote), Some(name)) = (&p.remote, super::remote::mpv_key(key)) else { return };
        // Closed: mpv is quitting (a `q` a moment ago), so the key has nowhere
        // to go, and the keys stay out of anipv until the session's end comes in.
        if let Err(e) = remote.keypress(&name)
            && !crate::mpv::closed(&e)
        {
            p.remote_on = false;
            self.fail(format!("could not send {name} to mpv: {e}"));
        }
    }

    /// Keys that work in every view. Returns true if handled.
    fn on_global_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('v') => match &mut self.playing {
                Some(p) if p.connected() => p.remote_on = true,
                Some(_) => self.say("mpv is still starting"),
                None => self.say("mpv is not running"),
            },
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => {
                self.popup = Some(Popup::Help { scroll: 0, fits: false });
            }
            KeyCode::Char(c @ '1'..='5') => {
                let to = View::TABS[(c as u8 - b'1') as usize];
                // The tab you're on, again: its other mode (Files) or next filter (Series).
                match (self.view == to, to) {
                    (true, View::Files) => self.toggle_files_mode(),
                    (true, View::Series) => self.cycle_series_filter(),
                    _ => self.view = to,
                }
            }
            KeyCode::Tab => self.view = self.tab().cycle(true),
            KeyCode::BackTab => self.view = self.tab().cycle(false),
            KeyCode::Char('r') => {
                self.start_scan();
            }
            KeyCode::Char('M') => match self.focused_series().map(|s| s.key.clone()) {
                Some(key) => self.start_meta(MetaRequest::Series(key)),
                None => self.say("no series under the cursor"),
            },
            KeyCode::Char('o') => self.open_anilist(),
            KeyCode::Char('p') => self.play_key(true),
            // In Episodes, `P` plays the episode under the cursor (see `on_detail_key`).
            KeyCode::Char('P') if self.view != View::Detail => self.play_key(false),
            KeyCode::Char('y') => {
                self.yank();
            }
            // In the Inbox, `s` skips the show instead (see `on_inbox_key`).
            KeyCode::Char('s') if self.view != View::Inbox => {
                if let Some(s) = self.focused_series() {
                    let idx = SeriesStatus::ALL.iter().position(|x| *x == s.status).unwrap_or(0);
                    self.popup = Some(Popup::Status { series: s.key.clone(), idx });
                }
            }
            // In Detail, top and bottom (g/G) are the first and last episode:
            // the season links are one more k/j away, so G ⏎ still plays.
            code if self.view == View::Detail && matches!(vi_alias(code), KeyCode::Home | KeyCode::End) => {
                let row = if vi_alias(code) == KeyCode::Home { self.first_item_row() } else { self.last_item_row() };
                self.detail_state.select(Some(row));
            }
            code if let Some(delta) = cursor_delta(vi_alias(code)) => self.move_cursor(delta),
            _ => return false,
        }
        true
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.rows_len(self.view);
        step_cursor(self.state_mut(self.view), len, delta);
    }

    /// After the episode list's filters change, put the cursor back on
    /// `target`: a season link (always still shown), or an item, else the
    /// nearest item still shown (later rows first).
    fn reselect_detail(&mut self, target: Option<&RowId>) {
        self.refresh_detail_rows();
        let item = match target {
            Some(RowId::Item(k)) => Some(k),
            // A season link, if it's still there.
            Some(id) => {
                let row = self.detail_row_of(id).unwrap_or_else(|| self.first_item_row());
                self.detail_state.select(Some(row));
                return;
            }
            None => None,
        };
        let idx = self.detail_series().zip(item).and_then(|(s, t)| {
            // The rows are these items with some left out: find each by index.
            let all = s.ordered_indices(self.episodes_shown.extras, true);
            let row_of: HashMap<usize, usize> =
                self.detail_ix.iter().enumerate().filter_map(|(row, r)| Some((r.item_ix()?, row))).collect();
            let shown = |i: &usize| row_of.get(i).copied();
            let at = all.iter().position(|&i| s.items[i].key == *t)?;
            all[at..].iter().find_map(shown).or_else(|| all[..at].iter().rev().find_map(shown))
        });
        self.detail_state.select(Some(idx.unwrap_or_else(|| self.first_item_row())));
    }

    fn open_detail(&mut self, key: String) {
        if self.view != View::Detail {
            self.back = self.view;
        }
        // Another series starts from its default folds.
        if self.detail_key.as_deref() != Some(key.as_str()) {
            self.folds.clear();
        }
        self.detail_key = Some(key);
        self.refresh_detail_rows();
        let first = self.detail_series().map(|s| {
            let target = s.next_up().map(|i| &i.key);
            let row = self.detail_rows().iter().position(|r| r.item().is_some_and(|i| Some(&i.key) == target));
            row.unwrap_or_else(|| self.first_item_row())
        });
        self.detail_state.select(first);
        self.view = View::Detail;
    }

    fn on_inbox_key(&mut self, key: KeyEvent) {
        let Some((key_s, title)) = self.selected_inbox().map(|s| (s.key.clone(), s.title.clone())) else { return };
        let triage = |status: SeriesStatus, verb: &str| (status, format!("{verb} {title}"));
        let (status, msg) = match key.code {
            KeyCode::Char('f') => triage(SeriesStatus::Following, "following"),
            KeyCode::Char('s') => triage(SeriesStatus::Skipped, "skipped"),
            KeyCode::Char('z') => triage(SeriesStatus::Paused, "later:"),
            KeyCode::Char(' ') => {
                self.queue_next_of(&key_s, false);
                return;
            }
            KeyCode::Enter | KeyCode::Right => return self.open_detail(key_s),
            _ => return self.series_actions(key, key_s),
        };
        self.say(msg);
        let res = self.ctx.set_status(&self.lib, &key_s, status, None);
        self.after_write(res);
    }

    fn on_upnext_key(&mut self, key: KeyEvent) {
        // Works on an empty list too: that's when paused series are wanted most.
        if key.code == KeyCode::Char('i') {
            self.include_paused = !self.include_paused;
            self.say(if self.include_paused { "showing paused series" } else { "hiding paused series" });
            self.refresh_rows();
            return;
        }
        let Some(sel) = self.selected_upnext().map(|s| s.key.clone()) else { return };
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_detail(sel),
            // Stay put while there's more to queue, so Space can be pressed again.
            KeyCode::Char(' ') => {
                if !self.queue_next_of(&sel, false) {
                    self.move_cursor(1);
                }
            }
            KeyCode::Char('a') => _ = self.queue_next_of(&sel, true),
            KeyCode::Char('w') => {
                let next = self.lib.get(&sel).and_then(|s| s.next_up().cloned());
                if let Some(item) = next {
                    self.say(format!("marked {key} watched", key = item.key.describe()));
                    self.toggle_watched(&sel, std::slice::from_ref(&item));
                }
            }
            _ => {}
        }
    }

    fn on_series_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('/') => {
                self.popup = Some(Popup::Search);
                return;
            }
            KeyCode::Esc if !self.query.is_empty() => {
                self.clear_search();
                return;
            }
            KeyCode::Char('F') => {
                self.fold_franchises = !self.fold_franchises;
                let keep = self.selected_series().map(|s| s.key.clone());
                self.refresh_series_rows();
                if let Some(k) = keep {
                    self.select_id(View::Series, &RowId::Series(k));
                }
                return;
            }
            KeyCode::Char('f') => {
                self.cycle_series_filter();
                return;
            }
            _ => {}
        }
        let Some(s) = self.selected_series() else { return };
        let key_s = s.key.clone();
        match key.code {
            // Space opens too: episodes are queued from inside a series.
            KeyCode::Enter | KeyCode::Char('l' | ' ') | KeyCode::Right => self.open_detail(key_s),
            _ => self.series_actions(key, key_s),
        }
    }

    /// Keys that act on a series from Series or Detail.
    fn series_actions(&mut self, key: KeyEvent, series: String) {
        match key.code {
            KeyCode::Char('m') => self.popup = Some(Popup::Merge(MergePicker::new(self, &series))),
            KeyCode::Char('R') => {
                let text = self.lib.get(&series).map(|s| s.title.clone()).unwrap_or_default();
                self.popup = Some(Popup::Input { series, purpose: InputPurpose::Rename, text });
            }
            KeyCode::Char('L') => {
                let text =
                    self.lib.get(&series).and_then(Series::anilist_id).map(|i| i.to_string()).unwrap_or_default();
                self.popup = Some(Popup::Input { series, purpose: InputPurpose::Link, text });
            }
            KeyCode::Char('C') => match crate::app::catch_up(&self.lib, &series) {
                Ok(plan) if plan.events.is_empty() => self.say("caught up already"),
                Ok(plan) => {
                    let (n, eps) = (plan.entries.len(), plan.episodes);
                    let entries = plan.entries.join(", ");
                    let prompt = match plan.remote {
                        0 => format!("{eps} episode(s) of {n} entries: {entries}"),
                        remote => format!("{eps} episode(s) of {n} entries ({remote} only on AniList): {entries}"),
                    };
                    self.popup = Some(Popup::CatchUp { series, prompt });
                }
                Err(e) => self.fail(format!("{e:#}")),
            },
            KeyCode::Char('U') => {
                let aliases = self.lib.get(&series).map(|s| s.aliases.clone()).unwrap_or_default();
                if aliases.is_empty() {
                    self.say("nothing merged into this series");
                } else {
                    self.popup = Some(Popup::Unmerge { series, names: aliases });
                }
            }
            _ => {}
        }
    }

    fn on_files_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('t') {
            self.toggle_files_mode();
            return;
        }
        if self.files_mode == FilesMode::Tree && self.on_tree_key(key) {
            return;
        }
        let Some(f) = self.selected_file() else { return };
        let series = f.series.clone();
        let items: Vec<Item> =
            f.items.iter().filter_map(|it| self.lib.get(&series).and_then(|s| s.item(it)).cloned()).collect();
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_detail(series),
            KeyCode::Char(' ') => {
                // One queue entry per file: playing a multi-episode file marks all
                // its episodes. The file under the cursor is the one queued (not
                // the item's best copy), and only its own entry is toggled off.
                let path = f.file.path.clone();
                if let Some(i) = self.queue_index_of(&path) {
                    self.queue.remove(i);
                } else if let Some(first) = items.first() {
                    self.queue.push(QueueEntry { series: series.clone(), item: first.key.clone(), file: path });
                }
                self.move_cursor(1);
            }
            KeyCode::Char('w') => self.toggle_watched(&series, &items),
            _ => {}
        }
    }

    /// The Series view's next status filter (`f`, or `3` on the Series
    /// view); a search stays, the two work together.
    fn cycle_series_filter(&mut self) {
        self.filter = self.filter.next();
        self.series_state.select(Some(0));
        self.refresh_series_rows();
    }

    /// Recent ⇄ tree (`t`, or `4` on the Files view). The tree opens on the
    /// roots the first time, then where you left it.
    fn toggle_files_mode(&mut self) {
        match self.files_mode {
            FilesMode::Tree => self.files_mode = FilesMode::Recent,
            FilesMode::Recent => {
                self.files_mode = FilesMode::Tree;
                self.tree_query.clear();
                self.rebuild_folders();
            }
        }
    }

    /// Keys of the tree that aren't about a file: moving through folders,
    /// searching them, and queueing or marking a whole folder. True if the
    /// key was one of them.
    fn on_tree_key(&mut self, key: KeyEvent) -> bool {
        let folder = self.selected_entry().and_then(Entry::dir).cloned();
        match (key.code, folder) {
            (KeyCode::Char('/'), _) => self.popup = Some(Popup::Search),
            (KeyCode::Esc, _) if !self.tree_query.is_empty() => self.end_tree_search(),
            (KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace, _) => {
                if let Some(up) = self.tree_at.parent() {
                    let came_from = RowId::Dir(self.tree_at.clone());
                    self.open_dir(up, Some(came_from));
                }
            }
            (KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right, Some(dir)) => self.open_dir(dir, None),
            (KeyCode::Char(' '), Some(dir)) => {
                self.toggle_queue_folder(&dir);
                self.move_cursor(1);
            }
            (KeyCode::Char('w'), Some(dir)) => self.toggle_watched_folder(&dir),
            _ => return false,
        }
        true
    }

    /// Queue the unwatched files below `dir` in path order (those not queued
    /// yet), or take them out of the queue if all of them are in it.
    fn toggle_queue_folder(&mut self, dir: &Dir) {
        let lib = Arc::clone(&self.lib);
        let mut unwatched = self.folders.countable_under(&lib, dir);
        unwatched.retain(|f| !crate::folders::file_state(&lib, f).0);
        let queued = |f: &crate::library::IndexedFile| self.is_file_queued(&f.file.path);
        let name = dir.name();
        if unwatched.is_empty() {
            self.say(format!("nothing unwatched in {name}"));
        } else if unwatched.iter().all(|f| queued(f)) {
            let paths: HashSet<&Path> = unwatched.iter().map(|f| f.file.path.as_path()).collect();
            self.queue.retain(|q| !paths.contains(q.file.as_path()));
            self.say(format!("took {paths} from {name} out of the queue", paths = paths.len()));
        } else {
            let new: Vec<QueueEntry> = unwatched
                .iter()
                .filter(|f| !queued(f))
                .map(|f| QueueEntry { series: f.series.clone(), item: f.items[0].clone(), file: f.file.path.clone() })
                .collect();
            let n = new.len();
            self.queue.extend(new);
            self.say(format!("queued {n} from {name} ({queue} in queue)", queue = self.queue.len()));
        }
    }

    /// Mark every file below `dir` watched, or unwatched if all of them are.
    fn toggle_watched_folder(&mut self, dir: &Dir) {
        let lib = Arc::clone(&self.lib);
        let files = self.folders.countable_under(&lib, dir);
        if files.is_empty() {
            self.say(format!("nothing to mark in {name}", name = dir.name()));
            return;
        }
        let watched = !files.iter().all(|f| crate::folders::file_state(&lib, f).0);
        let mut by_series: BTreeMap<&str, Vec<ItemKey>> = BTreeMap::new();
        for f in &files {
            by_series.entry(f.series.as_str()).or_default().extend(f.items.iter().cloned());
        }
        let mut bodies = Vec::new();
        for (key, items) in by_series {
            if let Some(s) = lib.get(key) {
                bodies.extend(crate::app::mark_events(s, &items, watched));
            }
        }
        let state = if watched { "watched" } else { "unwatched" };
        let said = format!("marked {n} episode(s) in {name} {state}", n = bodies.len(), name = dir.name());
        let res = self.ctx.record(bodies);
        let ok = res.is_ok();
        self.after_write(res);
        if ok {
            self.say(said);
        }
    }

    fn on_queue_key(&mut self, key: KeyEvent) {
        let Some(i) = self.queue_state.selected() else { return };
        if i >= self.queue.len() {
            return;
        }
        match key.code {
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                self.queue.remove(i);
            }
            KeyCode::Char('K') if i > 0 => {
                self.queue.swap(i, i - 1);
                self.queue_state.select(Some(i - 1));
            }
            KeyCode::Char('J') if i + 1 < self.queue.len() => {
                self.queue.swap(i, i + 1);
                self.queue_state.select(Some(i + 1));
            }
            KeyCode::Char('c') => self.queue.clear(),
            // Shift-⏎ (where the terminal tells it from ⏎) from the beginning.
            KeyCode::Enter => {
                self.play_queue(!key.modifiers.contains(KeyModifiers::SHIFT));
            }
            _ => {}
        }
    }

    fn on_detail_key(&mut self, key: KeyEvent) {
        let Some(series) = self.detail_key.clone() else {
            self.view = self.back;
            return;
        };
        // Borrow the selected item from our own handle on the library instead
        // of cloning it, so `self` stays free for the actions below.
        let lib = Arc::clone(&self.lib);
        let at = self.detail_state.selected().and_then(|i| self.detail_ix.get(i).copied());
        let item: Option<&Item> = match at {
            Some(DetailAt::Item(i)) => lib.get(&series).and_then(|s| s.items.get(i)),
            _ => None,
        };
        let heading = match at {
            Some(DetailAt::Divider(d)) => Some((d, self.detail_collapsed(d))),
            _ => None,
        };
        match (key.code, item) {
            // ← on an open heading closes it, → on a closed one opens it.
            (KeyCode::Char('h') | KeyCode::Left, None) if let Some((d, false)) = heading => self.toggle_fold(d),
            (KeyCode::Char('l') | KeyCode::Right, None) if let Some((d, true)) = heading => self.toggle_fold(d),
            (KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left, _) => self.view = self.back,
            // Backspace: the episode back to unwatched (a resume point too).
            (KeyCode::Backspace, Some(it)) => self.reset_item(&series, it),
            // A season link opens that season; [ and ] open them from anywhere.
            // ⏎ or Space on a divider collapses or expands what it heads.
            (KeyCode::Enter | KeyCode::Char(' '), None) if let Some((d, _)) = heading => self.toggle_fold(d),
            (KeyCode::Enter, None) => {
                if let Some(i) = at.and_then(DetailAt::linked) {
                    self.open_detail(lib.entry(i).key.clone());
                }
            }
            (KeyCode::Char(c @ ('[' | ']')), _) => self.step_season(c == ']'),
            (KeyCode::Char('x'), item) => {
                let hiding = self.episodes_shown.extras;
                // Keep the cursor on the same row if it is still shown, else
                // on the nearest item above it that is (hiding hides only extras).
                let target = if item.is_some() {
                    let rows = self.detail_rows();
                    let upto = self.detail_state.selected().map_or(0, |i| (i + 1).min(rows.len()));
                    rows[..upto]
                        .iter()
                        .rev()
                        .filter_map(|r| r.item())
                        .find(|it| !hiding || it.key.kind != ItemKind::Extra)
                        .map(|it| RowId::Item(it.key.clone()))
                } else {
                    self.selected_id(View::Detail)
                };
                self.episodes_shown.extras = !hiding;
                self.reselect_detail(target.as_ref());
            }
            (KeyCode::Char('d'), _) => {
                let target = self.selected_id(View::Detail);
                self.episodes_shown.missing = !self.episodes_shown.missing;
                self.reselect_detail(target.as_ref());
            }
            (KeyCode::Char(' '), Some(it)) => {
                self.toggle_queue(&series, it);
                // Down to the next episode, not onto the ▼ link.
                let next = self.detail_state.selected().map(|i| i + 1);
                if next.and_then(|i| self.detail_ix.get(i)?.item_ix()).is_some() {
                    self.move_cursor(1);
                }
            }
            (KeyCode::Char('w'), Some(it)) => self.toggle_watched(&series, std::slice::from_ref(it)),
            // This episode is where the linked AniList entry starts.
            (KeyCode::Char('N'), Some(it)) if it.key.kind == ItemKind::Episode => self.number_from(&series, it),
            (KeyCode::Char('W'), Some(it)) if it.key.kind == ItemKind::Episode => {
                self.watched_through(&series, &it.key);
            }
            (KeyCode::Char('a'), _) => _ = self.queue_next_of(&series, true),
            // Play from here: just this item, whatever is queued stays queued
            // (and an item not on disk says so rather than play something else).
            // `P`, or shift-⏎ where the terminal tells it from ⏎, from the beginning.
            (KeyCode::Enter, Some(it)) => self.play_item(&series, it, !key.modifiers.contains(KeyModifiers::SHIFT)),
            (KeyCode::Char('P'), Some(it)) => self.play_item(&series, it, false),
            // On a ▲ ▼ season link, as everywhere else: the queue.
            (KeyCode::Char('P'), None) => self.play_key(false),
            _ => self.series_actions(key, series),
        }
    }

    fn on_popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.popup.take() else { return };
        match popup {
            // Taller than the terminal, the help scrolls (j/k, the arrows,
            // Space, PgUp/PgDn) and other keys close it; when it fits, any key does.
            Popup::Help { scroll, fits } => {
                if fits {
                    return;
                }
                let scroll = match key.code {
                    KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => scroll.saturating_add(PAGE.into()),
                    KeyCode::PageUp => scroll.saturating_sub(PAGE.into()),
                    _ => return,
                };
                self.popup = Some(Popup::Help { scroll, fits });
            }
            Popup::ConfirmQuit => {
                if matches!(key.code, KeyCode::Char('y' | 'q') | KeyCode::Enter) {
                    self.quit = true;
                }
            }
            Popup::Unmerge { series, names } => {
                // The names you were shown that are still merged into it (a
                // reload may have changed them): never one you weren't shown.
                let now = self.lib.get(&series).map(|s| s.aliases.clone()).unwrap_or_default();
                let names: Vec<String> = names.into_iter().filter(|n| now.contains(n)).collect();
                if is_yes(key.code) && !names.is_empty() {
                    self.say(format!("split off {n} merged name(s)", n = names.len()));
                    let res = self.ctx.unmerge(&self.lib, names.iter().map(String::as_str));
                    self.after_write(res);
                }
            }
            Popup::CatchUp { series, .. } => {
                if is_yes(key.code) {
                    let mut entries = String::new();
                    let res = crate::app::catch_up(&self.lib, &series).and_then(|plan| {
                        entries = plan.entries.join(", ");
                        self.ctx.catch_up(plan)
                    });
                    let ok = res.is_ok();
                    self.after_write(res);
                    if ok {
                        self.say(format!("✓ caught up: {entries}"));
                    }
                }
            }
            Popup::Status { series, mut idx } => {
                let n = SeriesStatus::ALL.len();
                let pick = match key.code {
                    KeyCode::Esc => return,
                    KeyCode::Down | KeyCode::Char('j') => {
                        idx = (idx + 1) % n;
                        None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        idx = (idx + n - 1) % n;
                        None
                    }
                    KeyCode::Enter => Some(SeriesStatus::ALL[idx]),
                    KeyCode::Char(c) => SeriesStatus::parse(&c.to_string()),
                    _ => None,
                };
                match pick {
                    Some(st @ (SeriesStatus::Dropped | SeriesStatus::Paused)) => {
                        self.popup =
                            Some(Popup::Input { series, purpose: InputPurpose::Note(st), text: String::new() });
                    }
                    Some(st) => self.set_status(&series, st, None),
                    None => self.popup = Some(Popup::Status { series, idx }),
                }
            }
            Popup::Merge(mut picker) => {
                match key.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        let target =
                            picker.selected().and_then(|k| self.lib.get(k)).map(|s| (s.key.clone(), s.title.clone()));
                        // `from` is a current key and never among the targets;
                        // the reload follows the merge in Detail and the queue.
                        if let Some((to, to_title)) = target {
                            self.say(format!("merged into {to_title}"));
                            self.record(vec![EventBody::Alias { from: picker.from, to }]);
                        }
                        return;
                    }
                    code if let Some(delta) = cursor_delta(code) => picker.step(delta),
                    code => {
                        if edit_text(&mut picker.query, code) {
                            picker.refresh(self, None);
                        }
                    }
                }
                self.popup = Some(Popup::Merge(picker));
            }
            // A search is a filter that stays until Esc; ⏎ only closes the
            // box. The arrows work as in the list, the box staying open
            // while the view does (← → between folders, → with nothing to open).
            Popup::Search => {
                let (view, folders) = (self.view, self.view == View::Files);
                match key.code {
                    KeyCode::Esc if folders => self.end_tree_search(),
                    KeyCode::Esc => self.clear_search(),
                    KeyCode::Enter => {}
                    code => {
                        if let Some(delta) = cursor_delta(code) {
                            self.move_cursor(delta);
                        } else if matches!(code, KeyCode::Left | KeyCode::Right) {
                            if folders { self.on_files_key(key) } else { self.on_series_key(key) }
                        } else if folders {
                            if edit_text(&mut self.tree_query, code) {
                                self.tree_state.select(Some(0));
                                self.refresh_tree_rows();
                            }
                        } else if edit_text(&mut self.query, code) {
                            self.series_state.select(Some(0));
                            self.refresh_series_rows();
                        }
                        if self.view == view {
                            self.popup = Some(Popup::Search);
                        }
                    }
                }
            }
            Popup::Input { series, purpose, mut text } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.submit_input(series, purpose, &text),
                code => {
                    edit_text(&mut text, code);
                    self.popup = Some(Popup::Input { series, purpose, text });
                }
            },
        }
    }

    /// End the Series view's search.
    fn clear_search(&mut self) {
        self.query.clear();
        self.refresh_series_rows();
    }

    /// Candidate series to merge `from` into.
    fn merge_targets(&self, from: &str, query: &str) -> Vec<&Series> {
        let from_ix = self.lib.index_of(from);
        let others = self.lib.series.iter().enumerate().filter(move |(i, _)| Some(*i) != from_ix).map(|(_, s)| s);
        if query.is_empty() {
            let from_title = self.lib.get(from).map(|s| s.title.as_str()).unwrap_or_default();
            // The suggested series (see `Library::merge_hint`), then similarly
            // named ones, then the rest.
            let hint = from_ix.and_then(|i| self.lib.merge_hint(i)).map(|j| &self.lib.series[j]);
            let first_word = from_title.split_whitespace().next().unwrap_or("");
            let mut v: Vec<&Series> = hint.into_iter().collect();
            v.extend(fuzzy_rank(others.clone(), first_word, |s| s.title.as_str().into()).into_iter().map(|x| x.0));
            let mut listed: HashSet<&str> = HashSet::new();
            v.retain(|s| listed.insert(s.key.as_str()));
            v.extend(others.filter(|s| !listed.contains(s.key.as_str())));
            return v;
        }
        fuzzy_rank(others, query, |s| s.search_text().into()).into_iter().map(|x| x.0).collect()
    }

    fn submit_input(&mut self, series: String, purpose: InputPurpose, text: &str) {
        let text = text.trim();
        match purpose {
            InputPurpose::Note(status) => {
                let note = Some(text.to_string()).filter(|t| !t.is_empty());
                self.set_status(&series, status, note);
            }
            InputPurpose::Rename => {
                if !text.is_empty() {
                    self.record(vec![EventBody::Title { series, title: text.into() }]);
                }
            }
            InputPurpose::Link => {
                // Empty or 0 unlinks.
                let text = if text.is_empty() { "0" } else { text };
                let Some(id) = crate::meta::anilist::parse_ref(text) else {
                    self.fail("not an AniList id");
                    return;
                };
                let id = (id != 0).then_some(id);
                let res = self.ctx.link(&self.lib, &series, id);
                let linked = res.is_ok();
                if linked {
                    // Its cached rows are gone: drop them here too. (`series`
                    // is current, see `resolve_held_keys`, as `link` needs.)
                    let lib = Arc::clone(&self.lib);
                    let keys: HashSet<&str> = lib.spellings(&series).collect();
                    self.index.meta.rows.retain(|m| !keys.contains(m.series.as_str()));
                } else if let Ok(meta) = self.ctx.meta_cache() {
                    self.index.meta = meta;
                }
                self.after_write(res);
                // Nothing changed if the link was not recorded: leave a running
                // sync's results for this series alone.
                if linked {
                    self.meta_epoch += 1;
                    self.relinked.insert(series.clone(), self.meta_epoch);
                    self.start_meta(MetaRequest::Series(series));
                }
            }
        }
    }

    fn set_status(&mut self, series: &str, status: SeriesStatus, note: Option<String>) {
        let title = self.lib.title_of(series);
        self.say(format!("{title} → {status}"));
        let res = self.ctx.set_status(&self.lib, series, status, note);
        self.after_write(res);
    }

    // ---- background work ---------------------------------------------------

    /// Rescan media roots in a background thread.
    pub fn start_scan(&mut self) {
        if self.scanning.is_some() {
            return;
        }
        if self.ctx.cfg.roots.is_empty() {
            self.fail("no media roots configured — run `anipv init`");
            return;
        }
        self.scanning = Some("scanning…".into());
        let cfg = self.ctx.cfg.clone();
        let db_path = self.ctx.paths.db_file.clone();
        let p = self.tx.clone();
        spawn_scan(&self.tx, move || {
            let mut db = crate::index::Db::open(&db_path)?;
            let out = crate::app::scan_all(&cfg, &mut db, None, &move |root, n| {
                let _ = p.send(Msg::ScanProgress(root.to_string(), n));
            })?;
            Ok(scan_summary(&out))
        });
    }

    /// Start a background metadata update (see [`crate::meta::plan`]). While
    /// one is running, a requested one is queued and starts when it finishes.
    pub fn start_meta(&mut self, request: MetaRequest) {
        if self.meta_running.is_some() {
            if !matches!(request, MetaRequest::Auto { .. }) {
                self.say("metadata update queued: one is already running");
                // One slot: two different requests merge into a full update.
                self.meta_pending = Some(match self.meta_pending.take() {
                    Some(queued) if queued != request => MetaRequest::All,
                    _ => request,
                });
            }
            return;
        }
        let checks = &self.index.meta.anilist_checks;
        let Some(opts) = crate::meta::plan(&self.lib, &self.ctx.cfg, checks, &request, now()) else { return };
        let opts = crate::meta::SyncOptions { offline: Arc::clone(&self.offline), ..opts };
        self.meta_running = Some(MetaRun::new(request));
        let (lib, cache, tx, epoch) =
            (Arc::clone(&self.lib), self.ctx.paths.cache_dir.clone(), self.tx.clone(), self.meta_epoch);
        spawn_meta(&self.tx, epoch, move || {
            // `opts` says whether the network may be used (see `plan`).
            let waiting = tx.clone();
            let on_wait = move |d: Duration| {
                let _ = waiting.send(Msg::AniListWait(Instant::now() + d));
            };
            let ureq = crate::meta::anilist::Ureq::new(crate::meta::anilist::INTERACTIVE_WAIT, &on_wait);
            let http = Some(&ureq as &dyn crate::meta::anilist::Http);
            crate::meta::sync_with_progress(&lib, &cache, http, &opts, now(), &mut |step| {
                let _ = tx.send(Msg::MetaProgress(match step {
                    crate::meta::SyncStep::Downloading => "downloading anime database…",
                    crate::meta::SyncStep::Syncing => "metadata",
                }));
            })
        });
    }

    /// What to tell the user when an update finishes (automatic ones stay
    /// quiet unless something changed).
    fn meta_summary(&self, request: &MetaRequest, res: &crate::meta::SyncResult) -> Option<String> {
        match request {
            MetaRequest::Auto { .. } => {
                (!res.rows.is_empty()).then(|| format!("metadata: updated {rows} series", rows = res.rows.len()))
            }
            MetaRequest::All => {
                let (updated, unmatched) = (res.rows.len(), res.attempts.len());
                Some(format!("metadata: {updated} updated · {unmatched} without a match"))
            }
            MetaRequest::Series(key) => {
                let s = self.lib.get(key)?;
                Some(match &s.meta {
                    Some(m) => {
                        let (total, state) = (s.total_text(), s.airing_text(self.now()));
                        let (title, anime) = (&s.title, m.title.as_deref().unwrap_or("linked"));
                        format!("{title}: {anime} · {total} eps · {state}")
                    }
                    None => format!("{title}: no AniList match (link one with L)", title = s.title),
                })
            }
        }
    }
}

/// How long a status line message is shown.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(6);

/// The letter typed with Ctrl, in lower case: Caps Lock or Shift make no
/// other key of it (`C` is `c`).
fn ctrl_letter(key: KeyEvent) -> Option<char> {
    let KeyCode::Char(c) = key.code else { return None };
    key.modifiers.contains(KeyModifiers::CONTROL).then_some(c.to_ascii_lowercase())
}

/// Rows PgUp/PgDn move in lists and the help.
const PAGE: u8 = 15;

/// What the status line says about a scan of every root.
fn scan_summary(out: &[crate::app::RootScan]) -> ScanSummary {
    let parts: Vec<String> = out
        .iter()
        .map(|r| {
            let changes = match &r.stats {
                Ok(s) if s.new > 0 || s.gone > 0 => format!("+{new} −{gone}", new = s.new, gone = s.gone),
                Ok(_) => "no changes".into(),
                Err(e) => e.to_string(),
            };
            match r.errors.len() {
                0 => format!("{root}: {changes}", root = r.root),
                n => format!("{root}: {changes} ({n} unreadable dirs)", root = r.root),
            }
        })
        .collect();
    // Known from the scan itself rather than checked on the UI thread:
    // looking at a root may block on a network mount. (A root whose
    // scan could not be recorded is mounted, so not offline.)
    let offline_roots = out.iter().filter(|r| r.offline).map(|r| r.root.clone()).collect();
    // An unmounted root is expected (and shown in the header); a root
    // that scanned but failed to record, or had unreadable folders, is not.
    let failed = out.iter().any(|r| (r.stats.is_err() && !r.offline) || !r.errors.is_empty());
    ScanSummary { text: parts.join(" · "), offline_roots, failed }
}

/// How important a status line message is: errors are shown over info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// What just happened.
    Info,
    /// Something failed.
    Error,
}

/// A status line message.
#[derive(Debug, Clone)]
pub struct Message {
    /// The text.
    pub text: String,
    /// How important it is.
    pub severity: Severity,
    /// When it was posted.
    at: Instant,
}

impl Message {
    fn is_fresh(&self) -> bool {
        self.at.elapsed() < MESSAGE_TIMEOUT
    }
}

/// Toggles for the episode list (session-wide, not per series).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EpisodeFilter {
    /// Extras (`x`).
    pub extras: bool,
    /// Items not on disk, known from history (`d`).
    pub missing: bool,
}

/// Keep the cursor inside a list of `len` rows (none on an empty list).
fn clamp_cursor(state: &mut TableState, len: usize) {
    step_cursor(state, len, 0);
}

/// A queue entry for `item` of `series`: its best file, if it has one on disk.
fn entry_for(series: &str, item: &Item) -> Option<QueueEntry> {
    Some(QueueEntry { series: series.to_string(), item: item.key.clone(), file: item.best_file()?.path.clone() })
}

/// Apply a text editing key (Backspace, a character) to `text`. True if it was one.
fn edit_text(text: &mut String, code: KeyCode) -> bool {
    match code {
        KeyCode::Backspace => {
            text.pop();
            true
        }
        KeyCode::Char(c) => {
            text.push(c);
            true
        }
        _ => false,
    }
}

/// Move the cursor by `delta` rows, staying inside a list of `len` rows.
fn step_cursor(state: &mut TableState, len: usize, delta: isize) {
    let cur = state.selected().unwrap_or(0);
    state.select(len.checked_sub(1).map(|last| cur.saturating_add_signed(delta).min(last)));
}

/// The cursor key a vi letter in a list stands for (`j` ↓, `k` ↑, `g` Home,
/// `G` End); any other key is itself.
fn vi_alias(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char('j') => KeyCode::Down,
        KeyCode::Char('k') => KeyCode::Up,
        KeyCode::Char('g') => KeyCode::Home,
        KeyCode::Char('G') => KeyCode::End,
        code => code,
    }
}

/// A yes to a yes/no popup: `y` or Enter (any other key is a no).
fn is_yes(code: KeyCode) -> bool {
    matches!(code, KeyCode::Char('y') | KeyCode::Enter)
}

/// How far a cursor key moves the cursor in a list (`None`: not one).
fn cursor_delta(code: KeyCode) -> Option<isize> {
    Some(match code {
        KeyCode::Down => 1,
        KeyCode::Up => -1,
        KeyCode::PageDown => isize::from(PAGE),
        KeyCode::PageUp => -isize::from(PAGE),
        KeyCode::Home => isize::MIN / 2,
        KeyCode::End => isize::MAX / 2,
        _ => return None,
    })
}

/// `range` limited to a list of `len` rows, as a slice index.
fn clip(range: &Range<usize>, len: usize) -> Range<usize> {
    let end = range.end.min(len);
    range.start.min(end)..end
}

/// Run `job` on a background thread and always send the messages `report`
/// makes of how it ended, even if it panics (then `report` gets the panic
/// text), so a busy state it set never stays stuck.
fn spawn_reporting<T, M>(
    tx: &Sender<Msg>,
    job: impl FnOnce() -> T + Send + 'static,
    report: impl FnOnce(Result<T, String>) -> M + Send + 'static,
) where
    M: IntoIterator<Item = Msg>,
{
    let tx = tx.clone();
    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).map_err(|panic| {
            panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown error".into())
        });
        for msg in report(res) {
            let _ = tx.send(msg);
        }
    });
}

/// Run `scan` in the background; it always ends with `ScanDone`, so the
/// header and `r` never stay stuck on "scanning".
fn spawn_scan(tx: &Sender<Msg>, scan: impl FnOnce() -> Result<ScanSummary> + Send + 'static) {
    spawn_reporting(tx, scan, |res| {
        let res = match res {
            Ok(res) => res.map_err(|e| format!("{e:#}")),
            Err(panic) => Err(format!("scanner crashed: {panic}")),
        };
        [Msg::ScanDone(res)]
    });
}

/// Run a metadata update in the background; it always ends with `Meta`
/// (after `MetaError` if anything went wrong), so the busy state clears.
fn spawn_meta(tx: &Sender<Msg>, epoch: u64, sync: impl FnOnce() -> crate::meta::SyncResult + Send + 'static) {
    spawn_reporting(tx, sync, move |res| {
        let (res, errors) = match res {
            Ok(mut res) => {
                let errors = std::mem::take(&mut res.errors);
                (res, errors)
            }
            Err(panic) => (crate::meta::SyncResult::default(), vec![format!("metadata update crashed: {panic}")]),
        };
        let error = (!errors.is_empty()).then(|| Msg::MetaError(errors.join("; ")));
        error.into_iter().chain([Msg::Meta(res, epoch)])
    });
}

/// How often the TUI looks for scans and metadata stored by another process.
const CHANGES_INTERVAL: Duration = Duration::from_secs(30);

/// Scan progress is shown at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// What a metadata update covers: [`STARTUP`], everything (`ctrl-r`) or one
/// series (`M`).
pub use crate::meta::Request as MetaRequest;

/// The automatic update on start: only what is missing or stale.
pub const STARTUP: MetaRequest = MetaRequest::Auto { max_age: crate::meta::STARTUP_MAX_AGE };

/// Open `url` with the desktop's opener, without waiting for the browser.
fn open_url(url: &str) -> Result<()> {
    use std::process::{Command, Stdio};
    for cmd in ["xdg-open", "open"] {
        if let Ok(mut child) =
            Command::new(cmd).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        {
            // Reaped in the background, so it doesn't linger as a zombie.
            std::thread::spawn(move || child.wait());
            return Ok(());
        }
    }
    anyhow::bail!("no URL opener found")
}

fn copy_to_clipboard(bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    for (cmd, args) in candidates {
        if let Ok(mut child) =
            Command::new(cmd).args(*args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        {
            // A tool that exits early (no display, say) must not stop the
            // next candidate from being tried.
            if let Some(mut stdin) = child.stdin.take()
                && stdin.write_all(bytes).is_err()
            {
                let _ = child.wait();
                continue;
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return Ok(());
            }
        }
    }
    anyhow::bail!("no clipboard tool found")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TUI on a demo library (in a directory that goes with the handle).
    fn demo_app() -> (tempfile::TempDir, App) {
        let (dir, ctx) = crate::demo::ctx();
        (dir, App::new(ctx).unwrap())
    }

    /// Space in Up next stays on a series while it has more new episodes
    /// to queue, and moves on once it has none.
    #[test]
    fn upnext_space_stays_while_more_to_queue() {
        let (_dir, mut app) = demo_app();
        let new_of = |app: &App, i: usize| queue_new(&app.lib.series[app.upnext_ix[i]], usize::MAX, |_| false).len();
        let row = (0..app.upnext_ix.len().saturating_sub(1))
            .find(|&i| new_of(&app, i) >= 2)
            .expect("a demo series in Up next with 2+ new episodes, and a row below it");
        let n = new_of(&app, row);
        app.upnext_state.select(Some(row));
        for queued in 1..n {
            app.press(" ");
            assert_eq!(app.queue.len(), queued);
            assert_eq!(app.upnext_state.selected(), Some(row), "more to queue: stays");
        }
        app.press(" ");
        assert_eq!(app.queue.len(), n);
        assert_eq!(app.upnext_state.selected(), Some(row + 1), "nothing left: moves on");
    }

    /// Space in Series opens the series rather than queueing from it.
    #[test]
    fn series_space_opens_the_series() {
        let (_dir, mut app) = demo_app();
        app.press("3 ");
        assert_eq!(app.view, View::Detail);
        assert!(app.queue.is_empty());
    }

    /// The Files view's tree: drill down and up, queue and mark whole
    /// folders, search by path, and back to the newest files.
    #[test]
    fn files_tree_browses_queues_and_marks_folders() {
        let (_dir, mut app) = demo_app();
        let names = |app: &App| -> Vec<String> {
            app.tree_rows
                .iter()
                .map(|e| match e {
                    Entry::Folder(dir) => dir.name(),
                    Entry::File(i) => app.lib.files[*i].file.name(),
                })
                .collect()
        };
        let selected = |app: &App| app.selected_entry().and_then(Entry::dir).map(Dir::name);
        // `4` on the Files view switches to folders, which open on the roots.
        app.press("44");
        assert_eq!(app.files_mode, FilesMode::Tree);
        assert!(app.tree_at.is_top());
        assert_eq!(names(&app), ["Downloads", "Anime"], "roots in config order");
        app.press("\nh");
        assert_eq!(selected(&app).as_deref(), Some("Downloads"), "back up on the folder it came from");
        app.press("j\n");
        let mono = names(&app).iter().position(|n| n == "Monogatari Series").unwrap();
        app.tree_state.select(Some(mono));
        app.press("\n");
        assert_eq!(names(&app), ["01 - Bakemonogatari", "02 - Nisemonogatari", "03 - Nekomonogatari (Black)"]);

        // Space on a folder queues its unwatched episodes in order; again, unqueues them.
        app.press("j ");
        let eps: Vec<String> =
            app.queue.iter().map(|q| format!("{series} {item}", series = q.series, item = q.item.describe())).collect();
        assert_eq!(eps.len(), 8);
        assert_eq!((eps[0].as_str(), eps[7].as_str()), ("nisemonogatari 04", "nisemonogatari 11"));
        app.press("k ");
        assert!(app.queue.is_empty());

        // `w` on a folder (the cursor moved on to the next) marks everything in it watched.
        app.press("w");
        assert_eq!(app.lib.get("nekomonogatari").unwrap().watched_count(), 4);
        app.press("h");
        assert_eq!(selected(&app).as_deref(), Some("Monogatari Series"));

        // A search finds what is below the folder by path; ⏎ opens a hit and
        // the search stays (it now covers what is below that), Esc ends it.
        app.press("/mono nise\n");
        assert!(app.tree_rows[0].dir().unwrap().display().ends_with("Monogatari Series/02 - Nisemonogatari"));
        app.press("\n");
        assert_eq!((app.tree_at.name().as_str(), app.tree_query.as_str()), ("02 - Nisemonogatari", "mono nise"));
        app.press("\x1b");
        assert_eq!(app.tree_query, "");
        app.press("/nise\nt");
        assert_eq!(app.files_mode, FilesMode::Recent);
        // Back to folders: where you left them, the search over.
        app.press("4");
        assert_eq!((app.files_mode, app.tree_at.name().as_str()), (FilesMode::Tree, "02 - Nisemonogatari"));
        assert_eq!(app.tree_query, "");
    }

    fn arrow(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// In the Series search box ↑/↓ move through the matches while typing, →
    /// opens the one under the cursor (the query stays), ← does nothing.
    #[test]
    fn arrows_work_in_the_series_search() {
        let (_dir, mut app) = demo_app();
        app.press("3/mono");
        arrow(&mut app, KeyCode::Down);
        assert_eq!((app.popup.clone(), app.query.as_str()), (Some(Popup::Search), "mono"));
        assert_eq!(app.series_state.selected(), Some(1));
        arrow(&mut app, KeyCode::Left);
        assert_eq!(app.popup, Some(Popup::Search));
        let picked = app.selected_series().unwrap().key.clone();
        arrow(&mut app, KeyCode::Right);
        assert_eq!((app.view, app.popup.clone()), (View::Detail, None));
        assert_eq!(app.detail_key.as_deref(), Some(picked.as_str()));
        app.press("\x1b");
        assert_eq!((app.view, app.query.as_str()), (View::Series, "mono"), "the matches are still there");
    }

    /// In the folder search the arrows work as in the list, the box and the
    /// query staying: ↑/↓ move, → goes into a folder (a file: its series),
    /// ← goes up. Only Esc ends the search.
    #[test]
    fn arrows_work_in_the_folder_search() {
        let (_dir, mut app) = demo_app();
        app.press("44j\n/nise");
        // Below Anime: the Nisemonogatari folder, then its files.
        let nise = app.tree_rows[0].dir().cloned().unwrap();
        assert_eq!(nise.relative(&app.tree_at), "Monogatari Series/02 - Nisemonogatari");
        assert!(matches!(app.tree_rows[1], Entry::File(_)));
        arrow(&mut app, KeyCode::Down);
        arrow(&mut app, KeyCode::Up);
        assert_eq!((app.popup.clone(), app.tree_state.selected()), (Some(Popup::Search), Some(0)));
        arrow(&mut app, KeyCode::Right);
        assert_eq!((&app.tree_at, app.tree_query.as_str(), app.popup.clone()), (&nise, "nise", Some(Popup::Search)));
        assert!(app.tree_rows.iter().all(|e| matches!(e, Entry::File(_))), "its episodes");
        arrow(&mut app, KeyCode::Left);
        assert_eq!((app.tree_at.name().as_str(), app.tree_query.as_str()), ("Monogatari Series", "nise"));
        // ⏎ keeps the search; → on an episode opens its series, and back it is still there.
        app.press("\n");
        app.tree_state.select(Some(1));
        arrow(&mut app, KeyCode::Right);
        assert_eq!(app.view, View::Detail);
        app.press("\x1b");
        assert_eq!((app.view, app.tree_query.as_str()), (View::Files, "nise"));
        app.press("\x1b");
        assert_eq!(app.tree_query, "");
        // Nothing to open: the box stays open.
        app.press("/zzzz");
        arrow(&mut app, KeyCode::Right);
        assert_eq!((app.popup.clone(), app.tree_query.as_str()), (Some(Popup::Search), "zzzz"));
    }

    /// Ctrl-C, typed as `c` or (Caps Lock on) as `C`.
    fn ctrl_c(app: &mut App, letter: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL));
    }

    /// `q` and Ctrl-C (Caps Lock on too) ask first, playing or not; at the
    /// prompt `q`, `y`, Enter or a second Ctrl-C quits, any other key cancels.
    #[test]
    fn quitting_asks_first() {
        let (_dir, mut app) = demo_app();
        for playing in [true, false] {
            app.playing = playing.then(|| NowPlaying::starting(1));
            for quit in ["q", "y", "\n", "ctrl-c", "ctrl-C"] {
                for first in ["q", "ctrl-c", "ctrl-C"] {
                    let press = |app: &mut App, k: &str| match k {
                        "ctrl-c" => ctrl_c(app, 'c'),
                        "ctrl-C" => ctrl_c(app, 'C'),
                        _ => app.press(k),
                    };
                    for cancel in ["n", "\x1b", "x"] {
                        press(&mut app, first);
                        assert!(!app.quit, "{first} alone doesn't quit");
                        assert_eq!(app.popup, Some(Popup::ConfirmQuit));
                        press(&mut app, cancel);
                        assert!(!app.quit && app.popup.is_none(), "{cancel:?} cancels");
                    }
                    press(&mut app, first);
                    press(&mut app, quit);
                    assert!(app.quit, "{first} then {quit:?} quits (playing: {playing})");
                    app.quit = false;
                }
            }
        }
    }

    /// Ctrl-C closes any popup but the quit prompt (and a draft in it) like
    /// Esc, rather than asking to quit; then it asks as usual.
    #[test]
    fn ctrl_c_closes_popups_with_something_to_lose() {
        let (_dir, mut app) = demo_app();
        for (keys, letter) in ["?", "3s", "3m", "3R", "3/"].into_iter().flat_map(|k| [(k, 'c'), (k, 'C')]) {
            app.press(keys);
            assert!(app.popup.is_some(), "{keys:?} opens a popup");
            ctrl_c(&mut app, letter);
            assert!(app.popup.is_none() && !app.quit, "{keys:?}: Ctrl-{letter} closes it");
            ctrl_c(&mut app, letter);
            assert_eq!(app.popup, Some(Popup::ConfirmQuit), "then it asks");
            app.press("n");
        }
    }

    /// An error stays in the status line over later messages until it times
    /// out; the newest of each kind wins.
    #[test]
    fn errors_stay_visible_over_later_info() {
        let (_dir, mut app) = demo_app();
        app.say("one");
        assert_eq!(app.current_message(), Some("one"));
        app.fail("broken");
        app.say("two");
        assert_eq!(app.current_message(), Some("broken"));
        app.fail("worse");
        assert_eq!(app.current_message(), Some("worse"));
        // Once the errors are old, info shows again.
        for m in &mut app.messages {
            let Some(old) = m.at.checked_sub(MESSAGE_TIMEOUT) else { return };
            m.at = old;
        }
        app.say("three");
        assert_eq!(app.current_message(), Some("three"));
        assert_eq!(app.messages.len(), 1, "expired messages are dropped");
    }

    /// Every library warning is shown, not just the first.
    #[test]
    fn all_library_warnings_are_shown() {
        let (_dir, mut app) = demo_app();
        let mut lib = (*app.lib).clone();
        lib.warnings = vec!["first".into(), "second".into()];
        app.lib = Arc::new(lib);
        app.warn_all();
        app.say("info");
        assert_eq!(app.current_message(), Some("first; second"));
    }

    /// A warning that stays (here an unreadable event log file) shows when it
    /// appears, not again on every reload, where it would hide what each
    /// action reports.
    #[test]
    fn a_lasting_warning_shows_once() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, ctx) = crate::demo::ctx();
        let other = ctx.paths.events_dir.join("laptop.jsonl");
        std::fs::write(&other, "").unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&other).is_ok() {
            return; // Running as root: nothing is unreadable.
        }
        let mut app = App::new(ctx).unwrap();
        assert!(
            app.current_message().is_some_and(|m| m.contains("laptop.jsonl")),
            "{message:?}",
            message = app.current_message()
        );
        app.messages.clear();
        app.reload();
        assert_eq!(app.current_message(), None, "not again on a reload");
    }

    /// Every note of a sync shows, in one message that an error from the same
    /// sync or later info does not hide.
    #[test]
    fn sync_notes_show_together() {
        let (_dir, mut app) = demo_app();
        let res = crate::meta::SyncResult { notes: vec!["one".into(), "two".into()], ..Default::default() };
        app.on_msg(Msg::MetaError("rate limit".into()));
        app.on_msg(Msg::Meta(res, 0));
        app.say("later info");
        assert_eq!(app.current_message(), Some("! one · ! two"));
    }

    /// A scan reports unreadable folders and roots it could not record, also
    /// when new shows arrived; an unmounted root alone is no failure.
    #[test]
    fn scan_summaries_report_what_failed() {
        use crate::app::RootScan;
        use crate::index::db::ScanStats;
        let root = |name: &str, stats, offline, errors: usize| RootScan {
            root: name.into(),
            stats,
            offline,
            errors: vec!["x".into(); errors],
        };
        let new = Ok(ScanStats { seen: 3, new: 1, gone: 0 });
        let summary = scan_summary(&[root("Anime", new, false, 2), root("Ext", Err(anyhow::anyhow!("gone")), true, 0)]);
        assert_eq!(summary.text, "Anime: +1 −0 (2 unreadable dirs) · Ext: gone");
        assert!(summary.failed);
        assert_eq!(summary.offline_roots, ["Ext"]);
        let fine = scan_summary(&[
            root("Anime", Ok(ScanStats::default()), false, 0),
            root("Ext", Err(anyhow::anyhow!("gone")), true, 0),
        ]);
        assert!(!fine.failed, "an unmounted root is shown in the header instead");
        let locked = scan_summary(&[root("Anime", Err(anyhow::anyhow!("database is locked")), false, 0)]);
        assert!(locked.failed);

        let (_dir, mut app) = demo_app();
        app.on_msg(Msg::ScanDone(Ok(summary)));
        app.say("later info");
        assert_eq!(app.current_message(), Some("Anime: +1 −0 (2 unreadable dirs) · Ext: gone"), "shown as an error");
    }

    /// Tab moves on from the tab Detail was opened from (the one the header
    /// shows as active).
    #[test]
    fn tab_moves_on_from_the_tab_detail_belongs_to() {
        let (_dir, mut app) = demo_app();
        app.press("3\n");
        assert_eq!((app.view, app.tab()), (View::Detail, View::Series));
        app.press("\t");
        assert_eq!(app.view, View::Files);
        app.press("3\n");
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.view, View::Inbox);
    }

    /// Ctrl-C in the search box ends the search like Esc does.
    #[test]
    fn ctrl_c_in_the_search_box_clears_the_search() {
        let (_dir, mut app) = demo_app();
        app.press("3/fri");
        assert_eq!(app.query, "fri");
        ctrl_c(&mut app, 'c');
        assert!(app.popup.is_none() && app.query.is_empty());
        let accepted = app.lib.series.iter().enumerate().filter(|(_, s)| app.filter.accepts(s)).map(|(i, _)| i);
        assert_eq!(app.series_ix, franchise_order(&app.lib, accepted, app.fold_franchises));
    }

    /// `[` and `]` follow the franchise, not the rows: with only the `▼`
    /// link listed (no episodes shown), `[` still says there's nothing
    /// before, and a link whose series went away (a merge) leaves the cursor
    /// on an episode.
    #[test]
    fn season_links_are_told_apart() {
        let (_dir, mut app) = demo_app();
        app.press("3/frieren\n\n");
        // As for a series split around this one: both links open it.
        let s2 = app.lib.index_of("sousou no frieren s2").unwrap();
        app.detail_ix.insert(0, DetailAt::Season { series: s2, next: false });
        let last = app.detail_ix.len() - 1;
        app.detail_state.select(Some(last));
        let id = app.selected_id(View::Detail).unwrap();
        assert_eq!(app.detail_row_of(&id), Some(last), "the ▼ link, not the ▲ one");
    }

    #[test]
    fn season_keys_follow_the_franchise_not_the_rows() {
        let (_dir, mut app) = demo_app();
        app.press("3/frieren\n\n");
        assert_eq!(app.detail_key.as_deref(), Some("sousou no frieren"));
        // As if every episode were hidden: only the link to S2 is a row.
        app.detail_ix.retain(|r| matches!(r, DetailAt::Season { .. }));
        app.detail_state.select(Some(0));
        app.press("[");
        assert_eq!(app.detail_key.as_deref(), Some("sousou no frieren"));
        assert_eq!(app.current_message(), Some("nothing before this one"));
        // A link to a series no longer next to it falls back to an episode.
        app.refresh_detail_rows();
        app.reselect_detail(Some(&RowId::Series("one piece".into())));
        let row = app.detail_state.selected().unwrap();
        assert!(matches!(app.detail_ix[row], DetailAt::Item(_)));
        app.press("]");
        assert_eq!(app.detail_key.as_deref(), Some("sousou no frieren s2"));
    }

    /// Folded, the Series view shows the franchise entry you're at; `F`
    /// unfolds it into every entry, in watch order.
    #[test]
    fn franchises_fold_to_the_entry_you_are_at() {
        let (_dir, mut app) = demo_app();
        app.press("3");
        let keys = |app: &App| -> Vec<String> {
            app.series_ix.iter().map(|&i| app.lib.series[i].key.clone()).filter(|k| k.contains("monogatari")).collect()
        };
        assert_eq!(keys(&app), ["nisemonogatari"]);
        app.press("F");
        assert_eq!(keys(&app), ["bakemonogatari", "nisemonogatari", "nekomonogatari"]);
        // A search finds every entry by the franchise's name.
        app.press("/monogatari series\n");
        assert_eq!(keys(&app).len(), 3);
        // Quoted as a phrase too.
        app.press("\x1b/\"monogatari series\"\n");
        assert_eq!(keys(&app).len(), 3);
        // Folded, another entry you follow shows too.
        app.press("\x1bF");
        let res = app.ctx.set_status(&app.lib, "nekomonogatari", SeriesStatus::Following, None);
        app.after_write(res);
        assert_eq!(keys(&app), ["nisemonogatari", "nekomonogatari"]);
    }

    /// The status filter and the search work together: `f` (or `3` on the
    /// Series view) changes the filter and the search stays, until Esc.
    #[test]
    fn the_filter_and_the_search_work_together() {
        let (_dir, mut app) = demo_app();
        app.press("3/frieren\n");
        let keys =
            |app: &App| -> Vec<String> { app.series_ix.iter().map(|&i| app.lib.series[i].key.clone()).collect() };
        assert!(keys(&app).contains(&"sousou no frieren".to_string()));
        app.press("f");
        assert_eq!((app.query.as_str(), app.filter), ("frieren", SeriesFilter::Only(SeriesStatus::Following)));
        assert!(keys(&app).iter().all(|k| app.lib.get(k).unwrap().status == SeriesStatus::Following));
        app.press("f");
        assert_eq!(app.filter, SeriesFilter::Only(SeriesStatus::Paused));
        assert!(keys(&app).is_empty(), "no paused series matches");
        // `3` on the Series view does the same; in the search box it is a 3.
        app.press("3");
        assert_eq!((app.query.as_str(), app.filter), ("frieren", SeriesFilter::Only(SeriesStatus::Untracked)));
        app.press("/3");
        assert_eq!(app.query, "frieren3");
        app.press("\x1b");
        assert_eq!((app.query.as_str(), app.filter), ("", SeriesFilter::Only(SeriesStatus::Untracked)));
        app.press("13");
        assert_eq!(app.view, View::Series, "from another tab, 3 just switches");
    }

    /// The search finds a series by any `AniList` title, and `field:value`
    /// tokens filter on studio, year, season, score and hashtag, with or
    /// without words.
    #[test]
    fn search_finds_anilist_titles_and_filters_on_metadata() {
        let (_dir, mut app) = demo_app();
        let keys = |app: &mut App, query: &str| -> Vec<String> {
            app.query = query.into();
            app.refresh_series_rows();
            app.series_ix.iter().map(|&i| app.lib.series[i].key.clone()).collect()
        };
        assert_eq!(keys(&mut app, "delicious in dungeon"), ["dungeon meshi"], "English title");
        assert_eq!(keys(&mut app, "\"in dungeon\""), ["dungeon meshi"], "a quoted phrase");
        assert_eq!(keys(&mut app, "葬送"), ["sousou no frieren"], "native title");
        assert_eq!(keys(&mut app, "studio:madhouse"), ["sousou no frieren"]);
        assert_eq!(keys(&mut app, "studio:\"studio trig\""), ["dungeon meshi"]);
        assert_eq!(keys(&mut app, "year:2023 season:fall"), ["sousou no frieren"]);
        assert_eq!(keys(&mut app, "year<2000"), ["cowboy bebop"]);
        assert_eq!(keys(&mut app, "#ダンジョン"), ["dungeon meshi"]);
        let good = keys(&mut app, "score>=86");
        assert!(good.contains(&"dungeon meshi".to_string()) && good.contains(&"cowboy bebop".to_string()), "{good:?}");
        assert!(good.iter().all(|k| app.lib.get(k).unwrap().meta.as_ref().unwrap().score >= Some(86)));
        // Words and filters together; a filter no series passes leaves nothing.
        assert_eq!(keys(&mut app, "dungeon season:winter"), ["dungeon meshi"]);
        assert!(keys(&mut app, "dungeon season:summer").is_empty());
        // A filter still being typed hides nothing.
        let all = keys(&mut app, "");
        assert_eq!(keys(&mut app, "studio:"), all);
    }

    /// Without a query, the merge picker lists similarly named series first,
    /// then every other one.
    #[test]
    fn merge_targets_put_similar_names_first() {
        let (_dir, app) = demo_app();
        let from = app.lib.series.iter().find(|s| s.title.contains("Frieren S2")).unwrap();
        let targets = app.merge_targets(&from.key, "");
        assert_eq!(targets.len(), app.lib.series.len() - 1, "every other series");
        assert_eq!(targets[0].title, "Sousou no Frieren");
    }

    /// mpv connecting to a playback (started now unless one is): returns
    /// mpv's end of the connection, where the keys sent arrive.
    fn connect_mpv(app: &mut App) -> std::os::unix::net::UnixStream {
        app.playing.get_or_insert_with(|| NowPlaying::starting(1));
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        app.on_player(PlayerEvent::Connected(crate::mpv::Remote::new(ours)));
        theirs
    }

    /// Remote mode: on as soon as mpv connects (with `remote_auto`), keys
    /// go to mpv by their mpv names, Ctrl-C hands them back, `v` hands them
    /// to mpv again, and mpv quitting ends it.
    #[test]
    fn remote_mode_forwards_keys_to_mpv() {
        let (_dir, mut app) = demo_app();
        let mut mpv = std::io::BufReader::new(connect_mpv(&mut app));
        let mut pressed = || crate::fake_mpv::request(&mut mpv).unwrap()["command"][1].as_str().unwrap().to_owned();
        assert!(app.remote_active());

        let view = app.view;
        app.press(" q1");
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        for want in ["SPACE", "q", "1", "Ctrl+RIGHT", "ESC"] {
            assert_eq!(pressed(), want);
        }
        assert_eq!((app.view, &app.popup), (view, &None), "anipv itself saw none of them");

        app.on_player(PlayerEvent::Paused(true));
        assert!(app.playing.as_ref().unwrap().paused);

        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.remote_active());
        assert_eq!(app.popup, None, "ctrl-c only left remote mode");
        app.press("2");
        assert_eq!(app.view, View::Inbox, "keys are anipv's again");
        app.press("v");
        assert!(app.remote_active());
        // Caps Lock (or Ctrl-Shift-C) leaves too, rather than reaching mpv.
        app.on_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert!(!app.remote_active());
        app.press("v");

        app.on_player(PlayerEvent::Exited { played: true, error: None });
        assert!(!app.remote_active());
        app.press("v");
        assert_eq!(app.current_message(), Some("mpv is not running"));
    }

    /// A popup open when mpv connects (or opened from the quit key) keeps
    /// its keys; remote mode takes over once it closes.
    #[test]
    fn remote_mode_waits_for_an_open_popup() {
        let (_dir, mut app) = demo_app();
        app.playing = Some(NowPlaying::starting(1));
        app.press("3/");
        assert_eq!(app.popup, Some(Popup::Search));
        let _mpv = connect_mpv(&mut app);
        assert!(!app.remote_active(), "the search is typed into");
        app.press("frieq");
        assert_eq!(app.query, "frieq", "every key reached the search");
        app.press("\n");
        assert_eq!(app.popup, None);
        assert!(app.remote_active());
    }

    /// A player that stops reading its socket can't freeze the TUI: sending
    /// keys gives up after the write timeout and remote mode ends with an error.
    #[test]
    fn remote_mode_gives_up_on_a_stuck_player() {
        let (_dir, mut app) = demo_app();
        let _never_read = connect_mpv(&mut app);
        let started = Instant::now();
        while app.remote_active() {
            assert!(started.elapsed() < Duration::from_secs(10), "still sending after 10 s");
            app.press("l");
        }
        assert!(
            app.current_message().is_some_and(|m| m.starts_with("could not send l to mpv")),
            "{message:?}",
            message = app.current_message()
        );
    }

    /// mpv quitting (on a `q` sent to it) closes the connection a moment
    /// before the session ends: keys pressed meanwhile go nowhere, quietly
    /// (no error over what playback just recorded), and none reach anipv.
    #[test]
    fn keys_after_mpv_quit_are_dropped_quietly() {
        let (_dir, mut app) = demo_app();
        drop(connect_mpv(&mut app));
        app.say("◐ Sousou no Frieren 29 stopped at 12:00");
        app.press("qq ");
        assert_eq!(app.current_message(), Some("◐ Sousou no Frieren 29 stopped at 12:00"));
        assert!(app.remote_active());
        assert_eq!(app.popup, None, "no quit prompt");
        app.on_player(PlayerEvent::Exited { played: true, error: None });
        assert!(!app.remote_active());
    }

    /// With `remote_auto = false` playback starts with the keys in anipv.
    #[test]
    fn remote_mode_can_wait_for_v() {
        let (_dir, mut app) = demo_app();
        app.ctx.cfg.remote_auto = false;
        let _mpv = connect_mpv(&mut app);
        assert!(!app.remote_active());
        app.press("v");
        assert!(app.remote_active());
    }

    /// mpv quitting before playing shows the specific reason it came with
    /// instead of a generic message, and says the queue is kept when there
    /// is one (an episode played from Detail has nothing to do with it).
    #[test]
    fn early_mpv_exit_shows_the_reported_error() {
        let (_dir, mut app) = demo_app();
        app.queue.push(QueueEntry {
            series: "x".into(),
            item: ItemKey::episode(crate::model::EpNo::new(1)),
            file: "/x.mkv".into(),
        });
        app.playing = Some(NowPlaying::starting(1));
        let error = Some("mpv exited before playing anything (exit status: 1)".into());
        app.on_player(PlayerEvent::Exited { played: false, error });
        assert!(app.playing.is_none());
        assert_eq!(
            app.current_message(),
            Some("mpv exited before playing anything (exit status: 1); the queue is unchanged")
        );

        // No reason given: the generic message.
        app.queue.clear();
        app.playing = Some(NowPlaying::starting(1));
        app.on_player(PlayerEvent::Exited { played: false, error: None });
        assert_eq!(app.current_message(), Some("mpv exited before playing anything"));
    }

    /// docs/anilist.md R18: While a metadata update waits for AniList's rate limit, the header
    /// says so, with the seconds left; once it is over, it doesn't.
    #[test]
    fn rate_limit_waits_show_in_the_header() {
        let (_dir, mut app) = demo_app();
        assert_eq!(app.meta_status(), None);
        app.meta_running = Some(MetaRun::new(STARTUP));
        app.on_msg(Msg::AniListWait(Instant::now() + Duration::from_secs(41)));
        let shown = app.meta_status().unwrap();
        assert!(
            ["metadata · AniList rate limit 41s", "metadata · AniList rate limit 40s"].contains(&shown.as_str()),
            "{shown}"
        );
        app.meta_running.as_mut().unwrap().wait_until = Some(Instant::now());
        assert_eq!(app.meta_status().as_deref(), Some("metadata"));
    }

    /// A start right after a scan (the demo's own) doesn't scan again, but
    /// knows the unreachable roots; with `scan_fresh_for = 0` it scans.
    #[test]
    fn a_fresh_index_is_not_scanned_on_start() {
        let (_dir, mut app) = demo_app();
        let fingerprint = crate::app::scan_fingerprint(&app.ctx.cfg, &app.ctx.cfg.roots[0]);
        let tried = crate::index::db::ScanTry { at: now(), offline: true, fingerprint };
        app.ctx.db.set_scan_tried("Downloads", &tried).unwrap();
        app.scan_unless_fresh();
        assert!(app.scanning.is_none());
        assert_eq!(app.offline_roots, ["Downloads"]);
        assert_eq!(app.current_message(), Some("index scanned just now · r rescans"));
        app.ctx.cfg.scan_fresh_for = 0;
        app.scan_unless_fresh();
        assert!(app.scanning.is_some());
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.scanning.is_some() && Instant::now() < deadline {
            app.wait_msg(Duration::from_millis(50));
        }
        assert!(app.scanning.is_none(), "the scan finished");
    }

    /// What another process stores (here: a scan that finds a new file and an
    /// unreachable root, then a metadata update) is picked up, once, but not
    /// while a scan of the TUI's own runs.
    #[test]
    fn changes_stored_elsewhere_are_picked_up() {
        let (_dir, mut app) = demo_app();
        assert!(!app.pick_up_changes(), "nothing new");
        let roots = &app.ctx.cfg.roots;
        let (dl, anime) = (roots[0].resolved(), roots[1].resolved());
        std::fs::write(dl.join("[Fake] Brand New Show - 01.mkv"), b"x").unwrap();
        let mut other = crate::index::Db::open(&app.ctx.paths.db_file).unwrap();
        crate::app::scan_all(&app.ctx.cfg, &mut other, Some("Downloads"), &|_, _| {}).unwrap();
        std::fs::rename(&anime, anime.with_extension("away")).unwrap();
        crate::app::scan_all(&app.ctx.cfg, &mut other, Some("Anime"), &|_, _| {}).unwrap();
        app.scanning = Some("scanning…".into());
        assert!(!app.pick_up_changes(), "not while scanning");
        app.scanning = None;
        assert!(app.pick_up_changes());
        assert_eq!(app.offline_roots, ["Anime"]);
        assert!(app.lib.get("brand new show").is_some());
        assert_eq!(app.current_message(), Some("1 new show in your inbox (press 2)"));
        assert!(!app.pick_up_changes(), "once");
        other.store_sync(&[], &[], None, &crate::index::db::Fetched::default(), now()).unwrap();
        assert!(app.pick_up_changes());
        assert!(!app.pick_up_changes());
        // The TUI's own metadata reload doesn't hide a scan stored meanwhile.
        std::fs::write(dl.join("[Fake] Another Show - 01.mkv"), b"x").unwrap();
        crate::app::scan_all(&app.ctx.cfg, &mut other, Some("Downloads"), &|_, _| {}).unwrap();
        app.reload_meta();
        assert!(app.pick_up_changes(), "the scan is still new");
        // Nor does a link made elsewhere go unnoticed.
        other.delete_meta("sousou no frieren").unwrap();
        assert!(app.pick_up_changes());
    }

    /// mpv finishing a file and exiting right after reload the library once,
    /// after both messages are handled.
    #[test]
    fn player_messages_reload_once() {
        let (_dir, mut app) = demo_app();
        let next = app.lib.get("sousou no frieren").and_then(Series::next_up).cloned().unwrap();
        let path = next.best_file().unwrap().path.clone();
        app.playing = Some(NowPlaying::starting(1));
        let before = Arc::clone(&app.lib);
        app.on_msg(Msg::Player(PlayerEvent::Ended {
            path,
            pos: 1400.0,
            furthest: 1400.0,
            dur: Some(1420.0),
            eof: true,
        }));
        app.on_msg(Msg::Player(PlayerEvent::Exited { played: true, error: None }));
        assert!(Arc::ptr_eq(&before, &app.lib), "not yet");
        assert!(app.pump());
        let s = app.lib.get("sousou no frieren").unwrap();
        assert!(s.item(&next.key).unwrap().state.is_watched());
        let after = Arc::clone(&app.lib);
        assert!(!app.pump());
        assert!(Arc::ptr_eq(&after, &app.lib), "once");
    }

    /// A scan that panics still ends with `ScanDone`, so the TUI leaves the
    /// "scanning" state and `r` works again.
    #[test]
    fn a_panicking_scan_still_reports_done() {
        let (tx, rx) = channel();
        spawn_scan(&tx, || panic!("boom"));
        match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
            Msg::ScanDone(Err(e)) => assert!(e.contains("boom"), "{e}"),
            other => panic!("{other:?}"),
        }
    }

    /// Roots a scan could not reach are shown until a scan reaches them; a
    /// failed scan says nothing about them.
    #[test]
    fn offline_roots_come_from_the_last_scan() {
        let (_dir, mut app) = demo_app();
        assert!(app.offline_roots.is_empty(), "unknown before the first scan");
        let done = |offline: &[&str]| {
            let offline_roots = offline.iter().map(|r| (*r).to_string()).collect();
            Msg::ScanDone(Ok(ScanSummary { text: "done".into(), offline_roots, failed: false }))
        };
        app.on_msg(done(&["Downloads"]));
        assert_eq!(app.offline_roots, ["Downloads"]);
        app.on_msg(Msg::ScanDone(Err("boom".into())));
        assert_eq!(app.offline_roots, ["Downloads"]);
        app.on_msg(done(&[]));
        assert!(app.offline_roots.is_empty());
    }

    /// A metadata update that panics still ends the busy state (and says why).
    #[test]
    fn a_panicking_metadata_update_still_clears_busy() {
        let (_dir, mut app) = demo_app();
        app.meta_running = Some(MetaRun::new(STARTUP));
        spawn_meta(&app.tx, 0, || panic!("kaboom"));
        assert!(app.wait_for_meta(Duration::from_secs(10)), "still busy");
        let msg = app.current_message().unwrap_or_default();
        assert!(msg.contains("kaboom"), "{msg}");
    }
}
