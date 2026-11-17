//! `just rekey-check`: run two versions of anipv over copies of your library
//! and compare how they key its files and what history each shows, to review
//! a change to keying before it ships (see `docs/follow-ups.md`).
//!
//! ```text
//! cargo run --example rekey_check -- OLD-REV [NEW-REV] [--all]
//! ```
//!
//! NEW-REV defaults to the working tree. Each revision is built from
//! `git archive` in `target/rekey/src` (one folder, so the dependencies are
//! built once) and its binary kept as `target/rekey/<sha>/anipv`.
//!
//! Your config, index (a consistent snapshot, though anipv may be running)
//! and event log are found as anipv finds them and copied twice, one copy per
//! version, laid out for `ANIPV_HOME`; each copy's config
//! has no `events_dir`, so only the copies are ever opened (building the
//! library can write to them: aliases for legacy keys, moved metadata). When
//! a version finds its copy of the index from another schema (its export has
//! no files) or the two exports list different files, both copies are
//! rescanned, which reads the media folders and writes nothing there. The
//! exports and the report are kept in `target/rekey/last/`.
//!
//! The report, old → new:
//!
//! * files that changed series or item, grouped by series (expected when
//!   keying changes: review them);
//! * files whose watch state changed: history that didn't follow its file;
//! * files that moved to a series with another status: a status that didn't
//!   follow;
//! * watched or started items only the history knows (no file on disk) whose
//!   state changed, or that the other export has neither in their place nor
//!   where their series' files went (gone, or new);
//! * series that appeared or disappeared, statuses that changed, and statuses
//!   that didn't follow a series that is gone to where its files went.
//!
//! Exits with 1 when any watch state or status changed or any history is
//! gone or new, or when no file is in both exports: what a keying change must
//! never do, unless it is the fix. Moves alone exit 0; errors (a revision
//! that doesn't build, an export that fails) exit 2.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anipv::config::{Config, Paths};
use anipv::model::ItemKey;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

/// Entries listed per list without `--all`.
const SHOWN: usize = 5;

/// Where an item is: `(series key, item)`, the item as compact JSON.
type Place = (String, String);

/// The items a file holds (usually one), with their state.
type Items = BTreeMap<Place, Value>;

/// The series each series' files went to (or came from).
type Moves<'a> = BTreeMap<&'a str, BTreeSet<&'a str>>;

const USAGE: &str = "usage: rekey_check OLD-REV [NEW-REV] [--all]";

