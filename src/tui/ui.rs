//! Rendering. Pure functions of `App` state → frame.

use std::collections::HashSet;
use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Gauge, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
};

use super::app::{App, InputPurpose, MergePicker, Popup, Severity, View};
use crate::fmt::{ago, ago_opt, clock};
use crate::library::Hint;
use crate::library::{Item, Library, Series};
use crate::model::{ItemKey, ItemKind, SeriesStatus, WatchState};

const ACCENT: Color = Color::Magenta;

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
    if app.playing.is_some() {
        draw_playing(f, app, now_playing);
    }
    draw_footer(f, app, footer);

    match &mut app.popup {
        Some(Popup::Help) => draw_help(f),
        Some(Popup::Status { series, idx }) => draw_status_popup(f, &app.lib, series, *idx),
        Some(Popup::Merge(picker)) => draw_merge(f, &app.lib, picker),
        Some(Popup::Input { purpose, prompt, text }) => draw_input(f, purpose, prompt, text),
        Some(Popup::ConfirmQuit) => draw_confirm(f, app.playing.is_some()),
        None => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" anipv ", Style::new().fg(Color::Black).bg(ACCENT).bold()), Span::raw(" ")];
    let active = if app.view == View::Detail { app.back } else { app.view };
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
    if let Some(m) = app.meta_busy() {
        right.push(Span::styled(format!("⟳ {m} "), Style::new().fg(Color::Blue)));
    }
    if !app.offline_roots.is_empty() {
        let names = app.offline_roots.join(",");
        right.push(Span::styled(format!("✗ {names} offline "), Style::new().fg(Color::Red)));
    }
    right.push(Span::styled(format!("{} ", app.ctx.log.device()), Style::new().fg(Color::DarkGray)));
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
            Row::new(vec![
                Cell::from(s.title.clone()).style(title_style),
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
                Line::from("Go to 2 Series, pick a show and press s → f to follow it,"),
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
        Constraint::Length(9),
        Constraint::Length(5),
        Constraint::Length(16),
        Constraint::Length(app.inbox_hint_width().min(48)),
    ];
    let title = format!(" Inbox · new in your download folders · {len} ");
    let table = list_table(rows, widths, Some(["Series", "On disk", "Added", "Airing", ""]), title);
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
            Row::new(vec![
                Cell::from(s.title.clone()),
                Cell::from(s.status.as_str()).fg(status_color(s.status)),
                Cell::from(s.progress()),
                Cell::from(s.on_disk_items().to_string()).style(disk_style),
                Cell::from(latest).dark_gray(),
                Cell::from(ago_opt(s.last_activity(), ts)).dark_gray(),
                Cell::from(airing_span(s, ts)),
            ])
        })
        .collect();
    let mut title = vec![
        Span::raw(format!(" Series · {} ", app.filter.label())),
        Span::styled(format!("{len} "), Style::new().dark_gray()),
    ];
    if !app.query.is_empty() {
        title.push(Span::styled(format!("/{} ", app.query), Style::new().fg(Color::Yellow)));
    }
    let widths = [
        Constraint::Fill(1),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(18),
    ];
    let header = ["Series", "Status", "Watched", "Disk", "Last", "Seen", "Airing"];
    window.render(f, list_table(rows, widths, Some(header), Line::from(title)), area, &mut app.series_state);
}

fn draw_files(f: &mut Frame, app: &mut App, area: Rect) {
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
                Cell::from(fi.items.iter().map(super::super::model::ItemKey::describe).collect::<Vec<_>>().join(",")),
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
    let title = format!(" Files · newest first · {} ", app.rows_len(View::Files));
    let table = list_table(rows, widths, Some(["", "Add", "Series", "Item", "File"]), title);
    window.render(f, table, area, &mut app.files_state);
}

