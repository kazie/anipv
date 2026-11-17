//! Dated copies of the event log, made by `anipv update`.
//!
//! The log is the only thing anipv can't rebuild, and it may live on one
//! machine only. Each backup is a folder named by its UTC date
//! (`backups/2026-11-17/`) holding a copy of every `*.jsonl` file of the
//! events folder. One is made when any file grew since the newest backup (at
//! most one a day: a later change that day updates that day's copy), and the
//! newest [`KEPT`] are kept.
//!
//! A log only grows, so a backup takes a file only when it starts with its
//! copy in the newest backup. One that doesn't (cut short, replaced) or is
//! gone keeps that copy, carried into every later backup and reported each
//! time, so a broken log never displaces the good copy, however long it
//! takes to notice. Every backup is whole: pruning old ones never drops the
//! only copy of a log that stopped changing.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::log_files;

/// What a backup holds, by log name: the contents, and whether the newest
/// backup holds them already.
type Files<'a> = BTreeMap<&'a OsString, (&'a [u8], bool)>;

/// How many dated backups are kept.
pub const KEPT: usize = 30;

/// What [`backup`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The backup made or updated, if anything changed.
    pub made: Option<PathBuf>,
    /// Logs whose copy in the newest backup was kept instead of them, and why.
    pub kept: Vec<(String, Kept)>,
    /// Logs that could not be read (their copy, if any, was kept).
    pub unreadable: Vec<String>,
}

/// Why [`backup`] kept a log's copy from the newest backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// The log is shorter than its copy: cut short.
    Shorter,
    /// The log doesn't start with its copy: replaced by another file.
    Replaced,
    /// The log is gone from the events folder.
    Gone,
}

impl Kept {
    /// What happened to the log, for a warning.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Shorter => "is shorter than in the newest backup: it was cut short",
            Self::Replaced => "no longer starts the way its newest backup does: it was replaced",
            Self::Gone => "is gone from the events folder",
        }
    }
}

/// Copy the logs in `events_dir` into a backup in `backups_dir` dated `ts`
/// (unix seconds) when any grew since the newest backup, keeping the copy of
/// those that didn't only grow (see the module docs), then keep only the
/// newest `keep` backups.
pub fn backup(events_dir: &Path, backups_dir: &Path, ts: i64, keep: usize) -> Result<Report> {
    let mut report = Report::default();
    let newest = newest(backups_dir);
    let copies: BTreeMap<OsString, Vec<u8>> = match &newest {
        Some(dir) => read_logs(dir, &mut report.unreadable)?.into_iter().filter_map(|(n, b)| Some((n, b?))).collect(),
        None => BTreeMap::new(),
    };
    let logs = read_logs(events_dir, &mut report.unreadable)?;
    // What the backup holds (each log, or the copy kept instead of it), and
    // whether that is what the newest backup holds already.
    let mut files: Files = BTreeMap::new();
    for (name, bytes) in &logs {
        let copy = copies.get(name);
        let chosen = match (bytes, copy) {
            (Some(b), Some(c)) if !b.starts_with(c) => {
                let why = if b.len() < c.len() { Kept::Shorter } else { Kept::Replaced };
                report.kept.push((name.to_string_lossy().into_owned(), why));
                c
            }
            (Some(b), _) => b,
            // Unreadable (reported as such): its copy is kept.
            (None, Some(c)) => c,
            (None, None) => continue,
        };
        // `chosen` is the copy or starts with it: the same exactly when as long.
        files.insert(name, (chosen, copy.is_some_and(|c| c.len() == chosen.len())));
    }
    for (name, copy) in &copies {
        if !logs.contains_key(name) {
            report.kept.push((name.to_string_lossy().into_owned(), Kept::Gone));
            files.insert(name, (copy, true));
        }
    }
    if files.values().any(|(_, same)| !same) {
        report.made = Some(write(backups_dir, &utc_date(ts), &files, newest.as_deref())?);
    }
    let dated = dated(backups_dir);
    for old in &dated[..dated.len().saturating_sub(keep)] {
        std::fs::remove_dir_all(old).with_context(|| format!("removing {old}", old = old.display()))?;
    }
    Ok(report)
}

/// The `*.jsonl` files in `dir` by name: their contents, or `None` for one
/// that can't be read (told in `errors`).
fn read_logs(dir: &Path, errors: &mut Vec<String>) -> Result<BTreeMap<OsString, Option<Vec<u8>>>> {
    let mut logs = BTreeMap::new();
    for path in log_files(dir)? {
        let bytes = std::fs::read(&path).map_err(|e| errors.push(format!("{path}: {e}", path = path.display()))).ok();
        logs.insert(path.file_name().unwrap_or_default().to_owned(), bytes);
    }
    Ok(logs)
}

