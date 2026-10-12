//! Launching mpv and following playback over its JSON IPC socket.
//!
//! anipv starts mpv with `--input-ipc-server=<socket>` and observes `path`,
//! `time-pos` and `duration` for each file between its `start-file` and
//! `end-file` events. mpv reports a property only when it changes, so each
//! file starts out with the last reported path and duration (the same file
//! again, or one of the same length, gets no report); the file mpv may
//! already be on when anipv connects is followed too. When a file ends (or
//! mpv quits) the furthest position decides whether it counts as watched.
//! See <https://mpv.io/manual/stable/#json-ipc>.
//!
//! The same connection carries key presses the other way (see [`Remote`]).

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::Config;

/// A file to play, optionally resuming at a position (seconds).
#[derive(Debug, Clone, PartialEq)]
pub struct PlayFile {
    /// Absolute path.
    pub path: PathBuf,
    /// Resume position.
    pub start: Option<f64>,
}

/// mpv command-line arguments for a playlist.
pub(crate) fn build_args(cfg: &Config, socket: Option<&Path>, files: &[PlayFile]) -> Vec<OsString> {
    let mut args: Vec<OsString> = cfg.mpv_args.iter().map(OsString::from).collect();
    if let Some(s) = socket {
        let mut a = OsString::from("--input-ipc-server=");
        a.push(s);
        args.push(a);
    }
    for f in files {
        match f.start {
            Some(pos) if pos > 1.0 => {
                args.push("--{".into());
                args.push(format!("--start={pos:.0}").into());
                args.push(f.path.clone().into());
                args.push("--}".into());
            }
            _ => args.push(f.path.clone().into()),
        }
    }
    args
}

/// Quote a word so fish, bash and zsh all read it back unchanged.
///
/// Single quotes are closed around `'` and `\` and those are escaped outside
/// the quotes (`'it'\''s'`), because the shells disagree on escapes inside them.
///
/// Works on bytes, so file names that aren't valid UTF-8 come through intact.
fn shell_quote(s: &OsStr) -> Vec<u8> {
    const SAFE: &[u8] = b"-_./=+,:@%";
    let s = s.as_encoded_bytes();
    if !s.is_empty() && s.iter().all(|b| b.is_ascii_alphanumeric() || SAFE.contains(b)) {
        return s.to_vec();
    }
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'\'');
    for &b in s {
        match b {
            b'\'' => out.extend_from_slice(b"'\\''"),
            b'\\' => out.extend_from_slice(b"'\\\\'"),
            b => out.push(b),
        }
    }
    out.push(b'\'');
    out
}

/// A copy-pasteable `mpv …` command for the playlist (no IPC socket).
///
/// Bytes rather than a `String`: file names need not be valid UTF-8.
pub fn command_line(cfg: &Config, files: &[PlayFile]) -> Vec<u8> {
    std::iter::once(shell_quote(cfg.mpv.as_ref()))
        .chain(build_args(cfg, None, files).iter().map(|a| shell_quote(a)))
        .collect::<Vec<_>>()
        .join(&b' ')
}

/// Spawn mpv detached from the terminal (its output would garble the TUI).
fn spawn(cfg: &Config, socket: &Path, files: &[PlayFile]) -> Result<Child> {
    let _ = std::fs::remove_file(socket);
    Command::new(&cfg.mpv)
        .args(build_args(cfg, Some(socket), files))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", cfg.mpv))
}

/// Spawn mpv and follow it from a background thread, passing every
/// [`PlayerEvent`] (ending with [`PlayerEvent::Exited`]) to `emit`.
pub fn spawn_tracked(
    cfg: &Config,
    socket: PathBuf,
    files: &[PlayFile],
    emit: impl Fn(PlayerEvent) + Send + 'static,
) -> Result<()> {
    let child = spawn(cfg, &socket, files)?;
    std::thread::spawn(move || track(&socket, child, &emit));
    Ok(())
}

/// How long a command may wait for mpv to take it: a player that stopped
/// reading its socket must not freeze the TUI sending it a key.
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// A handle for sending key presses to a running mpv: the writing end of the
/// IPC connection, shared with its follower. Each command is written whole
/// under one lock, so commands from the two threads never interleave.
#[derive(Clone)]
pub struct Remote(Arc<Mutex<Writer>>);

/// The writing end of the IPC connection.
struct Writer {
    stream: UnixStream,
    /// The last command failed (the write timeout cut it off): mpv may have
    /// part of a line.
    torn: bool,
}

