//! Keeping to `AniList`'s rate limits (docs.anilist.co/guide/rate-limiting).
//!
//! `AniList` allows a number of requests per minute (90, or 30 while its API
//! is degraded), says in every response how many are left
//! (`X-RateLimit-Limit`, `X-RateLimit-Remaining`), and has a burst limiter on
//! top. Going over gives a one-minute timeout: 429 responses with
//! `Retry-After` (seconds) and `X-RateLimit-Reset` (unix time).
//!
//! The [`Limiter`] stays under the limit first: it counts this process's
//! requests over the last minute, slows down when `AniList`'s own count (which
//! includes other processes, such as `anipv update` while the TUI is open)
//! runs low, and keeps a small gap between requests. A 429 is the fallback:
//! [`paced`] waits as told and sends the request again, within a budget.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// The window `AniList` counts requests in.
const WINDOW: Duration = Duration::from_secs(60);

/// Assumed requests per minute until a response says (the degraded limit).
const DEFAULT_LIMIT: u32 = 30;

/// Requests per window kept unused, for other clients and clock skew.
const MARGIN: u32 = 2;

/// With this few requests left (by `AniList`'s count), requests are spread
/// evenly over the window.
const LOW: u32 = 5;

/// Always this long between two requests, against the burst limiter.
const BURST_GAP: Duration = Duration::from_millis(300);

/// A 429 without a usable wait means the documented one-minute timeout.
const DEFAULT_TIMEOUT: u64 = 60;

/// The longest wait a 429 is believed for (a broken header asks for days).
const MAX_TIMEOUT: u64 = 120;

/// How long to pause before the one retry of a server error or failed request.
const TRANSIENT_PAUSE: Duration = Duration::from_secs(5);

/// Waits shorter than this are pacing, not worth telling about.
const NOTEWORTHY: Duration = Duration::from_secs(2);

/// The rate limit headers of a response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateHeaders {
    /// `X-RateLimit-Limit`: requests per minute.
    pub limit: Option<u32>,
    /// `X-RateLimit-Remaining`: requests left in this minute.
    pub remaining: Option<u32>,
    /// `Retry-After`: seconds to wait (429 only).
    pub retry_after: Option<u64>,
    /// `X-RateLimit-Reset`: unix time requests are allowed again (429 only).
    pub reset: Option<i64>,
}

impl RateHeaders {
    /// Read them through `get` (a header by name); missing or malformed ones
    /// are `None`.
    pub fn parse<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Self {
        fn num<T: std::str::FromStr>(v: Option<&str>) -> Option<T> {
            v?.trim().parse().ok()
        }
        Self {
            limit: num(get("x-ratelimit-limit")),
            remaining: num(get("x-ratelimit-remaining")),
            retry_after: num(get("retry-after")),
            reset: num(get("x-ratelimit-reset")),
        }
    }
}

/// What this process knows about the rate limit (see the module docs).
#[derive(Debug)]
pub struct Limiter {
    /// Requests per minute.
    limit: u32,
    /// `AniList`'s count of requests left, and when it said so.
    remaining: Option<(u32, Instant)>,
    /// When this process's requests of the last minute were sent.
    sent: VecDeque<Instant>,
    /// No request before this (a 429's timeout).
    blocked_until: Option<Instant>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self { limit: DEFAULT_LIMIT, remaining: None, sent: VecDeque::new(), blocked_until: None }
    }
}

impl Limiter {
    /// How long to wait at `now` before the next request may be sent.
    pub fn wait_before(&mut self, now: Instant) -> Duration {
        while self.sent.front().is_some_and(|&t| now.duration_since(t) >= WINDOW) {
            self.sent.pop_front();
        }
        let mut until = now;
        let mut not_before = |t: Instant| until = until.max(t);
        if let Some(t) = self.blocked_until {
            not_before(t);
        }
        if let Some(&last) = self.sent.back() {
            not_before(last + BURST_GAP);
        }
        // Our own count: at most `limit - MARGIN` per window.
        let cap = self.limit.saturating_sub(MARGIN).max(1) as usize;
        if self.sent.len() >= cap {
            not_before(self.sent[self.sent.len() - cap] + WINDOW);
        }
        // AniList's count, which knows about other clients, while it is recent.
        if let Some((left, at)) = self.remaining.filter(|&(_, at)| now.duration_since(at) < WINDOW) {
            if left <= 1 {
                // The window started at most a minute before it said so.
                not_before(at + WINDOW);
            } else if left <= LOW
                && let Some(&last) = self.sent.back()
            {
                not_before(last + WINDOW / self.limit.max(1));
            }
        }
        until - now
    }

    /// Record a request sent at `now`.
    pub fn sent(&mut self, now: Instant) {
        self.sent.push_back(now);
    }

