//! Local-machine call accounting and conservative guardrails, not provider quota claims.
//! Reserve durably BEFORE sending; a crash remains charged as an unknown outcome.
//! All clients on this machine/user share the default ledger, including worktrees.
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const MAX_LEDGER_BYTES: u64 = 64 * 1024 * 1024;

/// Cooldown recorded for a `429` that carries no `Retry-After`.
///
/// Shared with [`crate::http_retry`]: that module must not retry sooner than this,
/// or the ledger refuses the retry locally and the operation fails on its second
/// attempt instead of waiting out the throttle.
pub(crate) const THROTTLE_FALLBACK_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Kbo,
    Nbb,
    BosaInt,
    BosaProd,
}
impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Self::Kbo => "kbo",
            Self::Nbb => "nbb",
            Self::BosaInt => "bosa_int",
            Self::BosaProd => "bosa_prod",
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            Self::Kbo => "SERVICE_KBO",
            Self::Nbb => "SERVICE_NBB",
            Self::BosaInt => "SERVICE_BOSA_INT",
            Self::BosaProd => "SERVICE_BOSA_PROD",
        }
    }
}

/// Closed labels: never log full URLs, query terms, company IDs, signed URLs or secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    KboEnterprise,
    NbbReferences,
    NbbReference,
    NbbDocument,
    NbbBatchReferences,
    NbbBatchDocuments,
    BosaOauth,
    BosaApi,
    BosaDownload,
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub daily_limit: u64,
    pub min_interval_ms: u64,
}
impl Policy {
    pub fn from_env(service: Service) -> Result<Self, AuditError> {
        let number = |suffix: &str, default: u64, max: u64| {
            let name = format!("{}_{suffix}", service.prefix());
            let n = match std::env::var(&name) {
                Ok(v) => v
                    .parse::<u64>()
                    .map_err(|_| AuditError(format!("{name} must be an unsigned integer")))?,
                Err(std::env::VarError::NotPresent) => default,
                Err(_) => return Err(AuditError(format!("{name} is not valid UTF-8"))),
            };
            if n > max {
                return Err(AuditError(format!(
                    "{name} exceeds the local safety bound {max}"
                )));
            }
            Ok(n)
        };
        Ok(Self {
            daily_limit: number("DAILY_LIMIT", 100, 100_000)?,
            min_interval_ms: number("MIN_INTERVAL_MS", 1000, 60_000)?,
        })
    }
}

