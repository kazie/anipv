//! Render the TUI against the demo library on a fake terminal and snapshot
//! each screen. Run `cargo insta review` after intentional UI changes.
#![expect(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]

use anipv::tui::app::{DetailRow, View};
use anipv::tui::{App, ui};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// A fixed "now" so relative times in snapshots never drift.
const NOW: i64 = 1_790_000_000;

fn app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = anipv::demo::setup(dir.path(), NOW).unwrap();
    let mut app = App::new(ctx).unwrap();
    app.clock = Some(NOW);
    (dir, app)
}

/// Queue the next episode of the first two series in Up next (Space stays
/// on a series with more new episodes, so pick the second row by hand).
fn queue_first_two(app: &mut App) {
    app.press(" ");
    app.upnext_state.select(Some(1));
    app.press(" ");
}

fn render(app: &mut App) -> String {
    render_sized(app, 110, 28)
}

/// The items in Detail (the rows that aren't season links).
fn items(app: &App) -> Vec<&anipv::library::Item> {
    app.detail_rows().into_iter().filter_map(DetailRow::item).collect()
}

/// The key of the item under the cursor in Detail.
fn selected_item(app: &App) -> Option<anipv::model::ItemKey> {
    let i = app.detail_state.selected()?;
    Some(app.detail_rows().get(i)?.item()?.key.clone())
}