    /// Learn from a response with `status` and headers `h`, received at `now`
    /// (`unix` seconds).
    pub fn on_response(&mut self, now: Instant, unix: i64, status: u16, h: &RateHeaders) {
        if let Some(limit) = h.limit.filter(|&l| l > 0) {
            self.limit = limit;
        }
        if let Some(left) = h.remaining {
            self.remaining = Some((left, now));
        }
        if status == 429 {
            let secs = h
                .retry_after
                .or_else(|| h.reset.map(|t| u64::try_from(t.saturating_sub(unix)).unwrap_or(0)))
                .unwrap_or(DEFAULT_TIMEOUT)
                .clamp(1, MAX_TIMEOUT);
            self.blocked_until = Some(now + Duration::from_secs(secs));
        }
    }
}

/// Time, and waiting, for [`paced`] (tests use a fake one).
pub trait Clock {
    /// Monotonic now.
    fn now(&self) -> Instant;
    /// Unix seconds now.
    fn unix(&self) -> i64;
    /// Wait for `d`.
    fn sleep(&self, d: Duration);
}

/// The real clock.
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn unix(&self) -> i64 {
        crate::events::now()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// A response, as far as [`paced`] needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// Its rate limit headers.
    pub headers: RateHeaders,
    /// The body.
    pub body: String,
}

