//! Read-only CBSO production client. Keys never enter URLs, logs or error bodies.
//! No automatic retries: callers decide whether and when to spend another request.
use chrono::{Datelike, NaiveDate, Utc};
use sha2::{Digest, Sha256};
use std::{fmt, io::Read, time::Duration};
use uuid::Uuid;

const ORIGIN: &str = "https://ws.cbso.nbb.be";
const USER_AGENT: &str = concat!("ehb-fin-aanbestedingen/", env!("CARGO_PKG_VERSION"));
const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Streamed batch archives go straight to disk; this bounds one daily ZIP.
pub const MAX_BATCH_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Streamed batch days can exceed 1 GB; bound stalls per read and the whole
/// transfer separately instead of the 120s total used for small responses.
const BATCH_READ_TIMEOUT: Duration = Duration::from_secs(60);
const BATCH_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    Authentic,
    Extracts,
    Archive,
}
impl Product {
    fn prefix(self) -> &'static str {
        match self {
            Self::Authentic => "NBB_AUTHENTIC_DATA",
            Self::Extracts => "NBB_EXTRACTS",
            Self::Archive => "NBB_AUTHENTIC_ARCHIVE_DATA",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySlot {
    Primary,
    Secondary,
}

#[derive(Clone)]
pub struct NbbCredentials(String);
impl fmt::Debug for NbbCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NbbCredentials([REDACTED])")
    }
}
impl NbbCredentials {
    pub fn from_env(product: Product, slot: KeySlot) -> Result<Self, NbbError> {
        let suffix = match slot {
            KeySlot::Primary => "PRIMARY_KEY",
            KeySlot::Secondary => "SECONDARY_KEY",
        };
        let name = format!("{}_{suffix}", product.prefix());
        let value = std::env::var(&name)
            .map_err(|_| NbbError::Configuration(format!("{name} is not set")))?;
        if value.is_empty()
            || value
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(NbbError::Configuration(format!(
                "{name} is blank or contains whitespace"
            )));
        }
        Ok(Self(value))
    }
}

#[derive(Debug)]
pub enum NbbError {
    Configuration(String),
    Transport,
    CallGuard(String),
    Http {
        status: u16,
        retry_after_seconds: Option<u64>,
    },
    Response(&'static str),
    /// The body did not arrive in full: a read failure, or fewer bytes than the
    /// server declared. Distinct from [`Self::Response`] because it is the one
    /// failure worth another attempt — a deliberate refusal (bad signature,
    /// oversized body) has to stay final.
    Incomplete(String),
    Io(String),
}
impl fmt::Display for NbbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(s) => write!(f, "NBB configuration: {s}"),
            Self::Transport => write!(f, "NBB transport failure"),
            Self::CallGuard(s) => write!(f, "{s}"),
            Self::Http {
                status,
                retry_after_seconds,
            } => write!(
                f,
                "NBB HTTP {status} (retry-after seconds: {retry_after_seconds:?})"
            ),
            Self::Response(s) => write!(f, "NBB invalid response: {s}"),
            Self::Incomplete(s) => write!(f, "NBB incomplete body: {s}"),
            Self::Io(s) => write!(f, "NBB capture file: {s}"),
        }
    }
}
impl std::error::Error for NbbError {}

/// The manifest schema owns this type; the client only parses into it.
pub use crate::nbb_bundle::FilingReference;

#[derive(Debug, Clone, Copy)]
pub enum Format {
    Pdf,
    Xbrl,
    JsonXbrl,
}
impl Format {
    fn media_type(self, batch: bool) -> &'static str {
        match (batch, self) {
            (false, Self::Pdf) => "application/pdf",
            (false, Self::Xbrl) => "application/x.xbrl",
            (false, Self::JsonXbrl) => "application/x.jsonxbrl",
            (true, Self::Pdf) => "application/x.zip+pdf",
            (true, Self::Xbrl) => "application/x.zip+xbrl",
            (true, Self::JsonXbrl) => "application/x.zip+jsonxbrl",
        }
    }
}

/// Metadata for a capture streamed directly to a create-only file.
pub struct FileCapture {
    pub bytes: u64,
    pub url: String,
    pub request_id: Uuid,
    pub media_type: String,
    pub sha256: String,
    pub retrieved_at: String,
}

