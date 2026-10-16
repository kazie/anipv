# Talking to AniList

How anipv keeps to [AniList's rate limits](https://docs.anilist.co/guide/rate-limiting)
and what it does when AniList can't be asked. The code is in
`src/meta/ratelimit.rs` (the limiter), `src/meta/anilist.rs` (the client) and
`src/meta/search.rs` (title searches).

These rules are decisions, each a trade-off made on purpose. Every rule names
the tests that hold it, and `tests/docs.rs` checks that those tests exist. To
change how anipv behaves, change the rule here first, with its reason, and
then the code and the tests.

## What AniList does

- It allows 90 requests per minute (30 while its API is degraded). Each
  response says how many are left (`X-RateLimit-Limit`,
  `X-RateLimit-Remaining`), and a burst limiter sits on top.
- Going over gives a timeout of about a minute: `429` responses with
  `Retry-After` (seconds) and `X-RateLimit-Reset` (unix time).
- Other clients on the same connection, such as `anipv update` while the TUI
  is open, count against the same limit.

## Rules

### Pacing: staying under the limit

| Rule | What happens | Why | Tests |
| --- | --- | --- | --- |
| R1 | At most the limit less 2 requests per minute by this process's own count, at least 300 ms apart. | Leave room for other clients and clock skew, and stay clear of the burst limiter. | `own_requests_stay_under_the_limit` |
| R2 | When AniList says 5 or fewer requests are left, requests are spread over the minute. When it says 1 or none, they wait for the minute to end. | AniList's count includes other clients; the last request is left to them. | `anilists_count_slows_down` |

### Timeouts: a 429

| Rule | What happens | Why | Tests |
| --- | --- | --- | --- |
| R3 | A 429 blocks every request of the process for as long as it says: `Retry-After`, else `X-RateLimit-Reset`. A value that isn't believable (0, in the past, over 15 minutes, a reset in milliseconds) means the documented minute. The count of 0 a 429 carries is ignored. | Never send while AniList refuses, but don't let a broken header block for long. | `a_429_blocks_as_long_as_it_says` |
| R4 | A 429 is waited out and the request sent again. | A rate limit is a delay, not a failure. | `a_429_is_waited_out_and_sent_again` |

### The wait budget

| Rule | What happens | Why | Tests |
| --- | --- | --- | --- |
| R5 | Each sync or command may wait out 429s for a total of 2 minutes while someone watches (the TUI, a command), or 10 minutes in `anipv update`. Its requests share that budget. | Someone watching shouldn't wait long; the timer runs unattended. | `the_budget_is_shared`, `waiting_stops_at_the_budget` |
| R6 | Only 429 timeouts count against the budget, never pacing (R1, R2), not even pacing that goes on after a timeout ends. | A big sync while AniList is degraded gets slower, never failed. | `pacing_is_not_counted`, `pacing_past_a_timeout_is_not_counted` |
| R7 | When waiting out a timeout would go past what is left of the budget, the request fails before it is sent, saying how long is left ("try again in N s"). So does every later request until the timeout ends. | Sending while AniList still refuses gets another 429 and can lengthen the timeout. | `a_long_timeout_fails_at_once` |
| R8 | Title searches, which are optional, may use only half of what is left of the budget. | The airing refresh after them must still have time. | `title_searches_get_half_the_budget` |

### Failures: no answer

| Rule | What happens | Why | Tests |
| --- | --- | --- | --- |
| R9 | A request that gets no answer, or a server error (5xx), is sent once more after 5 s. That pause isn't a rate limit wait: it isn't counted or announced. | Most failures are brief. | `server_errors_and_failures_are_retried_once` |
| R10 | The client gives up on AniList after two failed requests in a row. A failure is no answer, a 5xx, or an answer that isn't JSON (a proxy or captive portal answering instead). A JSON answer, even an error, resets the count. | One slow batch isn't an outage; two in a row are. | `the_client_gives_up_after_two_failures`, `answers_that_arent_json_give_up` |
| R11 | The client also gives up when a 429 outlasts the budget (R7). | Every later request would fail the same way. | `a_long_rate_limit_gives_up_unless_optional` |
| R12 | Optional requests (title searches) never give up and don't count towards it. | A failed search must not cost the airing refresh. | `optional_failures_dont_count`, `a_long_rate_limit_gives_up_unless_optional` |
| R13 | Once the client gives up, every later request fails at once without being sent, and the reason is reported once, not by every query. Ids not answered get no check record, so they are asked again next time. | No timeouts or waits for requests that would fail, and one message for one outage. | `the_client_gives_up_after_two_failures`, `a_given_up_client_skips_the_relations` |

### Batches

| Rule | What happens | Why | Tests |
| --- | --- | --- | --- |
| R14 | Queries by id go in batches of 50. A batch that fails doesn't discard the others; once the client gives up, the rest aren't sent. | Keep what was answered. | `a_failed_batch_keeps_the_others` |
| R15 | An error AniList answers (a GraphQL error, a 4xx with JSON) is about that batch: the other batches are still sent. | AniList is up; only that request was wrong. | `an_answered_error_keeps_going` |

### Title searches

| Rule | What happens | Why | Tests |
| --- | --- | --- | --- |
| R16 | Title searches stop at the first failed request. The series it failed for is recorded as failed and waits a day behind the others, unless AniList only refused for its rate limit. | A title that keeps failing must not hold up the rest, but a rate limit says nothing about the title. | `searches_stop_at_the_cap_and_at_a_failure`, `a_failing_title_does_not_hold_up_the_others` |
| R17 | When a search stops because its share of the budget (R8) ran out, nothing is said. Any other stop is a note. | The sync waits the same timeout out itself and carries on. | `searches_stop_at_the_cap_and_at_a_failure` |

### Telling the user

| Rule | What happens | Why | Tests |
| --- | --- | --- | --- |
| R18 | Waits of 2 s or more are announced before they start: a countdown in the TUI header, a line on stderr from commands (the journal, under systemd). Seconds are rounded up everywhere, so the last fraction of one shows as 1 s. | A pause should never look like a hang, and the TUI and the CLI should agree. | `rate_limit_waits_show_in_the_header`, `secs_round_up` |
