//! One retry policy for the upstream HTTP clients.
//!
//! Every client here already refuses redirects, caps response bodies and is
//! metered by [`crate::service_calls`]. What they did not share is what to do
//! with a *throttled* or *transiently failed* attempt: the TED client retried
//! with backoff and honoured `Retry-After`, while the NBB, BOSA and KBO clients
//! surfaced the failure and left the retry to an operator. That asymmetry is
//! where a scheduled or long-running fetch actually dies, so the rule set lives
//! here once and each client only chooses which rules apply to it.
//!
//! The rules stay deliberately narrow:
//!
//! - A status is retried only when the service's policy allows it: throttling,
//!   plus transient server errors for idempotent reads. A rejected request
//!   (400/403/404) is never retried: it will be rejected again.
//! - `Retry-After` is honoured plus a margin, because the shared call ledger
//!   records a server cooldown and *refuses* a retry that arrives before it
//!   elapses. Sleeping exactly the mandated cooldown would therefore spend an
//!   attempt on a locally refused request.
//! - A cooldown longer than the remaining budget stops immediately instead of
//!   being shortened into an early retry that the server has already refused.
//! - The total backoff of one logical operation is capped, so a hard throttle
//!   ends in a loud failure rather than an unbounded wait.
//! - Every attempt runs the caller's closure, so each one is charged to the
//!   ledger exactly once. Retries are attempts, not a hidden side channel.
//!
//! The module is crate-internal: binaries reach the behaviour through the
//! clients, never by choosing their own retry rules.

use std::time::Duration;

/// What one failed attempt says about whether trying again is sensible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetrySignal {
    /// The endpoint answered with a non-success status.
    Status {
        status: u16,
        retry_after: Option<u64>,
    },
    /// No status arrived (refused connection, reset, timeout).
    Transport,
    /// Not retryable at all; the caller owns the diagnosis.
    Final,
}

/// Why retrying stopped. The caller still owns the error type and its message,
/// because each client names its own service.
#[derive(Debug)]
pub(crate) enum RetryStop<E> {
    /// The attempt failed in a way this policy must not retry.
    Failed(E),
    /// Attempts or the backoff budget ran out.
    Exhausted {
        error: E,
        status: Option<u16>,
        attempts: usize,
        budget: Duration,
    },
}

impl<E> RetryStop<E> {
    /// The underlying failure, whichever way retrying stopped. Clients whose
    /// error already carries the status use this to keep one error path.
    pub(crate) fn into_error(self) -> E {
        match self {
            Self::Failed(error) | Self::Exhausted { error, .. } => error,
        }
    }

    /// How many attempts were made before giving up, for callers that report it.
    pub(crate) fn attempts(&self) -> usize {
        match self {
            Self::Failed(_) => 1,
            Self::Exhausted { attempts, .. } => *attempts,
        }
    }
}

/// The retry rules for one service. Constructed by the named service, never
/// built ad hoc at a call site, so no client can quietly invent its own policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    max_attempts: usize,
    base_delay: Duration,
    max_delay: Duration,
    max_total_wait: Duration,
    /// Added to a server-mandated cooldown so the shared ledger cooldown has
    /// really elapsed when the retry is dispatched.
    cooldown_margin: Duration,
    /// Wait used for a `429` that carries no `Retry-After`. It must match the
    /// cooldown the shared ledger records for exactly that case
    /// ([`crate::service_calls::THROTTLE_FALLBACK_COOLDOWN`]), or the retry is
    /// refused locally before it is dispatched. Zero for clients the ledger does
    /// not meter.
    throttle_fallback_cooldown: Duration,
    retry_server_errors: bool,
    retry_transport: bool,
}