/// Write `files` as the backup named `date`: a new one is filled under a
/// temporary name and renamed into place, so a backup that exists is whole;
/// today's, already there, gets only what changed since it is the newest
/// (each file replaced whole).
fn write(backups_dir: &Path, date: &str, files: &Files, newest: Option<&Path>) -> Result<PathBuf> {
    let dir = backups_dir.join(date);
    let fill = |into: &Path, all: bool| -> Result<()> {
        for (name, (bytes, same)) in files {
            if all || !same {
                let to = into.join(name);
                crate::config::replace_file(&to, bytes).with_context(|| format!("writing {to}", to = to.display()))?;
            }
        }
        Ok(())
    };
    if dir.is_dir() {
        fill(&dir, newest != Some(dir.as_path()))?;
        return Ok(dir);
    }
    let part = backups_dir.join(format!(".{date}.part"));
    if part.exists() {
        std::fs::remove_dir_all(&part).with_context(|| format!("removing {part}", part = part.display()))?;
    }
    std::fs::create_dir_all(&part).with_context(|| format!("creating {part}", part = part.display()))?;
    fill(&part, true)?;
    std::fs::rename(&part, &dir).with_context(|| format!("renaming {part} into place", part = part.display()))?;
    Ok(dir)
}

/// The newest backup in `backups_dir`, if any.
pub fn newest(backups_dir: &Path) -> Option<PathBuf> {
    dated(backups_dir).pop()
}

/// The backup folders in `backups_dir` (named `YYYY-MM-DD`), oldest first.
/// Anything else there is left alone.
fn dated(backups_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(backups_dir) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(is_date))
        .collect();
    dirs.sort();
    dirs
}