#[derive(Debug)]
pub struct AuditError(String);
impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "service call guard: {}", self.0)
    }
}
impl std::error::Error for AuditError {}
fn io_error(_: std::io::Error) -> AuditError {
    AuditError(
        "local ledger I/O failed; request refused (check permissions, disk and ledger path)".into(),
    )
}
fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}
fn day(ms: i64) -> Result<NaiveDate, AuditError> {
    DateTime::from_timestamp_millis(ms)
        .map(|d| d.date_naive())
        .ok_or_else(|| AuditError("invalid ledger timestamp".into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Attempt,
    Outcome,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    version: u8,
    id: Uuid,
    timestamp_ms: i64,
    service: Service,
    operation: Operation,
    phase: Phase,
    status: Option<u16>,
    transport_error: bool,
    cooldown_until_ms: Option<i64>,
}

#[derive(Clone)]
pub struct Ledger {
    directory: PathBuf,
}
impl Ledger {
    pub fn from_env() -> Result<Self, AuditError> {
        let directory = match std::env::var_os("SERVICE_CALLS_DIR") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            Some(_) => return Err(AuditError("SERVICE_CALLS_DIR is empty".into())),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .ok_or_else(|| AuditError("HOME or SERVICE_CALLS_DIR must be set".into()))?,
            )
            .join(".local/state/ehb-fin-aanbestedingen/service-calls"),
        };
        if !directory.is_absolute() {
            return Err(AuditError(
                "SERVICE_CALLS_DIR must be absolute and on a local filesystem".into(),
            ));
        }
        Ok(Self { directory })
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    fn prepare(&self) -> Result<(), AuditError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt};
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.directory)
                .map_err(io_error)?;
            let m = fs::symlink_metadata(&self.directory).map_err(io_error)?;
            // SAFETY: geteuid has no arguments and no preconditions.
            if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
                return Err(AuditError(
                    "ledger directory must be owned by this user, non-symlink and mode 0700".into(),
                ));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(AuditError(
                "call auditing requires Unix file locks; no request was sent".into(),
            ))
        }
    }
    fn lock(&self, name: &str, timeout: Duration) -> Result<File, AuditError> {
        let file = private_open(&self.directory.join(name))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let started = Instant::now();
            loop {
                // SAFETY: the descriptor is valid for the lifetime of `file`.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Ok(file);
                }
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::Interrupted
                {
                    return Err(io_error(e));
                }
                if started.elapsed() >= timeout {
                    return Err(AuditError(
                        "ledger/provider lock busy; request refused".into(),
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (file, timeout);
            Err(AuditError("Unix file locks unavailable".into()))
        }
    }
    fn read(&self) -> Result<Vec<Event>, AuditError> {
        let mut file = private_open(&self.directory.join("calls.jsonl"))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_LEDGER_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            return Err(AuditError(
                "ledger exceeds 64 MiB; preserve/archive it before further requests".into(),
            ));
        }
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(AuditError(
                "incomplete ledger tail; preserve and inspect it before further requests".into(),
            ));
        }
        let mut events = Vec::new();
        let mut attempts = HashMap::new();
        let mut latest_timestamp = BTreeMap::new();
        for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let e: Event = serde_json::from_slice(line).map_err(|_| {
                AuditError("corrupt ledger; request refused (do not delete its history)".into())
            })?;
            day(e.timestamp_ms)?;
            if latest_timestamp
                .get(&e.service)
                .is_some_and(|previous| *previous > e.timestamp_ms)
            {
                return Err(AuditError(
                    "out-of-order ledger timestamps; request refused".into(),
                ));
            }
            latest_timestamp.insert(e.service, e.timestamp_ms);
            if e.version != 1 {
                return Err(AuditError("unsupported ledger version".into()));
            }
            match e.phase {
                Phase::Attempt
                    if !attempts.contains_key(&e.id)
                        && e.status.is_none()
                        && !e.transport_error
                        && e.cooldown_until_ms.is_none() =>
                {
                    attempts.insert(e.id, (e.service, e.operation, false));
                }
                Phase::Outcome => match attempts.get_mut(&e.id) {
                    Some((service, operation, done))
                        if *service == e.service
                            && *operation == e.operation
                            && !*done
                            && (e.status.is_some() != e.transport_error) =>
                    {
                        *done = true;
                    }
                    _ => return Err(AuditError("inconsistent ledger outcome".into())),
                },
                _ => return Err(AuditError("inconsistent ledger attempt".into())),
            }
            events.push(e);
        }
        Ok(events)
    }
    fn append(&self, event: &Event) -> Result<(), AuditError> {
        let mut file = private_open(&self.directory.join("calls.jsonl"))?;
        let mut bytes = serde_json::to_vec(event)
            .map_err(|_| AuditError("cannot encode ledger event".into()))?;
        bytes.push(b'\n');
        if file.metadata().map_err(io_error)?.len() + bytes.len() as u64 > MAX_LEDGER_BYTES {
            return Err(AuditError("ledger exceeds 64 MiB; request refused".into()));
        }
        file.write_all(&bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        File::open(&self.directory)
            .and_then(|d| d.sync_all())
            .map_err(io_error)?;
        Ok(())
    }
    fn begin(
        &self,
        service: Service,
        operation: Operation,
        policy: Policy,
    ) -> Result<Ticket, AuditError> {
        self.prepare()?;
        // Held THROUGH the HTTP operation so paused/concurrent processes cannot bunch sends.
        let provider_lock = self.lock(
            &format!("{}.lock", service.name()),
            Duration::from_secs(180),
        )?;
        let acquired_at = now_ms();
        loop {
            let ledger_lock = self.lock("ledger.lock", Duration::from_secs(5))?;
            let events = self.read()?;
            let now = now_ms();
            let today = day(now)?;
            let service_events = events
                .iter()
                .filter(|e| e.service == service)
                .collect::<Vec<_>>();
            if service_events.iter().any(|e| e.timestamp_ms > now) {
                return Err(AuditError("clock moved backwards; request refused".into()));
            }
            let used = service_events
                .iter()
                .filter(|e| e.phase == Phase::Attempt && day(e.timestamp_ms).ok() == Some(today))
                .count() as u64;
            if used >= policy.daily_limit {
                return Err(AuditError(format!(
                    "{} UTC daily cap reached ({used}/{}); no request sent",
                    service.name(),
                    policy.daily_limit
                )));
            }
            if service_events
                .iter()
                .filter_map(|e| e.cooldown_until_ms)
                .any(|until| until > now)
            {
                return Err(AuditError(format!(
                    "{} server cooldown active; no request sent",
                    service.name()
                )));
            }
            // Pace from the previous outcome, not its pre-dispatch reservation:
            // a process may have paused between reserving and actually sending.
            // After an abandoned reservation, also wait from lock acquisition.
            let last = service_events.last().map(|e| {
                if e.phase == Phase::Attempt {
                    e.timestamp_ms.max(acquired_at)
                } else {
                    e.timestamp_ms
                }
            });
            if let Some(last) = last {
                let wait = last
                    .saturating_add(policy.min_interval_ms.min(60_000) as i64)
                    .saturating_sub(now);
                if wait > 0 {
                    drop(ledger_lock);
                    thread::sleep(Duration::from_millis(wait as u64));
                    continue;
                }
            }
            let event = Event {
                version: 1,
                id: Uuid::new_v4(),
                timestamp_ms: now,
                service,
                operation,
                phase: Phase::Attempt,
                status: None,
                transport_error: false,
                cooldown_until_ms: None,
            };
            self.append(&event)?;
            return Ok(Ticket {
                ledger: self.clone(),
                event,
                _provider_lock: provider_lock,
            });
        }
    }
    pub fn summary(&self) -> Result<Vec<Usage>, AuditError> {
        self.prepare()?;
        let _lock = self.lock("ledger.lock", Duration::from_secs(5))?;
        let events = self.read()?;
        let mut groups = BTreeMap::new();
        let mut attempts = HashMap::new();
        for e in &events {
            if e.phase == Phase::Attempt {
                let date = day(e.timestamp_ms)?;
                let row = groups.entry((date, e.service)).or_insert_with(|| Usage {
                    date: date.to_string(),
                    service: e.service,
                    attempts: 0,
                    unknown: 0,
                    transport_errors: 0,
                    statuses: BTreeMap::new(),
                });
                row.attempts += 1;
                row.unknown += 1;
                attempts.insert(e.id, (date, e.service));
            } else {
                let key = attempts[&e.id];
                let row = groups.get_mut(&key).expect("validated prior attempt");
                row.unknown -= 1;
                if let Some(status) = e.status {
                    *row.statuses.entry(status).or_default() += 1;
                } else {
                    row.transport_errors += 1;
                }
            }
        }
        Ok(groups.into_values().collect())
    }
}