impl RetryPolicy {
    /// A windowed TED refresh needs tens of sequential pages, and TED throttles
    /// that with `429`. Transport failures are not retried here: a page that
    /// never got an answer is reported, because the ingest must not silently
    /// paper over a broken connection across a long backfill.
    pub(crate) const fn ted_search() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(16),
            max_total_wait: Duration::from_secs(120),
            cooldown_margin: Duration::from_secs(1),
            // TED is not metered by the shared ledger, so a headerless 429 is
            // answered with ordinary exponential backoff rather than a penalising
            // minute-long wait.
            throttle_fallback_cooldown: Duration::ZERO,
            retry_server_errors: true,
            retry_transport: false,
        }
    }

    /// NBB's Authentic Data API: read-only document and reference downloads.
    /// Transport errors are retried because a lost connection is exactly what
    /// broke the serial season fetch, and every attempt is still reserved in the
    /// ledger before it is sent. The budget stays far below the 120s per-request
    /// timeout times the attempt count, so a hard outage still ends in minutes.
    pub(crate) const fn nbb() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
            max_total_wait: Duration::from_secs(180),
            cooldown_margin: Duration::from_secs(1),
            throttle_fallback_cooldown: crate::service_calls::THROTTLE_FALLBACK_COOLDOWN,
            retry_server_errors: true,
            retry_transport: true,
        }
    }

    /// BOSA e-Procurement reads, including the token exchange and signed object
    /// downloads. Every call routed here is an idempotent `GET` or a
    /// `client_credentials` token request that issues a fresh token, so a
    /// transient failure is worth another attempt. A rejected credential is a
    /// `4xx` and therefore final.
    pub(crate) const fn bosa() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(16),
            // Two fallback cooldowns (60s + 1s margin, twice) must both fit, or the
            // third attempt the policy advertises is unreachable for a headerless
            // 429.
            max_total_wait: Duration::from_secs(130),
            cooldown_margin: Duration::from_secs(1),
            throttle_fallback_cooldown: crate::service_calls::THROTTLE_FALLBACK_COOLDOWN,
            retry_server_errors: true,
            retry_transport: true,
        }
    }

    /// KBO's balance-metered SOAP lookup. Throttling only: the service answers
    /// transport faults *and* SOAP faults over HTTP, and a `500` carries the
    /// `faultstring` that diagnoses a bad credential or an exhausted balance.
    /// Retrying that status would spend balance twice on a diagnosis already in
    /// hand, so it stays a reported failure. Transport failures are reported for
    /// the same reason: the call may have been metered even without an answer.
    pub(crate) const fn kbo() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(16),
            // Throttling is the only status this policy retries, so it must fit two
            // fallback cooldowns: otherwise the third attempt is unreachable and the
            // advertised attempt count is fiction.
            max_total_wait: Duration::from_secs(130),
            cooldown_margin: Duration::from_secs(1),
            throttle_fallback_cooldown: crate::service_calls::THROTTLE_FALLBACK_COOLDOWN,
            retry_server_errors: false,
            retry_transport: false,
        }
    }

    fn is_retryable_status(&self, status: u16) -> bool {
        if status == 429 {
            return true;
        }
        self.retry_server_errors && (500..600).contains(&status)
    }

    /// Whether the signal is retryable, and the status/cooldown to carry into a
    /// give-up message.
    pub(crate) fn decide(&self, signal: &RetrySignal) -> Option<(Option<u16>, Option<u64>)> {
        match *signal {
            RetrySignal::Status {
                status,
                retry_after,
            } => {
                if !self.is_retryable_status(status) {
                    return None;
                }
                // A throttled response without `Retry-After` still imposes the
                // ledger's fallback cooldown. Retrying sooner would be refused
                // locally, so the operation would fail on its second attempt.
                let retry_after = match (status, retry_after) {
                    (429, None) if !self.throttle_fallback_cooldown.is_zero() => {
                        Some(self.throttle_fallback_cooldown.as_secs())
                    }
                    (_, retry_after) => retry_after,
                };
                Some((Some(status), retry_after))
            }
            RetrySignal::Transport => self.retry_transport.then_some((None, None)),
            RetrySignal::Final => None,
        }
    }

    /// Backoff before the retry that follows `attempt` (1-based). A
    /// server-mandated cooldown wins over the exponential schedule, because a
    /// `Retry-After: 60` shortened into an immediate retry is another throttle.
    fn delay_for(&self, attempt: usize, retry_after: Option<u64>) -> Duration {
        if let Some(seconds) = retry_after {
            return Duration::from_secs(seconds)
                .saturating_add(self.cooldown_margin)
                .min(self.max_total_wait);
        }
        let factor = 1_u32 << attempt.saturating_sub(1).min(4);
        (self.base_delay * factor).min(self.max_delay)
    }
}

/// Sends one logical request under `policy`, calling `send` again after a
/// retryable failure. `classify` maps the caller's error type onto
/// [`RetrySignal`] so each client keeps its own error vocabulary.
pub(crate) fn send_with_retry<E, F, C>(
    policy: &RetryPolicy,
    mut send: F,
    classify: C,
) -> Result<ureq::Response, RetryStop<E>>
where
    F: FnMut() -> Result<ureq::Response, E>,
    C: Fn(&E) -> RetrySignal,
{
    let mut waited = Duration::ZERO;
    let mut attempts = 0_usize;
    loop {
        attempts += 1;
        let error = match send() {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        let signal = classify(&error);
        let Some((status, retry_after)) = policy.decide(&signal) else {
            return Err(RetryStop::Failed(error));
        };
        let exhausted = |error| RetryStop::Exhausted {
            error,
            status,
            attempts,
            budget: policy.max_total_wait,
        };
        if attempts >= policy.max_attempts {
            return Err(exhausted(error));
        }
        let Wait::Sleep(delay) = next_wait(policy, attempts, retry_after, waited) else {
            return Err(exhausted(error));
        };
        std::thread::sleep(delay);
        waited += delay;
    }
}

/// What to do before attempting again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wait {
    Sleep(Duration),
    /// Retrying would exceed the operation's total backoff budget.
    Stop,
}