fn draw_queue(f: &mut Frame, app: &mut App, area: Rect) {
    let rows: Vec<Row> = app
        .queue
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let s = app.lib.get(&q.series);
            let st = s.and_then(|s| s.item(&q.item)).map(|i| i.state).unwrap_or_default();
            let resume = st.percent().map(|p| format!("resume {p}")).unwrap_or_default();
            Row::new(vec![
                Cell::from(format!("{}", i + 1)).dark_gray(),
                Cell::from(s.map_or_else(|| q.series.clone(), |s| s.title.clone())),
                Cell::from(q.item.describe()).bold(),
                Cell::from(resume).yellow(),
                Cell::from(crate::library::file_name_lossy(&q.file)).dark_gray(),
            ])
        })
        .collect();
    let empty = rows.is_empty();
    let widths = [
        Constraint::Length(3),
        Constraint::Percentage(30),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Fill(1),
    ];
    let table = list_table(rows, widths, Some(["#", "Series", "Item", "", "File"]), " Queue · plays in this order ");
    f.render_stateful_widget(table, area, &mut app.queue_state);
    if empty {
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
    // Hold our own handle so rows can borrow the series while `app` is borrowed mutably below.
    let lib = std::sync::Arc::clone(&app.lib);
    let Some(s) = app.detail_key.as_deref().and_then(|k| lib.get(k)) else {
        f.render_widget(Paragraph::new("series not found").block(block(" Series ")), area);
        return;
    };
    let ts = app.now();
    let mut info: Vec<Line> = Vec::new();
    let mut l1 = vec![
        Span::styled(s.title.clone(), Style::new().bold()),
        Span::raw("  "),
        Span::styled(s.status.as_str(), Style::new().fg(status_color(s.status)).bold()),
        Span::raw("  "),
        Span::styled(format!("watched {}", s.progress()), Style::new()),
        Span::raw("  "),
        Span::styled(format!("on disk {}", s.on_disk_items()), Style::new().dark_gray()),
        Span::raw("  "),
        airing_span(s, ts),
    ];
    if let Some(m) = &s.meta {
        if let Some(t) = &m.title
            && !t.eq_ignore_ascii_case(&s.title)
        {
            l1.push(Span::styled(format!("  ≈ {t}"), Style::new().dark_gray()));
        }
        let fmt_year =
            [m.format.clone(), m.year.map(|y| y.to_string())].into_iter().flatten().collect::<Vec<_>>().join(" ");
        if !fmt_year.is_empty() {
            l1.push(Span::styled(format!(" ({fmt_year})"), Style::new().dark_gray()));
        }
    }
    info.push(Line::from(l1));
    let mut l2 = vec![Span::styled(s.key.clone(), Style::new().dark_gray())];
    if !s.aliases.is_empty() {
        l2.push(Span::styled(format!("  also: {}", s.aliases.join(", ")), Style::new().dark_gray()));
    }
    if let Some(n) = &s.note {
        l2.push(Span::styled(format!("  “{n}”"), Style::new().fg(Color::Yellow)));
    }
    info.push(Line::from(l2));

    let [top, list] = Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).areas(area);
    f.render_widget(Paragraph::new(info).block(block(" Series ")).wrap(Wrap { trim: true }), top);

    let queued_set = queued_set(app);
    let window = Window::of_table(&app.detail_state, app.rows_len(View::Detail), list, false);
    let rows: Vec<Row> = app
        .detail_rows_in(window.range.clone())
        .iter()
        .map(|&it| {
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
            let label = if attached { format!("  {}", it.key.label) } else { it.key.describe() };
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

fn draw_playing(f: &mut Frame, app: &App, area: Rect) {
    let Some(p) = &app.playing else { return };
    let ratio = p.dur.map_or(0.0, |d| (p.pos / d).clamp(0.0, 1.0));
    let (position, current, total) = (position_text(p.pos, p.dur), (p.done + 1).min(p.total), p.total);
    let label = format!("▶ {title}  {position}  [{current}/{total}]", title = p.label);
    let g = Gauge::default()
        .gauge_style(Style::new().fg(Color::Indexed(53)).bg(Color::Indexed(235)))
        .ratio(ratio)
        .label(Span::styled(label, Style::new().fg(Color::White).bold()))
        .use_unicode(true);
    f.render_widget(g, area);
}

fn key_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&str, &str)> = match app.view {
        View::UpNext => vec![
            ("space", "queue next"),
            ("a", "queue all new"),
            ("⏎", "open"),
            ("w", "mark next seen"),
            ("P", "paused"),
        ],
        View::Inbox => {
            vec![("f", "follow"), ("s", "skip"), ("z", "later"), ("space", "queue ep 1"), ("⏎", "open"), ("m", "merge")]
        }
        View::Series => {
            vec![("⏎", "open"), ("/", "search"), ("f", "filter"), ("s", "status"), ("m", "merge"), ("M", "metadata")]
        }
        View::Files => vec![("space", "queue"), ("w", "watched"), ("⏎", "series")],
        View::Queue => vec![("⏎/p", "play"), ("J/K", "move"), ("d", "remove"), ("c", "clear"), ("y", "copy cmd")],
        View::Detail => vec![
            ("space", "queue"),
            ("⏎", "play"),
            ("w", "toggle seen"),
            ("W", "seen up to here"),
            ("x", "extras"),
            ("d", "not on disk"),
            ("M", "metadata"),
            ("esc", "back"),
        ],
    };
    if !app.queue.is_empty() && app.view != View::Queue {
        v.insert(0, ("p", "play queue"));
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
        f.render_widget(Line::from(Span::styled(format!(" {}", m.text), Style::new().fg(color))), area);
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
pub(super) fn cells(n: usize) -> u16 {
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
            ("1-5 / Tab", "switch view"),
            ("j/k ↑/↓", "move"),
            ("g/G", "top / bottom"),
            ("p", "play queue (or the next episode under the cursor)"),
            ("y", "copy mpv command for the queue"),
            ("s", "set series status (in the Inbox: skip)"),
            ("r", "rescan media folders"),
            ("M", "refresh metadata for the series under the cursor"),
            ("ctrl-r", "refresh all metadata now (updates the anime database if old)"),
            ("?", "this help"),
            ("q / ctrl-c", "quit (asks first: q, y or ⏎ confirms)"),
        ],
    ),
    (
        "Up next",
        &[
            ("space", "queue next new episode (repeat for more)"),
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
            ("/", "fuzzy search"),
            ("f", "cycle filter (on disk, following, paused, …)"),
            ("m", "merge into another series"),
            ("U", "undo merges into this series"),
            ("R", "rename"),
            ("L", "link AniList id"),
            ("⏎", "open series"),
        ],
    ),
    (
        "Episodes",
        &[
            ("space", "add / remove from queue"),
            ("⏎", "play this episode"),
            ("w", "toggle watched"),
            ("W", "mark everything up to here watched"),
            ("a", "queue all new"),
            ("x", "show / hide extras"),
            ("d", "show / hide episodes not on disk"),
            ("esc", "back"),
        ],
    ),
    ("Queue", &[("J/K", "move entry down / up"), ("d", "remove"), ("c", "clear"), ("⏎", "play")]),
];