impl Writer {
    /// Write `cmd` as one line. After a command that failed, the line it may
    /// have begun is ended first (mpv rejects what it got of it, and skips an
    /// empty line), so this one isn't appended to it and lost as well.
    fn send(&mut self, cmd: &Value) -> std::io::Result<()> {
        let line = format!("{end}{cmd}\n", end = if self.torn { "\n" } else { "" });
        self.torn = true;
        self.stream.write_all(line.as_bytes())?;
        self.torn = false;
        Ok(())
    }
}

impl Remote {
    /// A remote writing to `stream` (a clone of the followed connection;
    /// tests and screenshots pass any socket).
    #[doc(hidden)]
    pub fn new(stream: UnixStream) -> Self {
        // A timeout is all that can fail here, and without one sending still works.
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        Self(Arc::new(Mutex::new(Writer { stream, torn: false })))
    }

    /// Press (and release) the mpv key `name` (`SPACE`, `Ctrl+LEFT`, `q`, …),
    /// as if typed in the player: the user's `input.conf` bindings and
    /// scripts decide what it does.
    ///
    /// Fails after half a second when mpv takes no commands, and at once when
    /// it has closed the connection (see [`closed`]).
    pub fn keypress(&self, name: &str) -> std::io::Result<()> {
        self.send(&serde_json::json!({ "command": ["keypress", name] }))
    }

    /// Send one IPC command, whole, under the lock.
    fn send(&self, cmd: &Value) -> std::io::Result<()> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).send(cmd)
    }
}

/// Whether `e`, from sending mpv a command, means mpv closed the connection:
/// it is quitting, and the session's end is on its way.
pub fn closed(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, NotConnected};
    matches!(e.kind(), BrokenPipe | ConnectionReset | ConnectionAborted | NotConnected)
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Remote")
    }
}

impl PartialEq for Remote {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Something that happened in the player.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// Connected to mpv: keys can be sent to it from now on.
    Connected(Remote),
    /// Playback was paused (`true`) or resumed.
    Paused(bool),
    /// A file started playing (not just queued up: mpv reported a position).
    Started {
        /// File path as given to mpv.
        path: PathBuf,
    },
    /// Position update while playing (at most twice a second).
    Progress {
        /// File path.
        path: PathBuf,
        /// Current position (s); `Ended` reports the furthest one reached.
        pos: f64,
        /// Duration (s).
        dur: Option<f64>,
    },
    /// A file stopped playing.
    Ended {
        /// File path.
        path: PathBuf,
        /// Furthest position reached (s).
        pos: f64,
        /// Duration (s).
        dur: Option<f64>,
        /// Reached the end of the file.
        eof: bool,
    },
    /// The session is over: mpv exited (or the socket closed). Always the
    /// last event.
    Exited {
        /// At least one file started playing.
        played: bool,
        /// Why the session ended early, if it did (mpv quit before the IPC
        /// socket came up, or the socket failed).
        error: Option<String>,
    },
    /// A problem that did not end the session (e.g. one file failed to open).
    Error(String),
}

/// The last reported path and duration: mpv reports no change when the
/// same file plays again, or one of the same length, so each file starts
/// out with these.
#[derive(Default, Clone)]
struct Known {
    path: Option<PathBuf>,
    dur: Option<f64>,
}

impl Known {
    /// A `property-change` of `name` to `data`, as mpv has it now (`null`
    /// included: no file loaded). A duration mpv can't tell is reported as
    /// 0 (or nothing); `None` means "unknown" downstream.
    fn on_change(&mut self, name: Option<&str>, data: Option<&Value>) {
        match name {
            Some("path") => self.path = data.and_then(Value::as_str).map(PathBuf::from),
            Some("duration") => self.dur = data.and_then(Value::as_f64).filter(|d| *d > 0.0),
            _ => {}
        }
    }
}

/// The file mpv is on: opened by `start-file`, closed by the matching
/// `end-file` (both carry the playlist entry id).
#[derive(Default)]
struct Current {
    /// Playlist entry id from `start-file`.
    entry: Option<i64>,
    /// Path and duration: the last known ones when the file opened, then
    /// each reported value. A file keeps them when mpv reports none (the
    /// file unloading) before its `end-file`.
    path: Option<PathBuf>,
    dur: Option<f64>,
    /// Playback began (mpv reported a position). A file that fails to open
    /// never gets this far.
    started: bool,
    /// Furthest position reached: seeking back doesn't undo progress.
    max_pos: f64,
    /// When the last `Progress` for this file was sent (throttled to two a
    /// second); `None` = none yet, so each file's first position goes out.
    last_sent: Option<Instant>,
}

