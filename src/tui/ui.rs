//! Rendering: `App` state → frame. Drawing only writes back each list's
//! scroll offset.

use std::collections::HashSet;
use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Gauge, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
};

use super::app::{App, DetailRow, FilesMode, MergePicker, NowPlaying, Popup, Severity, View};
use crate::fmt::{ago, ago_opt, clock};
use crate::folders::Entry;
use crate::library::Hint;
use crate::library::{Item, Library, Series};
use crate::model::{ItemKey, ItemKind, SeriesStatus, WatchState};

const ACCENT: Color = Color::Magenta;

/// What marks AniList's facts (its id, its count, its name for a series): a
/// one-column text symbol, not an emoji.
const ANILIST_MARK: &str = "ⓐ";

/// AniList's blue, for AniList ids: the nearest of the 256 colours (as the
/// others here), not truecolor, which not every terminal has.
const ANILIST: Color = Color::Indexed(39);

fn status_color(s: SeriesStatus) -> Color {
    match s {
        SeriesStatus::Following => Color::Green,
        SeriesStatus::Paused => Color::Yellow,
        SeriesStatus::Dropped => Color::Red,
        SeriesStatus::Completed => Color::Cyan,
        SeriesStatus::Untracked | SeriesStatus::Skipped => Color::DarkGray,
    }
}

fn state_style(st: &WatchState) -> Style {
    match st {
        WatchState::Watched { .. } => Style::new().fg(Color::Green),
        WatchState::Started { .. } => Style::new().fg(Color::Yellow),
        WatchState::Unwatched => Style::new().fg(Color::DarkGray),
    }
}

fn highlight() -> Style {
    Style::new().bg(Color::Indexed(236)).add_modifier(Modifier::BOLD)
}

/// Draw the whole UI.
pub fn draw(f: &mut Frame, app: &mut App) {
    let playing_h = u16::from(app.playing.is_some());
    let [header, body, now_playing, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(playing_h),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, app, header);
    match app.view {
        View::UpNext => draw_upnext(f, app, body),
        View::Inbox => draw_inbox(f, app, body),
        View::Series => draw_series(f, app, body),
        View::Files => draw_files(f, app, body),
        View::Queue => draw_queue(f, app, body),
        View::Detail => draw_detail(f, app, body),
    }
    draw_playing(f, app, now_playing);
    draw_footer(f, app, footer);

    match &mut app.popup {
        Some(Popup::Help { scroll, fits }) => draw_help(f, scroll, fits),
        Some(Popup::Status { series, idx }) => draw_status_popup(f, &app.lib, series, *idx),
        Some(Popup::Merge(picker)) => draw_merge(f, &app.lib, picker),
        Some(Popup::Search) => draw_search(f, app.search_query()),
        Some(Popup::Input { purpose, text, .. }) => draw_input(f, &purpose.prompt(), text),
        Some(Popup::ConfirmQuit) => draw_confirm(f, app.playing.is_some()),
        Some(Popup::CatchUp { prompt, .. }) => draw_catch_up(f, prompt),
        Some(Popup::Unmerge { names, .. }) => draw_unmerge(f, names),
        None => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" anipv ", Style::new().fg(Color::Black).bg(ACCENT).bold()), Span::raw(" ")];
    let active = app.tab();
    let (upnext_ready, inbox_len) = app.tab_counts();
    for (i, v) in View::TABS.iter().enumerate() {
        let (key, title) = (i + 1, v.title());
        let count = match v {
            View::Queue if !app.queue.is_empty() => Some(app.queue.len()),
            View::UpNext => Some(upnext_ready),
            View::Inbox if inbox_len > 0 => Some(inbox_len),
            _ => None,
        };
        let label = match count {
            Some(n) => format!("{key} {title}({n})"),
            None => format!("{key} {title}"),
        };
        let style =
            if *v == active { Style::new().fg(ACCENT).bold().underlined() } else { Style::new().fg(Color::Gray) };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("  "));
    }
    f.render_widget(Line::from(spans), area);

    let mut right = Vec::new();
    if let Some(s) = &app.scanning {
        right.push(Span::styled(format!("⟳ {s} "), Style::new().fg(Color::Yellow)));
    }
    if let Some(m) = app.meta_status() {
        right.push(Span::styled(format!("⟳ {m} "), Style::new().fg(Color::Blue)));
    }
    if !app.offline_roots.is_empty() {
        let names = app.offline_roots.join(",");
        right.push(Span::styled(format!("✗ {names} offline "), Style::new().fg(Color::Red)));
    }
    right.push(Span::styled(format!("{device} ", device = app.ctx.log.device()), Style::new().fg(Color::DarkGray)));
    f.render_widget(Line::from(right).alignment(Alignment::Right), area);
}

fn block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(Color::DarkGray)).title(title)
}

/// The common table look: accent header, rounded block, highlight bar.
fn list_table<'a, const N: usize>(
    rows: Vec<Row<'a>>,
    widths: [Constraint; N],
    header: Option<[&'a str; N]>,
    title: impl Into<Line<'static>>,
) -> Table<'a> {
    let t = highlighted(Table::new(rows, widths).block(block(title)));
    match header {
        Some(h) => t.header(Row::new(h).style(Style::new().fg(ACCENT).bold())),
        None => t,
    }
}

/// The highlight bar every list uses.
fn highlighted(t: Table<'_>) -> Table<'_> {
    t.row_highlight_style(highlight()).highlight_symbol("▌").highlight_spacing(HighlightSpacing::Always)
}

/// Centered dim text shown inside an empty list.
fn empty_hint(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    let hint = Paragraph::new(lines).alignment(Alignment::Center).dark_gray();
    f.render_widget(hint, area.inner(ratatui::layout::Margin::new(2, 2)));
}

/// The slice of a list that is on screen.
struct Window {
    /// Rows to build.
    range: Range<usize>,
    /// Highlighted row, relative to `range`.
    selected: Option<usize>,
}

impl Window {
    /// The `height` rows of `len` that show with `state`'s cursor visible, scrolling
    /// from its offset as little as possible. Lists can hold thousands of rows,
    /// so callers build only these.
    ///
    /// The offset never goes past the last full screen, so a list that shrank
    /// (or a taller terminal) fills the view instead of leaving blank rows below.
    fn of(state: &TableState, len: usize, height: usize) -> Self {
        let height = height.max(1);
        let selected = state.selected().map(|s| s.min(len.saturating_sub(1)));
        let mut offset = state.offset().min(len.saturating_sub(height));
        if let Some(sel) = selected {
            // `sel < len`, so this stays within the cap above.
            offset = offset.min(sel).max((sel + 1).saturating_sub(height));
        }
        Self { range: offset..(offset + height).min(len), selected: selected.map(|s| s.saturating_sub(offset)) }
    }

    /// The window of a bordered table in `area` (minus a header row, if any).
    fn of_table(state: &TableState, len: usize, area: Rect, header: bool) -> Self {
        Self::of(state, len, usize::from(area.height.saturating_sub(2 + u16::from(header))))
    }

    /// Render `table` (built from only this window's rows) and remember the scroll offset in `state`.
    fn render(&self, f: &mut Frame, table: Table, area: Rect, state: &mut TableState) {
        let mut shown = TableState::default().with_selected(self.selected);
        f.render_stateful_widget(table, area, &mut shown);
        *state.offset_mut() = self.range.start;
    }
}

/// What is queued, for a per-frame membership test per row.
fn queued_set(app: &App) -> HashSet<(&str, &ItemKey)> {
    app.queue.iter().map(|q| (q.series.as_str(), &q.item)).collect()
}

/// Audience score (0–100) for a table cell: green when well liked, dim when
/// not, a dash when unknown.
fn score_span(s: &Series) -> Span<'static> {
    match s.meta.as_ref().and_then(|m| m.score) {
        Some(n) if n >= 75 => Span::styled(n.to_string(), Style::new().fg(Color::Green)),
        Some(n) if n < 60 => Span::styled(n.to_string(), Style::new().dark_gray()),
        Some(n) => Span::raw(n.to_string()),
        None => Span::styled("–", Style::new().dark_gray()),
    }
}