fn render_sized(app: &mut App, width: u16, height: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| ui::draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn up_next_lists_followed_series_with_new_episodes() {
    let (_d, mut app) = app();
    let s = render(&mut app);
    assert!(s.contains("Up next"), "{s}");
    assert!(s.contains("Sousou no Frieren"), "{s}");
    assert!(s.contains("Dandadan S2"), "{s}");
    assert!(!s.contains("K-On!"), "dropped series are not in up next:\n{s}");
    // Frieren: next is 29, three new on disk.
    let line = s.lines().find(|l| l.contains("Sousou no Frieren")).unwrap();
    assert!(line.contains("29"), "{line}");
    insta::assert_snapshot!("up_next", s);
}

#[test]
fn queue_and_play_flow() {
    let (_d, mut app) = app();
    // Queue next of the first two series, then look at the queue tab.
    queue_first_two(&mut app);
    assert_eq!(app.queue.len(), 2);
    app.press("5");
    let s = render(&mut app);
    assert!(s.contains("Queue · plays in this order"), "{s}");
    insta::assert_snapshot!("queue", s);
    // Reorder and remove.
    app.press("Jd");
    assert_eq!(app.queue.len(), 1);
}

/// Episodes known only from history are hidden until `d`, and toggling
/// keeps the cursor on (or next to) the same episode.
#[test]
fn detail_hides_episodes_not_on_disk() {
    let (_d, mut app) = app();
    app.press("3/frieren\n\n");
    let s = render(&mut app);
    assert!(s.contains("Episodes · d shows 26 not on disk"), "{s}");
    assert!(!s.lines().any(|l| l.starts_with('│') && l.contains("not on disk")), "no missing rows:\n{s}");
    let selected = |app: &App| selected_item(app).map(|k| k.describe());
    let next_up = selected(&app);
    app.press("d");
    let s = render(&mut app);
    assert!(s.contains("Episodes · d hides 26 not on disk"), "{s}");
    assert_eq!(items(&app).len(), 26 + app.lib.get("sousou no frieren").unwrap().on_disk_items());
    assert_eq!(selected(&app), next_up, "the cursor stays put");
    // From a hidden row, hiding moves to the first row still shown.
    app.press("g");
    assert_eq!(selected(&app).as_deref(), Some("01"));
    app.press("d");
    assert_eq!(selected(&app).as_deref(), Some("27"));
}

#[test]
fn series_view_filters_and_searches() {
    let (_d, mut app) = app();
    app.press("3");
    let s = render(&mut app);
    assert!(s.contains("Series · on disk"), "{s}");
    assert!(!s.contains("K-On!"), "dropped hidden by default:\n{s}");
    app.press("/aria\n");
    let s = render(&mut app);
    assert!(s.contains("Aria The Animation"), "{s}");
    assert!(!s.contains("One Piece"), "{s}");
    insta::assert_snapshot!("series_search", s);
    app.press("\x1b");
    assert!(app.query.is_empty());
}

/// A season of a franchise links to the seasons around it: a `▲` row first
/// and a `▼` row last (one more k or j past g and G), opened with ⏎ (or `[`
/// and `]`, from any row); Esc
/// still goes back to where Detail was opened from. The header names the
/// seasons short and marks the one shown.
#[test]
fn detail_links_the_seasons_around_it() {
    let (_d, mut app) = app();
    app.press("3/frieren s2\n\n");
    assert_eq!(app.detail_series().map(|s| s.key.as_str()), Some("sousou no frieren s2"));
    let s = render(&mut app);
    insta::assert_snapshot!("detail_seasons", s);
    let key = |app: &App| app.detail_series().map(|s| s.key.clone());
    let first = |app: &App| matches!(app.detail_rows()[0], DetailRow::Season { next: false, .. });
    assert!(first(&app));
    assert!(matches!(app.detail_rows().last(), Some(DetailRow::Item(_))), "no season after it");
    // g is the first episode; the link is one more k. Keys for items do
    // nothing on it.
    app.press("g");
    assert_eq!(app.detail_state.selected(), Some(1));
    app.press("k");
    let watched = app.lib.get("sousou no frieren s2").unwrap().watched_count();
    app.press("wW ");
    assert_eq!(app.lib.get("sousou no frieren s2").unwrap().watched_count(), watched);
    assert!(app.queue.is_empty());
    // ⏎ on it opens the season before, at its next episode.
    app.press("\n");
    assert_eq!(key(&app).as_deref(), Some("sousou no frieren"));
    let next_up = app.detail_series().unwrap().next_up().map(|i| i.key.clone());
    let selected = app.detail_state.selected().and_then(|i| app.detail_rows()[i].item().map(|it| it.key.clone()));
    assert_eq!(selected, next_up);
    assert!(matches!(app.detail_rows().last(), Some(DetailRow::Season { next: true, .. })));
    assert!(!first(&app), "no season before the first");
    // [ and ] from any row; at either end, a message.
    app.press("[");
    assert_eq!(app.current_message(), Some("nothing before this one"));
    app.press("]");
    assert_eq!(key(&app).as_deref(), Some("sousou no frieren s2"));
    app.press("]");
    assert_eq!(app.current_message(), Some("nothing after this one"));
    // On a season link, showing or hiding rows keeps the cursor on it.
    app.press("gk");
    for key in ["d", "x", "d", "x"] {
        app.press(key);
        assert_eq!(app.detail_state.selected(), Some(0), "after {key}");
    }
    app.press("[Gj");
    let last = app.rows_len(View::Detail) - 1;
    for key in ["d", "d"] {
        app.press(key);
        assert_eq!(app.detail_state.selected(), Some(app.rows_len(View::Detail) - 1), "after {key}");
    }
    assert_eq!(app.detail_state.selected(), Some(last));
    app.press("]");
    // Esc goes back to the Series view, not to the season before.
    app.press("\x1b");
    assert_eq!(app.view, View::Series);
}

/// A status imported from fish history (`via`) shows as a little fish in
/// the header's border; a note of yours is quoted, at the end of line 2,
/// whatever it says; one anipv wrote itself isn't shown.
#[test]
fn header_notes_fish_and_yours_only() {
    use anipv::events::{EventBody, StatusVia};
    use anipv::model::SeriesStatus;
    let status = |note: Option<&str>, auto, via| EventBody::SeriesStatus {
        series: "meitantei precure".into(),
        status: SeriesStatus::Following,
        note: note.map(str::to_string),
        auto,
        keep: false,
        via,
    };
    let (_d, mut app) = app();
    app.ctx.record([status(None, false, Some(StatusVia::FishImport))]).unwrap();
    app.reload();
    app.press("3/meitantei\n\n");
    let s = render(&mut app);
    assert!(s.lines().nth(1).is_some_and(|l| l.ends_with("ϵ( 'Θ' )϶ ─╮")), "{s}");

    // Your note, even one reading like what anipv used to write.
    for note in ["cute", "caught up", "from fish history"] {
        app.ctx.record([status(Some(note), false, None)]).unwrap();
        app.reload();
        let s = render(&mut app);
        let line2 = s.lines().nth(3).unwrap();
        assert!(line2.starts_with(&format!("│1/48 · 2 new · TV  “{note}”")), "your note ends line 2:\n{s}");
        assert!(!s.contains("ϵ("), "{s}");
    }

    app.ctx.record([status(Some("all 48 episodes watched"), true, None)]).unwrap();
    app.reload();
    let s = render(&mut app);
    assert!(!s.contains("episodes watched") && !s.contains("ϵ("), "{s}");
}

/// More episodes on disk than AniList counts (a folder numbering on through
/// later seasons): the total is the latest on disk, AniList's in brackets.
/// A total you set yourself stays.
#[test]
fn header_total_counts_what_is_on_disk() {
    use anipv::events::EventBody;
    use anipv::index::db::SeriesMeta;
    // Meitantei with AniList counting `episodes`, and `extra` files added.
    let app_with = |episodes, extra: &[&str]| {
        let (d, mut app) = app();
        let dl = d.path().join("media/Downloads");
        for f in extra {
            std::fs::write(dl.join(f), b"").unwrap();
        }
        app.ctx.scan(None, &|_, _| {}).unwrap();
        let meta = SeriesMeta {
            series: "meitantei precure".into(),
            anilist: Some(195_000),
            episodes: Some(episodes),
            ..SeriesMeta::default()
        };
        app.ctx.store_sync(&anipv::meta::SyncResult { rows: vec![meta], ..Default::default() }).unwrap();
        app.reload_index();
        app.press("3/meitantei\n\n");
        (d, app)
    };
    let line2 = |app: &mut App| render(app).lines().nth(3).unwrap().to_string();

    let (_d, mut app) = app_with(2, &[]);
    assert!(line2(&mut app).starts_with("│1/3 (ⓐ2 eps) · 2 new"), "{l}", l = line2(&mut app));
    // A total you set yourself stays.
    let yours = EventBody::Meta { series: "meitantei precure".into(), anilist: Some(195_000), episodes: Some(2) };
    app.ctx.record([yours]).unwrap();
    app.reload();
    assert!(line2(&mut app).starts_with("│1/2 · 2 new"), "{l}", l = line2(&mut app));

    // An episode 00 and a .5 recap don't count: 01–03 on disk, AniList's 3.
    let extra = ["[GroupB] Meitantei Precure! - 00 (1080p).mkv", "[GroupB] Meitantei Precure! - 02.5 (1080p).mkv"];
    let (_d, mut app) = app_with(3, &extra);
    assert!(line2(&mut app).starts_with("│1/3 · "), "{l}", l = line2(&mut app));
}

/// Names merged into a series show as a count; `U` lists them and asks
/// before splitting them off.
#[test]
fn merged_names_are_counted_and_listed_before_unmerging() {
    let (_d, mut app) = app();
    app.press("3/kusuriya\n\nm");
    app.press("frieren\n");
    assert_eq!(app.detail_series().map(|s| s.key.as_str()), Some("sousou no frieren"));
    let s = render(&mut app);
    assert!(s.contains("+1 name · ★ 90"), "{s}");
    app.press("U");
    let s = render(&mut app);
    assert!(s.contains("kusuriya no hitorigoto s3") && s.contains("y/Enter splits them off"), "{s}");
    app.press("n");
    assert_eq!(app.lib.get("sousou no frieren").unwrap().aliases.len(), 1, "any other key cancels");
    // A name merged in while it asks isn't split off with the listed ones.
    app.press("U");
    let merge = anipv::events::EventBody::Alias { from: "dungeon meshi".into(), to: "sousou no frieren".into() };
    app.ctx.record([merge]).unwrap();
    app.reload();
    app.press("y");
    assert_eq!(app.lib.get("sousou no frieren").unwrap().aliases, ["dungeon meshi"]);
}

/// Space queues and moves down, but not past the last episode onto the
/// link to the next season (so a ⏎ after it still plays).
#[test]
fn space_stays_on_the_episodes() {
    let (_d, mut app) = app();
    app.press("3/frieren\n\nG");
    let last = selected_item(&app);
    app.press(" ");
    assert_eq!(selected_item(&app), last);
    assert_eq!(app.queue.len(), 1);
}

/// A narrow header keeps the title and drops the small facts, least
/// important first; nothing wraps.
#[test]
fn narrow_header_drops_facts_first() {
    let (_d, mut app) = app();
    app.press("3/yuru\n\n");
    let wide = render_sized(&mut app, 110, 28);
    assert!(wide.contains("★ 83 · 251k · ⓐ98444│"), "{wide}");
    let s = render_sized(&mut app, 30, 28);
    let line = s.lines().nth(2).unwrap();
    assert!(line.starts_with("│Yuru Camp") && line.ends_with(" ★ 83 · ⓐ98444│"), "members go first:\n{s}");
    let s = render_sized(&mut app, 22, 28);
    insta::assert_snapshot!("detail_narrow", s);
    let line = s.lines().nth(2).unwrap();
    assert!(line.starts_with("│Yuru Camp") && line.ends_with("★ 83│"), "then the id:\n{s}");
}

#[test]
fn detail_view_marks_and_extras() {
    let (_d, mut app) = app();
    app.press("3/meitantei\n\n");
    let s = render(&mut app);
    assert!(s.contains("Episodes · x shows 3 extras"), "{s}");
    insta::assert_snapshot!("detail", s);
    app.press("x");
    let s = render(&mut app);
    assert!(s.contains("fanart corner"), "{s}");
    app.press("x");
    // Toggle watched on the selected row (episode 2 = next up).
    let before = app.lib.get("meitantei precure").unwrap().watched_count();
    app.press("w");
    assert_eq!(app.lib.get("meitantei precure").unwrap().watched_count(), before + 1);
}

/// Hiding extras keeps the cursor on the same row if it stays, else on the
/// nearest row above it: an unattached extra (at the end) leaves it on the
/// last episode, not on an episode made up from the extra's key.
#[test]
fn hiding_extras_keeps_the_cursor_near() {
    let (_d, mut app) = app();
    let selected = |app: &App| selected_item(app).map(|k| k.describe());
    app.press("3/aria\n\nxG");
    assert_eq!(selected(&app).as_deref(), Some("ncop"));
    app.press("x");
    assert_eq!(selected(&app).as_deref(), Some("13"));
    app.press("x");
    assert_eq!(selected(&app).as_deref(), Some("13"), "a row still shown stays put");

    // Extras are shown again (the toggle is session-wide).
    app.press("\x1b\x1b3/meitantei\n\n");
    app.press("g");
    app.press("jjj");
    assert_eq!(selected(&app).as_deref(), Some("02 · fanart corner"));
    app.press("x");
    assert_eq!(selected(&app).as_deref(), Some("02"), "an attached extra goes to its episode");
}

#[test]
fn status_popup_sets_status_with_note() {
    let (_d, mut app) = app();
    app.press("3/yuru\n");
    app.press("s");
    let s = render(&mut app);
    assert!(s.contains("following"), "{s}");
    app.press("d");
    // Dropped asks for a note.
    app.press("too comfy\n");
    let y = app.lib.get("yuru camp").unwrap();
    assert_eq!(y.status, anipv::model::SeriesStatus::Dropped);
    assert_eq!(y.note.as_deref(), Some("too comfy"));
}

#[test]
fn merge_popup_merges_series() {
    let (_d, mut app) = app();
    app.press("3/kusuriya\n");
    app.press("m");
    app.press("frieren\n");
    assert_eq!(app.lib.get("kusuriya no hitorigoto s3").unwrap().key, "sousou no frieren", "old key finds the target");
    assert_eq!(app.lib.get("sousou no frieren").unwrap().aliases, vec!["kusuriya no hitorigoto s3"]);
}

/// Merging the series shown in Detail switches to the target with a usable
/// cursor (keys like `w` act on it right away).
#[test]
fn merge_from_detail_keeps_a_cursor() {
    let (_d, mut app) = app();
    app.press("3/kusuriya\n\n");
    let selected = selected_item;
    let item = selected(&app).unwrap();
    app.press("m");
    app.press("frieren\n");
    assert_eq!(app.detail_series().map(|s| s.key.as_str()), Some("sousou no frieren"));
    // The cursor stays on the item it was on, now listed under the target.
    assert_eq!(selected(&app), Some(item));
    app.press("\x1b");
    assert_eq!(app.lib.get("sousou no frieren").unwrap().aliases, vec!["kusuriya no hitorigoto s3"]);
}

/// `i` shows paused series even when nothing is being followed.
#[test]
fn paused_toggle_works_on_an_empty_up_next() {
    use anipv::model::SeriesStatus;
    let (_d, mut app) = app();
    let following: Vec<String> = app.upnext_rows_in(0..usize::MAX).iter().map(|s| s.key.clone()).collect();
    for key in &following {
        app.ctx.set_status(&app.lib, key, SeriesStatus::Paused, None).unwrap();
    }
    app.reload();
    app.press("1");
    assert!(app.upnext_rows_in(0..usize::MAX).is_empty());
    app.press("i");
    assert!(!app.upnext_rows_in(0..usize::MAX).is_empty(), "paused series show up");
    app.press("i");
    assert!(app.upnext_rows_in(0..usize::MAX).is_empty());
}

/// mpv's report of a file that ended where it got furthest (no seeking back).
fn ended(path: std::path::PathBuf, pos: f64, dur: Option<f64>, eof: bool) -> anipv::mpv::PlayerEvent {
    anipv::mpv::PlayerEvent::Ended { path, pos, furthest: pos, dur, eof }
}

/// Run until the (fake) player has exited.
fn wait_for_player(app: &mut App) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while app.playing.is_some() && std::time::Instant::now() < deadline {
        app.wait_msg(std::time::Duration::from_millis(100));
    }
    assert!(app.playing.is_none(), "player never exited");
}