fn main() -> ExitCode {
    match run() {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => ExitCode::FAILURE,
        // Not 1, which says history changed.
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Run the check; true if history changed (see the module docs).
fn run() -> Result<bool> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let all = args.iter().any(|a| a == "--all");
    let revs: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--all").collect();
    ensure!(!revs.iter().any(|r| r.starts_with('-')), USAGE);
    let (old, new) = match revs[..] {
        [old] => (old, None),
        [old, new] => (old, Some(new)),
        _ => bail!(USAGE),
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let rekey = root.join("target/rekey");
    let bins = [build(root, &rekey, Some(old))?, build(root, &rekey, new)?];

    let work = tempfile::tempdir().context("making a work folder")?;
    prepare(work.path())?;
    let sides = [Side::new("old", &bins[0], work.path()), Side::new("new", &bins[1], work.path())];
    let last = rekey.join("last");
    fresh(&last)?;
    let mut exports = export(&sides, &last)?;
    let [a, b] = &exports;
    if a.files.is_empty() || b.files.is_empty() || a.files.keys().ne(b.files.keys()) {
        let why = "the exports list different files (an index from another schema?): scanning both copies …";
        writeln!(std::io::stderr(), "{why}")?;
        let scans = [
            sides[0].spawn("scan", Stdio::from(std::io::stderr()))?,
            sides[1].spawn("scan", Stdio::from(std::io::stderr()))?,
        ];
        for (side, mut scan) in sides.iter().zip(scans) {
            ensure!(scan.wait()?.success(), "{side}: anipv scan failed", side = side.name);
        }
        exports = export(&sides, &last)?;
    }

    let mut text = Vec::new();
    let bad = report(&exports[0], &exports[1], all, &mut text)?;
    std::io::stdout().write_all(&text)?;
    std::fs::write(last.join("report.txt"), &text)?;
    Ok(bad)
}

/// Both sides' `anipv export`, run at once, each kept in `last` as `<name>.json`.
fn export(sides: &[Side; 2], last: &Path) -> Result<[Export; 2]> {
    let running = [sides[0].spawn("export", Stdio::piped())?, sides[1].spawn("export", Stdio::piped())?];
    let mut exports = Vec::new();
    for (side, child) in sides.iter().zip(running) {
        let name = side.name;
        let out = child.wait_with_output().with_context(|| side.running("export"))?;
        ensure!(out.status.success(), "{name}: anipv export failed (its error is above)");
        std::fs::write(last.join(format!("{name}.json")), &out.stdout)?;
        exports.push(Export::parse(&out.stdout).with_context(|| format!("{name}'s export"))?);
    }
    exports.try_into().map_err(|_| anyhow::anyhow!("two sides, two exports"))
}

/// The anipv binary of revision `rev`, or with none, of the working tree.
fn build(root: &Path, rekey: &Path, rev: Option<&str>) -> Result<PathBuf> {
    // The working tree builds with the toolchain running this.
    let Some(rev) = rev else {
        let cargo = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        return cargo_build(cargo, &root.join("Cargo.toml"));
    };
    let sha = git(root, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])
        .with_context(|| format!("no such revision: {rev}"))?;
    let kept = rekey.join(&sha).join("anipv");
    if kept.is_file() {
        return Ok(kept);
    }
    writeln!(std::io::stderr(), "building {rev} ({sha}) …")?;
    let src = rekey.join("src");
    fresh(&src)?;
    let mut archive = Command::new("git")
        .args(["archive", &sha])
        .current_dir(root)
        .stdout(Stdio::piped())
        .spawn()
        .context("running git archive")?;
    let piped = archive.stdout.take().context("git archive")?;
    let tar = Command::new("tar").arg("-x").arg("-C").arg(&src).stdin(piped).status().context("running tar")?;
    ensure!(archive.wait()?.success() && tar.success(), "extracting {rev} failed");
    // A revision with the one its `rust-toolchain.toml` names, through
    // rustup's `cargo` (`cargo run` set aside the toolchain running this).
    let mut cargo = Command::new("cargo");
    cargo.env_remove("RUSTUP_TOOLCHAIN").env("CARGO_TARGET_DIR", rekey.join("build"));
    let bin = cargo_build(cargo, &src.join("Cargo.toml"))?;
    // Kept only once whole: a run cut short builds it again.
    let part = kept.with_extension("part");
    std::fs::create_dir_all(rekey.join(&sha))?;
    copy(&bin, &part)?;
    std::fs::rename(&part, &kept)?;
    Ok(kept)
}

/// Build the anipv binary of `manifest` with `cargo` and return its path,
/// wherever cargo's config puts it.
fn cargo_build(mut cargo: Command, manifest: &Path) -> Result<PathBuf> {
    cargo.args(["build", "--quiet", "--locked", "--message-format=json-render-diagnostics", "--manifest-path"]);
    cargo.arg(manifest).stderr(Stdio::inherit());
    let out = cargo.output().context("running cargo")?;
    ensure!(out.status.success(), "building {manifest} failed", manifest = manifest.display());
    executable(&out.stdout).context("cargo built no anipv binary")
}

/// The anipv binary in cargo's JSON messages.
fn executable(messages: &[u8]) -> Option<PathBuf> {
    messages
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "anipv")
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
}

/// `git args` in `root`, its output trimmed.
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(root).output().context("running git")?;
    ensure!(out.status.success(), "git {args} failed", args = args.join(" "));
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// One version and its copy of your data.
struct Side {
    name: &'static str,
    bin: PathBuf,
    home: PathBuf,
}

impl Side {
    fn new(name: &'static str, bin: &Path, work: &Path) -> Self {
        Self { name, bin: bin.to_path_buf(), home: work.join(name) }
    }