/// A member count, short: `843`, `12k`, `1.2M`.
fn short_count(n: u32) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..999_500 => format!("{k}k", k = (n + 500) / 1_000),
        _ => {
            let tenths = (n + 50_000) / 100_000;
            if tenths.is_multiple_of(10) {
                format!("{m}M", m = tenths / 10)
            } else {
                format!("{m}.{tenth}M", m = tenths / 10, tenth = tenths % 10)
            }
        }
    }
}

fn airing_span(s: &Series, ts: i64) -> Span<'static> {
    match s.airing(ts) {
        Some((text, true)) => Span::styled(text, Style::new().fg(Color::Blue)),
        Some((text, false)) => Span::styled(text, Style::new().fg(Color::DarkGray)),
        None => Span::raw(""),
    }
}

/// `▶` if queued, otherwise the watch-state glyph.
fn mark_span(queued: bool, st: &WatchState) -> Span<'static> {
    if queued { Span::styled("▶", Style::new().fg(ACCENT).bold()) } else { Span::styled(st.glyph(), state_style(st)) }
}

fn draw_upnext(f: &mut Frame, app: &mut App, area: Rect) {
    let ts = app.now();
    let (len, queued_set) = (app.rows_len(View::UpNext), queued_set(app));
    let window = Window::of_table(&app.upnext_state, len, area, true);
    let rows: Vec<Row> = app
        .upnext_rows_in(window.range.clone())
        .iter()
        .map(|s| {
            let new = s.new_episodes();
            let next = new.first().copied();
            let queued = new.iter().filter(|i| queued_set.contains(&(s.key.as_str(), &i.key))).count();
            let next_txt = next.map_or_else(|| "—".into(), |i| i.key.describe());
            let started = next.and_then(|i| i.state.percent()).map(|p| format!(" ◐{p}")).unwrap_or_default();
            let title_style =
                if s.status == SeriesStatus::Paused { Style::new().fg(Color::Yellow) } else { Style::new() };
            let new_cell = match (new.len(), queued) {
                (0, _) => Cell::from("0").dark_gray(),
                (n, 0) => Cell::from(n.to_string()).green().bold(),
                (n, q) => Cell::from(Line::from(vec![
                    Span::raw(n.to_string()).green().bold(),
                    Span::raw(format!(" +{q}▶")).magenta(),
                ])),
            };
            let next_style = if next.is_some() { Style::new().bold() } else { Style::new().dark_gray() };
            let title = titled(app, s, title_style);
            Row::new(vec![
                Cell::from(title),
                Cell::from(format!("{next_txt}{started}")).style(next_style),
                new_cell,
                Cell::from(s.progress()),
                Cell::from(ago_opt(s.last_activity(), ts)).dark_gray(),
                Cell::from(airing_span(s, ts)),
            ])
        })
        .collect();
    let title = Line::from(vec![
        Span::raw(" Up next "),
        Span::styled(if app.include_paused { "· incl. paused " } else { "" }, Style::new().fg(Color::Yellow)),
    ]);
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(20),
    ];
    let header = ["Series", "Next", "New", "Watched", "Seen", "Airing"];
    window.render(f, list_table(rows, widths, Some(header), title), area, &mut app.upnext_state);
    if len == 0 {
        empty_hint(
            f,
            area,
            vec![
                Line::from(""),
                Line::from("Nothing followed yet.").bold(),
                Line::from(""),
                Line::from("Go to 3 Series, pick a show and press s → f to follow it,"),
                Line::from("or import your history: anipv import-fish --apply-status"),
            ],
        );
    }
}

fn draw_inbox(f: &mut Frame, app: &mut App, area: Rect) {
    let ts = app.now();
    let len = app.rows_len(View::Inbox);
    let window = Window::of_table(&app.inbox_state, len, area, true);
    let rows: Vec<Row> = app
        .inbox_rows_in(window.range.clone())
        .iter()
        .map(|(s, hint)| {
            let hint_text = hint.map(|h| h.describe(&app.lib)).unwrap_or_default();
            let style = match hint {
                Some(Hint::NewSeasonOf(_)) => Style::new().fg(ACCENT).bold(),
                Some(Hint::PartOf(_)) => Style::new().dark_gray(),
                None => Style::new(),
            };
            let title_style =
                if matches!(hint, Some(Hint::PartOf(_))) { Style::new().dark_gray() } else { Style::new() };
            Row::new(vec![
                Cell::from(s.title.clone()).style(title_style),
                Cell::from(score_span(s)),
                Cell::from(s.disk_range()),
                Cell::from(ago_opt(s.last_added(), ts)).dark_gray(),
                Cell::from(airing_span(s, ts)),
                Cell::from(hint_text).style(style),
            ])
        })
        .collect();
    // The hint column fits its longest text (up to 48), the title takes the rest.
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(5),
        Constraint::Length(16),
        Constraint::Length(cells(app.inbox_hint_width().min(48))),
    ];
    let title = format!(" Inbox · new in your download folders · {len} ");
    let table = list_table(rows, widths, Some(["Series", "Score", "On disk", "Added", "Airing", ""]), title);
    window.render(f, table, area, &mut app.inbox_state);
    if len == 0 {
        empty_hint(
            f,
            area,
            vec![
                Line::from(""),
                Line::from("Inbox zero.").bold(),
                Line::from("New shows that appear in your download folders show up here."),
            ],
        );
    }
}