#[test]
fn queue_survives_a_player_that_exits_immediately() {
    // The demo's player is `true`: it exits without ever opening the IPC socket.
    let (_d, mut app) = app();
    queue_first_two(&mut app);
    app.play_queue(true);
    assert!(app.playing.is_some());
    wait_for_player(&mut app);
    assert_eq!(app.queue.len(), 2, "queue kept");
    assert!(app.current_message().unwrap_or_default().contains("exited before playing"));
}

/// A queued file stays queued while it plays and when it is stopped
/// partway (started by mistake, say); it leaves once it has been watched.
#[test]
fn queue_is_consumed_once_watched() {
    use anipv::mpv::PlayerEvent;
    use anipv::tui::app::Msg;
    let (_d, mut app) = app();
    app.press(" ");
    let path = app.queue[0].file.clone();
    app.play_queue(true);
    app.on_msg(Msg::Player(PlayerEvent::Started { path: path.clone() }));
    assert_eq!(app.queue.len(), 1, "still queued while playing");
    app.on_msg(Msg::Player(ended(path.clone(), 60.0, Some(1420.0), false)));
    assert_eq!(app.queue.len(), 1, "stopped partway");
    app.on_msg(Msg::Player(PlayerEvent::Started { path: path.clone() }));
    app.on_msg(Msg::Player(PlayerEvent::Ended { path, pos: 1400.0, furthest: 1400.0, dur: Some(1420.0), eof: true }));
    assert!(app.queue.is_empty(), "watched");
    wait_for_player(&mut app);
}

