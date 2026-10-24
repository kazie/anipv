//! Command-line interface.

mod meta;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::app::{Ctx, Session, find_series, parse_episode_list, queue_new};
use crate::config::{Config, Paths, RootKind};
use crate::events::{EventBody, Recorded, now};
use crate::fmt::{ago, ago_opt, bold, cyan, dim, green, magenta, pad, red, yellow};
use crate::model::{ItemKind, SeriesStatus, WatchState};

const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Green.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Red.on_default());

/// anipv — keep track of the anime you watch with mpv.
///
/// Run without arguments to open the TUI. Watch history is stored as
/// append-only per-device logs that you can sync between machines.
#[derive(Debug, Parser)]
#[command(name = "anipv", version, styles = STYLES, max_term_width = 100)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Open the interactive TUI (default).
    Tui,
    /// Create a config file with your media folders.
    Init(InitArgs),
    /// Scan media folders for new and removed files.
    Scan {
        /// Only scan this root.
        root: Option<String>,
    },
    /// Scan every media folder, then update metadata as the TUI does on start
    /// (and the offline database weekly). Made to run unattended, from a
    /// systemd timer say: a folder that can't be reached (unmounted) or
    /// trouble with the network is a warning, and what is known stays as it was.
    Update {
        /// Only scan; leave metadata alone.
        #[arg(long)]
        no_meta: bool,
    },
    /// Followed series with unwatched episodes on disk.
    Next {
        /// Include paused series.
        #[arg(short, long)]
        paused: bool,
    },
    /// New untracked shows in your download folders (the Inbox).
    New,
    /// List series.
    Ls {
        /// Only these statuses (repeatable).
        #[arg(short, long, value_enum)]
        status: Vec<SeriesStatus>,
        /// Include series with no files on disk.
        #[arg(short, long)]
        all: bool,
    },
    /// Show the episodes of a series.
    Show {
        /// Series title, key or fuzzy query.
        series: String,
        /// Also list episodes that are not on disk (known from history).
        #[arg(long)]
        all: bool,
    },
    /// Play the next episode(s) of one or more series in a single mpv.
    Play {
        /// Series to play (in this order).
        #[arg(required = true)]
        series: Vec<String>,
        /// Episodes per series.
        #[arg(short = 'n', long, default_value_t = 1, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        count: usize,
        /// Only print the mpv command line.
        #[arg(long)]
        print: bool,
    },
    /// Mark episodes watched (or unwatched).
    Mark {
        /// Series title, key or fuzzy query.
        series: String,
        /// Episodes, e.g. `7`, `1-12`, `3,5,8`.
        episodes: String,
        /// Mark as unwatched instead.
        #[arg(short, long)]
        unwatched: bool,
    },
    /// Set a series' status.
    Status {
        /// Series title, key or fuzzy query.
        series: String,
        /// New status.
        #[arg(value_enum)]
        status: SeriesStatus,
        /// Optional note, e.g. why you dropped it.
        #[arg(long)]
        note: Option<String>,
    },
    /// Merge series FROM into series TO (e.g. English and romaji names).
    Merge {
        /// Series to merge away.
        from: String,
        /// Series to keep.
        to: String,
    },
    /// Undo a merge: make a merged-away name its own series again.
    Unmerge {
        /// The merged-away name (key or title, as shown under "also:").
        key: String,
    },
    /// You've seen a series and everything before it in its franchise: mark
    /// them watched and completed, and follow the next entry.
    CatchUp {
        /// The last series you've seen (title, key or fuzzy query), or the
        /// name of a franchise's entry folder (`Kabukimonogatari`).
        series: String,
        /// Only print what would be marked.
        #[arg(long)]
        dry_run: bool,
    },
    /// Make a folder inside a series' folder a series of its own: another
    /// anime kept with it (a mini series, a sequel), named by its files.
    Split {
        /// The series the folder's files are in now (title, key or fuzzy query).
        series: String,
        /// The folder: its name, or part of it.
        folder: String,
        /// Undo a split: the folder's files rejoin the series around them.
        #[arg(long)]
        undo: bool,
    },
    /// Set a series' display title.
    Rename {
        /// Series title, key or fuzzy query.
        series: String,
        /// New title.
        title: String,
    },
    /// Import watch history from "mpv …" commands in fish history.
    ImportFish(ImportArgs),
    /// Metadata from the offline anime database and AniList.
    #[command(subcommand)]
    Meta(meta::MetaAction),
    /// Show file names the parser could not make sense of, and other checks.
    Doctor,
    /// Print all series and episode states as JSON.
    Export,
    /// Print shell completions.
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },
    /// Print the man page (roff).
    Man,
    /// Create a demo library in DIR (for trying anipv out / screenshots).
    #[cfg(feature = "demo")]
    #[command(hide = true)]
    Demo {
        /// Directory to create it in.
        dir: PathBuf,
    },
    /// Stand in for mpv, speaking its JSON IPC (tests, the README recording).
    #[cfg(feature = "demo")]
    #[command(hide = true)]
    FakeMpv {
        /// Play in real time and answer remote-mode keys.
        #[arg(long)]
        demo: bool,
        /// mpv's arguments: `--input-ipc-server=PATH` and the files.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<std::ffi::OsString>,
    },
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Folder with ongoing downloads (series from file names). Repeatable.
    #[arg(long, value_name = "DIR")]
    ongoing: Vec<PathBuf>,
    /// Archive folder with one sub-folder per series. Repeatable.
    #[arg(long, value_name = "DIR")]
    archive: Vec<PathBuf>,
    /// Where to keep event logs (point at a synced folder).
    #[arg(long, value_name = "DIR")]
    events_dir: Option<PathBuf>,
    /// Name of this device in the logs (default: hostname).
    #[arg(long)]
    device: Option<String>,
    /// Overwrite an existing config.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct ImportArgs {
    /// History file (default: `~/.local/share/fish/fish_history`).
    #[arg(long, value_name = "FILE")]
    history: Option<PathBuf>,
    /// Show what would be imported without writing anything.
    #[arg(long)]
    dry_run: bool,
    /// Also apply the suggested statuses (following / paused).
    #[arg(long)]
    apply_status: bool,
}