impl Current {
    /// A file opened with what is known now.
    fn open(entry: Option<i64>, known: &Known) -> Self {
        Self { entry, path: known.path.clone(), dur: known.dur, ..Self::default() }
    }

    /// A `property-change` of the observed property `name` to `data`
    /// (already applied to `known`).
    fn on_change(&mut self, name: Option<&str>, data: Option<&Value>, known: &Known, emit: &dyn Fn(PlayerEvent)) {
        match name {
            Some("path") if known.path.is_some() => self.path.clone_from(&known.path),
            Some("duration") if known.dur.is_some() => self.dur = known.dur,
            Some("time-pos") => {
                if let Some(pos) = data.and_then(Value::as_f64)
                    && let Some(path) = &self.path
                {
                    self.max_pos = self.max_pos.max(pos);
                    if !self.started {
                        self.started = true;
                        emit(PlayerEvent::Started { path: path.clone() });
                    }
                    if self.last_sent.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
                        self.last_sent = Some(Instant::now());
                        emit(PlayerEvent::Progress { path: path.clone(), pos, dur: self.dur });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Close the open file, if any, emitting its `Ended` event if it started playing.
fn close(cur: &mut Option<Current>, eof: bool, emit: &dyn Fn(PlayerEvent)) {
    if let Some(c) = cur.take()
        && c.started
        && let Some(path) = c.path
    {
        emit(PlayerEvent::Ended { path, pos: c.max_pos, dur: c.dur, eof });
    }
}

/// Read mpv IPC messages from `reader` until it closes, emitting events;
/// commands go out through `remote`.
///
/// Generic over the transport so tests can feed canned JSON.
pub(crate) fn follow(reader: impl std::io::Read, remote: &Remote, emit: &dyn Fn(PlayerEvent)) -> Result<()> {
    let mut reader = BufReader::new(reader);
    for (id, prop) in [(1, "path"), (2, "time-pos"), (3, "duration"), (4, "pause")] {
        remote.send(&serde_json::json!({ "command": ["observe_property", id, prop] }))?;
    }

    let mut known = Known::default();
    // The open file, if any. Property updates outside `start-file`..`end-file`
    // (a late position after a file ended) belong to no file and are dropped.
    // mpv may have started the first file before we connected (its
    // `start-file` is gone), so one is open from the start: it is whatever
    // mpv reports on observing. If that is nothing yet, it never starts and
    // the first `start-file` closes it unseen.
    let mut cur = Some(Current::default());
    // Raw bytes: mpv passes file names that aren't valid UTF-8 through as-is,
    // and such a line must cost only itself, not end the whole session.
    let mut line = Vec::new();
    // A read error (mpv crashed: connection reset) ends the stream like EOF,
    // so the open file still ends below; the error is reported after that.
    let read_error = loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break None,
            Ok(_) => {}
            Err(e) => break Some(e),
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&line) else { continue };
        let entry = msg.get("playlist_entry_id").and_then(Value::as_i64);
        match msg.get("event").and_then(Value::as_str) {
            Some("start-file") => {
                // A missed `end-file` still closes the previous file.
                close(&mut cur, false, emit);
                cur = Some(Current::open(entry, &known));
            }
            Some("end-file") => {
                // Only the open file's own `end-file` closes it (older mpv
                // versions send no id: trust those).
                if cur.as_ref().is_some_and(|c| entry.is_none() || c.entry.is_none() || c.entry == entry) {
                    close(&mut cur, msg.get("reason").and_then(Value::as_str) == Some("eof"), emit);
                }
                if let Some(e) = msg.get("file_error").and_then(Value::as_str) {
                    emit(PlayerEvent::Error(format!("mpv could not play a file: {e}")));
                }
            }
            Some("property-change") => {
                let (name, data) = (msg.get("name").and_then(Value::as_str), msg.get("data"));
                // Pausing is the player's, not a file's: reported between files too.
                if name == Some("pause") {
                    if let Some(p) = data.and_then(Value::as_bool) {
                        emit(PlayerEvent::Paused(p));
                    }
                } else {
                    known.on_change(name, data);
                    if let Some(c) = &mut cur {
                        c.on_change(name, data, &known, emit);
                    }
                }
            }
            _ => {}
        }
    };
    close(&mut cur, false, emit);
    match read_error {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

/// Connect to `socket` (waiting for mpv to create it), follow playback and
/// report `Exited` (with the reason, if the session failed) when done. Runs
/// until mpv quits; call from a thread.
fn track(socket: &Path, mut child: Child, emit: &dyn Fn(PlayerEvent)) {
    let played = std::cell::Cell::new(false);
    let emit_seen = |ev: PlayerEvent| {
        played.set(played.get() || matches!(ev, PlayerEvent::Started { .. }));
        emit(ev);
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let error = loop {
        match UnixStream::connect(socket) {
            Ok(s) => {
                let res = s.try_clone().map_err(anyhow::Error::from).and_then(|w| {
                    let remote = Remote::new(w);
                    emit_seen(PlayerEvent::Connected(remote.clone()));
                    follow(s, &remote, &emit_seen)
                });
                break res.err().map(|e| format!("mpv IPC: {e}"));
            }
            Err(_) if Instant::now() < deadline => {
                if let Ok(Some(status)) = child.try_wait() {
                    // Bad arguments or no playable file: say so rather than end silently.
                    break Some(format!("mpv exited before playing anything ({status})"));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => break Some(format!("mpv IPC: {e}")),
        }
    };
    let _ = child.wait();
    let _ = std::fs::remove_file(socket);
    emit(PlayerEvent::Exited { played: played.get(), error });
}

/// A temporary directory for a test's IPC socket: in the temporary directory,
/// or in `/tmp` when a socket path there would be too long (a deep `TMPDIR`).
#[cfg(test)]
pub(crate) fn socket_test_dir() -> tempfile::TempDir {
    // Named like `tempdir`'s own.
    let base = crate::app::socket_temp_dir(Path::new(".tmpXXXXXX/sock"));
    tempfile::tempdir_in(base).expect("a temporary directory")
}

/// What an ended file means for the watch log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// Count as watched.
    Watched,
    /// Remember the position.
    Partial {
        /// Position (s).
        pos: f64,
        /// Duration (s).
        dur: Option<f64>,
    },
    /// Barely started; ignore.
    Nothing,
}

/// How far (s) short of the duration playback may stop and still count as
/// played to the end (see [`outcome`]).
const END_SLACK: f64 = 5.0;

/// Decide what an `Ended` event means given the watched threshold.
///
/// Reaching the end of the file only counts when playback got near the
/// episode's duration: a file that is still downloading also "ends", after
/// however many minutes are on disk. Without a known duration, the end is
/// trusted. `dur` is a positive duration or `None` (the IPC follower drops zeros).
///
/// The last position mpv reports is the last frame's, a little short of the
/// duration, so stopping within five seconds of the end counts whatever the
/// threshold (a threshold of `1.0` could never be reached otherwise), also
/// without an end of file: with `keep-open`, mpv stays on the last frame and
/// the file ends when mpv is quit.
pub fn outcome(pos: f64, dur: Option<f64>, eof: bool, threshold: f64) -> Outcome {
    match dur {
        Some(d) if pos / d >= threshold || d - pos <= END_SLACK => Outcome::Watched,
        None if eof => Outcome::Watched,
        _ if pos >= 30.0 => Outcome::Partial { pos, dur },
        _ => Outcome::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn pf(p: &str, start: Option<f64>) -> PlayFile {
        PlayFile { path: p.into(), start }
    }

    #[test]
    fn args_and_resume() {
        let cfg = Config { mpv_args: vec!["--fs".into()], ..Config::default() };
        let args = build_args(&cfg, Some(Path::new("/tmp/s")), &[pf("/a.mkv", Some(612.4)), pf("/b.mkv", None)]);
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec!["--fs", "--input-ipc-server=/tmp/s", "--{", "--start=612", "/a.mkv", "--}", "/b.mkv"]);
    }

    fn quote(s: &str) -> String {
        String::from_utf8(shell_quote(s.as_ref())).unwrap()
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("plain.mkv"), "plain.mkv");
        assert_eq!(quote("[GroupA] A - 01.mkv"), "'[GroupA] A - 01.mkv'");
        assert_eq!(quote("JoJo's"), r"'JoJo'\''s'");
        assert_eq!(quote(r"a\b"), r"'a'\\'b'");
        let cfg = Config::default();
        assert_eq!(command_line(&cfg, &[pf("/x/a b.mkv", None)]), b"mpv '/x/a b.mkv'");
    }

    /// A file name that isn't UTF-8 is copied byte for byte, not replaced by `�`.
    #[test]
    fn command_line_keeps_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt as _;
        let name = OsStr::from_bytes(b"/x/caf\xe9 01.mkv");
        let files = [PlayFile { path: name.into(), start: None }];
        assert_eq!(command_line(&Config::default(), &files), b"mpv '/x/caf\xe9 01.mkv'");
    }

    /// Round-trip awkward names through every shell that is installed.
    #[test]
    fn quoting_round_trips_in_real_shells() {
        use std::os::unix::ffi::OsStrExt as _;
        let words: [&[u8]; 4] =
            [b"JoJo's Bizarre - 01.mkv", br"back\slash 'and' quotes.mkv", b"$HOME * ? [x] `c` ~a", b"caf\xe9.mkv"];
        for shell in ["bash", "zsh", "fish", "sh"] {
            for w in words {
                let script = [b"printf %s ".as_slice(), &shell_quote(OsStr::from_bytes(w))].concat();
                let Ok(out) = Command::new(shell).arg("-c").arg(OsStr::from_bytes(&script)).output() else { continue };
                let (got, script) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&script));
                assert_eq!(out.stdout, w, "{shell} read back {got:?} from {script}");
            }
        }
    }

    #[test]
    fn outcomes() {
        // The end of a file whose duration is known counts only if we got there.
        assert_eq!(outcome(1439.0, Some(1440.0), true, 0.85), Outcome::Watched);
        // A partly downloaded file: mpv hits its end after 8 of 24 minutes.
        assert_eq!(outcome(480.0, Some(1440.0), true, 0.85), Outcome::Partial { pos: 480.0, dur: Some(1440.0) });
        assert_eq!(outcome(10.0, None, true, 0.85), Outcome::Watched, "no duration: trust the end");
        assert_eq!(outcome(1300.0, Some(1440.0), false, 0.85), Outcome::Watched);
        assert_eq!(outcome(600.0, Some(1440.0), false, 0.85), Outcome::Partial { pos: 600.0, dur: Some(1440.0) });
        assert_eq!(outcome(5.0, Some(1440.0), false, 0.85), Outcome::Nothing);
        assert_eq!(outcome(100.0, None, false, 0.85), Outcome::Partial { pos: 100.0, dur: None });
        // The last frame is a little short of the duration: a threshold of 1 is still reachable.
        assert_eq!(outcome(1439.96, Some(1440.02), true, 1.0), Outcome::Watched);
        // `keep-open`: quit on the last frame, no end of file.
        assert_eq!(outcome(1439.96, Some(1440.02), false, 1.0), Outcome::Watched);
        assert_eq!(outcome(1430.0, Some(1440.02), false, 1.0), Outcome::Partial { pos: 1430.0, dur: Some(1440.02) });
    }

    /// Feed `lines` to [`follow`] as mpv would and collect everything it
    /// emits. All it ever sends mpv are the `observe_property` commands.
    fn feed(lines: &[&[u8]]) -> Vec<PlayerEvent> {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let remote = Remote::new(ours.try_clone().unwrap());
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || follow(ours, &remote, &|e| tx.send(e).unwrap()).unwrap());
        let cmds = BufReader::new(theirs.try_clone().unwrap());
        for l in lines {
            theirs.write_all(&[l, b"\n".as_slice()].concat()).unwrap();
        }
        theirs.shutdown(std::net::Shutdown::Write).unwrap();
        handle.join().unwrap();
        let sent: Vec<Value> = cmds.lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
        let observed: Vec<Value> = [(1, "path"), (2, "time-pos"), (3, "duration"), (4, "pause")]
            .map(|(id, prop)| serde_json::json!({ "command": ["observe_property", id, prop] }))
            .into();
        assert_eq!(sent, observed);
        rx.try_iter().collect()
    }

    /// [`feed`] for JSON lines, without `Progress`.
    fn run_follow(msgs: &[&str]) -> Vec<PlayerEvent> {
        let lines: Vec<&[u8]> = msgs.iter().map(|m| m.as_bytes()).collect();
        feed(&lines).into_iter().filter(|e| !matches!(e, PlayerEvent::Progress { .. })).collect()
    }

    /// mpv may start the first file before we connect: its `start-file` is
    /// never seen, but the values reported on observing name it, and it
    /// still ends.
    #[test]
    fn follow_tracks_a_file_already_playing_at_connect() {
        let evs = run_follow(&[
            r#"{"event":"property-change","id":1,"name":"path","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":3,"name":"duration","data":184.2}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":0.4}"#,
            r#"{"event":"property-change","id":4,"name":"pause","data":false}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":184.1}"#,
            r#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
            r#"{"event":"start-file","playlist_entry_id":2}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/b.mkv"}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":700.0}"#,
            r#"{"event":"property-change","id":4,"name":"pause","data":true}"#,
        ]);
        assert_eq!(
            evs,
            vec![
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Paused(false),
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 184.1, dur: Some(184.2), eof: true },
                PlayerEvent::Started { path: "/b.mkv".into() },
                PlayerEvent::Paused(true),
                // No duration reported for it: the same as the one before.
                PlayerEvent::Ended { path: "/b.mkv".into(), pos: 700.0, dur: Some(184.2), eof: false },
            ]
        );
    }

    /// Connected before mpv opened anything: the speculative file never
    /// starts and ends unseen when the first real one comes.
    #[test]
    fn follow_connected_before_any_file() {
        let evs = run_follow(&[
            r#"{"event":"property-change","id":1,"name":"path"}"#,
            r#"{"event":"start-file","playlist_entry_id":1}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":1439.0}"#,
            r#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
        ]);
        assert_eq!(
            evs,
            vec![
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 1439.0, dur: None, eof: true },
            ]
        );
    }

