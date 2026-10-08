//! Launching mpv and following playback over its JSON IPC socket.
//!
//! anipv starts mpv with `--input-ipc-server=<socket>` and observes `path`,
//! `time-pos` and `duration` for each file between its `start-file` and
//! `end-file` events (asking for `path` at each `start-file`, since mpv
//! does not report an unchanged value again). When a file ends (or mpv quits) the furthest position
//! decides whether it counts as watched. See
//! <https://mpv.io/manual/stable/#json-ipc>.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
pub fn build_args(cfg: &Config, socket: Option<&Path>, files: &[PlayFile]) -> Vec<OsString> {
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
pub fn shell_quote(s: &OsStr) -> Vec<u8> {
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
pub fn spawn(cfg: &Config, socket: &Path, files: &[PlayFile]) -> Result<Child> {
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

/// Something that happened in the player.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
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

/// The file mpv is on: opened by `start-file`, closed by the matching
/// `end-file` (both carry the playlist entry id).
#[derive(Default)]
struct Current {
    /// Playlist entry id from `start-file`.
    entry: Option<i64>,
    /// Request id of the `get_property path` sent at `start-file`.
    path_request: i64,
    /// From the reply to that request or a `path` property change. mpv
    /// reports no change when the same file plays again, so both are needed.
    path: Option<PathBuf>,
    /// Playback began (mpv reported a position). A file that fails to open
    /// never gets this far.
    started: bool,
    /// Furthest position reached: seeking back doesn't undo progress.
    max_pos: f64,
    dur: Option<f64>,
}

impl Current {
    /// The `Ended` event for this file, if it started playing.
    fn finish(self, eof: bool) -> Option<PlayerEvent> {
        let path = self.path.filter(|_| self.started)?;
        Some(PlayerEvent::Ended { path, pos: self.max_pos, dur: self.dur, eof })
    }
}

/// Read mpv IPC messages from `stream` until it closes, emitting events.
///
/// Generic over the transport so tests can feed canned JSON.
pub fn follow<S: std::io::Read + Write>(stream: S, emit: &dyn Fn(PlayerEvent)) -> Result<()> {
    let mut reader = BufReader::new(stream);
    for (id, prop) in [(1, "path"), (2, "time-pos"), (3, "duration")] {
        let cmd = serde_json::json!({ "command": ["observe_property", id, prop] });
        writeln!(reader.get_mut(), "{cmd}")?;
    }
    reader.get_mut().flush()?;

    // The open file, if any. Property updates outside `start-file`..`end-file`
    // (a late position after a file ended) belong to no file and are dropped.
    let mut cur: Option<Current> = None;
    // Id of the last `get_property path` request (observe replies carry 0).
    let mut requests = 0;
    // Progress is throttled to two updates a second; `None` = nothing sent yet.
    let mut last_sent: Option<Instant> = None;
    // Raw bytes: mpv passes file names that aren't valid UTF-8 through as-is,
    // and such a line must cost only itself, not end the whole session.
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&line) else { continue };
        let entry = msg.get("playlist_entry_id").and_then(Value::as_i64);
        match msg.get("event").and_then(Value::as_str) {
            Some("start-file") => {
                // A missed `end-file` still closes the previous file.
                if let Some(ev) = cur.take().and_then(|c| c.finish(false)) {
                    emit(ev);
                }
                requests += 1;
                let cmd = serde_json::json!({ "command": ["get_property", "path"], "request_id": requests });
                // A closed socket shows up as the end of the stream below.
                let _ = writeln!(reader.get_mut(), "{cmd}").and_then(|()| reader.get_mut().flush());
                cur = Some(Current { entry, path_request: requests, ..Current::default() });
            }
            Some("end-file") => {
                // Only the open file's own `end-file` closes it (older mpv
                // versions send no id: trust those).
                if cur.as_ref().is_some_and(|c| entry.is_none() || c.entry.is_none() || c.entry == entry) {
                    let eof = msg.get("reason").and_then(Value::as_str) == Some("eof");
                    if let Some(ev) = cur.take().and_then(|c| c.finish(eof)) {
                        emit(ev);
                    }
                }
                if let Some(e) = msg.get("file_error").and_then(Value::as_str) {
                    emit(PlayerEvent::Error(format!("mpv could not play a file: {e}")));
                }
            }
            Some("property-change") => {
                let Some(c) = &mut cur else { continue };
                let data = msg.get("data");
                match msg.get("name").and_then(Value::as_str) {
                    Some("path") => {
                        if let Some(p) = data.and_then(Value::as_str) {
                            c.path = Some(PathBuf::from(p));
                        }
                    }
                    Some("time-pos") => {
                        if let Some(pos) = data.and_then(Value::as_f64)
                            && let Some(path) = &c.path
                        {
                            c.max_pos = c.max_pos.max(pos);
                            if !c.started {
                                c.started = true;
                                emit(PlayerEvent::Started { path: path.clone() });
                            }
                            if last_sent.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
                                last_sent = Some(Instant::now());
                                emit(PlayerEvent::Progress { path: path.clone(), pos, dur: c.dur });
                            }
                        }
                    }
                    Some("duration") => {
                        // mpv reports 0 (or nothing) when it can't tell; keep
                        // only usable durations so `None` means "unknown" downstream.
                        if let Some(d) = data.and_then(Value::as_f64).filter(|d| *d > 0.0) {
                            c.dur = Some(d);
                        }
                    }
                    _ => {}
                }
            }
            // A reply to a command: the path asked for at `start-file`.
            None => {
                if let Some(c) = &mut cur
                    && msg.get("request_id").and_then(Value::as_i64) == Some(c.path_request)
                    && let Some(p) = msg.get("data").and_then(Value::as_str)
                {
                    c.path = Some(PathBuf::from(p));
                }
            }
            _ => {}
        }
    }
    if let Some(ev) = cur.and_then(|c| c.finish(false)) {
        emit(ev);
    }
    Ok(())
}