fn draw_series(f: &mut Frame, app: &mut App, area: Rect) {
    let ts = app.now();
    let len = app.rows_len(View::Series);
    let window = Window::of_table(&app.series_state, len, area, true);
    let rows: Vec<Row> = app
        .series_rows_in(window.range.clone())
        .iter()
        .map(|s| {
            let latest = s.latest_on_disk().map(|e| e.to_string()).unwrap_or_default();
            let disk_style = if s.present() { Style::new() } else { Style::new().dark_gray() };
            let title = titled(app, s, Style::new());
            Row::new(vec![
                Cell::from(title),
                Cell::from(s.status.as_str()).fg(status_color(s.status)),
                Cell::from(score_span(s)),
                Cell::from(s.progress()),
                Cell::from(s.on_disk_items().to_string()).style(disk_style),
                Cell::from(latest).dark_gray(),
                Cell::from(ago_opt(s.last_activity(), ts)).dark_gray(),
                Cell::from(airing_span(s, ts)),
            ])
        })
        .collect();
    let mut title = vec![
        Span::raw(format!(" Series · {filter} ", filter = app.filter.label())),
        Span::styled(format!("{len} "), Style::new().dark_gray()),
    ];
    if app.query.is_empty() && !app.lib.franchises.is_empty() {
        let folded = if app.fold_franchises { "franchises folded (F) " } else { "franchises unfolded (F) " };
        title.push(Span::styled(folded, Style::new().dark_gray()));
    }
    if !app.query.is_empty() {
        title.push(Span::styled(format!("/{query} ", query = app.query), Style::new().fg(Color::Yellow)));
    }
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(18),
    ];
    let header = ["Series", "Status", "Score", "Watched", "Disk", "Last", "Seen", "Airing"];
    window.render(f, list_table(rows, widths, Some(header), Line::from(title)), area, &mut app.series_state);
}