/// The clap command, for completions and man pages.
pub fn command() -> clap::Command {
    Cli::command()
}

/// Entry point used by `main.rs`.
pub fn main() -> Result<()> {
    let cli = Cli::parse();
    run(cli)
}

fn run(cli: Cli) -> Result<()> {
    let mut out = std::io::stdout().lock();
    match cli.command.unwrap_or(Cmd::Tui) {
        Cmd::Tui => crate::tui::run(Ctx::load()?),
        Cmd::Init(a) => init(a, &mut out),
        Cmd::Scan { root } => {
            let mut ctx = Ctx::load()?;
            scan(&mut ctx, root.as_deref(), &mut out)
        }
        Cmd::Update { no_meta } => {
            let mut ctx = Ctx::load()?;
            update(&mut ctx, no_meta, &mut out)
        }
        Cmd::Next { paused } => next(&Ctx::load()?, paused, &mut out),
        Cmd::New => inbox(&Ctx::load()?, &mut out),
        Cmd::Ls { status, all } => ls(&Ctx::load()?, &status, all, &mut out),
        Cmd::Show { series, all } => show(&Ctx::load()?, &series, all, &mut out),
        Cmd::Play { series, count, print } => {
            let ctx = Ctx::load()?;
            writing_unless(print, &ctx, &mut out, |s, out| play(&ctx, s, &series, count, print, out))
        }
        Cmd::Mark { series, episodes, unwatched } => {
            let ctx = Ctx::load()?;
            writing(&ctx, &mut out, |s, out| mark(&ctx, s, &series, &episodes, unwatched, out))
        }
        Cmd::Status { series, status, note } => {
            let ctx = Ctx::load()?;
            writing(&ctx, &mut out, |s, out| set_status(&ctx, s, &series, status, note, out))
        }
        Cmd::Merge { from, to } => {
            let ctx = Ctx::load()?;
            writing(&ctx, &mut out, |s, out| merge(&ctx, s, &from, &to, out))
        }
        Cmd::Unmerge { key } => {
            let ctx = Ctx::load()?;
            writing(&ctx, &mut out, |s, out| unmerge(&ctx, s, &key, out))
        }
        Cmd::CatchUp { series, dry_run } => {
            let ctx = Ctx::load()?;
            writing_unless(dry_run, &ctx, &mut out, |s, out| catch_up(&ctx, s, &series, dry_run, out))
        }
        Cmd::Split { series, folder, undo } => {
            let ctx = Ctx::load()?;
            writing(&ctx, &mut out, |s, out| split(&ctx, s, &series, &folder, undo, out))
        }
        Cmd::Rename { series, title } => {
            let ctx = Ctx::load()?;
            writing(&ctx, &mut out, |s, out| rename(&ctx, s, &series, &title, out))
        }
        Cmd::ImportFish(a) => {
            let ctx = Ctx::load()?;
            writing_unless(a.dry_run, &ctx, &mut out, |s, out| import_fish(&ctx, s, a, out))
        }
        Cmd::Meta(m) => meta::run(&Ctx::load()?, m, &mut out),
        Cmd::Doctor => doctor(&Ctx::load()?, &mut out),
        Cmd::Export => export(&Ctx::load()?, &mut out),
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut command(), "anipv", &mut out);
            Ok(())
        }
        Cmd::Man => {
            clap_mangen::Man::new(command()).render(&mut out)?;
            Ok(())
        }
        #[cfg(feature = "demo")]
        Cmd::Demo { dir } => {
            std::fs::create_dir_all(&dir)?;
            let dir = dir.canonicalize()?;
            let mut ctx = crate::demo::setup(&dir, now())?;
            // Playing works too: this binary (its own link or copy of it)
            // stands in for mpv, in real time and answering remote-mode keys.
            ctx.cfg.mpv = demo_player(&dir)?.display().to_string();
            ctx.cfg.mpv_args = vec!["fake-mpv".into(), "--demo".into()];
            ctx.cfg.save(&ctx.paths.config_file)?;
            writeln!(out, "{ok} demo library in {dir}", ok = green("✓"), dir = dir.display())?;
            writeln!(out, "  try: {cmd}", cmd = cyan(&format!("ANIPV_HOME={dir} anipv", dir = dir.display())))?;
            Ok(())
        }
        #[cfg(feature = "demo")]
        Cmd::FakeMpv { demo, args } => crate::fake_mpv::run(demo, &args),
    }
}

/// This binary as the demo's player, `bin/anipv` in the demo folder: a link
/// to it, else a copy. Not the binary itself, which the next build without
/// the `demo` feature replaces with one that has no `fake-mpv`.
#[cfg(feature = "demo")]
fn demo_player(dir: &Path) -> Result<PathBuf> {
    use anyhow::Context as _;
    let exe = std::env::current_exe()?;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let player = bin.join("anipv");
    if std::fs::hard_link(&exe, &player).is_err() {
        std::fs::copy(&exe, &player)
            .with_context(|| format!("copying {exe} to {bin}", exe = exe.display(), bin = bin.display()))?;
    }
    Ok(player)
}