    /// `anipv cmd` on this side's copy only.
    fn command(&self, cmd: &str) -> Command {
        let mut c = Command::new(&self.bin);
        c.arg(cmd).env_remove("ANIPV_CONFIG").env_remove("XDG_CONFIG_HOME").env_remove("XDG_DATA_HOME");
        c.env("ANIPV_HOME", &self.home).env("NO_COLOR", "1").stderr(Stdio::inherit());
        c
    }

    /// `anipv cmd`, started, its output going to `stdout`.
    fn spawn(&self, cmd: &str, stdout: Stdio) -> Result<std::process::Child> {
        self.command(cmd).stdout(stdout).spawn().with_context(|| self.running(cmd))
    }

    /// What running `anipv cmd` on this side is, for an error.
    fn running(&self, cmd: &str) -> String {
        format!("{name}: running {bin} {cmd}", name = self.name, bin = self.bin.display())
    }
}

/// Copy your config, index and event log into `work/old` and `work/new`.
fn prepare(work: &Path) -> Result<()> {
    // Found as `Ctx::load` finds them: the config can move the event log.
    let cfg = Config::load(&Paths::resolve(None)?.config_file)?;
    let paths = Paths::resolve(Some(&cfg))?;
    let config = &paths.config_file;
    ensure!(config.is_file(), "no config at {config}", config = config.display());
    let db = &paths.db_file;
    ensure!(db.is_file(), "no index at {db}: run anipv scan first", db = db.display());
    let mut table: toml::Table = read(config)?.parse().context("parsing the config")?;
    table.remove("events_dir");
    let text = toml::to_string(&table)?;
    // Every file of the events folder: each version reads what it takes for a log.
    let events = &paths.events_dir;
    let mut logs = Vec::new();
    match std::fs::read_dir(events) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                if path.is_file() {
                    logs.push(path);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {events}", events = events.display())),
    }
    if logs.is_empty() {
        writeln!(
            std::io::stderr(),
            "warning: {events} holds no history: comparing keys only",
            events = events.display()
        )?;
    }
    // One consistent snapshot of the index, though anipv may be writing to it
    // (a WAL checkpoint between copying its files would tear them).
    let snapshot = work.join("index.db");
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    rusqlite::Connection::open_with_flags(db, flags)
        .and_then(|c| c.execute("VACUUM INTO ?1", [snapshot.to_string_lossy()]))
        .with_context(|| format!("copying {db}", db = db.display()))?;
    for side in ["old", "new"] {
        let to = Paths::under(&work.join(side));
        to.ensure()?;
        let config = &to.config_file;
        std::fs::create_dir_all(config.parent().unwrap_or(work))?;
        std::fs::write(config, &text).with_context(|| format!("writing {config}", config = config.display()))?;
        copy(&snapshot, &to.db_file)?;
        for log in &logs {
            copy(log, &to.events_dir.join(log.file_name().unwrap_or_default()))?;
        }
    }
    Ok(())
}

/// Read a text file, saying which on failure.
fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {path}", path = path.display()))
}

/// Make `dir` an empty folder.
fn fresh(dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("removing {dir}", dir = dir.display()))?;
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}", dir = dir.display()))
}

/// Copy a file, saying which on failure.
fn copy(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to)
        .with_context(|| format!("copying {from} to {to}", from = from.display(), to = to.display()))?;
    Ok(())
}

/// One export, flattened.
#[derive(Default)]
struct Export {
    /// Each file on disk, with its items.
    files: BTreeMap<String, Items>,
    /// Items with no file on disk that the history watched or started.
    history: Items,
    /// Each series' status.
    status: BTreeMap<String, Value>,
    /// Every item, on disk or not, with its state.
    items: Items,
}