fn draw_files(f: &mut Frame, app: &mut App, area: Rect) {
    if app.files_mode == FilesMode::Tree {
        draw_tree(f, app, area);
        return;
    }
    let ts = app.now();
    let queued_set = queued_set(app);
    let window = Window::of_table(&app.files_state, app.rows_len(View::Files), area, true);
    let rows: Vec<Row> = app
        .file_rows_in(window.range.clone())
        .iter()
        .map(|fi| {
            let series = app.lib.get(&fi.series);
            let item: Option<&Item> = series.and_then(|s| fi.items.first().and_then(|k| s.item(k)));
            let state = item.map(|i| i.state).unwrap_or_default();
            let queued = fi.items.iter().any(|k| queued_set.contains(&(fi.series.as_str(), k)));
            let status = series.map(|s| s.status).unwrap_or_default();
            Row::new(vec![
                Cell::from(Line::from(mark_span(queued, &state))),
                Cell::from(ago(fi.file.added(), ts)).dark_gray(),
                Cell::from(series.map(|s| s.title.clone()).unwrap_or_default())
                    .fg(if status == SeriesStatus::Untracked { Color::Reset } else { status_color(status) }),
                Cell::from(fi.items.iter().map(ItemKey::describe).collect::<Vec<_>>().join(",")),
                Cell::from(fi.file.rel.to_string_lossy().into_owned()).dark_gray(),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Percentage(30),
        Constraint::Length(10),
        Constraint::Fill(1),
    ];
    let title = Line::from(vec![
        Span::raw(format!(" Files · newest first · {n} ", n = app.rows_len(View::Files))),
        Span::styled("· t: folders ", Style::new().dark_gray()),
    ]);
    let table = list_table(rows, widths, Some(["", "Add", "Series", "Item", "File"]), title);
    window.render(f, table, area, &mut app.files_state);
}

fn draw_queue(f: &mut Frame, app: &mut App, area: Rect) {
    let len = app.queue.len();
    let window = Window::of_table(&app.queue_state, len, area, true);
    let rows: Vec<Row> = app.queue[window.range.clone()]
        .iter()
        .zip(window.range.clone())
        .map(|(q, i)| {
            let s = app.lib.get(&q.series);
            let st = s.and_then(|s| s.item(&q.item)).map(|i| i.state).unwrap_or_default();
            let resume = st.percent().map(|p| format!("resume {p}")).unwrap_or_default();
            Row::new(vec![
                Cell::from((i + 1).to_string()).dark_gray(),
                Cell::from(app.lib.title_of(&q.series).to_string()),
                Cell::from(q.item.describe()).bold(),
                Cell::from(resume).yellow(),
                Cell::from(crate::library::file_name_lossy(&q.file)).dark_gray(),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(3),
        Constraint::Percentage(30),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Fill(1),
    ];
    let table = list_table(rows, widths, Some(["#", "Series", "Item", "", "File"]), " Queue · plays in this order ");
    window.render(f, table, area, &mut app.queue_state);
    if len == 0 {
        empty_hint(
            f,
            area,
            vec![
                Line::from(""),
                Line::from("Queue is empty.").bold(),
                Line::from("Press space on episodes or series to add them, then p to play."),
            ],
        );
    }
}

/// `pos / dur` (or just `pos`) as clock times.
fn position_text(pos: f64, dur: Option<f64>) -> String {
    match dur {
        Some(d) => format!("{pos} / {dur}", pos = clock(pos), dur = clock(d)),
        None => clock(pos),
    }
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(s) = app.detail_series() else {
        f.render_widget(Paragraph::new("series not found").block(block(" Series ")), area);
        return;
    };
    let ts = app.now();
    let width = usize::from(area.width.saturating_sub(2));
    let lib = &app.lib;
    let franchise = lib.index_of(&s.key).and_then(|at| Some((lib.franchise_pos(at)?.0, at)));
    let dim = Style::new().dark_gray();

    // Line 1: the title, coloured by status, and what AniList calls it when
    // that's another name; at the right, the small facts.
    // Untracked plain; skipped (decided against) dimmed.
    let title_style = match s.status {
        SeriesStatus::Untracked => Style::new(),
        st => Style::new().fg(status_color(st)),
    };
    let title = vec![Span::styled(s.title.clone(), title_style.bold())];
    let alt = s.meta.as_ref().and_then(|m| m.title.as_ref()).filter(|t| {
        let key = crate::identity::series_key(t);
        !t.eq_ignore_ascii_case(&s.title) && key != s.key && !s.aliases.contains(&key)
    });
    let alt = alt.map(|t| vec![Span::styled(format!("{ANILIST_MARK} {t}"), dim)]);
    // (rank, spans): the lowest rank goes first when the line is too narrow.
    let mut right: Vec<(u8, Vec<Span<'static>>)> = Vec::new();
    if !s.aliases.is_empty() {
        // Names merged into it (U lists them before splitting them off).
        let n = s.aliases.len();
        right.push((0, vec![Span::styled(format!("+{n} name{s}", s = if n == 1 { "" } else { "s" }), dim)]));
    }
    if let Some(m) = &s.meta {
        if m.score.is_some() {
            right.push((3, vec![Span::styled("★ ", dim), score_span(s)]));
        }
        if let Some(p) = m.popularity {
            right.push((1, vec![Span::styled(short_count(p), dim)]));
        }
    }
    if let Some(id) = s.anilist_id() {
        right.push((2, vec![Span::styled(format!("{ANILIST_MARK}{id}"), Style::new().fg(ANILIST))]));
    }
    let mut info = vec![fit_line(title, alt, right, width)];

    // Line 2: how far you are, and a note of your own; at the right, the
    // franchise's entries (on a line of their own when there's no room).
    let mut facts: Vec<Vec<Span<'static>>> = vec![progress_spans(s)];
    let new = s.new_count();
    if new > 0 {
        facts.push(vec![Span::styled(format!("{new} new"), Style::new().green())]);
    }
    if let Some(m) = &s.meta {
        let fmt_year = [m.format.clone(), m.year.map(|y| y.to_string())].into_iter().flatten().collect::<Vec<_>>();
        if !fmt_year.is_empty() {
            facts.push(vec![Span::styled(fmt_year.join(" "), dim)]);
        }
    }
    let mut facts = join_spans(facts, " · ");
    let chain = franchise.map(|(fr, at)| franchise_chain(fr, at));
    let chain_w = chain.as_ref().map_or(0, |(stops, _)| chain_width(stops));
    let chain_inline = chain.is_some() && spans_width(&facts) + 3 + chain_w <= width;
    let room = if chain_inline { width - chain_w - 3 } else { width };
    if let Some(note) = user_note(s) {
        // Cut to what's left, or left out when that's too little to read.
        let spare = room.saturating_sub(spans_width(&facts) + 2);
        if spare >= 8 {
            facts.push(Span::raw("  "));
            facts.extend(cut_spans(vec![Span::styled(format!("“{note}”"), Style::new().fg(Color::Yellow))], spare));
        }
    }
    let facts = cut_spans(facts, room);
    match chain {
        Some((stops, _)) if chain_inline => {
            let gap = width.saturating_sub(spans_width(&facts) + chain_w);
            info.push(Line::from([facts, vec![Span::raw(" ".repeat(gap))], join_spans(stops, " → ")].concat()));
        }
        Some((stops, current)) => {
            info.push(Line::from(facts));
            info.push(Line::from(chain_window(&stops, current, width)));
        }
        None => info.push(Line::from(facts)),
    }
    // A few at most: the episode list below needs the room.
    let shared = s.shared_dirs(&app.ctx.cfg);
    let warn = |text: String| Line::from(cut_spans(vec![Span::styled(text, Style::new().fg(Color::Yellow))], width));
    for (dir, n) in shared.iter().take(2) {
        info.push(warn(format!(
            "! {n} episode(s) in {dir} are also in another folder (anipv split, if it's another anime)"
        )));
    }
    if let Some(more) = shared.len().checked_sub(2).filter(|n| *n > 0) {
        info.push(warn(format!("! … {more} more such folders (anipv show)")));
    }

    // The border: the franchise's name, the fish (statuses from fish history),
    // and when the next episode airs.
    let name = franchise.map_or_else(|| " Series ".to_string(), |(fr, _)| format!(" {name} ", name = fr.name));
    let mut header = block(name);
    if s.status_via == Some(crate::events::StatusVia::FishImport) {
        let fish = Span::styled(" ϵ( 'Θ' )϶ ", Style::new().fg(Color::Cyan));
        header = header.title_top(Line::from(vec![fish, Span::styled("─", Style::new().dark_gray())]).right_aligned());
    }
    let airing = airing_span(s, ts);
    if !airing.content.is_empty() {
        header = header.title_bottom(Line::from(vec![Span::raw(" "), airing, Span::raw(" ")]).centered());
    }
    let height = u16::try_from(info.len()).unwrap_or(u16::MAX).saturating_add(2);
    let [top, list] = Layout::vertical([Constraint::Length(height), Constraint::Min(1)]).areas(area);
    f.render_widget(Paragraph::new(info).block(header), top);

    let queued_set = queued_set(app);
    let window = Window::of_table(&app.detail_state, app.rows_len(View::Detail), list, false);
    let rows: Vec<Row> = app
        .detail_rows_in(window.range.clone())
        .into_iter()
        .map(|row| {
            let it = match row {
                DetailRow::Item(it) => it,
                DetailRow::Season { series, index, next } => return season_row(&app.lib, series, index, next),
            };
            let (kind, kind_style) = match it.key.kind {
                ItemKind::Episode | ItemKind::Unknown => ("", Style::new()),
                ItemKind::Special => ("special", Style::new().fg(Color::Magenta)),
                ItemKind::Extra => ("extra", Style::new().dark_gray()),
                ItemKind::Movie => ("movie", Style::new().fg(Color::Cyan)),
            };
            let when = match it.state {
                WatchState::Watched { at } => Span::styled(ago(at, ts), Style::new().dark_gray()),
                WatchState::Started { pos, dur, .. } => Span::styled(position_text(pos, dur), Style::new().yellow()),
                WatchState::Unwatched => Span::raw(""),
            };
            let file = match it.best_file() {
                Some(fr) => Span::styled(fr.name(), Style::new().dark_gray()),
                None => Span::styled("not on disk", Style::new().fg(Color::Red).dim()),
            };
            // Extras attached to an episode are indented under it.
            let attached = it.key.kind == ItemKind::Extra && it.key.ep.is_some();
            let label = if attached { format!("  {key}", key = it.key.label) } else { it.key.describe() };
            let style = if it.present() { Style::new() } else { Style::new().dark_gray() };
            Row::new(vec![
                Cell::from(Line::from(mark_span(queued_set.contains(&(s.key.as_str(), &it.key)), &it.state))),
                Cell::from(label).style(style.bold()),
                Cell::from(kind).style(kind_style),
                Cell::from(Line::from(when)),
                Cell::from(Line::from(file)),
            ])
        })
        .collect();
    let toggle = |shown| if shown { "hides" } else { "shows" };
    let shown = app.episodes_shown;
    let (missing, extras) =
        (s.missing_count(shown.extras), s.items.iter().filter(|i| i.key.kind == ItemKind::Extra).count());
    let hints = [
        (missing > 0).then(|| format!("d {verb} {missing} not on disk", verb = toggle(shown.missing))),
        (extras > 0).then(|| format!("x {verb} {extras} extras", verb = toggle(shown.extras))),
    ];
    let title: String =
        std::iter::once("Episodes".to_string()).chain(hints.into_iter().flatten()).collect::<Vec<_>>().join(" · ");
    let title = format!(" {title} ");
    let widths = [
        Constraint::Length(1),
        Constraint::Length(22),
        Constraint::Length(7),
        Constraint::Length(13),
        Constraint::Fill(1),
    ];
    window.render(f, list_table(rows, widths, None, title), list, &mut app.detail_state);
}

/// `watched/total` for the Detail header, `✓` once all are watched. When
/// more episodes are on disk than `AniList` counts (a folder numbering on
/// through later seasons, which `AniList` lists apart), the total is the
/// latest on disk, with `AniList`'s in brackets: `0/51 (ⓐ24 eps)`. Not for a
/// season numbered on from the last (13–24 of 12), nor a total you set.
fn progress_spans(s: &Series) -> Vec<Span<'static>> {
    let watched = s.whole_watched_count();
    // Only what counts toward the total (no 00, no .5 recaps).
    let on_disk = s.items.iter().filter(|i| i.counts_toward_total() && i.present()).count();
    let latest = s.latest_on_disk().map(crate::model::EpNo::whole);
    let from_anilist = s.episodes_override.is_none();
    let (mut spans, total) = match (s.total(), latest) {
        (Some(total), Some(latest)) if from_anilist && on_disk > total as usize && latest > total => (
            vec![
                Span::raw(format!("{watched}/{latest}")),
                Span::styled(format!(" ({ANILIST_MARK}{total} eps)"), Style::new().dark_gray()),
            ],
            Some(latest),
        ),
        (total, _) => (vec![Span::raw(s.progress())], total),
    };
    if total.is_some_and(|t| t > 0 && watched >= t as usize) {
        spans.push(Span::styled(" ✓", Style::new().green()));
    }
    spans
}

/// A note you wrote on the series' status (an automatic status's says
/// nothing new).
fn user_note(s: &Series) -> Option<&str> {
    s.note.as_deref().filter(|_| s.status_by == crate::events::SetBy::User)
}

/// The franchise's entries for the Detail header, one span group per stop:
/// the one shown (`at`) marked `▸ ◂`, the ones watched `✓`; and which group
/// is the first of the one shown.
fn franchise_chain(fr: &crate::franchise::Franchise, at: usize) -> (Vec<Vec<Span<'static>>>, usize) {
    let mut current = None;
    let stops = fr
        .stops
        .iter()
        .enumerate()
        .map(|(i, stop)| {
            let style = if stop.watched { Style::new().green() } else { Style::new() };
            let mut spans = if stop.series == at {
                current.get_or_insert(i);
                vec![Span::styled(format!("▸{short}◂", short = stop.short), style.bold().reversed())]
            } else {
                vec![Span::styled(stop.short.clone(), style)]
            };
            if stop.watched {
                spans.push(Span::styled(" ✓", Style::new().green()));
            }
            spans
        })
        .collect();
    (stops, current.unwrap_or(0))
}

/// The `stops` around the one at `current`, joined by arrows, in `width`:
/// as many whole stops as fit on either side, `…` where some are left out.
fn chain_window(stops: &[Vec<Span<'static>>], current: usize, width: usize) -> Vec<Span<'static>> {
    let joined = |r: Range<usize>| join_spans(stops[r].to_vec(), " → ");
    let w = |r: Range<usize>| chain_width(&stops[r]);
    if w(0..stops.len()) <= width {
        return joined(0..stops.len());
    }
    let (mut start, mut end) = (current, current + 1);
    // Room for `… → ` on both sides.
    let room = width.saturating_sub(8);
    loop {
        let grown_end = end < stops.len() && w(start..end + 1) <= room;
        if grown_end {
            end += 1;
        }
        let grown_start = start > 0 && w(start - 1..end) <= room;
        if grown_start {
            start -= 1;
        }
        if !grown_end && !grown_start {
            break;
        }
    }
    let ell = |s: &'static str| Span::styled(s, Style::new().dark_gray());
    let head = (start > 0).then(|| ell("… → "));
    let tail = (end < stops.len()).then(|| ell(" → …"));
    cut_spans(head.into_iter().chain(joined(start..end)).chain(tail).collect(), width)
}

/// `spans` cut to `width` columns, ending in `…` when anything is cut.
fn cut_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= width {
        return spans;
    }
    // One column kept for the `…`.
    let mut out = Vec::new();
    let mut left = width.saturating_sub(1);
    for span in spans {
        let w = span.width();
        if w <= left {
            left -= w;
            out.push(span);
        } else {
            // `…` after it even when the span itself fits (more is cut).
            let cut = crate::fmt::trunc(&format!("{content}…", content = span.content), left + 1);
            out.push(Span::styled(cut, span.style));
            break;
        }
    }
    out
}

/// How wide `stops` are, joined by ` → `.
fn chain_width(stops: &[Vec<Span<'static>>]) -> usize {
    stops.iter().map(|stop| spans_width(stop)).sum::<usize>() + 3 * stops.len().saturating_sub(1)
}

/// How many columns `spans` take.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// `parts` one after the other, `sep` (dim) between them.
fn join_spans(parts: Vec<Vec<Span<'static>>>, sep: &'static str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            out.push(Span::styled(sep, Style::new().dark_gray()));
        }
        out.extend(part);
    }
    out
}

/// The title and the small facts on one line of `width` columns, the facts
/// (`right`, as `(rank, spans)`) ending at the right edge. When it is too
/// narrow, it gives in this order: `alt` (AniList's name) is cut, then left
/// out below 12 columns; the title is cut down to 20; the facts are left
/// out, lowest rank first; then the title is cut further.
fn fit_line(
    title: Vec<Span<'static>>,
    alt: Option<Vec<Span<'static>>>,
    mut right: Vec<(u8, Vec<Span<'static>>)>,
    width: usize,
) -> Line<'static> {
    const MIN_TITLE: usize = 20;
    const MIN_ALT: usize = 12;
    // The facts with ` · ` between them, and two columns before them.
    let room = |right: &[(u8, Vec<Span<'static>>)]| {
        let w: usize = right.iter().map(|(_, s)| spans_width(s) + 3).sum();
        width.saturating_sub(w.saturating_sub(1))
    };
    let title_w = spans_width(&title);
    while title_w.min(MIN_TITLE) > room(&right)
        && let Some(lowest) = right.iter().enumerate().min_by_key(|(_, (rank, _))| *rank).map(|(i, _)| i)
    {
        right.remove(lowest);
    }
    let room = room(&right);
    let right = join_spans(right.into_iter().map(|(_, s)| s).collect(), " · ");
    let mut left = cut_spans(title, room);
    let spare = room.saturating_sub(spans_width(&left) + 2);
    if let Some(alt) = alt.filter(|_| spare >= MIN_ALT) {
        left.push(Span::raw("  "));
        left.extend(cut_spans(alt, spare));
    }
    let gap = width.saturating_sub(spans_width(&left) + spans_width(&right));
    Line::from([left, vec![Span::raw(" ".repeat(gap))], right].concat())
}

/// A Detail row linking to the franchise entry before (`▲`) or after (`▼`):
/// its short name, and how far you are in it (green once it's all watched,
/// as the franchise line marks it ✓). "Before" and "after", not "season": it
/// may be a movie or a spin-off.
fn season_row<'a>(lib: &Library, series: &Series, at: usize, next: bool) -> Row<'a> {
    let (arrow, which) = if next { ("▼", "after this") } else { ("▲", "before this") };
    let label = lib.short_series_label(at).unwrap_or(&series.title);
    let watched = lib.franchise_pos(at).is_some_and(|(f, _)| f.series_watched(at));
    let style = if watched { Style::new().green() } else { Style::new() };
    let about = format!(
        "{which} · watched {progress} · ⏎ or {key}",
        progress = series.progress(),
        key = if next { "]" } else { "[" }
    );
    Row::new(vec![
        Cell::from(""),
        Cell::from(format!("{arrow} {label}")).style(style.bold()),
        Cell::from(""),
        Cell::from(""),
        Cell::from(about).style(Style::new().dark_gray()),
    ])
}

fn draw_playing(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = &app.playing else { return };
    let ratio = p.dur.map_or(0.0, |d| (p.pos / d).clamp(0.0, 1.0));
    let (position, current, total) = (position_text(p.pos, p.dur), (p.done + 1).min(p.total), p.total);
    let icon = if p.paused { "⏸" } else { "▶" };
    let mut label = format!("{icon} {title}  {position}  [{current}/{total}]", title = p.label);
    // Remote mode looks different: keys typed now go to mpv, not to anipv.
    let fill = if app.remote_active() {
        label.push_str("  · REMOTE: keys go to mpv, ctrl-c for anipv");
        Color::Indexed(23)
    } else {
        Color::Indexed(53)
    };
    let g = Gauge::default()
        .gauge_style(Style::new().fg(fill).bg(Color::Indexed(235)))
        .ratio(ratio)
        .label(Span::styled(label, Style::new().fg(Color::White).bold()))
        .use_unicode(true);
    f.render_widget(g, area);
}

fn key_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    if app.remote_active() {
        return vec![("keys", "go to mpv"), ("ctrl-c", "back to anipv")];
    }
    let mut v: Vec<(&str, &str)> = match app.view {
        View::UpNext => vec![
            ("space", "queue next"),
            ("a", "queue all new"),
            ("⏎", "open"),
            ("w", "mark next seen"),
            ("P", "paused"),
        ],
        View::Inbox => {
            vec![
                ("f", "follow"),
                ("s", "skip"),
                ("z", "later"),
                ("space", "queue ep 1"),
                ("⏎", "open"),
                ("o", "AniList"),
                ("m", "merge"),
            ]
        }
        View::Series => {
            vec![
                ("⏎", "open"),
                ("/", "search"),
                ("f", "filter"),
                ("s", "status"),
                ("o", "AniList"),
                ("m", "merge"),
                ("M", "metadata"),
            ]
        }
        View::Files if app.files_mode == FilesMode::Tree => vec![
            ("⏎", "open"),
            ("h", "up"),
            ("space", "queue"),
            ("w", "watched"),
            ("/", "search"),
            ("t", "newest files"),
        ],
        View::Files => vec![("space", "queue"), ("w", "watched"), ("⏎", "series"), ("t", "folders")],
        View::Queue => vec![("⏎/p", "play"), ("J/K", "move"), ("d", "remove"), ("c", "clear"), ("y", "copy cmd")],
        View::Detail => vec![
            ("space", "queue"),
            ("⏎", "play"),
            // Short, so `[ ]`, `esc` and `?` fit 110 columns with a queue
            // (o and M are in the help, under Everywhere).
            ("w", "seen"),
            ("W", "up to here"),
        ]
        .into_iter()
        .chain(app.detail_has_seasons().then_some(("[ ]", "season")))
        .chain([("x", "extras"), ("d", "missing"), ("esc", "back")])
        .collect(),
    };
    if !app.queue.is_empty() && app.view != View::Queue {
        v.insert(0, ("p", "play queue"));
    }
    // Only once keys can reach mpv (it has connected).
    if app.playing.as_ref().is_some_and(NowPlaying::connected) {
        v.insert(0, ("v", "control mpv"));
    }
    v.push(("?", "help"));
    v
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some(m) = app.current() {
        let color = match m.severity {
            Severity::Info => Color::Yellow,
            Severity::Error => Color::LightRed,
        };
        f.render_widget(Line::from(Span::styled(format!(" {text}", text = m.text), Style::new().fg(color))), area);
        return;
    }
    let mut spans = vec![Span::raw(" ")];
    for (k, d) in key_hints(app) {
        spans.push(Span::styled(k, Style::new().fg(ACCENT).bold()));
        spans.push(Span::styled(format!(" {d}  "), Style::new().dark_gray()));
    }
    f.render_widget(Line::from(spans), area);
}

/// A count of rows or characters as terminal cells (saturating).
fn cells(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// All key bindings, also used to generate `docs/keybindings.md`.
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Everywhere",
        &[
            ("1-5 / Tab", "switch view; again: other mode / next filter"),
            ("j/k ↑/↓", "move"),
            ("g/G", "top / bottom"),
            ("p", "play queue (or the next episode here)"),
            ("y", "copy mpv command for the queue"),
            ("v", "while mpv plays: remote mode, keys go to mpv"),
            ("s", "set series status (in the Inbox: skip)"),
            ("r", "rescan media folders"),
            ("M", "refresh metadata for the series under the cursor"),
            ("o", "open the series under the cursor on AniList"),
            ("ctrl-r", "refresh all metadata (and an old anime DB)"),
            ("?", "this help"),
            ("q / ctrl-c", "quit (asks first); ctrl-c closes a popup first"),
        ],
    ),
    (
        "Up next",
        &[
            ("space", "queue next new episode and move down"),
            ("a", "queue all new episodes"),
            ("w", "mark next episode watched without playing"),
            ("P", "include paused series"),
            ("⏎", "open series"),
        ],
    ),
    (
        "Inbox",
        &[
            ("f", "follow: moves it to Up next"),
            ("s", "skip: not interested, never started"),
            ("z", "later: mark it paused"),
            ("space", "queue its first episode"),
            ("⏎", "open series"),
            ("m", "merge with another series (duplicate names)"),
        ],
    ),
    (
        "Series",
        &[
            ("/", "fuzzy search (⏎ keeps it, Esc clears)"),
            ("f / 3", "next status filter (works with a search)"),
            ("m", "merge into another series"),
            ("U", "undo merges into this series"),
            ("R", "rename"),
            ("L", "link AniList id"),
            ("F", "fold franchises to the entry you're at"),
            ("C", "catch up: this and earlier entries watched"),
            ("⏎", "open series"),
        ],
    ),
    (
        "Episodes",
        &[
            ("space", "add / remove from queue"),
            ("⏎", "play this episode (or open a ▲ ▼ season)"),
            ("[ ]", "previous / next season"),
            ("w", "toggle watched"),
            ("W", "mark everything up to here watched"),
            ("C", "catch up: this and earlier entries watched"),
            ("a", "queue all new"),
            ("x", "show / hide extras"),
            ("d", "show / hide episodes not on disk"),
            ("esc", "back"),
        ],
    ),
    (
        "Files",
        &[
            ("t / 4", "newest files ⇄ folders"),
            ("space", "queue a file, or a folder's unwatched"),
            ("w", "toggle watched (a folder: all of it)"),
            ("⏎ / → / l", "open folder (a file: its series)"),
            ("← / h / ⌫", "up a folder"),
            ("/", "search below here (⏎ keeps it, Esc clears)"),
        ],
    ),
    ("Queue", &[("J/K", "move entry down / up"), ("d", "remove"), ("c", "clear"), ("⏎", "play")]),
    (
        "Remote mode (while mpv plays)",
        &[("any key", "pressed in mpv, so your input.conf applies"), ("ctrl-c", "keys back to anipv (v: back to mpv)")],
    ),
];

/// The key help; taller than the terminal, it scrolls (`scroll` is clamped
/// to the last full page). Sets `fits` when it all fits.
fn draw_help(f: &mut Frame, scroll: &mut u16, fits: &mut bool) {
    let mut lines = Vec::new();
    for (section, keys) in HELP {
        lines.push(Line::from(Span::styled(*section, Style::new().fg(ACCENT).bold())));
        for (k, d) in *keys {
            lines.push(Line::from(vec![Span::styled(format!("  {k:<12}"), Style::new().bold()), Span::raw(*d)]));
        }
        lines.push(Line::from(""));
    }
    lines.pop(); // no blank line after the last section
    let total = cells(lines.len());
    let area = centered(f.area(), 64, total.saturating_add(2));
    let shown = area.height.saturating_sub(2);
    *scroll = (*scroll).min(total.saturating_sub(shown));
    *fits = shown >= total;
    let title = if *fits {
        " Keys · any key closes ".to_string()
    } else {
        let (first, last) = (*scroll + 1, *scroll + shown);
        format!(" Keys {first}-{last} of {total} · j/k scroll · other keys close ")
    };
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).scroll((*scroll, 0)).block(block(title)), area);
}

fn draw_status_popup(f: &mut Frame, lib: &Library, series: &str, idx: usize) {
    let title = lib.title_of(series);
    let rows: Vec<Line> = SeriesStatus::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (first, rest) = s.as_str().split_at(1);
            let st = if i == idx { highlight() } else { Style::new() };
            Line::from(vec![
                Span::styled(if i == idx { "▌" } else { " " }, Style::new().fg(ACCENT)),
                Span::styled(first, st.fg(status_color(*s)).bold().underlined()),
                Span::styled(rest, st.fg(status_color(*s))),
            ])
        })
        .collect();
    let title_width = unicode_width::UnicodeWidthStr::width(title);
    let area = centered(f.area(), 40.max(cells(title_width).saturating_add(6)), cells(rows.len()).saturating_add(2));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(rows).block(block(format!(" {title} "))), area);
}

