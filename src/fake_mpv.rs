//! A stand-in for mpv that speaks just enough of its JSON IPC for anipv's
//! integration tests and the README recording (the hidden `anipv fake-mpv`).
//!
//! Like mpv it takes `--input-ipc-server=PATH` and file arguments (other
//! options, `-…`, are ignored, except `--start=S` for the next file and
//! `--pause`, to start paused).
//!
//! * **Instant** (default): plays the first file to the end (eof) and the
//!   second only halfway, then quits, all without waiting.
//! * **Demo** (`--demo`): plays each file in real time, ten times faster
//!   than mpv would, and handles the key presses anipv's remote mode sends:
//!   `SPACE`/`p` pause, `LEFT`/`RIGHT` seek 5 s, `DOWN`/`UP` seek 60 s,
//!   `q`/`ESC` quit.
//!
//! Both answer requests as they come, and like mpv report an observed
//! property right away and then only when its value changes.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Every fake episode's duration (s): 24:00.
const DURATION: f64 = 1440.0;
/// Demo mode: how often the position moves on by a second.
const TICK: Duration = Duration::from_millis(100);

/// A file to play, from the command line.
struct File {
    path: PathBuf,
    start: f64,
}

/// What the command line asks for.
struct Args {
    socket: PathBuf,
    files: Vec<File>,
    /// Start paused.
    pause: bool,
}

/// Read mpv-style arguments.
fn parse_args(args: &[OsString]) -> Result<Args> {
    let mut socket = None;
    let mut files = Vec::new();
    let (mut start, mut pause) = (0.0, false);
    for a in args {
        let s = a.to_string_lossy();
        if let Some(p) = s.strip_prefix("--input-ipc-server=") {
            socket = Some(PathBuf::from(p));
        } else if let Some(pos) = s.strip_prefix("--start=") {
            start = pos.parse().with_context(|| format!("bad {s}"))?;
        } else if s == "--pause" {
            pause = true;
        } else if !s.starts_with('-') {
            files.push(File { path: a.into(), start: std::mem::take(&mut start) });
        }
    }
    let Some(socket) = socket else { bail!("no --input-ipc-server=PATH") };
    Ok(Args { socket, files, pause })
}

/// Run the fake player on mpv-style `args` until it is done or quit.
pub fn run(demo: bool, args: &[OsString]) -> Result<()> {
    let args = parse_args(args)?;
    let socket = &args.socket;
    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {socket}", socket = socket.display()))?;
    let (conn, _) = listener.accept()?;
    let res = play(&conn, &args, demo);
    // Close the connection for every handle on it (the request reader holds
    // one), as mpv exiting would: anipv sees the end of the stream.
    let _ = conn.shutdown(std::net::Shutdown::Both);
    let _ = std::fs::remove_file(socket);
    res
}

/// Send one message to anipv.
fn send(mut conn: &UnixStream, msg: &Value) -> std::io::Result<()> {
    conn.write_all(format!("{msg}\n").as_bytes())
}

/// The next JSON line from `lines` (a request to the fake, or a message to anipv).
pub(crate) fn request(lines: &mut impl BufRead) -> Result<Value> {
    let mut l = String::new();
    if lines.read_line(&mut l)? == 0 {
        bail!("disconnected");
    }
    Ok(serde_json::from_str(&l)?)
}

/// The fake player's state.
struct Player<'a> {
    conn: &'a UnixStream,
    /// Observed properties: (id anipv chose, name).
    observed: Vec<(i64, String)>,
    /// The value last reported for each property.
    reported: Vec<(String, Value)>,
    path: Option<String>,
    pos: f64,
    paused: bool,
    quit: bool,
}

impl<'a> Player<'a> {
    fn new(conn: &'a UnixStream) -> Self {
        Self { conn, observed: Vec::new(), reported: Vec::new(), path: None, pos: 0.0, paused: false, quit: false }
    }