/// Of a session's files, only those watched leave the queue: one skipped
/// (or failed) stays. The playing file is known (and highlighted), and the
/// cursor stays on its row as the rows above it go.
#[test]
fn only_watched_files_leave_the_queue() {
    use anipv::mpv::PlayerEvent;
    use anipv::tui::app::Msg;
    let (_d, mut app) = app();
    app.press("   5");
    let [a, b, c] = [0, 1, 2].map(|i| app.queue[i].file.clone());
    app.press("jj");
    app.play_queue(true);
    app.on_msg(Msg::Player(PlayerEvent::Started { path: a.clone() }));
    assert_eq!(app.playing_file(), Some(a.as_path()), "the playing entry");
    app.on_msg(Msg::Player(ended(a.clone(), 0.0, Some(1420.0), false)));
    assert_eq!(app.playing_file(), None, "nothing playing between files");
    app.on_msg(Msg::Player(PlayerEvent::Started { path: b.clone() }));
    app.on_msg(Msg::Player(ended(b, 1400.0, Some(1420.0), true)));
    let left: Vec<_> = app.queue.iter().map(|q| q.file.clone()).collect();
    assert_eq!(left, [a, c.clone()], "the skipped file stays");
    assert_eq!(app.queue_state.selected().map(|i| &app.queue[i].file), Some(&c), "cursor kept");
    wait_for_player(&mut app);
}

/// `p` and ⏎ resume a started episode where it stopped; `P` and shift-⏎ play
/// it from the beginning.
#[test]
fn resume_or_play_from_the_beginning() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (d, mut app) = app();
    // An "mpv" that writes down its arguments and exits.
    let log = d.path().join("mpv-args");
    app.ctx.cfg.mpv = "sh".into();
    app.ctx.cfg.mpv_args =
        vec!["-c".into(), format!("printf '%s\\n' \"$@\" > '{log}'", log = log.display()), "sh".into()];
    let started_at = |app: &mut App, key: KeyEvent| {
        let _ = std::fs::remove_file(&log);
        app.on_key(key);
        wait_for_player(app);
        let args = std::fs::read_to_string(&log).unwrap();
        args.lines().find_map(|a| a.strip_prefix("--start=")).map(str::to_owned)
    };
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
    // Ranma's episode 1 is 42% in.
    app.press("3/ranma\n\n");
    assert_eq!(selected_item(&app).map(|k| k.describe()).as_deref(), Some("01"));
    let resume = started_at(&mut app, key(KeyCode::Enter));
    assert!(resume.is_some(), "⏎ resumes");
    assert_eq!(started_at(&mut app, key(KeyCode::Char('p'))), resume, "p resumes");
    assert_eq!(started_at(&mut app, key(KeyCode::Char('P'))), None, "P from the beginning");
    assert_eq!(started_at(&mut app, shift_enter), None, "shift-⏎ too");
    // The queue (`p` queued the episode it played): `P` anywhere else (a
    // season link included), shift-⏎ in the Queue view.
    app.press("3\x1b/nisemono\n\ngk");
    assert_eq!(selected_item(&app), None, "on the ▲ link");
    assert_eq!(started_at(&mut app, key(KeyCode::Char('P'))), None);
    app.press("5");
    assert_eq!(app.queue.len(), 1);
    assert_eq!(started_at(&mut app, key(KeyCode::Enter)), resume);
    assert_eq!(started_at(&mut app, key(KeyCode::Char('P'))), None);
    assert_eq!(started_at(&mut app, shift_enter), None);
}

/// A failed write must stay visible instead of being replaced by "Show → status".
#[test]
fn write_errors_are_not_hidden_by_success_messages() {
    use std::os::unix::fs::PermissionsExt;
    let (d, mut app) = app();
    let log = d.path().join("data/events/desktop.jsonl");
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().append(true).open(&log).is_ok() {
        return; // running as root
    }
    app.press("3/yuru\nsf");
    let msg = app.current_message().unwrap_or_default().to_string();
    assert!(msg.contains("could not write event log"), "{msg}");
}

/// A metadata sync that started before a manual link change must not write
/// its (now stale) automatic match over the new link, but the rest of what
/// it found is kept.
#[test]
fn stale_metadata_sync_results_are_discarded() {
    use anipv::index::db::{MatchAttempt, SeriesMeta};
    use anipv::meta::SyncResult;
    use anipv::tui::app::Msg;
    let (_d, mut app) = app();
    // The user links Yuru Camp by hand (clearing the prefilled id first).
    app.press("3/yuru\nL\x7f\x7f\x7f\x7f\x7f\x7f12345\n");
    // A sync from before the change reports the old automatic match, along
    // with results for other series.
    let stale = SeriesMeta { series: "yuru camp".into(), anilist: Some(98444), ..SeriesMeta::default() };
    let other = SeriesMeta { series: "one piece".into(), anilist: Some(21), ..SeriesMeta::default() };
    let none = MatchAttempt {
        series: "dungeon meshi".into(),
        db_version: "v".into(),
        at: 1,
        result: anipv::index::db::MatchResult::None,
    };
    let res = SyncResult {
        rows: vec![stale, other],
        attempts: vec![none],
        prequels: vec![(21, vec![])],
        ..SyncResult::default()
    };
    app.on_msg(Msg::Meta(res, 0));
    let cached = app.ctx.db.all_meta().unwrap();
    assert!(!cached.iter().any(|m| m.series == "yuru camp" && m.anilist == Some(98444)), "{cached:?}");
    assert_eq!(app.lib.get("yuru camp").unwrap().anilist_id(), Some(12345));
    assert!(cached.iter().any(|m| m.series == "one piece" && m.anilist == Some(21)), "{cached:?}");
    assert!(app.ctx.db.match_attempts().unwrap().iter().any(|a| a.series == "dungeon meshi"));
}

#[test]
fn inbox_lists_new_shows_with_sequels_first() {
    let (_d, mut app) = app();
    app.press("2");
    let s = render(&mut app);
    assert!(s.contains("Inbox · new in your download folders"), "{s}");
    let rows: Vec<&str> = s.lines().filter(|l| l.starts_with('│') && !l.contains("Series ")).collect();
    assert!(
        rows[0].contains("Sousou no Frieren S2") && rows[0].contains("sequel to Sousou no Frieren · following"),
        "{s}"
    );
    let fan_letter = rows.iter().position(|l| l.contains("One Piece Fan Letter")).unwrap();
    let dungeon = rows.iter().position(|l| l.contains("Dungeon Meshi")).unwrap();
    assert!(fan_letter > dungeon, "side entries go last:\n{s}");
    assert!(rows[fan_letter].contains("part of One Piece"), "{s}");
    assert!(!s.contains("Yuru Camp"), "archive-only series are not in the inbox:\n{s}");
    insta::assert_snapshot!("inbox", s);
}