fn draw_merge(f: &mut Frame, lib: &Library, picker: &mut MergePicker) {
    let from_title = lib.title_of(&picker.from);
    let area = centered(f.area(), 70, 18);
    f.render_widget(Clear, area);
    let outer = block(format!(" Merge “{from_title}” into… "));
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let [input, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    f.render_widget(
        Line::from(vec![Span::styled("› ", Style::new().fg(ACCENT)), Span::raw(picker.query.clone()), Span::raw("▏")]),
        input,
    );
    let window = Window::of(&picker.state, picker.targets.len(), usize::from(list.height));
    let rows: Vec<Row> = picker.targets[window.range.clone()]
        .iter()
        .map(|key| match lib.get(key) {
            Some(s) => {
                Row::new(vec![Cell::from(s.title.clone()), Cell::from(s.status.as_str()).fg(status_color(s.status))])
            }
            None => Row::new(vec![Cell::from(key.clone())]),
        })
        .collect();
    let table = highlighted(Table::new(rows, [Constraint::Fill(1), Constraint::Length(10)]));
    window.render(f, table, list, &mut picker.state);
}

/// The search box, over the footer.
fn draw_search(f: &mut Frame, query: &str) {
    let area = Rect { y: f.area().bottom().saturating_sub(1), height: 1, ..f.area() };
    f.render_widget(Clear, area);
    f.render_widget(
        Line::from(vec![
            Span::styled(" / ", Style::new().fg(ACCENT).bold()),
            Span::raw(query.to_string()),
            Span::raw("▏"),
        ]),
        area,
    );
}

fn draw_input(f: &mut Frame, prompt: &str, text: &str) {
    let area = centered(f.area(), 60, 3);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(format!("{text}▏")).block(block(format!(" {prompt} "))), area);
}