/// The wait decision, kept pure so the arithmetic that bounds total waiting can be
/// checked without sleeping.
///
/// This is the part of the loop that could actually hang a scheduled fetch: a
/// mistake here does not fail fast, it blocks. Two rules:
///
/// - A server-mandated cooldown (`Retry-After` plus the ledger margin) is honoured
///   exactly, or the operation stops. It is never shortened into an earlier retry
///   the server has already refused.
/// - Whatever the reason, the accumulated wait never exceeds `max_total_wait`:
///   `Sleep(d)` is only returned when `waited + d` still fits.
pub(crate) fn next_wait(
    policy: &RetryPolicy,
    attempt: usize,
    retry_after: Option<u64>,
    waited: Duration,
) -> Wait {
    let remaining = policy.max_total_wait.saturating_sub(waited);
    let delay = match retry_after {
        Some(seconds) => Duration::from_secs(seconds).saturating_add(policy.cooldown_margin),
        None => policy.delay_for(attempt, retry_after),
    };
    if delay > remaining {
        return Wait::Stop;
    }
    Wait::Sleep(delay)
}

/// Reads `Retry-After` the one way, for the retry driver and the call ledger
/// alike.
///
/// RFC 7231 allows either a delay in seconds or an HTTP-date, and the two
/// readers used to disagree: the ledger understood dates while this module
/// understood only seconds. A date header therefore read as "no header" here,
/// so TED retried on its first backoff step — earlier than the server allowed —
/// and the metered clients waited the fallback cooldown only to be refused by
/// their own ledger, which had recorded the date as a cooldown.
///
/// An unusable value returns `None` (the caller keeps its own fallback), and a
/// date in the past returns zero: there is nothing left to wait for.
pub(crate) fn retry_after_seconds_at(
    value: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<u64> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.signed_duration_since(now).num_seconds().max(0) as u64)
}

/// [`retry_after_seconds_at`] against the wall clock.
pub(crate) fn retry_after_seconds(value: Option<&str>) -> Option<u64> {
    retry_after_seconds_at(value, chrono::Utc::now())
}

/// Classify a raw `ureq` error for a client that has no error type of its own.
pub(crate) fn ureq_signal(error: &ureq::Error) -> RetrySignal {
    match error {
        ureq::Error::Status(status, response) => RetrySignal::Status {
            status: *status,
            retry_after: retry_after_seconds(response.header("Retry-After")),
        },
        _ => RetrySignal::Transport,
    }
}

/// One attempt's failure, kept unstructured so a *retryable* status does not
/// consume the body that explains it: only the final attempt is read for a
/// diagnostic. Clients with their own error type map this once, after retrying
/// has stopped, so no client has to invent a second attempt loop.
#[derive(Debug)]
pub(crate) enum Attempt {
    /// The shared call ledger refused the request or could not record it.
    /// Repeating it reads the same ledger state, so it is never retried.
    Guard(String),
    /// A response that is not the expected success, including a redirect:
    /// redirects stay refused everywhere. Boxed because `ureq::Response` is an
    /// order of magnitude larger than the other variants.
    Response(Box<ureq::Response>),
    /// No response arrived at all.
    Transport,
}

impl Attempt {
    /// Normalise a ledgered call into "success, or the reason to look again".
    pub(crate) fn classify(
        result: Result<ureq::Response, ureq::Error>,
    ) -> Result<ureq::Response, Self> {
        match result {
            Ok(response) if (200..300).contains(&response.status()) => Ok(response),
            Ok(response) => Err(Self::Response(Box::new(response))),
            Err(ureq::Error::Status(_, response)) => Err(Self::Response(Box::new(response))),
            Err(_) => Err(Self::Transport),
        }
    }

    pub(crate) fn signal(&self) -> RetrySignal {
        match self {
            Self::Response(response) => RetrySignal::Status {
                status: response.status(),
                retry_after: retry_after_seconds(response.header("Retry-After")),
            },
            Self::Transport => RetrySignal::Transport,
            Self::Guard(_) => RetrySignal::Final,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(4),
            max_total_wait: Duration::from_millis(50),
            cooldown_margin: Duration::from_millis(1),
            throttle_fallback_cooldown: Duration::ZERO,
            retry_server_errors: true,
            retry_transport: true,
        }
    }

