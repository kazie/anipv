//! Command-line interface.

mod meta;

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::app::{Ctx, Index, find_series, parse_episode_list, queue_new};
use crate::config::{Config, Paths, RootKind};
use crate::events::{CachedLog, EventBody, Recorded, now};
use crate::fmt::{ago, ago_opt, bold, cyan, dim, green, magenta, pad, red, yellow};
use crate::library::Library;
use crate::model::{ItemKey, ItemKind, SeriesStatus, WatchState};

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
        #[arg(short = 'n', long, default_value_t = 1)]
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
    #[command(hide = true)]
    Demo {
        /// Directory to create it in.
        dir: PathBuf,
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
        Cmd::Next { paused } => next(&Ctx::load()?, paused, &mut out),
        Cmd::New => inbox(&Ctx::load()?, &mut out),
        Cmd::Ls { status, all } => ls(&Ctx::load()?, &status, all, &mut out),
        Cmd::Show { series, all } => show(&Ctx::load()?, &series, all, &mut out),
        Cmd::Play { series, count, print } => {
            let ctx = Ctx::load()?;
            if print {
                play(&ctx, &mut Session::load(&ctx)?, &series, count, print, &mut out)
            } else {
                writing(&ctx, &mut out, |s, out| play(&ctx, s, &series, count, print, out))
            }
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
        Cmd::Rename { series, title } => {
            let ctx = Ctx::load()?;
            writing(&ctx, &mut out, |s, out| rename(&ctx, s, &series, &title, out))
        }
        Cmd::ImportFish(a) => {
            let ctx = Ctx::load()?;
            if a.dry_run {
                import_fish(&ctx, &mut Session::load(&ctx)?, a, &mut out)
            } else {
                writing(&ctx, &mut out, |s, out| import_fish(&ctx, s, a, out))
            }
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
        Cmd::Demo { dir } => {
            std::fs::create_dir_all(&dir)?;
            let dir = dir.canonicalize()?;
            crate::demo::setup(&dir, now())?;
            writeln!(out, "{ok} demo library in {dir}", ok = green("✓"), dir = dir.display())?;
            writeln!(out, "  try: {}", cyan(&format!("ANIPV_HOME={} anipv", dir.display())))?;
            Ok(())
        }
    }
}

fn init(a: InitArgs, out: &mut impl Write) -> Result<()> {
    let paths = Paths::resolve(None)?;
    if paths.config_file.exists() && !a.force {
        bail!("{} already exists (use --force to overwrite)", paths.config_file.display());
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
        "  next: {} then {} (optional) and {}",
        cyan("anipv scan"),
        cyan("anipv import-fish --dry-run"),
        cyan("anipv")
    )?;
    Ok(())
}

/// The library a command works on, with the index and event log it was
/// built from, so rebuilding it after a write reads and classifies nothing
/// again.
struct Session {
    index: Index,
    log: CachedLog,
    lib: Library,
}

impl Session {
    /// Read the index and the event log and build the library (printing its
    /// warnings, like [`Ctx::library`]).
    #[expect(clippy::print_stderr, reason = "CLI warnings go to the terminal, not to command output")]
    fn load(ctx: &Ctx) -> Result<Self> {
        let mut index = ctx.index()?;
        let mut log = ctx.log.load_cached()?;
        let lib = ctx.migrated_library(&mut index, &mut log)?;
        for w in &lib.warnings {
            eprintln!("warning: {w}");
        }
        Ok(Self { index, log, lib })
    }

    /// Keep events just recorded, without reading the log again.
    fn add(&mut self, ctx: &Ctx, recorded: Recorded) {
        ctx.log.merge(&mut self.log, recorded);
    }

    /// Rebuild the library with the events recorded and metadata cached since
    /// (anything recorded but not [`Session::add`]ed is read from the log).
    fn rebuild(&mut self, ctx: &Ctx) -> Result<()> {
        ctx.log.refresh(&mut self.log)?;
        self.index.meta = ctx.meta_cache()?;
        self.lib = ctx.migrated_library(&mut self.index, &mut self.log)?;
        Ok(())
    }
}

/// Run a command that writes (watch state, statuses, merges, metadata) on a
/// [`Session`], then complete followed series that are now finished and fully
/// watched, and say so. Every writing command runs through here, so none
/// leaves a finished series uncompleted.
fn writing<W: Write>(ctx: &Ctx, out: &mut W, command: impl FnOnce(&mut Session, &mut W) -> Result<()>) -> Result<()> {
    let mut s = Session::load(ctx)?;
    command(&mut s, out)?;
    s.rebuild(ctx)?;
    for title in ctx.auto_complete(&s.lib)?.1 {
        writeln!(out, "{ok} {title} completed: all episodes watched", ok = green("✓"), title = bold(&title))?;
    }
    Ok(())
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
    // Each episode once; marking unwatched leaves alone (and counts apart)
    // episodes anipv knows nothing about, while marking watched records them.
    let unique: HashSet<&ItemKey> = items.iter().collect();
    let unknown = if unwatched { unique.iter().filter(|k| series.item(k).is_none()).count() } else { 0 };
    let already = unique.len() - changed - unknown;
    let notes: Vec<String> = [(already, "already were"), (unknown, "not known")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    if notes.is_empty() {
        writeln!(out)?;
    } else {
        writeln!(out, " ({})", notes.join(", "))?;
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
    writeln!(out, "{} {} is a separate series again", green("✓"), lib.current_key(from))?;
    s.add(ctx, recorded);
    Ok(())
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
                    red(&format!("  ({} unreadable dirs)", r.errors.len()))
                }
            )?,
            Err(e) => {
                failed += 1;
                writeln!(out, "{fail} {root:<12} {e:#}", fail = red("✗"), root = r.root)?;
            }
        }
    }
    // Any failed root fails the command, so scripts notice an unmounted drive.
    if failed > 0 {
        bail!("{failed} of {total} root(s) could not be scanned");
    }
    Ok(())
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
        "{}",
        bold(&format!("{s:<44} {d:>9} {a:>6}  {air:<18}", s = "SERIES", d = "ON DISK", a = "ADDED", air = "AIRING"))
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
            "{title} {:>9} {:>6}  {}  {hint}",
            s.disk_range(),
            ago_opt(s.last_added(), now),
            dim(&pad(&s.airing_text(now), 18)),
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
            "Nothing followed yet. Try {} or {}.",
            cyan("anipv status <series> following"),
            cyan("anipv import-fish")
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "{}",
        bold(&format!(
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
        let new = s.new_episodes().len();
        let active = ago_opt(s.last_activity(), now);
        let title = if s.status == SeriesStatus::Paused { yellow(&pad(&s.title, 40)) } else { pad(&s.title, 40) };
        writeln!(
            out,
            "{title} {next:>6} {new:>4} {progress:>9} {active:>8}  {airing}",
            new = if new > 0 { green(&format!("{new:>4}")) } else { dim(&format!("{new:>4}")) },
            progress = s.progress(),
            airing = dim(&s.airing_text(now))
        )?;
    }
    Ok(())
}

