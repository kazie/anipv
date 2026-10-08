//! Render the TUI against the demo library on a fake terminal and snapshot
//! each screen. Run `cargo insta review` after intentional UI changes.
#![expect(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]

use anipv::tui::app::View;
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

fn render(app: &mut App) -> String {
    let mut t = Terminal::new(TestBackend::new(110, 28)).unwrap();
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
    app.press("  ");
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
    let selected = |app: &App| app.detail_state.selected().map(|i| app.detail_rows()[i].key.describe());
    let next_up = selected(&app);
    app.press("d");
    let s = render(&mut app);
    assert!(s.contains("Episodes · d hides 26 not on disk"), "{s}");
    assert_eq!(app.detail_rows().len(), 26 + app.lib.get("sousou no frieren").unwrap().on_disk_items());
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
    let selected = |app: &App| app.detail_state.selected().map(|i| app.detail_rows()[i].key.describe());
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
    let selected = |app: &App| app.detail_state.selected().map(|i| app.detail_rows()[i].key.clone());
    let item = selected(&app).unwrap();
    app.press("m");
    app.press("frieren\n");
    assert_eq!(app.detail_series().map(|s| s.key.as_str()), Some("sousou no frieren"));
    // The cursor stays on the item it was on, now listed under the target.
    assert_eq!(selected(&app), Some(item));
    app.press("\x1b");
    assert_eq!(app.lib.get("sousou no frieren").unwrap().aliases, vec!["kusuriya no hitorigoto s3"]);
}

/// `P` shows paused series even when nothing is being followed.
#[test]
fn paused_toggle_works_on_an_empty_up_next() {
    use anipv::model::SeriesStatus;
    let (_d, mut app) = app();
    let following: Vec<String> = app.upnext_rows_in(0..usize::MAX).iter().map(|s| s.key.clone()).collect();
    for key in &following {
        app.ctx.set_status(key, SeriesStatus::Paused, None).unwrap();
    }
    app.reload();
    app.press("1");
    assert!(app.upnext_rows_in(0..usize::MAX).is_empty());
    app.press("P");
    assert!(!app.upnext_rows_in(0..usize::MAX).is_empty(), "paused series show up");
    app.press("P");
    assert!(app.upnext_rows_in(0..usize::MAX).is_empty());
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
    app.press("  ");
    app.play_queue();
    assert!(app.playing.is_some());
    wait_for_player(&mut app);
    assert_eq!(app.queue.len(), 2, "queue kept");
    assert!(app.current_message().unwrap_or_default().contains("exited before playing"));
}

#[test]
fn queue_is_consumed_once_playback_starts() {
    use anipv::mpv::PlayerEvent;
    use anipv::tui::app::Msg;
    let (_d, mut app) = app();
    app.press(" ");
    let path = app.queue[0].file.clone();
    app.play_queue();
    app.on_msg(Msg::Player(PlayerEvent::Started { path }));
    assert!(app.queue.is_empty());
    wait_for_player(&mut app);
}

/// A failed write must stay visible instead of being replaced by "Show → status".
#[cfg(unix)]
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
    let none = MatchAttempt { series: "dungeon meshi".into(), db_version: "v".into(), at: 1, result: "none".into() };
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
    assert!(app.meta_busy().is_some());
    wait_for_meta(&mut app);
    let msg = app.current_message().unwrap_or_default().to_string();
    assert!(msg.starts_with("Yuru Camp:") && msg.contains("12 eps"), "{msg}");
}

/// `ctrl-r` refreshes everything; asking again while it runs queues one more.
#[test]
fn ctrl_r_refreshes_everything_and_queues_while_busy() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (_d, mut app) = app();
    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    app.on_key(ctrl_r);
    assert!(app.meta_busy().is_some());
    app.on_key(ctrl_r);
    assert!(app.current_message().unwrap_or_default().contains("queued"));
    wait_for_meta(&mut app);
    assert!(app.current_message().unwrap_or_default().starts_with("metadata:"));
}

#[test]
fn help_and_quit() {
    let (_d, mut app) = app();
    app.press("?");
    let s = render(&mut app);
    assert!(s.contains("Keys · any key closes"), "{s}");
    app.press("x");
    assert!(app.popup.is_none());
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
        .position(|it| it.present() && !app.is_queued("sousou no frieren", &it.key))
        .expect("an episode on disk that is not queued");
    drop(rows);
    app.detail_state.select(Some(idx));
    let before = app.queue.clone();
    app.press("\n");
    assert!(app.playing.is_some(), "{:?}", app.current_message());
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
    app.play_queue();
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
    let last = app.detail_rows().last().unwrap().key.describe();
    assert!(s.lines().any(|l| l.contains("│▌") && l.contains(&format!(" {last} "))), "last row selected:\n{s}");
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
    assert!(all.targets.len() > 3, "{:?}", all.targets);
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
    let item = app.detail_rows()[app.detail_state.selected().unwrap()].key.clone();
    // Behind the TUI's back: skip the first inbox row, and a new (earlier)
    // Frieren episode arrives, adding rows before the selected file and episode.
    app.ctx.set_status(&inbox[0], SeriesStatus::Skipped, None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let new = d.path().join("media/Downloads/[GroupA] Sousou no Frieren - 26 (1080p) [ABCD0026].mkv");
    std::fs::write(&new, b"").unwrap();
    app.ctx.scan(None, &|_, _| {}).unwrap();
    app.reload_index();
    assert_eq!(app.file_rows_in(0..1)[0].file.path, new, "the new file is listed first");
    assert_eq!(app.selected_inbox().map(|s| s.key.as_str()), Some(inbox[1].as_str()));
    let files_sel = app.files_state.selected().unwrap();
    assert_eq!(app.file_rows_in(files_sel..files_sel + 1)[0].file.path, file);
    assert_eq!(app.detail_rows()[app.detail_state.selected().unwrap()].key, item);
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
    assert!(app.queue.is_empty(), "{:?}", app.queue);
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
#[cfg(unix)]
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
    let shown = |app: &App| -> Vec<String> { app.detail_rows().iter().map(|i| i.key.describe()).collect() };
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