    /// A property's value as mpv would report it: `Ok(Null)` while it is
    /// unavailable (no file open), `Err` for one this fake doesn't know.
    fn property(&self, name: &str) -> Result<Value, &'static str> {
        let open = |v: Value| if self.path.is_some() { v } else { Value::Null };
        Ok(match name {
            "path" => self.path.clone().map_or(Value::Null, Value::from),
            "time-pos" => open(self.pos.into()),
            "duration" => open(DURATION.into()),
            "pause" => self.paused.into(),
            _ => return Err("property not found"),
        })
    }

    /// Report `name`'s value to anipv if it observes it, unless it is the
    /// value reported last (`force`: on observing, always).
    fn report(&mut self, name: &str, force: bool) -> std::io::Result<()> {
        let data = self.property(name).unwrap_or(Value::Null);
        match self.reported.iter_mut().find(|(n, _)| n == name) {
            Some((_, last)) if *last == data && !force => return Ok(()),
            Some((_, last)) => *last = data.clone(),
            None => self.reported.push((name.to_owned(), data.clone())),
        }
        for (id, _) in self.observed.iter().filter(|(_, n)| n == name) {
            send(self.conn, &json!({ "event": "property-change", "id": id, "name": name, "data": data }))?;
        }
        Ok(())
    }

    /// Answer one request from anipv.
    fn on_request(&mut self, req: &Value) -> std::io::Result<()> {
        let cmd = &req["command"];
        let mut observe = None;
        let mut reply = match (cmd[0].as_str(), cmd[1].as_i64(), cmd[1].as_str(), cmd[2].as_str()) {
            (Some("observe_property"), Some(id), _, Some(name)) => {
                self.observed.push((id, name.to_owned()));
                observe = Some(name);
                json!({ "error": "success" })
            }
            (Some("get_property"), _, Some(name), _) => match self.property(name) {
                Ok(Value::Null) => json!({ "error": "property unavailable" }),
                Ok(v) => json!({ "error": "success", "data": v }),
                Err(e) => json!({ "error": e }),
            },
            (Some("keypress"), _, Some(key), _) => {
                self.keypress(key)?;
                json!({ "error": "success" })
            }
            _ => json!({ "error": "invalid parameter" }),
        };
        // Like mpv: 0 when the request named none.
        reply["request_id"] = req.get("request_id").cloned().unwrap_or_else(|| Value::from(0));
        send(self.conn, &reply)?;
        // mpv reports an observed property's current value right away.
        observe.map_or(Ok(()), |name| self.report(name, true))
    }

    /// What mpv's default bindings do with these keys.
    fn keypress(&mut self, key: &str) -> std::io::Result<()> {
        let seek = match key {
            "SPACE" | "p" => {
                self.paused = !self.paused;
                return self.report("pause", false);
            }
            "q" | "ESC" => {
                self.quit = true;
                return Ok(());
            }
            "LEFT" => -5.0,
            "RIGHT" => 5.0,
            "DOWN" => -60.0,
            "UP" => 60.0,
            _ => return Ok(()),
        };
        self.pos = (self.pos + seek).clamp(0.0, DURATION);
        self.report("time-pos", false)
    }

    /// Handle requests until `until` (forever: `None`) or until `done`
    /// holds; anipv going away quits.
    fn serve(&mut self, rx: &Receiver<Value>, until: Option<Instant>, done: impl Fn(&Self) -> bool) -> Result<()> {
        while !self.quit && !done(self) {
            let req = match until {
                Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match req {
                Ok(req) => self.on_request(&req)?,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => self.quit = true,
            }
        }
        Ok(())
    }
}

