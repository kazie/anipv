//! TUI state and input handling (rendering lives in `ui.rs`).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use crate::app::{Ctx, Index, QueueEntry, fuzzy_rank, queue_new};
use crate::events::{CachedLog, EventBody, Recorded, now};
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

    /// The next (or previous) tab, wrapping; Detail counts as the first tab.
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

/// What a text input is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPurpose {
    /// Live filter in the Series view.
    Search,
    /// Note for a status change.
    Note {
        /// Series key.
        series: String,
        /// Status being set.
        status: SeriesStatus,
    },
    /// New display title.
    Rename {
        /// Series key.
        series: String,
    },
    /// `AniList` id to link.
    Link {
        /// Series key.
        series: String,
    },
}

/// Modal overlays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Popup {
    /// Key help.
    Help,
    /// Choose a status for a series.
    Status {
        /// Series key.
        series: String,
        /// Highlighted row.
        idx: usize,
    },
    /// Pick a series to merge into.
    Merge(MergePicker),
    /// Single-line text input.
    Input {
        /// What it's for.
        purpose: InputPurpose,
        /// Prompt.
        prompt: String,
        /// Current text.
        text: String,
    },
    /// Quit anipv? (Warns that tracking is lost while mpv is still running.)
    ConfirmQuit,
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

/// What a list row stands for, to find it again after a reload.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowId {
    /// A series, by key.
    Series(String),
    /// A file (or queue entry), by path.
    File(PathBuf),
    /// An item of the series in Detail.
    Item(ItemKey),
}

/// Live playback info.
#[derive(Debug, Clone, PartialEq)]
pub struct NowPlaying {
    /// Current file.
    pub path: Option<PathBuf>,
    /// Display label for the current file.
    pub label: String,
    /// Position (s).
    pub pos: f64,
    /// Duration (s).
    pub dur: Option<f64>,
    /// Files in this mpv session.
    pub total: usize,
    /// Files finished so far.
    pub done: usize,
}

impl NowPlaying {
    /// A session of `total` files that has not started playing yet.
    pub fn starting(total: usize) -> Self {
        Self { path: None, label: "starting mpv…".into(), pos: 0.0, dur: None, total, done: 0 }
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
}

/// The whole TUI state.
pub struct App {
    /// Shared context.
    pub ctx: Ctx,
    /// Current library snapshot.
    pub lib: Arc<Library>,
    /// Active view.
    pub view: View,
    /// View to return to from Detail.
    pub back: View,
    /// Selections per view.
    pub upnext_state: TableState,
    /// Inbox view selection.
    pub inbox_state: TableState,
    /// Series view selection.
    pub series_state: TableState,
    /// Files view selection.
    pub files_state: TableState,
    /// Queue view selection.
    pub queue_state: TableState,
    /// Detail view selection.
    pub detail_state: TableState,
    /// Series filter.
    pub filter: SeriesFilter,
    /// Applied fuzzy query for the Series view.
    pub query: String,
    /// Include paused series in Up next.
    pub include_paused: bool,
    /// What the episode list shows besides episodes on disk.
    pub episodes_shown: EpisodeFilter,
    /// Series shown in Detail.
    pub detail_key: Option<String>,
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
    /// The metadata update running now (with what it is doing, for the
    /// header), and one requested while it ran.
    meta_running: Option<(MetaRequest, &'static str)>,
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
    // Row caches: indices into `lib.series` / `lib.files`, rebuilt by `refresh_rows`.
    upnext_ix: Vec<usize>,
    /// Up next rows with a new episode on disk (the tab's count).
    upnext_ready: usize,
    inbox: Vec<InboxEntry>,
    /// Width in cells of the longest Inbox hint (so the column does not jump while scrolling).
    inbox_hint_width: u16,
    series_ix: Vec<usize>,
    files_ix: Vec<usize>,
    /// Detail rows: indices into the items of the series shown.
    detail_ix: Vec<usize>,
    /// When the last scan progress message was taken (they come in faster than they can be drawn).
    last_progress: Option<Instant>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    /// Create the app and load the library.
    pub fn new(ctx: Ctx) -> Result<Self> {
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
            queue_state: TableState::default().with_selected(0),
            detail_state: TableState::default().with_selected(0),
            filter: SeriesFilter::Active,
            query: String::new(),
            include_paused: false,
            episodes_shown: EpisodeFilter::default(),
            detail_key: None,
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
            tx,
            rx,
        };
        app.warn_all();
        if !completed.is_empty() {
            app.say(format!("✓ completed: {} (all episodes watched)", completed.join(", ")));
        }
        app.refresh_rows();
        if app.upnext_ix.is_empty() {
            app.view = if app.inbox.is_empty() { View::Series } else { View::Inbox };
        }
        Ok(app)
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
        match self.ctx.index_from(&mut self.index) {
            Ok(index) => self.index = index,
            Err(e) => self.fail(format!("reading the index failed: {e:#}")),
        }
        self.reload();
    }