    /// mpv reports nothing again that didn't change: the same file twice in
    /// a row (`--loop-playlist`) or two of the same length keep the last
    /// reported path and duration. A file unloading (`null`) before its
    /// `end-file` keeps its own.
    #[test]
    fn follow_carries_over_what_mpv_does_not_report_again() {
        let evs = run_follow(&[
            r#"{"event":"start-file","playlist_entry_id":1}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":3,"name":"duration","data":184.2}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":184.1}"#,
            r#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
            r#"{"event":"start-file","playlist_entry_id":2}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":600.0}"#,
            r#"{"event":"property-change","id":1,"name":"path"}"#,
            r#"{"event":"property-change","id":3,"name":"duration"}"#,
            r#"{"event":"end-file","reason":"quit","playlist_entry_id":2}"#,
            r#"{"event":"start-file","playlist_entry_id":3}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/b.mkv"}"#,
            r#"{"event":"property-change","id":3,"name":"duration","data":1440.0}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":100.0}"#,
            r#"{"event":"end-file","reason":"quit","playlist_entry_id":3}"#,
            r#"{"event":"start-file","playlist_entry_id":4}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/c.mkv"}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":50.0}"#,
        ]);
        assert_eq!(
            evs,
            vec![
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 184.1, dur: Some(184.2), eof: true },
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 600.0, dur: Some(184.2), eof: false },
                PlayerEvent::Started { path: "/b.mkv".into() },
                PlayerEvent::Ended { path: "/b.mkv".into(), pos: 100.0, dur: Some(1440.0), eof: false },
                PlayerEvent::Started { path: "/c.mkv".into() },
                PlayerEvent::Ended { path: "/c.mkv".into(), pos: 50.0, dur: Some(1440.0), eof: false },
            ]
        );
    }

    #[test]
    fn follow_parses_ipc_stream() {
        let evs = run_follow(&[
            r#"{"request_id":0,"error":"success"}"#,
            r#"{"event":"start-file","playlist_entry_id":1}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":3,"name":"duration","data":1440.0}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":1439.0}"#,
            r#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
            // A late position update between files counts for neither.
            r#"{"event":"property-change","id":2,"name":"time-pos","data":1439.5}"#,
            r#"{"event":"start-file","playlist_entry_id":2}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/b.mkv"}"#,
            r#"{"event":"property-change","id":3,"name":"duration","data":1400.0}"#,
            // mpv's "can't tell" zero is not a duration: the known one stays.
            r#"{"event":"property-change","id":3,"name":"duration","data":0.0}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":700.0}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":null}"#,
            // Another entry's `end-file` doesn't close this one.
            r#"{"event":"end-file","reason":"stop","playlist_entry_id":9}"#,
            r#"{"event":"end-file","reason":"quit","playlist_entry_id":2}"#,
            // A file that fails to open: named, never played; the reason is
            // reported but the session goes on.
            r#"{"event":"start-file","playlist_entry_id":3}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/broken.mkv"}"#,
            r#"{"event":"end-file","reason":"error","playlist_entry_id":3,"file_error":"unrecognized file format"}"#,
            "garbage that is not json",
        ]);
        assert_eq!(
            evs,
            vec![
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 1439.0, dur: Some(1440.0), eof: true },
                PlayerEvent::Started { path: "/b.mkv".into() },
                PlayerEvent::Ended { path: "/b.mkv".into(), pos: 700.0, dur: Some(1400.0), eof: false },
                PlayerEvent::Error("mpv could not play a file: unrecognized file format".into()),
            ]
        );
    }

    /// mpv quitting before its socket comes up ends the session with the
    /// reason in `Exited`, reported once.
    #[test]
    fn early_exit_reason_is_in_exited() {
        let dir = socket_test_dir();
        let child = Command::new("false").spawn().unwrap();
        let evs = std::cell::RefCell::new(Vec::new());
        track(&dir.path().join("sock"), child, &|e| evs.borrow_mut().push(e));
        let evs = evs.into_inner();
        assert!(
            matches!(&evs[..], [PlayerEvent::Exited { played: false, error: Some(e) }]
                if e.starts_with("mpv exited before playing anything")),
            "{evs:?}"
        );
    }

    /// A line that isn't valid UTF-8 (a non-UTF-8 file name, passed through
    /// by mpv) is skipped; following goes on with the next file.
    #[test]
    fn follow_survives_lines_that_are_not_utf8() {
        let evs = feed(&[
            br#"{"event":"start-file","playlist_entry_id":1}"#,
            b"{\"event\":\"property-change\",\"id\":1,\"name\":\"path\",\"data\":\"/caf\xe9.mkv\"}",
            br#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
            br#"{"event":"start-file","playlist_entry_id":2}"#,
            br#"{"event":"property-change","id":1,"name":"path","data":"/b.mkv"}"#,
            br#"{"event":"property-change","id":2,"name":"time-pos","data":600.0}"#,
        ]);
        let evs: Vec<PlayerEvent> = evs.into_iter().filter(|e| !matches!(e, PlayerEvent::Progress { .. })).collect();
        assert_eq!(
            evs,
            vec![
                PlayerEvent::Started { path: "/b.mkv".into() },
                PlayerEvent::Ended { path: "/b.mkv".into(), pos: 600.0, dur: None, eof: false },
            ]
        );
    }

    /// The duration may be reported before the path; it still belongs to the
    /// file `start-file` opened, and a file still open when mpv goes away ends.
    #[test]
    fn follow_keeps_a_duration_reported_before_the_path() {
        let evs = run_follow(&[
            r#"{"event":"start-file","playlist_entry_id":1}"#,
            r#"{"event":"property-change","id":3,"name":"duration","data":1440.0}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":300.0}"#,
        ]);
        assert_eq!(
            evs,
            vec![
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 300.0, dur: Some(1440.0), eof: false },
            ]
        );
    }

    /// The real mpv (skipped when `mpv` or `ffmpeg` is missing): a queue of
    /// three short clips, the same file twice at the end, counts every clip,
    /// also when anipv connects only after the first one started (its
    /// `start-file` is gone by then).
    #[test]
    fn real_mpv_queue_records_every_file() {
        let have = |cmd: &str, flag| Command::new(cmd).arg(flag).output().is_ok_and(|o| o.status.success());
        if !have("ffmpeg", "-version") || !have("mpv", "--version") {
            return; // CI installs both, so this runs there.
        }
        let dir = socket_test_dir();
        // The first outlasts the late connect below; the second plays twice
        // (no path or duration change for mpv to report).
        let makers: Vec<(PathBuf, std::process::Child)> = [("a", 1.2), ("b", 0.5)]
            .iter()
            .map(|(name, secs)| {
                let path = dir.path().join(format!("{name}.mkv"));
                let src = format!("testsrc=duration={secs}:size=64x48:rate=10");
                let child =
                    Command::new("ffmpeg").args(["-v", "error", "-f", "lavfi", "-i", &src]).arg(&path).spawn().unwrap();
                (path, child)
            })
            .collect();
        let mut clips = Vec::new();
        for (path, mut child) in makers {
            assert!(child.wait().unwrap().success(), "ffmpeg could not make {}", path.display());
            clips.push(path);
        }
        clips.push(clips[1].clone());
        let files: Vec<PlayFile> = clips.iter().map(|c| PlayFile { path: c.clone(), start: None }).collect();
        let cfg = Config {
            mpv_args: ["--no-config", "--vo=null", "--ao=null", "--no-terminal"].map(String::from).to_vec(),
            ..Config::default()
        };
        for late in [Duration::ZERO, Duration::from_millis(700)] {
            let socket = dir.path().join("sock");
            let child = spawn(&cfg, &socket, &files).unwrap();
            std::thread::sleep(late);
            let evs = std::cell::RefCell::new(Vec::new());
            track(&socket, child, &|e| evs.borrow_mut().push(e));
            let ended: Vec<(PathBuf, Outcome)> = evs
                .borrow()
                .iter()
                .filter_map(|e| match e {
                    PlayerEvent::Ended { path, pos, dur, eof } => Some((path.clone(), outcome(*pos, *dur, *eof, 0.85))),
                    _ => None,
                })
                .collect();
            let want: Vec<(PathBuf, Outcome)> = clips.iter().map(|c| (c.clone(), Outcome::Watched)).collect();
            assert_eq!(ended, want, "connecting {late:?} late: {:?}", evs.borrow());
            assert!(
                matches!(evs.borrow().last(), Some(PlayerEvent::Exited { played: true, error: None })),
                "{:?}",
                evs.borrow()
            );
        }
    }

    /// A transport that yields canned bytes, then fails (mpv crashed:
    /// connection reset).
    struct Failing(std::io::Cursor<Vec<u8>>);

    impl std::io::Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.read(buf)? {
                0 => Err(std::io::ErrorKind::ConnectionReset.into()),
                n => Ok(n),
            }
        }
    }

    /// A read error mid-stream still ends the open file (keeping its
    /// progress) before the error is reported.
    #[test]
    fn follow_ends_the_open_file_on_a_read_error() {
        let msgs = [
            r#"{"event":"start-file","playlist_entry_id":1}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":3,"name":"duration","data":1440.0}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":900.0}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":800.0}"#,
        ];
        let data = (msgs.join("\n") + "\n").into_bytes();
        let evs = std::cell::RefCell::new(Vec::new());
        let (ours, _mpv) = UnixStream::pair().unwrap();
        let res = follow(Failing(std::io::Cursor::new(data)), &Remote::new(ours), &|e| evs.borrow_mut().push(e));
        let err = res.expect_err("the read error is reported");
        assert!(
            err.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset),
            "{err:?}"
        );
        let evs: Vec<_> = evs.into_inner().into_iter().filter(|e| !matches!(e, PlayerEvent::Progress { .. })).collect();
        assert_eq!(
            evs,
            vec![
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 900.0, dur: Some(1440.0), eof: false },
            ]
        );
    }

    /// The Progress throttle is per file: the first position of the next
    /// file goes out even right after the previous file's.
    #[test]
    fn follow_sends_each_files_first_progress() {
        let evs = feed(&[
            br#"{"event":"start-file","playlist_entry_id":1}"#,
            br#"{"event":"property-change","id":1,"name":"path","data":"/a.mkv"}"#,
            br#"{"event":"property-change","id":2,"name":"time-pos","data":1439.0}"#,
            br#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
            br#"{"event":"start-file","playlist_entry_id":2}"#,
            br#"{"event":"property-change","id":1,"name":"path","data":"/b.mkv"}"#,
            br#"{"event":"property-change","id":2,"name":"time-pos","data":0.5}"#,
        ]);
        let progress: Vec<(PathBuf, f64)> = evs
            .into_iter()
            .filter_map(|e| match e {
                PlayerEvent::Progress { path, pos, .. } => Some((path, pos)),
                _ => None,
            })
            .collect();
        assert_eq!(progress, vec![("/a.mkv".into(), 1439.0), ("/b.mkv".into(), 0.5)]);
    }

    /// A command the write timeout cut off leaves part of a line with mpv:
    /// the next command goes on a line of its own, not appended to that one.
    #[test]
    fn a_command_cut_off_by_the_timeout_spoils_only_itself() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let remote = Remote::new(ours.try_clone().unwrap());
        // Sooner than mpv is given (the timeout is the socket's, not the handle's).
        ours.set_write_timeout(Some(Duration::from_millis(50))).unwrap();
        drop(ours);
        // More than the socket holds while mpv isn't reading: only part of it goes.
        let huge = "x".repeat(4 << 20);
        let err = remote.keypress(&huge).expect_err("cut off");
        assert!(!closed(&err), "{err:?}");
        // mpv reads again.
        let reader = std::thread::spawn(move || {
            BufReader::new(theirs).split(b'\n').map(Result::unwrap).collect::<Vec<Vec<u8>>>()
        });
        remote.keypress("SPACE").unwrap();
        drop(remote);
        let lines = reader.join().unwrap();
        let [cut, next] = &lines[..] else { panic!("{} lines", lines.len()) };
        assert!(cut.len() < huge.len() && serde_json::from_slice::<Value>(cut).is_err());
        assert_eq!(
            serde_json::from_slice::<Value>(next).unwrap(),
            serde_json::json!({ "command": ["keypress", "SPACE"] })
        );
    }

    /// Writing to an mpv that quit fails at once, and says so.
    #[test]
    fn sending_to_an_mpv_that_quit_fails_as_closed() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        drop(theirs);
        let err = Remote::new(ours).keypress("q").expect_err("nobody is there");
        assert!(closed(&err), "{err:?}");
    }
}