/// Capture metadata is returned with bytes; no persistent store is changed by this client.
pub struct Capture {
    pub bytes: Vec<u8>,
    pub url: String,
    pub request_id: Uuid,
    pub media_type: String,
    pub sha256: String,
    pub retrieved_at: String,
}

pub struct NbbClient {
    product: Product,
    credentials: NbbCredentials,
    agent: ureq::Agent,
    batch_agent: ureq::Agent,
    origin: String,
    max_bytes: u64,
}
impl NbbClient {
    pub fn from_env(product: Product, slot: KeySlot) -> Result<Self, NbbError> {
        Ok(Self::new(product, NbbCredentials::from_env(product, slot)?))
    }
    pub fn new(product: Product, credentials: NbbCredentials) -> Self {
        Self {
            product,
            credentials,
            agent: ureq::AgentBuilder::new()
                // Disable pooled-connection resends: each HTTP send needs its own ledger reservation.
                .max_idle_connections(0)
                .timeout_connect(Duration::from_secs(10))
                .timeout(Duration::from_secs(120))
                .redirects(0)
                .user_agent(USER_AGENT)
                .build(),
            batch_agent: ureq::AgentBuilder::new()
                .max_idle_connections(0)
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(BATCH_READ_TIMEOUT)
                .timeout(BATCH_TOTAL_TIMEOUT)
                .redirects(0)
                .user_agent(USER_AGENT)
                .build(),
            origin: ORIGIN.into(),
            max_bytes: MAX_BYTES,
        }
    }

    pub fn references(&self, enterprise: &str) -> Result<Vec<FilingReference>, NbbError> {
        self.require(Product::Authentic)?;
        if enterprise.len() != 10 || !enterprise.bytes().all(|b| b.is_ascii_digit()) {
            return Err(NbbError::Configuration(
                "enterprise must be 10 digits".into(),
            ));
        }
        let capture = self.get(
            &format!("/authentic/legalEntity/{enterprise}/references"),
            "application/json",
        )?;
        let references: Vec<FilingReference> = serde_json::from_slice(&capture.bytes)
            .map_err(|_| NbbError::Response("expected an array of filing references"))?;
        let expected = crate::kbo::normalize_enterprise_number(enterprise)
            .map_err(|_| NbbError::Configuration("invalid enterprise number".into()))?;
        for reference in &references {
            validate_reference(&reference.reference_number)?;
            if crate::kbo::normalize_enterprise_number(&reference.enterprise_number)
                .ok()
                .as_ref()
                != Some(&expected)
            {
                return Err(NbbError::Response("enterprise identity mismatch"));
            }
        }
        Ok(references)
    }

    pub fn reference(&self, reference: &str) -> Result<FilingReference, NbbError> {
        self.require(Product::Authentic)?;
        validate_reference(reference)?;
        let capture = self.get(
            &format!("/authentic/deposit/{reference}/reference"),
            "application/json",
        )?;
        let result: FilingReference = serde_json::from_slice(&capture.bytes)
            .map_err(|_| NbbError::Response("expected a filing reference"))?;
        if result.reference_number != reference {
            return Err(NbbError::Response("filing reference identity mismatch"));
        }
        Ok(result)
    }

    pub fn accounting_data(&self, reference: &str, format: Format) -> Result<Capture, NbbError> {
        self.require(Product::Authentic)?;
        validate_reference(reference)?;
        self.get(
            &format!("/authentic/deposit/{reference}/accountingData"),
            format.media_type(false),
        )
    }