/// Play the files of `args` (see the module docs).
fn play(conn: &UnixStream, args: &Args, demo: bool) -> Result<()> {
    let (tx, rx) = channel();
    let mut lines = BufReader::new(conn.try_clone()?);
    std::thread::spawn(move || {
        while let Ok(req) = request(&mut lines) {
            if tx.send(req).is_err() {
                break;
            }
        }
    });
    let mut p = Player { paused: args.pause, ..Player::new(conn) };
    // Like mpv opening its first file only after anipv observes, so nothing
    // is missed (anipv copes with that too, but the tests want a fixed order).
    let watching = |p: &Player| ["path", "time-pos", "duration"].iter().all(|n| p.observed.iter().any(|(_, o)| o == n));
    p.serve(&rx, None, watching)?;
    for (i, f) in args.files.iter().enumerate() {
        if p.quit {
            break;
        }
        let entry = i + 1;
        send(conn, &json!({ "event": "start-file", "playlist_entry_id": entry }))?;
        (p.path, p.pos) = (Some(f.path.to_string_lossy().into_owned()), f.start.clamp(0.0, DURATION));
        p.report("path", false)?;
        send(conn, &json!({ "event": "file-loaded" }))?;
        p.report("duration", false)?;
        if demo {
            while !p.quit && p.pos < DURATION {
                p.report("time-pos", false)?;
                p.serve(&rx, Some(Instant::now() + TICK), |_| false)?;
                if !p.paused {
                    p.pos = (p.pos + 1.0).min(DURATION);
                }
            }
        } else {
            // The first to the end, then the second halfway and quit.
            p.pos = if i == 0 { DURATION } else { DURATION / 2.0 };
            p.quit = i > 0;
        }
        p.report("time-pos", false)?;
        let reason = if p.quit { "quit" } else { "eof" };
        send(conn, &json!({ "event": "end-file", "reason": reason, "playlist_entry_id": entry }))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpv::{PlayerEvent, Remote};

    /// The fake reads the arguments anipv passes mpv.
    #[test]
    fn args_like_mpv() {
        let cfg = crate::config::Config { mpv_args: vec!["--fs".into()], ..Default::default() };
        let play = |p: &str, start| crate::mpv::PlayFile { path: p.into(), start };
        let args =
            crate::mpv::build_args(&cfg, Some("/s".as_ref()), &[play("/a.mkv", Some(612.0)), play("/b.mkv", None)]);
        let Args { socket, files, pause } = parse_args(&args).unwrap();
        assert_eq!((socket, pause), (PathBuf::from("/s"), false));
        let files: Vec<(PathBuf, f64)> = files.into_iter().map(|f| (f.path, f.start)).collect();
        assert_eq!(files, vec![("/a.mkv".into(), 612.0), ("/b.mkv".into(), 0.0)]);
        assert!(parse_args(&["/a.mkv".into()]).is_err(), "no socket");
        let args = parse_args(&["-v", "--pause", "--input-ipc-server=/s", "/a.mkv"].map(OsString::from)).unwrap();
        assert_eq!(args.files.len(), 1, "an option is no file");
        assert!(args.pause);
    }

    /// Demo-mode replies look like mpv's: properties by name, the observed
    /// value right after the reply, under the id anipv chose, and request id
    /// 0 when the request named none.
    #[test]
    fn demo_replies_like_mpv() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut p = Player { paused: true, ..Player::new(&ours) };
        let mut lines = BufReader::new(theirs);
        let mut ask = |req: Value, replies: usize| -> Vec<Value> {
            p.on_request(&req).unwrap();
            (0..replies).map(|_| request(&mut lines).unwrap()).collect()
        };
        assert_eq!(
            ask(json!({ "command": ["observe_property", 7, "pause"] }), 2),
            vec![
                json!({ "error": "success", "request_id": 0 }),
                json!({ "event": "property-change", "id": 7, "name": "pause", "data": true }),
            ]
        );
        assert_eq!(
            ask(json!({ "command": ["get_property", "pause"], "request_id": 3 }), 1),
            vec![json!({ "error": "success", "data": true, "request_id": 3 })]
        );
        assert_eq!(
            ask(json!({ "command": ["get_property", "time-pos"], "request_id": 4 }), 1),
            vec![json!({ "error": "property unavailable", "request_id": 4 })],
            "no file open"
        );
        assert_eq!(
            ask(json!({ "command": ["get_property", "volume"], "request_id": 5 }), 1),
            vec![json!({ "error": "property not found", "request_id": 5 })]
        );
    }

    /// The keys remote mode sends, taken as mpv's default bindings take them.
    #[test]
    fn demo_keys_pause_seek_and_quit() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let mut p = Player { path: Some("/a.mkv".into()), ..Player::new(&ours) };
        for (key, paused, pos) in [
            ("SPACE", true, 0.0),
            ("UP", true, 60.0),
            ("RIGHT", true, 65.0),
            ("LEFT", true, 60.0),
            ("DOWN", true, 0.0),
            ("LEFT", true, 0.0),
            ("p", false, 0.0),
            ("x", false, 0.0),
        ] {
            p.keypress(key).unwrap();
            assert_eq!((p.paused, p.pos), (paused, pos), "after {key}");
        }
        assert!(!p.quit);
        p.keypress("q").unwrap();
        assert!(p.quit);
    }

    /// Demo mode, followed like the real mpv: started paused (`--pause`) its
    /// clock stands still, `UP` seeks a minute on and `q` quits, ending the
    /// file right there.
    #[test]
    fn demo_mode_answers_remote_keys() {
        let dir = crate::mpv::socket_test_dir();
        let socket = dir.path().join("sock");
        let ipc = format!("--input-ipc-server={socket}", socket = socket.display());
        let args = vec![OsString::from(ipc), "--pause".into(), "/a.mkv".into()];
        let player = std::thread::spawn(move || run(true, &args));
        let conn = loop {
            if let Ok(c) = UnixStream::connect(&socket) {
                break c;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let remote = Remote::new(conn.try_clone().unwrap());
        let (tx, rx) = channel();
        let keys = remote.clone();
        let follower = std::thread::spawn(move || crate::mpv::follow(conn, &keys, &|e| tx.send(e).unwrap()));

        let mut seen = Vec::new();
        while !matches!(seen.last(), Some(PlayerEvent::Started { .. })) {
            seen.push(rx.recv().expect("the player ended first"));
        }
        // A tick that must not move the clock.
        std::thread::sleep(TICK + TICK / 2);
        remote.keypress("UP").unwrap();
        remote.keypress("q").unwrap();
        player.join().unwrap().unwrap();
        follower.join().unwrap().unwrap();
        seen.extend(rx.iter().filter(|e| !matches!(e, PlayerEvent::Progress { .. })));
        assert!(seen.contains(&PlayerEvent::Paused(true)) && !seen.contains(&PlayerEvent::Paused(false)), "{seen:?}");
        assert!(seen.contains(&PlayerEvent::Started { path: "/a.mkv".into() }), "{seen:?}");
        let ended =
            PlayerEvent::Ended { path: "/a.mkv".into(), pos: 60.0, furthest: 60.0, dur: Some(DURATION), eof: false };
        assert_eq!(seen.last(), Some(&ended), "{seen:?}");
    }
}