#[test]
fn inbox_triage_follows_and_skips() {
    use anipv::model::SeriesStatus;
    let (_d, mut app) = app();
    app.press("2");
    let before = app.inbox_rows().len();
    // First row is the sequel: skip it.
    app.press("s");
    assert_eq!(app.lib.get("sousou no frieren s2").unwrap().status, SeriesStatus::Skipped);
    assert_eq!(app.inbox_rows().len(), before - 1);
    // Follow Dungeon Meshi: it leaves the inbox and shows up in Up next.
    let i = app.inbox_rows().iter().position(|(s, _)| s.key == "dungeon meshi").unwrap();
    app.inbox_state.select(Some(i));
    app.press("f");
    assert_eq!(app.lib.get("dungeon meshi").unwrap().status, SeriesStatus::Following);
    assert!(!app.inbox_rows().iter().any(|(s, _)| s.key == "dungeon meshi"));
    assert!(app.upnext_rows_in(0..usize::MAX).iter().any(|s| s.key == "dungeon meshi"));
    // `z` = later (paused).
    app.inbox_state.select(Some(0));
    let key = app.inbox_rows()[0].0.key.clone();
    app.press("z");
    assert_eq!(app.lib.get(&key).unwrap().status, SeriesStatus::Paused);
}

/// Run until the background metadata update (and any queued one) is done.
fn wait_for_meta(app: &mut App) {
    assert!(app.wait_for_meta(std::time::Duration::from_secs(20)), "metadata update never finished");
}

/// `M` refreshes the series under the cursor and says what it found. The demo
/// has network metadata off, so this runs offline.
#[test]
fn m_refreshes_metadata_for_the_focused_series() {
    let (_d, mut app) = app();
    app.press("3/yuru\nM");
    assert!(app.meta_busy());
    wait_for_meta(&mut app);
    let msg = app.current_message().unwrap_or_default().to_string();
    assert!(msg.starts_with("Yuru Camp:") && msg.contains("12 eps"), "{msg}");
}

/// `o` opens the focused series' AniList page (here: recorded, not opened);
/// an unlinked series says how to link it, and a failing opener shows the URL.
#[test]
fn o_opens_the_anilist_page() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let (_d, mut app) = app();
    let opened = Rc::new(RefCell::new(Vec::new()));
    let rec = Rc::clone(&opened);
    app.opener = Box::new(move |url| {
        rec.borrow_mut().push(url.to_string());
        Ok(())
    });
    app.press("2jo");
    assert_eq!(*opened.borrow(), ["https://anilist.co/anime/153518"], "Dungeon Meshi, from the Inbox");
    assert_eq!(app.current_message(), Some("opened https://anilist.co/anime/153518"));
    app.press("jo");
    assert_eq!(opened.borrow().len(), 1, "Kusuriya no Hitorigoto S3 isn't linked");
    assert!(app.current_message().unwrap_or_default().contains("not linked to AniList"));
}

/// When no browser can be started, `o` shows the URL instead.
#[test]
fn o_shows_the_url_without_an_opener() {
    let (_d, mut app) = app();
    app.opener = Box::new(|_| anyhow::bail!("no opener"));
    app.press("3/yuru\n\no");
    assert_eq!(app.current_message(), Some("https://anilist.co/anime/98444"));
    let s = render(&mut app);
    assert!(s.contains("★ 83 · 251k · ⓐ98444") && s.contains("anilist.co/anime/98444"), "{s}");
}

/// `ctrl-r` refreshes everything; asking again while it runs queues one more.
#[test]
fn ctrl_r_refreshes_everything_and_queues_while_busy() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (_d, mut app) = app();
    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    app.on_key(ctrl_r);
    assert!(app.meta_busy());
    app.on_key(ctrl_r);
    assert!(app.current_message().unwrap_or_default().contains("queued"));
    wait_for_meta(&mut app);
    assert!(app.current_message().unwrap_or_default().starts_with("metadata:"));
}

/// Playing in remote mode: the bar says so (and shows a pause), the footer
/// lists only the keys anipv keeps; after ctrl-c the usual hints are back,
/// with `v` to hand the keys to mpv again.
#[test]
fn now_playing_in_remote_mode() {
    use anipv::tui::app::Msg;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (_d, mut app) = app();
    let (ours, _mpv) = std::os::unix::net::UnixStream::pair().unwrap();
    app.queue.push(anipv::app::QueueEntry {
        series: "one piece".into(),
        item: anipv::model::ItemKey::episode(anipv::model::EpNo::new(1182)),
        file: "/m/op.mkv".into(),
    });
    app.playing = Some(anipv::tui::app::NowPlaying {
        label: "Sousou no Frieren 29".into(),
        pos: 802.0,
        dur: Some(1420.0),
        total: 3,
        ..Default::default()
    });
    // Until mpv connects, keys can't reach it: no `v`, and the queue can still be played.
    let s = render(&mut app);
    assert!(!s.contains("control mpv") && s.contains("p play queue"), "{s}");
    app.on_msg(Msg::Player(anipv::mpv::PlayerEvent::Connected(anipv::mpv::Remote::new(ours))));
    app.on_msg(Msg::Player(anipv::mpv::PlayerEvent::Paused(true)));
    let s = render(&mut app);
    assert!(s.contains("⏸ Sousou no Frieren 29"), "{s}");
    assert!(s.contains("REMOTE: keys go to mpv, ctrl-c for anipv"), "{s}");
    assert!(s.contains("ctrl-c back to anipv") && !s.contains("queue next"), "{s}");
    insta::assert_snapshot!("now_playing_remote", s);

    app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let s = render(&mut app);
    assert!(!s.contains("REMOTE"), "{s}");
    assert!(s.contains("v control mpv") && s.contains("queue next"), "{s}");
}