    pub fn batch_references(&self, date: NaiveDate) -> Result<Capture, NbbError> {
        self.batch(date, "references", "application/x.zip+json")
    }
    pub fn batch_accounting_data(
        &self,
        date: NaiveDate,
        format: Format,
    ) -> Result<Capture, NbbError> {
        self.batch(date, "accountingData", format.media_type(true))
    }
    /// Stream a daily accounting-data ZIP to `path` (create-only, 0600). Large
    /// days exceed the in-memory cap; this bounds the file at `MAX_BATCH_FILE_BYTES`.
    pub fn batch_accounting_data_to_file(
        &self,
        date: NaiveDate,
        format: Format,
        path: &std::path::Path,
    ) -> Result<FileCapture, NbbError> {
        let route = self.batch_path(date, "accountingData")?;
        self.get_to_file(&route, format.media_type(true), path, MAX_BATCH_FILE_BYTES)
    }
    /// Stream a daily references ZIP to `path`; same guarantees as filings.
    pub fn batch_references_to_file(
        &self,
        date: NaiveDate,
        path: &std::path::Path,
    ) -> Result<FileCapture, NbbError> {
        let route = self.batch_path(date, "references")?;
        self.get_to_file(&route, "application/x.zip+json", path, MAX_BATCH_FILE_BYTES)
    }
    fn batch(&self, date: NaiveDate, operation: &str, media: &str) -> Result<Capture, NbbError> {
        let path = self.batch_path(date, operation)?;
        self.get(&path, media)
    }
    fn batch_path(&self, date: NaiveDate, operation: &str) -> Result<String, NbbError> {
        // UTC day is conservative: near Belgian midnight this may also refuse
        // yesterday until 01:00/02:00 local. It never admits today in Belgium.
        if date >= Utc::now().date_naive() || date.year() < 1978 {
            return Err(NbbError::Configuration(
                "batch date must be a past Belgian publication date (1978 or later)".into(),
            ));
        }
        let path = match self.product {
            Product::Extracts => format!("/extracts/batch/{date}/{operation}"),
            Product::Archive => format!("/authentic-archive/{date}/{operation}"),
            Product::Authentic => {
                return Err(NbbError::Configuration(
                    "batch operations require Extracts or Archive credentials".into(),
                ))
            }
        };
        Ok(path)
    }
    fn require(&self, product: Product) -> Result<(), NbbError> {
        if self.product == product {
            Ok(())
        } else {
            Err(NbbError::Configuration(
                "operation requires Authentic Data credentials".into(),
            ))
        }
    }
    fn get(&self, path: &str, accept: &str) -> Result<Capture, NbbError> {
        with_incomplete_retry(|| self.get_once(path, accept))
    }
    fn get_once(&self, path: &str, accept: &str) -> Result<Capture, NbbError> {
        let (response, url, request_id, media_type) = self.send_with(&self.agent, path, accept)?;
        let declared = declared_length(&response);
        // Refuse a body the server already says is too large, before reading it.
        if declared.is_some_and(|length| length > self.max_bytes) {
            return Err(NbbError::Response("declared body exceeds 64 MiB limit"));
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(self.max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| NbbError::Incomplete("body read failed".into()))?;
        if bytes.len() as u64 > self.max_bytes {
            return Err(NbbError::Response("body exceeds 64 MiB limit"));
        }
        check_declared_length(declared, bytes.len() as u64)?;
        if bytes.is_empty()
            || (accept.contains("zip")
                && !bytes.starts_with(b"PK\x03\x04")
                && !bytes.starts_with(b"PK\x05\x06"))
        {
            return Err(NbbError::Response("empty body or invalid ZIP signature"));
        }
        Ok(Capture {
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes,
            url,
            request_id,
            media_type,
            retrieved_at: Utc::now().to_rfc3339(),
        })
    }
    fn get_to_file(
        &self,
        route: &str,
        accept: &str,
        path: &std::path::Path,
        limit: u64,
    ) -> Result<FileCapture, NbbError> {
        // Refuse before spending a request: create-only, no symlinks, parent must exist.
        if path.symlink_metadata().is_ok() {
            return Err(NbbError::Io(
                "output already exists; refusing to overwrite".into(),
            ));
        }
        // The partial file is removed on failure inside, so a retried attempt
        // starts from an absent path rather than inheriting half a write.
        with_incomplete_retry(|| self.capture_to_file(route, accept, path, limit))
    }
    fn capture_to_file(
        &self,
        route: &str,
        accept: &str,
        path: &std::path::Path,
        limit: u64,
    ) -> Result<FileCapture, NbbError> {
        use std::io::Write;
        let (response, url, request_id, media_type) =
            self.send_with(&self.batch_agent, route, accept)?;
        let declared = declared_length(&response);
        if declared.is_some_and(|length| length > limit) {
            return Err(NbbError::Response("declared body exceeds batch file limit"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|e| NbbError::Io(e.to_string()))?;
        let result = (|| {
            let mut reader = response.into_reader().take(limit + 1);
            let mut hasher = Sha256::new();
            let mut total = 0u64;
            let mut head = Vec::with_capacity(4);
            let mut buffer = vec![0u8; 1 << 20];
            loop {
                let read = reader
                    .read(&mut buffer)
                    .map_err(|_| NbbError::Incomplete("body read failed".into()))?;
                if read == 0 {
                    break;
                }
                total += read as u64;
                if total > limit {
                    return Err(NbbError::Response("body exceeds batch file limit"));
                }
                if head.len() < 4 {
                    head.extend_from_slice(&buffer[..read.min(4 - head.len())]);
                }
                hasher.update(&buffer[..read]);
                file.write_all(&buffer[..read])
                    .map_err(|e| NbbError::Io(e.to_string()))?;
            }
            if total == 0 || (head != b"PK\x03\x04" && head != b"PK\x05\x06") {
                return Err(NbbError::Response("empty body or invalid ZIP signature"));
            }
            check_declared_length(declared, total)?;
            file.sync_all().map_err(|e| NbbError::Io(e.to_string()))?;
            Ok(FileCapture {
                bytes: total,
                url: url.clone(),
                request_id,
                media_type: media_type.clone(),
                sha256: format!("{:x}", hasher.finalize()),
                retrieved_at: Utc::now().to_rfc3339(),
            })
        })();
        drop(file);
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    }
    fn send_with(
        &self,
        agent: &ureq::Agent,
        path: &str,
        accept: &str,
    ) -> Result<(ureq::Response, String, Uuid, String), NbbError> {
        let url = format!("{}{path}", self.origin);
        use crate::service_calls::{Operation, Service};
        let operation = if path.contains("/legalEntity/") {
            Operation::NbbReferences
        } else if path.ends_with("/reference") {
            Operation::NbbReference
        } else if path.starts_with("/authentic/deposit/") {
            Operation::NbbDocument
        } else if path.ends_with("/references") {
            Operation::NbbBatchReferences
        } else {
            Operation::NbbBatchDocuments
        };
        // Each attempt is a new HTTP request, so it carries its own
        // X-Request-Id: the id identifies the exchange the provider can look up,
        // and a retried request is a different exchange. The capture keeps the
        // id of the attempt that produced the bytes.
        let mut request_id = Uuid::new_v4();
        let response = crate::http_retry::send_with_retry(
            &crate::http_retry::RetryPolicy::nbb(),
            || {
                let attempt_id = Uuid::new_v4();
                request_id = attempt_id;
                crate::service_calls::http(Service::Nbb, operation, || {
                    agent
                        .get(&url)
                        .set("NBB-CBSO-Subscription-Key", &self.credentials.0)
                        .set("X-Request-Id", &attempt_id.to_string())
                        .set("Accept", accept)
                        .call()
                })
                .map_err(|e| NbbError::CallGuard(e.to_string()))
                .and_then(|result| match result {
                    Ok(response) => Ok(response),
                    Err(ureq::Error::Status(status, response)) => Err(NbbError::Http {
                        status,
                        // Same parser as the ledger and the retry driver: seconds or
                        // an HTTP-date, never two readings of one header.
                        retry_after_seconds: crate::http_retry::retry_after_seconds(
                            response.header("Retry-After"),
                        ),
                    }),
                    Err(_) => Err(NbbError::Transport),
                })
            },
            retry_signal,
        )
        .map_err(|stop| stop.into_error())?;
        if response.status() != 200 {
            return Err(NbbError::Http {
                status: response.status(),
                retry_after_seconds: None,
            });
        }
        let media_type = response
            .header("Content-Type")
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if media_type != accept {
            return Err(NbbError::Response("unexpected Content-Type"));
        }
        Ok((response, url, request_id, media_type))
    }
}
/// Map an NBB failure onto the shared retry vocabulary.
///
/// A call-guard refusal (daily cap, active server cooldown, ledger I/O) is
/// final: repeating it only re-reads the same ledger state, and an operator has
/// to see it. A redirect refusal and a 4xx stay final for the same reason the
/// endpoint keeps refusing them.
fn retry_signal(error: &NbbError) -> crate::http_retry::RetrySignal {
    use crate::http_retry::RetrySignal;
    match error {
        NbbError::Http {
            status,
            retry_after_seconds,
        } => RetrySignal::Status {
            status: *status,
            retry_after: *retry_after_seconds,
        },
        NbbError::Transport => RetrySignal::Transport,
        _ => RetrySignal::Final,
    }
}

/// Retries exactly once when the body did not arrive in full.
///
/// Deliberately narrower than the request retry policy in [`crate::http_retry`]:
/// that one decides whether to *dispatch* again, this one decides whether to
/// *read* again, and only for the failure a second read can fix — an archive
/// that dies at 90% is worth one more attempt, while a deterministic truncation
/// must not become a loop. A refusal (bad signature, oversized body, wrong media
/// type) is returned untouched.
///
/// The two layers compose, so the bound is the product: at most two dispatch
/// ladders of three attempts each (six requests, about six minutes of backoff)
/// for one file in the worst case, and every one of those attempts is a charged
/// ledger reservation. That is accepted rather than restructured: a body retry
/// that skipped the dispatch ladder would re-send on the exact transport failure
/// the ladder exists to absorb.
fn with_incomplete_retry<T>(
    mut attempt: impl FnMut() -> Result<T, NbbError>,
) -> Result<T, NbbError> {
    match attempt() {
        Err(NbbError::Incomplete(_)) => attempt(),
        other => other,
    }
}

/// The body length the server declared, when it declared one.
///
/// This is an integrity guard, not a size hint: a stream that ends before the
/// declared length is a truncated capture, and writing it out would make the
/// receipt attest the truncation. The season script validates later runs against
/// exactly that receipt, so a truncated archive would then be accepted as
/// complete for ever.
fn declared_length(response: &ureq::Response) -> Option<u64> {
    response.header("Content-Length")?.trim().parse().ok()
}

fn check_declared_length(declared: Option<u64>, received: u64) -> Result<(), NbbError> {
    match declared {
        // A chunked response declares nothing, so there is nothing to compare.
        Some(declared) if declared != received => Err(NbbError::Incomplete(format!(
            "received {received} of {declared} declared bytes"
        ))),
        _ => Ok(()),
    }
}

fn validate_reference(reference: &str) -> Result<(), NbbError> {
    if reference.is_empty()
        || reference.len() > 64
        || !reference.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    {
        return Err(NbbError::Configuration(
            "reference must contain only digits and hyphens (1..64 bytes)".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn serve(status: &str, headers: &str, body: &[u8]) -> (String, thread::JoinHandle<String>) {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", socket.local_addr().unwrap());
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        );
        let body = body.to_vec();
        let handle = thread::spawn(move || {
            let (mut stream, _) = socket.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut b = [0u8; 1];
                stream.read_exact(&mut b).unwrap();
                bytes.push(b[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(reply.as_bytes()).unwrap();
            let _ = stream.write_all(&body);
            String::from_utf8(bytes).unwrap()
        });
        (origin, handle)
    }
    /// Serves one response per entry, in order, and reports how many requests
    /// arrived: a retry test needs proof of the attempt count, not just a result.
    fn serve_sequence(
        replies: Vec<(&'static str, &'static str, &'static [u8])>,
    ) -> (String, thread::JoinHandle<usize>) {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", socket.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let mut served = 0;
            for (status, headers, body) in replies {
                let (mut stream, _) = socket.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut b = [0u8; 1];
                    stream.read_exact(&mut b).unwrap();
                    bytes.push(b[0]);
                    if bytes.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                served += 1;
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                    body.len()
                )
                .unwrap();
                let _ = stream.write_all(body);
            }
            served
        });
        (origin, handle)
    }
    /// Serves `times` replies that declare `declared` bytes but send only `body`,
    /// so a stream can be cut short without the connection reporting an error.
    fn serve_truncated(
        declared: usize,
        content_type: &'static str,
        body: &'static [u8],
        times: usize,
    ) -> (String, thread::JoinHandle<usize>) {
        use std::io::Write;
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", socket.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let mut served = 0;
            for _ in 0..times {
                let (mut stream, _) = socket.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut b = [0u8; 1];
                    stream.read_exact(&mut b).unwrap();
                    bytes.push(b[0]);
                    if bytes.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                served += 1;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                     Content-Length: {declared}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                let _ = stream.write_all(body);
            }
            served
        });
        (origin, handle)
    }
    /// A body that ends before its declared length is a truncated capture. It used
    /// to be written out as if complete, which made the receipt attest the
    /// truncation and the season script's hash check accept it for ever.
    #[test]
    fn a_short_body_is_never_reported_as_a_complete_capture() {
        let (url, server) =
            serve_truncated(4096, "application/json", b"PK\x03\x04only-part-of-it", 2);
        let error = client(url, Product::Authentic)
            .references("0537875391")
            .unwrap_err();
        // ureq enforces `Content-Length` itself, so the short read surfaces as a
        // read failure rather than as a byte-count mismatch; both are incomplete.
        assert!(
            matches!(&error, NbbError::Incomplete(_)),
            "a truncated body must not be accepted: {error}"
        );
        assert_eq!(
            server.join().unwrap(),
            2,
            "an incomplete body is read again exactly once"
        );
    }

    /// The retry is what turns a connection that died mid-body into a capture
    /// instead of an operator re-running a 300-file season fetch by hand.
    #[test]
    fn a_truncated_body_is_retried_once_and_can_then_complete() {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", socket.local_addr().unwrap());
        let body = br#"[
            {"ReferenceNumber":"2025-00000001","EnterpriseNumber":"0537875391"}]"#;
        let second = body.to_vec();
        let server = thread::spawn(move || {
            use std::io::Write;
            for attempt in 0..2 {
                let (mut stream, _) = socket.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut b = [0u8; 1];
                    stream.read_exact(&mut b).unwrap();
                    bytes.push(b[0]);
                    if bytes.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                // First attempt declares the full length and sends half of it.
                let (declared, payload) = if attempt == 0 {
                    (body.len(), &body[..body.len() / 2])
                } else {
                    (second.len(), second.as_slice())
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {declared}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                let _ = stream.write_all(payload);
            }
        });
        let refs = client(origin, Product::Authentic)
            .references("0537875391")
            .expect("the retried read completes the body");
        assert_eq!(refs[0].reference_number, "2025-00000001");
        server.join().unwrap();
    }

    /// The declared-length check is defence in depth: ureq already errors on a
    /// `Content-Length` short read, but a body capped by `take()` or a server that
    /// closes cleanly must not be mistaken for a smaller complete capture.
    #[test]
    fn a_body_shorter_than_its_declared_length_is_incomplete() {
        assert!(check_declared_length(Some(10), 10).is_ok());
        assert!(matches!(
            check_declared_length(Some(10), 4),
            Err(NbbError::Incomplete(reason)) if reason.contains("4 of 10")
        ));
        assert!(
            check_declared_length(None, 4).is_ok(),
            "a chunked body declares nothing to compare against"
        );
    }

    /// A body the server already declares as oversized is refused before it is
    /// read, and a refusal is never retried.
    #[test]
    fn a_declared_oversized_body_is_refused_without_being_retried() {
        let (url, server) = serve_truncated(4096, "application/json", b"PK\x03\x04", 1);
        let mut c = client(url, Product::Authentic);
        c.max_bytes = 100;
        let error = c.references("0537875391").unwrap_err();
        assert!(
            matches!(error, NbbError::Response(message) if message.contains("declared body")),
            "got: {error}"
        );
        assert_eq!(server.join().unwrap(), 1, "a refusal is final");
    }

    fn client(origin: String, product: Product) -> NbbClient {
        let mut client = NbbClient::new(product, NbbCredentials("test-secret".into()));
        client.origin = origin;
        client
    }
    #[test]
    fn key_is_redacted() {
        assert!(!format!("{:?}", NbbCredentials("test-secret".into())).contains("test-secret"));
    }
    #[test]
    fn validates_paths_and_products_before_network() {
        let c = client("http://invalid.invalid".into(), Product::Authentic);
        assert!(c.references("../bad").is_err());
        assert!(c.accounting_data("../bad", Format::Xbrl).is_err());
        assert!(c.batch_references(Utc::now().date_naive()).is_err());
        assert!(client("http://invalid.invalid".into(), Product::Archive)
            .references("0537875391")
            .is_err());
    }
    #[test]
    fn authentic_headers_and_reference_schema() {
        let (url, server) = serve(
            "200 OK",
            "Content-Type: application/json\r\n",
            br#"[{"ReferenceNumber":"2025-00000001","EnterpriseNumber":"0537875391"}]"#,
        );
        let refs = client(url, Product::Authentic)
            .references("0537875391")
            .unwrap();
        assert_eq!(refs[0].reference_number, "2025-00000001");
        let req = server.join().unwrap();
        let lower = req.to_lowercase();
        assert!(req.starts_with("GET /authentic/legalEntity/0537875391/references HTTP/1.1"));
        assert!(!req.lines().next().unwrap().contains("test-secret"));
        assert!(lower.contains("nbb-cbso-subscription-key: test-secret"));
        assert!(lower.contains("user-agent: ehb-fin-aanbestedingen/"));
        let id = lower
            .lines()
            .find_map(|l| l.strip_prefix("x-request-id: "))
            .unwrap();
        assert_eq!(Uuid::parse_str(id).unwrap().get_version_num(), 4);
    }
    #[test]
    fn rejects_redirects_without_following_and_never_echoes_body() {
        let (url, server) = serve(
            "302 Found",
            "Location: http://invalid.invalid/\r\n",
            b"test-secret",
        );
        let err = client(url, Product::Authentic)
            .references("0537875391")
            .unwrap_err();
        assert!(matches!(err, NbbError::Http { status: 302, .. }));
        assert!(!err.to_string().contains("test-secret"));
        server.join().unwrap();
    }
    #[test]
    fn retries_a_throttled_request_after_the_server_cooldown() {
        let (url, server) = serve_sequence(vec![
            ("429 Too Many Requests", "Retry-After: 1\r\n", b""),
            (
                "200 OK",
                "Content-Type: application/json\r\n",
                br#"[{"ReferenceNumber":"2025-00000001","EnterpriseNumber":"0537875391"}]"#,
            ),
        ]);
        let refs = client(url, Product::Authentic)
            .references("0537875391")
            .unwrap();
        assert_eq!(refs[0].reference_number, "2025-00000001");
        assert_eq!(
            server.join().unwrap(),
            2,
            "the throttled request must be retried once the cooldown elapsed"
        );
        assert_eq!(
            crate::service_calls::test_ledger().summary().unwrap()[0].attempts,
            2,
            "the retry is a charged attempt, not a hidden extra request"
        );
    }
    /// A cooldown longer than the retry budget stops immediately: shortening it
    /// into an early retry would only earn another refusal, and the ledger would
    /// block it anyway.
    #[test]
    fn gives_up_when_the_server_cooldown_outlasts_the_retry_budget() {
        let (url, server) = serve_sequence(vec![(
            "429 Too Many Requests",
            "Retry-After: 700\r\n",
            b"secret diagnostics",
        )]);
        let started = std::time::Instant::now();
        let error = client(url.clone(), Product::Authentic)
            .references("0537875391")
            .unwrap_err();
        assert!(matches!(
            error,
            NbbError::Http {
                status: 429,
                retry_after_seconds: Some(700)
            }
        ));
        assert!(!error.to_string().contains("secret diagnostics"));
        assert_eq!(server.join().unwrap(), 1, "no second request is sent");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a 700s cooldown must fail fast, not be slept"
        );
        // The refusal is recorded, so the next caller is blocked until it passes.
        assert!(matches!(
            client(url, Product::Extracts)
                .batch_references(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap()),
            Err(NbbError::CallGuard(_))
        ));
    }
    #[test]
    fn rejects_wrong_media_and_oversized_body_and_bad_json() {
        for (media, body, limit) in [
            ("text/html", b"{}".as_slice(), 20),
            ("application/json", b"123456".as_slice(), 3),
            ("application/json", b"{}".as_slice(), 20),
        ] {
            let (url, server) = serve("200 OK", &format!("Content-Type: {media}\r\n"), body);
            let mut c = client(url, Product::Authentic);
            c.max_bytes = limit;
            assert!(c.references("0537875391").is_err());
            server.join().unwrap();
        }
    }
    #[test]
    fn rejects_misattributed_references_and_wrong_filing() {
        let (url, server) = serve(
            "200 OK",
            "Content-Type: application/json\r\n",
            br#"[{"ReferenceNumber":"2025-1","EnterpriseNumber":"0202239951"}]"#,
        );
        assert!(matches!(
            client(url, Product::Authentic).references("0537875391"),
            Err(NbbError::Response("enterprise identity mismatch"))
        ));
        server.join().unwrap();
        let (url, server) = serve(
            "200 OK",
            "Content-Type: application/json\r\n",
            br#"{"ReferenceNumber":"2025-2","EnterpriseNumber":"0537875391"}"#,
        );
        assert!(matches!(
            client(url, Product::Authentic).reference("2025-1"),
            Err(NbbError::Response("filing reference identity mismatch"))
        ));
        server.join().unwrap();
    }

    #[test]
    fn filing_routes_never_follow_provider_urls_and_select_format() {
        for (format, media) in [
            (Format::Pdf, "application/pdf"),
            (Format::Xbrl, "application/x.xbrl"),
            (Format::JsonXbrl, "application/x.jsonxbrl"),
        ] {
            let (url, server) = serve("200 OK", &format!("Content-Type: {media}\r\n"), b"fixture");
            let result = client(url, Product::Authentic)
                .accounting_data("2025-1", format)
                .unwrap();
            assert_eq!(result.media_type, media);
            let request = server.join().unwrap();
            assert!(request.starts_with("GET /authentic/deposit/2025-1/accountingData HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains(&format!("accept: {media}")));
        }
    }

    #[test]
    fn streamed_batch_writes_create_only_file_with_hash_and_removes_partial() {
        let dir = std::env::temp_dir().join(format!("nbb-stream-{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let body = b"PK\x03\x04streamed-archive-bytes";
        let (url, server) = serve("200 OK", "Content-Type: application/x.zip+xbrl\r\n", body);
        let out = dir.join("day.zip");
        let capture = client(url, Product::Extracts)
            .batch_accounting_data_to_file(
                NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(),
                Format::Xbrl,
                &out,
            )
            .unwrap();
        assert!(server
            .join()
            .unwrap()
            .starts_with("GET /extracts/batch/2026-01-05/accountingData HTTP/1.1"));
        assert_eq!(std::fs::read(&out).unwrap(), body);
        assert_eq!(capture.bytes, body.len() as u64);
        assert_eq!(capture.sha256, format!("{:x}", Sha256::digest(body)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Existing output is refused before any request is sent.
        assert!(matches!(
            client("http://invalid.invalid".into(), Product::Extracts)
                .batch_accounting_data_to_file(
                    NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(),
                    Format::Xbrl,
                    &out
                ),
            Err(NbbError::Io(_))
        ));
        // A non-ZIP body leaves no partial file behind.
        let (url, server) = serve(
            "200 OK",
            "Content-Type: application/x.zip+xbrl\r\n",
            b"not a zip",
        );
        let bad = dir.join("bad.zip");
        assert!(client(url, Product::Extracts)
            .batch_accounting_data_to_file(
                NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(),
                Format::Xbrl,
                &bad
            )
            .is_err());
        server.join().unwrap();
        assert!(bad.symlink_metadata().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn batch_routes_and_capture_hash() {
        for (product, path) in [
            (Product::Extracts, "/extracts/batch/2026-01-05/references"),
            (Product::Archive, "/authentic-archive/2026-01-05/references"),
        ] {
            let (url, server) = serve(
                "200 OK",
                "Content-Type: application/x.zip+json\r\n",
                b"PK\x05\x06",
            );
            let result = client(url, product)
                .batch_references(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap())
                .unwrap();
            assert_eq!(
                result.sha256,
                format!("{:x}", Sha256::digest(&result.bytes))
            );
            assert!(server
                .join()
                .unwrap()
                .starts_with(&format!("GET {path} HTTP/1.1")));
        }
    }
}