fn ls(ctx: &Ctx, wanted: &[SeriesStatus], all: bool, out: &mut impl Write) -> Result<()> {
    let lib = ctx.library()?;
    let now = now();
    writeln!(
        out,
        "{}",
        bold(&format!(
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
    if let Some(n) = &s.note {
        writeln!(out, "  {}", dim(n))?;
    }
    if !s.aliases.is_empty() {
        writeln!(out, "  {label} {names}", label = dim("also:"), names = s.aliases.join(", "))?;
    }
    if s.meta.as_ref().is_some_and(|m| m.anilist.is_some()) {
        let (label, progress, airing) = (dim("meta:"), s.progress(), dim(&s.airing_text(now)));
        writeln!(out, "  {label} watched {progress} · {airing}")?;
    }
    writeln!(out)?;
    let missing = s.missing_count(true);
    if missing > 0 && !all {
        writeln!(out, "  {}", dim(&format!("… {missing} more not on disk (--all lists them)")))?;
    }
    for it in s.ordered_items(true, all) {
        let kind = match it.key.kind {
            ItemKind::Episode | ItemKind::Unknown => String::new(),
            ItemKind::Special => magenta(" special"),
            ItemKind::Extra => dim(" extra"),
            ItemKind::Movie => cyan(" movie"),
        };
        let file = it.best_file().map_or_else(|| red("(not on disk)"), super::library::FileRef::name);
        let extra = match it.state {
            WatchState::Started { .. } => it.state.percent().map(|p| yellow(&format!(" {p}"))).unwrap_or_default(),
            WatchState::Watched { at } => dim(&format!(" {}", ago(at, now))),
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
            PlayerEvent::Ended { path, pos, dur, eof } => match ctx.record_playback(lib, &path, pos, dur, eof) {
                Err(e) => {
                    writeln!(out, "{fail} could not record {file}: {e:#}", fail = red("✗"), file = path.display())
                }
                Ok((series, items, o, recorded)) => {
                    ctx.log.merge(log, recorded);
                    let what = lib.label(&series, &items);
                    match o {
                        Outcome::Watched => {
                            writeln!(out, "  {} watched {what}", state_glyph(&WatchState::Watched { at: 0 }))
                        }
                        Outcome::Partial { pos, .. } => {
                            let (mark, at) =
                                (state_glyph(&WatchState::Started { pos, dur: None, at: 0 }), crate::fmt::clock(pos));
                            writeln!(out, "  {mark} stopped at {at} {what}")
                        }
                        Outcome::Nothing => writeln!(out, "  {} skipped {what}", state_glyph(&WatchState::Unwatched)),
                    }
                }
            },
            PlayerEvent::Error(e) => writeln!(out, "{} {e}", red("✗")),
            PlayerEvent::Exited { played, error } => {
                let res = match (played, error) {
                    (_, Some(e)) => writeln!(out, "{} {e}", red("✗")),
                    (false, None) => writeln!(out, "{} mpv exited before playing anything", red("✗")),
                    (true, None) => Ok(()),
                };
                written = written.and(res);
                break;
            }
            PlayerEvent::Progress { .. } => Ok(()),
        };
        written = written.and(res);
    }
    Ok(written?)
}

fn import_fish(ctx: &Ctx, s: &mut Session, a: ImportArgs, out: &mut impl Write) -> Result<()> {
    use crate::import_fish::{already_imported, default_history_path, plan, read_history};
    let path = a.history.unwrap_or_else(default_history_path);
    let plays = read_history(&path)?;
    let p = plan(&ctx.cfg, &s.lib, &plays, &already_imported(s.log.events()), now());
    let matched = p.watches.iter().filter(|w| w.matched).count();
    writeln!(
        out,
        "{} mpv commands → {} watched items ({} matched to files on disk, {} from file names only)",
        p.commands,
        bold(&p.watches.len().to_string()),
        green(&matched.to_string()),
        yellow(&(p.watches.len() - matched).to_string())
    )?;
    let mut per_series: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for w in &p.watches {
        *per_series.entry(&w.series).or_default() += 1;
    }
    writeln!(out, "{} series touched", per_series.len())?;
    if !p.ambiguous.is_empty() {
        let n = p.ambiguous.len();
        writeln!(out, "{} {n} ambiguous file name(s) skipped (several indexed files share the name)", yellow("!"))?;
        if a.dry_run {
            for name in &p.ambiguous {
                writeln!(out, "    {}", dim(name))?;
            }
        }
    }
    if !p.proposals.is_empty() {
        writeln!(out, "\n{}", bold("Suggested statuses (for untracked series):"))?;
        for pr in &p.proposals {
            let (status, title, last) = (status_cell(pr.status, 10), pad(&pr.title, 50), dim(&ago(pr.last, now())));
            writeln!(out, "  {status} {title}  {last}")?;
        }
    }
    if a.dry_run {
        writeln!(out, "\n{}", dim("dry run: nothing written"))?;
        return Ok(());
    }
    let events = p.events(ctx.log.device());
    let written = ctx.log.append_events(&events)?;
    s.add(ctx, Recorded { events, written });
    if a.apply_status {
        let recorded = ctx.record(
            p.proposals
                .iter()
                .map(|pr| EventBody::status(pr.series.clone(), pr.status, Some("from fish history".into()))),
        )?;
        s.add(ctx, recorded);
    }
    let also = if a.apply_status { " and applied statuses" } else { "" };
    writeln!(out, "\n{ok} imported{also}", ok = green("✓"))?;
    if !a.apply_status && !p.proposals.is_empty() {
        writeln!(out, "  re-run with {} to apply the suggestions", cyan("--apply-status"))?;
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
            "{}",
            yellow(&format!(
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
                "scanned {} · {} files · {}",
                match ago(s.finished_at, now()).as_str() {
                    "now" => "just now".to_string(),
                    t => format!("{t} ago"),
                },
                s.stats.seen,
                if s.complete { "complete" } else { "incomplete (some folders unreadable)" }
            ),
            None => "never scanned".into(),
        };
        writeln!(
            out,
            "  {} {:<12} {:?} {}  {}",
            if ok { green("✓") } else { red("✗") },
            r.name,
            r.kind,
            r.resolved().display(),
            dim(&last)
        )?;
    }
    let Session { log, lib, .. } = Session::load(ctx)?;
    writeln!(out, "{label} {n} events", label = bold("log:   "), n = log.events().len())?;
    for e in log.errors().iter().take(10) {
        writeln!(out, "  {} {e}", red("✗"))?;
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
        "\n{} {} files without an episode number (movies/one-offs or parser misses):",
        bold("parse:"),
        odd.len()
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
    serde_json::to_writer_pretty(&mut *out, &series)?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        command().debug_assert();
    }

    #[test]
    fn parses_common_invocations() {
        Cli::try_parse_from(["anipv"]).unwrap();
        Cli::try_parse_from(["anipv", "play", "one piece", "grand blue", "-n", "2", "--print"]).unwrap();
        Cli::try_parse_from(["anipv", "status", "x", "dropped", "--note", "meh"]).unwrap();
        Cli::try_parse_from(["anipv", "meta", "refresh"]).unwrap();
        Cli::try_parse_from(["anipv", "meta", "refresh", "frieren"]).unwrap();
        Cli::try_parse_from(["anipv", "meta", "refresh", "--all"]).unwrap();
        assert!(Cli::try_parse_from(["anipv", "meta", "refresh", "frieren", "--all"]).is_err());
        Cli::try_parse_from(["anipv", "init", "--ongoing", "~/dl", "--archive", "~/anime"]).unwrap();
        assert!(Cli::try_parse_from(["anipv", "status", "x", "bogus"]).is_err());
    }
}