impl Export {
    fn parse(json: &[u8]) -> Result<Self> {
        let series: Vec<Value> = serde_json::from_slice(json)?;
        let mut out = Self::default();
        for s in &series {
            let key = s["key"].as_str().context("a series without a key")?.to_string();
            out.status.insert(key.clone(), s["status"].clone());
            for item in s["items"].as_array().into_iter().flatten() {
                let place = (key.clone(), item["item"].to_string());
                let state = item["state"].clone();
                out.items.insert(place.clone(), state.clone());
                let files = item["files"].as_array().map_or(&[][..], Vec::as_slice);
                if files.is_empty() && state != "unwatched" {
                    out.history.insert(place.clone(), state.clone());
                }
                for f in files {
                    let f = f.as_str().context("a file that isn't a string")?.to_string();
                    out.files.entry(f).or_default().insert(place.clone(), state.clone());
                }
            }
        }
        Ok(out)
    }

    /// The statuses of the series of `items`, sorted.
    fn statuses(&self, items: &Items) -> Vec<String> {
        sorted(items.keys().filter_map(|(key, _)| self.status.get(key)))
    }
}

/// `items` as strings, sorted.
fn sorted<T: Display>(items: impl IntoIterator<Item = T>) -> Vec<String> {
    let mut v: Vec<String> = items.into_iter().map(|i| i.to_string()).collect();
    v.sort();
    v
}

/// An item (as compact JSON) as anipv shows it (`01`, `OVA 2`).
fn label(item: &str) -> String {
    serde_json::from_str::<ItemKey>(item).map_or_else(|_| item.to_string(), |k| k.describe())
}

/// The series `items` are in, deduplicated.
fn series_of(items: &Items) -> String {
    let mut s = sorted(items.keys().map(|k| &k.0));
    s.dedup();
    s.join("; ")
}

/// The items of `items`, as anipv shows them.
fn items_of(items: &Items) -> String {
    sorted(items.keys().map(|k| label(&k.1))).join("; ")
}

/// `what: from → to`, the lists `; `-separated.
fn arrow(what: &str, from: &[String], to: &[String]) -> String {
    format!("{what}: {from} → {to}", from = from.join("; "), to = to.join("; "))
}

/// The keys of `a` not in `b`.
fn only_in<'a, V, W>(a: &'a BTreeMap<String, V>, b: &BTreeMap<String, W>) -> Vec<&'a String> {
    a.keys().filter(|k| !b.contains_key(*k)).collect()
}

/// The base name of a path.
fn base(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

/// Write `title: N`, then the first `shown` lines and how many more there are.
fn list(out: &mut impl Write, indent: &str, title: &str, lines: &[impl Display], shown: usize) -> Result<()> {
    writeln!(out, "{indent}{title}: {n}", n = lines.len())?;
    for line in lines.iter().take(shown) {
        writeln!(out, "{indent}  {line}")?;
    }
    if lines.len() > shown {
        writeln!(out, "{indent}  … and {more} more (--all)", more = lines.len() - shown)?;
    }
    Ok(())
}

/// Series `key` and the series its files went to (`went`).
fn targets<'a>(key: &'a str, went: &'a Moves<'a>) -> impl Iterator<Item = &'a str> {
    went.get(key).into_iter().flatten().copied().chain([key])
}

/// History-only items of `a` that `b` has neither at their place nor in a
/// series their series' files went to (`went`), with that state.
fn unfollowed(a: &Export, b: &Export, went: &Moves) -> Vec<String> {
    a.history
        .iter()
        .filter(|((key, item), state)| {
            !targets(key, went).any(|s| b.items.get(&(s.to_string(), item.clone())) == Some(*state))
        })
        .map(|((key, item), state)| format!("{key} {item}: {state}", item = label(item)))
        .collect()
}

/// Statuses (other than untracked) of series of `a` that `b` doesn't have,
/// and that no series their files went to (`went`) has either.
fn unfollowed_status(a: &Export, b: &Export, went: &Moves) -> Vec<String> {
    only_in(&a.status, &b.status)
        .into_iter()
        .filter_map(|key| {
            let status = &a.status[key];
            let followed = targets(key, went).any(|s| b.status.get(s) == Some(status));
            (*status != "untracked" && !followed).then(|| format!("{key}: {status}"))
        })
        .collect()
}