fn draw_confirm(f: &mut Frame, playing: bool) {
    let area = centered(f.area(), 56, 4);
    f.render_widget(Clear, area);
    let lines = if playing {
        ["mpv is still playing; progress won't be recorded.", "Quit anyway? q/y/Enter quits, any other key cancels"]
    } else {
        ["Quit anipv?", "q/y/Enter quits, any other key cancels"]
    };
    f.render_widget(
        Paragraph::new(lines.map(Line::from).to_vec()).alignment(Alignment::Center).block(block(" Quit? ")),
        area,
    );
}

/// The Files view's folder tree: the folder shown (or the folders a search
/// found), folders first with what is watched below them, then files.
fn draw_tree(f: &mut Frame, app: &mut App, area: Rect) {
    let ts = app.now();
    let queued_set = queued_set(app);
    // The folders of the queued files (as tree folders).
    let queued_dirs: Vec<crate::folders::Dir> = app
        .queue
        .iter()
        .filter_map(|q| app.lib.file(&q.file))
        .map(|f| crate::folders::Dir::of_file(&f.file.root, &f.file.rel))
        .collect();
    let len = app.rows_len(View::Files);
    let window = Window::of_table(&app.tree_state, len, area, true);
    let searching = !app.tree_query.is_empty();
    let rows: Vec<Row> = app
        .tree_rows_in(window.range.clone())
        .iter()
        .map(|e| match e {
            Entry::Folder(dir) => {
                let empty = crate::folders::Stats::default();
                let stats = app.folders.stats(dir).unwrap_or(&empty);
                // The folder as one item: all watched, partly, or not at all.
                let state = if stats.videos > 0 && stats.watched == stats.videos {
                    WatchState::Watched { at: stats.added }
                } else if stats.watched > 0 || stats.started > 0 {
                    WatchState::Started { pos: 0.0, dur: None, at: stats.added }
                } else {
                    WatchState::Unwatched
                };
                let mark = mark_span(queued_dirs.iter().any(|d| dir.contains(d)), &state);
                // A search hit shows where it is, from the folder you're in.
                let name = if searching { e.path_from(&app.tree_at, &app.lib) } else { dir.name() };
                let series = match stats.series.len() {
                    1 => stats
                        .series
                        .first()
                        .and_then(|k| app.lib.get(k))
                        .map_or_else(|| Cell::from(""), |s| Cell::from(s.title.clone()).fg(status_color(s.status))),
                    n => Cell::from(format!("{n} series")).dark_gray(),
                };
                Row::new(vec![
                    Cell::from(Line::from(mark)),
                    Cell::from(format!("{name}/")).bold(),
                    Cell::from(format!("{watched}/{videos}", watched = stats.watched, videos = stats.videos)),
                    Cell::from(ago(stats.added, ts)).dark_gray(),
                    series,
                ])
            }
            Entry::File(i) => {
                let fi = &app.lib.files[*i];
                let series = app.lib.get(&fi.series);
                let item: Option<&Item> = series.and_then(|s| fi.items.first().and_then(|k| s.item(k)));
                let state = item.map(|i| i.state).unwrap_or_default();
                let queued = fi.items.iter().any(|k| queued_set.contains(&(fi.series.as_str(), k)));
                let items = fi.items.iter().map(ItemKey::describe).collect::<Vec<_>>().join(",");
                let status = series.map(|s| s.status).unwrap_or_default();
                Row::new(vec![
                    Cell::from(Line::from(mark_span(queued, &state))),
                    Cell::from(if searching { e.path_from(&app.tree_at, &app.lib) } else { fi.file.name() }),
                    Cell::from(items),
                    Cell::from(ago(fi.file.added(), ts)).dark_gray(),
                    Cell::from(series.map(|s| s.title.clone()).unwrap_or_default())
                        .fg(if status == SeriesStatus::Untracked { Color::Reset } else { status_color(status) }),
                ])
            }
        })
        .collect();
    let at = if app.tree_at.is_top() { "all folders".to_string() } else { app.tree_at.display() };
    let mut title =
        vec![Span::raw(format!(" Files · {at} · ")), Span::styled(format!("{len} "), Style::new().dark_gray())];
    if searching {
        title
            .push(Span::styled(format!("/{tree_query} ", tree_query = app.tree_query), Style::new().fg(Color::Yellow)));
    }
    let widths = [
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(9),
        Constraint::Length(5),
        Constraint::Percentage(30),
    ];
    let table = list_table(rows, widths, Some(["", "Name", "Watched", "Add", "Series"]), Line::from(title));
    window.render(f, table, area, &mut app.tree_state);
}