fn init(a: InitArgs, out: &mut impl Write) -> Result<()> {
    let paths = Paths::resolve(None)?;
    if paths.config_file.exists() && !a.force {
        bail!("{config_file} already exists (use --force to overwrite)", config_file = paths.config_file.display());
    }
    if a.ongoing.is_empty() && a.archive.is_empty() {
        bail!("give at least one --ongoing or --archive folder");
    }
    // Stored absolute (a leading `~` kept, like roots): a relative path would
    // put the event log wherever anipv happens to be run from.
    let events_dir = a.events_dir.as_deref().map(crate::config::absolute_keep_tilde).transpose()?;
    let mut cfg = Config { device: a.device, events_dir, ..Config::default() };
    for d in &a.ongoing {
        cfg.add_root(d, RootKind::Ongoing)?;
    }
    for d in &a.archive {
        cfg.add_root(d, RootKind::Archive)?;
    }
    cfg.validate()?;
    cfg.save(&paths.config_file)?;
    writeln!(out, "{ok} wrote {file}", ok = green("✓"), file = paths.config_file.display())?;
    writeln!(
        out,
        "  next: {scan} then {import} (optional) and {tui}",
        scan = cyan("anipv scan"),
        import = cyan("anipv import-fish --dry-run"),
        tui = cyan("anipv")
    )?;
    Ok(())
}

/// Run a command that writes (watch state, statuses, merges, metadata) on a
/// [`Session`], then complete followed series that are now finished and fully
/// watched, and say so. Every writing command runs through here, so none
/// leaves a finished series uncompleted.
///
/// Nothing is written while part of the event log can't be read: what a
/// command decides from the rest (what is unwatched, which status to
/// suggest) may be wrong, and a newer event would override what the missing
/// part holds once it is back.
#[expect(clippy::print_stderr, reason = "CLI warnings go to the terminal, not to command output")]
fn writing<W: Write>(ctx: &Ctx, out: &mut W, command: impl FnOnce(&mut Session, &mut W) -> Result<()>) -> Result<()> {
    let mut s = ctx.loaded()?;
    if s.lib.partial_history {
        bail!("part of the event log can't be read (see the warning above); nothing is changed until it can be");
    }
    // Completing runs even if the command failed part way (output closed,
    // say): what it recorded may have finished a series. What completing
    // records is reported either way; the command's own error comes first.
    let res = command(&mut s, out);
    let done = complete(ctx, &mut s);
    if let (Err(_), Err(e)) = (&res, &done) {
        eprintln!("warning: {e:#}");
    }
    let reported = done.iter().try_for_each(|done| report_automated(done, out));
    res?;
    done?;
    Ok(reported?)
}

/// Tell what [`complete`] recorded.
fn report_automated(done: &crate::app::Automated, out: &mut impl Write) -> std::io::Result<()> {
    for title in &done.completed {
        writeln!(out, "{ok} {title} completed: all episodes watched", ok = green("✓"), title = bold(title))?;
    }
    for (title, after) in &done.followed {
        let (ok, title) = (green("→"), bold(title));
        writeln!(out, "{ok} {title} is now following: the next entry after {after}")?;
    }
    Ok(())
}

/// [`writing`]'s second half: rebuild the library from what the command
/// recorded (showing the warnings that are new) and complete what is done.
#[expect(clippy::print_stderr, reason = "CLI warnings go to the terminal, not to command output")]
fn complete(ctx: &Ctx, s: &mut Session) -> Result<crate::app::Automated> {
    let shown = std::mem::take(&mut s.lib.warnings);
    s.rebuild(ctx)?;
    for w in s.lib.warnings.iter().filter(|w| !shown.contains(w)) {
        eprintln!("warning: {w}");
    }
    Ok(ctx.auto_complete(&s.lib)?.1)
}

/// [`writing`], or for a `dry_run` just the command on the library as loaded.
fn writing_unless<W: Write>(
    dry_run: bool,
    ctx: &Ctx,
    out: &mut W,
    command: impl FnOnce(&mut Session, &mut W) -> Result<()>,
) -> Result<()> {
    if dry_run { command(&mut ctx.loaded()?, out) } else { writing(ctx, out, command) }
}