/// Write the report; true if anything a keying change must never do happened.
fn report(old: &Export, new: &Export, all: bool, out: &mut impl Write) -> Result<bool> {
    let shown = if all { usize::MAX } else { SHOWN };

    // Files in both: moved (grouped by series old → new), states and statuses.
    let mut moved: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let (mut states, mut left) = (Vec::new(), Vec::new());
    let mut compared = 0;
    // Where each series' files went, old → new, and back.
    let (mut went, mut came): (Moves, Moves) = Default::default();
    for (file, was) in &old.files {
        let Some(is) = new.files.get(file) else { continue };
        compared += 1;
        for (from, _) in was.keys() {
            for (to, _) in is.keys() {
                went.entry(from).or_default().insert(to);
                came.entry(to).or_default().insert(from);
            }
        }
        if was.keys().ne(is.keys()) {
            let line = format!("{name}: {from} → {to}", name = base(file), from = items_of(was), to = items_of(is));
            moved.entry((series_of(was), series_of(is))).or_default().push(line);
            let (from, to) = (old.statuses(was), new.statuses(is));
            if from != to {
                left.push(arrow(base(file), &from, &to));
            }
        }
        let (from, to) = (sorted(was.values()), sorted(is.values()));
        if from != to {
            let at = sorted(was.keys().map(|(k, i)| format!("{k} {i}", i = label(i)))).join("; ");
            states.push(arrow(&format!("{file} ({at})"), &from, &to));
        }
    }
    writeln!(out, "files compared: {compared}")?;
    let n: usize = moved.values().map(Vec::len).sum();
    writeln!(out, "files that changed series or item: {n}")?;
    for ((from, to), lines) in &moved {
        let title = if from == to { format!("{from} (items)") } else { format!("{from} → {to}") };
        list(out, "  ", &title, lines, shown)?;
    }
    let (gone, added) = (only_in(&old.files, &new.files).len(), only_in(&new.files, &old.files).len());
    if gone + added > 0 {
        writeln!(out, "files in only one export (a scan in between?): {gone} old only, {added} new only")?;
    }
    list(out, "", "files whose watch state changed", &states, shown)?;
    list(out, "", "files that moved to a series with another status", &left, shown)?;

    // History only: another state in the same place, or none of that state
    // where its series' files went (gone), or the same the other way (new).
    let history: Vec<String> = old
        .history
        .iter()
        .filter_map(|(place @ (key, item), state)| {
            let now = new.history.get(place).filter(|now| *now != state)?;
            Some(format!("{key} {item}: {state} → {now}", item = label(item)))
        })
        .collect();
    let (lost, gained) = (unfollowed(old, new, &went), unfollowed(new, old, &came));
    list(out, "", "watched or started items only the history knows, state changed", &history, shown)?;
    list(out, "", "watched or started items only the history knows, now gone", &lost, shown)?;
    list(out, "", "watched or started items only the history knows, new", &gained, shown)?;

    list(out, "", "series gone", &only_in(&old.status, &new.status), shown)?;
    list(out, "", "series new", &only_in(&new.status, &old.status), shown)?;
    let statuses: Vec<String> = old
        .status
        .iter()
        .filter_map(|(k, s)| new.status.get(k).filter(|t| *t != s).map(|t| format!("{k}: {s} → {t}")))
        .collect();
    list(out, "", "statuses that changed", &statuses, shown)?;
    let (dropped, set) = (unfollowed_status(old, new, &went), unfollowed_status(new, old, &came));
    list(out, "", "statuses of series gone, not on the series their files went to", &dropped, shown)?;
    list(out, "", "statuses of new series, not from the series their files came from", &set, shown)?;

    let changed = [&states, &left, &history, &lost, &gained, &statuses, &dropped, &set].iter().any(|l| !l.is_empty());
    let bad = compared == 0 || changed;
    let verdict = if compared == 0 {
        "FAIL: no file is in both exports"
    } else if changed {
        "REVIEW: history changed; ship only if each change above is the fix you meant"
    } else {
        "ok: no history changed"
    };
    writeln!(out, "{verdict}")?;
    Ok(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(json: &str) -> Export {
        Export::parse(json.as_bytes()).unwrap()
    }

    fn run(old: &str, new: &str) -> (bool, String) {
        let mut out = Vec::new();
        let bad = report(&export(old), &export(new), false, &mut out).unwrap();
        (bad, String::from_utf8(out).unwrap())
    }

    const OLD: &str = r#"[
      {"key":"show","status":"following","items":[
        {"item":{"kind":"episode","ep":"1"},"state":{"watched":5},"files":["/a/Show S2 - 01.mkv"]},
        {"item":{"kind":"episode","ep":"2"},"state":"unwatched","files":["/a/Show - 02.mkv"]},
        {"item":{"kind":"episode","ep":"9"},"state":{"watched":3},"files":[]}]}]"#;

    /// A file moving to another series with its state and status is a move,
    /// not a failure, and so is history only moving with its series.
    #[test]
    fn moves_with_their_history_pass() {
        let new = r#"[
          {"key":"show","status":"following","items":[
            {"item":{"kind":"episode","ep":"2"},"state":"unwatched","files":["/a/Show - 02.mkv"]}]},
          {"key":"show s2","status":"following","items":[
            {"item":{"kind":"episode","ep":"1"},"state":{"watched":5},"files":["/a/Show S2 - 01.mkv"]},
            {"item":{"kind":"episode","ep":"9"},"state":{"watched":3},"files":[]}]}]"#;
        let (bad, text) = run(OLD, new);
        assert!(!bad, "{text}");
        assert!(text.contains("show → show s2: 1"), "{text}");
        assert!(text.contains("series new: 1\n  show s2"), "{text}");
    }

    /// History left behind, history-only items gone, and statuses changed fail.
    #[test]
    fn lost_history_fails() {
        let new = r#"[
          {"key":"show","status":"paused","items":[
            {"item":{"kind":"episode","ep":"2"},"state":"unwatched","files":["/a/Show - 02.mkv"]}]},
          {"key":"show s2","status":"following","items":[
            {"item":{"kind":"episode","ep":"1"},"state":"unwatched","files":["/a/Show S2 - 01.mkv"]}]}]"#;
        let (bad, text) = run(OLD, new);
        assert!(bad, "{text}");
        assert!(text.contains("files whose watch state changed: 1"), "{text}");
        assert!(text.contains("now gone: 1"), "{text}");
        assert!(text.contains("statuses that changed: 1"), "{text}");
    }

    /// A status that doesn't follow its files to their new series fails,
    /// though the series' key changed.
    #[test]
    fn a_status_left_behind_fails() {
        let new = r#"[
          {"key":"show s2","status":"untracked","items":[
            {"item":{"kind":"episode","ep":"1"},"state":{"watched":5},"files":["/a/Show S2 - 01.mkv"]},
            {"item":{"kind":"episode","ep":"2"},"state":"unwatched","files":["/a/Show - 02.mkv"]},
            {"item":{"kind":"episode","ep":"9"},"state":{"watched":3},"files":[]}]}]"#;
        let (bad, text) = run(OLD, new);
        assert!(bad, "{text}");
        assert!(text.contains("files that moved to a series with another status: 2"), "{text}");
    }

    /// Each item of a file holding several is compared, and a history-only
    /// item that stays put is compared too.
    #[test]
    fn every_item_of_a_file_counts() {
        let old = r#"[{"key":"show","status":"following","items":[
            {"item":{"kind":"episode","ep":"1"},"state":{"watched":5},"files":["/a/Show - 01-02.mkv"]},
            {"item":{"kind":"episode","ep":"2"},"state":"unwatched","files":["/a/Show - 01-02.mkv"]},
            {"item":{"kind":"episode","ep":"9"},"state":{"watched":3},"files":[]}]}]"#;
        let new = r#"[{"key":"show","status":"following","items":[
            {"item":{"kind":"episode","ep":"1"},"state":"unwatched","files":["/a/Show - 01-02.mkv"]},
            {"item":{"kind":"episode","ep":"2"},"state":"unwatched","files":["/a/Show - 01-02.mkv"]},
            {"item":{"kind":"episode","ep":"9"},"state":{"started":{"pos":1.0,"dur":null,"at":4}},"files":[]}]}]"#;
        let (bad, text) = run(old, new);
        assert!(bad, "{text}");
        assert!(text.contains("files whose watch state changed: 1"), "{text}");
        assert!(text.contains("state changed: 1"), "{text}");
    }

    #[test]
    fn nothing_compared_fails() {
        let (bad, text) = run(OLD, "[]");
        assert!(bad && text.contains("files compared: 0"), "{text}");
    }

    #[test]
    fn identical_exports_pass() {
        let (bad, text) = run(OLD, OLD);
        assert!(!bad);
        assert!(text.contains("files that changed series or item: 0"), "{text}");
    }

    /// History lost from one series isn't hidden by another series with the
    /// same item in the same state; history gained is reported too.
    #[test]
    fn history_is_followed_only_where_its_files_went() {
        let old = r#"[
          {"key":"a","status":"untracked","items":[
            {"item":{"kind":"episode","ep":"1"},"state":{"watched":7},"files":[]},
            {"item":{"kind":"episode","ep":"2"},"state":"unwatched","files":["/a/A - 02.mkv"]}]},
          {"key":"b","status":"untracked","items":[
            {"item":{"kind":"episode","ep":"1"},"state":{"watched":7},"files":["/a/B - 01.mkv"]}]}]"#;
        let new = r#"[
          {"key":"a","status":"untracked","items":[
            {"item":{"kind":"episode","ep":"2"},"state":"unwatched","files":["/a/A - 02.mkv"]},
            {"item":{"kind":"episode","ep":"5"},"state":{"watched":7},"files":[]}]},
          {"key":"b","status":"untracked","items":[
            {"item":{"kind":"episode","ep":"1"},"state":{"watched":7},"files":["/a/B - 01.mkv"]}]}]"#;
        let (bad, text) = run(old, new);
        assert!(bad, "{text}");
        assert!(text.contains("now gone: 1\n  a 01"), "{text}");
        assert!(text.contains("new: 1\n  a 05"), "{text}");
    }

    /// A status gone with its series fails, unless the series its files went
    /// to has it.
    #[test]
    fn a_status_gone_with_its_series_fails() {
        let old = r#"[{"key":"x","status":"dropped","items":[
            {"item":{"kind":"episode","ep":"1"},"state":"unwatched","files":["/a/X - 01.mkv"]}]}]"#;
        let kept = r#"[{"key":"x (2019)","status":"dropped","items":[
            {"item":{"kind":"episode","ep":"1"},"state":"unwatched","files":["/a/X - 01.mkv"]}]}]"#;
        let lost = r#"[{"key":"x (2019)","status":"untracked","items":[
            {"item":{"kind":"episode","ep":"1"},"state":"unwatched","files":["/a/X - 01.mkv"]}]}]"#;
        let (bad, text) = run(old, kept);
        assert!(!bad, "{text}");
        let (bad, text) = run(old, lost);
        assert!(bad, "{text}");
        assert!(text.contains("not on the series their files went to: 1\n  x: \"dropped\""), "{text}");
    }

    /// The binary is found among cargo's messages, whatever else is there.
    #[test]
    fn the_binary_is_found_in_cargos_messages() {
        let messages = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"serde"},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"anipv"},"executable":null}"#,
            "\nnot json\n",
            r#"{"reason":"compiler-artifact","target":{"name":"anipv"},"executable":"/t/debug/anipv"}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );
        assert_eq!(executable(messages.as_bytes()), Some(PathBuf::from("/t/debug/anipv")));
        assert_eq!(executable(b""), None);
    }

    /// Long lists say how many more `--all` shows.
    #[test]
    fn lists_say_how_many_more() {
        let mut out = Vec::new();
        let lines: Vec<String> = (0..7).map(|i| i.to_string()).collect();
        list(&mut out, "", "things", &lines, SHOWN).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("… and 2 more (--all)"));
    }
}