fn private_open(path: &Path) -> Result<File, AuditError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        let file = options.open(path).map_err(io_error)?;
        let m = file.metadata().map_err(io_error)?;
        // SAFETY: geteuid has no arguments and no preconditions.
        if !m.is_file()
            || m.uid() != unsafe { libc::geteuid() }
            || m.mode() & 0o077 != 0
            || m.nlink() != 1
        {
            return Err(AuditError(
                "ledger files must be private regular files owned by this user (no links)".into(),
            ));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = options;
        Err(AuditError("Unix file locks unavailable".into()))
    }
}

struct Ticket {
    ledger: Ledger,
    event: Event,
    _provider_lock: File,
}
impl Ticket {
    fn finish(mut self, status: Option<u16>, retry_after: Option<&str>) -> Result<(), AuditError> {
        let now = now_ms();
        if now < self.event.timestamp_ms {
            return Err(AuditError(
                "clock moved backwards after dispatch; outcome remains unknown".into(),
            ));
        }
        // The retry driver reads this same header through the same parser, so a
        // cooldown the ledger records and a wait the driver chooses can no longer
        // disagree about what the server asked for (seconds or an HTTP-date).
        let cooldown = retry_after
            .and_then(|value| crate::http_retry::retry_after_seconds(Some(value)))
            .map(|seconds| {
                now.saturating_add(seconds.saturating_mul(1000).min(i64::MAX as u64) as i64)
            });
        self.event.timestamp_ms = now;
        self.event.phase = Phase::Outcome;
        self.event.status = status;
        self.event.transport_error = status.is_none();
        self.event.cooldown_until_ms = if status == Some(429) {
            Some(cooldown.unwrap_or_else(|| {
                now.saturating_add(THROTTLE_FALLBACK_COOLDOWN.as_millis() as i64)
            }))
            .max(Some(now.saturating_add(1000)))
        } else {
            cooldown
        };
        let _lock = self.ledger.lock("ledger.lock", Duration::from_secs(5))?;
        self.ledger.append(&self.event)
    }
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub date: String,
    pub service: Service,
    pub attempts: u64,
    pub unknown: u64,
    pub transport_errors: u64,
    pub statuses: BTreeMap<u16, u64>,
}