    /// Serves the given status lines in order and reports how many requests
    /// arrived, so a test can prove a non-retryable failure sent exactly one.
    fn serve(
        statuses: &'static [&'static str],
        retry_after: &'static [&'static str],
    ) -> (String, std::thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for (index, status) in statuses.iter().enumerate() {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                served += 1;
                let header = retry_after
                    .get(index)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("Retry-After: {value}\r\n"))
                    .unwrap_or_default();
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status}\r\n{header}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                )
                .unwrap();
            }
            served
        });
        (endpoint, handle)
    }

    // ureq's own error type is large; boxing it is not this test's business.
    #[allow(clippy::result_large_err)]
    fn get(endpoint: &str) -> Result<ureq::Response, ureq::Error> {
        ureq::get(endpoint).call()
    }

    #[test]
    fn policy_retries_only_what_it_is_configured_to_retry() {
        let ted = RetryPolicy::ted_search();
        assert!(ted.is_retryable_status(429));
        assert!(ted.is_retryable_status(503));
        assert!(!ted.is_retryable_status(400));
        assert!(!ted.is_retryable_status(403));
        assert!(!ted.is_retryable_status(404));
        assert_eq!(
            ted.decide(&RetrySignal::Transport),
            None,
            "a page with no answer is reported, not retried"
        );

        let kbo = RetryPolicy::kbo();
        assert!(kbo.is_retryable_status(429));
        assert!(
            !kbo.is_retryable_status(500),
            "a SOAP fault carries the diagnosis; retrying it spends balance twice"
        );
        assert_eq!(kbo.decide(&RetrySignal::Transport), None);

        assert_eq!(
            fast_policy().decide(&RetrySignal::Final),
            None,
            "an explicit Final signal is never retried"
        );
    }

    #[test]
    fn backoff_honours_retry_after_and_the_attempt_schedule() {
        let policy = RetryPolicy::ted_search();
        assert_eq!(
            policy.delay_for(1, Some(45)),
            Duration::from_secs(45) + policy.cooldown_margin
        );
        assert_eq!(
            policy.delay_for(1, Some(600)),
            policy.max_total_wait,
            "a cooldown longer than the budget is capped for the message, not slept"
        );
        assert_eq!(policy.delay_for(1, None), policy.base_delay);
        assert_eq!(policy.delay_for(2, None), policy.base_delay * 2);
        assert_eq!(policy.delay_for(9, None), policy.max_delay);
    }

    #[test]
    fn retries_a_throttled_request_then_succeeds() {
        let (endpoint, server) = serve(&["429 Too Many Requests", "200 OK"], &["", ""]);
        let response = send_with_retry(&RetryPolicy::ted_search(), || get(&endpoint), ureq_signal)
            .expect("a throttled request must be retried, not fatal");
        assert_eq!(response.status(), 200);
        assert_eq!(server.join().unwrap(), 2);
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates_the_same_way() {
        let now = chrono::DateTime::parse_from_rfc2822("Sat, 7 Nov 2026 12:00:00 +0000")
            .expect("valid date")
            .with_timezone(&chrono::Utc);
        assert_eq!(retry_after_seconds_at(Some("45"), now), Some(45));
        assert_eq!(retry_after_seconds_at(Some(" 45 "), now), Some(45));
        assert_eq!(
            retry_after_seconds_at(Some("Sat, 7 Nov 2026 12:15:00 +0000"), now),
            Some(900),
            "an HTTP-date is a cooldown like any other"
        );
        assert_eq!(
            retry_after_seconds_at(Some("Sat, 7 Nov 2026 11:00:00 +0000"), now),
            Some(0),
            "a date in the past leaves nothing to wait for"
        );
        assert_eq!(retry_after_seconds_at(Some("soon"), now), None);
        assert_eq!(retry_after_seconds_at(Some("   "), now), None);
        assert_eq!(retry_after_seconds_at(None, now), None);
    }

    /// One header, one reading. A date used to arrive here as "no header", so the
    /// driver substituted its fallback and retried earlier than the server allowed
    /// (TED) or waited the fallback and was refused by its own ledger (NBB/BOSA).
    #[test]
    fn a_date_retry_after_reaches_the_policy_instead_of_the_fallback() {
        let future = (chrono::Utc::now() + chrono::Duration::minutes(15)).to_rfc2822();
        let leaked: &'static str = Box::leak(future.into_boxed_str());
        let headers: &'static [&'static str] = Box::leak(vec![leaked].into_boxed_slice());
        let (endpoint, server) = serve(&["429 Too Many Requests"], headers);
        let error = get(&endpoint).expect_err("429 must be reported as an error");
        let signal = ureq_signal(&error);
        server.join().unwrap();
        let (status, retry_after) = RetryPolicy::ted_search()
            .decide(&signal)
            .expect("429 is retryable");
        assert_eq!(status, Some(429));
        assert!(
            retry_after.is_some_and(|seconds| seconds >= 800),
            "the server's fifteen minutes must reach the policy, not the fallback: {retry_after:?}"
        );
    }

    #[test]
    fn does_not_retry_a_rejected_request() {
        let (endpoint, server) = serve(&["403 Forbidden"], &[""]);
        let stop = send_with_retry(&RetryPolicy::ted_search(), || get(&endpoint), ureq_signal)
            .expect_err("403 is not retryable");
        assert!(matches!(stop, RetryStop::Failed(_)));
        assert_eq!(
            server.join().unwrap(),
            1,
            "a rejected request must be sent once, not hammered"
        );
    }

    #[test]
    fn server_errors_are_retried_only_where_the_policy_allows() {
        let (endpoint, server) = serve(&["503 Service Unavailable"], &[""]);
        let stop = send_with_retry(&RetryPolicy::kbo(), || get(&endpoint), ureq_signal)
            .expect_err("KBO does not retry server errors");
        assert!(matches!(stop, RetryStop::Failed(_)));
        assert_eq!(server.join().unwrap(), 1);
    }

    #[test]
    fn gives_up_after_the_attempt_cap_and_reports_the_status() {
        let (endpoint, server) = serve(
            &[
                "429 Too Many Requests",
                "429 Too Many Requests",
                "429 Too Many Requests",
            ],
            &["", "", ""],
        );
        let stop =
            send_with_retry(&fast_policy(), || get(&endpoint), ureq_signal).expect_err("throttled");
        match stop {
            RetryStop::Exhausted {
                status,
                attempts,
                budget,
                ..
            } => {
                assert_eq!(status, Some(429));
                assert_eq!(attempts, 3);
                assert_eq!(budget, Duration::from_millis(50));
            }
            other => panic!("expected exhaustion, got {other:?}"),
        }
        assert_eq!(server.join().unwrap(), 3);
    }

    #[test]
    fn a_cooldown_beyond_the_budget_fails_immediately_instead_of_sleeping() {
        // The server asks for an hour; the budget is 50ms. Sleeping the capped
        // delay and retrying early would only earn another throttle.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            write!(
                reader.get_mut(),
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\n\
                 Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            )
            .unwrap();
        });
        let started = std::time::Instant::now();
        let stop =
            send_with_retry(&fast_policy(), || get(&endpoint), ureq_signal).expect_err("throttled");
        assert!(matches!(stop, RetryStop::Exhausted { .. }));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "an hour-long cooldown must not be slept or shortened into a retry"
        );
        server.join().unwrap();
    }

    #[test]
    fn transport_failures_are_retried_only_where_the_policy_allows() {
        // A closed port refuses the connection immediately, so this exercises
        // the classify path rather than a timeout.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let stop = send_with_retry(&RetryPolicy::ted_search(), || get(&endpoint), ureq_signal)
            .expect_err("no listener");
        assert!(
            matches!(stop, RetryStop::Failed(_)),
            "the TED search policy reports transport failures"
        );
    }

    /// One scripted attempt outcome. `Response::new` builds a synthetic reply, so
    /// the loop can be exercised with no socket and no sleeping.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        Ok,
        Status {
            status: u16,
            retry_after: Option<u64>,
        },
        Transport,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Class {
        Ok,
        Failed,
        Exhausted,
    }

    #[derive(Debug)]
    enum TestFailure {
        Status {
            status: u16,
            retry_after: Option<u64>,
        },
        Transport,
    }

    fn classify(error: &TestFailure) -> RetrySignal {
        match error {
            TestFailure::Status {
                status,
                retry_after,
            } => RetrySignal::Status {
                status: *status,
                retry_after: *retry_after,
            },
            TestFailure::Transport => RetrySignal::Transport,
        }
    }

    fn signal_of(step: Step) -> RetrySignal {
        match step {
            Step::Status {
                status,
                retry_after,
            } => RetrySignal::Status {
                status,
                retry_after,
            },
            Step::Transport => RetrySignal::Transport,
            Step::Ok => RetrySignal::Final,
        }
    }

    /// The contract, stated independently of the loop's bookkeeping: stop at the
    /// first success, stop at the first attempt the policy will not retry, and stop
    /// once `max_attempts` attempts have failed retroactively-retryably.
    ///
    /// Delays are zero and the budget is zero in this policy, so any server
    /// cooldown ends the operation immediately instead of sleeping; that is the
    /// same rule the production policies apply, only without the wait.
    /// The retryability rules, stated here rather than read back from the policy.
    ///
    /// `None` means "not retryable"; `Some(retry_after)` means retryable after that
    /// cooldown. Written out so a mistake in the policy tables cannot make the model
    /// and the implementation agree with each other — that circularity is exactly
    /// how a "403 is retried" regression slipped past the first version of this
    /// check.
    fn expected_retryable(
        step: Step,
        retry_server_errors: bool,
        retry_transport: bool,
        throttle_fallback: Duration,
    ) -> Option<Option<u64>> {
        match step {
            Step::Ok => None,
            Step::Status {
                status,
                retry_after,
            } => {
                let retryable =
                    status == 429 || (retry_server_errors && (500..600).contains(&status));
                if !retryable {
                    return None;
                }
                // A throttled response with no header still has to wait out the
                // cooldown the ledger records for exactly that case.
                let retry_after = match (status, retry_after) {
                    (429, None) if !throttle_fallback.is_zero() => {
                        Some(throttle_fallback.as_secs())
                    }
                    (_, retry_after) => retry_after,
                };
                Some(retry_after)
            }
            Step::Transport => retry_transport.then_some(None),
        }
    }

    /// `None` when the script is too short to determine the outcome: the loop would
    /// have to attempt once more than the script describes, and a script is a
    /// prefix of the attempt sequence, not a whole one.
    fn contract(policy: &RetryPolicy, steps: &[Step]) -> Option<(usize, Class)> {
        let mut attempts = 0;
        for step in steps {
            attempts += 1;
            if *step == Step::Ok {
                return Some((attempts, Class::Ok));
            }
            let Some(retry_after) = expected_retryable(
                *step,
                policy.retry_server_errors,
                policy.retry_transport,
                policy.throttle_fallback_cooldown,
            ) else {
                return Some((attempts, Class::Failed));
            };
            if retry_after.is_some() {
                // A cooldown is never shortened into an early retry.
                return Some((attempts, Class::Exhausted));
            }
            if attempts >= policy.max_attempts {
                return Some((attempts, Class::Exhausted));
            }
        }
        None
    }

    fn run_scripted(policy: &RetryPolicy, steps: &[Step]) -> (usize, Class, Option<u16>) {
        let mut index = 0_usize;
        let mut calls = 0_usize;
        let outcome = send_with_retry(
            policy,
            || {
                calls += 1;
                let step = steps
                    .get(index)
                    .copied()
                    .unwrap_or_else(|| panic!("loop attempted past the scripted prefix {steps:?}"));
                index += 1;
                match step {
                    Step::Ok => {
                        ureq::Response::new(200, "OK", "{}").map_err(|_| TestFailure::Transport)
                    }
                    Step::Status {
                        status,
                        retry_after,
                    } => Err(TestFailure::Status {
                        status,
                        retry_after,
                    }),
                    Step::Transport => Err(TestFailure::Transport),
                }
            },
            classify,
        );
        match outcome {
            Ok(response) => (calls, Class::Ok, Some(response.status())),
            Err(RetryStop::Failed(_)) => (calls, Class::Failed, None),
            Err(RetryStop::Exhausted { .. }) => (calls, Class::Exhausted, None),
        }
    }

    /// Exhaustive, not sampled: every sequence of up to three scripted outcomes over
    /// {success, 429, 503, 403, transport}, under each retryable-status policy. The
    /// retry loop is what every provider client now depends on, and a hand-picked
    /// case cannot show that no *combination* re-dispatches a request the policy
    /// refused or exceeds the attempt cap.
    /// Walks every script of exactly `length` steps over `steps`.
    fn walk(
        policy: &RetryPolicy,
        steps: &[Step],
        length: usize,
        script: &mut Vec<Step>,
        determined: &mut usize,
        skipped: &mut usize,
    ) {
        if script.len() == length {
            let Some(expected) = contract(policy, script) else {
                // The script is a prefix; the loop would need one attempt more.
                *skipped += 1;
                return;
            };
            *determined += 1;
            for step in script.iter() {
                let implementation = policy
                    .decide(&signal_of(*step))
                    .map(|(_, retry_after)| retry_after);
                assert_eq!(
                    implementation,
                    expected_retryable(
                        *step,
                        policy.retry_server_errors,
                        policy.retry_transport,
                        policy.throttle_fallback_cooldown,
                    ),
                    "policy table disagrees with the stated rules for {step:?}"
                );
            }
            let (calls, class, status) = run_scripted(policy, script);
            assert_eq!((calls, class), expected, "policy {policy:?} on {script:?}");
            assert!(
                calls <= policy.max_attempts,
                "attempts never exceed the cap: {script:?}"
            );
            if class == Class::Ok {
                assert_eq!(status, Some(200));
            }
            return;
        }
        for step in steps {
            script.push(*step);
            walk(policy, steps, length, script, determined, skipped);
            script.pop();
        }
    }

    #[test]
    fn every_short_outcome_sequence_obeys_the_policy_contract() {
        let steps = [
            Step::Ok,
            Step::Status {
                status: 429,
                retry_after: None,
            },
            Step::Status {
                status: 503,
                retry_after: None,
            },
            Step::Status {
                status: 403,
                retry_after: None,
            },
            Step::Transport,
        ];
        let policies: Vec<RetryPolicy> = [true, false]
            .into_iter()
            .flat_map(|retry_server_errors| {
                [true, false].into_iter().flat_map(move |retry_transport| {
                    [
                        Duration::ZERO,
                        crate::service_calls::THROTTLE_FALLBACK_COOLDOWN,
                    ]
                    .map(move |throttle_fallback_cooldown| RetryPolicy {
                        max_attempts: 3,
                        base_delay: Duration::ZERO,
                        max_delay: Duration::ZERO,
                        max_total_wait: Duration::ZERO,
                        cooldown_margin: Duration::ZERO,
                        throttle_fallback_cooldown,
                        retry_server_errors,
                        retry_transport,
                    })
                })
            })
            .collect();
        let mut determined = 0_usize;
        let mut skipped = 0_usize;
        for policy in &policies {
            for length in 1..=3 {
                walk(
                    policy,
                    &steps,
                    length,
                    &mut Vec::new(),
                    &mut determined,
                    &mut skipped,
                );
            }
        }
        assert_eq!(
            determined + skipped,
            8 * (5 + 25 + 125),
            "every script of up to three steps was walked under each policy"
        );
        assert!(
            determined > 0 && skipped > 0,
            "the walk must both check and skip under-determined prefixes: {determined}/{skipped}"
        );
    }

    /// A server cooldown must end the operation without sleeping, in every policy.
    #[test]
    fn a_mandated_cooldown_always_ends_the_operation_immediately() {
        for retry_server_errors in [true, false] {
            let policy = RetryPolicy {
                max_attempts: 5,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
                max_total_wait: Duration::from_secs(30),
                cooldown_margin: Duration::from_millis(1),
                throttle_fallback_cooldown: Duration::ZERO,
                retry_server_errors,
                retry_transport: true,
            };
            let started = std::time::Instant::now();
            let (calls, class, _) = run_scripted(
                &policy,
                &[Step::Status {
                    status: 429,
                    retry_after: Some(600),
                }],
            );
            assert_eq!((calls, class), (1, Class::Exhausted));
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "a 600s cooldown must not be slept"
            );
        }
    }

    /// The wait schedule written out, so the property check below cannot be
    /// satisfied by reading the implementation back to itself.
    #[test]
    fn the_backoff_schedule_is_the_documented_one() {
        let policy = RetryPolicy::ted_search();
        let schedule = [
            (1_usize, Duration::from_secs(1)),
            (2, Duration::from_secs(2)),
            (3, Duration::from_secs(4)),
            (4, Duration::from_secs(8)),
            (5, Duration::from_secs(16)),
            (9, Duration::from_secs(16)),
        ];
        for (attempt, expected) in schedule {
            assert_eq!(
                next_wait(&policy, attempt, None, Duration::ZERO),
                Wait::Sleep(expected),
                "attempt {attempt}"
            );
        }
        // A cooldown is honoured exactly, including the ledger margin.
        assert_eq!(
            next_wait(&policy, 1, Some(30), Duration::ZERO),
            Wait::Sleep(Duration::from_secs(31))
        );
        assert_eq!(
            next_wait(&policy, 1, Some(0), Duration::ZERO),
            Wait::Sleep(policy.cooldown_margin)
        );
    }

    /// Exhaustive over the wait-arithmetic inputs. This is the part of the loop that
    /// can block a scheduled fetch rather than fail fast, and it used to be
    /// reachable only through a real `thread::sleep`.
    #[test]
    fn the_wait_decision_never_exceeds_the_budget_or_shortens_a_cooldown() {
        let mut policies = Vec::new();
        for budget_secs in [0_u64, 1, 30, 120] {
            for margin_ms in [0_u64, 1_000] {
                for base_secs in [1_u64, 2] {
                    for max_secs in [16_u64, 30] {
                        policies.push(RetryPolicy {
                            max_attempts: 5,
                            base_delay: Duration::from_secs(base_secs),
                            max_delay: Duration::from_secs(max_secs),
                            max_total_wait: Duration::from_secs(budget_secs),
                            cooldown_margin: Duration::from_millis(margin_ms),
                            throttle_fallback_cooldown: Duration::ZERO,
                            retry_server_errors: true,
                            retry_transport: true,
                        });
                    }
                }
            }
        }
        let retry_afters = [
            None,
            Some(0_u64),
            Some(1),
            Some(29),
            Some(30),
            Some(59),
            Some(121),
            Some(600),
        ];
        let waits = [0_u64, 1, 15, 30, 59, 120, 1_000];
        let mut checked = 0_usize;
        for policy in &policies {
            for attempt in 1..=5_usize {
                for retry_after in retry_afters {
                    for waited_secs in waits {
                        let waited = Duration::from_secs(waited_secs);
                        let remaining = policy.max_total_wait.saturating_sub(waited);
                        checked += 1;
                        match next_wait(policy, attempt, retry_after, waited) {
                            Wait::Sleep(delay) => {
                                // `waited > budget` cannot be reached from the loop,
                                // which only ever adds a wait that fits; a synthetic
                                // zero delay is likewise harmless. The invariant is
                                // about the states the loop can actually be in.
                                if waited <= policy.max_total_wait {
                                    assert!(
                                        waited.saturating_add(delay) <= policy.max_total_wait,
                                        "sleep would exceed the budget: {policy:?} attempt \
                                         {attempt} retry_after {retry_after:?} waited {waited:?} \
                                         delay {delay:?}"
                                    );
                                }
                                // A cooldown is never shortened to fit the budget.
                                if let Some(seconds) = retry_after {
                                    assert_eq!(
                                        delay,
                                        Duration::from_secs(seconds)
                                            .saturating_add(policy.cooldown_margin),
                                        "cooldown must be honoured exactly"
                                    );
                                } else {
                                    assert!(
                                        delay <= policy.max_delay,
                                        "backoff never exceeds max_delay"
                                    );
                                }
                            }
                            Wait::Stop => {
                                // Stopping is only allowed when the wait that would
                                // have been applied does not fit.
                                if let Some(seconds) = retry_after {
                                    let wanted = Duration::from_secs(seconds)
                                        .saturating_add(policy.cooldown_margin);
                                    assert!(
                                        wanted > remaining,
                                        "stopped although the cooldown fits: {policy:?} \
                                         retry_after {retry_after:?} waited {waited:?}"
                                    );
                                }
                                // Once stopped, waiting longer cannot revive the run.
                                let later = waited.saturating_add(Duration::from_secs(60));
                                assert_eq!(
                                    next_wait(policy, attempt, retry_after, later),
                                    Wait::Stop,
                                    "stopping is monotone in waited"
                                );
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(checked, policies.len() * 5 * 8 * 7);
    }

    /// A `429` with no `Retry-After` is the case that used to break silently: the
    /// ledger records its fallback cooldown, so a client that retried after its own
    /// short backoff was refused locally and the operation failed on attempt two.
    #[test]
    fn a_throttle_without_a_header_waits_out_the_ledgers_fallback_cooldown() {
        let headerless = RetrySignal::Status {
            status: 429,
            retry_after: None,
        };
        for policy in [RetryPolicy::nbb(), RetryPolicy::bosa(), RetryPolicy::kbo()] {
            let Some((status, retry_after)) = policy.decide(&headerless) else {
                panic!("a throttled response must be retryable: {policy:?}");
            };
            assert_eq!(status, Some(429));
            assert_eq!(
                retry_after,
                Some(crate::service_calls::THROTTLE_FALLBACK_COOLDOWN.as_secs()),
                "the wait must match the cooldown the ledger records: {policy:?}"
            );
            // And it must actually be slept, not turned into an immediate stop.
            let per_attempt =
                crate::service_calls::THROTTLE_FALLBACK_COOLDOWN + policy.cooldown_margin;
            assert_eq!(
                next_wait(&policy, 1, retry_after, Duration::ZERO),
                Wait::Sleep(per_attempt),
                "the fallback cooldown has to fit the policy's budget: {policy:?}"
            );
            // Every attempt the policy advertises has to be reachable: after one
            // fallback wait, a second one must still fit for the third attempt.
            for attempt in 2..policy.max_attempts {
                assert_eq!(
                    next_wait(
                        &policy,
                        attempt,
                        retry_after,
                        per_attempt * (attempt as u32 - 1)
                    ),
                    Wait::Sleep(per_attempt),
                    "attempt {} is unreachable with a {} budget: {policy:?}",
                    attempt + 1,
                    policy.max_total_wait.as_secs()
                );
            }
        }
        // TED is not metered by the ledger, so it keeps ordinary backoff instead of
        // paying a minute-long penalty for a headerless throttle.
        assert_eq!(
            RetryPolicy::ted_search().decide(&headerless),
            Some((Some(429), None))
        );
        assert!(
            RetryPolicy::ted_search()
                .throttle_fallback_cooldown
                .is_zero(),
            "an unmetered client must not pay the ledger's throttle penalty"
        );
    }

    #[test]
    fn exhausted_failures_keep_their_underlying_error() {
        let stop: RetryStop<&str> = RetryStop::Exhausted {
            error: "boom",
            status: Some(429),
            attempts: 3,
            budget: Duration::from_secs(1),
        };
        assert_eq!(stop.attempts(), 3);
        assert_eq!(stop.into_error(), "boom");
    }
}