    /// Re-read only the cached metadata (the files are unchanged), then reload.
    fn reload_meta(&mut self) {
        match self.ctx.meta_cache() {
            Ok(meta) => self.index.meta = meta,
            Err(e) => self.fail(format!("reading the index failed: {e:#}")),
        }
        self.reload();
    }

    /// Replay the event log (read again only if its files changed) and
    /// rebuild the library, keeping selections on the same series where possible.
    pub fn reload(&mut self) {
        self.reload_due = false;
        let keep: Vec<(View, RowId)> = View::ALL.iter().filter_map(|&v| Some((v, self.selected_id(v)?))).collect();
        if let Err(e) = self.ctx.log.refresh(&mut self.events) {
            self.fail(format!("reading the event log failed: {e:#}"));
        }
        match self.ctx.library_completed(&mut self.index, &mut self.events) {
            Ok((lib, done)) => {
                self.lib = Arc::new(lib);
                self.warn_all();
                if !done.is_empty() {
                    self.say(format!("✓ completed: {} (all episodes watched)", done.join(", ")));
                }
            }
            Err(e) => self.fail(format!("reload failed: {e:#}")),
        }
        self.resolve_held_keys();
        self.refresh_rows();
        for (view, id) in &keep {
            self.select_id(*view, id);
        }
        self.clamp_selections();
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
            View::Files => &self.files_state,
            View::Queue => &self.queue_state,
            View::Detail => &self.detail_state,
        }
    }

    fn clamp_selections(&mut self) {
        for view in View::ALL {
            let len = self.rows_len(view);
            clamp_cursor(self.state_mut(view), len);
        }
    }