fn mark(ctx: &Ctx, s: &mut Session, query: &str, episodes: &str, unwatched: bool, out: &mut impl Write) -> Result<()> {
    let series = find_series(&s.lib, query)?;
    let items = parse_episode_list(episodes)?;
    let recorded = ctx.mark(series, &items, !unwatched)?;
    let changed = recorded.events.len();
    let state = if unwatched { "unwatched" } else { "watched" };
    write!(
        out,
        "{ok} marked {changed} episode(s) of {title} as {state}",
        ok = green("✓"),
        title = bold(&series.title)
    )?;
    // Marking unwatched leaves alone (and counts apart) episodes anipv knows
    // nothing about, while marking watched records them.
    let unknown = if unwatched { items.iter().filter(|k| series.item(k).is_none()).count() } else { 0 };
    let already = items.len() - changed - unknown;
    let notes: Vec<String> = [(already, "already were"), (unknown, "not known")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    if notes.is_empty() {
        writeln!(out)?;
    } else {
        writeln!(out, " ({notes})", notes = notes.join(", "))?;
    }
    s.add(ctx, recorded);
    Ok(())
}

fn set_status(
    ctx: &Ctx,
    s: &mut Session,
    query: &str,
    status: SeriesStatus,
    note: Option<String>,
    out: &mut impl Write,
) -> Result<()> {
    let series = find_series(&s.lib, query)?;
    let recorded = ctx.set_status(&s.lib, &series.key, status, note)?;
    let (ok, title, status) = (green("✓"), bold(&series.title), status_label(status));
    writeln!(out, "{ok} {title} is now {status}")?;
    s.add(ctx, recorded);
    Ok(())
}

fn merge(ctx: &Ctx, s: &mut Session, from: &str, to: &str, out: &mut impl Write) -> Result<()> {
    let (f, t) = (find_series(&s.lib, from)?, find_series(&s.lib, to)?);
    if f.key == t.key {
        bail!("{from} and {to} are already the same series");
    }
    let recorded = ctx.record([EventBody::Alias { from: f.key.clone(), to: t.key.clone() }])?;
    writeln!(out, "{ok} merged {from} into {to}", ok = green("✓"), from = bold(&f.title), to = bold(&t.title))?;
    s.add(ctx, recorded);
    Ok(())
}

fn unmerge(ctx: &Ctx, s: &mut Session, key: &str, out: &mut impl Write) -> Result<()> {
    let lib = &s.lib;
    let normalized = crate::identity::series_key(key);
    // The name may be keyed as it was before kana voiced marks were kept.
    let legacy = crate::identity::legacy_series_key(key);
    let Some(from) = [key, normalized.as_str(), legacy.as_str()].into_iter().find(|k| lib.is_merged_away(k)) else {
        // Maybe they named the series that others were merged into.
        if let Ok(series) = find_series(lib, key)
            && !series.aliases.is_empty()
        {
            let (title, names) = (&series.title, series.aliases.join(", "));
            bail!("{title} has merged names; unmerge one of: {names}");
        }
        bail!("{key:?} is not a merged-away name (see the \"also:\" line of `anipv show`)");
    };
    let recorded = ctx.unmerge(lib, [from])?;
    writeln!(out, "{ok} {key} is a separate series again", ok = green("✓"), key = lib.current_key(from))?;
    s.add(ctx, recorded);
    Ok(())
}

fn catch_up(ctx: &Ctx, s: &mut Session, query: &str, dry_run: bool, out: &mut impl Write) -> Result<()> {
    // An entry folder's name names that stop (`kabukimonogatari`, part of a
    // longer series); otherwise all of the series named.
    let plan = match s.lib.stop_named(query) {
        Some((f, pos)) => crate::app::catch_up_to(&s.lib, f, pos),
        None => crate::app::catch_up(&s.lib, &find_series(&s.lib, query)?.key)?,
    };
    let (n, eps, list) = (plan.entries.len(), plan.episodes, plan.entries.join(", "));
    if dry_run {
        writeln!(out, "would mark {eps} episode(s) of {n} entries watched: {list}")?;
        return Ok(());
    }
    let recorded = ctx.catch_up(plan)?;
    writeln!(out, "{ok} caught up: {eps} episode(s) of {n} entries marked watched ({list})", ok = green("✓"))?;
    s.add(ctx, recorded);
    Ok(())
}

fn split(ctx: &Ctx, s: &mut Session, query: &str, folder: &str, undo: bool, out: &mut impl Write) -> Result<()> {
    let series = find_series(&s.lib, query)?;
    let candidates = if undo { s.lib.splits_of(series) } else { s.lib.splittable(&ctx.cfg, series) };
    let picked = pick_folder(&candidates, folder);
    let (root, dir) = match &picked[..] {
        [one] => one.clone(),
        [] => {
            let what = if undo { "split folders" } else { "folders that could be split off" };
            let names: Vec<&str> = candidates.iter().map(|(_, d)| d.as_str()).collect();
            let names = if names.is_empty() { "none".to_string() } else { names.join(", ") };
            bail!("{folder:?} names none of {title}'s {what} ({names})", title = series.title)
        }
        many => {
            let names: Vec<&str> = many.iter().map(|(_, d)| d.as_str()).collect();
            bail!("{folder:?} names several folders; give more of the path: {names}", names = names.join(", "))
        }
    };
    let body =
        if undo { EventBody::Unsplit { root, dir: dir.clone() } } else { EventBody::Split { root, dir: dir.clone() } };
    let recorded = ctx.record([body])?;
    let what = if undo { "rejoins its series" } else { "is a series of its own" };
    writeln!(out, "{ok} {dir} {what}", ok = green("✓"), dir = bold(&dir))?;
    s.add(ctx, recorded);
    Ok(())
}

/// The folders among `candidates` (`(root, dir)`) whose last part (or whole
/// path) is `query`, else those whose last part contains it (ignoring case).
fn pick_folder(candidates: &[(String, String)], query: &str) -> Vec<(String, String)> {
    let q = query.to_lowercase();
    let last = |d: &str| crate::library::file_name_lossy(Path::new(d)).to_lowercase();
    let exact: Vec<_> = candidates.iter().filter(|(_, d)| last(d) == q || d.to_lowercase() == q).collect();
    let pool = if exact.is_empty() { candidates.iter().filter(|(_, d)| last(d).contains(&q)).collect() } else { exact };
    pool.into_iter().cloned().collect()
}

fn rename(ctx: &Ctx, s: &mut Session, query: &str, title: &str, out: &mut impl Write) -> Result<()> {
    let series = find_series(&s.lib, query)?;
    let recorded = ctx.record([EventBody::Title { series: series.key.clone(), title: title.to_string() }])?;
    writeln!(out, "{ok} {old} → {new}", ok = green("✓"), old = series.title, new = bold(title))?;
    s.add(ctx, recorded);
    Ok(())
}

#[expect(clippy::print_stderr, reason = "live progress goes to the terminal, not to `out`")]
fn scan(ctx: &mut Ctx, only: Option<&str>, out: &mut impl Write) -> Result<()> {
    let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let res = ctx.scan(only, &|root, n| {
        if tty {
            eprint!("\r\x1b[2Kscanning {root}… {n} files");
        }
    })?;
    if tty {
        eprint!("\r\x1b[2K");
    }
    let total = res.len();
    let failed = print_scans(res, false, out)?;
    // Any failed root fails the command, so scripts notice an unmounted drive.
    if failed > 0 {
        bail!("{failed} of {total} root(s) could not be scanned");
    }
    Ok(())
}

/// One line per scanned root. Returns how many failed; with `lenient`, a
/// root that could not be reached (unmounted) is a warning instead.
fn print_scans(res: Vec<crate::app::RootScan>, lenient: bool, out: &mut impl Write) -> Result<usize> {
    let mut failed = 0;
    for r in res {
        match r.stats {
            Ok(s) => writeln!(
                out,
                "{ok} {root:<12} {seen:>6} files  {new} new  {gone} gone{unreadable}",
                ok = green("✓"),
                root = r.root,
                seen = s.seen,
                new = green(&s.new.to_string()),
                gone = yellow(&s.gone.to_string()),
                unreadable = if r.errors.is_empty() {
                    String::new()
                } else {
                    red(&format!("  ({errors} unreadable dirs)", errors = r.errors.len()))
                }
            )?,
            Err(e) if lenient && r.offline => {
                writeln!(out, "{warn} {root:<12} {e:#}; index kept as it was", warn = yellow("!"), root = r.root)?;
            }
            Err(e) => {
                failed += 1;
                writeln!(out, "{fail} {root:<12} {e:#}", fail = red("✗"), root = r.root)?;
            }
        }
    }
    Ok(failed)
}

/// `update`: [`scan`] every root, leniently, then a background metadata
/// update (see [`meta::background`]), which runs even if a root failed.
fn update(ctx: &mut Ctx, no_meta: bool, out: &mut impl Write) -> Result<()> {
    let res = ctx.scan(None, &|_, _| {})?;
    let total = res.len();
    let failed = print_scans(res, true, out)?;
    let meta = if no_meta { Ok(()) } else { meta::background(ctx, out) };
    if failed > 0 {
        // The scan's error is returned; the metadata one is still told.
        if let Err(e) = &meta {
            writeln!(out, "{fail} metadata: {e:#}", fail = red("✗"))?;
        }
        bail!("{failed} of {total} root(s) could not be scanned");
    }
    meta
}

fn status_label(s: SeriesStatus) -> String {
    status_cell(s, 0)
}

/// Status name padded to `width`, then colored (so ANSI codes don't break alignment).
fn status_cell(s: SeriesStatus, width: usize) -> String {
    let text = pad(s.as_str(), width.max(s.as_str().len()));
    match s {
        SeriesStatus::Following => green(&text),
        SeriesStatus::Paused => yellow(&text),
        SeriesStatus::Dropped => red(&text),
        SeriesStatus::Completed => cyan(&text),
        SeriesStatus::Untracked | SeriesStatus::Skipped => dim(&text),
    }
}

fn inbox(ctx: &Ctx, out: &mut impl Write) -> Result<()> {
    use crate::library::Hint;
    let lib = ctx.library()?;
    let now = now();
    let entries = lib.inbox(&ctx.cfg);
    if entries.is_empty() {
        writeln!(out, "Inbox zero: no untracked shows in your download folders.")?;
        return Ok(());
    }
    writeln!(
        out,
        "{header}",
        header = bold(&format!(
            "{s:<44} {d:>9} {a:>6}  {air:<18}",
            s = "SERIES",
            d = "ON DISK",
            a = "ADDED",
            air = "AIRING"
        ))
    )?;
    for e in &entries {
        let s = &lib.series[e.index];
        let text = e.hint.map(|h| h.describe(&lib)).unwrap_or_default();
        let (title, hint) = match e.hint {
            Some(Hint::NewSeasonOf(_)) => (bold(&pad(&s.title, 44)), magenta(&text)),
            Some(Hint::PartOf(_)) => (dim(&pad(&s.title, 44)), dim(&text)),
            None => (pad(&s.title, 44), text),
        };
        writeln!(
            out,
            "{title} {disk:>9} {added:>6}  {airing}  {hint}",
            disk = s.disk_range(),
            added = ago_opt(s.last_added(), now),
            airing = dim(&pad(&s.airing_text(now), 18))
        )?;
    }
    let (follow, skip) = (cyan("anipv status <series> following"), cyan("anipv status <series> skipped"));
    writeln!(out, "\n{follow} or {skip}")?;
    Ok(())
}

fn next(ctx: &Ctx, paused: bool, out: &mut impl Write) -> Result<()> {
    let lib = ctx.library()?;
    let now = now();
    let list = lib.up_next(paused);
    if list.is_empty() {
        writeln!(
            out,
            "Nothing followed yet. Try {follow} or {import}.",
            follow = cyan("anipv status <series> following"),
            import = cyan("anipv import-fish")
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "{header}",
        header = bold(&format!(
            "{s:<40} {n:>6} {new:>4} {w:>9} {a:>8}  {air}",
            s = "SERIES",
            n = "NEXT",
            new = "NEW",
            w = "WATCHED",
            a = "ACTIVE",
            air = "AIRING"
        ))
    )?;
    for s in list {
        let next = s.next_up().map_or_else(|| "—".into(), |i| i.key.describe());
        let new = s.new_count();
        let active = ago_opt(s.last_activity(), now);
        let title = if s.status == SeriesStatus::Paused { yellow(&pad(&s.title, 40)) } else { pad(&s.title, 40) };
        writeln!(
            out,
            "{title} {next:>6} {new:>4} {progress:>9} {active:>8}  {airing}",
            new = if new > 0 { green(&format!("{new:>4}")) } else { dim(&format!("{new:>4}")) },
            progress = s.progress(),
            airing = dim(&s.airing_text(now))
        )?;
        if let Some(place) = lib.franchise_place(&s.key) {
            writeln!(out, "  {place}", place = dim(&format!("↳ {place}")))?;
        }
    }
    Ok(())
}

fn ls(ctx: &Ctx, wanted: &[SeriesStatus], all: bool, out: &mut impl Write) -> Result<()> {
    let lib = ctx.library()?;
    let now = now();
    writeln!(
        out,
        "{header}",
        header = bold(&format!(
            "{s:<44} {st:<10} {w:>9} {d:>6} {a:>8}",
            s = "SERIES",
            st = "STATUS",
            w = "WATCHED",
            d = "DISK",
            a = "ACTIVE"
        ))
    )?;
    for s in &lib.series {
        if !wanted.is_empty() && !wanted.contains(&s.status) {
            continue;
        }
        if !all && !s.present() && s.status == SeriesStatus::Untracked {
            continue;
        }
        writeln!(
            out,
            "{title} {status} {progress:>9} {disk:>6} {active:>8}",
            title = pad(&s.title, 44),
            status = status_cell(s.status, 10),
            progress = s.progress(),
            disk = s.on_disk_items(),
            active = ago_opt(s.last_activity(), now)
        )?;
    }
    Ok(())
}

fn state_glyph(st: &WatchState) -> String {
    let g = st.glyph();
    match st {
        WatchState::Watched { .. } => green(g),
        WatchState::Started { .. } => yellow(g),
        WatchState::Unwatched => dim(g),
    }
}

fn show(ctx: &Ctx, query: &str, all: bool, out: &mut impl Write) -> Result<()> {
    let lib = ctx.library()?;
    let s = find_series(&lib, query)?;
    let now = now();
    writeln!(
        out,
        "{title}  {status}  {key}",
        title = bold(&s.title),
        status = status_label(s.status),
        key = dim(&s.key)
    )?;
    for n in s.status_via.and_then(crate::events::StatusVia::label).into_iter().chain(s.note.as_deref()) {
        writeln!(out, "  {note}", note = dim(n))?;
    }
    if !s.aliases.is_empty() {
        writeln!(out, "  {label} {names}", label = dim("also:"), names = s.aliases.join(", "))?;
    }
    if let Some((f, pos)) = lib.franchise_of(&s.key) {
        // This series' stops in bold, the ones watched with a ✓.
        let stops: Vec<String> = f
            .stops
            .iter()
            .map(|stop| {
                let label = lib.stop_label(stop);
                let label = if stop.watched { format!("{label} ✓") } else { label.to_string() };
                if lib.series[stop.series].key == s.key { bold(&label) } else { label }
            })
            .collect();
        let label = dim(&format!("{name} {at}/{stops}:", name = f.name, at = pos + 1, stops = f.stops.len()));
        writeln!(out, "  {label} {stops}", stops = stops.join(" → "))?;
    }
    if s.meta.as_ref().is_some_and(|m| m.anilist.is_some()) {
        let (label, progress, airing) = (dim("meta:"), s.progress(), dim(&s.airing_text(now)));
        writeln!(out, "  {label} watched {progress} · {airing}")?;
    }
    for (dir, n) in s.shared_dirs(&ctx.cfg) {
        let hint =
            format!("anipv split {key:?} {folder:?}", key = s.key, folder = dir.rsplit('/').next().unwrap_or(dir));
        writeln!(
            out,
            "  {warn} {n} episode(s) in {dir} are also in another folder: a second release, or another anime ({hint})?",
            warn = yellow("!"),
            dir = bold(dir),
            hint = cyan(&hint)
        )?;
    }
    writeln!(out)?;
    let missing = s.missing_count(true);
    if missing > 0 && !all {
        writeln!(out, "  {more}", more = dim(&format!("… {missing} more not on disk (--all lists them)")))?;
    }
    for it in s.ordered_items(true, all) {
        let kind = match it.key.kind {
            ItemKind::Episode | ItemKind::Unknown => String::new(),
            ItemKind::Special => magenta(" special"),
            ItemKind::Extra => dim(" extra"),
            ItemKind::Movie => cyan(" movie"),
        };
        let file = it.best_file().map_or_else(|| red("(not on disk)"), crate::library::FileRef::name);
        let extra = match it.state {
            WatchState::Started { .. } => it.state.percent().map(|p| yellow(&format!(" {p}"))).unwrap_or_default(),
            WatchState::Watched { at } => dim(&format!(" {when}", when = ago(at, now))),
            WatchState::Unwatched => String::new(),
        };
        let (glyph, item, file) = (state_glyph(&it.state), it.key.describe(), dim(&file));
        writeln!(out, " {glyph} {item:>8}{kind}{extra}  {file}")?;
    }
    Ok(())
}

fn play(ctx: &Ctx, s: &mut Session, queries: &[String], count: usize, print: bool, out: &mut impl Write) -> Result<()> {
    let lib = &s.lib;
    let mut queue = Vec::new();
    for q in queries {
        let series = find_series(lib, q)?;
        let entries = queue_new(series, count, |_| false);
        if entries.is_empty() {
            writeln!(out, "{warn} nothing new for {title}", warn = yellow("!"), title = series.title)?;
        }
        queue.extend(entries);
    }
    if queue.is_empty() {
        bail!("nothing to play");
    }
    let files = ctx.play_files(lib, &queue);
    if print {
        out.write_all(&crate::mpv::command_line(&ctx.cfg, &files))?;
        writeln!(out)?;
        return Ok(());
    }
    play_blocking(ctx, s, &files, out)
}

/// Run mpv, report and record each finished file.
fn play_blocking(ctx: &Ctx, s: &mut Session, files: &[crate::mpv::PlayFile], out: &mut impl Write) -> Result<()> {
    use crate::mpv::{Outcome, PlayerEvent};
    let Session { lib, log, .. } = s;
    let (tx, rx) = std::sync::mpsc::channel();
    crate::mpv::spawn_tracked(&ctx.cfg, ctx.socket_path(), files, move |e| {
        let _ = tx.send(e);
    })?;
    // Keep following mpv even if writing to `out` fails (a closed pipe), or
    // later files go unrecorded; the first such error is returned at the end.
    let mut written: std::io::Result<()> = Ok(());
    for ev in rx {
        let res = match ev {
            PlayerEvent::Started { path } => {
                let name = crate::library::file_name_lossy(&path);
                writeln!(out, "{play} {name}", play = cyan("▶"))
            }
            PlayerEvent::Ended { path, pos, furthest, dur, eof } => {
                let o = ctx.outcome(pos, furthest, dur, eof);
                match ctx.record_playback(lib, log, &path, o) {
                    Err(e) => {
                        writeln!(out, "{fail} could not record {file}: {e:#}", fail = red("✗"), file = path.display())
                    }
                    Ok(what) => match o {
                        Outcome::Watched => {
                            writeln!(out, "  {mark} watched {what}", mark = state_glyph(&WatchState::Watched { at: 0 }))
                        }
                        Outcome::Partial { pos, .. } => {
                            let (mark, at) =
                                (state_glyph(&WatchState::Started { pos, dur: None, at: 0 }), crate::fmt::clock(pos));
                            writeln!(out, "  {mark} stopped at {at} {what}")
                        }
                        Outcome::Nothing => {
                            writeln!(out, "  {mark} skipped {what}", mark = state_glyph(&WatchState::Unwatched))
                        }
                    },
                }
            }
            PlayerEvent::Error(e) => writeln!(out, "{fail} {e}", fail = red("✗")),
            PlayerEvent::Exited { played, error } => {
                let res = match (played, error) {
                    (_, Some(e)) => writeln!(out, "{fail} {e}", fail = red("✗")),
                    (false, None) => writeln!(out, "{fail} mpv exited before playing anything", fail = red("✗")),
                    (true, None) => Ok(()),
                };
                written = written.and(res);
                break;
            }
            PlayerEvent::Progress { .. } | PlayerEvent::Connected(_) | PlayerEvent::Paused(_) => Ok(()),
        };
        written = written.and(res);
    }
    Ok(written?)
}

fn import_fish(ctx: &Ctx, s: &mut Session, a: ImportArgs, out: &mut impl Write) -> Result<()> {
    use crate::import_fish::{already_imported, default_history_path, plan, read_history};
    let path = a.history.unwrap_or_else(default_history_path);
    let plays = read_history(&path)?;
    let now = now();
    let p = plan(&ctx.cfg, &s.lib, &plays, &already_imported(s.log.events()), now);
    let matched = p.watches.iter().filter(|w| w.matched).count();
    writeln!(
        out,
        "{commands} mpv commands → {watches} watched items ({matched} matched to files on disk, {unmatched} from file names only)",
        commands = p.commands,
        watches = bold(&p.watches.len().to_string()),
        matched = green(&matched.to_string()),
        unmatched = yellow(&(p.watches.len() - matched).to_string())
    )?;
    let touched: HashSet<&str> = p.watches.iter().map(|w| w.series.as_str()).collect();
    writeln!(out, "{touched} series touched", touched = touched.len())?;
    let skipped = [
        (&p.ambiguous, "ambiguous file name(s) skipped (several indexed files share the name)"),
        (&p.unidentified, "file name(s) skipped (nothing in the name or its folder says which show)"),
    ];
    for (names, why) in skipped.into_iter().filter(|(names, _)| !names.is_empty()) {
        writeln!(out, "{warn} {n} {why}", n = names.len(), warn = yellow("!"))?;
        if a.dry_run {
            for name in names {
                writeln!(out, "    {name}", name = dim(name))?;
            }
        }
    }
    if !p.proposals.is_empty() {
        writeln!(out, "\n{heading}", heading = bold("Suggested statuses (for untracked series):"))?;
        for pr in &p.proposals {
            let (status, title, last) = (status_cell(pr.status, 10), pad(&pr.title, 50), dim(&ago(pr.last, now)));
            writeln!(out, "  {status} {title}  {last}")?;
        }
    }
    if a.dry_run {
        writeln!(out, "\n{note}", note = dim("dry run: nothing written"))?;
        return Ok(());
    }
    let events = p.events(ctx.log.device());
    let written = ctx.log.append_events(&events)?;
    s.add(ctx, Recorded { events, written });
    if a.apply_status {
        let recorded = ctx.record(
            p.proposals
                .iter()
                .map(|pr| EventBody::status_via(pr.series.clone(), pr.status, crate::events::StatusVia::FishImport)),
        )?;
        s.add(ctx, recorded);
    }
    let also = if a.apply_status { " and applied statuses" } else { "" };
    writeln!(out, "\n{ok} imported{also}", ok = green("✓"))?;
    if !a.apply_status && !p.proposals.is_empty() {
        writeln!(out, "  re-run with {cmd} to apply the suggestions", cmd = cyan("--apply-status"))?;
    }
    Ok(())
}

fn doctor(ctx: &Ctx, out: &mut impl Write) -> Result<()> {
    writeln!(out, "{label} {file}", label = bold("config:"), file = ctx.paths.config_file.display())?;
    let (dir, device) = (ctx.log.dir().display(), ctx.log.device());
    writeln!(out, "{label} {dir} ({device})", label = bold("events:"))?;
    if ctx.cfg.device.is_none() {
        writeln!(
            out,
            "{warning}",
            warning = yellow(&format!(
                "  device name {device:?} comes from the hostname; set `device` in the config so a hostname \
                 change does not start a new event log"
            ))
        )?;
    }
    let rebuilt = if ctx.db.was_rebuilt() {
        yellow("  (cache was outdated and has been reset; run `anipv scan`)")
    } else {
        String::new()
    };
    writeln!(out, "{label} {file}{rebuilt}", label = bold("index: "), file = ctx.paths.db_file.display())?;
    for r in &ctx.cfg.roots {
        let ok = r.resolved().is_dir();
        let last = match ctx.db.last_scan(&r.name)? {
            Some(s) => format!(
                "scanned {when} · {seen} files · {completeness}",
                when = match ago(s.finished_at, now()).as_str() {
                    "now" => "just now".to_string(),
                    t => format!("{t} ago"),
                },
                seen = s.stats.seen,
                completeness = if s.complete { "complete" } else { "incomplete (some folders unreadable)" }
            ),
            None => "never scanned".into(),
        };
        writeln!(
            out,
            "  {mark} {name:<12} {kind:?} {resolved}  {last}",
            mark = if ok { green("✓") } else { red("✗") },
            name = r.name,
            kind = r.kind,
            resolved = r.resolved().display(),
            last = dim(&last)
        )?;
    }
    // Warnings are part of this report, not printed on their own.
    let Session { log, lib, .. } = ctx.session()?;
    writeln!(out, "{label} {n} events", label = bold("log:   "), n = log.events().len())?;
    for w in &lib.warnings {
        writeln!(out, "  {fail} {w}", fail = red("✗"))?;
    }
    // Unreadable files are among the warnings; these are the malformed lines.
    let lines: Vec<&String> = log.errors().iter().filter(|e| !log.unreadable().contains(e)).collect();
    for e in lines.iter().take(10) {
        writeln!(out, "  {fail} {e}", fail = red("✗"))?;
    }
    if lines.len() > 10 {
        writeln!(out, "  {more}", more = dim(&format!("… and {n} more malformed lines", n = lines.len() - 10)))?;
    }
    let mut odd: Vec<&crate::library::IndexedFile> = lib
        .files
        .iter()
        .filter(|f| f.file.present && f.items.iter().any(|i| i.kind == ItemKind::Episode && i.ep.is_none()))
        .collect();
    let ongoing = ctx.cfg.ongoing_roots();
    let movies_in_ongoing: Vec<_> = lib
        .files
        .iter()
        .filter(|f| f.file.present && f.items.iter().any(|i| i.kind == ItemKind::Movie))
        .filter(|f| ongoing.contains(f.file.root.as_str()))
        .collect();
    odd.extend(movies_in_ongoing);
    writeln!(
        out,
        "\n{label} {odd} files without an episode number (movies/one-offs or parser misses):",
        label = bold("parse:"),
        odd = odd.len()
    )?;
    for f in odd.iter().take(50) {
        let (series, file) = (pad(&f.series, 40), dim(&f.file.rel.to_string_lossy()));
        writeln!(out, "  {series}  {file}")?;
    }
    Ok(())
}

fn export(ctx: &Ctx, out: &mut impl Write) -> Result<()> {
    let lib = ctx.library()?;
    let series: Vec<serde_json::Value> = lib
        .series
        .iter()
        .map(|s| {
            serde_json::json!({
                "key": s.key,
                "title": s.title,
                "status": s.status,
                "note": s.note,
                // `null` for a way from a newer version (not a made-up name).
                "via": s.status_via.filter(|v| *v != crate::events::StatusVia::Unknown),
                "aliases": s.aliases,
                "anilist": s.anilist_id(),
                "total": s.total(),
                "items": s.items.iter().map(|i| serde_json::json!({
                    "item": i.key,
                    "state": match i.state {
                        WatchState::Unwatched => serde_json::json!("unwatched"),
                        WatchState::Started { pos, dur, at } => serde_json::json!({"started": {"pos": pos, "dur": dur, "at": at}}),
                        WatchState::Watched { at } => serde_json::json!({"watched": at}),
                    },
                    "files": i.files.iter().filter(|f| f.present).map(|f| f.path.to_string_lossy().into_owned()).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    // Buffered here: stdout flushes every line, and this is a lot of lines.
    let mut buf = std::io::BufWriter::with_capacity(1 << 16, out);
    // As an I/O error, so a closed pipe (`anipv export | head`) is recognized.
    serde_json::to_writer_pretty(&mut buf, &series).map_err(std::io::Error::from)?;
    writeln!(buf)?;
    buf.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command that fails after recording (its output closed, say) still
    /// completes what it finished, and its error is what is returned.
    #[test]
    fn writing_completes_even_when_the_command_fails() {
        let (_dir, ctx) = crate::demo::ctx();
        ctx.set_status(&ctx.library().unwrap(), "yuru camp", SeriesStatus::Following, None).unwrap();
        let mut out = Vec::new();
        let res = writing(&ctx, &mut out, |s, _| {
            let recorded = ctx.mark(s.lib.get("yuru camp").unwrap(), &parse_episode_list("1-12")?, true)?;
            s.add(&ctx, recorded);
            bail!("output closed")
        });
        assert_eq!(res.unwrap_err().to_string(), "output closed");
        assert_eq!(ctx.library().unwrap().get("yuru camp").unwrap().status, SeriesStatus::Completed);
        // ... and says so.
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("completed: all episodes watched"), "{text}");

        // A failure while completing doesn't hide the command's own error.
        let res = writing(&ctx, &mut Vec::new(), |_, _| {
            std::fs::remove_dir_all(&ctx.paths.events_dir)?;
            bail!("disk full")
        });
        assert_eq!(res.unwrap_err().to_string(), "disk full");
    }

    #[test]
    fn cli_definition_is_valid() {
        command().debug_assert();
    }

    #[test]
    fn parses_common_invocations() {
        Cli::try_parse_from(["anipv"]).unwrap();
        Cli::try_parse_from(["anipv", "play", "one piece", "grand blue", "-n", "2", "--print"]).unwrap();
        Cli::try_parse_from(["anipv", "status", "x", "dropped", "--note", "meh"]).unwrap();
        Cli::try_parse_from(["anipv", "update"]).unwrap();
        Cli::try_parse_from(["anipv", "update", "--no-meta"]).unwrap();
        assert!(Cli::try_parse_from(["anipv", "update", "anime"]).is_err(), "always every root");
        Cli::try_parse_from(["anipv", "meta", "refresh"]).unwrap();
        Cli::try_parse_from(["anipv", "meta", "refresh", "frieren"]).unwrap();
        Cli::try_parse_from(["anipv", "meta", "refresh", "--all"]).unwrap();
        assert!(Cli::try_parse_from(["anipv", "meta", "refresh", "frieren", "--all"]).is_err());
        Cli::try_parse_from(["anipv", "meta", "refresh", "--all", "--untracked"]).unwrap();
        assert!(Cli::try_parse_from(["anipv", "meta", "refresh", "--untracked"]).is_err(), "only with --all");
        Cli::try_parse_from(["anipv", "init", "--ongoing", "~/dl", "--archive", "~/anime"]).unwrap();
        assert!(Cli::try_parse_from(["anipv", "status", "x", "bogus"]).is_err());
    }
}