/// Counts every HTTP attempt (including token exchanges, retries and object downloads).
/// HTTP status is known at headers; later body/parse failure does not change that status.
pub fn http<F>(
    service: Service,
    operation: Operation,
    send: F,
) -> Result<Result<ureq::Response, ureq::Error>, AuditError>
where
    F: FnOnce() -> Result<ureq::Response, ureq::Error>,
{
    #[cfg(not(test))]
    let (ledger, policy) = (Ledger::from_env()?, Policy::from_env(service)?);
    #[cfg(test)]
    let (ledger, policy) = (
        test_ledger(),
        Policy {
            daily_limit: 100_000,
            min_interval_ms: 0,
        },
    );
    http_with(ledger, policy, service, operation, send)
}

fn http_with<F>(
    ledger: Ledger,
    policy: Policy,
    service: Service,
    operation: Operation,
    send: F,
) -> Result<Result<ureq::Response, ureq::Error>, AuditError>
where
    F: FnOnce() -> Result<ureq::Response, ureq::Error>,
{
    let ticket = ledger.begin(service, operation, policy)?;
    let result = send();
    let response = match &result {
        Ok(r) | Err(ureq::Error::Status(_, r)) => Some(r),
        Err(_) => None,
    };
    ticket.finish(
        response.map(|r| r.status()),
        response.and_then(|r| r.header("Retry-After")),
    )?;
    Ok(result)
}

