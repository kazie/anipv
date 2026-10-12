//! End-to-end tests of the `anipv` binary against the demo library.
#![expect(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]
#![expect(clippy::print_stderr, reason = "explains why a test is skipped")]

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn demo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    anipv::demo::setup_now(dir.path()).unwrap();
    dir
}

fn anipv(home: &Path) -> Command {
    let mut c = Command::cargo_bin("anipv").unwrap();
    c.env("ANIPV_HOME", home).env("NO_COLOR", "1").env_remove("ANIPV_CONFIG");
    c
}

#[test]
fn next_lists_followed_series() {
    let d = demo();
    anipv(d.path())
        .arg("next")
        .assert()
        .success()
        .stdout(predicate::str::contains("Sousou no Frieren").and(predicate::str::contains("K-On!").not()));
    anipv(d.path())
        .args(["next", "--paused"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Aria The Animation"));
}

#[test]
fn show_mark_and_status() {
    let d = demo();
    anipv(d.path())
        .args(["show", "frieren"])
        .assert()
        .success()
        .stdout(predicate::str::contains("✓       28"))
        .stdout(predicate::str::contains("… 26 more not on disk (--all lists them)"))
        .stdout(predicate::str::contains("(not on disk)").not());
    anipv(d.path())
        .args(["show", "frieren", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(not on disk)").count(26))
        .stdout(predicate::str::contains("more not on disk").not());
    anipv(d.path()).args(["mark", "frieren", "29-30"]).assert().success();
    // Re-marking a range only touches what changes, keeping earlier watch times.
    let before = std::fs::read_to_string(d.path().join("data/events/desktop.jsonl")).unwrap().lines().count();
    anipv(d.path())
        .args(["mark", "frieren", "1-30"])
        .assert()
        .success()
        .stdout(predicate::str::contains("marked 0 episode(s) of Sousou no Frieren as watched (30 already were)"));
    let after = std::fs::read_to_string(d.path().join("data/events/desktop.jsonl")).unwrap().lines().count();
    assert_eq!(before, after);
    // Marks record the file name as evidence for possible future re-keying.
    let log = std::fs::read_to_string(d.path().join("data/events/desktop.jsonl")).unwrap();
    assert!(log.lines().last().unwrap().contains(r#""file":"[GroupA] Sousou no Frieren - 30"#), "{log}");
    anipv(d.path())
        .args(["next"])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"Sousou no Frieren\s+31").unwrap());
    anipv(d.path()).args(["mark", "frieren", "30", "--unwatched"]).assert().success();
    // An episode listed twice counts once.
    anipv(d.path())
        .args(["mark", "frieren", "30,30"])
        .assert()
        .success()
        .stdout(predicate::str::contains("marked 1 episode(s) of Sousou no Frieren as watched\n"));
    // Unwatching a started episode clears its resume point; unknown episodes
    // aren't invented, nor counted as already unwatched.
    anipv(d.path())
        .args(["mark", "ranma", "1,99", "--unwatched"])
        .assert()
        .success()
        .stdout(predicate::str::contains("marked 1 episode(s) of Ranma 1-2 2024 S3 as unwatched (1 not known)\n"));
    anipv(d.path()).args(["mark", "ranma", "1,1,99", "--unwatched"]).assert().success().stdout(
        predicate::str::contains(
            "marked 0 episode(s) of Ranma 1-2 2024 S3 as unwatched (1 already were, 1 not known)\n",
        ),
    );
    anipv(d.path()).args(["show", "ranma"]).assert().success().stdout(predicate::str::contains("%").not());
    anipv(d.path()).args(["status", "yuru camp", "following"]).assert().success();
    anipv(d.path())
        .args(["ls", "--status", "following"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Yuru Camp"));
    anipv(d.path()).args(["status", "nonexistent-zzz", "following"]).assert().failure();
}

/// A followed show completes when its last episode is marked watched.
#[test]
fn finishing_a_followed_series_completes_it() {
    let d = demo();
    anipv(d.path()).args(["status", "yuru camp", "following"]).assert().success();
    anipv(d.path())
        .args(["mark", "yuru camp", "1-11"])
        .assert()
        .success()
        .stdout(predicate::str::contains("completed").not());
    anipv(d.path())
        .args(["mark", "yuru camp", "12"])
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ Yuru Camp completed: all episodes watched"));
    anipv(d.path())
        .args(["ls", "--status", "completed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Yuru Camp"));
}

/// A status chosen for a show that is already finished and fully watched
/// stays: anipv doesn't complete it behind the user's back (a rewatch).
#[test]
fn status_chosen_on_a_finished_series_is_kept() {
    let d = demo();
    anipv(d.path())
        .args(["mark", "yuru camp", "1-12"])
        .assert()
        .success()
        .stdout(predicate::str::contains("completed").not());
    anipv(d.path())
        .args(["status", "yuru camp", "following"])
        .assert()
        .success()
        .stdout(predicate::str::contains("completed").not());
    anipv(d.path())
        .args(["ls", "--status", "following"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Yuru Camp"));
}

#[test]
fn play_print_builds_mpv_command() {
    let d = demo();
    anipv(d.path())
        .args(["play", "frieren", "dandadan", "-n", "2", "--print"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Sousou no Frieren - 29").and(predicate::str::contains("Dandadan S2 - 06v2")));
    // Started episodes resume where they stopped.
    anipv(d.path())
        .args(["play", "ranma", "--print"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--start=590"));
}

#[test]
fn merge_and_rename() {
    let d = demo();
    anipv(d.path()).args(["merge", "kusuriya", "frieren"]).assert().success();
    anipv(d.path()).args(["show", "frieren"]).assert().success().stdout(predicate::str::contains("also: kusuriya"));
    // Naming the target series explains which names can be unmerged.
    anipv(d.path())
        .args(["unmerge", "frieren"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unmerge one of: kusuriya no hitorigoto s3"));
    anipv(d.path())
        .args(["unmerge", "no such name"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a merged-away name"));
    // A title works as well as the key.
    anipv(d.path()).args(["unmerge", "Kusuriya no Hitorigoto S3"]).assert().success();
    anipv(d.path()).args(["rename", "frieren", "Frieren"]).assert().success();
    anipv(d.path()).args(["show", "frieren"]).assert().success().stdout(predicate::str::starts_with("Frieren"));
}

#[test]
fn import_fish_history() {
    let d = demo();
    let hist = d.path().join("fish_history");
    std::fs::write(
        &hist,
        "- cmd: mpv \\\\[GroupA\\\\]\\\\ Sousou\\\\ no\\\\ Frieren\\\\ -\\\\ 29\\\\ \\\\(1080p\\\\)\\\\ \\\\[ABCD0029\\\\].mkv\n  when: 1700000000\n- cmd: mpv '[GroupA] Old Show - 03 (1080p).mkv'\n  when: 1700000100\n",
    )
    .unwrap();
    anipv(d.path())
        .args(["import-fish", "--history"])
        .arg(&hist)
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("2 mpv commands → 2 watched items (1 matched"));
    anipv(d.path()).args(["import-fish", "--history"]).arg(&hist).assert().success();
    anipv(d.path()).args(["show", "frieren"]).assert().success().stdout(predicate::str::contains("✓       29"));
    // Second run imports nothing new, even after the index cache is thrown away.
    anipv(d.path())
        .args(["import-fish", "--history"])
        .arg(&hist)
        .assert()
        .success()
        .stdout(predicate::str::contains("0 mpv commands"));
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(d.path().join(format!("data/index.db{ext}")));
    }
    anipv(d.path())
        .args(["import-fish", "--history"])
        .arg(&hist)
        .assert()
        .success()
        .stdout(predicate::str::contains("0 mpv commands"));
}

#[test]
fn import_fish_resolves_by_path_and_skips_ambiguous_names() {
    let d = demo();
    let anime = d.path().join("media/Anime");
    for show in ["Show A", "Show B"] {
        std::fs::create_dir_all(anime.join(show)).unwrap();
        std::fs::write(anime.join(show).join("01.mkv"), b"x").unwrap();
    }
    anipv(d.path()).arg("scan").assert().success();
    let hist = d.path().join("fish_history");
    std::fs::write(
        &hist,
        format!(
            "- cmd: mpv {}/Show\\ B/01.mkv\n  when: 1700000000\n- cmd: mpv 01.mkv\n  when: 1700000100\n",
            anime.display()
        ),
    )
    .unwrap();
    anipv(d.path())
        .args(["import-fish", "--dry-run", "--history"])
        .arg(&hist)
        .assert()
        .success()
        .stdout(predicate::str::contains("1 mpv commands → 1 watched items (1 matched"))
        .stdout(predicate::str::contains("1 ambiguous file name(s) skipped").and(predicate::str::contains("01.mkv")));
}

/// A file played through a symlinked mount resolves by its show folder, and a
/// folder naming another show doesn't borrow an indexed file's base name.
#[test]
fn import_fish_follows_symlinked_mounts() {
    let d = demo();
    let anime = d.path().join("media/Anime");
    for show in ["Show A", "Show B"] {
        std::fs::create_dir_all(anime.join(show)).unwrap();
        std::fs::write(anime.join(show).join("01.mkv"), b"x").unwrap();
    }
    std::fs::create_dir_all(anime.join("Show C")).unwrap();
    std::fs::write(anime.join("Show C").join("07.mkv"), b"x").unwrap();
    anipv(d.path()).arg("scan").assert().success();
    let link = d.path().join("nas");
    std::os::unix::fs::symlink(&anime, &link).unwrap();
    let hist = d.path().join("fish_history");
    std::fs::write(
        &hist,
        format!(
            "- cmd: mpv {}/Show\\ B/01.mkv\n  when: 1700000000\n- cmd: mpv Other\\ Show/07.mkv\n  when: 1700000100\n",
            link.display()
        ),
    )
    .unwrap();
    anipv(d.path())
        .args(["import-fish", "--dry-run", "--history"])
        .arg(&hist)
        .assert()
        .success()
        .stdout(predicate::str::contains("2 mpv commands → 2 watched items (1 matched"))
        .stdout(predicate::str::contains("ambiguous").not());
}

#[test]
fn new_lists_the_inbox() {
    let d = demo();
    anipv(d.path()).arg("new").assert().success().stdout(
        predicate::str::contains("Dungeon Meshi")
            .and(predicate::str::is_match(r"Sousou no Frieren S2 .*sequel to Sousou no Frieren · following").unwrap())
            .and(predicate::str::contains("part of One Piece"))
            .and(predicate::str::contains("Yuru Camp").not()),
    );
    anipv(d.path()).args(["status", "dungeon meshi", "skipped"]).assert().success();
    anipv(d.path()).arg("new").assert().success().stdout(predicate::str::contains("Dungeon Meshi").not());
}

#[test]
fn export_and_doctor() {
    let d = demo();
    let out = anipv(d.path()).arg("export").assert().success().get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v.as_array().unwrap().iter().any(|s| s["key"] == "one piece"));
    anipv(d.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("Downloads").and(predicate::str::contains("comes from the hostname").not()));
}

#[test]
fn doctor_warns_about_hostname_device_and_config_rejects_bad_device() {
    let d = demo();
    let file = d.path().join("config/config.toml");
    let cfg = std::fs::read_to_string(&file).unwrap();
    assert!(cfg.contains("device = \"desktop\""), "{cfg}");
    std::fs::write(&file, cfg.replace("device = \"desktop\"\n", "")).unwrap();
    anipv(d.path()).arg("doctor").assert().success().stdout(predicate::str::contains("comes from the hostname"));
    std::fs::write(&file, cfg.replace("device = \"desktop\"", "device = \"my laptop\"")).unwrap();
    anipv(d.path()).arg("doctor").assert().failure().stderr(predicate::str::contains("`device`"));
}

#[test]
fn help_completions_and_man() {
    let d = demo();
    anipv(d.path()).arg("--help").assert().success().stdout(predicate::str::contains("import-fish"));
    anipv(d.path())
        .args(["completions", "fish"])
        .assert()
        .success()
        .stdout(predicate::str::contains("complete -c anipv"));
    anipv(d.path()).arg("man").assert().success().stdout(predicate::str::contains(".TH anipv"));
}

/// Characters the generated completions don't escape inside a quoted
/// value list (fish `-a "…"`, zsh and bash value specs): a `"` there ends the
/// quote early and breaks the whole completion file.
const UNSAFE_IN_COMPLETIONS: &[char] = &['"', '`', '$', '\\'];

/// Every possible value (of a `ValueEnum` or otherwise) of every argument in
/// `cmd` and its subcommands: `(where, name, help)`.
fn possible_values(cmd: &clap::Command, at: &str, out: &mut Vec<(String, String, String)>) {
    for arg in cmd.get_arguments() {
        for v in arg.get_possible_values() {
            let help = v.get_help().map(ToString::to_string).unwrap_or_default();
            out.push((format!("{at} --{}", arg.get_id()), v.get_name().to_string(), help));
        }
    }
    for sub in cmd.get_subcommands() {
        possible_values(sub, &format!("{at} {}", sub.get_name()), out);
    }
}

/// Completions stay valid shell code: no value or value help contains a
/// character the generators leave unescaped, every fish `-a "…"` argument
/// closes where it should, and the scripts parse in the shells installed.
#[test]
fn completions_are_valid_shell_code() {
    let mut values = Vec::new();
    possible_values(&anipv::cli::command(), "anipv", &mut values);
    assert!(values.iter().any(|(_, name, help)| name == "following" && !help.is_empty()), "{values:?}");
    for (at, name, help) in &values {
        assert!(!name.contains(UNSAFE_IN_COMPLETIONS) && !help.contains(UNSAFE_IN_COMPLETIONS), "{at}: {name}: {help}");
    }

    let d = tempfile::tempdir().unwrap();
    for shell in ["fish", "bash", "zsh"] {
        let out = anipv(d.path()).args(["completions", shell]).output().unwrap();
        assert!(out.status.success(), "{shell}");
        let script = String::from_utf8(out.stdout).unwrap();
        if shell == "fish" {
            // Each `-a "…"` closes right before whitespace or the end of the line.
            for (i, _) in script.match_indices(r#" -a ""#) {
                let rest = &script[i + 5..];
                let end = rest.find('"').unwrap_or_else(|| panic!("unclosed -a at {i}"));
                let after = rest[end + 1..].chars().next();
                assert!(after.is_none_or(char::is_whitespace), "-a argument closes early: {}", &rest[..=end]);
            }
        }
        let file = d.path().join(format!("anipv.{shell}"));
        std::fs::write(&file, &script).unwrap();
        match std::process::Command::new(shell).arg("-n").arg(&file).output() {
            Ok(check) => assert!(
                check.status.success(),
                "{shell} -n rejects the completions: {}",
                String::from_utf8_lossy(&check.stderr)
            ),
            Err(_) => eprintln!("{shell} not installed: skipping its syntax check"),
        }
    }
}

#[test]
fn init_refuses_to_overwrite() {
    let d = demo();
    anipv(d.path())
        .args(["init", "--ongoing", "/tmp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
    let fresh = tempfile::tempdir().unwrap();
    anipv(fresh.path()).args(["init"]).assert().failure();
    anipv(fresh.path()).args(["init", "--ongoing", "/tmp/dl", "--archive", "/tmp/anime"]).assert().success();
    let cfg = std::fs::read_to_string(fresh.path().join("config/config.toml")).unwrap();
    assert!(cfg.contains("kind = \"archive\""));
}

/// Demo library whose `mpv` is `anipv fake-mpv`, a fake speaking the JSON
/// IPC protocol (see `src/fake_mpv.rs`).
fn fake_mpv_demo() -> tempfile::TempDir {
    let d = demo();
    let cfg_path = d.path().join("config/config.toml");
    let mut cfg = anipv::config::Config::load(&cfg_path).unwrap();
    cfg.mpv = env!("CARGO_BIN_EXE_anipv").into();
    cfg.mpv_args = vec!["fake-mpv".into()];
    cfg.save(&cfg_path).unwrap();
    d
}

/// `anipv demo` plays with this binary as a real-time fake mpv, so the
/// demo (and the README recording) can show playback.
#[test]
fn demo_command_plays_with_the_fake_player() {
    // On the binary's file system, so it is linked, not copied.
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let dir = home.path().join("demo");
    anipv(home.path()).arg("demo").arg(&dir).assert().success();
    let cfg = anipv::config::Config::load(&dir.join("config/config.toml")).unwrap();
    assert_eq!(cfg.mpv_args, ["fake-mpv", "--demo"]);
    // Its own copy of this binary, which other builds don't replace.
    assert_eq!(Path::new(&cfg.mpv), dir.canonicalize().unwrap().join("bin/anipv"));
    let len = |p: &Path| std::fs::metadata(p).unwrap().len();
    assert_eq!(len(cfg.mpv.as_ref()), len(env!("CARGO_BIN_EXE_anipv").as_ref()));
    Command::new(&cfg.mpv).arg("fake-mpv").assert().failure().stderr(predicate::str::contains("--input-ipc-server"));
}

/// `anipv play …` with its own runtime dir and a timeout.
fn play(home: &Path, args: &[&str]) -> Command {
    let mut c = anipv(home);
    c.env("XDG_RUNTIME_DIR", home).arg("play").args(args).timeout(std::time::Duration::from_secs(30));
    c
}

/// Full playback loop against a fake mpv.
#[test]
fn play_tracks_progress_via_ipc() {
    let d = fake_mpv_demo();
    play(d.path(), &["frieren", "-n", "2"]).assert().success().stdout(
        predicate::str::contains("watched Sousou no Frieren 29").and(predicate::str::contains("stopped at 12:00")),
    );
    anipv(d.path()).args(["show", "frieren"]).assert().success().stdout(predicate::str::contains("◐       30 50%"));
}

/// Without `XDG_RUNTIME_DIR` the socket goes to a fallback dir that must be created.
#[test]
fn play_tracks_without_runtime_dir() {
    let d = fake_mpv_demo();
    play(d.path(), &["frieren"])
        .env_remove("XDG_RUNTIME_DIR")
        .assert()
        .success()
        .stdout(predicate::str::contains("watched Sousou no Frieren 29"));
}

/// A failed write reports the error and keeps following mpv.
#[test]
fn play_keeps_going_when_a_write_fails() {
    use std::os::unix::fs::PermissionsExt;
    let d = fake_mpv_demo();
    let log = d.path().join("data/events/desktop.jsonl");
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new().append(true).open(&log).is_ok() {
        eprintln!("running as root; can't make the log unwritable; skipping");
        return;
    }
    let out = play(d.path(), &["frieren", "-n", "2"]).assert().success().get_output().stdout.clone();
    let out = String::from_utf8_lossy(&out);
    assert_eq!(out.matches("could not record").count(), 2, "both files were reported:\n{out}");
}

#[test]
fn scan_rejects_an_unknown_root() {
    let d = demo();
    anipv(d.path())
        .args(["scan", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no root named \"nope\""));
    anipv(d.path()).args(["scan", "downloads"]).assert().success();
}

/// A root that cannot be scanned (say, an unmounted drive) fails the command,
/// after the per-root lines, whether all roots or just that one were asked for.
#[test]
fn scan_fails_when_a_root_fails() {
    let d = demo();
    std::fs::rename(d.path().join("media/Downloads"), d.path().join("media/gone")).unwrap();
    anipv(d.path())
        .arg("scan")
        .assert()
        .failure()
        .stdout(predicate::str::contains("✓ Anime").and(predicate::str::contains("✗ Downloads")))
        .stderr(predicate::str::contains("1 of 2 root(s) could not be scanned"));
    anipv(d.path())
        .args(["scan", "downloads"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1 of 1 root(s) could not be scanned"));
    anipv(d.path()).args(["scan", "anime"]).assert().success();
}

/// `anilist = false` (as in the demo) means no network from the CLI either.
#[test]
fn meta_refresh_respects_anilist_off() {
    let d = demo();
    anipv(d.path())
        .args(["meta", "refresh", "--all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("anilist = false"));
}

/// While part of the event log can't be read, commands that write refuse
/// (what they would decide from the rest may be wrong), and `doctor` lists
/// the file once, among its own findings.
#[test]
fn writes_wait_for_an_unreadable_log_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let d = demo();
    let other = d.path().join("data/events/laptop.jsonl");
    std::fs::write(&other, "").unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&other).is_ok() {
        eprintln!("skipped: running as root, nothing is unreadable");
        return;
    }
    let events = || std::fs::read_to_string(d.path().join("data/events/desktop.jsonl")).unwrap();
    let before = events();
    anipv(d.path())
        .args(["mark", "frieren", "29"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("laptop.jsonl").and(predicate::str::contains("nothing is changed")));
    assert_eq!(events(), before);
    anipv(d.path()).arg("ls").assert().success().stderr(predicate::str::contains("laptop.jsonl"));
    anipv(d.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("laptop.jsonl").count(1))
        .stderr(predicate::str::contains("laptop.jsonl").not());
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o644)).unwrap();
    anipv(d.path()).args(["mark", "frieren", "29"]).assert().success();
}

#[test]
fn play_needs_at_least_one_episode() {
    let d = demo();
    anipv(d.path()).args(["play", "frieren", "-n", "0", "--print"]).assert().failure();
}

/// A reader that stops early (`anipv export | head`) is not an error.
#[test]
fn export_into_a_closed_pipe_succeeds() {
    let d = demo();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("anipv"))
        .arg("export")
        .env("ANIPV_HOME", d.path())
        .env("NO_COLOR", "1")
        .env_remove("ANIPV_CONFIG")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