    /// What the highlighted row of `view` stands for.
    fn selected_id(&self, view: View) -> Option<RowId> {
        let i = self.state(view).selected()?;
        let series = |ix: usize| RowId::Series(self.lib.series[ix].key.clone());
        Some(match view {
            View::UpNext => series(*self.upnext_ix.get(i)?),
            View::Inbox => series(self.inbox.get(i)?.index),
            View::Series => series(*self.series_ix.get(i)?),
            View::Files => RowId::File(self.lib.files[*self.files_ix.get(i)?].file.path.clone()),
            View::Queue => RowId::File(self.queue.get(i)?.file.clone()),
            View::Detail => RowId::Item(self.detail_rows().get(i)?.key.clone()),
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
            (View::Files, RowId::File(p)) => self.files_ix.iter().position(|&i| lib.files[i].file.path == *p),
            (View::Queue, RowId::File(p)) => self.queue.iter().position(|q| q.file == *p),
            (View::Detail, RowId::Item(k)) => self.detail_rows().iter().position(|it| it.key == *k),
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
            .map(|h| super::ui::cells(unicode_width::UnicodeWidthStr::width(h.describe(lib).as_str())))
            .max()
            .unwrap_or(0);
        self.upnext_ready = upnext.iter().filter(|&&i| lib.series[i].next_up().is_some()).count();
        (self.upnext_ix, self.inbox, self.files_ix) = (upnext, inbox, files);
        self.clamp_selections();
    }

    /// Recompute only the Series list: all a filter or search change affects,
    /// so typing in the search box stays cheap on a big library.
    fn refresh_series_rows(&mut self) {
        let lib = &self.lib;
        let candidates: Vec<(usize, &Series)> =
            lib.series.iter().enumerate().filter(|(_, s)| !self.query.is_empty() || self.filter.accepts(s)).collect();
        self.series_ix = if self.query.is_empty() {
            candidates.iter().map(|c| c.0).collect()
        } else {
            fuzzy_rank(candidates.iter(), &self.query, |(_, s)| s.search_text().into())
                .into_iter()
                .map(|(c, _)| c.0)
                .collect()
        };
        self.clamp_selections();
    }

    /// Recompute the Detail rows (after a reload, or when the series shown
    /// or the episode list's filters change).
    fn refresh_detail_rows(&mut self) {
        let EpisodeFilter { extras, missing } = self.episodes_shown;
        self.detail_ix = self.detail_series().map(|s| s.ordered_indices(extras, missing)).unwrap_or_default();
    }

    // ---- rows -------------------------------------------------------------

    /// What a running metadata update is doing, for the header.
    pub fn meta_busy(&self) -> Option<&'static str> {
        self.meta_running.as_ref().map(|(_, doing)| *doing)
    }

    /// Counts for the tab bar: Up next rows with something new, inbox size.
    pub fn tab_counts(&self) -> (usize, usize) {
        (self.upnext_ready, self.inbox.len())
    }

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

    fn selected_file(&self) -> Option<&crate::library::IndexedFile> {
        self.files_state.selected().and_then(|i| self.files_ix.get(i)).map(|&i| &self.lib.files[i])
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
        self.queue.iter_mut().map(|q| &mut q.series).for_each(&resolve);
        match &mut self.popup {
            Some(
                Popup::Status { series, .. }
                | Popup::Input {
                    purpose:
                        InputPurpose::Note { series, .. } | InputPurpose::Rename { series } | InputPurpose::Link { series },
                    ..
                },
            ) => resolve(series),
            Some(Popup::Merge(picker)) => resolve(&mut picker.from),
            Some(Popup::Help | Popup::ConfirmQuit | Popup::Input { purpose: InputPurpose::Search, .. }) | None => {}
        }
        let running = self.meta_running.as_mut().map(|(r, _)| r);
        for request in running.into_iter().chain(&mut self.meta_pending) {
            if let MetaRequest::Series(key) = request {
                resolve(key);
            }
        }
    }

    /// The series shown in Detail.
    pub fn detail_series(&self) -> Option<&Series> {
        self.detail_key.as_deref().and_then(|k| self.lib.get(k))
    }

    /// Items in Detail, extras placed right after their episode.
    pub fn detail_rows(&self) -> Vec<&Item> {
        self.detail_rows_in(0..self.detail_ix.len())
    }

    /// Items in Detail, rows `range` only.
    pub fn detail_rows_in(&self, range: Range<usize>) -> Vec<&Item> {
        let Some(s) = self.detail_series() else { return Vec::new() };
        self.detail_ix[clip(&range, self.detail_ix.len())].iter().filter_map(|&i| s.items.get(i)).collect()
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
    pub fn inbox_hint_width(&self) -> u16 {
        self.inbox_hint_width
    }

    /// Highlighted series in the Inbox.
    pub fn selected_inbox(&self) -> Option<&Series> {
        self.inbox_state.selected().and_then(|i| self.inbox.get(i)).map(|e| &self.lib.series[e.index])
    }

    /// Highlighted series in Up next.
    pub fn selected_upnext(&self) -> Option<&Series> {
        self.upnext_state.selected().and_then(|i| self.upnext_ix.get(i)).map(|&i| &self.lib.series[i])
    }

    /// Highlighted series in the Series view.
    pub fn selected_series(&self) -> Option<&Series> {
        self.series_state.selected().and_then(|i| self.series_ix.get(i)).map(|&i| &self.lib.series[i])
    }

    /// Series under the cursor in whatever view is active.
    pub fn focused_series(&self) -> Option<&Series> {
        match self.view {
            View::UpNext => self.selected_upnext(),
            View::Inbox => self.selected_inbox(),
            View::Series => self.selected_series(),
            View::Detail => self.detail_series(),
            View::Files => self.selected_file().and_then(|f| self.lib.get(&f.series)),
            View::Queue => {
                self.queue_state.selected().and_then(|i| self.queue.get(i)).and_then(|q| self.lib.get(&q.series))
            }
        }
    }

    /// True if the item is queued.
    pub fn is_queued(&self, series: &str, item: &ItemKey) -> bool {
        self.queue.iter().any(|q| q.series == series && &q.item == item)
    }

    // ---- actions ----------------------------------------------------------

    fn toggle_queue(&mut self, series: &str, item: &Item) {
        if let Some(i) = self.queue.iter().position(|q| q.series == series && q.item == item.key) {
            self.queue.remove(i);
        } else if let Some(f) = item.best_file() {
            self.queue.push(QueueEntry { series: series.to_string(), item: item.key.clone(), file: f.path.clone() });
        } else {
            self.say("not on disk");
        }
        self.clamp_selections();
    }

    /// Queue the next new episode(s) of a series that aren't queued yet.
    fn queue_next_of(&mut self, key: &str, all: bool) {
        let Some(s) = self.lib.get(key) else { return };
        let title = s.title.clone();
        // A multi-episode file is queued once: skip items whose file already is.
        let queued_file = |item: &ItemKey| {
            s.item(item).and_then(Item::best_file).is_some_and(|f| self.queue.iter().any(|q| q.file == f.path))
        };
        let entries =
            queue_new(s, if all { usize::MAX } else { 1 }, |item| self.is_queued(key, item) || queued_file(item));
        if entries.is_empty() {
            self.say(format!("nothing new to queue for {title}"));
            return;
        }
        let n = entries.len();
        self.queue.extend(entries);
        self.clamp_selections();
        self.say(format!("queued {n} from {title} ({} in queue)", self.queue.len()));
    }

    /// Report a failed write, then reload to show the new state.
    fn after_write(&mut self, res: Result<Recorded>) {
        self.add_recorded(res);
        self.reload();
    }

    /// Keep events just recorded (without reading the log again), or report
    /// that writing them failed.
    fn add_recorded(&mut self, res: Result<Recorded>) {
        match res {
            Ok(recorded) => self.ctx.log.merge(&mut self.events, recorded),
            Err(e) => self.fail(format!("{}: {e:#}", crate::app::WRITE_FAILED)),
        }
    }

    fn record(&mut self, bodies: Vec<EventBody>) {
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
        self.say(format!("marked {} episode(s) watched", keys.len()));
        let res = self.ctx.mark(s, &keys, true);
        self.after_write(res);
    }

    /// `p`: play the queue, or, with nothing queued, the next episode of the
    /// series under the cursor.
    fn play_key(&mut self) {
        let before = self.queue.len();
        if self.queue.is_empty() && self.playing.is_none() {
            match self.focused_series().map(|s| s.key.clone()) {
                Some(key) => self.queue_next_of(&key, false),
                None => self.say("queue is empty"),
            }
            if self.queue.is_empty() {
                return;
            }
        }
        if !self.play_queue() {
            // mpv did not start: take back what this press queued.
            self.queue.truncate(before);
            self.clamp_selections();
        }
    }

    /// Start mpv on the whole queue, in order. True if it started.
    pub fn play_queue(&mut self) -> bool {
        if self.queue.is_empty() && self.playing.is_none() {
            self.say("queue is empty");
            return false;
        }
        let files = self.ctx.play_files(&self.lib, &self.queue);
        self.launch(&files)
    }

    /// Start mpv on just this item, leaving the queue as it is.
    pub fn play_item(&mut self, series: &str, item: &Item) {
        let Some(file) = item.best_file() else {
            self.say("not on disk");
            return;
        };
        let entry = QueueEntry { series: series.to_string(), item: item.key.clone(), file: file.path.clone() };
        let files = self.ctx.play_files(&self.lib, std::slice::from_ref(&entry));
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
                // Entries leave the queue once mpv is actually playing each file
                // (see `on_player`), so an mpv that quits early or a file that
                // fails to open loses nothing.
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

    /// Copy the mpv command for the queue to the clipboard (or show it).
    fn yank(&mut self) {
        if self.queue.is_empty() {
            self.say("queue is empty");
            return;
        }
        let files = self.ctx.play_files(&self.lib, &self.queue);
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
                let before: HashSet<String> = self.inbox_rows().iter().map(|(s, _)| s.key.clone()).collect();
                self.reload_index();
                let arrived = self.inbox_rows().iter().filter(|(s, _)| !before.contains(&s.key)).count();
                match res {
                    Ok(_) if arrived > 0 => self.say(format!(
                        "{arrived} new show{} in your inbox (press 2)",
                        if arrived == 1 { "" } else { "s" }
                    )),
                    Ok(done) => self.say(done.text),
                    Err(e) => self.fail(format!("scan failed: {e}")),
                }
            }
            Msg::Player(ev) => self.on_player(ev),
            Msg::MetaProgress(what) => {
                if let Some((_, doing)) = &mut self.meta_running {
                    *doing = what;
                }
            }
            Msg::Meta(mut res, epoch) => {
                let request = self.meta_running.take().map_or(STARTUP, |(r, _)| r);
                // Keep everything except what this sync found for series
                // relinked while it ran; their own refresh is queued.
                let stale = |series: &String| self.relinked.get(series).is_some_and(|&at| at > epoch);
                res.rows.retain(|m| !stale(&m.series));
                res.attempts.retain(|a| !stale(&a.series));
                self.relinked.retain(|_, at| *at > epoch);
                if let Err(e) = self.ctx.store_sync(&res) {
                    self.fail(format!("could not cache metadata: {e:#}"));
                }
                if !res.rows.is_empty()
                    || !res.attempts.is_empty()
                    || !res.prequels.is_empty()
                    || !res.checked.is_empty()
                {
                    self.reload_meta();
                }
                if let Some(msg) = self.meta_summary(&request, &res) {
                    self.say(msg);
                }
                if let Some(next) = self.meta_pending.take() {
                    self.start_meta(next);
                }
            }
            // Always followed by `Meta`, which ends the busy state.
            Msg::MetaError(e) => self.fail(e),
        }
        true
    }

    fn on_player(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Started { path } => {
                self.queue.retain(|q| q.file != path);
                self.clamp_selections();
                let (series, items) = self.lib.identify(&self.ctx.cfg, &path);
                let label = self.lib.label(&series, &items);
                if let Some(p) = &mut self.playing {
                    p.path = Some(path);
                    p.label = label;
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
            PlayerEvent::Ended { path, pos, dur, eof } => {
                match self.ctx.record_playback(&self.lib, &path, pos, dur, eof) {
                    Ok((series, items, outcome, recorded)) => {
                        self.ctx.log.merge(&mut self.events, recorded);
                        let label = self.lib.label(&series, &items);
                        match outcome {
                            Outcome::Watched => self.say(format!("✓ watched {label}")),
                            Outcome::Partial { pos, .. } => {
                                self.say(format!("◐ {label} stopped at {}", crate::fmt::clock(pos)));
                            }
                            Outcome::Nothing => {}
                        }
                    }
                    Err(e) => self.fail(format!("could not record playback: {e:#}")),
                }
                if let Some(p) = &mut self.playing {
                    p.done += 1;
                }
                // mpv exiting usually follows at once: one reload for both.
                self.reload_due = true;
            }
            PlayerEvent::Exited { played, error } => {
                self.playing = None;
                match (played, error) {
                    (false, error) => {
                        let why = error.as_deref().unwrap_or("mpv exited before playing anything");
                        self.fail(format!("{why}; the queue is unchanged"));
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
        any
    }

    /// Wait up to `timeout` for one background message.
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
        while self.meta_busy().is_some() && Instant::now() < deadline {
            self.wait_msg(Duration::from_millis(50));
        }
        self.meta_busy().is_none()
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
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // Like `q`, but a second Ctrl-C at the prompt quits anyway.
                KeyCode::Char('c') => self.request_quit(),
                KeyCode::Char('r') => self.start_meta(MetaRequest::All),
                _ => {}
            }
            return;
        }
        if self.popup.is_some() {
            self.on_popup_key(key);
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

    /// Keys that work in every view. Returns true if handled.
    fn on_global_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => {
                self.popup = Some(Popup::Help);
            }
            KeyCode::Char(c @ '1'..='5') => {
                self.view = View::TABS[(c as u8 - b'1') as usize];
            }
            KeyCode::Tab => self.view = self.view.cycle(true),
            KeyCode::BackTab => self.view = self.view.cycle(false),
            KeyCode::Char('r') => {
                self.start_scan();
            }
            KeyCode::Char('M') => match self.focused_series().map(|s| s.key.clone()) {
                Some(key) => self.start_meta(MetaRequest::Series(key)),
                None => self.say("no series under the cursor"),
            },
            KeyCode::Char('p') => self.play_key(),
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
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
            }
            KeyCode::PageDown => {
                self.move_cursor(15);
            }
            KeyCode::PageUp => {
                self.move_cursor(-15);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.move_cursor(isize::MIN / 2);
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.move_cursor(isize::MAX / 2);
            }
            _ => return false,
        }
        true
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.rows_len(self.view);
        step_cursor(self.state_mut(self.view), len, delta);
    }

    /// After the episode list's filters change, put the cursor back on
    /// `target`, or on the nearest row still shown (later rows first).
    fn reselect_detail(&mut self, target: Option<&ItemKey>) {
        self.refresh_detail_rows();
        let idx = self.detail_series().zip(target).and_then(|(s, t)| {
            // The rows are these items with some left out: find each by index.
            let all = s.ordered_indices(self.episodes_shown.extras, true);
            let row_of: HashMap<usize, usize> = self.detail_ix.iter().enumerate().map(|(n, &i)| (i, n)).collect();
            let shown = |i: &usize| row_of.get(i).copied();
            let at = all.iter().position(|&i| s.items[i].key == *t)?;
            all[at..].iter().find_map(shown).or_else(|| all[..at].iter().rev().find_map(shown))
        });
        self.detail_state.select(idx.or(Some(0)));
        self.clamp_selections();
    }

    fn open_detail(&mut self, key: String) {
        if self.view != View::Detail {
            self.back = self.view;
        }
        self.detail_key = Some(key);
        self.refresh_detail_rows();
        let first = self.detail_series().map(|s| {
            let target = s.next_up().map(|i| &i.key);
            self.detail_rows().iter().position(|i| Some(&i.key) == target).unwrap_or(0)
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
            KeyCode::Char(' ') => return self.queue_next_of(&key_s, false),
            KeyCode::Enter | KeyCode::Right => return self.open_detail(key_s),
            _ => return self.series_actions(key, key_s),
        };
        self.say(msg);
        let res = self.ctx.set_status(&key_s, status, None);
        self.after_write(res);
    }

    fn on_upnext_key(&mut self, key: KeyEvent) {
        // Works on an empty list too: that's when paused series are wanted most.
        if key.code == KeyCode::Char('P') {
            self.include_paused = !self.include_paused;
            self.say(if self.include_paused { "showing paused series" } else { "hiding paused series" });
            self.refresh_rows();
            return;
        }
        let Some(sel) = self.selected_upnext().map(|s| s.key.clone()) else { return };
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_detail(sel),
            KeyCode::Char(' ') => {
                self.queue_next_of(&sel, false);
                self.move_cursor(1);
            }
            KeyCode::Char('a') => self.queue_next_of(&sel, true),
            KeyCode::Char('w') => {
                let next = self.lib.get(&sel).and_then(|s| s.next_up().cloned());
                if let Some(item) = next {
                    self.say(format!("marked {} watched", item.key.describe()));
                    self.toggle_watched(&sel, std::slice::from_ref(&item));
                }
            }
            _ => {}
        }
    }

    fn on_series_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('/') => {
                self.popup = Some(Popup::Input {
                    purpose: InputPurpose::Search,
                    prompt: "search".into(),
                    text: self.query.clone(),
                });
                return;
            }
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.refresh_series_rows();
                return;
            }
            KeyCode::Char('f') => {
                self.filter = self.filter.next();
                self.series_state.select(Some(0));
                self.refresh_series_rows();
                return;
            }
            _ => {}
        }
        let Some(s) = self.selected_series() else { return };
        let key_s = s.key.clone();
        match key.code {
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_detail(key_s),
            KeyCode::Char(' ') => self.queue_next_of(&key_s, false),
            _ => self.series_actions(key, key_s),
        }
    }

    /// Keys that act on a series from Series or Detail.
    fn series_actions(&mut self, key: KeyEvent, series: String) {
        match key.code {
            KeyCode::Char('m') => self.popup = Some(Popup::Merge(MergePicker::new(self, &series))),
            KeyCode::Char('R') => {
                let current = self.lib.get(&series).map(|s| s.title.clone()).unwrap_or_default();
                self.popup = Some(Popup::Input {
                    purpose: InputPurpose::Rename { series },
                    prompt: "title".into(),
                    text: current,
                });
            }
            KeyCode::Char('L') => {
                let current = self
                    .lib
                    .get(&series)
                    .and_then(super::super::library::Series::anilist_id)
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                self.popup = Some(Popup::Input {
                    purpose: InputPurpose::Link { series },
                    prompt: "AniList id (empty to unlink)".into(),
                    text: current,
                });
            }
            KeyCode::Char('U') => {
                let aliases = self.lib.get(&series).map(|s| s.aliases.clone()).unwrap_or_default();
                if aliases.is_empty() {
                    self.say("nothing merged into this series");
                } else {
                    self.say(format!("split off {} merged name(s)", aliases.len()));
                    let res = self.ctx.unmerge(&self.lib, aliases.iter().map(String::as_str));
                    self.after_write(res);
                }
            }
            _ => {}
        }
    }

    fn on_files_key(&mut self, key: KeyEvent) {
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
                if let Some(i) = self.queue.iter().position(|q| q.file == path) {
                    self.queue.remove(i);
                } else if let Some(first) = items.first() {
                    self.queue.push(QueueEntry { series: series.clone(), item: first.key.clone(), file: path });
                }
                self.clamp_selections();
                self.move_cursor(1);
            }
            KeyCode::Char('w') => self.toggle_watched(&series, &items),
            _ => {}
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
                self.clamp_selections();
            }
            KeyCode::Char('K') if i > 0 => {
                self.queue.swap(i, i - 1);
                self.queue_state.select(Some(i - 1));
            }
            KeyCode::Char('J') if i + 1 < self.queue.len() => {
                self.queue.swap(i, i + 1);
                self.queue_state.select(Some(i + 1));
            }
            KeyCode::Char('c') => {
                self.queue.clear();
                self.clamp_selections();
            }
            KeyCode::Enter => {
                self.play_queue();
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
        let item: Option<&Item> =
            self.detail_state.selected().and_then(|i| lib.get(&series)?.items.get(*self.detail_ix.get(i)?));
        match (key.code, item) {
            (KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left, _) => self.view = self.back,
            (KeyCode::Char('x'), _) => {
                let shown = self.episodes_shown;
                let s = lib.get(&series);
                let rows: Vec<&Item> = self.detail_ix.iter().filter_map(|&i| s?.items.get(i)).collect();
                self.episodes_shown.extras = !shown.extras;
                // Keep the cursor on the same item if it is still shown, else
                // on the nearest row above it that is (hiding hides only extras).
                let upto = self.detail_state.selected().map_or(0, |i| (i + 1).min(rows.len()));
                let target = rows[..upto].iter().rev().find(|it| !shown.extras || it.key.kind != ItemKind::Extra);
                self.reselect_detail(target.map(|it| &it.key));
            }
            (KeyCode::Char('d'), item) => {
                self.episodes_shown.missing = !self.episodes_shown.missing;
                self.reselect_detail(item.map(|it| &it.key));
            }
            (KeyCode::Char(' '), Some(it)) => {
                self.toggle_queue(&series, it);
                self.move_cursor(1);
            }
            (KeyCode::Char('w'), Some(it)) => self.toggle_watched(&series, std::slice::from_ref(it)),
            (KeyCode::Char('W'), Some(it)) if it.key.kind == ItemKind::Episode => {
                self.watched_through(&series, &it.key);
            }
            (KeyCode::Char('a'), _) => self.queue_next_of(&series, true),
            // Play from here: just this item, whatever is queued stays queued
            // (and an item not on disk says so rather than play something else).
            (KeyCode::Enter, Some(it)) => self.play_item(&series, it),
            _ => self.series_actions(key, series),
        }
    }

    fn on_popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.popup.take() else { return };
        match popup {
            Popup::Help => {}
            Popup::ConfirmQuit => {
                if matches!(key.code, KeyCode::Char('y' | 'q') | KeyCode::Enter) {
                    self.quit = true;
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
                        self.popup = Some(Popup::Input {
                            purpose: InputPurpose::Note { series, status: st },
                            prompt: format!("{st} — note (optional)"),
                            text: String::new(),
                        });
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
                    KeyCode::Down => picker.step(1),
                    KeyCode::Up => picker.step(-1),
                    KeyCode::Backspace => {
                        picker.query.pop();
                        picker.refresh(self, None);
                    }
                    KeyCode::Char(c) => {
                        picker.query.push(c);
                        picker.refresh(self, None);
                    }
                    _ => {}
                }
                self.popup = Some(Popup::Merge(picker));
            }
            Popup::Input { purpose, prompt, mut text } => match key.code {
                KeyCode::Esc => {
                    if purpose == InputPurpose::Search {
                        self.query.clear();
                        self.refresh_series_rows();
                    }
                }
                KeyCode::Enter => self.submit_input(purpose, text),
                KeyCode::Backspace => {
                    text.pop();
                    self.live_input(&purpose, &text);
                    self.popup = Some(Popup::Input { purpose, prompt, text });
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    self.live_input(&purpose, &text);
                    self.popup = Some(Popup::Input { purpose, prompt, text });
                }
                _ => self.popup = Some(Popup::Input { purpose, prompt, text }),
            },
        }
    }

    /// Candidate series to merge `from` into.
    pub fn merge_targets(&self, from: &str, query: &str) -> Vec<&Series> {
        let from_ix = self.lib.index_of(from);
        let others = self.lib.series.iter().enumerate().filter(move |(i, _)| Some(*i) != from_ix).map(|(_, s)| s);
        if query.is_empty() {
            let from_title = self.lib.get(from).map(|s| s.title.clone()).unwrap_or_default();
            // Suggest similarly named series first.
            let first_word = from_title.split_whitespace().next().unwrap_or("").to_string();
            let mut v: Vec<&Series> =
                fuzzy_rank(others.clone(), &first_word, |s| s.title.as_str().into()).into_iter().map(|x| x.0).collect();
            if v.is_empty() {
                v = others.collect();
            }
            return v;
        }
        fuzzy_rank(others, query, |s| s.search_text().into()).into_iter().map(|x| x.0).collect()
    }

    fn live_input(&mut self, purpose: &InputPurpose, text: &str) {
        if *purpose == InputPurpose::Search {
            self.query = text.to_string();
            self.series_state.select(Some(0));
            self.refresh_series_rows();
        }
    }

    fn submit_input(&mut self, purpose: InputPurpose, text: String) {
        match purpose {
            InputPurpose::Search => {
                self.query = text;
                self.refresh_series_rows();
            }
            InputPurpose::Note { series, status } => {
                let note = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                self.set_status(&series, status, note);
            }
            InputPurpose::Rename { series } => {
                let t = text.trim().to_string();
                if !t.is_empty() {
                    self.record(vec![EventBody::Title { series, title: t }]);
                }
            }
            InputPurpose::Link { series } => {
                // Empty or 0 unlinks.
                let text = if text.trim().is_empty() { "0" } else { text.trim() };
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
        let title = self.lib.get(series).map_or_else(|| series.to_string(), |s| s.title.clone());
        self.say(format!("{title} → {status}"));
        let res = self.ctx.set_status(series, status, note);
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
            let parts: Vec<String> = out
                .iter()
                .map(|r| match &r.stats {
                    Ok(s) if s.new > 0 || s.gone > 0 => {
                        format!("{root}: +{new} −{gone}", root = r.root, new = s.new, gone = s.gone)
                    }
                    Ok(_) => format!("{}: no changes", r.root),
                    Err(e) => format!("{}: {e}", r.root),
                })
                .collect();
            // Found here rather than on the UI thread: checking a root may
            // block on a network mount.
            let offline_roots = out.iter().filter(|r| r.stats.is_err()).map(|r| r.root.clone()).collect();
            Ok(ScanSummary { text: parts.join(" · "), offline_roots })
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
        self.meta_running = Some((request, "metadata"));
        let (lib, cache, tx, epoch) =
            (Arc::clone(&self.lib), self.ctx.paths.cache_dir.clone(), self.tx.clone(), self.meta_epoch);
        spawn_meta(&self.tx, epoch, move || {
            // `opts` says whether the network may be used (see `plan`).
            let http = Some(&crate::meta::anilist::Ureq as &dyn crate::meta::anilist::Http);
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
                (!res.rows.is_empty()).then(|| format!("metadata: updated {} series", res.rows.len()))
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
                    None => format!("{}: no AniList match (link one with L)", s.title),
                })
            }
        }
    }
}