/// On a short terminal the help scrolls with j/k (and stops at its end);
/// any other key closes it.
#[test]
fn help_scrolls_when_taller_than_the_terminal() {
    let (_d, mut app) = app();
    app.press("?");
    let s = render(&mut app);
    let total = ui::HELP.iter().map(|(_, keys)| keys.len() + 2).sum::<usize>() - 1;
    assert!(s.contains(&format!("Keys 1-24 of {total} · j/k scroll")), "{s}");
    assert!(!s.contains("Remote mode (while mpv plays)"), "the end is below:\n{s}");
    app.press(&"j".repeat(200));
    let s = render(&mut app);
    assert!(s.contains(&format!("of {total} ·")) && s.contains("Remote mode (while mpv plays)"), "{s}");
    assert!(s.contains(&format!("Keys {first}-{total} of", first = total - 23)), "scrolling stops at the end:\n{s}");
    app.press("k");
    assert!(render(&mut app).contains(&format!("Keys {first}-{last} of", first = total - 24, last = total - 1)));
    app.press("x");
    assert!(app.popup.is_none());
}

/// On a terminal tall enough for all of it, any key closes the help, the
/// scroll keys too.
#[test]
fn help_that_fits_closes_on_any_key() {
    let (_d, mut app) = app();
    for key in ["j", " ", "k", "x"] {
        app.press("?");
        let s = render_sized(&mut app, 110, 80);
        assert!(s.contains("Keys · any key closes"), "{s}");
        app.press(key);
        assert!(app.popup.is_none(), "{key:?} closes it");
    }
}

#[test]
fn help_and_quit() {
    let (_d, mut app) = app();
    app.press("?");
    let s = render(&mut app);
    assert!(s.contains("Keys") && s.contains("scroll · other keys close"), "{s}");
    app.press("x");
    assert!(app.popup.is_none());
    app.press("q");
    assert!(!app.quit, "quitting asks first");
    let s = render(&mut app);
    assert!(s.contains("Quit anipv?"), "{s}");
    app.press("q");
    assert!(app.quit);
}

/// Enter in the episode list plays that item alone and leaves the queue as it was.
#[test]
fn enter_in_detail_plays_only_that_item() {
    let (_d, mut app) = app();
    // Frieren opens on the next episode (29): queue it and the one after.
    app.press("3/frieren\n\n  ");
    let queued: Vec<_> = app.queue.iter().map(|q| q.item.describe()).collect();
    assert_eq!(queued.len(), 2, "{queued:?}");
    app.press("g");
    app.press("d");
    app.press("g");
    // Row 0 is episode 01, which is not on disk: nothing starts.
    app.press("\n");
    assert!(app.playing.is_none());
    assert_eq!(app.current_message(), Some("not on disk"));
    assert_eq!(app.queue.len(), 2);
    // Back to the on-disk rows: Enter on the third (not queued) episode.
    app.press("d");
    let rows = app.detail_rows();
    let idx = rows
        .iter()
        .position(|r| r.item().is_some_and(|it| it.present() && !app.is_queued("sousou no frieren", &it.key)))
        .expect("an episode on disk that is not queued");
    drop(rows);
    app.detail_state.select(Some(idx));
    let before = app.queue.clone();
    app.press("\n");
    assert!(app.playing.is_some(), "{message:?}", message = app.current_message());
    assert_eq!(app.queue, before, "the queue is untouched");
    // Another Enter while mpv runs changes nothing either.
    app.press("\n");
    assert_eq!(app.current_message(), Some("mpv is already running"));
    assert_eq!(app.queue, before);
    wait_for_player(&mut app);
    assert_eq!(app.queue, before);
}

/// `p` still falls back to the series under the cursor when nothing is queued.
#[test]
fn p_queues_the_next_episode_when_the_queue_is_empty() {
    let (_d, mut app) = app();
    assert!(app.queue.is_empty());
    app.press("p");
    assert_eq!(app.queue.len(), 1);
    assert!(app.playing.is_some());
    wait_for_player(&mut app);
}

#[test]
fn play_queue_with_nothing_queued_says_so() {
    let (_d, mut app) = app();
    app.play_queue(true);
    assert!(app.playing.is_none());
    assert_eq!(app.current_message(), Some("queue is empty"));
}

/// Only the rows around the cursor are drawn, and the view scrolls with it.
#[test]
fn long_episode_list_scrolls_with_the_cursor() {
    let (_d, mut app) = app();
    app.press("3/frieren\n\nd");
    let total = app.detail_rows().len();
    assert!(total > 25, "{total}");
    app.press("g");
    let s = render(&mut app);
    assert!(s.lines().any(|l| l.contains("│▌") && l.contains(" 01 ")), "first row is highlighted:\n{s}");
    assert_eq!(app.detail_state.offset(), 0);
    app.press("G");
    let s = render(&mut app);
    // G is the last episode; the link to the next season is one more j.
    let last = items(&app).last().unwrap().key.describe();
    assert!(s.lines().any(|l| l.contains("│▌") && l.contains(&format!(" {last} "))), "last episode selected:\n{s}");
    assert!(!s.contains(" 01 "), "scrolled past the top:\n{s}");
    let offset = app.detail_state.offset();
    assert!(offset > 0 && offset < total);
    // Moving up inside the window does not scroll.
    app.press("kk");
    render(&mut app);
    assert_eq!(app.detail_state.offset(), offset);
    // Moving above it does.
    app.press("g");
    render(&mut app);
    assert_eq!(app.detail_state.offset(), 0);
}

/// The Series view draws only what fits too, and still reaches the last row.
#[test]
fn series_view_windows_to_the_terminal_height() {
    let (_d, mut app) = app();
    app.press("3");
    app.press("f");
    let mut t = Terminal::new(TestBackend::new(110, 8)).unwrap();
    t.draw(|f| ui::draw(f, &mut app)).unwrap();
    app.press("G");
    t.draw(|f| ui::draw(f, &mut app)).unwrap();
    let last = app.series_rows_in(0..usize::MAX).last().unwrap().title.clone();
    let buf = t.backend().buffer().clone();
    let screen: String = (0..buf.area.height)
        .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
        .collect();
    assert!(screen.contains(&last), "{screen}");
    assert!(app.series_state.offset() > 0 || app.series_rows_in(0..usize::MAX).len() <= 3, "{screen}");
}

/// The cheap row count agrees with the rows themselves under every toggle.
#[test]
fn detail_len_matches_detail_rows() {
    let (_d, mut app) = app();
    for series in ["frieren", "meitantei", "yuru", "kusuriya"] {
        app.press(&format!("3/{series}\n\n"));
        for toggles in ["", "x", "d", "x"] {
            app.press(toggles);
            assert_eq!(app.rows_len(View::Detail), app.detail_rows().len(), "{series} after {toggles}");
        }
        app.press("\x1b\x1b");
    }
}