/// Send a request with `send` when `limiter` allows it, telling `on_wait`
/// about every wait worth mentioning. A 429 is waited out and sent again; a
/// server error (5xx) or a failed request is sent again once, after a short
/// pause. Fails when the waiting would go past `budget` in total.
pub fn paced(
    limiter: &Mutex<Limiter>,
    clock: &dyn Clock,
    budget: Duration,
    on_wait: &dyn Fn(Duration),
    send: &mut dyn FnMut() -> Result<Reply>,
) -> Result<Reply> {
    let mut waited = Duration::ZERO;
    let mut retried_transient = false;
    let wait = |d: Duration, waited: &mut Duration| -> Result<()> {
        if *waited + d > budget {
            bail!(
                "AniList rate limit: still limited after waiting {waited} s; try again in {left} s",
                waited = waited.as_secs(),
                left = d.as_secs().max(1)
            );
        }
        if d >= NOTEWORTHY {
            on_wait(d);
        }
        clock.sleep(d);
        *waited += d;
        Ok(())
    };
    loop {
        // Checked and recorded under one lock, so threads don't both go.
        loop {
            let mut l = limiter.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = clock.now();
            let d = l.wait_before(now);
            if d.is_zero() {
                l.sent(now);
                break;
            }
            drop(l);
            wait(d, &mut waited)?;
        }
        match send() {
            Ok(reply) => {
                let mut l = limiter.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                l.on_response(clock.now(), clock.unix(), reply.status, &reply.headers);
                drop(l);
                match reply.status {
                    // Waited out by the limiter on the next round.
                    429 => {}
                    500..=599 if !retried_transient => {
                        retried_transient = true;
                        wait(TRANSIENT_PAUSE, &mut waited)?;
                    }
                    _ => return Ok(reply),
                }
            }
            Err(e) if !retried_transient => {
                retried_transient = true;
                // A request that fails twice is reported as itself.
                if wait(TRANSIENT_PAUSE, &mut waited).is_err() {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const S: fn(u64) -> Duration = Duration::from_secs;

    fn headers(limit: u32, remaining: u32) -> RateHeaders {
        RateHeaders { limit: Some(limit), remaining: Some(remaining), ..RateHeaders::default() }
    }

    #[test]
    fn headers_are_parsed_leniently() {
        let all = [("x-ratelimit-limit", "90"), ("x-ratelimit-remaining", " 59"), ("retry-after", "30")];
        let get = |name: &str| all.iter().find(|(n, _)| *n == name).map(|(_, v)| *v);
        let h = RateHeaders::parse(get);
        assert_eq!(h, RateHeaders { limit: Some(90), remaining: Some(59), retry_after: Some(30), reset: None });
        let garbage = |_: &str| Some("soon");
        assert_eq!(RateHeaders::parse(garbage), RateHeaders::default());
    }

    /// With plenty left, only the burst gap applies; our own count never
    /// goes past the limit less the margin within a minute.
    #[test]
    fn own_requests_stay_under_the_limit() {
        let t0 = Instant::now();
        let mut l = Limiter::default();
        l.on_response(t0, 0, 200, &headers(10, 9));
        assert_eq!(l.wait_before(t0), Duration::ZERO);
        l.sent(t0);
        assert_eq!(l.wait_before(t0), BURST_GAP);
        // 8 (10 - margin) in the window: the next waits for the first to age out.
        for i in 1..8 {
            l.sent(t0 + S(i));
        }
        let now = t0 + S(8);
        assert_eq!(l.wait_before(now), S(52));
        assert_eq!(l.wait_before(t0 + S(60)), Duration::ZERO);
    }

    /// AniList's count (other clients included): spread out when low, wait
    /// for the window when used up, and forget it once it is a minute old.
    #[test]
    fn anilists_count_slows_down() {
        let t0 = Instant::now();
        let mut l = Limiter::default();
        l.sent(t0);
        l.on_response(t0, 0, 200, &headers(30, 4));
        assert_eq!(l.wait_before(t0), S(2), "60 s / 30");
        l.on_response(t0, 0, 200, &headers(30, 1));
        assert_eq!(l.wait_before(t0 + S(1)), S(59));
        assert_eq!(l.wait_before(t0 + S(60)), Duration::ZERO, "a minute later it says nothing");
    }

    #[test]
    fn a_429_blocks_as_long_as_it_says() {
        let t0 = Instant::now();
        let blocked = |h: RateHeaders| {
            let mut l = Limiter::default();
            l.on_response(t0, 1000, 429, &h);
            l.wait_before(t0)
        };
        assert_eq!(blocked(RateHeaders { retry_after: Some(30), ..RateHeaders::default() }), S(30));
        assert_eq!(blocked(RateHeaders { reset: Some(1042), ..RateHeaders::default() }), S(42));
        assert_eq!(blocked(RateHeaders::default()), S(60), "the documented minute");
        assert_eq!(blocked(RateHeaders { retry_after: Some(86_400), ..RateHeaders::default() }), S(120));
        assert_eq!(blocked(RateHeaders { retry_after: Some(0), ..RateHeaders::default() }), S(1));
    }

    /// A clock that only moves when slept on.
    struct Fake {
        start: Instant,
        offset: Cell<Duration>,
        slept: RefCell<Vec<Duration>>,
    }

    impl Fake {
        fn new() -> Self {
            Self { start: Instant::now(), offset: Cell::new(Duration::ZERO), slept: RefCell::default() }
        }
    }

    impl Clock for Fake {
        fn now(&self) -> Instant {
            self.start + self.offset.get()
        }
        fn unix(&self) -> i64 {
            1000 + i64::try_from(self.offset.get().as_secs()).unwrap()
        }
        fn sleep(&self, d: Duration) {
            self.offset.set(self.offset.get() + d);
            self.slept.borrow_mut().push(d);
        }
    }

    #[expect(clippy::unnecessary_wraps, reason = "what `send` returns, for the reply lists")]
    fn reply(status: u16, h: RateHeaders) -> Result<Reply> {
        Ok(Reply { status, headers: h, body: String::new() })
    }

    fn run(budget: u64, replies: Vec<Result<Reply>>) -> (Result<Reply>, Vec<Duration>, Vec<Duration>, usize) {
        let clock = Fake::new();
        let limiter = Mutex::new(Limiter::default());
        let told = RefCell::new(Vec::new());
        let mut replies = replies.into_iter();
        let mut sent = 0;
        let res = paced(&limiter, &clock, S(budget), &|d| told.borrow_mut().push(d), &mut || {
            sent += 1;
            replies.next().expect("no more replies")
        });
        (res, clock.slept.take(), told.take(), sent)
    }

    #[test]
    fn a_429_is_waited_out_and_sent_again() {
        let limited = RateHeaders { retry_after: Some(30), ..RateHeaders::default() };
        let (res, slept, told, sent) = run(600, vec![reply(429, limited), reply(200, headers(30, 28))]);
        assert_eq!(res.unwrap().status, 200);
        assert_eq!((slept, told, sent), (vec![S(30)], vec![S(30)], 2));
    }

    #[test]
    fn waiting_stops_at_the_budget() {
        let limited = || reply(429, RateHeaders { retry_after: Some(60), ..RateHeaders::default() });
        let (res, slept, _, sent) = run(120, vec![limited(), limited(), limited()]);
        let err = res.unwrap_err().to_string();
        assert!(err.contains("still limited after waiting 120 s; try again in 60 s"), "{err}");
        assert_eq!((slept, sent), (vec![S(60), S(60)], 3));
    }

    #[test]
    fn server_errors_and_failures_are_retried_once() {
        let (res, slept, told, _) = run(600, vec![reply(502, RateHeaders::default()), reply(200, headers(30, 28))]);
        assert_eq!(res.unwrap().status, 200);
        assert_eq!((slept, told), (vec![TRANSIENT_PAUSE], vec![TRANSIENT_PAUSE]));
        let (res, _, _, sent) = run(600, vec![reply(503, RateHeaders::default()), reply(503, RateHeaders::default())]);
        assert_eq!((res.unwrap().status, sent), (503, 2), "the second is the answer");
        let (res, _, _, sent) = run(600, vec![Err(anyhow::anyhow!("timed out")), Err(anyhow::anyhow!("refused"))]);
        assert_eq!((res.unwrap_err().to_string(), sent), ("refused".to_string(), 2));
    }
}