fn draw_help(f: &mut Frame) {
    let mut lines = Vec::new();
    for (section, keys) in HELP {
        lines.push(Line::from(Span::styled(*section, Style::new().fg(ACCENT).bold())));
        for (k, d) in *keys {
            lines.push(Line::from(vec![Span::styled(format!("  {k:<12}"), Style::new().bold()), Span::raw(*d)]));
        }
        lines.push(Line::from(""));
    }
    let h = cells(lines.len()).saturating_add(2);
    let area = centered(f.area(), 64, h);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block(" Keys · any key closes ")), area);
}

fn draw_status_popup(f: &mut Frame, lib: &Library, series: &str, idx: usize) {
    let title = lib.get(series).map_or_else(|| series.to_string(), |s| s.title.clone());
    let rows: Vec<Line> = SeriesStatus::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let first = &s.as_str()[..1];
            let rest = &s.as_str()[1..];
            let st = if i == idx { highlight() } else { Style::new() };
            Line::from(vec![
                Span::styled(if i == idx { "▌" } else { " " }, Style::new().fg(ACCENT)),
                Span::styled(first, st.fg(status_color(*s)).bold().underlined()),
                Span::styled(rest, st.fg(status_color(*s))),
            ])
        })
        .collect();
    let title_width = unicode_width::UnicodeWidthStr::width(title.as_str());
    let area = centered(f.area(), 40.max(cells(title_width).saturating_add(6)), cells(rows.len()).saturating_add(2));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(rows).block(block(format!(" {title} "))), area);
}

fn draw_merge(f: &mut Frame, lib: &Library, picker: &mut MergePicker) {
    let from_title = lib.get(&picker.from).map_or_else(|| picker.from.clone(), |s| s.title.clone());
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

fn draw_input(f: &mut Frame, purpose: &InputPurpose, prompt: &str, text: &str) {
    if *purpose == InputPurpose::Search {
        let area = Rect { y: f.area().bottom().saturating_sub(1), height: 1, ..f.area() };
        f.render_widget(Clear, area);
        f.render_widget(
            Line::from(vec![
                Span::styled(" / ", Style::new().fg(ACCENT).bold()),
                Span::raw(text.to_string()),
                Span::raw("▏"),
            ]),
            area,
        );
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn window(offset: usize, selected: Option<usize>, len: usize, height: usize) -> (Range<usize>, Option<usize>) {
        let mut state = TableState::default().with_selected(selected);
        *state.offset_mut() = offset;
        let w = Window::of(&state, len, height);
        (w.range, w.selected)
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