/// The merge picker keeps its candidates until the query changes, scrolls with
/// the cursor and re-filters from the top.
#[test]
fn merge_picker_filters_and_scrolls() {
    use anipv::tui::app::Popup;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (_d, mut app) = app();
    app.press("3/kusuriya\nm");
    let picker = |app: &App| match &app.popup {
        Some(Popup::Merge(m)) => m.clone(),
        other => panic!("{other:?}"),
    };
    let all = picker(&app);
    assert!(all.targets.len() > 3, "{targets:?}", targets = all.targets);
    assert!(!all.targets.contains(&"kusuriya no hitorigoto s3".to_string()));
    assert_eq!(all.state.selected(), Some(0));
    // Arrows move the cursor without touching the list.
    for _ in 0..30 {
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    let moved = picker(&app);
    assert_eq!(moved.targets, all.targets);
    assert_eq!(moved.state.selected(), Some(all.targets.len() - 1));
    let s = render(&mut app);
    assert!(s.contains("Merge"), "{s}");
    assert!(picker(&app).state.offset() > 0 || all.targets.len() < 16);
    // Typing re-filters and puts the cursor back on the top.
    app.press("frie");
    let narrowed = picker(&app);
    assert!(narrowed.targets.len() < all.targets.len());
    assert_eq!(narrowed.state.selected(), Some(0));
    assert_eq!(narrowed.state.offset(), 0);
    // No match: no cursor, and Enter merges nothing.
    app.press("zzzzqq");
    assert_eq!(picker(&app).state.selected(), None);
    app.press("\n");
    assert!(app.popup.is_none());
    assert!(app.lib.get("kusuriya no hitorigoto s3").unwrap().aliases.is_empty());
}

/// Scan progress redraws at most every 100 ms, and only when it shows something new.
#[test]
fn scan_progress_is_throttled() {
    use anipv::tui::app::Msg;
    let (_d, mut app) = app();
    assert!(app.on_msg(Msg::ScanProgress("anime".into(), 1)));
    assert_eq!(app.scanning.as_deref(), Some("scanning anime… 1"));
    assert!(!app.on_msg(Msg::ScanProgress("anime".into(), 2)), "too soon after the last one");
    assert_eq!(app.scanning.as_deref(), Some("scanning anime… 2"), "the text is kept anyway");
    std::thread::sleep(std::time::Duration::from_millis(120));
    assert!(!app.on_msg(Msg::ScanProgress("anime".into(), 2)), "same text: nothing to redraw");
    assert!(app.on_msg(Msg::ScanProgress("anime".into(), 3)));
    assert_eq!(app.scanning.as_deref(), Some("scanning anime… 3"));
}

/// Progress always keeps the latest count; the throttle only holds back the
/// redraw, and a new root redraws right away.
#[test]
fn scan_progress_keeps_the_latest_count_and_redraws_on_a_new_root() {
    use anipv::tui::app::Msg;
    let (_d, mut app) = app();
    assert!(app.on_msg(Msg::ScanProgress("anime".into(), 1)));
    assert!(!app.on_msg(Msg::ScanProgress("anime".into(), 2)), "too soon to redraw");
    assert_eq!(app.scanning.as_deref(), Some("scanning anime… 2"), "but the text is current");
    assert!(app.on_msg(Msg::ScanProgress("archive".into(), 1)), "another root redraws at once");
    assert_eq!(app.scanning.as_deref(), Some("scanning archive… 1"));
}

/// A reload (after a scan, say) keeps the cursor on the same row by key in
/// every view, even when rows before it come and go.
#[test]
fn reload_keeps_selections_by_key() {
    use anipv::model::SeriesStatus;
    let (d, mut app) = app();
    // Inbox: the second row; the first is then triaged elsewhere.
    let inbox: Vec<String> = app.inbox_rows().iter().map(|(s, _)| s.key.clone()).collect();
    assert!(inbox.len() > 2, "{inbox:?}");
    app.inbox_state.select(Some(1));
    // Files: the second row.
    app.files_state.select(Some(1));
    let file = app.file_rows_in(1..2)[0].file.path.clone();
    // Detail: the episode after next up.
    app.press("3/frieren\n\nj");
    let item = app.detail_rows()[app.detail_state.selected().unwrap()].item().unwrap().key.clone();
    // Behind the TUI's back: skip the first inbox row, and a new (earlier)
    // Frieren episode arrives, adding rows before the selected file and episode.
    app.ctx.set_status(&app.lib, &inbox[0], SeriesStatus::Skipped, None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let new = d.path().join("media/Downloads/[GroupA] Sousou no Frieren - 26 (1080p) [ABCD0026].mkv");
    std::fs::write(&new, b"").unwrap();
    app.ctx.scan(None, &|_, _| {}).unwrap();
    app.reload_index();
    assert_eq!(app.file_rows_in(0..1)[0].file.path, new, "the new file is listed first");
    assert_eq!(app.selected_inbox().map(|s| s.key.as_str()), Some(inbox[1].as_str()));
    let files_sel = app.files_state.selected().unwrap();
    assert_eq!(app.file_rows_in(files_sel..files_sel + 1)[0].file.path, file);
    assert_eq!(app.detail_rows()[app.detail_state.selected().unwrap()].item().unwrap().key, item);
}

/// The merge picker keeps its cursor on the same target across a reload.
#[test]
fn reload_keeps_the_merge_picker_target() {
    use anipv::tui::app::Popup;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (_d, mut app) = app();
    app.press("3/kusuriya\nma");
    for _ in 0..3 {
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    let target = |app: &App| match &app.popup {
        Some(Popup::Merge(m)) => m.state.selected().map(|i| m.targets[i].clone()),
        other => panic!("{other:?}"),
    };
    let before = target(&app).unwrap();
    // A series listed before the target goes away (merged elsewhere).
    let gone = match &app.popup {
        Some(Popup::Merge(m)) => m.targets[0].clone(),
        _ => unreachable!(),
    };
    let other = match &app.popup {
        Some(Popup::Merge(m)) => m.targets.iter().find(|t| **t != gone && **t != before).unwrap().clone(),
        _ => unreachable!(),
    };
    app.ctx.record([anipv::events::EventBody::Alias { from: gone, to: other }]).unwrap();
    app.reload();
    assert_eq!(target(&app).as_deref(), Some(before.as_str()));
}

/// If mpv cannot be started, `p` takes back the episode it queued.
#[test]
fn p_unqueues_when_the_player_fails_to_start() {
    let (_d, mut app) = app();
    app.ctx.cfg.mpv = "/nonexistent/anipv-test-player".into();
    assert!(app.queue.is_empty());
    app.press("p");
    assert!(app.playing.is_none());
    assert!(app.queue.is_empty(), "{queue:?}", queue = app.queue);
    // An existing queue is left alone.
    app.press(" ");
    let queued = app.queue.clone();
    app.press("p");
    assert_eq!(app.queue, queued);
}

/// Events with an item kind from a newer version never reach the library
/// (debug builds assert this on every reload).
#[test]
fn unknown_item_kinds_stay_out_of_the_library() {
    use std::io::Write as _;
    let (d, mut app) = app();
    let log = d.path().join("data/events/desktop.jsonl");
    let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    writeln!(
        f,
        r#"{{"ts":{NOW},"dev":"desktop","e":"watched","series":"yuru camp","item":{{"kind":"ova2","ep":"1"}}}}"#
    )
    .unwrap();
    app.reload();
    assert!(app.lib.series.iter().flat_map(|s| &s.items).all(|i| i.key.kind.is_known()));
}

/// Keys held across a reload (Detail, queue entries) follow a merge made
/// elsewhere, so what is written for them names the series they are now.
#[test]
fn held_keys_follow_a_merge() {
    use anipv::events::EventBody;
    let (_d, mut app) = app();
    app.press("3/kusuriya\n\n ");
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.detail_key.as_deref(), Some("kusuriya no hitorigoto s3"));
    let alias = EventBody::Alias { from: "kusuriya no hitorigoto s3".into(), to: "sousou no frieren".into() };
    app.ctx.record([alias]).unwrap();
    app.reload();
    assert_eq!(app.detail_key.as_deref(), Some("sousou no frieren"));
    assert_eq!(app.queue[0].series, "sousou no frieren");

    // A popup opened before a merge writes under the key the series has now.
    app.press("R");
    let alias = EventBody::Alias { from: "sousou no frieren".into(), to: "yuru camp".into() };
    app.ctx.record([alias]).unwrap();
    app.reload();
    assert!(app.popup.is_some(), "a reload leaves an open popup alone");
    app.press("!\n");
    let events = app.ctx.log.load_all().unwrap().events;
    let title = events.iter().rev().find(|e| matches!(e.body, EventBody::Title { .. })).unwrap();
    assert!(matches!(&title.body, EventBody::Title { series, .. } if series == "yuru camp"), "{title:?}");
}

/// Actions don't read the event log again (here another device's log has
/// become unreadable since it was read), but events arriving from elsewhere
/// are picked up on the next reload.
#[test]
fn actions_reuse_the_loaded_event_log() {
    use anipv::events::{EventBody, EventLog};
    use anipv::model::SeriesStatus;
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let ctx = anipv::demo::setup(dir.path(), NOW).unwrap();
    let events = ctx.paths.events_dir.clone();
    let laptop = EventLog::open(&events, "laptop").unwrap();
    laptop.append([EventBody::status("one piece", SeriesStatus::Paused, None)]).unwrap();
    let mut app = App::new(ctx).unwrap();
    assert_eq!(app.lib.get("one piece").unwrap().status, SeriesStatus::Paused);

    let other = events.join("laptop.jsonl");
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&other).is_err() {
        app.press("3/yuru\nsf");
        let msg = app.current_message().unwrap_or_default().to_string();
        assert!(!msg.contains("failed"), "{msg}");
        assert_eq!(app.lib.get("yuru camp").unwrap().status, SeriesStatus::Following);
        assert_eq!(app.lib.get("one piece").unwrap().status, SeriesStatus::Paused, "still known");
    } // else running as root: nothing is unreadable.
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o644)).unwrap();

    laptop.append([EventBody::status("one piece", SeriesStatus::Dropped, None)]).unwrap();
    app.reload();
    assert_eq!(app.lib.get("one piece").unwrap().status, SeriesStatus::Dropped, "synced in");
}