/// True for a `YYYY-MM-DD` name.
fn is_date(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 10 && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

/// The UTC date of unix time `ts`, as `YYYY-MM-DD`.
fn utc_date(ts: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    /// 2026-11-17 12:00 UTC.
    const T: i64 = 1_794_916_800;

    #[test]
    fn utc_dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_735_689_599), "2024-12-31");
        assert_eq!(utc_date(1_735_689_600), "2025-01-01");
        assert_eq!(utc_date(T), "2026-11-17");
        assert_eq!(utc_date(-1), "1969-12-31");
        assert!(is_date("2026-11-17") && !is_date("2026-11-1x") && !is_date("notes") && !is_date("2026_11_17"));
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let (events, backups) = (dir.path().join("events"), dir.path().join("backups"));
        std::fs::create_dir_all(&events).unwrap();
        std::fs::write(events.join("desk.jsonl"), "a\n").unwrap();
        std::fs::write(events.join("laptop.jsonl"), "x\n").unwrap();
        std::fs::write(events.join("notes.txt"), "not a log").unwrap();
        (dir, events, backups)
    }

    fn read(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap()
    }

    /// A backup is made when something changed, holds every log, and is
    /// one a day.
    #[test]
    fn backups_are_made_when_the_logs_change() {
        let (_tmp, events, backups) = setup();
        let r = backup(&events, &backups, T, KEPT).unwrap();
        let today = backups.join("2026-11-17");
        assert_eq!(r.made.as_deref(), Some(today.as_path()));
        assert_eq!((read(&today, "desk.jsonl"), read(&today, "laptop.jsonl")), ("a\n".into(), "x\n".into()));
        assert!(!today.join("notes.txt").exists(), "only logs");

        // Nothing changed: nothing made, the next day either.
        assert_eq!(backup(&events, &backups, T + DAY, KEPT).unwrap(), Report::default());

        // A change the same day updates that day's backup.
        std::fs::write(events.join("desk.jsonl"), "a\nb\n").unwrap();
        assert_eq!(backup(&events, &backups, T + 60, KEPT).unwrap().made.as_deref(), Some(today.as_path()));
        assert_eq!(read(&today, "desk.jsonl"), "a\nb\n");

        // The next day's change is a new backup, with every log in it.
        std::fs::write(events.join("desk.jsonl"), "a\nb\nc\n").unwrap();
        let next = backups.join("2026-11-18");
        assert_eq!(backup(&events, &backups, T + DAY, KEPT).unwrap().made.as_deref(), Some(next.as_path()));
        assert_eq!((read(&next, "desk.jsonl"), read(&next, "laptop.jsonl")), ("a\nb\nc\n".into(), "x\n".into()));
        assert_eq!(read(&today, "desk.jsonl"), "a\nb\n", "the day before stays");
        assert_eq!(newest(&backups).as_deref(), Some(next.as_path()));
    }

    /// Only the newest backups are kept; anything else in the folder is left alone.
    #[test]
    fn old_backups_are_pruned() {
        let (_tmp, events, backups) = setup();
        std::fs::create_dir_all(backups.join("keep me")).unwrap();
        for day in 0..4 {
            std::fs::write(events.join("desk.jsonl"), "a\n".repeat(usize::try_from(day + 1).unwrap())).unwrap();
            backup(&events, &backups, T + day * DAY, 2).unwrap();
        }
        assert_eq!(dated(&backups), [backups.join("2026-11-19"), backups.join("2026-11-20")]);
        assert!(backups.join("keep me").is_dir());
        let names: Vec<_> = std::fs::read_dir(&backups).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(!names.iter().any(|n| n.to_string_lossy().ends_with(".part")), "{names:?}");
    }

    /// A log cut short keeps its longer copy, today and on every later day,
    /// so the newest backup is always the one to restore from.
    #[test]
    fn a_cut_log_never_displaces_its_copy() {
        let (_tmp, events, backups) = setup();
        std::fs::write(events.join("desk.jsonl"), "a\nb\nc\n").unwrap();
        backup(&events, &backups, T, KEPT).unwrap();
        std::fs::write(events.join("desk.jsonl"), "").unwrap();
        let mut laptop = String::from("x\n");
        for (ts, day) in [(T + 60, "2026-11-17"), (T + DAY, "2026-11-18"), (T + 2 * DAY, "2026-11-19")] {
            laptop.push_str("y\n");
            std::fs::write(events.join("laptop.jsonl"), &laptop).unwrap();
            let r = backup(&events, &backups, ts, KEPT).unwrap();
            assert_eq!(r.kept, [("desk.jsonl".to_string(), Kept::Shorter)], "{day}");
            let dir = backups.join(day);
            assert_eq!(read(&dir, "desk.jsonl"), "a\nb\nc\n", "{day}: the longer copy stays");
            assert_eq!(read(&dir, "laptop.jsonl"), laptop, "{day}: the other log is backed up");
        }
        assert_eq!(newest(&backups), Some(backups.join("2026-11-19")));
        // Nothing grew: no new backup, but still a warning.
        let r = backup(&events, &backups, T + 3 * DAY, KEPT).unwrap();
        assert_eq!((r.made, r.kept.len()), (None, 1));
    }

    /// A log that doesn't start the way its copy does was replaced, however
    /// long it is; one that is gone keeps its copy too.
    #[test]
    fn replaced_and_gone_logs_keep_their_copies() {
        let (_tmp, events, backups) = setup();
        backup(&events, &backups, T, KEPT).unwrap();
        std::fs::write(events.join("desk.jsonl"), "b\nmore\n").unwrap();
        std::fs::remove_file(events.join("laptop.jsonl")).unwrap();
        std::fs::write(events.join("new.jsonl"), "n\n").unwrap();
        let r = backup(&events, &backups, T + DAY, KEPT).unwrap();
        assert_eq!(r.kept, [("desk.jsonl".to_string(), Kept::Replaced), ("laptop.jsonl".to_string(), Kept::Gone)]);
        let dir = backups.join("2026-11-18");
        assert_eq!((read(&dir, "desk.jsonl"), read(&dir, "laptop.jsonl")), ("a\n".into(), "x\n".into()));
        assert_eq!(read(&dir, "new.jsonl"), "n\n");
    }

    /// A log that can't be read is reported and keeps its copy; the others
    /// are still backed up.
    #[test]
    fn an_unreadable_log_doesnt_stop_the_others() {
        use std::os::unix::fs::PermissionsExt;
        let (_tmp, events, backups) = setup();
        backup(&events, &backups, T, KEPT).unwrap();
        let laptop = events.join("laptop.jsonl");
        std::fs::set_permissions(&laptop, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&laptop).is_ok() {
            return; // root reads anything
        }
        std::fs::write(events.join("desk.jsonl"), "a\nb\n").unwrap();
        let r = backup(&events, &backups, T + DAY, KEPT).unwrap();
        assert!(r.kept.is_empty(), "{kept:?}", kept = r.kept);
        assert_eq!(r.unreadable.len(), 1);
        assert!(r.unreadable[0].contains("laptop.jsonl"), "{u:?}", u = r.unreadable);
        let dir = backups.join("2026-11-18");
        assert_eq!((read(&dir, "desk.jsonl"), read(&dir, "laptop.jsonl")), ("a\nb\n".into(), "x\n".into()));
    }

    #[test]
    fn no_events_folder_is_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let r = backup(&dir.path().join("missing"), &dir.path().join("b"), T, KEPT).unwrap();
        assert_eq!(r, Report::default());
    }
}