/// How long a status line message is shown.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(6);

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
    let sel = state.selected().unwrap_or(0);
    state.select(len.checked_sub(1).map(|last| sel.min(last)));
}

/// Move the cursor by `delta` rows, staying inside a list of `len` rows.
fn step_cursor(state: &mut TableState, len: usize, delta: isize) {
    let cur = state.selected().unwrap_or(0);
    state.select(len.checked_sub(1).map(|last| cur.saturating_add_signed(delta).min(last)));
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

/// Scan progress is shown at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// What a metadata update covers: [`STARTUP`], everything (`ctrl-r`) or one
/// series (`M`).
pub use crate::meta::Request as MetaRequest;

/// The automatic update on start: only what is missing or stale.
pub const STARTUP: MetaRequest = MetaRequest::Auto { max_age: crate::meta::STARTUP_MAX_AGE };

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

    fn ctrl_c(app: &mut App) {
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    }

    /// `q` and Ctrl-C ask first, playing or not; at the prompt `q`, `y`,
    /// Enter or a second Ctrl-C quits, any other key cancels.
    #[test]
    fn quitting_asks_first() {
        let (_dir, mut app) = demo_app();
        for playing in [true, false] {
            app.playing = playing.then(|| NowPlaying::starting(1));
            for quit in ["q", "y", "\n", "ctrl-c"] {
                for first in ["q", "ctrl-c"] {
                    let press = |app: &mut App, k: &str| if k == "ctrl-c" { ctrl_c(app) } else { app.press(k) };
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

    /// mpv quitting before playing shows the specific reason it came with
    /// instead of a generic message.
    #[test]
    fn early_mpv_exit_shows_the_reported_error() {
        let (_dir, mut app) = demo_app();
        app.playing = Some(NowPlaying::starting(1));
        let error = Some("mpv exited before playing anything (exit status: 1)".into());
        app.on_player(PlayerEvent::Exited { played: false, error });
        assert!(app.playing.is_none());
        assert_eq!(
            app.current_message(),
            Some("mpv exited before playing anything (exit status: 1); the queue is unchanged")
        );

        // No reason given: the generic message.
        app.playing = Some(NowPlaying::starting(1));
        app.on_player(PlayerEvent::Exited { played: false, error: None });
        assert_eq!(app.current_message(), Some("mpv exited before playing anything; the queue is unchanged"));
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
        app.on_msg(Msg::Player(PlayerEvent::Ended { path, pos: 1400.0, dur: Some(1420.0), eof: true }));
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
            Msg::ScanDone(Ok(ScanSummary { text: "done".into(), offline_roots }))
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
        app.meta_running = Some((STARTUP, "metadata"));
        spawn_meta(&app.tx, 0, || panic!("kaboom"));
        assert!(app.wait_for_meta(Duration::from_secs(10)), "still busy");
        let msg = app.current_message().unwrap_or_default();
        assert!(msg.contains("kaboom"), "{msg}");
    }
}