/// The Detail rows are kept between key presses, and follow a reload (here a
/// merge that brings in another series' episodes).
#[test]
fn detail_rows_follow_a_reload() {
    use anipv::events::EventBody;
    let (_d, mut app) = app();
    app.press("3/yuru\n\n");
    let shown = |app: &App| -> Vec<String> { items(app).iter().map(|i| i.key.describe()).collect() };
    let expected = |app: &App| -> Vec<String> {
        let s = app.detail_series().unwrap();
        s.ordered_items(app.episodes_shown.extras, app.episodes_shown.missing)
            .iter()
            .map(|i| i.key.describe())
            .collect()
    };
    let before = shown(&app);
    assert_eq!(before, expected(&app));
    app.ctx.record([EventBody::Alias { from: "sousou no frieren".into(), to: "yuru camp".into() }]).unwrap();
    app.reload();
    assert_eq!(shown(&app), expected(&app));
    assert!(shown(&app).len() > before.len());
    app.press("d");
    assert_eq!(shown(&app), expected(&app));
}

/// The queue scrolls like the other lists: after it shrinks while scrolled
/// down, it fills the view from the top again instead of showing blank rows.
#[test]
fn queue_fills_the_view_after_it_shrinks() {
    use anipv::app::QueueEntry;
    use anipv::model::{EpNo, ItemKey};
    let (_d, mut app) = app();
    let entry = |n: u32| QueueEntry {
        series: format!("show {n}"),
        item: ItemKey::episode(EpNo::new(1)),
        file: format!("/m/{n}.mkv").into(),
    };
    app.queue = (1..=100).map(entry).collect();
    app.press("5G");
    assert!(render(&mut app).contains("show 100"));
    app.queue.truncate(3);
    let s = render(&mut app);
    assert!(s.contains("show 1 ") && s.contains("show 3"), "{s}");
}

/// The Files view's folder tree: a franchise folder, its entry folders'
/// watch marks and counts, and a folder queued with space.
#[test]
fn files_tree_shows_folders_with_what_is_watched() {
    let (_d, mut app) = app();
    // 4 again: the roots → Anime → Monogatari Series.
    app.press("44");
    app.press("j\n");
    app.press("jjj\n");
    app.press("j ");
    let s = render(&mut app);
    assert!(s.contains("Files · Anime/Monogatari Series · 3"), "{s}");
    assert!(s.contains("✓ 01 - Bakemonogatari/") && s.contains("15/15"), "{s}");
    assert!(s.contains("▶ 02 - Nisemonogatari/") && s.contains("3/11"), "{s}");
    assert!(s.contains("queued 8 from 02 - Nisemonogatari"), "{s}");
    insta::assert_snapshot!("files_tree", s);
}