/// Connect to `socket` (waiting for mpv to create it), follow playback and
/// report `Exited` (with the reason, if the session failed) when done. Runs
/// until mpv quits; call from a thread.
pub fn track(socket: &Path, mut child: Child, emit: &dyn Fn(PlayerEvent)) {
    let played = std::cell::Cell::new(false);
    let emit_seen = |ev: PlayerEvent| {
        played.set(played.get() || matches!(ev, PlayerEvent::Started { .. }));
        emit(ev);
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let error = loop {
        match UnixStream::connect(socket) {
            Ok(s) => break follow(s, &emit_seen).err().map(|e| format!("mpv IPC: {e}")),
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

/// How far (s) short of the duration a file that reached its end may stop
/// and still count as played to the end (see [`outcome`]).
pub const EOF_SLACK: f64 = 5.0;

/// Decide what an `Ended` event means given the watched threshold.
///
/// Reaching the end of the file only counts when playback got near the
/// episode's duration: a file that is still downloading also "ends", after
/// however many minutes are on disk. Without a known duration, the end is
/// trusted. `dur` is a positive duration or `None` ([`follow`] drops zeros).
///
/// The last position mpv reports is the last frame's, a little short of the
/// duration, so reaching the end within [`EOF_SLACK`] counts whatever the
/// threshold (a threshold of `1.0` could never be reached otherwise).
pub fn outcome(pos: f64, dur: Option<f64>, eof: bool, threshold: f64) -> Outcome {
    match dur {
        Some(d) if pos / d >= threshold || (eof && d - pos <= EOF_SLACK) => Outcome::Watched,
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
        assert_eq!(outcome(1439.96, Some(1440.02), false, 1.0), Outcome::Partial { pos: 1439.96, dur: Some(1440.02) });
    }

    /// Feed `msgs` to [`follow`] as mpv would and collect what it emits
    /// (without `Progress`).
    fn run_follow(msgs: &[&str]) -> Vec<PlayerEvent> {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || follow(ours, &|e| tx.send(e).unwrap()).unwrap());

        let mut cmds = BufReader::new(theirs.try_clone().unwrap());
        let mut l = String::new();
        for (id, prop) in [(1, "path"), (2, "time-pos"), (3, "duration")] {
            l.clear();
            cmds.read_line(&mut l).unwrap();
            let cmd: Value = serde_json::from_str(&l).unwrap();
            assert_eq!(cmd["command"], serde_json::json!(["observe_property", id, prop]));
        }
        for m in msgs {
            writeln!(theirs, "{m}").unwrap();
        }
        theirs.shutdown(std::net::Shutdown::Write).unwrap();
        handle.join().unwrap();
        // Every `start-file` asked for the path, numbering the requests.
        let sent: Vec<Value> = cmds.lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
        let starts = msgs.iter().filter(|m| m.contains(r#""start-file""#)).count();
        let asked: Vec<Value> =
            (1..=starts).map(|n| serde_json::json!({ "command": ["get_property", "path"], "request_id": n })).collect();
        assert_eq!(sent, asked);
        rx.try_iter().filter(|e| !matches!(e, PlayerEvent::Progress { .. })).collect()
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
        let dir = tempfile::tempdir().unwrap();
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
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || follow(ours, &|e| tx.send(e).unwrap()));
        let lines: [&[u8]; 6] = [
            br#"{"event":"start-file","playlist_entry_id":1}"#,
            b"{\"event\":\"property-change\",\"id\":1,\"name\":\"path\",\"data\":\"/caf\xe9.mkv\"}",
            br#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
            br#"{"event":"start-file","playlist_entry_id":2}"#,
            br#"{"event":"property-change","id":1,"name":"path","data":"/b.mkv"}"#,
            br#"{"event":"property-change","id":2,"name":"time-pos","data":600.0}"#,
        ];
        for l in lines {
            theirs.write_all(&[l, b"\n"].concat()).unwrap();
        }
        theirs.shutdown(std::net::Shutdown::Write).unwrap();
        handle.join().unwrap().unwrap();
        let evs: Vec<PlayerEvent> = rx.try_iter().filter(|e| !matches!(e, PlayerEvent::Progress { .. })).collect();
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

    /// mpv reports no `path` change when the same file plays again (the same
    /// file twice in the playlist, `--loop-playlist`): the reply to the
    /// `get_property` sent at `start-file` names it, so both plays count.
    #[test]
    fn follow_tracks_the_same_file_twice_in_a_row() {
        let evs = run_follow(&[
            r#"{"event":"start-file","playlist_entry_id":1}"#,
            r#"{"request_id":1,"error":"success","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":1,"name":"path","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":1439.0}"#,
            r#"{"event":"end-file","reason":"eof","playlist_entry_id":1}"#,
            r#"{"event":"start-file","playlist_entry_id":2}"#,
            // A stale or failed reply is not this file's path.
            r#"{"request_id":1,"error":"success","data":"/stale.mkv"}"#,
            r#"{"request_id":2,"error":"success","data":"/a.mkv"}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":600.0}"#,
            r#"{"event":"end-file","reason":"quit","playlist_entry_id":2}"#,
            r#"{"event":"start-file","playlist_entry_id":3}"#,
            r#"{"request_id":3,"error":"property unavailable"}"#,
            r#"{"event":"property-change","id":2,"name":"time-pos","data":5.0}"#,
        ]);
        assert_eq!(
            evs,
            vec![
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 1439.0, dur: None, eof: true },
                PlayerEvent::Started { path: "/a.mkv".into() },
                PlayerEvent::Ended { path: "/a.mkv".into(), pos: 600.0, dur: None, eof: false },
            ]
        );
    }
}