/// A yes/no popup: `question`, what it is about (bold), and the keys.
fn draw_yes_no(f: &mut Frame, title: &str, question: &str, what: &str, keys: &str) {
    let area = centered(f.area(), 64, 6);
    f.render_widget(Clear, area);
    let lines = vec![
        Line::from(question.to_string()),
        Line::from(what.to_string()).bold(),
        Line::from(Span::styled(keys.to_string(), Style::new().dark_gray())),
    ];
    let para = Paragraph::new(lines).alignment(Alignment::Center).wrap(Wrap { trim: true });
    f.render_widget(para.block(block(format!(" {title} "))), area);
}

fn draw_unmerge(f: &mut Frame, names: &[String]) {
    let (question, keys) =
        ("Make these names series of their own again?", "y/Enter splits them off, any other key cancels");
    draw_yes_no(f, "Undo merges", question, &names.join(", "), keys);
}

fn draw_catch_up(f: &mut Frame, prompt: &str) {
    let (question, keys) = ("Seen all of it? Mark watched and completed:", "y/Enter confirms, any other key cancels");
    draw_yes_no(f, "Catch up", question, prompt, keys);
}

/// A series' title with its place in its franchise, dimmed:
/// `Monogatari Series 9/12 · Owarimonogatari`, or `Yuru Camp S2 · 2/3`
/// when the title names the franchise already.
fn titled(app: &App, s: &Series, style: Style) -> Line<'static> {
    let title = Span::styled(s.title.clone(), style);
    let Some((f, pos)) = app.lib.franchise_of(&s.key) else { return Line::from(title) };
    let place = format!("{at}/{stops}", at = pos + 1, stops = f.stops.len());
    if s.title.to_lowercase().starts_with(&f.name.to_lowercase()) {
        Line::from(vec![title, Span::styled(format!(" · {place}"), Style::new().dark_gray())])
    } else {
        Line::from(vec![Span::styled(format!("{name} {place} · ", name = f.name), Style::new().dark_gray()), title])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(offset: usize, selected: Option<usize>, len: usize, height: usize) -> (Range<usize>, Option<usize>) {
        let mut state = TableState::default().with_selected(selected);
        *state.offset_mut() = offset;
        let w = Window::of(&state, len, height);
        (w.range, w.selected)
    }

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The facts end at the right edge; when it's narrow, AniList's name is
    /// cut and then left out, the title is cut to 20, then facts are left
    /// out (lowest rank first), then the title is cut further.
    /// A cut always shows `…`, also between two spans; a cut chain shows
    /// whole entries around the one shown, never a dangling arrow.
    #[test]
    fn cuts_show_and_keep_whole_entries() {
        let spans = vec![Span::raw("1/48"), Span::raw(" · 2 new")];
        assert_eq!(text(&Line::from(cut_spans(spans.clone(), 4))), "1/4…");
        assert_eq!(text(&Line::from(cut_spans(spans.clone(), 5))), "1/48…");
        assert_eq!(text(&Line::from(cut_spans(spans, 20))), "1/48 · 2 new");
        let stops: Vec<Vec<Span<'static>>> =
            ["S1", "S2", "S3", "▸S4◂", "S5", "S6", "S7"].map(|s| vec![Span::raw(s)]).to_vec();
        let window = |w| text(&Line::from(chain_window(&stops, 3, w)));
        assert_eq!(window(80), "S1 → S2 → S3 → ▸S4◂ → S5 → S6 → S7");
        assert_eq!(window(30), "… → S3 → ▸S4◂ → S5 → S6 → …");
        assert_eq!(window(20), "… → ▸S4◂ → S5 → …");
    }

    #[test]
    fn header_lines_give_way_in_order() {
        let spans = |s: &str| vec![Span::raw(s.to_string())];
        let right = || vec![(3, spans("★ 85")), (1, spans("43k")), (2, spans("ⓐ1"))];
        let line = |w| {
            let l = fit_line(
                spans("Steel Ball Run - JoJo's Bizarre Adventure"),
                Some(spans("ⓐ JoJo no Kimyou")),
                right(),
                w,
            );
            let t = text(&l);
            assert_eq!(unicode_width::UnicodeWidthStr::width(t.as_str()), w, "{t}");
            t
        };
        assert!(line(90).starts_with("Steel Ball Run - JoJo's Bizarre Adventure  ⓐ JoJo no Kimyou "));
        assert!(line(90).ends_with("★ 85 · 43k · ⓐ1"));
        assert!(
            line(72).contains("Adventure  ⓐ JoJo no K…") && line(72).ends_with("★ 85 · 43k · ⓐ1"),
            "{l}",
            l = line(72)
        );
        assert!(line(60).starts_with("Steel Ball Run - JoJo's Bizarre Adventure "), "{l}", l = line(60));
        assert!(!line(60).contains("ⓐ JoJo"), "below 12 columns, left out");
        assert!(line(40).contains("…") && line(40).ends_with("★ 85 · 43k · ⓐ1"), "{l}", l = line(40));
        assert_eq!(line(30), "Steel Ball Run - JoJo's…  ★ 85", "the title fills what ★ leaves");
        assert_eq!(line(10), "Steel Bal…");
    }

    #[test]
    fn member_counts_are_short() {
        let got: Vec<String> = [0, 843, 999, 1_000, 12_345, 999_499, 999_500, 1_000_000, 1_249_999, 1_250_000]
            .into_iter()
            .map(short_count)
            .collect();
        assert_eq!(got, ["0", "843", "999", "1k", "12k", "999k", "1M", "1M", "1.2M", "1.3M"]);
    }

    #[test]
    fn window_fills_the_view_after_the_list_shrinks_or_the_view_grows() {
        // Scrolled to 90 of 100 rows, the list shrinks to 20 (cursor clamped to 19).
        assert_eq!(window(90, Some(95), 20, 10), (10..20, Some(9)));
        // Cursor near the top of a shrunken list: still a full view, cursor visible.
        assert_eq!(window(90, Some(12), 20, 10), (10..20, Some(2)));
        // The terminal grew taller than what is left below the offset.
        assert_eq!(window(15, Some(17), 20, 10), (10..20, Some(7)));
        // Everything fits: start at the top.
        assert_eq!(window(5, Some(7), 8, 10), (0..8, Some(7)));
        assert_eq!(window(5, None, 8, 10), (0..8, None));
        // Ordinary scrolling is unchanged.
        assert_eq!(window(0, Some(12), 100, 10), (3..13, Some(9)));
        assert_eq!(window(20, Some(25), 100, 10), (20..30, Some(5)));
        assert_eq!(window(3, None, 0, 10), (0..0, None));
    }
}