#[cfg(test)]
pub(crate) fn test_ledger() -> Ledger {
    thread_local! {static DIRECTORY:PathBuf=std::env::temp_dir().join(format!("service-calls-test-{}",Uuid::new_v4()));}
    DIRECTORY.with(|directory| Ledger {
        directory: directory.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ledger() -> Ledger {
        Ledger {
            directory: std::env::temp_dir().join(format!("service-ledger-{}", Uuid::new_v4())),
        }
    }
    fn policy(limit: u64) -> Policy {
        Policy {
            daily_limit: limit,
            min_interval_ms: 0,
        }
    }
    #[test]
    fn zero_cap_never_invokes_the_network_closure() {
        let result = http_with(
            ledger(),
            policy(0),
            Service::Nbb,
            Operation::NbbReferences,
            || panic!("network must not run when denied"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn pacing_survives_separate_ledger_instances() {
        let l = ledger();
        l.begin(Service::Nbb, Operation::NbbReferences, policy(2))
            .unwrap()
            .finish(Some(200), None)
            .unwrap();
        let reopened = Ledger {
            directory: l.directory.clone(),
        };
        reopened
            .begin(
                Service::Nbb,
                Operation::NbbReferences,
                Policy {
                    daily_limit: 2,
                    min_interval_ms: 40,
                },
            )
            .unwrap()
            .finish(Some(200), None)
            .unwrap();
        let events = l.read().unwrap();
        let attempts = events
            .iter()
            .filter(|e| e.phase == Phase::Attempt)
            .collect::<Vec<_>>();
        let first_outcome = events.iter().find(|e| e.phase == Phase::Outcome).unwrap();
        assert!(attempts[1].timestamp_ms - first_outcome.timestamp_ms >= 40);
    }

    #[test]
    fn utc_rollover_and_backwards_clock_are_conservative() {
        let l = ledger();
        l.prepare().unwrap();
        let mut e = Event {
            version: 1,
            id: Uuid::new_v4(),
            timestamp_ms: now_ms() - 86_400_000,
            service: Service::Nbb,
            operation: Operation::NbbReferences,
            phase: Phase::Attempt,
            status: None,
            transport_error: false,
            cooldown_until_ms: None,
        };
        l.append(&e).unwrap();
        l.begin(Service::Nbb, Operation::NbbReferences, policy(1))
            .unwrap()
            .finish(Some(200), None)
            .unwrap();
        e.id = Uuid::new_v4();
        e.timestamp_ms = now_ms() + 86_400_000;
        l.append(&e).unwrap();
        assert!(l
            .begin(Service::Nbb, Operation::NbbReferences, policy(10))
            .err()
            .unwrap()
            .to_string()
            .contains("clock moved backwards"));
    }

    #[test]
    fn http_date_retry_after_is_persisted() {
        let l = ledger();
        let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc2822();
        l.begin(Service::Nbb, Operation::NbbReferences, policy(2))
            .unwrap()
            .finish(Some(503), Some(&future))
            .unwrap();
        assert!(l
            .begin(Service::Nbb, Operation::NbbReferences, policy(2))
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_linked_ledger_files() {
        use std::os::unix::fs::symlink;
        let l = ledger();
        l.prepare().unwrap();
        let target = l.directory.join("target");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, l.directory.join("calls.jsonl")).unwrap();
        assert!(l
            .begin(Service::Nbb, Operation::NbbReferences, policy(1))
            .is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
    }

    #[test]
    fn attempted_calls_are_charged_before_dispatch_and_unknown_after_crash() {
        let l = ledger();
        let t = l
            .begin(Service::Nbb, Operation::NbbReferences, policy(1))
            .unwrap();
        drop(t);
        let rows = l.summary().unwrap();
        assert_eq!(rows[0].attempts, 1);
        assert_eq!(rows[0].unknown, 1);
        assert!(l
            .begin(Service::Nbb, Operation::NbbReferences, policy(1))
            .is_err());
    }
    #[test]
    fn status_transport_and_zero_caps_are_distinct() {
        let l = ledger();
        assert!(l
            .begin(Service::Kbo, Operation::KboEnterprise, policy(0))
            .is_err());
        l.begin(Service::Kbo, Operation::KboEnterprise, policy(3))
            .unwrap()
            .finish(Some(200), None)
            .unwrap();
        l.begin(Service::Kbo, Operation::KboEnterprise, policy(3))
            .unwrap()
            .finish(None, None)
            .unwrap();
        let rows = l.summary().unwrap();
        assert_eq!(rows[0].attempts, 2);
        assert_eq!(rows[0].unknown, 0);
        assert_eq!(rows[0].transport_errors, 1);
        assert_eq!(rows[0].statuses[&200], 1);
    }
    /// The retry policy and the ledger have to agree on what a headerless `429`
    /// costs. When they did not, the client waited one second, the ledger refused
    /// the retry locally, and the operation failed on its second attempt — the bug
    /// this pins. Waiting *at least* as long as the cooldown is the invariant;
    /// waiting longer is only wasted time, but the reverse loses the retry.
    #[test]
    fn every_policy_waits_at_least_as_long_as_the_cooldown_it_will_meet() {
        for retry in [
            crate::http_retry::RetryPolicy::nbb(),
            crate::http_retry::RetryPolicy::bosa(),
            crate::http_retry::RetryPolicy::kbo(),
        ] {
            let l = ledger();
            let recorded_at = now_ms();
            l.begin(Service::Nbb, Operation::NbbBatchReferences, policy(10))
                .unwrap()
                // A 429 with no Retry-After: the case the ledger has to guess at.
                .finish(Some(429), None)
                .unwrap();
            let cooldown_until = l
                .read()
                .unwrap()
                .iter()
                .filter_map(|event| event.cooldown_until_ms)
                .max()
                .expect("a 429 records a cooldown");
            let enforced = Duration::from_millis((cooldown_until - recorded_at).max(0) as u64);
            // Follow the order `send_with_retry` uses: the policy decides what a
            // throttled response costs, then the budget check bounds the wait.
            let throttled = crate::http_retry::RetrySignal::Status {
                status: 429,
                retry_after: None,
            };
            let (_, retry_after) = retry
                .decide(&throttled)
                .expect("a 429 is retryable for every ledgered policy");
            let waited = crate::http_retry::next_wait(&retry, 1, retry_after, Duration::ZERO);
            let waited = match waited {
                crate::http_retry::Wait::Sleep(duration) => duration,
                other => panic!("a headerless 429 must be waited out, not refused: {other:?}"),
            };
            assert!(
                waited >= enforced,
                "{retry:?} waits {waited:?} but the ledger enforces {enforced:?}; the retry \
                 is refused locally and the operation fails on its second attempt"
            );
        }
    }
    #[test]
    fn cooldown_survives_new_instances_and_covers_all_nbb_products() {
        let l = ledger();
        l.begin(Service::Nbb, Operation::NbbReferences, policy(10))
            .unwrap()
            .finish(Some(429), Some("120"))
            .unwrap();
        let reopened = Ledger {
            directory: l.directory.clone(),
        };
        assert!(reopened
            .begin(Service::Nbb, Operation::NbbBatchReferences, policy(10))
            .is_err());
        assert!(reopened
            .begin(Service::Kbo, Operation::KboEnterprise, policy(10))
            .is_ok());
    }
    #[test]
    fn reordered_timestamps_fail_closed_before_network() {
        let l = ledger();
        l.begin(Service::Nbb, Operation::NbbReferences, policy(3))
            .unwrap()
            .finish(Some(200), None)
            .unwrap();
        let older = Event {
            version: 1,
            id: Uuid::new_v4(),
            timestamp_ms: now_ms() - 60_000,
            service: Service::Nbb,
            operation: Operation::NbbReferences,
            phase: Phase::Attempt,
            status: None,
            transport_error: false,
            cooldown_until_ms: None,
        };
        l.append(&older).unwrap();
        let result = http_with(l, policy(3), Service::Nbb, Operation::NbbReferences, || {
            panic!("corrupt history must block dispatch")
        });
        assert!(result.err().unwrap().to_string().contains("out-of-order"));
    }

    #[test]
    fn partial_or_invalid_history_fails_closed() {
        let l = ledger();
        l.prepare().unwrap();
        fs::write(l.directory.join("calls.jsonl"), b"partial").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                l.directory.join("calls.jsonl"),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        assert!(l
            .begin(Service::Nbb, Operation::NbbReferences, policy(10))
            .is_err());
    }
    #[test]
    fn simultaneous_callers_cannot_exceed_cap() {
        let l = ledger();
        let mut threads = Vec::new();
        for _ in 0..8 {
            let l = l.clone();
            threads.push(thread::spawn(move || {
                match l.begin(Service::Nbb, Operation::NbbReferences, policy(3)) {
                    Ok(t) => {
                        t.finish(Some(200), None).unwrap();
                        true
                    }
                    Err(_) => false,
                }
            }));
        }
        let count = threads
            .into_iter()
            .filter(|h| h.thread().id() != thread::current().id())
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(count, 3);
        assert_eq!(l.summary().unwrap()[0].attempts, 3);
    }
    #[test]
    fn independent_process_worker() {
        let Some(path) = std::env::var_os("SERVICE_AUDIT_TEST_CHILD_DIR") else {
            return;
        };
        let l = Ledger {
            directory: path.into(),
        };
        if let Ok(t) = l.begin(Service::Nbb, Operation::NbbReferences, policy(2)) {
            t.finish(Some(200), None).unwrap();
        }
    }
    #[test]
    fn processes_share_one_budget() {
        let l = ledger();
        let mut children = Vec::new();
        for _ in 0..5 {
            children.push(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "service_calls::tests::independent_process_worker",
                    ])
                    .env("SERVICE_AUDIT_TEST_CHILD_DIR", &l.directory)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(l.summary().unwrap()[0].attempts, 2);
    }
    #[test]
    fn ledger_has_no_request_data_fields_and_private_permissions() {
        let l = ledger();
        l.begin(Service::Nbb, Operation::NbbDocument, policy(1))
            .unwrap()
            .finish(Some(200), None)
            .unwrap();
        let data = fs::read_to_string(l.directory.join("calls.jsonl")).unwrap();
        for secret in [
            "https://",
            "Authorization",
            "Subscription-Key",
            "0537875391",
        ] {
            assert!(!data.contains(secret));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(l.directory.join("calls.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
