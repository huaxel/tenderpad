use crate::analysis::{AnalysisDecision, MAX_ANALYSIS_DECISION_BYTES};
use crate::recommendations::CorpusEvidence;
use crate::{
    validate_value_limits, AwardLot, AwardRecord, CompanyPreferences, Tender, TenderRequirement,
};

/// Allowed `feedback.outcome` labels (`bid|no-bid|won|lost`). Validated in one
/// place; the CLI, the API, and the store all reject anything else.
pub const VALID_FEEDBACK_OUTCOMES: &[&str] = &["bid", "no-bid", "won", "lost"];

/// Maximum filing rows returned in the recommendation API's NBB shadow projection.
pub const NBB_EVIDENCE_PROJECTION_LIMIT: usize = 100;

/// Allowed `audit_log.action` values. Reads are deliberately never logged.
pub const AUDIT_FEEDBACK_SAVE: &str = "feedback.save";
pub const AUDIT_PREFERENCES_SAVE: &str = "preferences.save";
pub const AUDIT_MATTER_CLOSE: &str = "matter.close";

pub fn validate_feedback_outcome(value: &str) -> Result<(), String> {
    if VALID_FEEDBACK_OUTCOMES.contains(&value) {
        Ok(())
    } else {
        Err("outcome must be one of bid, no-bid, won, lost".to_owned())
    }
}
use chrono::{Duration, NaiveDate};
use duckdb::{params, Config, Connection, Row};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::path::Path;
use std::thread;
use std::time::Duration as StdDuration;

// DuckDB validates writes from separate connections optimistically at commit
// time, so concurrent API requests (or an ingest job in another process) can
// lose a race. Upserts are idempotent, so retrying the same statement is safe:
// after the winner commits, the retry takes the DO UPDATE path.
const WRITE_ATTEMPTS: u32 = 6;
const WRITE_RETRY_BASE_DELAY: StdDuration = StdDuration::from_millis(50);
// A separate long-running ingest process holds the write lock for its whole
// run, so opening for write may have to wait briefly as well.
pub(crate) const OPEN_LOCK_ATTEMPTS: u32 = 24;
pub(crate) const OPEN_LOCK_DELAY: StdDuration = StdDuration::from_millis(250);

pub(crate) fn is_retryable_write_error(error: &duckdb::Error) -> bool {
    let duckdb::Error::DuckDBFailure(_, Some(message)) = error else {
        return false;
    };
    let lowered = message.to_lowercase();
    // Cross-connection write conflicts surface at commit time; upserts cannot
    // produce them single-threaded. Data errors such as NOT NULL or CHECK
    // violations are worded differently and are not retried.
    lowered.contains("conflict on update")
        || lowered.contains("failed to commit")
        || lowered.contains("duplicate key")
        || lowered.contains("database is locked")
        || lowered.contains("could not set lock")
        // A read-only and a write connection on the same file cannot coexist
        // within one process; the conflict clears when either closes.
        || lowered.contains("different configuration")
}

fn boxed_error_is_retryable(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<duckdb::Error>()
        .is_some_and(is_retryable_write_error)
}

fn write_retry_delay(attempt: u32) -> StdDuration {
    WRITE_RETRY_BASE_DELAY.saturating_mul(attempt + 1)
        + StdDuration::from_millis(10 * u64::from(attempt % 3))
}

/// Run one idempotent write, retrying cross-connection conflicts.
fn retry_write<T>(
    mut operation: impl FnMut() -> Result<T, duckdb::Error>,
) -> Result<T, duckdb::Error> {
    for attempt in 0..WRITE_ATTEMPTS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if attempt + 1 < WRITE_ATTEMPTS && is_retryable_write_error(&error) => {
                thread::sleep(write_retry_delay(attempt));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the retry loop returns from inside its body")
}

fn open_write_connection(path: &Path) -> Result<Connection, duckdb::Error> {
    for attempt in 0..OPEN_LOCK_ATTEMPTS {
        match Connection::open(path) {
            Ok(connection) => return Ok(connection),
            Err(error) if attempt + 1 < OPEN_LOCK_ATTEMPTS && is_retryable_write_error(&error) => {
                thread::sleep(OPEN_LOCK_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the lock retry loop returns from inside its body")
}

fn json_decimal_values(value: &str) -> Result<Vec<Decimal>, Box<dyn Error>> {
    Ok(serde_json::from_str::<Vec<serde_json::Value>>(value)?
        .into_iter()
        .filter_map(|item| match item {
            serde_json::Value::Number(number) => number.to_string().parse().ok(),
            serde_json::Value::String(text) => text.parse().ok(),
            _ => None,
        })
        .collect())
}

fn sql_limit(limit: usize) -> Result<i64, Box<dyn Error>> {
    i64::try_from(limit).map_err(|_| "tender limit is too large".into())
}

// Version headers have a separate budget from the bounded payload projection.
// Exhausting it is an explicit completeness failure, never a shortened ladder.
const MAX_NOTICE_IDENTITY_BYTES: usize = 64 * 1024 * 1024;

const MAX_PERSISTENT_ANALYSIS_CACHE_ENTRIES: i64 = 10_000;
const MAX_PERSISTENT_ANALYSIS_CACHE_BYTES: i64 = 64 * 1024 * 1024;
// DuckDB permits disjoint inserts from separate in-process connections. Without
// serialization two transactions can each see a below-budget snapshot yet
// commit an above-budget combined cache. Cross-process writers are excluded
// by DuckDB's file lock; this mutex covers saves through this process.
static ANALYSIS_CACHE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// NBB Central Balance Sheet Office financials, one row per (enterprise, fiscal
/// year, filing). Manual Consult downloads seed it (`manual-consult`); the API
/// backfills the same shape later (`api-authentic-query` / `-extract`). The
/// acquisition method is part of the key discipline: a row always says how it
/// got here, so a future API re-ingest can supersede manual rows explicitly
/// instead of silently duplicating them.
#[derive(Debug, Clone, PartialEq)]
pub struct NbbFinancials {
    pub enterprise_kbo: String,
    pub fiscal_year: i64,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub turnover: Option<Decimal>,
    pub staff_fte: Option<Decimal>,
    pub equity: Option<Decimal>,
    pub result: Option<Decimal>,
    pub source_ref: String,
    pub source_format: String,
    pub acquisition_method: String,
}

/// Read projection for exposing stored NBB filings with acquisition provenance.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct NbbFinancialEvidence {
    pub enterprise_kbo: String,
    pub fiscal_year: i64,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub turnover: Option<Decimal>,
    pub staff_fte: Option<Decimal>,
    pub equity: Option<Decimal>,
    pub result: Option<Decimal>,
    pub source_ref: String,
    pub source_format: String,
    pub acquisition_method: String,
    pub stored_at: String,
}

/// Coverage state for the optional NBB filing lookup. Empty rows do not claim
/// that the company has no filings at the source.
#[derive(Debug, Clone, PartialEq)]
pub struct NbbFinancialEvidenceLookup {
    pub coverage: &'static str,
    pub filings: Vec<NbbFinancialEvidence>,
    /// True when the projection omits older rows; summaries then cover only `filings`.
    pub truncated: bool,
}

const SCHEMA: &str = r#"
-- Shared id sequence for run/provenance tables, first: table defaults below
-- reference it at creation time.
CREATE SEQUENCE IF NOT EXISTS refresh_runs_id START 1;
CREATE TABLE IF NOT EXISTS tenders (
    source VARCHAR NOT NULL,
    external_id VARCHAR NOT NULL,
    title VARCHAR NOT NULL,
    buyer VARCHAR,
    description VARCHAR,
    cpv_codes JSON,
    regions JSON,
    estimated_value DECIMAL(18, 2),
    lot_values JSON,
    deadline DATE,
    languages JSON,
    notice_type VARCHAR,
    publication_date DATE,
    requirements JSON,
    document_urls JSON,
    raw JSON,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (source, external_id)
);

CREATE TABLE IF NOT EXISTS awards (
    enterprise_number VARCHAR NOT NULL,
    publication_number VARCHAR NOT NULL,
    title VARCHAR,
    buyer VARCHAR,
    winner_identifiers JSON,
    winner_names JSON,
    cpv_codes JSON,
    values JSON,
    links JSON,
    raw JSON,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (enterprise_number, publication_number)
);

CREATE TABLE IF NOT EXISTS bda_awards (
    source_name VARCHAR NOT NULL,
    notice_ref VARCHAR,
    winner_name VARCHAR NOT NULL,
    winner_town VARCHAR,
    winner_postcode VARCHAR,
    winner_country VARCHAR,
    value DECIMAL(18, 2),
    currency VARCHAR NOT NULL,
    date_conclusion DATE,
    tenders_received INTEGER,
    cpv VARCHAR,
    buyer VARCHAR,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (source_name, winner_name)
);

CREATE TABLE IF NOT EXISTS ted_awards_history (
    year INTEGER NOT NULL,
    notice_url VARCHAR,
    buyer_name VARCHAR,
    winner_raw VARCHAR NOT NULL,
    winner_member VARCHAR,
    winner_enterprise VARCHAR,
    join_confidence VARCHAR NOT NULL,
    award_value DECIMAL(18, 2),
    cpv VARCHAR,
    dt_award VARCHAR,
    sme VARCHAR,
    number_offers INTEGER,
    ambiguous BOOLEAN NOT NULL DEFAULT false,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp
);

-- Named tenderers on BOSA award lots that settled a contract. `outcome` is
-- 'won' or 'not-awarded'; tenderers are participants, not a complete bidder
-- list unless the notice names every tender. Replaced per publication.
CREATE TABLE IF NOT EXISTS bosa_lot_bidders (
    publication_number VARCHAR NOT NULL,
    dossier_id VARCHAR,
    lot_id VARCHAR NOT NULL,
    organization_reference VARCHAR NOT NULL,
    enterprise_number VARCHAR,
    raw_identifier VARCHAR,
    name VARCHAR,
    tendering_party_reference VARCHAR,
    group_lead BOOLEAN,
    outcome VARCHAR NOT NULL,
    lot_tenderers INTEGER NOT NULL,
    lowest_tender_value DECIMAL(18, 2),
    highest_tender_value DECIMAL(18, 2),
    cpv_codes JSON,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (publication_number, lot_id, organization_reference)
);

CREATE TABLE IF NOT EXISTS award_lots (
    enterprise_number VARCHAR NOT NULL,
    publication_number VARCHAR NOT NULL,
    lot_id VARCHAR NOT NULL,
    winner_identifier VARCHAR,
    winner_name VARCHAR,
    value DECIMAL(18, 2),
    lowest_value DECIMAL(18, 2),
    highest_value DECIMAL(18, 2),
    cpv_codes JSON,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (enterprise_number, publication_number, lot_id)
);

CREATE TABLE IF NOT EXISTS nbb_financials (
    enterprise_kbo VARCHAR NOT NULL,
    fiscal_year INTEGER NOT NULL,
    period_start VARCHAR,
    period_end VARCHAR,
    turnover DECIMAL(18, 2),
    staff_fte DECIMAL(18, 1),
    equity DECIMAL(18, 2),
    result DECIMAL(18, 2),
    source_ref VARCHAR NOT NULL,
    source_format VARCHAR NOT NULL,
    acquisition_method VARCHAR NOT NULL,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (enterprise_kbo, fiscal_year, source_ref)
);

CREATE TABLE IF NOT EXISTS inventory_awards (
    source_file VARCHAR NOT NULL,
    row_hash VARCHAR NOT NULL,
    buyer_identifier VARCHAR,
    buyer_name VARCHAR,
    awardee_identifier VARCHAR,
    awardee_name VARCHAR,
    engagement_id VARCHAR,
    engagement_date DATE,
    engagement_amount DECIMAL(18, 2),
    object VARCHAR,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (source_file, row_hash)
);

CREATE TABLE IF NOT EXISTS preferences (
    enterprise_number VARCHAR PRIMARY KEY,
    confirmed_activities JSON,
    excluded_terms JSON,
    preferred_regions JSON,
    languages JSON,
    minimum_value DECIMAL(18, 2),
    maximum_value DECIMAL(18, 2),
    recognition_class INTEGER,
    recognition_categories JSON,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp
);

CREATE TABLE IF NOT EXISTS feedback (
    enterprise_number VARCHAR NOT NULL,
    source VARCHAR NOT NULL,
    external_id VARCHAR NOT NULL,
    relevant BOOLEAN NOT NULL,
    pursuable BOOLEAN,
    outcome VARCHAR,
    reason VARCHAR,
    created_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (enterprise_number, source, external_id)
);

CREATE TABLE IF NOT EXISTS analysis_cache (
    cache_key VARCHAR PRIMARY KEY,
    decision JSON NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp
);

-- Working-papers trail: every judgment, preference change, and matter-lifecycle
-- event, with a self-asserted operator label (see operator_label). This is
-- file traceability, not access control: reads are never logged (the dashboard
-- polls read-only), and the actor label is not authenticated.
CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY DEFAULT nextval('refresh_runs_id'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    actor VARCHAR NOT NULL,
    action VARCHAR NOT NULL,
    enterprise_number VARCHAR,
    source VARCHAR,
    external_id VARCHAR,
    detail VARCHAR
);

-- One row per ingest run: which population a refresh drew from. `window` is
-- `days=N` for a rolling refresh or `FROM..TO` for one backfill chunk, so a
-- reader can tell whether the store came from a 30-day refresh or a backfill.
CREATE TABLE IF NOT EXISTS refresh_runs (
    id INTEGER PRIMARY KEY DEFAULT nextval('refresh_runs_id'),
    started_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
    kind VARCHAR NOT NULL,
    scope VARCHAR NOT NULL,
    window_desc VARCHAR NOT NULL,
    fetched INTEGER NOT NULL,
    stored INTEGER NOT NULL,
    total_notice_count INTEGER,
    duration_ms INTEGER NOT NULL,
    -- Evidence snapshot over the whole store after the run (opportunities and
    -- field presence are time-invariant; deadline-relative openness is not, so
    -- it stays live-computed). NULL on runs recorded before the snapshot, and
    -- on backfill chunks (a chunk row describes its fetch; the rolling refresh
    -- fills the snapshot).
    evidence_opportunities INTEGER,
    evidence_with_deadline INTEGER,
    evidence_with_value INTEGER,
    evidence_with_requirements INTEGER,
    evidence_stated INTEGER,
    evidence_hinted INTEGER,
    evidence_unknown INTEGER
)

"#;

fn parse_bosa_version_created_at(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|timestamp| timestamp.and_utc())
        })
}

/// BOSA correction XML often repeats VersionID=01 across published revisions.
/// Compare only within the same publication workspace; unrelated notice types
/// sharing a procurement folder need lifecycle reconciliation, not this rule.
fn bosa_revision_order(left: &Tender, right: &Tender) -> Option<std::cmp::Ordering> {
    let raw_string = |t: &Tender, key: &str| {
        t.raw
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .filter(|v| !v.is_empty())
    };
    if raw_string(left, "bosa_publication_workspace_id")?
        != raw_string(right, "bosa_publication_workspace_id")?
    {
        return None;
    }
    let left_activation = raw_string(left, "bosa_activation_date")?;
    let right_activation = raw_string(right, "bosa_activation_date")?;
    let left_created =
        parse_bosa_version_created_at(&raw_string(left, "bosa_version_created_at")?)?;
    let right_created =
        parse_bosa_version_created_at(&raw_string(right, "bosa_version_created_at")?)?;
    Some((left_activation, left_created).cmp(&(right_activation, right_created)))
}

pub struct TenderStore {
    connection: Connection,
    /// Columns added after the first release. Read-only consumers can meet a
    /// database written by an older build, so the read statements are built
    /// against what the file actually has instead of failing to bind.
    schema: SchemaColumns,
}

/// Self-asserted operator label for the audit trail (`AANBESTEDINGEN_OPERATOR`).
/// This is file traceability, not authentication: anyone holding the database
/// file or the API token can write any label.
pub fn operator_label() -> String {
    std::env::var("AANBESTEDINGEN_OPERATOR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local-operator".to_owned())
}

/// One audit event: a judgment, preference change, or matter-lifecycle event.
/// Reads are never logged; the dashboard polls read-only connections.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuditEvent {
    pub id: i64,
    pub created_at: String,
    pub actor: String,
    pub action: String,
    pub enterprise_number: Option<String>,
    pub source: Option<String>,
    pub external_id: Option<String>,
    pub detail: Option<String>,
}

/// Counts of confidential rows removed by [`TenderStore::close_matter`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatterCloseCounts {
    pub feedback: i64,
    pub preferences: i64,
    pub cached_decisions: i64,
}

/// One feedback row: topic relevance plus the pursuable/outcome split.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FeedbackLabel {
    pub relevant: bool,
    pub pursuable: Option<bool>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
}

/// Missing JSON fields preserve stored values; explicit null clears them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FieldUpdate<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for FieldUpdate<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Set(value),
            None => Self::Clear,
        })
    }
}

impl<T: Clone> FieldUpdate<T> {
    fn resolve(&self, previous: Option<T>) -> Option<T> {
        match self {
            Self::Keep => previous,
            Self::Clear => None,
            Self::Set(value) => Some(value.clone()),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct FeedbackUpdate {
    pub relevant: bool,
    #[serde(default)]
    pub pursuable: FieldUpdate<bool>,
    #[serde(default)]
    pub outcome: FieldUpdate<String>,
    #[serde(default)]
    pub reason: FieldUpdate<String>,
}

impl FeedbackUpdate {
    /// CLI full replacement, including intentional clearing of absent values.
    pub fn replacement(
        relevant: bool,
        pursuable: Option<bool>,
        outcome: Option<String>,
        reason: Option<String>,
    ) -> Self {
        Self {
            relevant,
            pursuable: pursuable.map_or(FieldUpdate::Clear, FieldUpdate::Set),
            outcome: outcome.map_or(FieldUpdate::Clear, FieldUpdate::Set),
            reason: reason.map_or(FieldUpdate::Clear, FieldUpdate::Set),
        }
    }
}

/// Which population one ingest run drew from. `window` is `days=N` for a
/// rolling refresh or `FROM..TO` for one backfill chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshRun {
    pub id: i64,
    pub started_at: String,
    pub kind: String,
    pub scope: String,
    pub window: String,
    pub fetched: i64,
    pub stored: i64,
    pub total_notice_count: Option<i64>,
    pub duration_ms: i64,
    /// Evidence snapshot over the whole store after the run, or `None` for
    /// runs recorded before the snapshot (and for backfill chunks, which
    /// describe their fetch, not the store).
    pub evidence: Option<CorpusEvidence>,
}

impl RefreshRun {
    /// The `last_refresh` object served by `/health` and `report`: one
    /// definition, so the two endpoints cannot disagree on the shape.
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "at": self.started_at,
            "kind": self.kind,
            "scope": self.scope,
            "window": self.window,
            "fetched": self.fetched,
            "stored": self.stored,
            "total_notice_count": self.total_notice_count,
            "duration_ms": self.duration_ms,
            "evidence": self.evidence.as_ref().map(|evidence| serde_json::json!({
                "opportunities": evidence.opportunities,
                "with_deadline": evidence.with_deadline,
                "with_value": evidence.with_value,
                "with_requirements": evidence.with_requirements,
                "stated": evidence.stated,
                "hinted": evidence.hinted,
                "unknown": evidence.unknown,
            })),
        })
    }
}

/// One awarded contract from an inventory, collapsed from the accounting rows
/// that repeat it (one per liquidation).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InventoryEngagement {
    pub engagement_id: Option<String>,
    pub buyer_name: Option<String>,
    pub engagement_date: Option<String>,
    pub engagement_amount: Option<Decimal>,
    pub object: Option<String>,
    /// How many accounting rows in the file carry this contract.
    pub accounting_rows: i64,
}

#[derive(Debug, Clone, Copy)]
struct SchemaColumns {
    tenders_notice_type: bool,
    tenders_publication_date: bool,
    tenders_lot_values: bool,
    preferences_recognition: bool,
    inventory_awards: bool,
    feedback_pursuable: bool,
    feedback_outcome: bool,
    refresh_runs: bool,
    refresh_evidence: bool,
    audit_log: bool,
}

impl SchemaColumns {
    fn probe(connection: &Connection) -> Result<Self, duckdb::Error> {
        Ok(Self {
            tenders_notice_type: column_exists(connection, "tenders", "notice_type")?,
            tenders_publication_date: column_exists(connection, "tenders", "publication_date")?,
            tenders_lot_values: column_exists(connection, "tenders", "lot_values")?,
            preferences_recognition: column_exists(connection, "preferences", "recognition_class")?,
            inventory_awards: table_exists(connection, "inventory_awards")?,
            feedback_pursuable: column_exists(connection, "feedback", "pursuable")?,
            feedback_outcome: column_exists(connection, "feedback", "outcome")?,
            refresh_runs: table_exists(connection, "refresh_runs")?,
            refresh_evidence: column_exists(connection, "refresh_runs", "evidence_opportunities")?,
            audit_log: table_exists(connection, "audit_log")?,
        })
    }

    fn notice_type_expression(self) -> &'static str {
        if self.tenders_notice_type {
            "notice_type"
        } else {
            "NULL::VARCHAR"
        }
    }

    fn opportunity_filter(self) -> &'static str {
        // Without the column every row is unknown, which the filter treats as
        // eligible anyway; spelling that out keeps old databases readable.
        if self.tenders_notice_type {
            OPPORTUNITY_FILTER
        } else {
            EVERY_NOTICE_FILTER
        }
    }

    fn publication_date_expression(self) -> &'static str {
        if self.tenders_publication_date {
            "publication_date"
        } else {
            "NULL::DATE"
        }
    }

    fn lot_values_expression(self) -> &'static str {
        if self.tenders_lot_values {
            "lot_values"
        } else {
            "NULL::JSON"
        }
    }

    fn recognition_expression(self) -> &'static str {
        if self.preferences_recognition {
            "recognition_class, recognition_categories"
        } else {
            // Keep fallback NULLs typed: duckdb-rs cannot decode Arrow Null columns.
            "NULL::INTEGER AS recognition_class, NULL::VARCHAR AS recognition_categories"
        }
    }
}

fn column_exists(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, duckdb::Error> {
    let mut statement = connection.prepare(
        "SELECT count(*) FROM duckdb_columns() WHERE table_name = ? AND column_name = ?",
    )?;
    let count: i64 = statement.query_row(params![table, column], |row| row.get(0))?;
    Ok(count > 0)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, duckdb::Error> {
    let mut statement = connection
        .prepare("SELECT count(*) FROM information_schema.tables WHERE table_name = ?")?;
    let count: i64 = statement.query_row(params![table], |row| row.get(0))?;
    Ok(count > 0)
}

const TENDERS_SQL: &str = "SELECT source, external_id, title, buyer, description, cpv_codes, \
     regions, estimated_value, CAST(deadline AS VARCHAR), languages, requirements, \
     document_urls, {notice_type}, CAST({publication_date} AS VARCHAR), {lot_values}, \
     CASE WHEN json_type(raw, '$.contract_folder_id') = 'VARCHAR' THEN json_extract_string(raw, '$.contract_folder_id') END, json_extract_string(raw, '$.dossier_container'), \
     json_extract_string(raw, '$.framework_maximum_amounts'), \
     json_extract_string(raw, '$.notice_lot_cpv_evidence'), \
     json_extract_string(raw, '$.framework_maximum_evidence'), \
     json_extract_string(raw, '$.framework_maximum_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_ids_unique'), \
     json_extract_string(raw, '$.notice_lot_place_evidence'), \
     json_extract_string(raw, '$.notice_lot_place_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_award_criteria_evidence'), \
     json_extract_string(raw, '$.notice_lot_award_criteria_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_selection_evidence'), \
     json_extract_string(raw, '$.notice_lot_selection_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_contract_execution_evidence'), \
     json_extract_string(raw, '$.notice_lot_contract_execution_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_subsequent_process_evidence'), \
     json_extract_string(raw, '$.notice_lot_subsequent_process_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_guarantee_curricula_evidence'), \
     json_extract_string(raw, '$.notice_lot_guarantee_curricula_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_ted_evidence'), \
     CASE WHEN json_type(raw, '$.bosa_publication_workspace_id') = 'VARCHAR' THEN json_extract_string(raw, '$.bosa_publication_workspace_id') END \
     FROM tenders ORDER BY deadline NULLS LAST, external_id LIMIT ?";
const TENDERS_WITH_RAW_SQL: &str = concat!(
    "SELECT source, external_id, title, buyer, description, cpv_codes, ",
    "regions, estimated_value, CAST(deadline AS VARCHAR), languages, requirements, ",
    "document_urls, {notice_type}, CAST({publication_date} AS VARCHAR), raw, {lot_values}, ",
    "CAST(fetched_at AS VARCHAR) FROM tenders WHERE source = ? ORDER BY external_id LIMIT ?",
);

#[derive(Debug, Clone)]
pub struct TenderSnapshotRow {
    pub tender: Tender,
    pub fetched_at: String,
}

type TenderFullRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<Decimal>,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    String,
);

fn read_tender_full_row(row: &Row<'_>) -> duckdb::Result<TenderFullRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
    ))
}

fn tender_from_full_row(row: TenderFullRow) -> Result<TenderSnapshotRow, Box<dyn Error>> {
    let (
        source,
        external_id,
        title,
        buyer,
        description,
        cpv_codes,
        regions,
        estimated_value,
        deadline,
        languages,
        requirements,
        document_urls,
        notice_type,
        publication_date,
        raw,
        lot_values,
        storage_fetched_at,
    ) = row;
    Ok(TenderSnapshotRow {
        tender: Tender {
            source,
            external_id,
            title,
            buyer,
            description: description.unwrap_or_default(),
            cpv_codes: serde_json::from_str(&cpv_codes)?,
            regions: serde_json::from_str(&regions)?,
            estimated_value,
            lot_values: parse_lot_values(lot_values.as_deref())?,
            deadline,
            languages: serde_json::from_str(&languages)?,
            notice_type,
            publication_date,
            requirements: serde_json::from_str::<Vec<TenderRequirement>>(&requirements)?,
            document_urls: serde_json::from_str(&document_urls)?,
            raw: serde_json::from_str(&raw)?,
        },
        fetched_at: storage_fetched_at,
    })
}

/// Load compact payloads only for the identities selected by the shared
/// version resolver. Eligibility and bounds have already been applied.
const SELECTED_TENDERS_SQL: &str = "SELECT source, external_id, title, buyer, description, cpv_codes, \
     regions, estimated_value, CAST(deadline AS VARCHAR), languages, requirements, \
     document_urls, {notice_type}, CAST({publication_date} AS VARCHAR), {lot_values}, \
     CASE WHEN json_type(raw, '$.contract_folder_id') = 'VARCHAR' THEN json_extract_string(raw, '$.contract_folder_id') END, json_extract_string(raw, '$.dossier_container'), \
     json_extract_string(raw, '$.framework_maximum_amounts'), \
     json_extract_string(raw, '$.notice_lot_cpv_evidence'), \
     json_extract_string(raw, '$.framework_maximum_evidence'), \
     json_extract_string(raw, '$.framework_maximum_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_ids_unique'), \
     json_extract_string(raw, '$.notice_lot_place_evidence'), \
     json_extract_string(raw, '$.notice_lot_place_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_award_criteria_evidence'), \
     json_extract_string(raw, '$.notice_lot_award_criteria_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_selection_evidence'), \
     json_extract_string(raw, '$.notice_lot_selection_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_contract_execution_evidence'), \
     json_extract_string(raw, '$.notice_lot_contract_execution_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_subsequent_process_evidence'), \
     json_extract_string(raw, '$.notice_lot_subsequent_process_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_guarantee_curricula_evidence'), \
     json_extract_string(raw, '$.notice_lot_guarantee_curricula_evidence_truncated'), \
     json_extract_string(raw, '$.notice_lot_ted_evidence'), \
     CASE WHEN json_type(raw, '$.bosa_publication_workspace_id') = 'VARCHAR' THEN json_extract_string(raw, '$.bosa_publication_workspace_id') END \
     FROM tenders JOIN json_each(?) AS selected \
     ON tenders.source = json_extract_string(selected.value, '$[0]') \
     AND tenders.external_id = json_extract_string(selected.value, '$[1]') \
     ORDER BY deadline NULLS LAST, external_id, source";

const OPPORTUNITY_FILTER: &str =
    "(notice_type IS NULL OR notice_type LIKE 'cn%' OR notice_type LIKE 'pin-cfc%')";
const EVERY_NOTICE_FILTER: &str = "TRUE";

type TenderListRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<Decimal>,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// NULL (legacy rows), empty and explicit-null JSON all mean no recorded
/// lots — never an error, so old databases read exactly as before.
fn parse_lot_values(raw: Option<&str>) -> Result<Vec<Decimal>, Box<dyn Error>> {
    match raw.map(str::trim) {
        None | Some("") | Some("null") => Ok(Vec::new()),
        Some(text) => Ok(serde_json::from_str(text)?),
    }
}

fn read_tender_list_row(row: &Row<'_>) -> duckdb::Result<TenderListRow> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, Option<String>>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, String>(5)?,
        row.get::<_, String>(6)?,
        row.get::<_, Option<Decimal>>(7)?,
        row.get::<_, Option<String>>(8)?,
        row.get::<_, String>(9)?,
        row.get::<_, String>(10)?,
        row.get::<_, String>(11)?,
        row.get::<_, Option<String>>(12)?,
        row.get::<_, Option<String>>(13)?,
        row.get::<_, Option<String>>(14)?,
        row.get::<_, Option<String>>(15)?,
        row.get::<_, Option<String>>(16)?,
        row.get::<_, Option<String>>(17)?,
        row.get::<_, Option<String>>(18)?,
        row.get::<_, Option<String>>(19)?,
        row.get::<_, Option<String>>(20)?,
        row.get::<_, Option<String>>(21)?,
        row.get::<_, Option<String>>(22)?,
        row.get::<_, Option<String>>(23)?,
        row.get::<_, Option<String>>(24)?,
        row.get::<_, Option<String>>(25)?,
        row.get::<_, Option<String>>(26)?,
        row.get::<_, Option<String>>(27)?,
        row.get::<_, Option<String>>(28)?,
        row.get::<_, Option<String>>(29)?,
        row.get::<_, Option<String>>(30)?,
        row.get::<_, Option<String>>(31)?,
        row.get::<_, Option<String>>(32)?,
        row.get::<_, Option<String>>(33)?,
        row.get::<_, Option<String>>(34)?,
        row.get::<_, Option<String>>(35)?,
    ))
}

fn tender_from_list_row(row: TenderListRow) -> Result<Tender, Box<dyn Error>> {
    let (
        source,
        external_id,
        title,
        buyer,
        description,
        cpv_codes,
        regions,
        estimated_value,
        deadline,
        languages,
        requirements,
        document_urls,
        notice_type,
        publication_date,
        lot_values,
        contract_folder_id,
        dossier_container,
        framework_maximum_amounts,
        notice_lot_cpv_evidence,
        framework_maximum_evidence,
        framework_maximum_evidence_truncated,
        notice_lot_ids_unique,
        notice_lot_place_evidence,
        notice_lot_place_evidence_truncated,
        notice_lot_award_criteria_evidence,
        notice_lot_award_criteria_evidence_truncated,
        notice_lot_selection_evidence,
        notice_lot_selection_evidence_truncated,
        notice_lot_contract_execution_evidence,
        notice_lot_contract_execution_evidence_truncated,
        notice_lot_subsequent_process_evidence,
        notice_lot_subsequent_process_evidence_truncated,
        notice_lot_guarantee_curricula_evidence,
        notice_lot_guarantee_curricula_evidence_truncated,
        notice_lot_ted_evidence,
        bosa_publication_workspace_id,
    ) = row;
    Ok(Tender {
        source,
        external_id,
        title,
        buyer,
        description: description.unwrap_or_default(),
        cpv_codes: serde_json::from_str(&cpv_codes)?,
        regions: serde_json::from_str(&regions)?,
        estimated_value,
        lot_values: parse_lot_values(lot_values.as_deref())?,
        deadline,
        languages: serde_json::from_str(&languages)?,
        notice_type,
        publication_date,
        requirements: serde_json::from_str::<Vec<TenderRequirement>>(&requirements)?,
        document_urls: serde_json::from_str(&document_urls)?,
        // List rows carry no full `raw` payload (notably no extracted PDF text).
        // Only bounded dossier, framework and lot evidence needed for the
        // recommendation response is projected into this lightweight row.
        raw: list_row_raw(ListRowRawEvidence {
            workspace: bosa_publication_workspace_id.as_deref(),
            folder: contract_folder_id.as_deref(),
            container: dossier_container.as_deref(),
            framework_maximum_amounts: framework_maximum_amounts.as_deref(),
            notice_lot_cpv_evidence: notice_lot_cpv_evidence.as_deref(),
            framework_maximum_evidence: framework_maximum_evidence.as_deref(),
            framework_maximum_evidence_truncated: framework_maximum_evidence_truncated.as_deref(),
            notice_lot_ids_unique: notice_lot_ids_unique.as_deref(),
            notice_lot_place_evidence: notice_lot_place_evidence.as_deref(),
            notice_lot_place_evidence_truncated: notice_lot_place_evidence_truncated.as_deref(),
            notice_lot_award_criteria_evidence: notice_lot_award_criteria_evidence.as_deref(),
            notice_lot_award_criteria_evidence_truncated:
                notice_lot_award_criteria_evidence_truncated.as_deref(),
            notice_lot_selection_evidence: notice_lot_selection_evidence.as_deref(),
            notice_lot_selection_evidence_truncated: notice_lot_selection_evidence_truncated
                .as_deref(),
            notice_lot_contract_execution_evidence: notice_lot_contract_execution_evidence
                .as_deref(),
            notice_lot_contract_execution_evidence_truncated:
                notice_lot_contract_execution_evidence_truncated.as_deref(),
            notice_lot_subsequent_process_evidence: notice_lot_subsequent_process_evidence
                .as_deref(),
            notice_lot_subsequent_process_evidence_truncated:
                notice_lot_subsequent_process_evidence_truncated.as_deref(),
            notice_lot_guarantee_curricula_evidence: notice_lot_guarantee_curricula_evidence
                .as_deref(),
            notice_lot_guarantee_curricula_evidence_truncated:
                notice_lot_guarantee_curricula_evidence_truncated.as_deref(),
            notice_lot_ted_evidence: notice_lot_ted_evidence.as_deref(),
        }),
    })
}

const MAX_PROJECTED_LOT_CPV_LOTS: usize = 20;
const MAX_PROJECTED_LOT_CPVS_PER_LOT: usize = 10;

fn bounded_lot_cpv_evidence(raw: Option<&str>) -> Option<(serde_json::Value, bool)> {
    let value = serde_json::from_str::<serde_json::Value>(raw?).ok()?;
    let records = value.as_array()?;
    let mut truncated = records.len() > MAX_PROJECTED_LOT_CPV_LOTS;
    let mut lots = Vec::with_capacity(records.len().min(MAX_PROJECTED_LOT_CPV_LOTS));

    for record in records.iter().take(MAX_PROJECTED_LOT_CPV_LOTS) {
        let Some(object) = record.as_object() else {
            truncated = true;
            continue;
        };
        let lot_id = object
            .get("lot_id")
            .and_then(serde_json::Value::as_str)
            .map(|value| value.chars().take(128).collect::<String>())
            .filter(|value| !value.trim().is_empty())
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
        let mut seen = HashSet::new();
        let mut cpv_codes = Vec::new();
        if let Some(codes) = object
            .get("cpv_codes")
            .and_then(serde_json::Value::as_array)
        {
            for code in codes.iter().filter_map(serde_json::Value::as_str) {
                let code = code.trim();
                if code.len() == 8
                    && code.bytes().all(|byte| byte.is_ascii_digit())
                    && seen.insert(code.to_owned())
                {
                    cpv_codes.push(code.to_owned());
                }
            }
        }
        if cpv_codes.len() > MAX_PROJECTED_LOT_CPVS_PER_LOT {
            truncated = true;
            cpv_codes.truncate(MAX_PROJECTED_LOT_CPVS_PER_LOT);
        }
        lots.push(serde_json::json!({
            "lot_id": lot_id,
            "cpv_codes": cpv_codes,
        }));
    }

    (!lots.is_empty()).then_some((serde_json::Value::Array(lots), truncated))
}

const MAX_PROJECTED_LOT_PLACE_LOTS: usize = 20;
const MAX_PROJECTED_LOT_PLACE_CODES_PER_LOT: usize = 10;

fn bounded_lot_place_evidence(
    raw: Option<&str>,
    source_truncated: Option<&str>,
) -> Option<(serde_json::Value, bool)> {
    let source_truncated = match source_truncated.map(str::trim) {
        None | Some("false") | Some("0") | Some("") => false,
        Some("true") | Some("1") => true,
        Some(_) => true,
    };
    let Some(raw) = raw else {
        return source_truncated.then(|| (serde_json::Value::Array(Vec::new()), true));
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let Some(records) = value.as_array() else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let mut truncated = source_truncated || records.len() > MAX_PROJECTED_LOT_PLACE_LOTS;
    let mut evidence = Vec::with_capacity(records.len().min(MAX_PROJECTED_LOT_PLACE_LOTS));
    for record in records.iter().take(MAX_PROJECTED_LOT_PLACE_LOTS) {
        let Some(object) = record.as_object() else {
            truncated = true;
            continue;
        };
        let lot_id = object
            .get("lot_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.chars().count() <= 128)
            .map(str::to_owned);
        if lot_id.is_none() {
            truncated = true;
        }
        let Some(codes) = object
            .get("country_subentity_codes")
            .and_then(serde_json::Value::as_array)
        else {
            truncated = true;
            continue;
        };
        let mut seen = HashSet::new();
        let mut country_subentity_codes = Vec::new();
        for code in codes.iter().filter_map(serde_json::Value::as_str) {
            let code = code.trim();
            if code.is_empty() || code.chars().count() > 128 {
                truncated = true;
                continue;
            }
            if seen.insert(code.to_owned()) {
                if country_subentity_codes.len() == MAX_PROJECTED_LOT_PLACE_CODES_PER_LOT {
                    truncated = true;
                    continue;
                }
                country_subentity_codes.push(code.to_owned());
            }
        }
        if codes.iter().any(|code| !code.is_string()) {
            truncated = true;
        }
        evidence.push(serde_json::json!({
            "lot_id": lot_id,
            "country_subentity_codes": country_subentity_codes,
        }));
    }
    Some((serde_json::Value::Array(evidence), truncated))
}

const MAX_PROJECTED_FRAMEWORK_MAXIMUMS: usize = 100;

fn bounded_framework_maximum_evidence(
    raw: Option<&str>,
    source_truncated: Option<&str>,
) -> Option<(serde_json::Value, bool)> {
    let source_truncated = match source_truncated.map(str::trim) {
        None | Some("false") | Some("0") | Some("") => false,
        Some("true") | Some("1") => true,
        Some(_) => true,
    };
    let Some(raw) = raw else {
        return source_truncated.then(|| (serde_json::Value::Array(Vec::new()), true));
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let Some(records) = value.as_array() else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let mut truncated = source_truncated || records.len() > MAX_PROJECTED_FRAMEWORK_MAXIMUMS;
    let mut evidence = Vec::with_capacity(records.len().min(MAX_PROJECTED_FRAMEWORK_MAXIMUMS));
    for record in records.iter().take(MAX_PROJECTED_FRAMEWORK_MAXIMUMS) {
        let Some(object) = record.as_object() else {
            truncated = true;
            continue;
        };
        let Some(scope) = object.get("scope").and_then(serde_json::Value::as_str) else {
            truncated = true;
            continue;
        };
        if !matches!(scope, "lot" | "notice") {
            truncated = true;
            continue;
        }
        let amount = object
            .get("amount")
            .and_then(serde_json::Value::as_str)
            .filter(|amount| !amount.trim().is_empty());
        let Some(amount) = amount else {
            truncated = true;
            continue;
        };
        let currency = object
            .get("currency_id")
            .and_then(serde_json::Value::as_str);
        let lot_id = object.get("lot_id").and_then(serde_json::Value::as_str);
        if amount.chars().count() > 128
            || currency.is_some_and(|value| value.chars().count() > 16)
            || lot_id.is_some_and(|value| value.chars().count() > 128)
        {
            truncated = true;
            continue;
        }
        evidence.push(serde_json::json!({
            "amount": amount,
            "currency_id": currency,
            "scope": scope,
            "lot_id": lot_id,
        }));
    }
    Some((serde_json::Value::Array(evidence), truncated))
}

const MAX_PROJECTED_LOT_AWARD_CRITERIA_LOTS: usize = 20;
const MAX_PROJECTED_LOT_AWARD_CRITERIA: usize = 100;
const MAX_PROJECTED_LOT_AWARD_CRITERION_PARAMETERS: usize = 10;
const MAX_PROJECTED_LOT_AWARD_CRITERION_TEXT_CHARS: usize = 1_000;
const MAX_PROJECTED_LOT_AWARD_CRITERION_PATH_DEPTH: usize = 20;

fn bounded_lot_award_criteria_evidence(
    raw: Option<&str>,
    source_truncated: Option<&str>,
) -> Option<(serde_json::Value, bool)> {
    let source_truncated = match source_truncated.map(str::trim) {
        None | Some("false") | Some("0") | Some("") => false,
        Some("true") | Some("1") => true,
        Some(_) => true,
    };
    let Some(raw) = raw else {
        return source_truncated.then(|| (serde_json::Value::Array(Vec::new()), true));
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let Some(records) = value.as_array() else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let mut truncated = source_truncated || records.len() > MAX_PROJECTED_LOT_AWARD_CRITERIA_LOTS;
    let mut evidence = Vec::with_capacity(records.len().min(MAX_PROJECTED_LOT_AWARD_CRITERIA_LOTS));
    let mut criterion_count = 0;
    let bounded_optional_text = |object: &serde_json::Map<String, serde_json::Value>,
                                 key: &str,
                                 limit: usize,
                                 truncated: &mut bool| {
        match object.get(key) {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(value)) => {
                if value.chars().count() > limit {
                    *truncated = true;
                    Some(value.chars().take(limit).collect::<String>())
                } else {
                    Some(value.clone())
                }
            }
            Some(_) => {
                *truncated = true;
                None
            }
        }
    };

    for record in records.iter().take(MAX_PROJECTED_LOT_AWARD_CRITERIA_LOTS) {
        let Some(object) = record.as_object() else {
            truncated = true;
            continue;
        };
        let lot_id = match object.get("lot_id") {
            Some(serde_json::Value::String(value))
                if !value.trim().is_empty() && value.chars().count() <= 128 =>
            {
                Some(value.clone())
            }
            Some(serde_json::Value::Null) => None,
            Some(_) => {
                truncated = true;
                None
            }
            None => {
                truncated = true;
                None
            }
        };
        let Some(criteria) = object.get("criteria").and_then(serde_json::Value::as_array) else {
            truncated = true;
            continue;
        };
        let mut projected = Vec::new();
        for criterion in criteria {
            if criterion_count == MAX_PROJECTED_LOT_AWARD_CRITERIA {
                truncated = true;
                break;
            }
            let Some(criterion) = criterion.as_object() else {
                truncated = true;
                continue;
            };
            let source_element = match criterion
                .get("source_element")
                .and_then(serde_json::Value::as_str)
            {
                Some("AwardingCriterion") => "AwardingCriterion",
                Some("SubordinateAwardingCriterion") => "SubordinateAwardingCriterion",
                _ => {
                    truncated = true;
                    continue;
                }
            };
            let Some(path) = criterion
                .get("source_ordinal_path")
                .and_then(serde_json::Value::as_array)
            else {
                truncated = true;
                continue;
            };
            if path.len() > MAX_PROJECTED_LOT_AWARD_CRITERION_PATH_DEPTH {
                truncated = true;
                continue;
            }
            let mut source_ordinal_path = Vec::with_capacity(path.len());
            let mut valid_path = true;
            for index in path {
                match index.as_u64().filter(|value| *value <= 10_000) {
                    Some(index) => source_ordinal_path.push(index),
                    None => {
                        truncated = true;
                        valid_path = false;
                        break;
                    }
                }
            }
            if !valid_path {
                continue;
            }
            let mut parameters = Vec::new();
            let Some(source_parameters) = criterion
                .get("parameters")
                .and_then(serde_json::Value::as_array)
            else {
                truncated = true;
                continue;
            };
            for parameter in source_parameters {
                if parameters.len() == MAX_PROJECTED_LOT_AWARD_CRITERION_PARAMETERS {
                    truncated = true;
                    break;
                }
                let Some(parameter) = parameter.as_object() else {
                    truncated = true;
                    continue;
                };
                let code = bounded_optional_text(parameter, "code", 128, &mut truncated);
                let numeric = bounded_optional_text(parameter, "numeric", 128, &mut truncated);
                if code.is_some() || numeric.is_some() {
                    parameters.push(serde_json::json!({"code": code, "numeric": numeric}));
                }
            }
            projected.push(serde_json::json!({
                "source_element": source_element,
                "source_ordinal_path": source_ordinal_path,
                "type_code": bounded_optional_text(criterion, "type_code", 128, &mut truncated),
                "description": bounded_optional_text(criterion, "description", MAX_PROJECTED_LOT_AWARD_CRITERION_TEXT_CHARS, &mut truncated),
                "calculation_expression": bounded_optional_text(criterion, "calculation_expression", MAX_PROJECTED_LOT_AWARD_CRITERION_TEXT_CHARS, &mut truncated),
                "parameters": parameters,
            }));
            criterion_count += 1;
        }
        evidence.push(serde_json::json!({"lot_id": lot_id, "criteria": projected}));
    }
    Some((serde_json::Value::Array(evidence), truncated))
}

const MAX_PROJECTED_LOT_SELECTION_LOTS: usize = 20;
const MAX_PROJECTED_LOT_SELECTION_RECORDS: usize = 100;
const MAX_PROJECTED_LOT_SELECTION_PER_LOT: usize = 20;
const MAX_PROJECTED_LOT_SELECTION_DESCRIPTIONS: usize = 10;
const MAX_PROJECTED_LOT_SELECTION_TEXT_CHARS: usize = 1_000;

fn bounded_lot_selection_evidence(
    raw: Option<&str>,
    source_truncated: Option<&str>,
) -> Option<(serde_json::Value, bool)> {
    let source_truncated = match source_truncated.map(str::trim) {
        None | Some("false") | Some("0") | Some("") => false,
        Some("true") | Some("1") => true,
        Some(_) => true,
    };
    let Some(raw) = raw else {
        return source_truncated.then(|| (serde_json::Value::Array(Vec::new()), true));
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let Some(records) = value.as_array() else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let mut truncated = source_truncated || records.len() > MAX_PROJECTED_LOT_SELECTION_LOTS;
    let mut evidence = Vec::with_capacity(records.len().min(MAX_PROJECTED_LOT_SELECTION_LOTS));
    let mut record_count = 0;
    for record in records.iter().take(MAX_PROJECTED_LOT_SELECTION_LOTS) {
        let Some(object) = record.as_object() else {
            truncated = true;
            continue;
        };
        let lot_id = match object.get("lot_id") {
            Some(serde_json::Value::String(value))
                if !value.trim().is_empty() && value.chars().count() <= 128 =>
            {
                Some(value.clone())
            }
            Some(serde_json::Value::Null) => None,
            _ => {
                truncated = true;
                None
            }
        };
        let Some(source_records) = object
            .get("selection_evidence")
            .and_then(serde_json::Value::as_array)
        else {
            truncated = true;
            continue;
        };
        let mut projected = Vec::new();
        for source_record in source_records {
            if record_count == MAX_PROJECTED_LOT_SELECTION_RECORDS
                || projected.len() == MAX_PROJECTED_LOT_SELECTION_PER_LOT
            {
                truncated = true;
                break;
            }
            let Some(source_record) = source_record.as_object() else {
                truncated = true;
                continue;
            };
            let source_element = source_record
                .get("source_element")
                .and_then(serde_json::Value::as_str);
            let (source_element, source_path) = match source_element {
                Some("SpecificTendererRequirement") => (
                    "SpecificTendererRequirement",
                    "TenderingTerms/TendererQualificationRequest/SpecificTendererRequirement",
                ),
                Some("CompanyLegalFormCode") => (
                    "CompanyLegalFormCode",
                    "TenderingTerms/TendererQualificationRequest/CompanyLegalFormCode",
                ),
                Some("SelectionCriteria") => (
                    "SelectionCriteria",
                    "TenderingTerms/UBLExtensions/UBLExtension/ExtensionContent/EformsExtension/SelectionCriteria",
                ),
                _ => {
                    truncated = true;
                    continue;
                }
            };
            if source_record
                .get("source_path")
                .and_then(serde_json::Value::as_str)
                != Some(source_path)
            {
                truncated = true;
                continue;
            }
            let Some(path) = source_record
                .get("source_ordinal_path")
                .and_then(serde_json::Value::as_array)
            else {
                truncated = true;
                continue;
            };
            if path.len() != 2 {
                truncated = true;
                continue;
            }
            let mut source_ordinal_path = Vec::with_capacity(2);
            let mut valid_path = true;
            for index in path {
                match index.as_u64().filter(|value| *value <= 10_000) {
                    Some(index) => source_ordinal_path.push(index),
                    None => {
                        truncated = true;
                        valid_path = false;
                        break;
                    }
                }
            }
            if !valid_path {
                continue;
            }
            let bounded_optional_text =
                |key: &str, limit: usize, truncated: &mut bool| match source_record.get(key) {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(value)) => {
                        if value.chars().count() > limit {
                            *truncated = true;
                            Some(value.chars().take(limit).collect::<String>())
                        } else {
                            Some(value.clone())
                        }
                    }
                    Some(_) => {
                        *truncated = true;
                        None
                    }
                };
            let code = bounded_optional_text("code", 128, &mut truncated);
            let code_list_name = bounded_optional_text("code_list_name", 128, &mut truncated);
            let Some(descriptions) = source_record
                .get("descriptions")
                .and_then(serde_json::Value::as_array)
            else {
                truncated = true;
                continue;
            };
            let mut projected_descriptions = Vec::new();
            if descriptions.len() > MAX_PROJECTED_LOT_SELECTION_DESCRIPTIONS {
                truncated = true;
            }
            for description in descriptions
                .iter()
                .take(MAX_PROJECTED_LOT_SELECTION_DESCRIPTIONS)
            {
                let Some(description) = description.as_str() else {
                    truncated = true;
                    continue;
                };
                if description.chars().count() > MAX_PROJECTED_LOT_SELECTION_TEXT_CHARS {
                    truncated = true;
                    projected_descriptions.push(
                        description
                            .chars()
                            .take(MAX_PROJECTED_LOT_SELECTION_TEXT_CHARS)
                            .collect::<String>(),
                    );
                } else {
                    projected_descriptions.push(description.to_owned());
                }
            }
            projected.push(serde_json::json!({
                "source_element": source_element,
                "source_path": source_path,
                "source_ordinal_path": source_ordinal_path,
                "code": code,
                "code_list_name": code_list_name,
                "descriptions": projected_descriptions,
            }));
            record_count += 1;
        }
        evidence.push(serde_json::json!({"lot_id": lot_id, "selection_evidence": projected}));
    }
    Some((serde_json::Value::Array(evidence), truncated))
}

const MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LOTS: usize = 20;
const MAX_PROJECTED_LOT_CONTRACT_EXECUTION_RECORDS: usize = 100;
const MAX_PROJECTED_LOT_CONTRACT_EXECUTION_PER_LOT: usize = 20;
const MAX_PROJECTED_LOT_CONTRACT_EXECUTION_CODE_CHARS: usize = 256;
const MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LIST_NAME_CHARS: usize = 128;

fn bounded_lot_contract_execution_evidence(
    raw: Option<&str>,
    source_truncated: Option<&str>,
) -> Option<(serde_json::Value, bool)> {
    let source_truncated = match source_truncated.map(str::trim) {
        None | Some("false") | Some("0") | Some("") => false,
        Some("true") | Some("1") => true,
        Some(_) => true,
    };
    let Some(raw) = raw else {
        return source_truncated.then(|| (serde_json::Value::Array(Vec::new()), true));
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let Some(records) = value.as_array() else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let mut truncated =
        source_truncated || records.len() > MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LOTS;
    let mut evidence =
        Vec::with_capacity(records.len().min(MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LOTS));
    let mut record_count = 0;
    for record in records
        .iter()
        .take(MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LOTS)
    {
        let Some(object) = record.as_object() else {
            truncated = true;
            continue;
        };
        let lot_id = match object.get("lot_id") {
            Some(serde_json::Value::String(value))
                if !value.trim().is_empty() && value.chars().count() <= 128 =>
            {
                Some(value.clone())
            }
            Some(serde_json::Value::Null) => None,
            _ => {
                truncated = true;
                None
            }
        };
        let Some(conditions) = object
            .get("conditions")
            .and_then(serde_json::Value::as_array)
        else {
            truncated = true;
            continue;
        };
        let mut projected = Vec::new();
        if conditions.len() > MAX_PROJECTED_LOT_CONTRACT_EXECUTION_PER_LOT {
            truncated = true;
        }
        for condition in conditions
            .iter()
            .take(MAX_PROJECTED_LOT_CONTRACT_EXECUTION_PER_LOT)
        {
            if record_count == MAX_PROJECTED_LOT_CONTRACT_EXECUTION_RECORDS {
                truncated = true;
                break;
            }
            let Some(condition) = condition.as_object() else {
                truncated = true;
                continue;
            };
            if condition
                .get("source_element")
                .and_then(serde_json::Value::as_str)
                != Some("ContractExecutionRequirement")
                || condition
                    .get("source_path")
                    .and_then(serde_json::Value::as_str)
                    != Some("TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode")
            {
                truncated = true;
                continue;
            }
            let Some(path) = condition
                .get("source_ordinal_path")
                .and_then(serde_json::Value::as_array)
                .filter(|path| path.len() == 1)
            else {
                truncated = true;
                continue;
            };
            let Some(index) = path[0].as_u64().filter(|index| *index <= 10_000) else {
                truncated = true;
                continue;
            };
            let bounded_optional_text =
                |key: &str, limit: usize, truncated: &mut bool| match condition.get(key) {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(value)) => {
                        if value.chars().count() > limit {
                            *truncated = true;
                            Some(value.chars().take(limit).collect::<String>())
                        } else {
                            Some(value.clone())
                        }
                    }
                    Some(_) => {
                        *truncated = true;
                        None
                    }
                };
            let code = bounded_optional_text(
                "code",
                MAX_PROJECTED_LOT_CONTRACT_EXECUTION_CODE_CHARS,
                &mut truncated,
            );
            let code_list_name = bounded_optional_text(
                "code_list_name",
                MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LIST_NAME_CHARS,
                &mut truncated,
            );
            projected.push(serde_json::json!({
                "source_element": "ContractExecutionRequirement",
                "source_path": "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode",
                "source_ordinal_path": [index],
                "code": code,
                "code_list_name": code_list_name,
            }));
            record_count += 1;
        }
        evidence.push(serde_json::json!({"lot_id": lot_id, "conditions": projected}));
    }
    Some((serde_json::Value::Array(evidence), truncated))
}

const MAX_PROJECTED_SUBSEQUENT_REQUIREMENT_LOTS: usize = 20;
const MAX_PROJECTED_SUBSEQUENT_REQUIREMENTS: usize = 100;
const MAX_PROJECTED_SUBSEQUENT_PER_LOT: usize = 20;
const MAX_PROJECTED_SUBSEQUENT_NAME_CHARS: usize = 2_000;

fn bounded_lot_subsequent_process_evidence(
    raw: Option<&str>,
    source_truncated: Option<&str>,
) -> Option<(serde_json::Value, bool)> {
    let source_truncated = match source_truncated.map(str::trim) {
        None | Some("false") | Some("0") | Some("") => false,
        Some("true") | Some("1") | Some(_) => true,
    };
    let Some(raw) = raw else {
        return source_truncated.then(|| (serde_json::Value::Array(Vec::new()), true));
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let Some(records) = value.as_array() else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let mut truncated =
        source_truncated || records.len() > MAX_PROJECTED_SUBSEQUENT_REQUIREMENT_LOTS;
    let mut evidence =
        Vec::with_capacity(records.len().min(MAX_PROJECTED_SUBSEQUENT_REQUIREMENT_LOTS));
    let mut total = 0;
    for record in records
        .iter()
        .take(MAX_PROJECTED_SUBSEQUENT_REQUIREMENT_LOTS)
    {
        let Some(object) = record.as_object() else {
            truncated = true;
            continue;
        };
        let lot_id = match object.get("lot_id") {
            Some(serde_json::Value::String(value))
                if !value.trim().is_empty() && value.chars().count() <= 128 =>
            {
                Some(value.clone())
            }
            Some(serde_json::Value::Null) => None,
            _ => {
                truncated = true;
                None
            }
        };
        let Some(requirements) = object
            .get("requirements")
            .and_then(serde_json::Value::as_array)
        else {
            truncated = true;
            continue;
        };
        if requirements.len() > MAX_PROJECTED_SUBSEQUENT_PER_LOT {
            truncated = true;
        }
        let mut projected = Vec::new();
        for requirement in requirements.iter().take(MAX_PROJECTED_SUBSEQUENT_PER_LOT) {
            if total == MAX_PROJECTED_SUBSEQUENT_REQUIREMENTS {
                truncated = true;
                break;
            }
            let Some(item) = requirement.as_object() else {
                truncated = true;
                continue;
            };
            if item.get("source_element").and_then(serde_json::Value::as_str) != Some("SubsequentProcessTenderRequirement")
                || item.get("source_path").and_then(serde_json::Value::as_str) != Some("TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name")
            { truncated = true; continue; }
            let Some(path) = item
                .get("source_ordinal_path")
                .and_then(serde_json::Value::as_array)
                .filter(|path| path.len() == 1)
            else {
                truncated = true;
                continue;
            };
            let Some(index) = path[0].as_u64().filter(|index| *index <= 10_000) else {
                truncated = true;
                continue;
            };
            let name = match item.get("name") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(value)) => {
                    if value.chars().count() > MAX_PROJECTED_SUBSEQUENT_NAME_CHARS {
                        truncated = true;
                    }
                    Some(
                        value
                            .chars()
                            .take(MAX_PROJECTED_SUBSEQUENT_NAME_CHARS)
                            .collect::<String>(),
                    )
                }
                Some(_) => {
                    truncated = true;
                    None
                }
            };
            projected.push(serde_json::json!({
                "source_element": "SubsequentProcessTenderRequirement",
                "source_path": "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name",
                "source_ordinal_path": [index],
                "name": name,
            }));
            total += 1;
        }
        evidence.push(serde_json::json!({"lot_id": lot_id, "requirements": projected}));
    }
    Some((serde_json::Value::Array(evidence), truncated))
}

const MAX_PROJECTED_GUARANTEE_CURRICULA_LOTS: usize = 20;
const MAX_PROJECTED_GUARANTEE_CURRICULA_RECORDS: usize = 100;
const MAX_PROJECTED_GUARANTEE_CURRICULA_PER_FAMILY_PER_LOT: usize = 20;
const MAX_PROJECTED_GUARANTEE_CURRICULA_TEXT_CHARS: usize = 2_000;

fn bounded_lot_guarantee_curricula_evidence(
    raw: Option<&str>,
    source_truncated: Option<&str>,
) -> Option<(serde_json::Value, bool)> {
    let source_truncated = match source_truncated.map(str::trim) {
        None | Some("false") | Some("0") | Some("") => false,
        Some("true") | Some("1") | Some(_) => true,
    };
    let Some(raw) = raw else {
        return source_truncated.then(|| (serde_json::Value::Array(Vec::new()), true));
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let Some(records) = value.as_array() else {
        return Some((serde_json::Value::Array(Vec::new()), true));
    };
    let mut truncated = source_truncated || records.len() > MAX_PROJECTED_GUARANTEE_CURRICULA_LOTS;
    let mut evidence =
        Vec::with_capacity(records.len().min(MAX_PROJECTED_GUARANTEE_CURRICULA_LOTS));
    let mut total = 0;
    for record in records.iter().take(MAX_PROJECTED_GUARANTEE_CURRICULA_LOTS) {
        let Some(object) = record.as_object() else {
            truncated = true;
            continue;
        };
        let lot_id = match object.get("lot_id") {
            Some(serde_json::Value::String(value))
                if !value.trim().is_empty() && value.chars().count() <= 128 =>
            {
                Some(value.clone())
            }
            Some(serde_json::Value::Null) => None,
            _ => {
                truncated = true;
                None
            }
        };
        let Some(guarantees) = object
            .get("guarantees")
            .and_then(serde_json::Value::as_array)
        else {
            truncated = true;
            continue;
        };
        let Some(curricula) = object
            .get("curricula")
            .and_then(serde_json::Value::as_array)
        else {
            truncated = true;
            continue;
        };
        let mut projected_guarantees = Vec::new();
        let mut projected_curricula = Vec::new();
        for (items, destination, source_element, source_path, has_description) in [
            (
                guarantees,
                &mut projected_guarantees,
                "RequiredFinancialGuarantee",
                "TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode",
                true,
            ),
            (
                curricula,
                &mut projected_curricula,
                "RequiredCurriculaCode",
                "TenderingTerms/RequiredCurriculaCode",
                false,
            ),
        ] {
            if items.len() > MAX_PROJECTED_GUARANTEE_CURRICULA_PER_FAMILY_PER_LOT {
                truncated = true;
            }
            for item in items
                .iter()
                .take(MAX_PROJECTED_GUARANTEE_CURRICULA_PER_FAMILY_PER_LOT)
            {
                if total == MAX_PROJECTED_GUARANTEE_CURRICULA_RECORDS {
                    truncated = true;
                    break;
                }
                let Some(item) = item.as_object() else {
                    truncated = true;
                    continue;
                };
                if item
                    .get("source_element")
                    .and_then(serde_json::Value::as_str)
                    != Some(source_element)
                    || item.get("source_path").and_then(serde_json::Value::as_str)
                        != Some(source_path)
                {
                    truncated = true;
                    continue;
                }
                let Some(path) = item
                    .get("source_ordinal_path")
                    .and_then(serde_json::Value::as_array)
                    .filter(|path| path.len() == 1)
                else {
                    truncated = true;
                    continue;
                };
                let Some(index) = path[0].as_u64().filter(|index| *index <= 10_000) else {
                    truncated = true;
                    continue;
                };
                let optional_text = |key: &str, truncated: &mut bool| match item.get(key) {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(value)) => {
                        if value.chars().count() > MAX_PROJECTED_GUARANTEE_CURRICULA_TEXT_CHARS {
                            *truncated = true;
                        }
                        Some(
                            value
                                .chars()
                                .take(MAX_PROJECTED_GUARANTEE_CURRICULA_TEXT_CHARS)
                                .collect::<String>(),
                        )
                    }
                    Some(_) => {
                        *truncated = true;
                        None
                    }
                };
                let code = optional_text("code", &mut truncated);
                let code_list_name = optional_text("code_list_name", &mut truncated);
                if has_description {
                    let description = optional_text("description", &mut truncated);
                    destination.push(serde_json::json!({
                        "source_element": source_element,
                        "source_path": source_path,
                        "source_ordinal_path": [index],
                        "code": code,
                        "code_list_name": code_list_name,
                        "description": description,
                    }));
                } else {
                    destination.push(serde_json::json!({
                        "source_element": source_element,
                        "source_path": source_path,
                        "source_ordinal_path": [index],
                        "code": code,
                        "code_list_name": code_list_name,
                    }));
                }
                total += 1;
            }
        }
        evidence.push(serde_json::json!({
            "lot_id": lot_id,
            "guarantees": projected_guarantees,
            "curricula": projected_curricula,
        }));
    }
    Some((serde_json::Value::Array(evidence), truncated))
}

struct ListRowRawEvidence<'a> {
    workspace: Option<&'a str>,
    folder: Option<&'a str>,
    container: Option<&'a str>,
    framework_maximum_amounts: Option<&'a str>,
    notice_lot_cpv_evidence: Option<&'a str>,
    framework_maximum_evidence: Option<&'a str>,
    framework_maximum_evidence_truncated: Option<&'a str>,
    notice_lot_ids_unique: Option<&'a str>,
    notice_lot_place_evidence: Option<&'a str>,
    notice_lot_place_evidence_truncated: Option<&'a str>,
    notice_lot_award_criteria_evidence: Option<&'a str>,
    notice_lot_award_criteria_evidence_truncated: Option<&'a str>,
    notice_lot_selection_evidence: Option<&'a str>,
    notice_lot_selection_evidence_truncated: Option<&'a str>,
    notice_lot_contract_execution_evidence: Option<&'a str>,
    notice_lot_contract_execution_evidence_truncated: Option<&'a str>,
    notice_lot_subsequent_process_evidence: Option<&'a str>,
    notice_lot_subsequent_process_evidence_truncated: Option<&'a str>,
    notice_lot_guarantee_curricula_evidence: Option<&'a str>,
    notice_lot_guarantee_curricula_evidence_truncated: Option<&'a str>,
    notice_lot_ted_evidence: Option<&'a str>,
}

const MAX_TED_LOT_EVIDENCE_BYTES: usize = 4_096;
const TED_LOT_EVIDENCE_FIELDS: &[&str] = &[
    "lot_id",
    "main_classification",
    "estimated_value",
    "estimated_currency",
    "framework_maximum",
    "framework_currency",
];

fn bounded_ted_lot_evidence(raw: Option<&str>) -> Option<serde_json::Value> {
    let text = raw?;
    if text.len() > MAX_TED_LOT_EVIDENCE_BYTES {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let object = value.as_object()?;
    if object.get("association_rule")?.as_str()?
        != "singleton_field_cardinality_only_no_positional_join"
    {
        return None;
    }
    let fields = object.get("fields")?.as_object()?;
    let mut bounded_fields = serde_json::Map::new();
    for field in TED_LOT_EVIDENCE_FIELDS {
        let summary = fields.get(*field)?.as_object()?;
        let count = summary.get("source_cardinality")?.as_u64()?;
        let status = summary.get("status")?.as_str()?;
        if count > 1_000
            || !matches!(
                status,
                "not_recorded" | "singleton" | "invalid_singleton" | "multiple_values"
            )
        {
            return None;
        }
        let singleton = summary
            .get("singleton_value")
            .filter(|value| !value.is_null())
            .and_then(serde_json::Value::as_str);
        if singleton.is_some_and(|value| value.chars().count() > 256)
            || (status == "singleton" && (count != 1 || singleton.is_none()))
            || (status == "multiple_values" && (count <= 1 || singleton.is_some()))
            || (status == "invalid_singleton" && (count != 1 || singleton.is_some()))
            || (status == "not_recorded" && (count != 0 || singleton.is_some()))
        {
            return None;
        }
        bounded_fields.insert(
            (*field).to_owned(),
            serde_json::json!({
                "source_cardinality": count,
                "singleton_value": singleton,
                "status": status,
            }),
        );
    }
    Some(serde_json::json!({
        "association_rule": "singleton_field_cardinality_only_no_positional_join",
        "fields": bounded_fields,
    }))
}

fn list_row_raw(evidence: ListRowRawEvidence<'_>) -> serde_json::Value {
    let ListRowRawEvidence {
        workspace,
        folder,
        container,
        framework_maximum_amounts,
        notice_lot_cpv_evidence,
        framework_maximum_evidence,
        framework_maximum_evidence_truncated,
        notice_lot_ids_unique,
        notice_lot_place_evidence,
        notice_lot_place_evidence_truncated,
        notice_lot_award_criteria_evidence,
        notice_lot_award_criteria_evidence_truncated,
        notice_lot_selection_evidence,
        notice_lot_selection_evidence_truncated,
        notice_lot_contract_execution_evidence,
        notice_lot_contract_execution_evidence_truncated,
        notice_lot_subsequent_process_evidence,
        notice_lot_subsequent_process_evidence_truncated,
        notice_lot_guarantee_curricula_evidence,
        notice_lot_guarantee_curricula_evidence_truncated,
        notice_lot_ted_evidence,
    } = evidence;
    let mut raw = serde_json::Map::new();
    for (field, value) in [
        ("bosa_publication_workspace_id", workspace),
        ("contract_folder_id", folder),
        ("dossier_container", container),
    ] {
        if let Some(value) = value {
            if !value.is_empty() {
                raw.insert(
                    field.to_owned(),
                    serde_json::Value::String(value.to_owned()),
                );
            }
        }
    }
    if let Some(notice_lot_ids_unique) = notice_lot_ids_unique {
        raw.insert(
            "notice_lot_ids_unique".to_owned(),
            serde_json::Value::Bool(matches!(notice_lot_ids_unique.trim(), "true" | "1")),
        );
    }
    if let Some(amounts) = framework_maximum_amounts
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .filter(|values| values.as_array().is_some_and(|values| !values.is_empty()))
    {
        raw.insert("framework_maximum_amounts".to_owned(), amounts);
    }
    if let Some((evidence, truncated)) = bounded_framework_maximum_evidence(
        framework_maximum_evidence,
        framework_maximum_evidence_truncated,
    ) {
        raw.insert("framework_maximum_evidence".to_owned(), evidence);
        if truncated {
            raw.insert(
                "framework_maximum_evidence_truncated".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Some((evidence, truncated)) = bounded_lot_cpv_evidence(notice_lot_cpv_evidence) {
        raw.insert("notice_lot_cpv_evidence".to_owned(), evidence);
        if truncated {
            raw.insert(
                "notice_lot_cpv_evidence_truncated".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Some((evidence, truncated)) = bounded_lot_place_evidence(
        notice_lot_place_evidence,
        notice_lot_place_evidence_truncated,
    ) {
        raw.insert("notice_lot_place_evidence".to_owned(), evidence);
        if truncated {
            raw.insert(
                "notice_lot_place_evidence_truncated".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Some((evidence, truncated)) = bounded_lot_award_criteria_evidence(
        notice_lot_award_criteria_evidence,
        notice_lot_award_criteria_evidence_truncated,
    ) {
        raw.insert("notice_lot_award_criteria_evidence".to_owned(), evidence);
        if truncated {
            raw.insert(
                "notice_lot_award_criteria_evidence_truncated".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Some((evidence, truncated)) = bounded_lot_selection_evidence(
        notice_lot_selection_evidence,
        notice_lot_selection_evidence_truncated,
    ) {
        raw.insert("notice_lot_selection_evidence".to_owned(), evidence);
        if truncated {
            raw.insert(
                "notice_lot_selection_evidence_truncated".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Some((evidence, truncated)) = bounded_lot_contract_execution_evidence(
        notice_lot_contract_execution_evidence,
        notice_lot_contract_execution_evidence_truncated,
    ) {
        raw.insert(
            "notice_lot_contract_execution_evidence".to_owned(),
            evidence,
        );
        if truncated {
            raw.insert(
                "notice_lot_contract_execution_evidence_truncated".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Some((evidence, truncated)) = bounded_lot_subsequent_process_evidence(
        notice_lot_subsequent_process_evidence,
        notice_lot_subsequent_process_evidence_truncated,
    ) {
        raw.insert(
            "notice_lot_subsequent_process_evidence".to_owned(),
            evidence,
        );
        if truncated {
            raw.insert(
                "notice_lot_subsequent_process_evidence_truncated".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Some((evidence, truncated)) = bounded_lot_guarantee_curricula_evidence(
        notice_lot_guarantee_curricula_evidence,
        notice_lot_guarantee_curricula_evidence_truncated,
    ) {
        raw.insert(
            "notice_lot_guarantee_curricula_evidence".to_owned(),
            evidence,
        );
        if truncated {
            raw.insert(
                "notice_lot_guarantee_curricula_evidence_truncated".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
    }
    if let Some(evidence) = bounded_ted_lot_evidence(notice_lot_ted_evidence) {
        raw.insert("notice_lot_ted_evidence".to_owned(), evidence);
    }
    if raw.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Object(raw)
    }
}

fn migrate_decimal_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<(), duckdb::Error> {
    let data_type = connection.query_row(
        "SELECT data_type FROM information_schema.columns WHERE table_name = ? AND column_name = ?",
        params![table, column],
        |row| row.get::<_, String>(0),
    )?;
    if data_type != "DECIMAL(18,2)" {
        connection.execute(
            &format!("ALTER TABLE {table} ALTER COLUMN {column} SET DATA TYPE DECIMAL(18,2)"),
            [],
        )?;
    }
    Ok(())
}

/// Price-benchmark rows: per-CPV-division aggregates over the flat TED history.
#[derive(Debug, Clone, PartialEq)]
pub struct CpvSpread {
    pub cpv_division: String,
    pub awards: usize,
    pub avg_awarded: Option<Decimal>,
    pub avg_offers: Option<f64>,
    pub single_bid_share: f64,
}

/// Offer-range rows: per-division averages of lowest/highest admissible bids.
#[derive(Debug, Clone, PartialEq)]
pub struct LotRangeSpread {
    pub cpv_division: String,
    pub lots: usize,
    pub avg_awarded: Option<Decimal>,
    pub avg_lowest: Option<Decimal>,
    pub avg_highest: Option<Decimal>,
}

/// Where a capacity turnover figure came from, for callers that explain it.
///
/// `period_end` keeps the strings the notice always used ("unknown",
/// "unparseable") so the CLI text does not change shape when a filing is
/// undated. `age_days` stays `None` in exactly those cases: an age that cannot
/// be computed is unknown, not zero.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NbbTurnoverProvenance {
    pub fiscal_year: i64,
    pub period_end: String,
    pub age_days: Option<i64>,
    pub as_of: String,
    pub source_ref: String,
}

/// Age of a filing in days, from its NBB period end.
///
/// `None` when the filing carries no period end or an unparseable one: an age
/// that cannot be computed is unknown, not zero. Shared so the audit and the
/// capacity triage cannot disagree about what "age" means.
pub fn filing_age_days(period_end: Option<&str>, as_of: NaiveDate) -> Option<i64> {
    let raw = period_end?;
    let parsed = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    Some((as_of - parsed).num_days())
}

impl NbbTurnoverProvenance {
    /// The CLI notice, in the wording it has always used.
    pub fn notice(&self) -> String {
        self.notice_with_limit(None)
    }

    /// The same notice, but stating the limit actually in force. Without this
    /// the line would claim "no freshness threshold is applied" while the
    /// caller had just asked for one.
    pub fn notice_with_limit(&self, max_age_days: Option<i64>) -> String {
        let age = match self.age_days {
            Some(days) => days.to_string(),
            None => "unknown".to_owned(),
        };
        let closing = match max_age_days {
            Some(limit) => format!("Age is descriptive; a {limit} day freshness limit was applied and this filing is within it."),
            None => "Age is descriptive only; no freshness threshold is applied.".to_owned(),
        };
        format!(
            "Capacity triage NBB turnover provenance: fiscal_year={}, period_end={}, age_days={} as_of={}, source_ref={}. {}",
            self.fiscal_year, self.period_end, age, self.as_of, self.source_ref, closing
        )
    }

    /// Short clause for a warning sentence, so a demotion names its evidence.
    pub fn short_clause(&self) -> String {
        match self.age_days {
            Some(days) => format!("boekjaar FY{}, {} dagen oud", self.fiscal_year, days),
            None => format!("boekjaar FY{}, leeftijd onbekend", self.fiscal_year),
        }
    }
}

/// Turnover for capacity triage, taken from the first stored filing.
///
/// The single place that decides which filing represents a company, so the CLI,
/// the API and `capacity-check` cannot drift apart. `financials` must come from
/// `nbb_financials_for`, whose ordering puts the current period of the newest
/// fiscal year first. There is deliberately **no fallback** to an older year
/// when that filing discloses no turnover: an absent figure stays unknown
/// rather than being borrowed from a different period.
///
/// The second return value says where the figure came from, so a caller can
/// print it or cite it in a warning. Age is descriptive only; no freshness
/// threshold is applied.
pub fn nbb_capacity_turnover(
    financials: &[NbbFinancials],
    as_of: NaiveDate,
    max_age_days: Option<i64>,
) -> (Option<f64>, Option<NbbTurnoverProvenance>) {
    let Some(financial) = financials.first() else {
        return (None, None);
    };
    let Some(turnover) = financial.turnover.as_ref().and_then(ToPrimitive::to_f64) else {
        return (None, None);
    };

    let (period_end, age_days) = match financial.period_end.as_deref() {
        None => ("unknown".to_owned(), None),
        Some(raw_period_end) => match NaiveDate::parse_from_str(raw_period_end, "%Y-%m-%d") {
            Ok(period_end) => (
                period_end.format("%Y-%m-%d").to_string(),
                filing_age_days(financial.period_end.as_deref(), as_of),
            ),
            Err(_) => ("unparseable".to_owned(), None),
        },
    };
    let safe_reference = financial
        .source_ref
        .chars()
        .take(80)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let provenance = NbbTurnoverProvenance {
        fiscal_year: financial.fiscal_year,
        period_end,
        age_days,
        as_of: as_of.format("%Y-%m-%d").to_string(),
        source_ref: safe_reference,
    };
    // An opt-in limit withholds a figure that cannot be shown to be recent
    // enough. An undated filing cannot be shown either, so it is withheld too
    // rather than assumed fresh. The provenance is still returned: the caller
    // needs it to say why the figure is missing instead of claiming there is
    // none.
    if let Some(limit) = max_age_days {
        match provenance.age_days {
            Some(age) if age <= limit => {}
            _ => return (None, Some(provenance)),
        }
    }
    (Some(turnover), Some(provenance))
}

/// What a capacity report could and could not assess.
///
/// `lots_with_value` is the denominator; the `without_*` fields account for
/// every lot that did not become an assessed comparison, and they are disjoint:
/// no identifier, not a Belgian enterprise number, no filing, no usable
/// turnover.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CapacityCoverage {
    pub lots_with_value: i64,
    pub without_winner_identifier: i64,
    /// Winners whose identifier is not a Belgian enterprise number (a foreign
    /// VAT number, a SIREN, or a truncated value). NBB cannot answer for these,
    /// so they are not evidence of a missing filing.
    pub not_belgian_identifier: i64,
    pub without_financials: i64,
    pub without_turnover: i64,
    pub assessed: i64,
}

impl CapacityCoverage {
    pub fn unassessed(&self) -> i64 {
        self.without_winner_identifier
            + self.not_belgian_identifier
            + self.without_financials
            + self.without_turnover
    }

    /// One-line summary for CLI output. States the gap explicitly so an empty
    /// report cannot be mistaken for a clean one.
    pub fn describe(&self) -> String {
        if self.lots_with_value == 0 {
            return "no awarded lots with a value: capacity is unknown".to_owned();
        }
        format!(
            "{} of {} awarded lot(s) could not be assessed ({} without a winner identifier, \
             {} without a Belgian enterprise number, {} without any NBB filing, \
             {} with a filing but no usable turnover); {} assessed",
            self.unassessed(),
            self.lots_with_value,
            self.without_winner_identifier,
            self.not_belgian_identifier,
            self.without_financials,
            self.without_turnover,
            self.assessed,
        )
    }
}

/// Period-boundary coverage of `nbb_financials`.
///
/// A store imported by an older importer can hold rows without period
/// boundaries, which silently makes every filing undatable. The archive audit
/// then reports zero comparable evidence rather than a stale store, so the
/// condition has to be visible at import time.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NbbPeriodCoverage {
    pub rows: i64,
    pub with_period_start: i64,
    pub with_period_end: i64,
    pub with_both_boundaries: i64,
}

impl NbbPeriodCoverage {
    /// Rows that cannot be dated at all.
    pub fn undatable(&self) -> i64 {
        self.rows - self.with_both_boundaries
    }

    /// One-line summary for CLI output. Names the shortfall instead of
    /// reporting a successful import count on its own.
    pub fn describe(&self) -> String {
        if self.rows == 0 {
            return "nbb_financials is empty: no filing can be dated".to_owned();
        }
        format!(
            "nbb period coverage: {} of {} row(s) carry both boundaries ({} start, {} end); \
             {} undatable",
            self.with_both_boundaries,
            self.rows,
            self.with_period_start,
            self.with_period_end,
            self.undatable(),
        )
    }
}

/// One awarded lot whose value is large relative to the winner's turnover.
/// Winners without financials (or without a usable KBO) never appear here:
/// unknown capacity is a coverage gap, not a clean bill of health.
#[derive(Debug, Clone, PartialEq)]
pub struct CapacityOutlier {
    pub publication_number: String,
    pub lot_id: String,
    pub winner_name: Option<String>,
    pub winner_kbo: String,
    pub award_value: Decimal,
    pub fiscal_year: i64,
    /// Period end of the filing the turnover came from, so the report can state
    /// its age instead of implying the figure is current.
    pub period_end: Option<String>,
    pub turnover: Decimal,
    pub staff_fte: Option<Decimal>,
    pub value_to_turnover: f64,
}

fn decimal_avg(sum: Decimal, n: usize) -> Option<Decimal> {
    if n == 0 {
        None
    } else {
        Some((sum / Decimal::from(n)).round_dp(2))
    }
}

#[derive(Debug, Default)]
struct RangeAccum {
    lots: usize,
    value_sum: Decimal,
    value_n: usize,
    low_sum: Decimal,
    low_n: usize,
    high_sum: Decimal,
    high_n: usize,
}

impl TenderStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, duckdb::Error> {
        let connection = open_write_connection(path.as_ref())?;
        connection.execute_batch(SCHEMA)?;
        for (table, column) in [
            ("tenders", "estimated_value"),
            ("preferences", "minimum_value"),
            ("preferences", "maximum_value"),
        ] {
            migrate_decimal_column(&connection, table, column)?;
        }
        // Databases written before the notice type was requested have no such
        // column; their rows keep `NULL` and stay eligible as opportunities.
        connection.execute(
            "ALTER TABLE tenders ADD COLUMN IF NOT EXISTS notice_type VARCHAR",
            [],
        )?;
        // Lot values postdate the notice type; older rows assess at the
        // notice total, exactly as before.
        connection.execute(
            "ALTER TABLE tenders ADD COLUMN IF NOT EXISTS lot_values JSON",
            [],
        )?;
        // Recognition declarations are optional; older databases simply have no
        // such columns until one is saved.
        connection.execute(
            "ALTER TABLE tenders ADD COLUMN IF NOT EXISTS publication_date DATE",
            [],
        )?;
        connection.execute(
            "ALTER TABLE preferences ADD COLUMN IF NOT EXISTS recognition_class INTEGER",
            [],
        )?;
        connection.execute(
            "ALTER TABLE preferences ADD COLUMN IF NOT EXISTS recognition_categories JSON",
            [],
        )?;
        // Period boundaries were captured by the NBB parsers before they were
        // persisted. Older rows remain NULL rather than deriving dates from FY.
        connection.execute(
            "ALTER TABLE nbb_financials ADD COLUMN IF NOT EXISTS period_start VARCHAR",
            [],
        )?;
        connection.execute(
            "ALTER TABLE nbb_financials ADD COLUMN IF NOT EXISTS period_end VARCHAR",
            [],
        )?;
        // Offer-range columns postdate the value column; older databases gain
        // NULLs (unknown, never zero) until a re-ingest fills them.
        connection.execute(
            "ALTER TABLE award_lots ADD COLUMN IF NOT EXISTS lowest_value DECIMAL(18, 2)",
            [],
        )?;
        connection.execute(
            "ALTER TABLE award_lots ADD COLUMN IF NOT EXISTS highest_value DECIMAL(18, 2)",
            [],
        )?;
        // Split relevance label: topic-relevant vs realistically pursuable, plus
        // bid outcome. Older databases gain NULL columns; reads fall back to
        // `relevant` when the new columns are absent.
        connection.execute(
            "ALTER TABLE feedback ADD COLUMN IF NOT EXISTS pursuable BOOLEAN",
            [],
        )?;
        connection.execute(
            "ALTER TABLE feedback ADD COLUMN IF NOT EXISTS outcome VARCHAR",
            [],
        )?;
        // Evidence snapshot columns for databases whose refresh_runs table
        // predates them; read as absent when missing.
        for column in [
            "evidence_opportunities",
            "evidence_with_deadline",
            "evidence_with_value",
            "evidence_with_requirements",
            "evidence_stated",
            "evidence_hinted",
            "evidence_unknown",
        ] {
            connection.execute(
                &format!("ALTER TABLE refresh_runs ADD COLUMN IF NOT EXISTS {column} INTEGER"),
                [],
            )?;
        }
        connection.execute(
            "CREATE INDEX IF NOT EXISTS tenders_deadline_idx ON tenders(deadline)",
            [],
        )?;
        connection.execute(
            "CREATE INDEX IF NOT EXISTS analysis_cache_created_idx ON analysis_cache(created_at)",
            [],
        )?;
        Ok(Self {
            schema: SchemaColumns::probe(&connection)?,
            connection,
        })
    }

    /// Open an existing database for writing without re-running the schema,
    /// migrations or index creation. The caller must have opened the same file
    /// with [`TenderStore::open`] in this process first, as the API does at
    /// startup; otherwise those eleven statements ran on every write request.
    pub fn open_initialized(path: impl AsRef<Path>) -> Result<Self, duckdb::Error> {
        let connection = open_write_connection(path.as_ref())?;
        Ok(Self {
            schema: SchemaColumns::probe(&connection)?,
            connection,
        })
    }

    /// Open an existing database without creating or migrating its schema.
    /// Read-only connections let several reader processes query the same
    /// file at once and never contend with writes, but DuckDB still refuses
    /// to open alongside another process's write lock, so brief overlaps are
    /// retried.
    ///
    /// A database written before a column existed is still readable: the read
    /// statements are built from the columns the file actually has.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, duckdb::Error> {
        for attempt in 0..OPEN_LOCK_ATTEMPTS {
            let config = Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
            match Connection::open_with_flags(path.as_ref(), config) {
                Ok(connection) => {
                    return Ok(Self {
                        schema: SchemaColumns::probe(&connection)?,
                        connection,
                    })
                }
                Err(error)
                    if attempt + 1 < OPEN_LOCK_ATTEMPTS && is_retryable_write_error(&error) =>
                {
                    thread::sleep(OPEN_LOCK_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("the lock retry loop returns from inside its body")
    }

    pub fn upsert(&self, tender: &Tender) -> Result<(), Box<dyn Error>> {
        if matches!(tender.source.as_str(), "bda" | "bda-int") {
            self.with_transaction(|| self.upsert_with_identity(tender))
        } else {
            self.upsert_tender_row(tender)
        }
    }

    /// Look up a BOSA workspace without losing provenance to lightweight list rows.
    pub fn bosa_workspace_tender(
        &self,
        source: &str,
        workspace_id: &str,
    ) -> Result<Option<Tender>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            "SELECT external_id FROM tenders WHERE source = ? AND json_extract_string(raw, '$.bosa_publication_workspace_id') = ? LIMIT 2"
        )?;
        let ids = statement
            .query_map(params![source, workspace_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        match ids.as_slice() {
            [] => Ok(None),
            [id] => self.tender(source, id),
            _ => Err("multiple stored tenders match BOSA workspace".into()),
        }
    }

    /// All stored API notices for a folder; refuse truncation rather than imply completeness.
    pub fn bosa_folder_notices(
        &self,
        source: &str,
        folder: &str,
    ) -> Result<Vec<Tender>, Box<dyn Error>> {
        if !matches!(source, "bda" | "bda-int") || folder.trim().is_empty() {
            return Err("invalid BOSA source or folder".into());
        }
        let mut statement = self.connection.prepare(
            "SELECT external_id FROM tenders WHERE source = ? AND json_extract_string(raw, '$.contract_folder_id') = ? AND json_extract_string(raw, '$.bosa_publication_workspace_id') IS NOT NULL ORDER BY external_id LIMIT 101"
        )?;
        let ids = statement
            .query_map(params![source, folder], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > 100 {
            return Err("folder has more than 100 notices; report would be incomplete".into());
        }
        ids.iter()
            .map(|id| {
                self.tender(source, id)?
                    .ok_or_else(|| "BOSA notice disappeared".into())
            })
            .collect()
    }

    /// Update attachment evidence only if the fetched notice is still current.
    /// This avoids a stale enrichment overwriting a concurrent correction.
    pub fn replace_attachment_evidence(
        &self,
        source: &str,
        external_id: &str,
        workspace_id: &str,
        expected_notice_id: &str,
        evidence: &[crate::bosa::evidence::AttachmentEvidence],
    ) -> Result<usize, Box<dyn Error>> {
        self.with_transaction(|| {
            let mut tender = self
                .tender(source, external_id)?
                .ok_or("tender not found")?;
            if tender
                .raw
                .get("bosa_publication_workspace_id")
                .and_then(serde_json::Value::as_str)
                != Some(workspace_id)
                || tender
                    .raw
                    .get("bosa_notice_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(expected_notice_id)
            {
                return Err(
                    "notice changed during extraction; retry against the current dossier".into(),
                );
            }
            let count = crate::bosa::evidence::replace_evidence(&mut tender, evidence)?;
            self.upsert_tender_row(&tender)?;
            Ok(count)
        })
    }

    // Caller supplies the transaction so batch imports never nest BEGINs.
    fn upsert_with_identity(&self, tender: &Tender) -> Result<(), Box<dyn Error>> {
        self.upsert_with_identity_at(tender, None)
    }

    fn upsert_with_identity_at(
        &self,
        tender: &Tender,
        fetched_at: Option<&str>,
    ) -> Result<(), Box<dyn Error>> {
        if tender.source == "bda-int" {
            if let Some(existing) = self.tender("bda-int", &tender.external_id)? {
                let left = existing
                    .raw
                    .get("bosa_publication_workspace_id")
                    .and_then(serde_json::Value::as_str);
                let right = tender
                    .raw
                    .get("bosa_publication_workspace_id")
                    .and_then(serde_json::Value::as_str);
                if left != right || left.is_none() {
                    return Err("INT workspace identity collision; refusing overwrite".into());
                }
                let same_notice = existing
                    .raw
                    .get("bosa_notice_id")
                    .and_then(serde_json::Value::as_str)
                    .zip(
                        tender
                            .raw
                            .get("bosa_notice_id")
                            .and_then(serde_json::Value::as_str),
                    )
                    .is_some_and(|(a, b)| a == b);
                let order = bosa_revision_order(&existing, tender);
                if (order.is_none() || order == Some(std::cmp::Ordering::Equal)) && !same_notice {
                    return Err("ambiguous INT correction ordering; refusing overwrite".into());
                }
                if order.is_some_and(|o| o.is_gt()) {
                    return Ok(());
                }
                let mut winner = tender.clone();
                if same_notice {
                    if let Some(records) = existing.raw.get("bosa_attachment_evidence") {
                        let evidence: Vec<crate::bosa::evidence::AttachmentEvidence> =
                            serde_json::from_value(records.clone())?;
                        crate::bosa::evidence::replace_evidence(&mut winner, &evidence)?;
                    }
                }
                return self.upsert_tender_row_with_timestamp(&winner, fetched_at);
            }
            return self.upsert_tender_row_with_timestamp(tender, fetched_at);
        }
        if tender.source != "bda" {
            return self.upsert_tender_row_with_timestamp(tender, fetched_at);
        }
        let folder = tender
            .raw
            .get("contract_folder_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty());
        let mut winner = tender.clone();
        let mut aliases = Vec::new();
        if let Some(workspace) = tender
            .raw
            .get("bosa_publication_workspace_id")
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty())
        {
            if let Some(existing) = self.bosa_workspace_tender("bda", workspace)? {
                if existing.raw.get("contract_folder_id") != tender.raw.get("contract_folder_id") {
                    return Err(
                        "BOSA workspace changed procurement folder; refusing identity reassignment"
                            .into(),
                    );
                }
                winner.external_id = existing.external_id.clone();
                let order = bosa_revision_order(&existing, &winner);
                let same_notice = existing
                    .raw
                    .get("bosa_notice_id")
                    .and_then(serde_json::Value::as_str)
                    .zip(
                        winner
                            .raw
                            .get("bosa_notice_id")
                            .and_then(serde_json::Value::as_str),
                    )
                    .is_some_and(|(a, b)| a == b);
                if order.is_none() && !same_notice {
                    return Err(
                        "BOSA correction lacks comparable timestamps; refusing overwrite".into(),
                    );
                }
                if order == Some(std::cmp::Ordering::Equal) && !same_notice {
                    return Err("ambiguous BOSA revision ordering; refusing to overwrite".into());
                }
                if order.is_some_and(|o| o.is_gt()) {
                    return Ok(());
                }
                if same_notice {
                    if let Some(records) = existing.raw.get("bosa_attachment_evidence") {
                        let evidence: Vec<crate::bosa::evidence::AttachmentEvidence> =
                            serde_json::from_value(records.clone())?;
                        crate::bosa::evidence::replace_evidence(&mut winner, &evidence)?;
                    }
                }
                return self.upsert_tender_row_with_timestamp(&winner, fetched_at);
            }
            if let Some(folder) = folder {
                // Existing canonical folder belongs to a different workspace: keep
                // it and assign the incoming notice its stable workspace identity.
                if let Some(existing) = self.tender("bda", folder)? {
                    if existing
                        .raw
                        .get("contract_folder_id")
                        .and_then(serde_json::Value::as_str)
                        != Some(folder)
                    {
                        return Err("BDA canonical identity conflicts with unrelated notice".into());
                    }
                    if existing
                        .raw
                        .get("bosa_publication_workspace_id")
                        .and_then(serde_json::Value::as_str)
                        .is_some()
                    {
                        winner.external_id = format!("bosa-workspace:{workspace}");
                        if self.tender("bda", &winner.external_id)?.is_some() {
                            return Err(
                                "BOSA workspace identity conflicts with an existing row".into()
                            );
                        }
                        return self.upsert_tender_row_with_timestamp(&winner, fetched_at);
                    }
                    // A manual import may represent any workspace in the folder.
                    // Do not claim its identity without evidence.
                    return Err(
                        "manual BDA folder already exists; workspace linkage requires review"
                            .into(),
                    );
                }
            }
        } else if let Some(folder) = folder {
            let mut statement = self.connection.prepare("SELECT count(*) FROM tenders WHERE source = 'bda' AND json_extract_string(raw, '$.contract_folder_id') = ? AND json_extract_string(raw, '$.bosa_publication_workspace_id') IS NOT NULL")?;
            let count: i64 = statement.query_row(params![folder], |row| row.get(0))?;
            if count > 0 {
                return Err("manual BDA import conflicts with API workspace identities; review before merging".into());
            }
        }
        if let Some(folder) = folder {
            winner.external_id = folder.to_owned();
            let mut statement = self.connection.prepare(
                "SELECT external_id FROM tenders WHERE source = 'bda' AND (
                    external_id = ? OR json_extract_string(raw, '$.contract_folder_id') = ?
                ) ORDER BY external_id",
            )?;
            let ids = statement
                .query_map(params![folder, folder], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for id in ids {
                let existing = self
                    .tender("bda", &id)?
                    .ok_or("BDA row disappeared during consolidation")?;
                if existing
                    .raw
                    .get("contract_folder_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(folder)
                {
                    return Err(
                        "BDA canonical identity conflicts with an unrelated stored notice".into(),
                    );
                }
                let existing_workspace = existing
                    .raw
                    .get("bosa_publication_workspace_id")
                    .and_then(serde_json::Value::as_str);
                let incoming_workspace = winner
                    .raw
                    .get("bosa_publication_workspace_id")
                    .and_then(serde_json::Value::as_str);
                if let (Some(left), Some(right)) = (existing_workspace, incoming_workspace) {
                    if left != right {
                        return Err("distinct BOSA workspaces share a procurement folder; refusing to overwrite until lifecycle reconciliation is available".into());
                    }
                }
                let bosa_order = bosa_revision_order(&existing, &winner);
                if bosa_order == Some(std::cmp::Ordering::Equal)
                    && existing.raw.get("bosa_notice_id") != winner.raw.get("bosa_notice_id")
                {
                    return Err("ambiguous BOSA revision ordering; refusing to overwrite".into());
                }
                if bosa_order == Some(std::cmp::Ordering::Equal)
                    && existing.raw.get("bosa_notice_id") == winner.raw.get("bosa_notice_id")
                {
                    if let Some(records) = existing.raw.get("bosa_attachment_evidence") {
                        let evidence: Vec<crate::bosa::evidence::AttachmentEvidence> =
                            serde_json::from_value(records.clone())?;
                        crate::bosa::evidence::replace_evidence(&mut winner, &evidence)?;
                    }
                }
                if bosa_order
                    .unwrap_or_else(|| {
                        crate::bda::compare_notice_versions(
                            existing
                                .raw
                                .get("notice_version")
                                .and_then(serde_json::Value::as_str),
                            winner
                                .raw
                                .get("notice_version")
                                .and_then(serde_json::Value::as_str),
                        )
                    })
                    .is_gt()
                {
                    winner = existing;
                    winner.external_id = folder.to_owned();
                }
                if id != folder {
                    aliases.push(id);
                }
            }
        } else if let (Some(legacy), Some(source_name)) = (
            tender
                .raw
                .get("legacy_external_id")
                .and_then(serde_json::Value::as_str),
            tender
                .raw
                .get("source_name")
                .and_then(serde_json::Value::as_str),
        ) {
            if let Some(existing) = self.tender("bda", &tender.external_id)? {
                if existing
                    .raw
                    .get("source_name")
                    .and_then(serde_json::Value::as_str)
                    != Some(source_name)
                    || existing
                        .raw
                        .get("contract_folder_id")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|s| !s.trim().is_empty())
                    || existing
                        .raw
                        .get("notice_identifier")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|s| !s.trim().is_empty())
                {
                    return Err(
                        "BDA filename identity conflicts with an unrelated stored notice".into(),
                    );
                }
            }
            if legacy != tender.external_id {
                if let Some(existing) = self.tender("bda", legacy)? {
                    // A former same-stem XML/PDF collision belongs only to the
                    // source name actually retained in the legacy row.
                    if existing
                        .raw
                        .get("notice_identifier")
                        .and_then(serde_json::Value::as_str)
                        .is_none()
                        && existing
                            .raw
                            .get("source_name")
                            .and_then(serde_json::Value::as_str)
                            == Some(source_name)
                        && existing
                            .raw
                            .get("contract_folder_id")
                            .and_then(serde_json::Value::as_str)
                            .is_none()
                    {
                        aliases.push(legacy.to_owned());
                    }
                }
            }
        }
        self.upsert_tender_row_with_timestamp(&winner, fetched_at)?;
        self.move_bda_feedback_and_remove_aliases(&aliases, &winner.external_id)
    }

    fn move_bda_feedback_and_remove_aliases(
        &self,
        aliases: &[String],
        canonical: &str,
    ) -> Result<(), Box<dyn Error>> {
        if aliases.is_empty() {
            return Ok(());
        }
        let aliases = serde_json::to_string(aliases)?;
        // Preserve explicit canonical feedback. If only legacy choices exist,
        // choose each company's most recent one, with a stable tie-breaker.
        self.connection.execute(
            "INSERT INTO feedback (enterprise_number, source, external_id, relevant, pursuable, outcome, reason, created_at)
             SELECT enterprise_number, source, ?, relevant, pursuable, outcome, reason, created_at FROM (
                 SELECT *, row_number() OVER (
                     PARTITION BY enterprise_number ORDER BY created_at DESC, external_id ASC
                 ) AS priority
                 FROM feedback WHERE source = 'bda' AND external_id IN (
                     SELECT json_extract_string(value, '$') FROM json_each(?)
                 )
             ) WHERE priority = 1
             ON CONFLICT (enterprise_number, source, external_id) DO NOTHING",
            params![canonical, aliases],
        )?;
        self.connection.execute(
            "DELETE FROM feedback WHERE source = 'bda' AND external_id IN (
                SELECT json_extract_string(value, '$') FROM json_each(?)
            )",
            params![aliases],
        )?;
        self.connection.execute(
            "DELETE FROM tenders WHERE source = 'bda' AND external_id IN (
                SELECT json_extract_string(value, '$') FROM json_each(?)
            )",
            params![aliases],
        )?;
        Ok(())
    }

    fn upsert_tender_row(&self, tender: &Tender) -> Result<(), Box<dyn Error>> {
        self.upsert_tender_row_with_timestamp(tender, None)
    }

    fn upsert_tender_row_with_timestamp(
        &self,
        tender: &Tender,
        fetched_at: Option<&str>,
    ) -> Result<(), Box<dyn Error>> {
        let cpv_codes = serde_json::to_string(&tender.cpv_codes)?;
        let regions = serde_json::to_string(&tender.regions)?;
        let languages = serde_json::to_string(&tender.languages)?;
        let requirements = serde_json::to_string(&tender.requirements)?;
        let document_urls = serde_json::to_string(&tender.document_urls)?;
        let lot_values = serde_json::to_string(&tender.lot_values)?;
        let raw = serde_json::to_string(&tender.raw)?;
        retry_write(|| {
            self.connection.execute(
                r#"
            INSERT INTO tenders (
                source, external_id, title, buyer, description, cpv_codes, regions,
                estimated_value, lot_values, deadline, languages, notice_type, publication_date,
                requirements, document_urls, raw, fetched_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                      coalesce(CAST(? AS TIMESTAMPTZ), now()))
            ON CONFLICT (source, external_id) DO UPDATE SET
                title = excluded.title,
                buyer = excluded.buyer,
                description = excluded.description,
                cpv_codes = excluded.cpv_codes,
                regions = excluded.regions,
                estimated_value = excluded.estimated_value,
                lot_values = excluded.lot_values,
                deadline = excluded.deadline,
                languages = excluded.languages,
                notice_type = excluded.notice_type,
                publication_date = excluded.publication_date,
                requirements = excluded.requirements,
                document_urls = excluded.document_urls,
                raw = excluded.raw,
                fetched_at = coalesce(CAST(? AS TIMESTAMPTZ), now())
            "#,
                params![
                    tender.source,
                    tender.external_id,
                    tender.title,
                    tender.buyer,
                    tender.description,
                    cpv_codes,
                    regions,
                    tender.estimated_value,
                    lot_values,
                    tender.deadline,
                    languages,
                    tender.notice_type,
                    tender.publication_date,
                    requirements,
                    document_urls,
                    raw,
                    fetched_at,
                    fetched_at,
                ],
            )
        })?;
        Ok(())
    }

    fn with_transaction<T, F>(&self, mut operation: F) -> Result<T, Box<dyn Error>>
    where
        F: FnMut() -> Result<T, Box<dyn Error>>,
    {
        // The operation must be re-runnable: a batch that loses a
        // cross-connection conflict fails at COMMIT, so the whole transaction
        // is rolled back and retried. Non-retryable errors roll back and
        // propagate immediately.
        for attempt in 0..WRITE_ATTEMPTS {
            self.connection.execute_batch("BEGIN TRANSACTION")?;
            // Commit-time validation is exactly where cross-connection
            // conflicts surface, so a failed COMMIT must follow the same
            // rollback-and-retry path as a failed operation.
            let staged = operation().and_then(|value| {
                self.connection
                    .execute_batch("COMMIT")
                    .map(|()| value)
                    .map_err(|error| Box::new(error) as Box<dyn Error>)
            });
            match staged {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let _ = self.connection.execute_batch("ROLLBACK");
                    if attempt + 1 < WRITE_ATTEMPTS && boxed_error_is_retryable(error.as_ref()) {
                        thread::sleep(write_retry_delay(attempt));
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        unreachable!("the retry loop returns from inside its body")
    }

    pub fn upsert_many<'a, I>(&self, tenders: I) -> Result<usize, Box<dyn Error>>
    where
        I: IntoIterator<Item = &'a Tender>,
    {
        // Materialize so the whole batch can be retried after a conflict.
        let tenders: Vec<&Tender> = tenders.into_iter().collect();
        self.with_transaction(|| {
            for &tender in &tenders {
                self.upsert_with_identity(tender)?;
            }
            Ok(tenders.len())
        })
    }

    /// Upsert a bounded source snapshot while retaining the acquisition time
    /// already recorded in that source database.
    pub fn upsert_snapshot_many<'a, I>(&self, rows: I) -> Result<usize, Box<dyn Error>>
    where
        I: IntoIterator<Item = &'a TenderSnapshotRow>,
    {
        let rows: Vec<&TenderSnapshotRow> = rows.into_iter().collect();
        self.with_transaction(|| {
            for row in &rows {
                self.upsert_with_identity_at(&row.tender, Some(&row.fetched_at))?;
            }
            Ok(rows.len())
        })
    }

    pub fn count(&self) -> Result<i64, duckdb::Error> {
        self.connection
            .query_row("SELECT count(*) FROM tenders", [], |row| row.get(0))
    }

    pub fn count_source(&self, source: &str) -> Result<i64, duckdb::Error> {
        self.connection.query_row(
            "SELECT count(*) FROM tenders WHERE source = ?",
            params![source],
            |row| row.get(0),
        )
    }

    /// Aggregate and maximum serialized tender-field sizes for bounded exports.
    pub fn tender_payload_size_bytes(&self) -> Result<(i64, i64), duckdb::Error> {
        let sql = "WITH row_sizes AS (
                SELECT strlen(coalesce(source, ''))
                     + strlen(coalesce(external_id, ''))
                     + strlen(coalesce(title, ''))
                     + strlen(coalesce(buyer, ''))
                     + strlen(coalesce(description, ''))
                     + strlen(coalesce(CAST(cpv_codes AS VARCHAR), ''))
                     + strlen(coalesce(CAST(regions AS VARCHAR), ''))
                     + strlen(coalesce(CAST(estimated_value AS VARCHAR), ''))
                     + strlen(coalesce(CAST(deadline AS VARCHAR), ''))
                     + strlen(coalesce(CAST(languages AS VARCHAR), ''))
                     + strlen(coalesce(CAST(requirements AS VARCHAR), ''))
                     + strlen(coalesce(CAST(document_urls AS VARCHAR), ''))
                     + strlen(coalesce(CAST({notice_type} AS VARCHAR), ''))
                     + strlen(coalesce(CAST({publication_date} AS VARCHAR), ''))
                     + strlen(coalesce(CAST(raw AS VARCHAR), ''))
                     + strlen(coalesce(CAST({lot_values} AS VARCHAR), '')) AS row_bytes
                FROM tenders
            )
            SELECT coalesce(sum(row_bytes), 0), coalesce(max(row_bytes), 0) FROM row_sizes"
            .replace("{notice_type}", self.schema.notice_type_expression())
            .replace(
                "{publication_date}",
                self.schema.publication_date_expression(),
            )
            .replace("{lot_values}", self.schema.lot_values_expression());
        self.connection
            .query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
    }

    /// How many notices, and how many distinct procurements, a company could be
    /// matched against today. The pairing matters: 1,340 open notices can be 980
    /// procurements once the re-publications collapse, and only the second number
    /// is what a recommendation list can offer. Measured 2026-10-19 after a stale
    /// store silently made every recommendation irrelevant.
    pub fn open_opportunity_counts(&self, today: NaiveDate) -> Result<(i64, i64), Box<dyn Error>> {
        self.with_transaction(|| {
            let sql = format!(
                "SELECT count(*) FROM tenders WHERE (deadline IS NULL OR deadline >= CAST(? AS DATE)) AND {}",
                self.schema.opportunity_filter()
            );
            let notices: i64 = self.connection.query_row(&sql, params![today.to_string()], |row| row.get(0))?;
            let procurements = self.operative_notice_headers(None, MAX_NOTICE_IDENTITY_BYTES)?
                .iter().filter(|t| crate::recommendations::is_eligible(t, today, None)).count() as i64;
            Ok((notices, procurements))
        })
    }

    pub fn tender_stats(&self) -> Result<(i64, Option<String>), duckdb::Error> {
        self.connection.query_row(
            "SELECT count(*), CAST(max(fetched_at) AS VARCHAR) FROM tenders",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    /// Record which population an ingest run drew from. Called after a run
    /// stores, so a failed run that stored nothing leaves no row — matching
    /// the loud-clean-failure property (no database, no row).
    #[allow(clippy::too_many_arguments)]
    pub fn record_refresh_run(
        &self,
        kind: &str,
        scope: &str,
        window: &str,
        fetched: i64,
        stored: i64,
        total_notice_count: Option<i64>,
        duration_ms: i64,
    ) -> Result<i64, Box<dyn Error>> {
        if !matches!(
            kind,
            "refresh" | "backfill" | "refresh-partial" | "awards" | "awards-partial"
        ) {
            return Err(
                "refresh kind must be refresh, backfill, refresh-partial, awards or \
                 awards-partial"
                    .into(),
            );
        }
        retry_write(|| {
            self.connection.execute(
                "INSERT INTO refresh_runs \
                 (kind, scope, window_desc, fetched, stored, total_notice_count, duration_ms) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    kind,
                    scope,
                    window,
                    fetched,
                    stored,
                    total_notice_count,
                    duration_ms
                ],
            )
        })?;
        // Sequence-backed id of the row just written, so the caller can
        // attach the evidence snapshot to it. `currval` reads this session's
        // value, so a concurrent writer cannot hand us its id.
        let run_id: i64 =
            self.connection
                .query_row("SELECT currval('refresh_runs_id')", [], |row| row.get(0))?;
        Ok(run_id)
    }

    /// Attach the whole-store evidence snapshot to a recorded run. Called once
    /// per rolling refresh, after the upsert, so the numbers describe the store
    /// the next reader actually meets.
    pub fn record_refresh_evidence(
        &self,
        run_id: i64,
        evidence: &CorpusEvidence,
    ) -> Result<(), Box<dyn Error>> {
        retry_write(|| {
            self.connection.execute(
                "UPDATE refresh_runs SET evidence_opportunities = ?, evidence_with_deadline = ?, \
                 evidence_with_value = ?, evidence_with_requirements = ?, evidence_stated = ?, \
                 evidence_hinted = ?, evidence_unknown = ? WHERE id = ?",
                params![
                    evidence.opportunities as i64,
                    evidence.with_deadline as i64,
                    evidence.with_value as i64,
                    evidence.with_requirements as i64,
                    evidence.stated as i64,
                    evidence.hinted as i64,
                    evidence.unknown as i64,
                    run_id
                ],
            )
        })?;
        Ok(())
    }

    /// Date of the newest recorded *rolling refresh*, so an incremental refresh
    /// can start where the last successful run ended instead of re-reading its
    /// whole lookback window.
    ///
    /// Only a *completed* rolling refresh is a watermark. A `refresh-partial` row
    /// records that some chunks were stored before a later one failed, so treating
    /// it as coverage would make the next incremental run start after the range it
    /// never fetched — losing it for good. Backfill chunks are excluded for the
    /// same reason: one closed historical month says nothing about rolling
    /// coverage. The
    /// newest row is chosen by insertion order, the same rule `latest_refresh_run`
    /// uses, and the timestamp is read as text because DuckDB refuses a direct
    /// `TIMESTAMPTZ -> DATE` cast. A database without the table, or a value that
    /// will not parse, reads as "no watermark" rather than as a date.
    pub fn last_refresh_date(&self) -> Result<Option<NaiveDate>, Box<dyn Error>> {
        if !self.schema.refresh_runs {
            return Ok(None);
        }
        let mut statement = self.connection.prepare(
            "SELECT CAST(started_at AS VARCHAR) FROM refresh_runs WHERE kind = 'refresh' \
             ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = statement.query_map([], |row| row.get::<_, Option<String>>(0))?;
        let value = match rows.next() {
            Some(row) => row?,
            None => None,
        };
        Ok(value.and_then(|value| NaiveDate::parse_from_str(value.get(..10)?, "%Y-%m-%d").ok()))
    }

    /// Latest recorded run, or `None` on databases written before the table
    /// existed. Read-only consumers never fail on the missing table or
    /// columns; evidence predating the snapshot reads as absent.
    /// Rows that describe tender coverage. Award rows live in the same table but
    /// answer a different question, so they must not become "the last run" on a
    /// tender card.
    const TENDER_RUNS: &'static str = "kind NOT LIKE 'awards%'";
    /// Rows that describe an award-history fetch.
    const AWARD_RUNS: &'static str = "kind LIKE 'awards%'";

    pub fn latest_refresh_run(&self) -> Result<Option<RefreshRun>, Box<dyn Error>> {
        self.latest_run(Self::TENDER_RUNS)
    }

    /// The newest award-history fetch, so a weekly run leaves evidence of what it
    /// covered even when nothing reads it into the dashboard.
    pub fn latest_award_run(&self) -> Result<Option<RefreshRun>, Box<dyn Error>> {
        self.latest_run(Self::AWARD_RUNS)
    }

    fn latest_run(&self, kinds: &str) -> Result<Option<RefreshRun>, Box<dyn Error>> {
        if !self.schema.refresh_runs {
            return Ok(None);
        }
        let evidence = if self.schema.refresh_evidence {
            "evidence_opportunities, evidence_with_deadline, evidence_with_value, \
             evidence_with_requirements, evidence_stated, evidence_hinted, evidence_unknown"
        } else {
            // These legacy-schema projections must decode as nullable integers,
            // not Arrow Null columns (unsupported by duckdb-rs).
            "NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT, \
             NULL::BIGINT, NULL::BIGINT, NULL::BIGINT"
        };
        let sql = format!(
            "SELECT id, CAST(started_at AS VARCHAR), kind, scope, window_desc, fetched, \
             stored, total_notice_count, duration_ms, {evidence} FROM refresh_runs \
             WHERE {kinds} ORDER BY id DESC LIMIT 1"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query_map([], |row| {
            let evidence: Option<CorpusEvidence> = match (
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, Option<i64>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
                row.get::<_, Option<i64>>(15)?,
            ) {
                (
                    Some(opportunities),
                    Some(with_deadline),
                    Some(with_value),
                    Some(with_requirements),
                    Some(stated),
                    Some(hinted),
                    Some(unknown),
                ) => Some(CorpusEvidence {
                    opportunities: opportunities as usize,
                    with_deadline: with_deadline as usize,
                    with_value: with_value as usize,
                    with_requirements: with_requirements as usize,
                    stated: stated as usize,
                    hinted: hinted as usize,
                    unknown: unknown as usize,
                }),
                _ => None,
            };
            Ok(RefreshRun {
                id: row.get(0)?,
                started_at: row.get(1)?,
                kind: row.get(2)?,
                scope: row.get(3)?,
                window: row.get(4)?,
                fetched: row.get(5)?,
                stored: row.get(6)?,
                total_notice_count: row.get(7)?,
                duration_ms: row.get(8)?,
                evidence,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Record a working-papers audit event. The table is created on demand so
    /// writers going through `open_initialized` (which skips migrations) work
    /// on old databases too.
    pub fn record_audit(
        &self,
        actor: &str,
        action: &str,
        enterprise_number: Option<&str>,
        source: Option<&str>,
        external_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), Box<dyn Error>> {
        if ![
            AUDIT_FEEDBACK_SAVE,
            AUDIT_PREFERENCES_SAVE,
            AUDIT_MATTER_CLOSE,
        ]
        .contains(&action)
        {
            return Err(
                "audit action must be feedback.save, preferences.save or matter.close".into(),
            );
        }
        self.connection.execute_batch(
            "CREATE SEQUENCE IF NOT EXISTS refresh_runs_id START 1;
             CREATE TABLE IF NOT EXISTS audit_log (
                 id INTEGER PRIMARY KEY DEFAULT nextval('refresh_runs_id'),
                 created_at TIMESTAMPTZ NOT NULL DEFAULT current_timestamp,
                 actor VARCHAR NOT NULL,
                 action VARCHAR NOT NULL,
                 enterprise_number VARCHAR,
                 source VARCHAR,
                 external_id VARCHAR,
                 detail VARCHAR
             );",
        )?;
        retry_write(|| {
            self.connection.execute(
                "INSERT INTO audit_log (actor, action, enterprise_number, source, external_id, detail) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![actor, action, enterprise_number, source, external_id, detail],
            )
        })?;
        Ok(())
    }

    /// Newest-first audit events, capped. Empty on databases whose audit table
    /// was never written to; read-only consumers never fail on its absence.
    pub fn audit_log(&self, limit: i64) -> Result<Vec<AuditEvent>, Box<dyn Error>> {
        if !self.schema.audit_log {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 1000);
        let mut statement = self.connection.prepare(
            "SELECT id, CAST(created_at AS VARCHAR), actor, action, enterprise_number, \
             source, external_id, detail FROM audit_log ORDER BY id DESC LIMIT ?",
        )?;
        let rows = statement
            .query_map(params![limit], |row| {
                Ok(AuditEvent {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    actor: row.get(2)?,
                    action: row.get(3)?,
                    enterprise_number: row.get(4)?,
                    source: row.get(5)?,
                    external_id: row.get(6)?,
                    detail: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Matter close: purge the confidential per-company tables (judgments,
    /// preferences, cached analysis) while keeping public tender/award data,
    /// run provenance, and the audit log itself — the log's `matter.close`
    /// row is the record that the purge happened. Returns per-table counts so
    /// the caller can state what was removed.
    pub fn close_matter(&self, actor: &str) -> Result<MatterCloseCounts, Box<dyn Error>> {
        // One transaction: a partial purge with no record of it is the worst
        // outcome for a deletion tool. Retried on write conflicts like every
        // other writer, via with_transaction.
        self.with_transaction(|| {
            let feedback: i64 = self
                .connection
                .query_row("SELECT count(*) FROM feedback", [], |row| row.get(0))
                .map_err(|error| Box::new(error) as Box<dyn Error>)?;
            let preferences: i64 = self
                .connection
                .query_row("SELECT count(*) FROM preferences", [], |row| row.get(0))
                .map_err(|error| Box::new(error) as Box<dyn Error>)?;
            let cached_decisions: i64 = self
                .connection
                .query_row("SELECT count(*) FROM analysis_cache", [], |row| row.get(0))
                .map_err(|error| Box::new(error) as Box<dyn Error>)?;
            self.connection
                .execute_batch(
                    "DELETE FROM feedback; DELETE FROM preferences; DELETE FROM analysis_cache;",
                )
                .map_err(|error| Box::new(error) as Box<dyn Error>)?;
            self.record_audit(actor, AUDIT_MATTER_CLOSE, None, None, None, None)?;
            Ok(MatterCloseCounts {
                feedback,
                preferences,
                cached_decisions,
            })
        })
    }

    /// Import one Brussels inventory file. Rows are keyed by their content hash,
    /// so re-importing the same file updates rather than duplicates.
    pub fn upsert_inventory_awards(
        &self,
        source_file: &str,
        awards: &[crate::bxl_inventory::InventoryAward],
    ) -> Result<usize, Box<dyn Error>> {
        let mut stored = 0_usize;
        self.with_transaction(|| {
            stored = 0;
            for award in awards {
                self.connection.execute(
                    r#"
                    INSERT INTO inventory_awards (
                        source_file, row_hash, buyer_identifier, buyer_name,
                        awardee_identifier, awardee_name, engagement_id,
                        engagement_date, engagement_amount, object, fetched_at
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, CAST(? AS DATE), ?, ?, now())
                    ON CONFLICT (source_file, row_hash) DO UPDATE SET
                        buyer_identifier = excluded.buyer_identifier,
                        buyer_name = excluded.buyer_name,
                        awardee_identifier = excluded.awardee_identifier,
                        awardee_name = excluded.awardee_name,
                        engagement_id = excluded.engagement_id,
                        engagement_date = excluded.engagement_date,
                        engagement_amount = excluded.engagement_amount,
                        object = excluded.object,
                        fetched_at = now()
                    "#,
                    params![
                        source_file,
                        award.row_hash,
                        award.buyer_identifier,
                        award.buyer_name,
                        award.awardee_identifier,
                        award.awardee_name,
                        award.engagement_id,
                        award.engagement_date,
                        award.engagement_amount,
                        award.object,
                    ],
                )?;
                stored += 1;
            }
            Ok(())
        })?;
        Ok(stored)
    }

    /// How many inventory rows the store holds for a buyer or a supplier KBO.
    pub fn inventory_award_count(&self, kbo: Option<&str>) -> Result<i64, duckdb::Error> {
        if !self.schema.inventory_awards {
            return Ok(0);
        }
        match kbo {
            Some(kbo) => self.connection.query_row(
                "SELECT count(*) FROM inventory_awards \
                 WHERE replace(buyer_identifier, '.', '') = replace(?, '.', '')
                    OR replace(awardee_identifier, '.', '') = replace(?, '.', '')",
                params![kbo, kbo],
                |row| row.get(0),
            ),
            None => self
                .connection
                .query_row("SELECT count(*) FROM inventory_awards", [], |row| {
                    row.get(0)
                }),
        }
    }

    /// The contracts an inventory file records for one supplier KBO, newest
    /// first — the read that makes a label-free evaluation possible.
    pub fn inventory_awards_for_awardee(
        &self,
        kbo: &str,
        limit: usize,
    ) -> Result<Vec<crate::bxl_inventory::InventoryAward>, Box<dyn Error>> {
        if !self.schema.inventory_awards {
            return Ok(Vec::new());
        }
        let limit = sql_limit(limit)?;
        let mut statement = self.connection.prepare(
            r#"
            SELECT buyer_identifier, buyer_name, awardee_identifier, awardee_name,
                   engagement_id, CAST(engagement_date AS VARCHAR), engagement_amount,
                   object, row_hash
            FROM inventory_awards
            WHERE replace(awardee_identifier, '.', '') = replace(?, '.', '')
            ORDER BY engagement_date DESC NULLS LAST, engagement_amount DESC NULLS LAST,
                     row_hash, source_file
            LIMIT ?
            "#,
        )?;
        let rows = statement
            .query_map(params![kbo, limit], |row| {
                Ok(crate::bxl_inventory::InventoryAward {
                    buyer_identifier: row.get(0)?,
                    buyer_name: row.get(1)?,
                    awardee_identifier: row.get(2)?,
                    awardee_name: row.get(3)?,
                    engagement_id: row.get(4)?,
                    engagement_date: row.get(5)?,
                    engagement_amount: row.get(6)?,
                    object: row.get(7)?,
                    row_hash: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The publication-date range the store holds, for coverage reporting: a
    /// refresh timestamp says when we ran, this says how far the data reaches.
    /// Rows imported before the column existed are counted separately so the
    /// range is not mistaken for the whole store.
    pub fn tender_publication_range(
        &self,
    ) -> Result<(Option<String>, Option<String>, i64), duckdb::Error> {
        if !self.schema.tenders_publication_date {
            let total: i64 =
                self.connection
                    .query_row("SELECT count(*) FROM tenders", [], |row| row.get(0))?;
            return Ok((None, None, total));
        }
        self.connection.query_row(
            "SELECT MIN(CAST(publication_date AS VARCHAR)), MAX(CAST(publication_date AS VARCHAR)), \
             count(*) - count(publication_date) FROM tenders",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
    }

    /// The contracts an inventory records for one supplier, **one row per
    /// engagement**: the stored table keeps every accounting row (an engagement
    /// repeats on each liquidation), so a per-accounting-row listing shows one
    /// contract many times and a sum over it counts that contract repeatedly.
    /// Rows without an engagement id are keyed by their content hash so they
    /// cannot collapse into each other.
    pub fn inventory_engagements_for_awardee(
        &self,
        kbo: &str,
        limit: usize,
    ) -> Result<Vec<InventoryEngagement>, Box<dyn Error>> {
        if !self.schema.inventory_awards {
            return Ok(Vec::new());
        }
        let limit = sql_limit(limit)?;
        // `engagement_id = '0'` is a placeholder, not an engagement: 657 of 5,304
        // imported rows (12.4%, 81 awardees) carry it. The rows themselves still
        // collapse — identical accounting lines are one purchase liquidated more
        // than once, which is what `accounting_rows` reports — but the id is
        // reported as NULL instead of an id of "0" that no engagement ever had.
        let mut statement = self.connection.prepare(
            r#"
            WITH identified AS (
                SELECT *, CASE
                    WHEN engagement_id IS NULL OR trim(engagement_id) IN ('', '0', '000', '0000')
                    THEN NULL ELSE engagement_id END AS engagement_key
                FROM inventory_awards
                WHERE replace(awardee_identifier, '.', '') = replace(?, '.', '')
            )
            SELECT any_value(engagement_key), buyer_name, CAST(engagement_date AS VARCHAR),
                   engagement_amount, any_value(object), count(*)
            FROM identified
            GROUP BY COALESCE(engagement_id, row_hash), buyer_name,
                     CAST(engagement_date AS VARCHAR), engagement_amount
            ORDER BY CAST(engagement_date AS VARCHAR) DESC NULLS LAST,
                     engagement_amount DESC NULLS LAST, COALESCE(engagement_id, row_hash)
            LIMIT ?
            "#,
        )?;
        let rows = statement
            .query_map(params![kbo, limit], |row| {
                Ok(InventoryEngagement {
                    engagement_id: row.get(0)?,
                    buyer_name: row.get(1)?,
                    engagement_date: row.get(2)?,
                    engagement_amount: row.get(3)?,
                    object: row.get(4)?,
                    accounting_rows: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn award_stats(&self) -> Result<(i64, Option<String>), duckdb::Error> {
        self.connection.query_row(
            "SELECT count(*), CAST(max(fetched_at) AS VARCHAR) FROM awards",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    /// How many companies have award history on file, and how many awards that
    /// is. The `history_match` term (15% of the score) stays neutral at 0.5 for
    /// every company without one, so a store with awards only for a handful of
    /// companies quietly scores everyone else on defaults alone.
    pub fn award_history_coverage(&self) -> Result<(i64, i64), duckdb::Error> {
        self.connection.query_row(
            "SELECT count(*), count(DISTINCT enterprise_number) FROM awards",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    pub fn award_lot_stats(&self) -> Result<(i64, i64), duckdb::Error> {
        self.connection.query_row(
            "SELECT count(*), count(winner_identifier) FROM award_lots",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    /// Insert or replace one NBB filing row. Same (enterprise, year, ref)
    /// from a richer source supersedes, never duplicates: re-ingesting the
    /// same filing via the API after a manual Consult download is an
    /// upsert with a new acquisition_method, and the audit trail (fetched_at)
    /// shows which copy won.
    pub fn upsert_nbb_financials(&self, fin: &NbbFinancials) -> Result<usize, Box<dyn Error>> {
        Ok(self.connection.execute(
            r#"
            INSERT INTO nbb_financials (
                enterprise_kbo, fiscal_year, period_start, period_end,
                turnover, staff_fte, equity, result,
                source_ref, source_format, acquisition_method, fetched_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, now())
            ON CONFLICT (enterprise_kbo, fiscal_year, source_ref) DO UPDATE SET
                period_start = COALESCE(excluded.period_start, nbb_financials.period_start),
                period_end = COALESCE(excluded.period_end, nbb_financials.period_end),
                turnover = excluded.turnover,
                staff_fte = excluded.staff_fte,
                equity = excluded.equity,
                result = excluded.result,
                source_format = excluded.source_format,
                acquisition_method = excluded.acquisition_method,
                fetched_at = now()
            "#,
            params![
                fin.enterprise_kbo,
                fin.fiscal_year,
                fin.period_start,
                fin.period_end,
                fin.turnover,
                fin.staff_fte,
                fin.equity,
                fin.result,
                fin.source_ref,
                fin.source_format,
                fin.acquisition_method,
            ],
        )?)
    }

    /// Financial history for one enterprise, newest year first. Joins on the
    /// normalized Belgian enterprise number so KBO's dotted form and NBB's
    /// digits-only form resolve to the same company. Returns every filing row
    /// (manual and API copies side by side).
    ///
    /// Within one fiscal year the order is `period_end` descending, then
    /// `fetched_at`. Callers that read only the first row (capacity triage does)
    /// therefore get the current period rather than a comparative one, which
    /// matters because a comparative period may disclose no turnover.
    pub fn nbb_financials_for(
        &self,
        enterprise_kbo: &str,
    ) -> Result<Vec<NbbFinancials>, Box<dyn Error>> {
        self.nbb_financial_evidence_for(enterprise_kbo).map(|rows| {
            rows.into_iter()
                .map(|row| NbbFinancials {
                    enterprise_kbo: row.enterprise_kbo,
                    fiscal_year: row.fiscal_year,
                    period_start: row.period_start,
                    period_end: row.period_end,
                    turnover: row.turnover,
                    staff_fte: row.staff_fte,
                    equity: row.equity,
                    result: row.result,
                    source_ref: row.source_ref,
                    source_format: row.source_format,
                    acquisition_method: row.acquisition_method,
                })
                .collect()
        })
    }

    /// Explain NBB row availability separately from whether the database has a
    /// compatible financial-evidence table. This is a storage-coverage status,
    /// not a statement about filings available at NBB. The returned history is
    /// capped; callers must treat summaries as partial when `truncated` is true.
    pub fn nbb_financial_evidence_lookup_for(
        &self,
        enterprise_kbo: &str,
    ) -> Result<NbbFinancialEvidenceLookup, Box<dyn Error>> {
        let enterprise_kbo = crate::kbo::normalize_enterprise_number(enterprise_kbo)?;
        if !table_exists(&self.connection, "nbb_financials")? {
            return Ok(NbbFinancialEvidenceLookup {
                coverage: "financials_table_missing",
                filings: Vec::new(),
                truncated: false,
            });
        }
        let required_columns = [
            "enterprise_kbo",
            "fiscal_year",
            "turnover",
            "staff_fte",
            "equity",
            "result",
            "source_ref",
            "source_format",
            "acquisition_method",
            "fetched_at",
        ];
        for column in required_columns {
            if !column_exists(&self.connection, "nbb_financials", column)? {
                return Ok(NbbFinancialEvidenceLookup {
                    coverage: "financials_schema_incomplete",
                    filings: Vec::new(),
                    truncated: false,
                });
            }
        }
        let mut filings = self.nbb_financial_evidence_query_for(
            &enterprise_kbo,
            Some(NBB_EVIDENCE_PROJECTION_LIMIT + 1),
        )?;
        let truncated = filings.len() > NBB_EVIDENCE_PROJECTION_LIMIT;
        filings.truncate(NBB_EVIDENCE_PROJECTION_LIMIT);
        let coverage = if filings.is_empty() {
            "no_filing_stored"
        } else {
            "filings_stored"
        };
        Ok(NbbFinancialEvidenceLookup {
            coverage,
            filings,
            truncated,
        })
    }

    /// Provenance-rich NBB history for a company, newest filing first.
    /// Enterprise identifiers are joined canonically across dotted KBO and
    /// digits-only NBB spellings; missing financial values remain NULL.
    pub fn nbb_financial_evidence_for(
        &self,
        enterprise_kbo: &str,
    ) -> Result<Vec<NbbFinancialEvidence>, Box<dyn Error>> {
        self.nbb_financial_evidence_query_for(enterprise_kbo, None)
    }

    fn nbb_financial_evidence_query_for(
        &self,
        enterprise_kbo: &str,
        limit: Option<usize>,
    ) -> Result<Vec<NbbFinancialEvidence>, Box<dyn Error>> {
        let period_start = if column_exists(&self.connection, "nbb_financials", "period_start")? {
            "period_start"
        } else {
            "CAST(NULL AS VARCHAR)"
        };
        let period_end = if column_exists(&self.connection, "nbb_financials", "period_end")? {
            "period_end"
        } else {
            "CAST(NULL AS VARCHAR)"
        };
        // Legacy stores have no period columns, so the tie-break has to use the
        // same substitution as the projection.
        // A batch import stamps one `fetched_at` on every row, so fetch time
        // alone does not separate two filings for the same year and period.
        // Ranking reads the first row of this listing, so the decisive keys have
        // to be the ones the capacity queries use, in the same direction:
        // fiscal_year, then period_end, then source_ref. `fetched_at` breaks only
        // what is left. With fetch time ranked above the filing reference, the
        // two paths could still choose different rows for the same year.
        let order = format!(
            "ORDER BY fiscal_year DESC, {period_end} DESC NULLS LAST, source_ref DESC, \
             fetched_at DESC"
        );
        let (order_clause, limit_clause) = match limit {
            Some(limit) => (order, format!("LIMIT {limit}")),
            None => (order, String::new()),
        };
        let query = format!(
            r#"
            WITH normalized AS (
                SELECT *, regexp_replace(enterprise_kbo, '[^0-9]', '', 'g') AS enterprise_digits
                FROM nbb_financials
            )
            SELECT enterprise_kbo, fiscal_year, {period_start}, {period_end},
                   turnover, staff_fte, equity, result,
                   source_ref, source_format, acquisition_method, CAST(fetched_at AS VARCHAR)
            FROM normalized
            WHERE CASE WHEN length(enterprise_digits) = 9
                       THEN '0' || enterprise_digits
                       ELSE enterprise_digits END = ?
              AND length(enterprise_digits) IN (9, 10)
            {order_clause}
            {limit_clause}
            "#
        );
        let mut statement = self.connection.prepare(&query)?;
        let enterprise_digits = crate::kbo::normalize_enterprise_number(enterprise_kbo)?
            .chars()
            .filter(char::is_ascii_digit)
            .collect::<String>();
        statement
            .query_map(params![enterprise_digits], |row| {
                Ok(NbbFinancialEvidence {
                    enterprise_kbo: row.get(0)?,
                    fiscal_year: row.get(1)?,
                    period_start: row.get(2)?,
                    period_end: row.get(3)?,
                    turnover: row.get(4)?,
                    staff_fte: row.get(5)?,
                    equity: row.get(6)?,
                    result: row.get(7)?,
                    source_ref: row.get(8)?,
                    source_format: row.get(9)?,
                    acquisition_method: row.get(10)?,
                    stored_at: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn tenders_sql(&self) -> String {
        TENDERS_SQL
            .replace("{notice_type}", self.schema.notice_type_expression())
            .replace(
                "{publication_date}",
                self.schema.publication_date_expression(),
            )
            .replace("{lot_values}", self.schema.lot_values_expression())
    }

    pub fn tenders(&self, limit: usize) -> Result<Vec<Tender>, Box<dyn Error>> {
        let limit = sql_limit(limit)?;
        let mut statement = self.connection.prepare(&self.tenders_sql())?;
        let rows = statement
            .query_map(params![limit], read_tender_list_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(tender_from_list_row).collect()
    }

    /// Bounded bulk export that includes each notice's full raw provenance.
    /// Keep this separate from ranking paths, which intentionally omit large raw payloads.
    pub fn tenders_with_raw_for_source(
        &self,
        source: &str,
        limit: usize,
    ) -> Result<Vec<TenderSnapshotRow>, Box<dyn Error>> {
        let limit = sql_limit(limit)?;
        let sql = TENDERS_WITH_RAW_SQL
            .replace("{notice_type}", self.schema.notice_type_expression())
            .replace(
                "{publication_date}",
                self.schema.publication_date_expression(),
            )
            .replace("{lot_values}", self.schema.lot_values_expression());
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(params![source, limit], read_tender_full_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(tender_from_full_row).collect()
    }

    /// Read only identity/version headers across the opportunity population.
    /// A raw deadline/LIMIT predicate would hide expired or distant corrections
    /// before authority is resolved. Bound metadata, not the number of survivors;
    /// refuse explicitly rather than serving a silently incomplete version set.
    fn operative_notice_headers(
        &self,
        as_of: Option<NaiveDate>,
        byte_budget: usize,
    ) -> Result<Vec<Tender>, Box<dyn Error>> {
        let publication = self.schema.publication_date_expression();
        let sql = format!(
            "SELECT source, external_id, title, buyer, cpv_codes, CAST(deadline AS VARCHAR), \
             CAST({publication} AS VARCHAR), {folder}, {workspace} FROM tenders WHERE {} \
             AND (? IS NULL OR {publication} IS NULL OR {publication} <= CAST(? AS DATE)) \
             ORDER BY source, external_id",
            self.schema.opportunity_filter(),
            folder = "CASE WHEN json_type(raw, '$.contract_folder_id') = 'VARCHAR' THEN json_extract_string(raw, '$.contract_folder_id') END",
            workspace = "CASE WHEN json_type(raw, '$.bosa_publication_workspace_id') = 'VARCHAR' THEN json_extract_string(raw, '$.bosa_publication_workspace_id') END",
        );
        let date = as_of.map(|date| date.to_string());
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params![date, date])?;
        let mut headers = Vec::new();
        let mut retained = 0_usize;
        while let Some(row) = rows.next()? {
            let cpvs: String = row.get(4)?;
            let folder: Option<String> = row.get(7)?;
            let workspace: Option<String> = row.get(8)?;
            let identity_bytes =
                folder.as_ref().map_or(0, String::len) + workspace.as_ref().map_or(0, String::len);
            let mut raw = serde_json::Map::new();
            for (field, value) in [
                ("contract_folder_id", folder),
                ("bosa_publication_workspace_id", workspace),
            ] {
                if let Some(value) = value {
                    raw.insert(field.into(), value.into());
                }
            }
            let tender = Tender {
                source: row.get(0)?,
                external_id: row.get(1)?,
                title: row.get(2)?,
                buyer: row.get(3)?,
                cpv_codes: serde_json::from_str(&cpvs)?,
                deadline: row.get(5)?,
                publication_date: row.get(6)?,
                raw: serde_json::Value::Object(raw),
                ..Tender::default()
            };
            // Charge text plus conservative per-row overhead, including the
            // grouping keys cloned by the shared selector. No raw/description
            // blobs are loaded during this pass.
            let text_bytes = tender.source.len()
                + tender.external_id.len()
                + tender.title.len()
                + tender.buyer.as_ref().map_or(0, String::len)
                + cpvs.len()
                + identity_bytes;
            retained = retained.saturating_add(text_bytes.saturating_mul(4) + 1024);
            if retained > byte_budget {
                return Err(
                    "notice version identity budget exceeded; refusing incomplete candidates"
                        .into(),
                );
            }
            headers.push(tender);
        }
        Ok(crate::collapse_notice_versions(headers))
    }

    /// Resolve versions before deadline eligibility and before the output bound.
    /// The as-of variant additionally excludes publications not yet known then.
    pub fn open_tenders_as_of(
        &self,
        limit: usize,
        as_of: NaiveDate,
    ) -> Result<Vec<Tender>, Box<dyn Error>> {
        self.selected_open_tenders(limit, as_of, None, Some(as_of))
    }

    pub fn open_tenders(
        &self,
        limit: usize,
        today: NaiveDate,
        due_within: Option<i64>,
    ) -> Result<Vec<Tender>, Box<dyn Error>> {
        self.selected_open_tenders(limit, today, due_within, None)
    }

    fn selected_open_tenders(
        &self,
        limit: usize,
        today: NaiveDate,
        due_within: Option<i64>,
        as_of: Option<NaiveDate>,
    ) -> Result<Vec<Tender>, Box<dyn Error>> {
        sql_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        if let Some(days) = due_within {
            if days < 0 || today.checked_add_signed(Duration::days(days)).is_none() {
                return Err("due window is outside the supported date range".into());
            }
        }
        // A single snapshot ties the authority pass to the selected payloads.
        self.with_transaction(|| {
            let selected = self
                .operative_notice_headers(as_of, MAX_NOTICE_IDENTITY_BYTES)?
                .into_iter()
                .filter(|t| crate::recommendations::is_eligible(t, today, due_within))
                .take(limit)
                .map(|t| (t.source, t.external_id))
                .collect::<Vec<_>>();
            let identities = serde_json::to_string(&selected)?;
            let sql = SELECTED_TENDERS_SQL
                .replace("{notice_type}", self.schema.notice_type_expression())
                .replace(
                    "{publication_date}",
                    self.schema.publication_date_expression(),
                )
                .replace("{lot_values}", self.schema.lot_values_expression());
            let mut statement = self.connection.prepare(&sql)?;
            let rows = statement
                .query_map(params![identities], read_tender_list_row)?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().map(tender_from_list_row).collect()
        })
    }

    pub fn tender(
        &self,
        source: &str,
        external_id: &str,
    ) -> Result<Option<Tender>, Box<dyn Error>> {
        let row = self.connection.query_row(
            &r#"
            SELECT source, external_id, title, buyer, description, cpv_codes,
                   regions, estimated_value, CAST(deadline AS VARCHAR), languages,
                   requirements, document_urls, {notice_type},
                   CAST({publication_date} AS VARCHAR), raw, {lot_values}
            FROM tenders
            WHERE source = ? AND external_id = ?
            "#
            .replace("{notice_type}", self.schema.notice_type_expression())
            .replace(
                "{publication_date}",
                self.schema.publication_date_expression(),
            )
            .replace("{lot_values}", self.schema.lot_values_expression()),
            params![source, external_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<Decimal>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, Option<String>>(15)?,
                ))
            },
        );
        let (
            source,
            external_id,
            title,
            buyer,
            description,
            cpv_codes,
            regions,
            estimated_value,
            deadline,
            languages,
            requirements,
            document_urls,
            notice_type,
            publication_date,
            raw,
            lot_values,
        ) = match row {
            Ok(values) => values,
            Err(duckdb::Error::QueryReturnedNoRows) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(Tender {
            source,
            external_id,
            title,
            buyer,
            description: description.unwrap_or_default(),
            cpv_codes: serde_json::from_str(&cpv_codes)?,
            regions: serde_json::from_str(&regions)?,
            estimated_value,
            deadline,
            languages: serde_json::from_str(&languages)?,
            notice_type,
            publication_date,
            requirements: serde_json::from_str::<Vec<TenderRequirement>>(&requirements)?,
            document_urls: serde_json::from_str(&document_urls)?,
            raw: serde_json::from_str(&raw)?,
            lot_values: parse_lot_values(lot_values.as_deref())?,
        }))
    }

    /// The mutation and its audit event commit or roll back together.
    pub fn save_preferences_audited(
        &self,
        actor: &str,
        enterprise_number: &str,
        preferences: &CompanyPreferences,
    ) -> Result<(), Box<dyn Error>> {
        self.with_transaction(|| {
            self.save_preferences(enterprise_number, preferences)?;
            self.record_audit(
                actor,
                AUDIT_PREFERENCES_SAVE,
                Some(enterprise_number),
                None,
                None,
                None,
            )
        })
    }

    pub fn save_preferences(
        &self,
        enterprise_number: &str,
        preferences: &CompanyPreferences,
    ) -> Result<(), Box<dyn Error>> {
        validate_value_limits(preferences.minimum_value, preferences.maximum_value)
            .map_err(str::to_owned)?;
        crate::recognition::validate_recognition(
            preferences.recognition_class,
            &preferences.recognition_categories,
        )
        .map_err(|error| -> Box<dyn Error> { error.into() })?;
        let confirmed_activities = serde_json::to_string(&preferences.confirmed_activities)?;
        let excluded_terms = serde_json::to_string(&preferences.excluded_terms)?;
        let preferred_regions = serde_json::to_string(&preferences.preferred_regions)?;
        let languages = serde_json::to_string(&preferences.languages)?;
        let recognition_categories = serde_json::to_string(
            &crate::recognition::normalized_categories(&preferences.recognition_categories),
        )?;
        retry_write(|| {
            self.connection.execute(
                r#"
            INSERT INTO preferences (
                enterprise_number, confirmed_activities, excluded_terms,
                preferred_regions, languages, minimum_value, maximum_value,
                recognition_class, recognition_categories, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, now())
            ON CONFLICT (enterprise_number) DO UPDATE SET
                confirmed_activities = excluded.confirmed_activities,
                excluded_terms = excluded.excluded_terms,
                preferred_regions = excluded.preferred_regions,
                languages = excluded.languages,
                minimum_value = excluded.minimum_value,
                maximum_value = excluded.maximum_value,
                recognition_class = excluded.recognition_class,
                recognition_categories = excluded.recognition_categories,
                updated_at = now()
            "#,
                params![
                    enterprise_number,
                    confirmed_activities,
                    excluded_terms,
                    preferred_regions,
                    languages,
                    preferences.minimum_value,
                    preferences.maximum_value,
                    preferences.recognition_class,
                    recognition_categories,
                ],
            )
        })?;
        Ok(())
    }

    /// Enterprises carrying stored ranking context (preferences, feedback or
    /// award history). Reverse search resolves full context only for these
    /// and uses defaults for everyone else: identical scores to per-company
    /// loading (the tables are empty for unjudged companies by construction)
    /// at three queries instead of three per candidate.
    pub fn enterprises_with_context(&self) -> Result<HashSet<String>, Box<dyn Error>> {
        let mut enterprises = HashSet::new();
        for table in ["preferences", "feedback", "awards"] {
            let mut statement = self
                .connection
                .prepare(&format!("SELECT DISTINCT enterprise_number FROM {table}"))?;
            for row in statement.query_map([], |row| row.get::<_, String>(0))? {
                enterprises.insert(row?);
            }
        }
        Ok(enterprises)
    }

    pub fn preferences_for(
        &self,
        enterprise_number: &str,
    ) -> Result<CompanyPreferences, Box<dyn Error>> {
        let row = self.connection.query_row(
            &r#"
            SELECT confirmed_activities, excluded_terms, preferred_regions,
                   languages, minimum_value, maximum_value,
                   {recognition}
            FROM preferences
            WHERE enterprise_number = ?
            "#
            .replace("{recognition}", self.schema.recognition_expression()),
            params![enterprise_number],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<Decimal>>(4)?,
                    row.get::<_, Option<Decimal>>(5)?,
                    row.get::<_, Option<i32>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        );
        let (
            confirmed,
            excluded,
            regions,
            languages,
            minimum,
            maximum,
            recognition_class,
            recognition_categories,
        ) = match row {
            Ok(values) => values,
            Err(duckdb::Error::QueryReturnedNoRows) => return Ok(CompanyPreferences::default()),
            Err(error) => return Err(error.into()),
        };
        Ok(CompanyPreferences {
            confirmed_activities: serde_json::from_str(confirmed.as_deref().unwrap_or("[]"))?,
            excluded_terms: serde_json::from_str(excluded.as_deref().unwrap_or("[]"))?,
            preferred_regions: serde_json::from_str(regions.as_deref().unwrap_or("[]"))?,
            languages: serde_json::from_str(languages.as_deref().unwrap_or("[]"))?,
            minimum_value: minimum,
            maximum_value: maximum,
            recognition_class: recognition_class.and_then(|class| u8::try_from(class).ok()),
            recognition_categories: serde_json::from_str(
                recognition_categories.as_deref().unwrap_or("[]"),
            )?,
        })
    }

    pub fn delete_award(
        &self,
        enterprise_number: &str,
        publication_number: &str,
    ) -> Result<(), Box<dyn Error>> {
        self.with_transaction(|| {
            self.connection.execute(
                "DELETE FROM award_lots WHERE enterprise_number = ? AND publication_number = ?",
                params![enterprise_number, publication_number],
            )?;
            self.connection.execute(
                "DELETE FROM awards WHERE enterprise_number = ? AND publication_number = ?",
                params![enterprise_number, publication_number],
            )?;
            Ok(())
        })
    }

    pub fn replace_award_refresh<'a, I>(
        &self,
        enterprise_number: &str,
        updates: I,
    ) -> Result<(), Box<dyn Error>>
    where
        I: IntoIterator<Item = (&'a str, Option<&'a AwardRecord>, &'a [AwardLot])>,
    {
        // Materialize so the whole refresh can be retried after a conflict.
        let updates: Vec<(&str, Option<&AwardRecord>, &[AwardLot])> = updates.into_iter().collect();
        self.with_transaction(|| {
            for &(publication_number, award, lots) in &updates {
                self.connection.execute(
                    "DELETE FROM award_lots WHERE enterprise_number = ? AND publication_number = ?",
                    params![enterprise_number, publication_number],
                )?;
                match award {
                    Some(award) => {
                        self.upsert_award(enterprise_number, award)?;
                        for lot in lots {
                            self.upsert_award_lot(enterprise_number, lot)?;
                        }
                    }
                    None => {
                        self.connection.execute(
                            "DELETE FROM awards WHERE enterprise_number = ? AND publication_number = ?",
                            params![enterprise_number, publication_number],
                        )?;
                    }
                }
            }
            Ok(())
        })
    }

    /// Replace BOSA-derived company history for the supplied source notices.
    /// Each notice uses a `bosa:` publication namespace and is marked in raw
    /// provenance, so TED/manual award rows are never removed by this sync.
    pub fn replace_bosa_award_history(
        &self,
        publications: &[String],
        records: &[(String, AwardRecord)],
    ) -> Result<usize, Box<dyn Error>> {
        let publication_set: HashSet<&str> = publications.iter().map(String::as_str).collect();
        if publication_set.len() != publications.len()
            || publications
                .iter()
                .any(|publication| !publication.starts_with("bosa:"))
        {
            return Err("BOSA publications must be unique and use the bosa: namespace".into());
        }
        let mut row_keys = HashSet::new();
        for (enterprise, award) in records {
            if !publication_set.contains(award.publication_number.as_str())
                || award.raw.get("source").and_then(serde_json::Value::as_str) != Some("bosa")
                || !row_keys.insert((enterprise.as_str(), award.publication_number.as_str()))
            {
                return Err(
                    "BOSA history record has invalid provenance or duplicate identity".into(),
                );
            }
        }

        self.with_transaction(|| {
            for publication in publications {
                self.connection.execute(
                    "DELETE FROM awards WHERE publication_number = ? AND json_extract_string(raw, '$.source') = 'bosa'",
                    params![publication],
                )?;
            }
            for (enterprise, award) in records {
                self.upsert_award(enterprise, award)?;
            }
            Ok(records.len())
        })
    }

    /// Replace named BOSA lot tenderers for the supplied publications.
    pub fn replace_bosa_lot_bidders(
        &self,
        publications: &[String],
        bidders: &[crate::bosa_award_history::BosaLotBidder],
    ) -> Result<usize, Box<dyn Error>> {
        let publication_set: HashSet<&str> = publications.iter().map(String::as_str).collect();
        if bidders
            .iter()
            .any(|bidder| !publication_set.contains(bidder.publication_number.as_str()))
        {
            return Err("BOSA bidder row references a publication outside this batch".into());
        }
        self.with_transaction(|| {
            for publication in publications {
                self.connection.execute(
                    "DELETE FROM bosa_lot_bidders WHERE publication_number = ?",
                    params![publication],
                )?;
            }
            let mut statement = self.connection.prepare(
                r#"
            INSERT INTO bosa_lot_bidders (
                publication_number, dossier_id, lot_id, organization_reference,
                enterprise_number, raw_identifier, name, tendering_party_reference,
                group_lead, outcome, lot_tenderers, lowest_tender_value,
                highest_tender_value, cpv_codes, fetched_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, TRY_CAST(? AS DECIMAL(18,2)),
                      TRY_CAST(? AS DECIMAL(18,2)), ?, now())
            "#,
            )?;
            for bidder in bidders {
                statement.execute(params![
                    bidder.publication_number,
                    bidder.dossier_id,
                    bidder.lot_id,
                    bidder.organization_reference,
                    bidder.enterprise_number,
                    bidder.raw_identifier,
                    bidder.name,
                    bidder.tendering_party_reference,
                    bidder.group_lead,
                    bidder.outcome,
                    bidder.lot_tenderers as i64,
                    bidder.lowest_tender_value,
                    bidder.highest_tender_value,
                    serde_json::to_string(&bidder.cpv_codes)?,
                ])?;
            }
            Ok(bidders.len())
        })
    }

    pub fn upsert_award(
        &self,
        enterprise_number: &str,
        award: &AwardRecord,
    ) -> Result<(), Box<dyn Error>> {
        let winner_identifiers = serde_json::to_string(&award.winner_identifiers)?;
        let winner_names = serde_json::to_string(&award.winner_names)?;
        let cpv_codes = serde_json::to_string(&award.cpv_codes)?;
        let values = serde_json::to_string(&award.values)?;
        let links = serde_json::to_string(&award.links)?;
        let raw = serde_json::to_string(&award.raw)?;
        retry_write(|| {
            self.connection.execute(
                r#"
            INSERT INTO awards (
                enterprise_number, publication_number, title, buyer,
                winner_identifiers, winner_names, cpv_codes, values, links, raw,
                fetched_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, now())
            ON CONFLICT (enterprise_number, publication_number) DO UPDATE SET
                title = excluded.title,
                buyer = excluded.buyer,
                winner_identifiers = excluded.winner_identifiers,
                winner_names = excluded.winner_names,
                cpv_codes = excluded.cpv_codes,
                values = excluded.values,
                links = excluded.links,
                raw = excluded.raw,
                fetched_at = now()
            "#,
                params![
                    enterprise_number,
                    award.publication_number,
                    award.title,
                    award.buyer,
                    winner_identifiers,
                    winner_names,
                    cpv_codes,
                    values,
                    links,
                    raw,
                ],
            )
        })?;
        Ok(())
    }

    pub fn upsert_awards_many<'a, I>(
        &self,
        enterprise_number: &str,
        awards: I,
    ) -> Result<usize, Box<dyn Error>>
    where
        I: IntoIterator<Item = &'a AwardRecord>,
    {
        // Materialize so the whole batch can be retried after a conflict.
        let awards: Vec<&AwardRecord> = awards.into_iter().collect();
        self.with_transaction(|| {
            for &award in &awards {
                self.upsert_award(enterprise_number, award)?;
            }
            Ok(awards.len())
        })
    }

    /// Replace one file's BDA awards wholesale: re-ingesting a dossier must
    /// update corrected notices, never duplicate them. Winner names are
    /// unresolved strings (no BCE travels in the notice); the denomination
    /// join happens at query time.
    pub fn replace_bda_awards(
        &self,
        source_name: &str,
        awards: &[crate::bda::BdaAward],
    ) -> Result<usize, Box<dyn Error>> {
        self.with_transaction(|| {
            self.connection.execute(
                "DELETE FROM bda_awards WHERE source_name = ?",
                params![source_name],
            )?;
            for award in awards {
                self.connection.execute(
                    r#"
            INSERT INTO bda_awards (
                source_name, notice_ref, winner_name, winner_town,
                winner_postcode, winner_country, value, currency,
                date_conclusion, tenders_received, cpv, buyer, fetched_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, now())
            "#,
                    params![
                        award.source_name,
                        award.notice_ref,
                        award.winner_name,
                        award.winner_town,
                        award.winner_postcode,
                        award.winner_country,
                        award.value.map(|value| value.to_string()),
                        award.currency,
                        award.date_conclusion,
                        award.tenders_received,
                        award.cpv,
                        award.buyer,
                    ],
                )?;
            }
            Ok(awards.len())
        })
    }

    /// Replace one CSV year's history wholesale: re-imports stay idempotent
    /// and never duplicate. Unresolved rows persist with `join_confidence =
    /// 'none'` — training filters on confidence, never on silence.
    pub fn replace_ted_awards_history(
        &self,
        year: i64,
        awards: &[crate::ted_csv::HistoryAward],
    ) -> Result<usize, Box<dyn Error>> {
        self.with_transaction(|| {
            self.connection.execute(
                "DELETE FROM ted_awards_history WHERE year = ?",
                params![year],
            )?;
            for award in awards {
                self.connection.execute(
                    r#"
            INSERT INTO ted_awards_history (
                year, notice_url, buyer_name, winner_raw, winner_member,
                winner_enterprise, join_confidence, award_value, cpv,
                dt_award, sme, number_offers, ambiguous, fetched_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, now())
            "#,
                    params![
                        award.year,
                        award.notice_url,
                        award.buyer_name,
                        award.winner_raw,
                        award.winner_member,
                        award.winner_enterprise,
                        award.join_confidence,
                        award.award_value.map(|value| value.to_string()),
                        award.cpv,
                        award.dt_award,
                        award.sme,
                        award.number_offers,
                        award.ambiguous,
                    ],
                )?;
            }
            Ok(awards.len())
        })
    }

    pub fn upsert_award_lot(
        &self,
        enterprise_number: &str,
        lot: &AwardLot,
    ) -> Result<(), Box<dyn Error>> {
        let cpv_codes = serde_json::to_string(&lot.cpv_codes)?;
        retry_write(|| {
            self.connection.execute(
                r#"
            INSERT INTO award_lots (
                enterprise_number, publication_number, lot_id, winner_identifier,
                winner_name, value, lowest_value, highest_value, cpv_codes, fetched_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, now())
            ON CONFLICT (enterprise_number, publication_number, lot_id) DO UPDATE SET
                winner_identifier = excluded.winner_identifier,
                winner_name = excluded.winner_name,
                value = excluded.value,
                lowest_value = excluded.lowest_value,
                highest_value = excluded.highest_value,
                cpv_codes = excluded.cpv_codes,
                fetched_at = now()
            "#,
                params![
                    enterprise_number,
                    lot.publication_number,
                    lot.lot_id,
                    lot.winner_identifier,
                    lot.winner_name,
                    lot.value,
                    lot.lowest_value,
                    lot.highest_value,
                    cpv_codes,
                ],
            )
        })?;
        Ok(())
    }

    pub fn upsert_award_lots_many<'a, I>(
        &self,
        enterprise_number: &str,
        publication_number: &str,
        lots: I,
    ) -> Result<usize, Box<dyn Error>>
    where
        I: IntoIterator<Item = &'a AwardLot>,
    {
        // Materialize so the whole replacement can be retried after a conflict.
        let lots: Vec<&AwardLot> = lots.into_iter().collect();
        self.with_transaction(|| {
            self.connection.execute(
                "DELETE FROM award_lots WHERE enterprise_number = ? AND publication_number = ?",
                params![enterprise_number, publication_number],
            )?;
            for &lot in &lots {
                self.upsert_award_lot(enterprise_number, lot)?;
            }
            Ok(lots.len())
        })
    }

    pub fn award_lots_for(&self, enterprise_number: &str) -> Result<Vec<AwardLot>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT publication_number, lot_id, winner_identifier, winner_name,
                   value, lowest_value, highest_value, cpv_codes
            FROM award_lots
            WHERE enterprise_number = ?
            ORDER BY publication_number, lot_id
            "#,
        )?;
        let rows = statement
            .query_map(params![enterprise_number], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<Decimal>>(4)?,
                    row.get::<_, Option<Decimal>>(5)?,
                    row.get::<_, Option<Decimal>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(
                    publication_number,
                    lot_id,
                    winner_identifier,
                    winner_name,
                    value,
                    lowest_value,
                    highest_value,
                    cpvs,
                )| {
                    Ok(AwardLot {
                        publication_number,
                        lot_id,
                        winner_identifier,
                        winner_name,
                        value,
                        lowest_value,
                        highest_value,
                        cpv_codes: serde_json::from_str(&cpvs)?,
                    })
                },
            )
            .collect()
    }

    /// Price-benchmark core: per-CPV-division aggregates over the flat TED
    /// history (counts + awarded values + competition intensity). Averages are
    /// exact Decimal arithmetic in Rust, never DuckDB AVG type quirks.
    /// Divisions are CPV first-two-digits; rows without a CPV are skipped
    /// (unstated, never bucketed into a fake category).
    pub fn award_value_spreads(&self) -> Result<Vec<CpvSpread>, Box<dyn Error>> {
        let mut statement = self
            .connection
            .prepare("SELECT cpv, award_value, number_offers FROM ted_awards_history")?;
        // division -> (rows, value_sum, value_n, offers_sum, offers_n, single_bid_n)
        let mut groups: std::collections::BTreeMap<
            String,
            (usize, Decimal, usize, i64, usize, usize),
        > = std::collections::BTreeMap::new();
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<Decimal>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;
        for row in rows {
            let (cpv, value, offers) = row?;
            let division = match cpv {
                Some(c) if c.len() >= 2 => c[..2].to_owned(),
                _ => continue,
            };
            let entry = groups
                .entry(division)
                .or_insert((0, Decimal::ZERO, 0, 0, 0, 0));
            entry.0 += 1;
            if let Some(v) = value {
                entry.1 += v;
                entry.2 += 1;
            }
            if let Some(o) = offers {
                entry.3 += o;
                entry.4 += 1;
                if o == 1 {
                    entry.5 += 1;
                }
            }
        }
        Ok(groups
            .into_iter()
            .map(
                |(cpv_division, (rows, sum, n, osum, on, single))| CpvSpread {
                    cpv_division,
                    awards: rows,
                    avg_awarded: decimal_avg(sum, n),
                    avg_offers: if on == 0 {
                        None
                    } else {
                        Some(osum as f64 / on as f64)
                    },
                    single_bid_share: if on == 0 {
                        0.0
                    } else {
                        single as f64 / on as f64
                    },
                },
            )
            .collect())
    }

    /// Offer-range spreads from eForms award lots (BT-710/711): per-division
    /// averages of lowest/highest admissible bids. Lots with NULL range values
    /// are excluded from the range averages (unknown, never zero) but still
    /// counted; divisions whose lots all lack a bound report None for it.
    pub fn lot_range_spreads(&self) -> Result<Vec<LotRangeSpread>, Box<dyn Error>> {
        let mut statement = self
            .connection
            .prepare("SELECT cpv_codes, value, lowest_value, highest_value FROM award_lots")?;
        // division -> per-division accumulator; lots with NULL range values
        // are excluded from the range averages (unknown, never zero).
        let mut groups: std::collections::BTreeMap<String, RangeAccum> =
            std::collections::BTreeMap::new();
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<Decimal>>(1)?,
                row.get::<_, Option<Decimal>>(2)?,
                row.get::<_, Option<Decimal>>(3)?,
            ))
        })?;
        for row in rows {
            let (cpvs_json, value, lowest, highest) = row?;
            let cpvs: Vec<String> = serde_json::from_str(&cpvs_json).unwrap_or_default();
            let Some(first) = cpvs.into_iter().find(|c| c.len() >= 2) else {
                continue;
            };
            let entry = groups.entry(first[..2].to_owned()).or_default();
            entry.lots += 1;
            if let Some(v) = value {
                entry.value_sum += v;
                entry.value_n += 1;
            }
            if let Some(v) = lowest {
                entry.low_sum += v;
                entry.low_n += 1;
            }
            if let Some(v) = highest {
                entry.high_sum += v;
                entry.high_n += 1;
            }
        }
        Ok(groups
            .into_iter()
            .map(|(cpv_division, acc)| LotRangeSpread {
                cpv_division,
                lots: acc.lots,
                avg_awarded: decimal_avg(acc.value_sum, acc.value_n),
                avg_lowest: decimal_avg(acc.low_sum, acc.low_n),
                avg_highest: decimal_avg(acc.high_sum, acc.high_n),
            })
            .collect())
    }

    /// Awarded lots where the lot value reaches `min_ratio` of the winner's
    /// latest turnover. Winner identifiers are normalized to digits (9-digit
    /// zero-padded, mirroring `normalize_award_identifier`) before joining to
    /// `nbb_financials`; the newest fiscal year per enterprise wins. A company
    /// can hold several rows for one fiscal year (an XBRL export yields a
    /// current and a comparative period), so ties break on the latest
    /// `period_end` and then `source_ref`: without that the chosen row, and
    /// therefore the ratio, is arbitrary.
    pub fn capacity_outliers(
        &self,
        min_ratio: f64,
    ) -> Result<Vec<CapacityOutlier>, Box<dyn Error>> {
        let has_period_end = column_exists(&self.connection, "nbb_financials", "period_end")?;
        let period_end = if has_period_end {
            "period_end"
        } else {
            "CAST(NULL AS VARCHAR)"
        };
        // The projection needs a name; the ORDER BY use above does not.
        let period_end_column = if has_period_end {
            "period_end"
        } else {
            "CAST(NULL AS VARCHAR) AS period_end"
        };
        let mut statement = self.connection.prepare(&format!(
            r#"
            WITH norm_lots AS (
                SELECT publication_number, lot_id, winner_name, value,
                       CASE WHEN length(digits) = 9 THEN '0' || digits ELSE digits END AS kbo
                FROM (
                    SELECT publication_number, lot_id, winner_name, value,
                           regexp_replace(winner_identifier, '[^0-9]', '', 'g') AS digits
                    FROM award_lots
                    WHERE value IS NOT NULL
                )
            ),
            latest_fin AS (
                SELECT enterprise_kbo, fiscal_year, {period_end_column}, turnover, staff_fte
                FROM (
                    SELECT *, ROW_NUMBER() OVER (
                        PARTITION BY enterprise_kbo
                        ORDER BY fiscal_year DESC, {period_end} DESC NULLS LAST, source_ref DESC
                    ) AS rn
                    FROM nbb_financials
                )
                WHERE rn = 1
            )
            SELECT n.publication_number, n.lot_id, n.winner_name, n.kbo,
                   n.value, f.fiscal_year, f.period_end, f.turnover, f.staff_fte,
                   CAST(n.value AS DOUBLE) / NULLIF(CAST(f.turnover AS DOUBLE), 0) AS ratio
            FROM norm_lots n
            JOIN latest_fin f ON f.enterprise_kbo = n.kbo
            WHERE f.turnover IS NOT NULL
              AND CAST(n.value AS DOUBLE) / NULLIF(CAST(f.turnover AS DOUBLE), 0) >= ?
            ORDER BY ratio DESC, n.publication_number, n.lot_id
            "#,
        ))?;
        let rows = statement.query_map(params![min_ratio], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Decimal>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Decimal>(7)?,
                row.get::<_, Option<Decimal>>(8)?,
                row.get::<_, f64>(9)?,
            ))
        })?;
        rows.map(|row| {
            let (
                publication_number,
                lot_id,
                winner_name,
                winner_kbo,
                award_value,
                fiscal_year,
                period_end,
                turnover,
                staff_fte,
                value_to_turnover,
            ) = row?;
            Ok(CapacityOutlier {
                publication_number,
                lot_id,
                winner_name,
                winner_kbo,
                award_value,
                fiscal_year,
                period_end,
                turnover,
                staff_fte,
                value_to_turnover,
            })
        })
        .collect()
    }

    /// How much of the capacity check could actually be performed.
    ///
    /// `capacity_outliers` inner-joins on a non-null turnover, so a lot whose
    /// winner filed no turnover simply disappears. Without this companion
    /// count an empty report reads as "nothing to flag" when it usually means
    /// "almost nothing was checkable". Missing evidence is reported as
    /// unknown, never as a clean bill of health.
    pub fn capacity_coverage(&self) -> Result<CapacityCoverage, Box<dyn Error>> {
        let period_end = if column_exists(&self.connection, "nbb_financials", "period_end")? {
            "period_end"
        } else {
            "CAST(NULL AS VARCHAR)"
        };
        let mut statement = self.connection.prepare(&format!(
            r#"
            WITH lots AS (
                SELECT value,
                       COALESCE(regexp_replace(winner_identifier, '[^0-9]', '', 'g'), '') AS digits
                FROM award_lots
                WHERE value IS NOT NULL
            ),
            keyed AS (
                SELECT value, digits,
                       CASE WHEN length(digits) = 9 THEN '0' || digits ELSE digits END AS kbo,
                       length(digits) IN (9, 10) AS is_belgian
                FROM lots
            ),
            latest_fin AS (
                SELECT enterprise_kbo, turnover
                FROM (
                    SELECT *, ROW_NUMBER() OVER (
                        PARTITION BY enterprise_kbo
                        ORDER BY fiscal_year DESC, {period_end} DESC NULLS LAST, source_ref DESC
                    ) AS rn
                    FROM nbb_financials
                )
                WHERE rn = 1
            )
            SELECT count(*),
                   count(*) FILTER (WHERE k.digits = ''),
                   count(*) FILTER (WHERE k.digits <> '' AND NOT k.is_belgian),
                   count(*) FILTER (WHERE k.is_belgian AND f.enterprise_kbo IS NULL),
                   count(*) FILTER (WHERE k.is_belgian AND f.enterprise_kbo IS NOT NULL
                                      AND (f.turnover IS NULL OR f.turnover <= 0)),
                   count(*) FILTER (WHERE k.is_belgian AND f.turnover > 0)
            FROM keyed k
            LEFT JOIN latest_fin f ON f.enterprise_kbo = k.kbo AND k.is_belgian
            "#,
        ))?;
        let mut rows = statement.query([])?;
        let row = rows.next()?.ok_or("capacity coverage returned no row")?;
        Ok(CapacityCoverage {
            lots_with_value: row.get(0)?,
            without_winner_identifier: row.get(1)?,
            not_belgian_identifier: row.get(2)?,
            without_financials: row.get(3)?,
            without_turnover: row.get(4)?,
            assessed: row.get(5)?,
        })
    }

    /// How many stored filings can actually be dated.
    ///
    /// Stores built by an older importer keep `period_start`/`period_end`
    /// empty, which makes every filing undatable without any visible error.
    /// Reporting it at import time turns a silent staleness into a number.
    pub fn nbb_period_coverage(&self) -> Result<NbbPeriodCoverage, Box<dyn Error>> {
        // A store older than these columns reports zero coverage rather than a
        // binder error, which is the same answer: nothing here is datable.
        let period_start = if column_exists(&self.connection, "nbb_financials", "period_start")? {
            "period_start"
        } else {
            "CAST(NULL AS VARCHAR)"
        };
        let period_end = if column_exists(&self.connection, "nbb_financials", "period_end")? {
            "period_end"
        } else {
            "CAST(NULL AS VARCHAR)"
        };
        let mut statement = self.connection.prepare(&format!(
            r#"
            SELECT count(*),
                   count({period_start}),
                   count({period_end}),
                   count(*) FILTER (
                       WHERE {period_start} IS NOT NULL AND {period_end} IS NOT NULL
                   )
            FROM nbb_financials
            "#,
        ))?;
        let mut rows = statement.query([])?;
        let row = rows.next()?.ok_or("nbb period coverage returned no row")?;
        Ok(NbbPeriodCoverage {
            rows: row.get(0)?,
            with_period_start: row.get(1)?,
            with_period_end: row.get(2)?,
            with_both_boundaries: row.get(3)?,
        })
    }

    fn prune_analysis_cache(&self) -> Result<usize, duckdb::Error> {
        self.prune_analysis_cache_with_budget(MAX_PERSISTENT_ANALYSIS_CACHE_BYTES)
    }

    fn prune_analysis_cache_with_budget(&self, byte_budget: i64) -> Result<usize, duckdb::Error> {
        let mut removed = retry_write(|| {
            self.connection.execute(
                "DELETE FROM analysis_cache WHERE created_at < CAST(now() AS TIMESTAMP) - INTERVAL '30 days'",
                [],
            )
        })?;
        let count =
            self.connection
                .query_row("SELECT count(*) FROM analysis_cache", [], |row| {
                    row.get::<_, i64>(0)
                })?;
        if count > MAX_PERSISTENT_ANALYSIS_CACHE_ENTRIES {
            removed += retry_write(|| {
                self.connection.execute(
                    "DELETE FROM analysis_cache WHERE cache_key IN (
                    SELECT cache_key FROM analysis_cache
                    ORDER BY created_at ASC, cache_key ASC
                    LIMIT ?
                )",
                    params![count - MAX_PERSISTENT_ANALYSIS_CACHE_ENTRIES],
                )
            })?;
        }
        // Remove legacy oversized decisions before computing the retained
        // newest-first byte window. UTF-8 byte counts, not character counts.
        removed += retry_write(|| {
            self.connection.execute(
            "DELETE FROM analysis_cache WHERE octet_length(encode(CAST(decision AS VARCHAR))) > ?",
            params![MAX_ANALYSIS_DECISION_BYTES as i64],
        )
        })?;
        removed += retry_write(|| {
            self.connection.execute(
                "DELETE FROM analysis_cache WHERE cache_key IN (
                SELECT cache_key FROM (
                    SELECT cache_key,
                        sum(octet_length(encode(CAST(decision AS VARCHAR)))) OVER (
                            ORDER BY created_at DESC, cache_key DESC
                            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                        ) AS retained_bytes
                    FROM analysis_cache
                ) WHERE retained_bytes > ?
            )",
                params![byte_budget],
            )
        })?;
        Ok(removed)
    }

    /// The cache entry count without pruning, so read-only callers can
    /// report health while an ingest job holds the write lock.
    pub fn analysis_cache_len(&self) -> Result<i64, duckdb::Error> {
        self.connection
            .query_row("SELECT count(*) FROM analysis_cache", [], |row| row.get(0))
    }

    pub fn analysis_cache_count(&self) -> Result<i64, duckdb::Error> {
        self.prune_analysis_cache()?;
        self.connection
            .query_row("SELECT count(*) FROM analysis_cache", [], |row| row.get(0))
    }

    pub fn clear_analysis_cache(&self) -> Result<usize, duckdb::Error> {
        self.connection.execute("DELETE FROM analysis_cache", [])
    }

    pub fn cached_analysis(
        &self,
        cache_key: &str,
    ) -> Result<Option<AnalysisDecision>, Box<dyn Error>> {
        let result = self.connection.query_row(
            "SELECT decision FROM analysis_cache WHERE cache_key = ? AND created_at >= CAST(now() AS TIMESTAMP) - INTERVAL '30 days'
             AND octet_length(encode(CAST(decision AS VARCHAR))) <= ?",
            params![cache_key, MAX_ANALYSIS_DECISION_BYTES as i64],
            |row| row.get::<_, String>(0),
        );
        match result {
            Ok(encoded) => {
                let decision: AnalysisDecision = serde_json::from_str(&encoded)?;
                // A cache hit adds cached=true; validate the deserialized form as
                // well in case a legacy row used a different JSON encoding.
                decision.validate_size()?;
                Ok(Some(decision))
            }
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save_analysis(
        &self,
        cache_key: &str,
        decision: &AnalysisDecision,
    ) -> Result<(), Box<dyn Error>> {
        self.save_analysis_with_budget(cache_key, decision, MAX_PERSISTENT_ANALYSIS_CACHE_BYTES)
    }

    fn save_analysis_with_budget(
        &self,
        cache_key: &str,
        decision: &AnalysisDecision,
        byte_budget: i64,
    ) -> Result<(), Box<dyn Error>> {
        decision.validate_size()?;
        let decision = serde_json::to_string(decision)?;
        let _cache_write = ANALYSIS_CACHE_WRITE_LOCK
            .lock()
            .map_err(|_| "analysis cache writer is unavailable")?;
        self.with_transaction(|| {
            retry_write(|| {
                self.connection.execute(
                    r#"
            INSERT INTO analysis_cache (cache_key, decision)
            VALUES (?, ?)
            ON CONFLICT (cache_key) DO UPDATE SET
                decision = excluded.decision,
                created_at = now()
            "#,
                    params![cache_key, decision],
                )
            })?;
            self.prune_analysis_cache_with_budget(byte_budget)?;
            Ok(())
        })
    }

    /// Apply a partial judgment and audit its resolved state in one transaction.
    /// Concurrent conflicts replay the read/merge/write, not a stale snapshot.
    pub fn update_feedback(
        &self,
        actor: &str,
        enterprise_number: &str,
        source: &str,
        external_id: &str,
        update: &FeedbackUpdate,
    ) -> Result<FeedbackLabel, Box<dyn Error>> {
        self.with_transaction(|| {
            let previous = self
                .feedback_details_for(enterprise_number)?
                .remove(&(source.to_owned(), external_id.to_owned()));
            let resolved = FeedbackLabel {
                relevant: update.relevant,
                pursuable: update
                    .pursuable
                    .resolve(previous.as_ref().and_then(|p| p.pursuable)),
                outcome: update
                    .outcome
                    .resolve(previous.as_ref().and_then(|p| p.outcome.clone())),
                reason: update
                    .reason
                    .resolve(previous.as_ref().and_then(|p| p.reason.clone())),
            };
            self.record_feedback(
                enterprise_number,
                source,
                external_id,
                resolved.relevant,
                resolved.pursuable,
                resolved.outcome.as_deref(),
                resolved.reason.as_deref(),
            )?;
            self.record_audit(
                actor,
                AUDIT_FEEDBACK_SAVE,
                Some(enterprise_number),
                Some(source),
                Some(external_id),
                Some(&serde_json::to_string(&resolved)?),
            )?;
            Ok(resolved)
        })
    }

    /// Full replacement primitive for imports/fixtures. Interactive writers use
    /// update_feedback so omission, clearing and auditing share one boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn record_feedback(
        &self,
        enterprise_number: &str,
        source: &str,
        external_id: &str,
        relevant: bool,
        pursuable: Option<bool>,
        outcome: Option<&str>,
        reason: Option<&str>,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(outcome) = outcome {
            validate_feedback_outcome(outcome)?;
        }
        // The API writes through open_initialized, which skips migrations, so
        // ensure the columns exist here; IF NOT EXISTS makes this a no-op
        // after the first run on an old database.
        self.connection.execute(
            "ALTER TABLE feedback ADD COLUMN IF NOT EXISTS pursuable BOOLEAN",
            [],
        )?;
        self.connection.execute(
            "ALTER TABLE feedback ADD COLUMN IF NOT EXISTS outcome VARCHAR",
            [],
        )?;
        retry_write(|| {
            self.connection.execute(
                r#"
            INSERT INTO feedback (
                enterprise_number, source, external_id, relevant, pursuable, outcome, reason, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, now())
            ON CONFLICT (enterprise_number, source, external_id) DO UPDATE SET
                relevant = excluded.relevant,
                pursuable = excluded.pursuable,
                outcome = excluded.outcome,
                reason = excluded.reason,
                created_at = now()
            "#,
                params![enterprise_number, source, external_id, relevant, pursuable, outcome, reason],
            )
        })?;
        Ok(())
    }

    /// One feedback row: topic relevance plus the pursuable/outcome split.
    /// `pursuable`/`outcome` are None on rows written before the split or when
    /// the judge left them unset.
    pub fn feedback_details_for(
        &self,
        enterprise_number: &str,
    ) -> Result<HashMap<(String, String), FeedbackLabel>, Box<dyn Error>> {
        let pursuable_expr = if self.schema.feedback_pursuable {
            "pursuable"
        } else {
            "NULL::BOOLEAN"
        };
        let outcome_expr = if self.schema.feedback_outcome {
            "outcome"
        } else {
            "NULL::VARCHAR"
        };
        let sql = format!(
            "SELECT source, external_id, relevant, {pursuable_expr}, {outcome_expr}, reason \
             FROM feedback WHERE enterprise_number = ?"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(params![enterprise_number], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<bool>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(
                |(source, external_id, relevant, pursuable, outcome, reason)| {
                    (
                        (source, external_id),
                        FeedbackLabel {
                            relevant,
                            pursuable,
                            outcome,
                            reason,
                        },
                    )
                },
            )
            .collect())
    }

    pub fn feedback_for(
        &self,
        enterprise_number: &str,
    ) -> Result<HashMap<(String, String), bool>, Box<dyn Error>> {
        // Ranking relevance: pursuable when the judge set it, else the legacy
        // topic-relevant flag. Old databases without the columns read exactly
        // as before.
        Ok(self
            .feedback_details_for(enterprise_number)?
            .into_iter()
            .map(|(key, label)| (key, label.pursuable.unwrap_or(label.relevant)))
            .collect())
    }

    pub fn awards_for(&self, enterprise_number: &str) -> Result<Vec<AwardRecord>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT publication_number, title, buyer, winner_identifiers,
                   winner_names, cpv_codes, values, links, raw
            FROM awards
            WHERE enterprise_number = ?
            ORDER BY publication_number
            "#,
        )?;
        let rows = statement
            .query_map(params![enterprise_number], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(
                    publication_number,
                    title,
                    buyer,
                    identifiers,
                    names,
                    cpvs,
                    values,
                    links,
                    raw,
                )| {
                    Ok(AwardRecord {
                        publication_number,
                        title: title.unwrap_or_default(),
                        buyer,
                        winner_identifiers: serde_json::from_str(&identifiers)?,
                        winner_names: serde_json::from_str(&names)?,
                        cpv_codes: serde_json::from_str(&cpvs)?,
                        values: json_decimal_values(&values)?,
                        links: serde_json::from_str(&links)?,
                        raw: serde_json::from_str(&raw)?,
                    })
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ted::notice_to_tender;

    fn conflict_error() -> duckdb::Error {
        duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(duckdb::ffi::DuckDBError),
            Some("TransactionContext Error: Conflict on update!".to_owned()),
        )
    }

    #[test]
    fn classifies_retryable_write_errors() {
        assert!(is_retryable_write_error(&conflict_error()));
        assert!(is_retryable_write_error(&duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(duckdb::ffi::DuckDBError),
            Some("Constraint Error: Duplicate key \"ted\" violates primary key".to_owned()),
        )));
        assert!(is_retryable_write_error(&duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(duckdb::ffi::DuckDBError),
            Some("Could not set lock on file: Conflicting lock is held".to_owned()),
        )));
        // Data errors such as NOT NULL violations must fail fast, not retry.
        assert!(!is_retryable_write_error(&duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(duckdb::ffi::DuckDBError),
            Some("Constraint Error: NOT NULL constraint violated: column 'title'".to_owned()),
        )));
        assert!(!is_retryable_write_error(
            &duckdb::Error::QueryReturnedNoRows
        ));
    }

    #[test]
    fn retry_write_recovers_from_conflicts() {
        let mut attempts = 0;
        let result = retry_write(|| {
            attempts += 1;
            if attempts < 3 {
                Err(conflict_error())
            } else {
                Ok(42)
            }
        })
        .expect("retry should eventually succeed");
        assert_eq!(result, 42);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn retry_write_fails_fast_on_data_errors() {
        let mut attempts = 0;
        let result = retry_write(|| {
            attempts += 1;
            Err::<usize, _>(duckdb::Error::DuckDBFailure(
                duckdb::ffi::Error::new(duckdb::ffi::DuckDBError),
                Some("Constraint Error: NOT NULL constraint violated".to_owned()),
            ))
        });
        assert!(result.is_err());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn rejects_limits_that_do_not_fit_sql_integer_type() {
        assert!(sql_limit(usize::MAX).is_err());
        assert_eq!(sql_limit(100).expect("small limit should fit"), 100);
    }
    use serde_json::json;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reads_numeric_and_string_award_values() {
        assert_eq!(
            json_decimal_values("[125000, \"584550\"]").unwrap(),
            vec![Decimal::new(125000, 0), Decimal::new(584550, 0)]
        );
    }

    /// Coverage is reported as a publication range, and rows written before the
    /// column existed are counted separately rather than pretending the range
    /// covers the whole store.
    #[test]
    fn publication_range_reports_coverage_and_unknown_rows() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-coverage-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let dated = notice_to_tender(&json!({
            "publication-number": "dated-1",
            "notice-title": "Dated",
            "publication-date": "2026-09-18",
            "deadline-receipt-tender-date-lot": "2026-11-01"
        }));
        let later = notice_to_tender(&json!({
            "publication-number": "dated-2",
            "notice-title": "Later",
            "publication-date": "2026-10-16"
        }));
        // A row from an older writer: stored, but with no publication date.
        let undated = notice_to_tender(&json!({
            "publication-number": "undated-1",
            "notice-title": "Stored before the column existed"
        }));
        store
            .upsert_many([&dated, &later, &undated])
            .expect("tenders should store");
        let (oldest, newest, unknown) = store.tender_publication_range().expect("range");
        assert_eq!(oldest.as_deref(), Some("2026-09-18"));
        assert_eq!(newest.as_deref(), Some("2026-10-16"));
        assert_eq!(unknown, 1);
        let _ = std::fs::remove_file(&path);
    }

    /// The read path collapses accounting rows into engagements: an engagement
    /// repeats on every liquidation, and a listing that showed rows made one
    /// contract look like several.
    #[test]
    fn engagements_collapse_accounting_rows_for_one_awardee() {
        use crate::bxl_inventory::InventoryAward;
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-engagements-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let row = |hash: &str| InventoryAward {
            buyer_identifier: Some("0207365709".to_owned()),
            buyer_name: Some("Administration communale d'Evere".to_owned()),
            awardee_identifier: Some("0429366144".to_owned()),
            awardee_name: Some("RENEWI BELGIUM".to_owned()),
            engagement_id: Some("2024002611".to_owned()),
            engagement_date: Some("2024-03-20".to_owned()),
            engagement_amount: Some(Decimal::new(5_887_198, 2)),
            object: Some("LOT 12 ENCOMBRANTS".to_owned()),
            row_hash: hash.to_owned(),
        };
        let awards = [
            row("hash-1"),
            row("hash-2"),
            row("hash-3"),
            InventoryAward {
                engagement_id: None,
                row_hash: "hash-4".to_owned(),
                ..row("unused")
            },
        ];
        store
            .upsert_inventory_awards("evere2024.xlsx", &awards)
            .expect("rows should import");
        assert_eq!(store.inventory_award_count(None).expect("count"), 4);
        let engagements = store
            .inventory_engagements_for_awardee("0429366144", 10)
            .expect("engagements should load");
        assert_eq!(engagements.len(), 2, "three rows are one engagement");
        assert_eq!(engagements[0].accounting_rows, 3);
        assert_eq!(
            engagements[0].engagement_amount,
            Some(Decimal::new(5_887_198, 2))
        );
        assert_eq!(
            engagements[1].accounting_rows, 1,
            "a row without an engagement id cannot collapse into another"
        );
        // A placeholder id is not an engagement: 657 of 5,304 real imported rows
        // carry '0'. Identical accounting rows must still collapse — that is what
        // `accounting_rows` reports — but the id must not be reported as "0".
        let mut placeholders = Vec::new();
        for hash in ["ph-1", "ph-2", "ph-3"] {
            placeholders.push(InventoryAward {
                engagement_id: Some("0".to_owned()),
                row_hash: hash.to_owned(),
                ..row("unused")
            });
        }
        // Same placeholder, different amount: a different purchase, so its own row.
        placeholders.push(InventoryAward {
            engagement_id: Some("0".to_owned()),
            row_hash: "ph-4".to_owned(),
            engagement_amount: Some(Decimal::new(1_234, 2)),
            ..row("unused")
        });
        store
            .upsert_inventory_awards("evere2025.xlsx", &placeholders)
            .expect("placeholder rows should import");
        let after = store
            .inventory_engagements_for_awardee("0429366144", 20)
            .expect("engagements should load");
        let collapsed = after
            .iter()
            .find(|engagement| {
                engagement.engagement_id.is_none()
                    && engagement.engagement_amount == Some(Decimal::new(5_887_198, 2))
                    && engagement.accounting_rows == 3
            })
            .expect("three identical placeholder rows collapse into one entry");
        assert!(
            collapsed.engagement_id.is_none(),
            "a placeholder id must never be reported as an engagement"
        );
        assert!(
            after
                .iter()
                .all(|engagement| engagement.engagement_id.as_deref() != Some("0")),
            "no caller may see \"0\" as an engagement id"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A database written before the notice-type and recognition columns existed
    /// must still be readable: read-only readers (`report`, `recommend`, the API
    /// on an existing file) cannot migrate it, and failing to bind was a
    /// regression.
    /// Inventory rows are keyed by file and content hash: re-importing a year
    /// updates instead of duplicating, and a database written before the table
    /// existed returns nothing rather than failing to bind.
    #[test]
    fn inventory_awards_round_trip_and_are_idempotent() {
        use crate::bxl_inventory::InventoryAward;
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aanbestedingen-inventory-store-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let award = InventoryAward {
            buyer_identifier: Some("0207365709".to_owned()),
            buyer_name: Some("Administration communale d'Evere".to_owned()),
            awardee_identifier: Some("0403170701".to_owned()),
            awardee_name: Some("Some Supplier".to_owned()),
            engagement_id: Some("2024021141".to_owned()),
            engagement_date: Some("2024-12-31".to_owned()),
            engagement_amount: Some(Decimal::new(486_323, 2)),
            object: Some("FACTURES FIN 2024".to_owned()),
            row_hash: "abc123".to_owned(),
        };
        let other = InventoryAward {
            row_hash: "def456".to_owned(),
            engagement_amount: Some(Decimal::new(2_404_286, 2)),
            ..award.clone()
        };
        assert_eq!(
            store
                .upsert_inventory_awards("evere2024.xlsx", &[award.clone(), other])
                .expect("inventory should import"),
            2
        );
        assert_eq!(store.inventory_award_count(None).expect("count"), 2);
        assert_eq!(
            store
                .upsert_inventory_awards("evere2024.xlsx", &[award.clone()])
                .expect("re-import should succeed"),
            1
        );
        assert_eq!(
            store.inventory_award_count(None).expect("count"),
            2,
            "re-importing the same row must not duplicate it"
        );
        let found = store
            .inventory_awards_for_awardee("0403170701", 10)
            .expect("awardee lookup");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].engagement_amount, Some(Decimal::new(2_404_286, 2)));
        assert_eq!(
            store
                .inventory_award_count(Some("0207365709"))
                .expect("buyer count"),
            2
        );
        assert_eq!(
            store
                .inventory_award_count(Some("0999999999"))
                .expect("unknown count"),
            0
        );
        drop(store);
        // A database written by an older build has no inventory table at all.
        let legacy_path =
            std::env::temp_dir().join(format!("aanbestedingen-inventory-legacy-{suffix}.duckdb"));
        {
            let connection = Connection::open(&legacy_path).expect("legacy database should open");
            connection
                .execute_batch(
                    "CREATE TABLE tenders (source VARCHAR, external_id VARCHAR, title VARCHAR);",
                )
                .expect("legacy schema should create");
        }
        let legacy =
            TenderStore::open_read_only(&legacy_path).expect("legacy database should open");
        assert_eq!(legacy.inventory_award_count(None).expect("count"), 0);
        assert!(legacy
            .inventory_awards_for_awardee("0403170701", 10)
            .expect("lookup")
            .is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&legacy_path);
    }

    #[test]
    fn read_only_reads_a_database_without_the_new_columns() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-old-schema-{suffix}.duckdb"));
        {
            let connection = Connection::open(&path).expect("legacy database should open");
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE tenders (
                        source VARCHAR, external_id VARCHAR, title VARCHAR, buyer VARCHAR,
                        description VARCHAR, cpv_codes JSON, regions JSON,
                        estimated_value DECIMAL(18,2), deadline DATE, languages JSON,
                        requirements JSON, document_urls JSON, raw JSON
                    );
                    CREATE TABLE preferences (
                        enterprise_number VARCHAR PRIMARY KEY, confirmed_activities JSON,
                        excluded_terms JSON, preferred_regions JSON, languages JSON,
                        minimum_value DECIMAL(18,2), maximum_value DECIMAL(18,2)
                    );
                    INSERT INTO tenders VALUES (
                        'ted', 'old-1', 'Tender from an older build', NULL, '', '[]', '[]',
                        NULL, '2026-12-01', '[]', '[]', '[]', '{}'
                    );
                    INSERT INTO preferences VALUES (
                        '0123.456.789', '[]', '[]', '[]', '[]', NULL, NULL
                    );
                    "#,
                )
                .expect("legacy schema should create");
        }

        let store =
            TenderStore::open_read_only(&path).expect("legacy database should open read-only");
        let tenders = store.tenders(10).expect("tenders should load");
        assert_eq!(tenders.len(), 1);
        assert_eq!(tenders[0].notice_type, None);
        let (total_bytes, row_bytes) = store
            .tender_payload_size_bytes()
            .expect("snapshot size preflight should support legacy columns");
        assert!(total_bytes > 0);
        assert_eq!(total_bytes, row_bytes);
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 18).expect("date should be valid");
        assert_eq!(
            store
                .open_tenders(10, today, None)
                .expect("candidates should load")
                .len(),
            1,
            "an unknown notice type stays eligible"
        );
        let preferences = store
            .preferences_for("0123.456.789")
            .expect("preferences should load");
        assert_eq!(preferences.recognition_class, None);
        assert!(preferences.recognition_categories.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// Lot values must survive the store round trip, and legacy rows
    /// without the column must assess at the notice total.
    #[test]
    fn lot_values_round_trip_through_the_store() {
        use crate::Tender;
        use rust_decimal::Decimal;
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-lots-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "lots".to_owned(),
            title: "Soil analyses".to_owned(),
            estimated_value: Some(Decimal::new(1_238_890, 0)),
            lot_values: vec![Decimal::new(384_390, 0), Decimal::new(164_580, 0)],
            ..Tender::default()
        };
        store.upsert_many([&tender]).expect("upsert should work");
        let read = store
            .tender("test", "lots")
            .expect("read should work")
            .expect("tender should exist");
        assert_eq!(read.lot_values, tender.lot_values);
        assert_eq!(read.assessed_value(), Some(Decimal::new(164_580, 0)));
        let _ = std::fs::remove_file(&path);
    }

    /// The incremental watermark is the date of the newest *rolling refresh*.
    /// A backfill chunk must not set it: a closed historical month says nothing
    /// about how current the rolling coverage is, and treating it as a watermark
    /// would narrow the next refresh to a week it may never have covered.
    /// Award runs share the table but answer a different question. If they were read
    /// back as "the last run", a weekly award fetch would replace the tender
    /// coverage card with award counts and a null population.
    #[test]
    fn award_runs_are_kept_out_of_the_tender_readers() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-award-kinds-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        store
            .record_refresh_run("refresh", "ACTIVE", "days=30", 10, 10, Some(120), 5)
            .expect("a tender run should record");
        store
            .record_refresh_run(
                "awards",
                "ACTIVE",
                "awards 2026-11-01..2026-11-07",
                7,
                9,
                None,
                5,
            )
            .expect("an award run should record");
        assert_eq!(
            store
                .latest_award_run()
                .expect("read should work")
                .map(|r| r.kind),
            Some("awards".to_owned()),
            "the award run is readable"
        );
        let tender = store
            .latest_refresh_run()
            .expect("read should work")
            .expect("the tender run is still the last tender run");
        assert_eq!(tender.kind, "refresh");
        assert_eq!(tender.total_notice_count, Some(120));
        let _ = std::fs::remove_file(&path);
    }

    /// The dangerous ordering: a partial run is recorded *after* a completed one, so
    /// it has the highest id and the newest timestamp. Reading the newest row of any
    /// kind would move the watermark forward past months that were never fetched,
    /// and the next incremental run would start after them — losing them for good.
    /// This was a real defect: the partial row was written with `kind = 'refresh'`.
    #[test]
    fn a_partial_run_never_moves_an_older_watermark() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aanbestedingen-partial-watermark-{suffix}.duckdb"));
        drop(TenderStore::open(&path).expect("store should open"));
        // Write the dates directly: `record_refresh_run` always stamps the current
        // day, and the test needs the partial row to be strictly newer.
        let connection = Connection::open(&path).expect("database should open");
        connection
            .execute_batch(
                "INSERT INTO refresh_runs
                    (started_at, kind, scope, window_desc, fetched, stored, duration_ms)
                 VALUES
                    ('2026-01-05 06:00:00+00', 'refresh', 'ACTIVE', 'days=30', 1877, 1877, 25000),
                    ('2026-11-07 06:00:00+00', 'refresh-partial', 'ACTIVE',
                     'days=30 (2 of 4 chunks walked, fetch failed)', 900, 900, 12000);",
            )
            .expect("runs should insert");
        drop(connection);
        let store = TenderStore::open(&path).expect("store should reopen");
        assert_eq!(
            store.last_refresh_date().expect("read should work"),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 5),
            "the partial run is not coverage, so the watermark stays on the completed run"
        );
        let latest = store
            .latest_refresh_run()
            .expect("read should work")
            .expect("the partial run is recorded");
        assert_eq!(
            latest.kind, "refresh-partial",
            "a human still sees the most recent run, including its failure"
        );
        assert_eq!(latest.total_notice_count, None);
        let _ = std::fs::remove_file(&path);
    }
    #[test]
    fn last_refresh_date_tracks_refreshes_and_ignores_backfill_chunks() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-watermark-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        assert_eq!(
            store.last_refresh_date().expect("read should work"),
            None,
            "a fresh store has no watermark"
        );
        store
            .record_refresh_run(
                "backfill",
                "ALL",
                "2024-08-01..2024-08-31",
                1240,
                1240,
                None,
                20000,
            )
            .expect("backfill chunk should record");
        assert_eq!(
            store.last_refresh_date().expect("read should work"),
            None,
            "a backfill chunk is not a coverage watermark"
        );
        store
            .record_refresh_run("refresh", "ACTIVE", "days=30", 1877, 1877, None, 25000)
            .expect("refresh run should record");
        let watermark = store
            .last_refresh_date()
            .expect("read should work")
            .expect("a refresh run sets the watermark");
        let today = chrono::Utc::now().date_naive();
        assert!(
            watermark == today || watermark == today - Duration::days(1),
            "the recorded date is the run's own UTC day, got {watermark} against {today}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refresh_runs_record_which_population_a_run_drew_from() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-refresh-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        assert!(store
            .latest_refresh_run()
            .expect("read should work")
            .is_none());
        store
            .record_refresh_run(
                "backfill",
                "ALL",
                "2024-08-01..2024-08-31",
                1240,
                1240,
                None,
                20000,
            )
            .expect("backfill chunk should record");
        let run_id = store
            .record_refresh_run(
                "refresh",
                "ACTIVE",
                "days=30",
                1877,
                1877,
                Some(19093),
                25000,
            )
            .expect("refresh run should record");
        store
            .record_refresh_evidence(
                run_id,
                &CorpusEvidence {
                    opportunities: 1877,
                    with_deadline: 1200,
                    with_value: 500,
                    with_requirements: 400,
                    stated: 300,
                    hinted: 20,
                    unknown: 1457,
                },
            )
            .expect("evidence snapshot should record");
        let latest = store
            .latest_refresh_run()
            .expect("read should work")
            .expect("a run should be recorded");
        assert_eq!(latest.kind, "refresh");
        assert_eq!(latest.scope, "ACTIVE");
        assert_eq!(latest.window, "days=30");
        assert_eq!(latest.total_notice_count, Some(19093));
        let evidence = latest.evidence.expect("refresh runs carry a snapshot");
        assert_eq!(evidence.opportunities, 1877);
        assert_eq!(evidence.with_deadline, 1200);
        assert_eq!(evidence.stated, 300);
        assert_eq!(evidence.hinted, 20);
        assert_eq!(evidence.unknown, 1457);
        assert!(store
            .record_refresh_run("sweep", "ACTIVE", "days=30", 1, 1, None, 1)
            .is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn enterprises_with_context_lists_only_judged_companies() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-context-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        assert!(store
            .enterprises_with_context()
            .expect("read should work")
            .is_empty());
        store
            .save_preferences("0123.456.789", &CompanyPreferences::default())
            .expect("preferences should save");
        store
            .record_feedback("0123.456.789", "bda", "notice-1", true, None, None, None)
            .expect("feedback should save");
        let context = store.enterprises_with_context().expect("read should work");
        assert_eq!(context, HashSet::from(["0123.456.789".to_owned()]));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn feedback_updates_preserve_omissions_and_clear_only_explicit_fields() {
        let store = TenderStore::open(":memory:").unwrap();
        for pursuable in [None, Some(false), Some(true)] {
            for outcome in [None, Some("bid"), Some("no-bid"), Some("won"), Some("lost")] {
                let initial = FeedbackUpdate::replacement(
                    true,
                    pursuable,
                    outcome.map(str::to_owned),
                    Some("retain reason".into()),
                );
                store
                    .update_feedback("op", "company", "ted", "one", &initial)
                    .unwrap();
                let partial: FeedbackUpdate =
                    serde_json::from_str(r#"{"relevant":false}"#).unwrap();
                let saved = store
                    .update_feedback("op", "company", "ted", "one", &partial)
                    .unwrap();
                assert!(!saved.relevant);
                assert_eq!(saved.pursuable, pursuable);
                assert_eq!(saved.outcome.as_deref(), outcome);
                assert_eq!(saved.reason.as_deref(), Some("retain reason"));
                let clear: FeedbackUpdate = serde_json::from_str(
                    r#"{"relevant":true,"pursuable":null,"outcome":null,"reason":null}"#,
                )
                .unwrap();
                let cleared = store
                    .update_feedback("op", "company", "ted", "one", &clear)
                    .unwrap();
                assert_eq!(
                    cleared,
                    FeedbackLabel {
                        relevant: true,
                        pursuable: None,
                        outcome: None,
                        reason: None
                    }
                );
            }
        }
    }

    #[test]
    fn audited_mutations_roll_back_when_audit_insert_fails() {
        let store = TenderStore::open(":memory:").unwrap();
        // Constraint injection: record_audit must fail after the mutation, not
        // before dispatch, proving the encompassing transaction rolls it back.
        store
            .connection
            .execute_batch(
                "DROP TABLE audit_log;
            CREATE TABLE audit_log (actor VARCHAR CHECK(actor <> 'reject'), action VARCHAR,
                enterprise_number VARCHAR, source VARCHAR, external_id VARCHAR, detail VARCHAR);",
            )
            .unwrap();
        let before = CompanyPreferences::default();
        store
            .save_preferences_audited("op", "company", &before)
            .unwrap();
        let after = CompanyPreferences {
            languages: vec!["nl".into()],
            ..before.clone()
        };
        assert!(store
            .save_preferences_audited("reject", "company", &after)
            .is_err());
        assert_eq!(store.preferences_for("company").unwrap(), before);
        let initial = FeedbackUpdate::replacement(true, Some(true), Some("won".into()), None);
        store
            .update_feedback("op", "company", "ted", "one", &initial)
            .unwrap();
        let rejected = FeedbackUpdate::replacement(false, None, None, None);
        assert!(store
            .update_feedback("reject", "company", "ted", "one", &rejected)
            .is_err());
        let labels = store.feedback_details_for("company").unwrap();
        let label = labels.get(&("ted".into(), "one".into())).unwrap();
        assert!(label.relevant);
        assert_eq!(label.pursuable, Some(true));
        assert_eq!(label.outcome.as_deref(), Some("won"));
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM audit_log", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn audit_log_records_writes_and_close_purges_confidential_tables() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-audit-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        assert!(store.audit_log(10).expect("read should work").is_empty());
        store
            .record_feedback("0123.456.789", "ted", "t1", true, None, None, None)
            .expect("feedback write should work");
        store
            .record_audit(
                "op",
                "feedback.save",
                Some("0123.456.789"),
                Some("ted"),
                Some("t1"),
                None,
            )
            .expect("audit write should work");
        assert!(store
            .record_audit("op", "tender.view", None, None, None, None)
            .is_err());
        store
            .save_preferences("0123.456.789", &CompanyPreferences::default())
            .expect("preference write should work");
        let counts = store.close_matter("op").expect("matter close should purge");
        assert_eq!(counts.feedback, 1);
        assert!(store
            .feedback_for("0123.456.789")
            .expect("read should work")
            .is_empty());
        assert_eq!(
            store
                .preferences_for("0123.456.789")
                .expect("read should work"),
            CompanyPreferences::default()
        );
        // The purge keeps one row: its own record.
        let events = store.audit_log(10).expect("read should work");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].action, "matter.close");
        assert_eq!(events[0].actor, "op");
        assert_eq!(events[1].action, "feedback.save");
        assert_eq!(events[1].enterprise_number.as_deref(), Some("0123.456.789"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_only_without_refresh_runs_reads_as_no_provenance() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aanbestedingen-no-provenance-{suffix}.duckdb"));
        {
            let connection = Connection::open(&path).expect("legacy database should open");
            connection
                .execute_batch("CREATE TABLE tenders (source VARCHAR, external_id VARCHAR);")
                .expect("legacy schema should create");
        }
        let store =
            TenderStore::open_read_only(&path).expect("legacy database should open read-only");
        assert!(store
            .latest_refresh_run()
            .expect("read should work")
            .is_none());
        assert!(store.audit_log(10).expect("read should work").is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_only_runs_predating_the_snapshot_read_without_evidence() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-old-runs-{suffix}.duckdb"));
        {
            let connection = Connection::open(&path).expect("legacy database should open");
            connection
                .execute_batch(
                    "CREATE SEQUENCE refresh_runs_id START 1;
                    CREATE TABLE refresh_runs (
                        id INTEGER PRIMARY KEY DEFAULT nextval('refresh_runs_id'),
                        started_at TIMESTAMPTZ DEFAULT current_timestamp,
                        kind VARCHAR, scope VARCHAR, window_desc VARCHAR,
                        fetched INTEGER, stored INTEGER,
                        total_notice_count INTEGER, duration_ms INTEGER
                    );
                    INSERT INTO refresh_runs
                        (kind, scope, window_desc, fetched, stored, total_notice_count, duration_ms)
                    VALUES ('refresh', 'ACTIVE', 'days=30', 1877, 1877, 19093, 25000);
                    -- An award run is newer, so a reader without the kind filter would
                    -- pick it as the tender store's last run on a legacy database too.
                    INSERT INTO refresh_runs
                        (kind, scope, window_desc, fetched, stored, total_notice_count, duration_ms)
                    VALUES ('awards', 'ACTIVE', 'awards 2026-11-01..2026-11-07', 7, 9, NULL, 5000);",
                )
                .expect("legacy runs schema should create");
        }
        let store =
            TenderStore::open_read_only(&path).expect("legacy database should open read-only");
        let latest = store
            .latest_refresh_run()
            .expect("read should work")
            .expect("the recorded run should load");
        assert_eq!(latest.scope, "ACTIVE");
        assert_eq!(
            latest.kind, "refresh",
            "the award run is not tender coverage"
        );
        assert_eq!(latest.total_notice_count, Some(19093));
        assert_eq!(latest.evidence, None);
        let awards = store
            .latest_award_run()
            .expect("read should work")
            .expect("the award run should load on a legacy database");
        assert_eq!(awards.kind, "awards");
        assert_eq!(awards.stored, 9);
        assert_eq!(
            awards.evidence, None,
            "legacy runs have no snapshot columns"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_only_reads_feedback_without_the_split_columns() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aanbestedingen-old-feedback-{suffix}.duckdb"));
        {
            let connection = Connection::open(&path).expect("legacy database should open");
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE feedback (
                        enterprise_number VARCHAR, source VARCHAR, external_id VARCHAR,
                        relevant BOOLEAN, reason VARCHAR,
                        created_at TIMESTAMPTZ DEFAULT current_timestamp,
                        PRIMARY KEY (enterprise_number, source, external_id)
                    );
                    INSERT INTO feedback VALUES ('0123.456.789', 'ted', 'old-1', true, NULL, now());
                    "#,
                )
                .expect("legacy feedback schema should create");
        }

        let store =
            TenderStore::open_read_only(&path).expect("legacy database should open read-only");
        let labels = store
            .feedback_details_for("0123.456.789")
            .expect("feedback should load");
        let label = labels
            .get(&("ted".to_owned(), "old-1".to_owned()))
            .expect("legacy label should load");
        assert!(label.relevant);
        assert_eq!(label.pursuable, None);
        assert_eq!(label.outcome, None);
        // Ranking falls back to the legacy relevant flag.
        assert_eq!(
            store
                .feedback_for("0123.456.789")
                .expect("feedback should load")
                .get(&("ted".to_owned(), "old-1".to_owned())),
            Some(&true)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrates_legacy_double_money_columns() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-legacy-{suffix}.duckdb"));
        {
            let connection = Connection::open(&path).expect("legacy database should open");
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE tenders (
                        source VARCHAR, external_id VARCHAR, title VARCHAR, buyer VARCHAR,
                        description VARCHAR, cpv_codes JSON, regions JSON,
                        estimated_value DOUBLE, deadline DATE, languages JSON,
                        requirements JSON, document_urls JSON, raw JSON
                    );
                    CREATE TABLE preferences (
                        enterprise_number VARCHAR, confirmed_activities JSON,
                        excluded_terms JSON, preferred_regions JSON, languages JSON,
                        minimum_value DOUBLE, maximum_value DOUBLE
                    );
                    INSERT INTO tenders VALUES (
                        'test', 'legacy-1', 'Legacy tender', NULL, '', '[]', '[]',
                        12345.67, NULL, '[]', '[]', '[]', '{}'
                    );
                    INSERT INTO preferences VALUES (
                        '0123.456.789', '[]', '[]', '[]', '[]', NULL, 98765.43
                    );
                    "#,
                )
                .expect("legacy schema should create");
        }

        let store = TenderStore::open(&path).expect("legacy database should migrate");
        // The notice-type column is added in place; legacy rows keep NULL and
        // therefore stay eligible as opportunities.
        let legacy = store
            .tender("test", "legacy-1")
            .expect("tender should load")
            .expect("legacy tender should survive");
        assert_eq!(legacy.notice_type, None);
        assert!(legacy.is_opportunity());
        // The legacy preferences table gains the recognition columns in place.
        // The fixture's legacy table has no unique key on the enterprise number,
        // so it is only read here; the save path is covered by the round-trip
        // test above.
        let legacy_preferences = store
            .preferences_for("0123.456.789")
            .expect("legacy preferences should load");
        assert_eq!(legacy_preferences.recognition_class, None);
        assert!(legacy_preferences.recognition_categories.is_empty());
        assert_eq!(
            store.tenders(10).expect("tenders should load")[0].estimated_value,
            Some(Decimal::new(1_234_567, 2))
        );
        assert_eq!(
            store
                .preferences_for("0123.456.789")
                .expect("preferences should load")
                .maximum_value,
            Some(Decimal::new(9_876_543, 2))
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn writes_to_schemas_with_required_timestamps() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-timestamps-{suffix}.duckdb"));
        let connection = Connection::open(&path).expect("database should open");
        connection
            .execute_batch(
                r#"
                CREATE TABLE tenders (
                    source VARCHAR NOT NULL, external_id VARCHAR NOT NULL, title VARCHAR NOT NULL,
                    buyer VARCHAR, description VARCHAR, cpv_codes JSON, regions JSON,
                    estimated_value DECIMAL(18, 2), deadline DATE, languages JSON,
                    requirements JSON, document_urls JSON, raw JSON,
                    fetched_at TIMESTAMPTZ NOT NULL, PRIMARY KEY (source, external_id)
                );
                CREATE TABLE awards (
                    enterprise_number VARCHAR NOT NULL, publication_number VARCHAR NOT NULL,
                    title VARCHAR, buyer VARCHAR, winner_identifiers JSON, winner_names JSON,
                    cpv_codes JSON, values JSON, links JSON, raw JSON,
                    fetched_at TIMESTAMPTZ NOT NULL,
                    PRIMARY KEY (enterprise_number, publication_number)
                );
                CREATE TABLE preferences (
                    enterprise_number VARCHAR PRIMARY KEY, confirmed_activities JSON,
                    excluded_terms JSON, preferred_regions JSON, languages JSON,
                    minimum_value DECIMAL(18, 2), maximum_value DECIMAL(18, 2),
                    updated_at TIMESTAMPTZ NOT NULL
                );
                CREATE TABLE feedback (
                    enterprise_number VARCHAR NOT NULL, source VARCHAR NOT NULL,
                    external_id VARCHAR NOT NULL, relevant BOOLEAN NOT NULL, reason VARCHAR,
                    created_at TIMESTAMPTZ NOT NULL,
                    PRIMARY KEY (enterprise_number, source, external_id)
                );
                "#,
            )
            .expect("timestamp-required schema should create");
        drop(connection);

        let store = TenderStore::open(&path).expect("timestamp-required store should open");
        let tender = notice_to_tender(&json!({
            "publication-number": "timestamp-1",
            "notice-title": "Timestamp compatibility"
        }));
        store.upsert(&tender).expect("tender write should work");
        store
            .save_preferences("0123.456.789", &CompanyPreferences::default())
            .expect("preference write should work");
        store
            .upsert_award("0123.456.789", &AwardRecord::default())
            .expect("award write should work");
        store
            .record_feedback("0123.456.789", "ted", "timestamp-1", true, None, None, None)
            .expect("feedback write should work");
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn audit_concurrent_cache_saves_respect_aggregate_budget() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cache-budget-{}-{nonce}.duckdb",
            std::process::id()
        ));
        drop(TenderStore::open(&path).unwrap());
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let store = TenderStore::open_initialized(path).unwrap();
                    let decision = AnalysisDecision {
                        provider: "test".into(),
                        activity_match: None,
                        geography_match: None,
                        previous_work_match: None,
                        qualification_risk: None,
                        relevance: None,
                        raw: std::collections::BTreeMap::from([(
                            "text".into(),
                            serde_json::json!("x".repeat(4096)),
                        )]),
                        prompt_version: None,
                        cached: false,
                    };
                    for index in 0..4 {
                        store
                            .save_analysis_with_budget(
                                &format!("{worker}-{index}"),
                                &decision,
                                8192,
                            )
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let store = TenderStore::open_read_only(&path).unwrap();
        let retained: i64 = store.connection.query_row("SELECT coalesce(sum(octet_length(encode(CAST(decision AS VARCHAR)))), 0)::BIGINT FROM analysis_cache", [], |row| row.get(0)).unwrap();
        assert!(retained > 0 && retained <= 8192);
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn audit_cache_byte_budget_uses_utf8_and_keeps_newest() {
        let store = TenderStore::open(":memory:").unwrap();
        let mut decision = AnalysisDecision {
            provider: "test".into(),
            activity_match: None,
            geography_match: None,
            previous_work_match: None,
            qualification_risk: None,
            relevance: None,
            raw: std::collections::BTreeMap::from([(
                "text".into(),
                serde_json::json!("é".repeat(512)),
            )]),
            prompt_version: None,
            cached: false,
        };
        let encoded = serde_json::to_string(&decision).unwrap();
        let bytes = encoded.len() as i64;
        assert!(encoded.len() > encoded.chars().count());
        for (key, age) in [("new", 0), ("middle", 1), ("old", 2)] {
            store.connection.execute(
                &format!("INSERT INTO analysis_cache VALUES (?, ?, CAST(now() AS TIMESTAMP) - INTERVAL '{age} minutes')"),
                params![key, encoded],
            ).unwrap();
        }
        store.prune_analysis_cache_with_budget(bytes * 2).unwrap();
        assert!(store.cached_analysis("new").unwrap().is_some());
        assert!(store.cached_analysis("middle").unwrap().is_some());
        assert!(store.cached_analysis("old").unwrap().is_none());
        let retained: i64 = store.connection.query_row("SELECT sum(octet_length(encode(CAST(decision AS VARCHAR))))::BIGINT FROM analysis_cache", [], |row| row.get(0)).unwrap();
        assert_eq!(retained, bytes * 2);
        decision.raw.insert(
            "text".into(),
            serde_json::json!("x".repeat(MAX_ANALYSIS_DECISION_BYTES)),
        );
        let large = serde_json::to_string(&decision).unwrap();
        assert!(store.save_analysis("too-large", &decision).is_err());
        // Rows written before these limits existed must not be restored into RAM.
        store
            .connection
            .execute(
                "INSERT INTO analysis_cache (cache_key, decision) VALUES (?, ?)",
                params!["legacy", large],
            )
            .unwrap();
        assert!(store.cached_analysis("legacy").unwrap().is_none());
        store.prune_analysis_cache().unwrap();
        assert_eq!(store.analysis_cache_len().unwrap(), 2);
        store.prune_analysis_cache_with_budget(0).unwrap();
        assert_eq!(store.analysis_cache_len().unwrap(), 0);
    }

    fn bda_record(id: &str, folder: &str, version: &str, title: &str) -> Tender {
        Tender {
            source: "bda".into(),
            external_id: id.into(),
            title: title.into(),
            raw: serde_json::json!({"contract_folder_id": folder, "notice_version": version}),
            ..Tender::default()
        }
    }

    #[test]
    fn bda_corrections_consolidate_legacy_rows_and_preserve_feedback() {
        let store = TenderStore::open(":memory:").unwrap();
        for row in [
            bda_record("PUB-1", "FOLDER", "01", "First"),
            bda_record("PUB-2", "FOLDER", "02", "Second"),
            bda_record("FOLDER", "FOLDER", "01", "Old canonical"),
        ] {
            store.upsert_tender_row(&row).unwrap(); // Simulate pre-fix rows.
        }
        let ted = Tender {
            source: "ted".into(),
            external_id: "PUB-1".into(),
            ..Tender::default()
        };
        store.upsert(&ted).unwrap();
        store
            .record_feedback("company-A", "bda", "PUB-1", true, None, None, None)
            .unwrap();
        store
            .record_feedback("company-A", "bda", "PUB-2", false, None, None, None)
            .unwrap();
        store.connection.execute("UPDATE feedback SET created_at = CAST(now() AS TIMESTAMP) - INTERVAL '1 day' WHERE external_id = 'PUB-1'", []).unwrap();
        store
            .record_feedback(
                "company-B",
                "bda",
                "FOLDER",
                true,
                None,
                None,
                Some("canonical preference"),
            )
            .unwrap();
        store
            .record_feedback("company-B", "bda", "PUB-2", false, None, None, None)
            .unwrap();
        store
            .upsert(&bda_record("FOLDER", "FOLDER", "03", "Latest"))
            .unwrap();
        assert_eq!(store.count().unwrap(), 2);
        assert!(store.tender("bda", "PUB-1").unwrap().is_none());
        assert!(store.tender("bda", "PUB-2").unwrap().is_none());
        assert!(store.tender("ted", "PUB-1").unwrap().is_some());
        for (company, relevant) in [("company-A", false), ("company-B", true)] {
            let feedback = store.feedback_for(company).unwrap();
            assert_eq!(feedback.len(), 1);
            assert_eq!(
                feedback.get(&("bda".into(), "FOLDER".into())),
                Some(&relevant)
            );
        }
        // Reimporting an older file later cannot undo the correction.
        store
            .upsert(&bda_record("FOLDER", "FOLDER", "02", "Stale"))
            .unwrap();
        assert_eq!(
            store.tender("bda", "FOLDER").unwrap().unwrap().title,
            "Latest"
        );
    }

    #[test]
    fn bda_identity_migration_rolls_back_on_invalid_input() {
        let store = TenderStore::open(":memory:").unwrap();
        store
            .upsert_tender_row(&bda_record("PUB-1", "FOLDER", "01", "Original"))
            .unwrap();
        store
            .record_feedback("company", "bda", "PUB-1", true, None, None, None)
            .unwrap();
        let mut invalid = bda_record("FOLDER", "FOLDER", "02", "Invalid");
        invalid.estimated_value = Some(Decimal::MAX);
        assert!(store.upsert(&invalid).is_err());
        assert!(store.tender("bda", "PUB-1").unwrap().is_some());
        assert!(store.tender("bda", "FOLDER").unwrap().is_none());
        assert!(store
            .feedback_for("company")
            .unwrap()
            .contains_key(&("bda".into(), "PUB-1".into())));
        let valid = bda_record("FOLDER", "FOLDER", "02", "Valid correction");
        let invalid_ted = Tender {
            source: "ted".into(),
            external_id: "bad".into(),
            estimated_value: Some(Decimal::MAX),
            ..Tender::default()
        };
        // The BDA migration completes first; a later batch error must restore
        // both its deleted aliases and their moved feedback.
        assert!(store.upsert_many([&valid, &invalid_ted]).is_err());
        assert!(store.tender("bda", "PUB-1").unwrap().is_some());
        assert!(store.tender("bda", "FOLDER").unwrap().is_none());
        assert!(store
            .feedback_for("company")
            .unwrap()
            .contains_key(&("bda".into(), "PUB-1".into())));
    }

    #[test]
    fn bda_filename_migration_uses_exact_source_name() {
        let store = TenderStore::open(":memory:").unwrap();
        let legacy = Tender {
            source: "bda".into(),
            external_id: "dir::notice".into(),
            raw: serde_json::json!({"source_name": "dir/notice.xml"}),
            ..Tender::default()
        };
        store.upsert_tender_row(&legacy).unwrap();
        store
            .record_feedback("company", "bda", "dir::notice", true, None, None, None)
            .unwrap();
        let xml =
            crate::bda::parse_xml(b"<Notice><Name>XML</Name></Notice>", "dir/notice.xml").unwrap();
        let pdf =
            crate::bda::parse_xml(b"<Notice><Name>PDF</Name></Notice>", "dir/notice.pdf").unwrap();
        store.upsert_many([&pdf, &xml]).unwrap();
        assert_eq!(store.count().unwrap(), 2);
        assert!(store.tender("bda", "dir::notice").unwrap().is_none());
        assert!(store.tender("bda", &xml.external_id).unwrap().is_some());
        assert!(store.tender("bda", &pdf.external_id).unwrap().is_some());
        store.upsert(&xml).unwrap(); // Reimport is idempotent under its new ID.
        assert_eq!(store.count().unwrap(), 2);
        let feedback = store.feedback_for("company").unwrap();
        assert!(feedback.contains_key(&("bda".into(), xml.external_id)));
        assert!(!feedback.contains_key(&("bda".into(), pdf.external_id)));
    }

    #[test]
    fn bda_canonical_identity_collision_fails_without_overwriting() {
        let store = TenderStore::open(":memory:").unwrap();
        store
            .upsert_tender_row(&bda_record("FOLDER", "UNRELATED", "01", "Keep me"))
            .unwrap();
        assert!(store
            .upsert(&bda_record("FOLDER", "FOLDER", "02", "Collision"))
            .is_err());
        assert_eq!(
            store.tender("bda", "FOLDER").unwrap().unwrap().title,
            "Keep me"
        );
        let fallback =
            crate::bda::parse_xml(b"<Notice><Name>Fallback</Name></Notice>", "notice.xml").unwrap();
        let explicit = Tender {
            source: "bda".into(),
            external_id: fallback.external_id.clone(),
            title: "Explicit notice".into(),
            raw: serde_json::json!({"source_name": "other.xml", "notice_identifier": fallback.external_id}),
            ..Tender::default()
        };
        store.upsert_tender_row(&explicit).unwrap();
        assert!(store.upsert(&fallback).is_err());
        assert_eq!(
            store
                .tender("bda", &fallback.external_id)
                .unwrap()
                .unwrap()
                .title,
            "Explicit notice"
        );
    }

    #[test]
    fn persists_analysis_decisions() {
        let path = std::env::temp_dir().join(format!(
            "aanbestedingen-analysis-cache-{}.duckdb",
            std::process::id()
        ));
        let store = TenderStore::open(&path).expect("database should open");
        let decision = AnalysisDecision {
            provider: "deterministic-rust".to_owned(),
            activity_match: Some(1.0),
            geography_match: Some(0.5),
            previous_work_match: Some(0.0),
            qualification_risk: Some("none_obvious".to_owned()),
            relevance: Some("possible".to_owned()),
            raw: std::collections::BTreeMap::new(),
            prompt_version: None,
            cached: false,
        };
        assert!(store
            .cached_analysis("missing")
            .expect("missing cache lookup should work")
            .is_none());
        store
            .save_analysis("old", &decision)
            .expect("old analysis should save");
        store
            .connection
            .execute(
                "UPDATE analysis_cache SET created_at = CAST(now() AS TIMESTAMP) - INTERVAL '31 days' WHERE cache_key = 'old'",
                [],
            )
            .expect("old analysis should age");
        store
            .save_analysis("key", &decision)
            .expect("analysis should save");
        assert!(store
            .cached_analysis("old")
            .expect("old cache lookup should work")
            .is_none());
        assert_eq!(
            store
                .cached_analysis("key")
                .expect("analysis should load")
                .expect("saved analysis should exist")
                .provider,
            "deterministic-rust"
        );
        assert_eq!(
            store
                .analysis_cache_count()
                .expect("cache count should load"),
            1
        );
        store
            .connection
            .execute(
                "UPDATE analysis_cache SET created_at = CAST(now() AS TIMESTAMP) - INTERVAL '31 days' WHERE cache_key = 'key'",
                [],
            )
            .expect("saved analysis should age");
        assert!(store
            .cached_analysis("key")
            .expect("expired cache lookup should work")
            .is_none());
        assert_eq!(
            store
                .analysis_cache_count()
                .expect("cache count should load"),
            0
        );
        store
            .save_analysis("clear-me", &decision)
            .expect("analysis should save before clearing");
        assert_eq!(
            store
                .clear_analysis_cache()
                .expect("analysis cache should clear"),
            1
        );
        assert_eq!(
            store
                .analysis_cache_count()
                .expect("cache count should load"),
            0
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn failed_transaction_rolls_back_tender_writes() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-rollback-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let tender = notice_to_tender(&json!({
            "publication-number": "rollback-1",
            "notice-title": "Rolled back tender"
        }));
        let result = store.with_transaction(|| -> Result<(), Box<dyn Error>> {
            store.upsert(&tender)?;
            Err("forced transaction failure".into())
        });
        assert!(result.is_err());
        assert_eq!(store.count().expect("count should work"), 0);
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn bulk_upsert_commits_all_tenders_as_one_batch() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-bulk-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let first = notice_to_tender(&json!({
            "publication-number": "bulk-1",
            "notice-title": "First tender"
        }));
        let second = notice_to_tender(&json!({
            "publication-number": "bulk-2",
            "notice-title": "Second tender"
        }));
        assert_eq!(
            store
                .upsert_many([&first, &second])
                .expect("bulk upsert should work"),
            2
        );
        assert_eq!(store.count().expect("count should work"), 2);
        drop(store);
        let _ = fs::remove_file(path);
    }

    /// Award results and other non-competition notices must not become ranking
    /// candidates, while rows written before the type was requested stay
    /// eligible instead of silently disappearing.
    #[test]
    fn open_tenders_exclude_award_results_but_keep_untyped_rows() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-opportunity-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let competition = notice_to_tender(&json!({
            "publication-number": "cn-1",
            "notice-title": "Call for competition",
            "notice-type": "cn-standard",
            "deadline-receipt-tender-date-lot": "2026-11-01"
        }));
        let award = notice_to_tender(&json!({
            "publication-number": "can-1",
            "notice-title": "Award result",
            "notice-type": "can-standard"
        }));
        let pin = notice_to_tender(&json!({
            "publication-number": "pin-1",
            "notice-title": "Prior information notice",
            "notice-type": "pin-only"
        }));
        // A prior information notice used as the call for competition is a real
        // opportunity in the accelerated procedure.
        let pin_cfc = notice_to_tender(&json!({
            "publication-number": "pin-cfc-1",
            "notice-title": "Prior information notice as a call for competition",
            "notice-type": "pin-cfc-standard"
        }));
        let untyped = notice_to_tender(&json!({
            "publication-number": "untyped-1",
            "notice-title": "Stored before the type was requested"
        }));
        store
            .upsert_many([&competition, &award, &pin, &pin_cfc, &untyped])
            .expect("tenders should store");

        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 18).expect("date should be valid");
        let open = store
            .open_tenders(10, today, None)
            .expect("open tenders should load");
        let identifiers = open
            .iter()
            .map(|tender| tender.external_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(identifiers, vec!["cn-1", "pin-cfc-1", "untyped-1"]);
        // Every stored notice is still readable and counted; only the ranking
        // candidate list is filtered.
        assert_eq!(store.tenders(10).expect("tenders should load").len(), 5);
        assert_eq!(
            store
                .tender("ted", "can-1")
                .expect("tender should load")
                .expect("award notice should exist")
                .notice_type
                .as_deref(),
            Some("can-standard")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The historical ladder must not contain a notice published after the
    /// reference date, or a closed notice would be ranked against notices that
    /// did not exist when it was biddable.
    /// The health counts must match what the ladder actually offers: open
    /// notices, and the smaller number of distinct procurements after collapsing
    /// re-publications.
    /// Award history coverage is per company on purpose: a store can hold many
    /// awards for one company and none for everybody else, which leaves the
    /// history term neutral for those others.
    /// Inventory files write enterprise numbers without dots (`0214596464`) while
    /// the product normalizes them with dots (`0214.596.464`). An exact string
    /// comparison silently returned nothing — measured 2026-10-19, a company with
    /// 87 accounting rows read as having no records at all.
    #[test]
    fn inventory_lookups_ignore_the_dotted_enterprise_spelling() {
        use crate::bxl_inventory::InventoryAward;
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-dots-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let row = |hash: &str, amount: i64| InventoryAward {
            buyer_identifier: Some("0207365709".to_owned()),
            buyer_name: Some("SCR".to_owned()),
            awardee_identifier: Some("0214596464".to_owned()),
            awardee_name: Some("BPOST".to_owned()),
            engagement_id: Some("2024002611".to_owned()),
            engagement_date: Some("2024-12-31".to_owned()),
            engagement_amount: Some(Decimal::new(amount, 2)),
            object: Some("Justice fees".to_owned()),
            row_hash: hash.to_owned(),
        };
        store
            .upsert_inventory_awards("scrcocom2024.xlsx", &[row("h1", 12_983), row("h2", 26_496)])
            .expect("rows should import");

        for spelling in ["0214596464", "0214.596.464"] {
            let engagements = store
                .inventory_engagements_for_awardee(spelling, 10)
                .expect("lookup should succeed");
            assert_eq!(engagements.len(), 2, "spelling {spelling}");
            let awards = store
                .inventory_awards_for_awardee(spelling, 10)
                .expect("lookup should succeed");
            assert_eq!(awards.len(), 2, "spelling {spelling}");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The health counters and the ladder must agree on how many procurements are
    /// open: two numbers that describe the same thing and differ is the kind of
    /// inconsistency that erodes trust in both.
    #[test]
    fn health_counts_agree_with_the_ladder() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-agree-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let version = |external_id: &str, deadline: &str, title: &str| {
            notice_to_tender(&json!({
                "publication-number": external_id,
                "notice-title": title,
                "buyer-name": {"nld": ["Stad Gent"]},
                "notice-type": "cn-standard",
                "classification-cpv": ["45210000"],
                "deadline-receipt-tender-date-lot": deadline
            }))
        };
        let first = version("1-2026", "2027-01-01", "School renovation");
        // A corrigendum: same procurement, later deadline, title in another case
        // and with different punctuation.
        let corrigendum = version("2-2026", "2027-02-01", "school  RENOVATION");
        let other = version("3-2026", "2027-01-15", "Bridge works");
        store
            .upsert_many([&first, &corrigendum, &other])
            .expect("tenders should store");

        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).expect("valid date");
        let (notices, procurements) = store.open_opportunity_counts(today).expect("counts");
        let ladder = store.open_tenders(1_000, today, None).expect("ladder");
        assert_eq!(notices, 3);
        assert_eq!(
            procurements, 2,
            "case and punctuation still mean one procurement"
        );
        assert_eq!(
            procurements as usize,
            ladder.len(),
            "the health counter and the ladder must agree"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn award_history_coverage_counts_companies_not_only_awards() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-history-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        for (enterprise, publication) in [
            ("0123.456.789", "1-2026"),
            ("0123.456.789", "2-2026"),
            ("0987.654.321", "3-2026"),
        ] {
            store
                .upsert_award(
                    enterprise,
                    &AwardRecord {
                        publication_number: publication.to_owned(),
                        title: "Award".to_owned(),
                        ..AwardRecord::default()
                    },
                )
                .expect("award should store");
        }
        let (awards, companies) = store
            .award_history_coverage()
            .expect("coverage should load");
        assert_eq!((awards, companies), (3, 2));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bosa_history_refresh_replaces_only_namespaced_bosa_records() {
        let store = TenderStore::open(":memory:").expect("database should open");
        store
            .upsert_award(
                "0123.456.789",
                &AwardRecord {
                    publication_number: "707653-2026".into(),
                    cpv_codes: vec!["45210000".into()],
                    ..AwardRecord::default()
                },
            )
            .expect("TED history should store");
        let publication = "bosa:workspace:notice".to_owned();
        let old = AwardRecord {
            publication_number: publication.clone(),
            cpv_codes: vec!["45000000".into()],
            raw: serde_json::json!({"source":"bosa"}),
            ..AwardRecord::default()
        };
        assert_eq!(
            store
                .replace_bosa_award_history(&[publication.clone()], &[("0123.456.789".into(), old)])
                .expect("first BOSA import should succeed"),
            1
        );

        let corrected = AwardRecord {
            publication_number: publication.clone(),
            cpv_codes: vec!["45210000".into()],
            values: Vec::new(),
            raw: serde_json::json!({"source":"bosa", "amounts_used_for_ranking":false}),
            ..AwardRecord::default()
        };
        store
            .replace_bosa_award_history(
                &[publication.clone()],
                &[("0987.654.321".into(), corrected)],
            )
            .expect("corrected BOSA import should replace old participation");
        assert!(store
            .awards_for("0123.456.789")
            .unwrap()
            .iter()
            .all(|award| { award.publication_number != publication }));
        assert!(store
            .awards_for("0987.654.321")
            .unwrap()
            .iter()
            .any(|award| award.publication_number == publication));
        assert_eq!(store.awards_for("0123.456.789").unwrap().len(), 1);

        // An empty current notice removes only its previous BOSA record.
        store
            .replace_bosa_award_history(&[publication.clone()], &[])
            .expect("empty BOSA replacement should clear stale participation");
        assert!(store
            .awards_for("0987.654.321")
            .unwrap()
            .iter()
            .all(|award| award.publication_number != publication));
        assert_eq!(store.awards_for("0123.456.789").unwrap().len(), 1);
        assert!(store
            .replace_bosa_award_history(&["ted:wrong-namespace".into()], &[])
            .is_err());
    }

    #[test]
    fn open_counts_separate_notices_from_procurements() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-counts-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let open = |external_id: &str, deadline: &str, buyer: &str| {
            notice_to_tender(&json!({
                "publication-number": external_id,
                "notice-title": "Same procurement republished",
                "buyer-name": {"nld": [buyer]},
                "notice-type": "cn-standard",
                "classification-cpv": ["45210000"],
                "deadline-receipt-tender-date-lot": deadline
            }))
        };
        let original = open("1-2026", "2026-12-01", "Stad Gent");
        let extended = open("2-2026", "2026-12-20", "Stad Gent");
        let other = notice_to_tender(&json!({
            "publication-number": "3-2026",
            "notice-title": "A different procurement",
            "buyer-name": {"nld": ["Stad Gent"]},
            "notice-type": "cn-standard",
            "classification-cpv": ["90511000"],
            "deadline-receipt-tender-date-lot": "2026-12-01"
        }));
        let award = notice_to_tender(&json!({
            "publication-number": "4-2026",
            "notice-title": "An award result",
            "buyer-name": {"nld": ["Stad Gent"]},
            "notice-type": "can-standard"
        }));
        let expired = open("5-2026", "2020-01-01", "Stad Gent");
        store
            .upsert_many([&original, &extended, &other, &award, &expired])
            .expect("tenders should store");

        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).expect("date should be valid");
        let (notices, procurements) = store
            .open_opportunity_counts(today)
            .expect("counts should load");
        assert_eq!(notices, 3, "award results and expired notices are excluded");
        assert_eq!(procurements, 2, "the republication collapses into one");
        let _ = std::fs::remove_file(&path);
    }

    /// The single-row read must map the publication date the way the list reads
    /// do. It did not, so on any store that recorded one the tender-detail and
    /// analysis endpoints answered `Invalid column type Date32` — a 500 users see
    /// on every fresh deployment, invisible to fixtures whose dates are null.
    #[test]
    fn single_tender_reads_a_populated_publication_date() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-single-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let tender = notice_to_tender(&json!({
            "publication-number": "1-2026",
            "notice-title": "Elektrische installatie",
            "notice-type": "cn-standard",
            "publication-date": "2026-10-19",
            "deadline-receipt-tender-date-lot": "2026-11-20"
        }));
        store.upsert(&tender).expect("tender should store");
        let loaded = store
            .tender("ted", "1-2026")
            .expect("single read should succeed")
            .expect("tender should exist");
        assert_eq!(loaded.publication_date.as_deref(), Some("2026-10-19"));
        assert_eq!(loaded.deadline.as_deref(), Some("2026-11-20"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn source_workspace_scope_survives_compact_reads_and_ranking() {
        for (other_source, other_workspace) in [("bda", "workspace-B"), ("bda-int", "workspace-A")]
        {
            for reverse in [false, true] {
                let store = TenderStore::open(":memory:").unwrap();
                let make = |source: &str, workspace: &str| Tender {
                    source: source.into(),
                    external_id: "shared-folder".into(),
                    title: "Same title".into(),
                    buyer: Some("Same buyer".into()),
                    notice_type: Some("cn-general".into()),
                    deadline: Some("2026-12-01".into()),
                    publication_date: Some("2026-10-30".into()),
                    raw: serde_json::json!({"contract_folder_id":"shared-folder",
                    "bosa_publication_workspace_id":workspace, "bosa_notice_id":format!("notice-{workspace}"),
                    "bosa_activation_date":"2026-10-30", "bosa_version_created_at":"2026-10-30T08:00:00"}),
                    ..Tender::default()
                };
                let a = make("bda", "workspace-A");
                let b = make(other_source, other_workspace);
                store
                    .upsert_many(if reverse { [&b, &a] } else { [&a, &b] })
                    .unwrap();
                assert_eq!(store.count().unwrap(), 2);
                let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
                let company: crate::CompanyProfile = serde_json::from_value(serde_json::json!({
                    "enterprise_number":"0404.484.060", "status":null
                }))
                .unwrap();
                for rows in [
                    store.tenders(10).unwrap(),
                    store.open_tenders(10, today, None).unwrap(),
                    store.open_tenders_as_of(10, today).unwrap(),
                ] {
                    assert_eq!(rows.len(), 2);
                    let scopes: std::collections::BTreeSet<_> = rows
                        .iter()
                        .map(|t| {
                            (
                                t.source.as_str(),
                                t.raw["bosa_publication_workspace_id"].as_str().unwrap(),
                            )
                        })
                        .collect();
                    assert_eq!(
                        scopes,
                        std::collections::BTreeSet::from([
                            ("bda", "workspace-A"),
                            (other_source, other_workspace)
                        ])
                    );
                    let ranked = crate::recommendations::rank_on(
                        &company,
                        &rows,
                        &[],
                        &CompanyPreferences::default(),
                        &HashMap::new(),
                        10,
                        today,
                    );
                    assert_eq!(
                        ranked.len(),
                        2,
                        "ranking must not undo source-scope preservation"
                    );
                    assert!(ranked.iter().all(|item| item.dossier_members == 1));
                }
                assert_eq!(store.open_opportunity_counts(today).unwrap().1, 2);
            }
        }
    }

    #[test]
    fn malformed_source_scope_is_not_coerced_into_authority() {
        let store = TenderStore::open(":memory:").unwrap();
        let invalid = Tender {
            source: "test".into(),
            external_id: "invalid".into(),
            title: "Same".into(),
            buyer: Some("Buyer".into()),
            raw: serde_json::json!({"contract_folder_id":42,"bosa_publication_workspace_id":["not-an-id"]}),
            ..Tender::default()
        };
        let unscoped = Tender {
            external_id: "unscoped".into(),
            raw: serde_json::Value::Null,
            ..invalid.clone()
        };
        store.upsert_many([&invalid, &unscoped]).unwrap();
        for row in store.tenders(10).unwrap() {
            assert_eq!(row.version_key(), unscoped.version_key());
        }
        assert_eq!(
            store
                .operative_notice_headers(None, MAX_NOTICE_IDENTITY_BYTES)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn source_distinct_folders_survive_version_selection() {
        let fixture = include_str!("../../tests/fixtures/bda/notice.xml");
        for second_expired in [false, true] {
            for reverse in [false, true] {
                let store = TenderStore::open(":memory:").unwrap();
                let first = crate::bda::parse_xml(fixture.as_bytes(), "first.xml").unwrap();
                let xml = fixture
                    .replace("FIXTURE-0001", "FIXTURE-0002")
                    .replace("fixture-folder-0001", "fixture-folder-0002")
                    .replace("2026-01-05", "2026-02-05");
                let xml = if second_expired {
                    xml.replace("2027-06-30", "2026-11-01")
                } else {
                    xml
                };
                let second = crate::bda::parse_xml(xml.as_bytes(), "second.xml").unwrap();
                assert_eq!(first.title, second.title);
                assert_eq!(first.buyer, second.buyer);
                assert_eq!(first.cpv_codes, second.cpv_codes);
                let rows = if reverse {
                    [&second, &first]
                } else {
                    [&first, &second]
                };
                store.upsert_many(rows).unwrap();
                assert_eq!(
                    store.count().unwrap(),
                    2,
                    "the canonical store retains both source folders"
                );
                let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
                let expected = if second_expired { 1 } else { 2 };
                for historical in [false, true] {
                    let open = if historical {
                        store.open_tenders_as_of(10, today)
                    } else {
                        store.open_tenders(10, today, None)
                    }
                    .unwrap();
                    assert_eq!(open.len(), expected,
                        "separate folder hidden: second_expired={second_expired}, reverse={reverse}, historical={historical}");
                    assert!(open.iter().any(|t| t.external_id == "fixture-folder-0001"));
                    let grouped = crate::collapse_dossier_members(open);
                    assert_eq!(grouped.len(), expected);
                }
                assert_eq!(
                    store.open_opportunity_counts(today).unwrap().1,
                    expected as i64
                );
            }
        }
    }

    #[test]
    fn candidate_identity_budget_fails_closed() {
        let store = TenderStore::open(":memory:").unwrap();
        store
            .upsert(&Tender {
                source: "ted".into(),
                external_id: "one".into(),
                ..Tender::default()
            })
            .unwrap();
        let error = store.operative_notice_headers(None, 1).unwrap_err();
        assert!(error.to_string().contains("identity budget exceeded"));
    }

    #[test]
    fn candidate_limits_follow_complete_version_selection() {
        let store = TenderStore::open(":memory:").unwrap();
        let version = |id: &str, title: &str, deadline: Option<&str>, published: &str| Tender {
            source: "ted".into(),
            external_id: id.into(),
            title: title.into(),
            buyer: Some("Synthetic buyer".into()),
            deadline: deadline.map(str::to_owned),
            publication_date: Some(published.into()),
            notice_type: Some("cn-standard".into()),
            ..Tender::default()
        };
        // The later version is outside the old raw LIMIT window, and expires
        // before today. Neither the original nor the correction may be offered.
        let rows = [
            version("A1", "A", Some("2026-11-10"), "2026-10-01"),
            version("A2", "A", Some("2026-11-11"), "2026-10-02"),
            version("B", "B", Some("2026-11-12"), "2026-10-03"),
            version("C1", "C", Some("2026-11-30"), "2026-10-01"),
            version("C2", "C", Some("2026-11-05"), "2026-11-01"),
            version("D1", "D", Some("2026-11-15"), "2026-10-01"),
            version("D2", "D", None, "2026-11-01"),
        ];
        store.upsert_many(rows.iter()).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let expected = ["A2", "B", "D2"];
        for limit in 0..=5 {
            for historical in [false, true] {
                let actual = if historical {
                    store.open_tenders_as_of(limit, today)
                } else {
                    store.open_tenders(limit, today, None)
                }
                .unwrap();
                assert_eq!(
                    actual
                        .iter()
                        .map(|t| t.external_id.as_str())
                        .collect::<Vec<_>>(),
                    expected.iter().copied().take(limit).collect::<Vec<_>>(),
                    "limit={limit}, historical={historical}"
                );
            }
        }
        let due = store.open_tenders(10, today, Some(30)).unwrap();
        assert_eq!(
            due.iter()
                .map(|t| t.external_id.as_str())
                .collect::<Vec<_>>(),
            ["A2", "B"]
        );
        assert_eq!(store.open_opportunity_counts(today).unwrap().1, 3);
        // Historical selection must not allow the future correction to suppress
        // the version that was known at the reference date.
        let then = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let old = store.open_tenders_as_of(10, then).unwrap();
        assert!(old.iter().any(|t| t.external_id == "C1"));
        assert!(old.iter().any(|t| t.external_id == "D1"));
    }

    /// A corrigendum that moves a deadline out of the requested window must make
    /// the procurement disappear from it, not reappear with the superseded date.
    /// Measured 2026-10-19: a call corrected from 27 October to 26 November was
    /// listed in a 30-day window (19 Oct + 30 = 18 Nov) at the *old* deadline,
    /// because the window filtered before the versions were collapsed.
    #[test]
    fn due_window_follows_the_operative_deadline_not_a_superseded_one() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-window-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let version = |external_id: &str, deadline: &str, published: &str| {
            notice_to_tender(&json!({
                "publication-number": external_id,
                "notice-title": "Sewage treatment plant",
                "buyer-name": {"nld": ["AIDE"]},
                "notice-type": "cn-standard",
                "classification-cpv": ["45252100"],
                "publication-date": published,
                "deadline-receipt-tender-date-lot": deadline
            }))
        };
        store
            .upsert_many([
                &version("old-1", "2026-10-27", "2026-09-01"),
                &version("new-2", "2026-11-26", "2026-10-05"),
            ])
            .expect("tenders should store");

        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 19).expect("valid date");
        // 19 Oct + 30 days = 18 Nov: the operative deadline (26 Nov) is outside.
        let within_30 = store
            .open_tenders(10, today, Some(30))
            .expect("window should load");
        assert!(
            within_30.is_empty(),
            "the procurement is due in 38 days, so a 30-day window must not list it \
             (got {:?})",
            within_30
                .iter()
                .map(|tender| (tender.external_id.clone(), tender.deadline.clone()))
                .collect::<Vec<_>>()
        );
        // 19 Oct + 60 days includes it, with the operative deadline.
        let within_60 = store
            .open_tenders(10, today, Some(60))
            .expect("window should load");
        assert_eq!(within_60.len(), 1);
        assert_eq!(within_60[0].external_id, "new-2");
        assert_eq!(within_60[0].deadline.as_deref(), Some("2026-11-26"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_tenders_as_of_excludes_later_and_expired_notices() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-as-of-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let open_then = notice_to_tender(&json!({
            "publication-number": "open-then",
            "notice-title": "Open on the reference date",
            "notice-type": "cn-standard",
            "publication-date": "2024-06-01",
            "deadline-receipt-tender-date-lot": "2024-09-09"
        }));
        let published_later = notice_to_tender(&json!({
            "publication-number": "later",
            "notice-title": "Published after the reference date",
            "notice-type": "cn-standard",
            "publication-date": "2024-12-01",
            "deadline-receipt-tender-date-lot": "2025-02-01"
        }));
        let already_expired = notice_to_tender(&json!({
            "publication-number": "expired",
            "notice-title": "Deadline before the reference date",
            "notice-type": "cn-standard",
            "publication-date": "2024-01-01",
            "deadline-receipt-tender-date-lot": "2024-03-01"
        }));
        let award = notice_to_tender(&json!({
            "publication-number": "award",
            "notice-title": "Award result",
            "notice-type": "can-standard",
            "publication-date": "2024-06-01"
        }));
        store
            .upsert_many([&open_then, &published_later, &already_expired, &award])
            .expect("tenders should store");

        let as_of = chrono::NaiveDate::from_ymd_opt(2024, 9, 9).expect("date should be valid");
        let ladder = store
            .open_tenders_as_of(10, as_of)
            .expect("historical ladder should load");
        let identifiers = ladder
            .iter()
            .map(|tender| tender.external_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(identifiers, vec!["open-then"]);
        // The publication date is stored as a DATE but every list query reads it
        // as text, the way `deadline` is read. A fresh database made that mapping
        // fail outright, so the round trip is asserted here.
        assert_eq!(
            ladder[0].publication_date.as_deref(),
            Some("2024-06-01"),
            "publication date should survive the read as text"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_tender_rows_project_framework_amounts_without_full_raw_payload() {
        let store = TenderStore::open(":memory:").expect("database should open");
        let tender = Tender {
            source: "bda".into(),
            external_id: "framework-ceiling".into(),
            title: "Framework notice".into(),
            deadline: Some("2026-11-30".into()),
            notice_type: Some("cn-general".into()),
            raw: serde_json::json!({
                "framework_maximum_amounts": ["350000.00", "300000.00"],
                "framework_maximum_evidence": [
                    {"amount": "900000.00", "currency_id": "EUR", "scope": "notice", "lot_id": null},
                    {"amount": "350000.00", "currency_id": "EUR", "scope": "lot", "lot_id": "LOT-1"}
                ],
                "framework_maximum_evidence_truncated": false,
                "notice_lot_ids_unique": true,
                "notice_lot_cpv_evidence": [{"lot_id": "LOT-1", "cpv_codes": ["33600000"]}],
                "notice_lot_place_evidence": [
                    {"lot_id": "LOT-1", "country_subentity_codes": ["BE231"]},
                    {"lot_id": "LOT-2", "country_subentity_codes": []}
                ],
                "notice_lot_place_evidence_truncated": false,
                "notice_lot_award_criteria_evidence": [
                    {"lot_id": "LOT-1", "criteria": [{"source_element": "SubordinateAwardingCriterion", "source_ordinal_path": [0, 0], "type_code": "QUALITY", "description": "Service quality", "calculation_expression": null, "parameters": [{"code": "WEIGHT", "numeric": "35"}]}]},
                    {"lot_id": "LOT-2", "criteria": []}
                ],
                "notice_lot_award_criteria_evidence_truncated": false,
                "notice_lot_selection_evidence": [
                    {"lot_id": "LOT-1", "selection_evidence": [{"source_element": "SelectionCriteria", "source_path": "TenderingTerms/UBLExtensions/UBLExtension/ExtensionContent/EformsExtension/SelectionCriteria", "source_ordinal_path": [0, 0], "code": "FINANCIAL_STANDING", "code_list_name": "eForms selection", "descriptions": ["Turnover"]}]},
                    {"lot_id": "LOT-2", "selection_evidence": []}
                ],
                "notice_lot_selection_evidence_truncated": false,
                "notice_lot_contract_execution_evidence": [
                    {"lot_id": "LOT-1", "conditions": [{"source_element": "ContractExecutionRequirement", "source_path": "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode", "source_ordinal_path": [0], "code": "SAFE_HANDLING", "code_list_name": "Execution condition types"}]},
                    {"lot_id": "LOT-2", "conditions": []}
                ],
                "notice_lot_contract_execution_evidence_truncated": false,
                "notice_lot_subsequent_process_evidence": [
                    {"lot_id": "LOT-1", "requirements": [{"source_element": "SubsequentProcessTenderRequirement", "source_path": "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name", "source_ordinal_path": [0], "name": "Second-stage capability"}]},
                    {"lot_id": "LOT-2", "requirements": []}
                ],
                "notice_lot_subsequent_process_evidence_truncated": false,
                "notice_lot_guarantee_curricula_evidence": [
                    {"lot_id": "LOT-1", "guarantees": [{"source_element": "RequiredFinancialGuarantee", "source_path": "TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode", "source_ordinal_path": [0], "code": "PERFORMANCE_BOND", "code_list_name": "guarantee types", "description": "Guarantee description"}], "curricula": [{"source_element": "RequiredCurriculaCode", "source_path": "TenderingTerms/RequiredCurriculaCode", "source_ordinal_path": [0], "code": "CV_TEAM", "code_list_name": "curriculum codes"}]},
                    {"lot_id": "LOT-2", "guarantees": [], "curricula": []}
                ],
                "notice_lot_guarantee_curricula_evidence_truncated": false,
                "notice_lot_ted_evidence": {
                    "association_rule": "singleton_field_cardinality_only_no_positional_join",
                    "fields": {
                        "lot_id": {"source_cardinality": 1, "singleton_value": "LOT-1", "status": "singleton"},
                        "main_classification": {"source_cardinality": 1, "singleton_value": "33600000", "status": "singleton"},
                        "estimated_value": {"source_cardinality": 1, "singleton_value": "350000.00", "status": "singleton"},
                        "estimated_currency": {"source_cardinality": 1, "singleton_value": "EUR", "status": "singleton"},
                        "framework_maximum": {"source_cardinality": 0, "singleton_value": null, "status": "not_recorded"},
                        "framework_currency": {"source_cardinality": 0, "singleton_value": null, "status": "not_recorded"}
                    }
                },
                "bosa_attachment_evidence": [{"text": "large extracted payload"}],
                "unrelated": "must stay out of ranking rows"
            }),
            ..Tender::default()
        };
        store.upsert(&tender).expect("tender should store");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 2).expect("valid date");
        let open = store
            .open_tenders(10, today, None)
            .expect("candidate row should load");
        assert_eq!(open.len(), 1);
        assert_eq!(
            open[0].raw["framework_maximum_amounts"],
            serde_json::json!(["350000.00", "300000.00"])
        );
        assert_eq!(open[0].raw["notice_lot_ids_unique"], true);
        assert_eq!(
            open[0].raw["framework_maximum_evidence"],
            serde_json::json!([
                {"amount": "900000.00", "currency_id": "EUR", "scope": "notice", "lot_id": null},
                {"amount": "350000.00", "currency_id": "EUR", "scope": "lot", "lot_id": "LOT-1"}
            ])
        );
        assert_eq!(
            open[0].raw["notice_lot_cpv_evidence"],
            serde_json::json!([{"lot_id": "LOT-1", "cpv_codes": ["33600000"]}])
        );
        assert_eq!(
            open[0].raw["notice_lot_ted_evidence"]["fields"]["lot_id"]["singleton_value"],
            "LOT-1"
        );
        assert_eq!(
            open[0].raw["notice_lot_place_evidence"],
            serde_json::json!([
                {"lot_id": "LOT-1", "country_subentity_codes": ["BE231"]},
                {"lot_id": "LOT-2", "country_subentity_codes": []}
            ])
        );
        assert_eq!(
            open[0].raw["notice_lot_award_criteria_evidence"][0]["criteria"][0]["description"],
            "Service quality"
        );
        assert_eq!(
            open[0].raw["notice_lot_award_criteria_evidence"][0]["criteria"][0]["parameters"][0]
                ["numeric"],
            "35"
        );
        assert_eq!(
            open[0].raw["notice_lot_selection_evidence"][0]["selection_evidence"][0]["code"],
            "FINANCIAL_STANDING"
        );
        assert_eq!(
            open[0].raw["notice_lot_contract_execution_evidence"][0]["conditions"][0]["code"],
            "SAFE_HANDLING"
        );
        assert_eq!(
            open[0].raw["notice_lot_contract_execution_evidence"][0]["conditions"][0]
                ["code_list_name"],
            "Execution condition types"
        );
        assert_eq!(
            open[0].raw["notice_lot_contract_execution_evidence"][1]["conditions"],
            json!([])
        );
        assert_eq!(
            open[0].raw["notice_lot_subsequent_process_evidence"][0]["requirements"][0]["name"],
            "Second-stage capability"
        );
        assert_eq!(
            open[0].raw["notice_lot_subsequent_process_evidence"][1]["requirements"],
            json!([])
        );
        assert_eq!(
            open[0].raw["notice_lot_guarantee_curricula_evidence"][0]["guarantees"][0]
                ["description"],
            "Guarantee description"
        );
        assert_eq!(
            open[0].raw["notice_lot_guarantee_curricula_evidence"][0]["curricula"][0]
                ["code_list_name"],
            "curriculum codes"
        );
        assert!(open[0]
            .raw
            .get("notice_lot_place_evidence_truncated")
            .is_none());
        assert!(open[0]
            .raw
            .get("framework_maximum_evidence_truncated")
            .is_none());
        assert!(open[0]
            .raw
            .get("notice_lot_cpv_evidence_truncated")
            .is_none());
        assert!(open[0]
            .raw
            .get("notice_lot_award_criteria_evidence_truncated")
            .is_none());
        assert!(open[0]
            .raw
            .get("notice_lot_contract_execution_evidence_truncated")
            .is_none());
        assert!(open[0].raw.get("bosa_attachment_evidence").is_none());
        assert!(open[0].raw.get("unrelated").is_none());
    }

    #[test]
    fn projected_framework_maximum_evidence_is_bounded_and_reports_truncation() {
        let evidence = (0..101)
            .map(|index| {
                json!({
                    "amount": format!("{index}.00"),
                    "currency_id": "EUR",
                    "scope": "lot",
                    "lot_id": format!("LOT-{index}"),
                })
            })
            .collect::<Vec<_>>();
        let raw = serde_json::to_string(&evidence).unwrap();
        let projected = list_row_raw(ListRowRawEvidence {
            workspace: None,
            folder: None,
            container: None,
            framework_maximum_amounts: None,
            notice_lot_cpv_evidence: None,
            framework_maximum_evidence: Some(&raw),
            framework_maximum_evidence_truncated: Some("false"),
            notice_lot_ids_unique: None,
            notice_lot_place_evidence: None,
            notice_lot_place_evidence_truncated: None,
            notice_lot_award_criteria_evidence: None,
            notice_lot_award_criteria_evidence_truncated: None,
            notice_lot_selection_evidence: None,
            notice_lot_selection_evidence_truncated: None,
            notice_lot_contract_execution_evidence: None,
            notice_lot_contract_execution_evidence_truncated: None,
            notice_lot_subsequent_process_evidence: None,
            notice_lot_subsequent_process_evidence_truncated: None,
            notice_lot_guarantee_curricula_evidence: None,
            notice_lot_guarantee_curricula_evidence_truncated: None,
            notice_lot_ted_evidence: None,
        });
        assert_eq!(
            projected["framework_maximum_evidence"]
                .as_array()
                .unwrap()
                .len(),
            100
        );
        assert_eq!(projected["framework_maximum_evidence_truncated"], true);

        let malformed = list_row_raw(ListRowRawEvidence {
            workspace: None,
            folder: None,
            container: None,
            framework_maximum_amounts: None,
            notice_lot_cpv_evidence: None,
            framework_maximum_evidence: Some("not-json"),
            framework_maximum_evidence_truncated: None,
            notice_lot_ids_unique: None,
            notice_lot_place_evidence: None,
            notice_lot_place_evidence_truncated: None,
            notice_lot_award_criteria_evidence: None,
            notice_lot_award_criteria_evidence_truncated: None,
            notice_lot_selection_evidence: None,
            notice_lot_selection_evidence_truncated: None,
            notice_lot_contract_execution_evidence: None,
            notice_lot_contract_execution_evidence_truncated: None,
            notice_lot_subsequent_process_evidence: None,
            notice_lot_subsequent_process_evidence_truncated: None,
            notice_lot_guarantee_curricula_evidence: None,
            notice_lot_guarantee_curricula_evidence_truncated: None,
            notice_lot_ted_evidence: None,
        });
        assert_eq!(malformed["framework_maximum_evidence"], json!([]));
        assert_eq!(malformed["framework_maximum_evidence_truncated"], true);
    }

    #[test]
    fn projected_lot_cpv_evidence_is_bounded_and_reports_truncation() {
        let records = (0..21)
            .map(|lot| {
                json!({
                    "lot_id": format!("LOT-{lot}"),
                    "cpv_codes": (0..12).map(|code| format!("{code:08}")).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let serialized = serde_json::to_string(&records).expect("lot evidence should serialize");
        let (bounded, truncated) =
            bounded_lot_cpv_evidence(Some(&serialized)).expect("non-empty evidence should project");
        let lots = bounded
            .as_array()
            .expect("projected evidence should be an array");
        assert_eq!(lots.len(), MAX_PROJECTED_LOT_CPV_LOTS);
        assert!(truncated);
        assert!(lots.iter().all(|lot| {
            lot["cpv_codes"]
                .as_array()
                .is_some_and(|codes| codes.len() == MAX_PROJECTED_LOT_CPVS_PER_LOT)
        }));
    }

    #[test]
    fn projected_lot_place_evidence_is_bounded_and_reports_truncation() {
        let records = (0..21)
            .map(|lot| {
                json!({
                    "lot_id": format!("LOT-{lot}"),
                    "country_subentity_codes": (0..12)
                        .map(|code| format!("BE-{lot}-{code}"))
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let raw = serde_json::to_string(&records).expect("place evidence should serialize");
        let (bounded, truncated) = bounded_lot_place_evidence(Some(&raw), Some("false"))
            .expect("present evidence should be projected");
        let lots = bounded.as_array().expect("projected evidence is an array");
        assert_eq!(lots.len(), MAX_PROJECTED_LOT_PLACE_LOTS);
        assert!(truncated);
        assert!(lots.iter().all(|lot| {
            lot["country_subentity_codes"]
                .as_array()
                .is_some_and(|codes| codes.len() == MAX_PROJECTED_LOT_PLACE_CODES_PER_LOT)
        }));
        assert!(bounded_lot_place_evidence(None, None).is_none());
        let (malformed, truncated) = bounded_lot_place_evidence(Some("not-json"), None).unwrap();
        assert_eq!(malformed, json!([]));
        assert!(truncated);
    }

    #[test]
    fn projected_lot_award_criteria_are_bounded_and_report_truncation() {
        let criterion = json!({
            "source_element": "SubordinateAwardingCriterion",
            "source_ordinal_path": [0, 1],
            "type_code": "QUALITY",
            "description": "x".repeat(MAX_PROJECTED_LOT_AWARD_CRITERION_TEXT_CHARS + 1),
            "calculation_expression": null,
            "parameters": (0..12)
                .map(|index| json!({"code": format!("PARAM-{index}"), "numeric": index.to_string()}))
                .collect::<Vec<_>>(),
        });
        let records = (0..21)
            .map(|lot| json!({"lot_id": format!("LOT-{lot}"), "criteria": vec![criterion.clone(); 6]}))
            .collect::<Vec<_>>();
        let raw = serde_json::to_string(&records).expect("criteria should serialize");
        let (bounded, truncated) =
            bounded_lot_award_criteria_evidence(Some(&raw), Some("false")).unwrap();
        let lots = bounded.as_array().expect("criteria should be an array");
        assert_eq!(lots.len(), MAX_PROJECTED_LOT_AWARD_CRITERIA_LOTS);
        let projected = lots
            .iter()
            .flat_map(|lot| lot["criteria"].as_array().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(projected.len(), MAX_PROJECTED_LOT_AWARD_CRITERIA);
        assert_eq!(
            projected[0]["description"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_PROJECTED_LOT_AWARD_CRITERION_TEXT_CHARS
        );
        assert_eq!(
            projected[0]["parameters"].as_array().unwrap().len(),
            MAX_PROJECTED_LOT_AWARD_CRITERION_PARAMETERS
        );
        assert!(truncated);
        assert!(bounded_lot_award_criteria_evidence(None, None).is_none());
        let (malformed, truncated) =
            bounded_lot_award_criteria_evidence(Some("not-json"), None).unwrap();
        assert_eq!(malformed, json!([]));
        assert!(truncated);
    }

    #[test]
    fn projected_lot_selection_evidence_is_bounded_and_reports_bad_provenance() {
        let item = json!({
            "source_element": "SelectionCriteria",
            "source_path": "TenderingTerms/UBLExtensions/UBLExtension/ExtensionContent/EformsExtension/SelectionCriteria",
            "source_ordinal_path": [0, 0],
            "code": "C".repeat(129),
            "code_list_name": "eForms",
            "descriptions": ["D".repeat(MAX_PROJECTED_LOT_SELECTION_TEXT_CHARS + 1)],
        });
        let records = json!([{
            "lot_id": "LOT-1",
            "selection_evidence": vec![item.clone(); MAX_PROJECTED_LOT_SELECTION_PER_LOT + 1],
        }]);
        let raw = serde_json::to_string(&records).unwrap();
        let (bounded, truncated) =
            bounded_lot_selection_evidence(Some(&raw), Some("false")).unwrap();
        assert!(truncated);
        assert_eq!(
            bounded[0]["selection_evidence"].as_array().unwrap().len(),
            MAX_PROJECTED_LOT_SELECTION_PER_LOT
        );
        assert_eq!(
            bounded[0]["selection_evidence"][0]["code"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            128
        );
        assert_eq!(
            bounded[0]["selection_evidence"][0]["descriptions"][0]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_PROJECTED_LOT_SELECTION_TEXT_CHARS
        );
        assert_eq!(
            bounded[0]["selection_evidence"][0]["source_path"],
            item["source_path"]
        );
        let mut bad_path = item;
        bad_path["source_path"] = json!("TenderingTerms/NoticeWide/SelectionCriteria");
        let malformed = serde_json::to_string(&json!([{
            "lot_id": "LOT-1",
            "selection_evidence": [bad_path],
        }]))
        .unwrap();
        let (bounded, truncated) =
            bounded_lot_selection_evidence(Some(&malformed), Some("false")).unwrap();
        assert!(truncated);
        assert!(bounded[0]["selection_evidence"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(bounded_lot_selection_evidence(None, None).is_none());
    }

    #[test]
    fn projected_lot_contract_execution_evidence_is_bounded_and_rejects_bad_provenance() {
        let condition = json!({
            "source_element": "ContractExecutionRequirement",
            "source_path": "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode",
            "source_ordinal_path": [0],
            "code": "C".repeat(MAX_PROJECTED_LOT_CONTRACT_EXECUTION_CODE_CHARS + 1),
            "code_list_name": "N".repeat(MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LIST_NAME_CHARS + 1),
        });
        let records = (0..21)
            .map(|lot| {
                json!({
                    "lot_id": format!("LOT-{lot}"),
                    "conditions": vec![condition.clone(); 6],
                })
            })
            .collect::<Vec<_>>();
        let raw = serde_json::to_string(&records).unwrap();
        let (bounded, truncated) =
            bounded_lot_contract_execution_evidence(Some(&raw), Some("false")).unwrap();
        let lots = bounded.as_array().unwrap();
        assert_eq!(lots.len(), MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LOTS);
        assert_eq!(
            lots.iter()
                .map(|lot| lot["conditions"].as_array().unwrap().len())
                .sum::<usize>(),
            MAX_PROJECTED_LOT_CONTRACT_EXECUTION_RECORDS
        );
        assert_eq!(
            lots[0]["conditions"][0]["code"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_PROJECTED_LOT_CONTRACT_EXECUTION_CODE_CHARS
        );
        assert_eq!(
            lots[0]["conditions"][0]["code_list_name"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_PROJECTED_LOT_CONTRACT_EXECUTION_LIST_NAME_CHARS
        );
        assert!(truncated);

        let mut bad_path = condition;
        bad_path["source_path"] = json!("TenderingTerms/NoticeWide/ExecutionRequirementCode");
        let malformed = serde_json::to_string(&json!([{
            "lot_id": "LOT-1",
            "conditions": [bad_path],
        }]))
        .unwrap();
        let (bounded, truncated) =
            bounded_lot_contract_execution_evidence(Some(&malformed), Some("false")).unwrap();
        assert!(truncated);
        assert!(bounded[0]["conditions"].as_array().unwrap().is_empty());
        assert!(bounded_lot_contract_execution_evidence(None, None).is_none());
    }

    #[test]
    fn projected_lot_subsequent_process_evidence_is_bounded_and_rejects_bad_provenance() {
        let requirement = json!({
            "source_element": "SubsequentProcessTenderRequirement",
            "source_path": "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name",
            "source_ordinal_path": [0],
            "name": "N".repeat(MAX_PROJECTED_SUBSEQUENT_NAME_CHARS + 1),
        });
        let records = (0..21)
            .map(|lot| json!({"lot_id": format!("LOT-{lot}"), "requirements": vec![requirement.clone(); 6]}))
            .collect::<Vec<_>>();
        let raw = serde_json::to_string(&records).unwrap();
        let (bounded, truncated) =
            bounded_lot_subsequent_process_evidence(Some(&raw), Some("false")).unwrap();
        assert_eq!(
            bounded.as_array().unwrap().len(),
            MAX_PROJECTED_SUBSEQUENT_REQUIREMENT_LOTS
        );
        assert_eq!(
            bounded
                .as_array()
                .unwrap()
                .iter()
                .map(|lot| lot["requirements"].as_array().unwrap().len())
                .sum::<usize>(),
            MAX_PROJECTED_SUBSEQUENT_REQUIREMENTS
        );
        assert_eq!(
            bounded[0]["requirements"][0]["name"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_PROJECTED_SUBSEQUENT_NAME_CHARS
        );
        assert!(truncated);

        let mut bad_path = requirement;
        bad_path["source_path"] = json!("TenderingProcess/NoticeWide/Name");
        let malformed =
            serde_json::to_string(&json!([{"lot_id": "LOT-1", "requirements": [bad_path]}]))
                .unwrap();
        let (bounded, truncated) =
            bounded_lot_subsequent_process_evidence(Some(&malformed), Some("false")).unwrap();
        assert!(truncated);
        assert!(bounded[0]["requirements"].as_array().unwrap().is_empty());
        assert!(bounded_lot_subsequent_process_evidence(None, None).is_none());
    }

    #[test]
    fn projected_lot_guarantee_curricula_evidence_is_bounded_and_rejects_bad_provenance() {
        let guarantee = json!({
            "source_element": "RequiredFinancialGuarantee",
            "source_path": "TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode",
            "source_ordinal_path": [0],
            "code": "G".repeat(MAX_PROJECTED_GUARANTEE_CURRICULA_TEXT_CHARS + 1),
            "code_list_name": null,
            "description": null,
        });
        let raw = serde_json::to_string(&json!([{
            "lot_id": "LOT-1",
            "guarantees": vec![guarantee.clone(); 21],
            "curricula": [],
        }]))
        .unwrap();
        let (bounded, truncated) =
            bounded_lot_guarantee_curricula_evidence(Some(&raw), Some("false")).unwrap();
        assert_eq!(bounded[0]["guarantees"].as_array().unwrap().len(), 20);
        assert_eq!(
            bounded[0]["guarantees"][0]["code"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_PROJECTED_GUARANTEE_CURRICULA_TEXT_CHARS
        );
        assert!(truncated);

        let mut bad_path = guarantee;
        bad_path["source_path"] = json!("TenderingTerms/NoticeWide/GuaranteeTypeCode");
        let malformed = serde_json::to_string(&json!([{
            "lot_id": "LOT-1",
            "guarantees": [bad_path],
            "curricula": [],
        }]))
        .unwrap();
        let (bounded, truncated) =
            bounded_lot_guarantee_curricula_evidence(Some(&malformed), Some("false")).unwrap();
        assert!(truncated);
        assert!(bounded[0]["guarantees"].as_array().unwrap().is_empty());
        assert!(bounded_lot_guarantee_curricula_evidence(None, None).is_none());
    }

    #[test]
    fn open_tenders_skip_expired_notices_and_keep_undated_ones() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-open-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let expired = notice_to_tender(&json!({
            "publication-number": "expired-1",
            "notice-title": "Expired tender",
            "deadline-receipt-tender": "2026-09-19"
        }));
        let closing_today = notice_to_tender(&json!({
            "publication-number": "closing-today",
            "notice-title": "Closing today",
            "deadline-receipt-tender": "2026-09-20"
        }));
        let upcoming = notice_to_tender(&json!({
            "publication-number": "upcoming-1",
            "notice-title": "Upcoming tender",
            "deadline-receipt-tender": "2026-10-01"
        }));
        let later = notice_to_tender(&json!({
            "publication-number": "later-1",
            "notice-title": "Later tender",
            "deadline-receipt-tender": "2026-12-01"
        }));
        let undated = notice_to_tender(&json!({
            "publication-number": "undated-1",
            "notice-title": "Undated tender"
        }));
        store
            .upsert_many([&expired, &closing_today, &upcoming, &later, &undated])
            .expect("tenders should store");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 20).expect("date should be valid");
        let open = store
            .open_tenders(10, today, None)
            .expect("open tenders should load");
        let identifiers = open
            .iter()
            .map(|tender| tender.external_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            identifiers,
            vec!["closing-today", "upcoming-1", "later-1", "undated-1"]
        );
        assert!(open.iter().all(|tender| tender.raw.is_null()));
        assert_eq!(
            store
                .open_tenders(1, today, None)
                .expect("limited open tenders should load")
                .len(),
            1
        );
        let due = store
            .open_tenders(10, today, Some(7))
            .expect("due tenders should load");
        assert_eq!(
            due.iter()
                .map(|tender| tender.external_id.as_str())
                .collect::<Vec<_>>(),
            vec!["closing-today"]
        );
        assert_eq!(
            store
                .open_tenders(1, today, Some(7))
                .expect("limited due tenders should load")
                .len(),
            1
        );
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn read_only_store_serves_existing_data_and_rejects_missing_files() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-ro-{suffix}.duckdb"));
        let missing = std::env::temp_dir().join(format!("aanbestedingen-missing-{suffix}.duckdb"));
        {
            let store = TenderStore::open(&path).expect("database should open");
            let tender = notice_to_tender(&json!({
                "publication-number": "ro-1",
                "notice-title": "Read only tender"
            }));
            store.upsert(&tender).expect("tender should store");
        }
        let read_only = TenderStore::open_read_only(&path).expect("read-only open should work");
        assert_eq!(read_only.tenders(10).expect("read-only load").len(), 1);
        assert!(TenderStore::open_read_only(&missing).is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn upsert_is_idempotent() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let tender = notice_to_tender(&json!({
            "publication-number": "123-2026",
            "notice-title": "Works",
            "deadline-receipt-tender": "2026-10-15"
        }));
        store.upsert(&tender).expect("first insert should work");
        store.upsert(&tender).expect("second insert should update");
        assert_eq!(store.count().expect("count should work"), 1);
        let (tender_count, tenders_last_fetched) =
            store.tender_stats().expect("tender stats should work");
        assert_eq!(tender_count, 1);
        assert!(tenders_last_fetched.is_some());
        let loaded = store.tenders(10).expect("tenders should load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].external_id, "123-2026");
        assert_eq!(
            store
                .tender("ted", "123-2026")
                .expect("direct tender lookup should work")
                .expect("stored tender should exist")
                .external_id,
            "123-2026"
        );
        assert!(store
            .tender("ted", "missing")
            .expect("missing lookup should work")
            .is_none());

        let preferences = CompanyPreferences {
            excluded_terms: vec!["night work".to_owned()],
            maximum_value: Some(Decimal::new(100_000, 0)),
            recognition_class: Some(5),
            recognition_categories: vec!["d17".to_owned(), " c ".to_owned()],
            ..CompanyPreferences::default()
        };
        store
            .save_preferences("0123.456.789", &preferences)
            .expect("preferences should save");
        let loaded = store
            .preferences_for("0123.456.789")
            .expect("preferences should load");
        assert_eq!(loaded.maximum_value, Some(Decimal::new(100_000, 0)));
        assert_eq!(loaded.recognition_class, Some(5));
        // Categories are stored normalised so a later comparison is stable.
        assert_eq!(loaded.recognition_categories, vec!["C", "D17"]);
        let invalid_preferences = CompanyPreferences {
            minimum_value: Some(Decimal::new(200, 0)),
            maximum_value: Some(Decimal::new(100, 0)),
            ..CompanyPreferences::default()
        };
        assert!(store
            .save_preferences("0123.456.789", &invalid_preferences)
            .is_err());
        // A recognition class outside 1-8 is rejected rather than stored.
        let invalid_recognition = CompanyPreferences {
            recognition_class: Some(9),
            ..CompanyPreferences::default()
        };
        assert!(store
            .save_preferences("0123.456.789", &invalid_recognition)
            .is_err());

        store
            .record_feedback(
                "0123.456.789",
                "test",
                "123-2026",
                false,
                None,
                None,
                Some("too large"),
            )
            .expect("feedback should save");
        assert_eq!(
            store
                .feedback_for("0123.456.789")
                .expect("feedback should load")
                .get(&(String::from("test"), String::from("123-2026"))),
            Some(&false)
        );

        let award = AwardRecord {
            publication_number: "award-1".to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            values: vec![Decimal::new(50_000, 0)],
            ..AwardRecord::default()
        };
        assert_eq!(
            store
                .upsert_awards_many("0123.456.789", std::iter::once(&award))
                .expect("award batch should save"),
            1
        );
        assert_eq!(
            store
                .awards_for("0123.456.789")
                .expect("awards should load")
                .len(),
            1
        );
        let lot = AwardLot {
            publication_number: "award-1".to_owned(),
            lot_id: "LOT-1".to_owned(),
            winner_identifier: Some("0123456789".to_owned()),
            winner_name: Some("Example BV".to_owned()),
            value: Some(Decimal::new(50_000, 0)),
            lowest_value: Some(Decimal::new(48_000, 0)),
            highest_value: Some(Decimal::new(75_000, 0)),
            cpv_codes: vec!["45310000".to_owned()],
        };
        assert_eq!(
            store
                .upsert_award_lots_many("0123.456.789", "award-1", std::iter::once(&lot))
                .expect("award lot batch should save"),
            1
        );
        let lots = store
            .award_lots_for("0123.456.789")
            .expect("award lots should load");
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].lot_id, "LOT-1");
        assert_eq!(lots[0].winner_name.as_deref(), Some("Example BV"));
        assert_eq!(lots[0].lowest_value, Some(Decimal::new(48_000, 0)));
        assert_eq!(lots[0].highest_value, Some(Decimal::new(75_000, 0)));
        assert_eq!(
            store
                .upsert_award_lots_many("0123.456.789", "award-1", std::iter::empty())
                .expect("empty award lot refresh should clear stale lots"),
            0
        );
        assert!(store
            .award_lots_for("0123.456.789")
            .expect("cleared award lots should load")
            .is_empty());
        store
            .upsert_award_lots_many("0123.456.789", "award-1", std::iter::once(&lot))
            .expect("award lot should be restored");
        store
            .delete_award("0123.456.789", "award-1")
            .expect("award deletion should cascade");
        assert!(store
            .awards_for("0123.456.789")
            .expect("deleted awards should load")
            .is_empty());
        assert!(store
            .award_lots_for("0123.456.789")
            .expect("cascaded lots should load")
            .is_empty());
        let (award_count, awards_last_fetched) =
            store.award_stats().expect("award stats should work");
        assert_eq!(award_count, 0);
        assert!(awards_last_fetched.is_none());
        assert_eq!(
            store
                .award_lot_stats()
                .expect("award lot stats should work"),
            (0, 0)
        );
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn award_value_spreads_group_by_cpv_division() {
        use crate::ted_csv::HistoryAward;
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-spreads-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let mk = |cpv: &str, value: i64, offers: i64| HistoryAward {
            year: 2026,
            notice_url: None,
            buyer_name: None,
            winner_raw: "Example BV".to_owned(),
            winner_member: None,
            winner_enterprise: None,
            join_confidence: "none",
            award_value: Some(Decimal::new(value, 0)),
            cpv: Some(cpv.to_owned()),
            dt_award: None,
            sme: None,
            number_offers: Some(offers),
            ambiguous: false,
        };
        store
            .replace_ted_awards_history(
                2026,
                &[
                    mk("45210000", 100_000, 1),
                    mk("45310000", 200_000, 4),
                    mk("45210000", 300_000, 5),
                    mk("72000000", 50_000, 2),
                ],
            )
            .expect("history should save");
        let spreads = store.award_value_spreads().expect("spreads should compute");
        assert_eq!(spreads.len(), 2);
        let div45 = spreads
            .iter()
            .find(|s| s.cpv_division == "45")
            .expect("45 present");
        assert_eq!(div45.awards, 3);
        assert_eq!(div45.avg_awarded, Some(Decimal::new(200_000, 0)));
        assert!((div45.avg_offers.unwrap() - 10.0 / 3.0).abs() < 1e-9);
        assert!((div45.single_bid_share - 1.0 / 3.0).abs() < 1e-12);
        let div72 = spreads
            .iter()
            .find(|s| s.cpv_division == "72")
            .expect("72 present");
        assert_eq!(div72.awards, 1);
        assert_eq!(div72.avg_awarded, Some(Decimal::new(50_000, 0)));
        assert_eq!(div72.single_bid_share, 0.0);
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn lot_range_spreads_average_only_present_bounds() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-ranges-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let lot =
            |lot_id: &str, cpv: &str, value: i64, low: Option<i64>, high: Option<i64>| AwardLot {
                publication_number: "award-1".to_owned(),
                lot_id: lot_id.to_owned(),
                winner_identifier: None,
                winner_name: None,
                value: Some(Decimal::new(value, 0)),
                lowest_value: low.map(|v| Decimal::new(v, 0)),
                highest_value: high.map(|v| Decimal::new(v, 0)),
                cpv_codes: vec![cpv.to_owned()],
            };
        store
            .upsert_award_lots_many(
                "0123.456.789",
                "award-1",
                [
                    lot("LOT-1", "45310000", 50_000, Some(48_000), Some(75_000)),
                    lot("LOT-2", "45210000", 150_000, None, None),
                    lot("LOT-3", "72000000", 90_000, Some(80_000), Some(120_000)),
                ]
                .iter(),
            )
            .expect("range lots should save");
        let spreads = store
            .lot_range_spreads()
            .expect("range spreads should compute");
        assert_eq!(spreads.len(), 2);
        let div45 = spreads
            .iter()
            .find(|s| s.cpv_division == "45")
            .expect("45 present");
        assert_eq!(div45.lots, 2);
        assert_eq!(div45.avg_awarded, Some(Decimal::new(100_000, 0)));
        // NULL bounds excluded from averages, never zero-filled.
        assert_eq!(div45.avg_lowest, Some(Decimal::new(48_000, 0)));
        assert_eq!(div45.avg_highest, Some(Decimal::new(75_000, 0)));
        let div72 = spreads
            .iter()
            .find(|s| s.cpv_division == "72")
            .expect("72 present");
        assert_eq!(div72.lots, 1);
        assert_eq!(div72.avg_lowest, Some(Decimal::new(80_000, 0)));
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn nbb_financials_upsert_supersedes_same_filing() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-nbb-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let manual = NbbFinancials {
            enterprise_kbo: "0123456789".to_owned(),
            fiscal_year: 2024,
            period_start: Some("2023-01-01".to_owned()),
            period_end: Some("2023-12-31".to_owned()),
            turnover: Some(Decimal::new(5_000_000, 0)),
            staff_fte: Some(Decimal::new(42, 0)),
            equity: Some(Decimal::new(1_200_000, 0)),
            result: Some(Decimal::new(300_000, 0)),
            source_ref: "2024-12345".to_owned(),
            source_format: "json".to_owned(),
            acquisition_method: "manual-consult".to_owned(),
        };
        assert_eq!(
            store
                .upsert_nbb_financials(&manual)
                .expect("manual row should save"),
            1
        );
        // Same filing re-ingested via the API supersedes in place: no duplicate.
        let mut api = manual.clone();
        api.acquisition_method = "api-authentic-query".to_owned();
        api.turnover = Some(Decimal::new(5_000_001, 0));
        api.period_start = None;
        api.period_end = None;
        assert_eq!(
            store
                .upsert_nbb_financials(&api)
                .expect("api row should save"),
            1
        );
        let rows = store
            .nbb_financials_for("0123.456.789")
            .expect("financials should load");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].acquisition_method, "api-authentic-query");
        assert_eq!(rows[0].turnover, Some(Decimal::new(5_000_001, 0)));
        assert_eq!(rows[0].period_start.as_deref(), Some("2023-01-01"));
        assert_eq!(rows[0].period_end.as_deref(), Some("2023-12-31"));
        let evidence = store
            .nbb_financial_evidence_for("0123.456.789")
            .expect("provenance-rich financial evidence should load");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].enterprise_kbo, "0123456789");
        assert_eq!(evidence[0].source_ref, "2024-12345");
        assert_eq!(evidence[0].period_start.as_deref(), Some("2023-01-01"));
        assert_eq!(evidence[0].period_end.as_deref(), Some("2023-12-31"));
        assert_eq!(evidence[0].source_format, "json");
        assert_eq!(evidence[0].acquisition_method, "api-authentic-query");
        assert!(!evidence[0].stored_at.is_empty());
        // A different fiscal year is a separate row, not an overwrite.
        let mut other_year = manual.clone();
        other_year.fiscal_year = 2023;
        other_year.source_ref = "2023-999".to_owned();
        store
            .upsert_nbb_financials(&other_year)
            .expect("other year should save");
        let rows = store
            .nbb_financials_for("0123.456.789")
            .expect("financials should load");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fiscal_year, 2024); // newest first
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn nbb_evidence_lookup_distinguishes_storage_coverage_from_source_absence() {
        let connection = Connection::open_in_memory().unwrap();
        let schema = SchemaColumns::probe(&connection).unwrap();
        let store = TenderStore { connection, schema };
        let lookup = store
            .nbb_financial_evidence_lookup_for("0123.456.789")
            .unwrap();
        assert_eq!(lookup.coverage, "financials_table_missing");
        assert!(lookup.filings.is_empty());
        drop(store);

        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute("CREATE TABLE nbb_financials (enterprise_kbo VARCHAR)", [])
            .unwrap();
        let schema = SchemaColumns::probe(&connection).unwrap();
        let store = TenderStore { connection, schema };
        let lookup = store
            .nbb_financial_evidence_lookup_for("0123.456.789")
            .unwrap();
        assert_eq!(lookup.coverage, "financials_schema_incomplete");
        assert!(lookup.filings.is_empty());
        drop(store);

        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE nbb_financials (
                    enterprise_kbo VARCHAR, fiscal_year INTEGER,
                    turnover DECIMAL(18, 2), staff_fte DECIMAL(18, 1),
                    equity DECIMAL(18, 2), result DECIMAL(18, 2),
                    source_ref VARCHAR, source_format VARCHAR,
                    acquisition_method VARCHAR, fetched_at TIMESTAMPTZ
                );
                INSERT INTO nbb_financials VALUES (
                    '0123456789', 2023, NULL, NULL, NULL, NULL,
                    'legacy-ref', 'csv', 'manual-consult',
                    TIMESTAMPTZ '2025-01-01 00:00:00+00'
                );",
            )
            .unwrap();
        let schema = SchemaColumns::probe(&connection).unwrap();
        let store = TenderStore { connection, schema };
        let legacy = store
            .nbb_financial_evidence_lookup_for("0123.456.789")
            .unwrap();
        assert_eq!(legacy.coverage, "filings_stored");
        assert_eq!(legacy.filings.len(), 1);
        assert_eq!(legacy.filings[0].period_start, None);
        assert_eq!(legacy.filings[0].period_end, None);
        drop(store);

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-nbb-legacy-{suffix}.duckdb"));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE nbb_financials (
                    enterprise_kbo VARCHAR, fiscal_year INTEGER,
                    turnover DECIMAL(18, 2), staff_fte DECIMAL(18, 1),
                    equity DECIMAL(18, 2), result DECIMAL(18, 2),
                    source_ref VARCHAR, source_format VARCHAR,
                    acquisition_method VARCHAR, fetched_at TIMESTAMPTZ
                );
                INSERT INTO nbb_financials VALUES (
                    '0123456789', 2023, NULL, NULL, NULL, NULL,
                    'legacy-ref', 'csv', 'manual-consult',
                    TIMESTAMPTZ '2025-01-01 00:00:00+00'
                );",
            )
            .unwrap();
        drop(connection);
        let store = TenderStore::open(&path).unwrap();
        assert!(column_exists(&store.connection, "nbb_financials", "period_start").unwrap());
        assert!(column_exists(&store.connection, "nbb_financials", "period_end").unwrap());
        let migrated = store
            .nbb_financial_evidence_lookup_for("0123.456.789")
            .unwrap();
        assert_eq!(migrated.coverage, "filings_stored");
        assert_eq!(migrated.filings[0].period_start, None);
        assert_eq!(migrated.filings[0].period_end, None);
        drop(store);
        let _ = fs::remove_file(path);

        let store = TenderStore::open(":memory:").unwrap();
        let empty = store
            .nbb_financial_evidence_lookup_for("0123.456.789")
            .unwrap();
        assert_eq!(empty.coverage, "no_filing_stored");
        assert!(empty.filings.is_empty());
        let filing = NbbFinancials {
            enterprise_kbo: "0123456789".to_owned(),
            fiscal_year: 2024,
            period_start: None,
            period_end: None,
            turnover: None,
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: "2024-12345".to_owned(),
            source_format: "json".to_owned(),
            acquisition_method: "manual-consult".to_owned(),
        };
        store.upsert_nbb_financials(&filing).unwrap();
        let populated = store
            .nbb_financial_evidence_lookup_for("0123.456.789")
            .unwrap();
        assert_eq!(populated.coverage, "filings_stored");
        assert_eq!(populated.filings.len(), 1);
        assert_eq!(populated.filings[0].turnover, None);
        assert_eq!(populated.filings[0].period_start, None);
        assert_eq!(populated.filings[0].period_end, None);
    }

    #[test]
    fn nbb_evidence_lookup_bounds_rows_and_signals_truncation() {
        let store = TenderStore::open(":memory:").unwrap();
        for index in 0..=NBB_EVIDENCE_PROJECTION_LIMIT {
            store
                .upsert_nbb_financials(&NbbFinancials {
                    enterprise_kbo: "0123456789".to_owned(),
                    fiscal_year: 2025 - index as i64,
                    period_start: None,
                    period_end: None,
                    turnover: None,
                    staff_fte: None,
                    equity: None,
                    result: None,
                    source_ref: format!("synthetic-{index:03}"),
                    source_format: "xbrl".to_owned(),
                    acquisition_method: "manual-consult".to_owned(),
                })
                .unwrap();
        }

        let projected = store
            .nbb_financial_evidence_lookup_for("0123.456.789")
            .unwrap();
        assert_eq!(projected.coverage, "filings_stored");
        assert_eq!(projected.filings.len(), NBB_EVIDENCE_PROJECTION_LIMIT);
        assert!(projected.truncated);
        assert_eq!(projected.filings[0].fiscal_year, 2025);

        let complete = store.nbb_financial_evidence_for("0123.456.789").unwrap();
        assert_eq!(complete.len(), NBB_EVIDENCE_PROJECTION_LIMIT + 1);
    }

    #[test]
    fn evidence_listing_puts_the_current_period_first_within_a_year() {
        // Capacity triage reads the first row of this listing. Several filings
        // can cover one fiscal year, and a comparative period may carry no
        // turnover, so a listing ordered by fetch time can hide the turnover
        // that the current period discloses.
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-order-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let fin = |end: Option<&str>, turnover: Option<i64>, ref_: &str| NbbFinancials {
            enterprise_kbo: "0123456789".to_owned(),
            fiscal_year: 2024,
            period_start: end.map(|_| "2024-01-01".to_owned()),
            period_end: end.map(str::to_owned),
            turnover: turnover.map(|value| Decimal::new(value, 0)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: ref_.to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-archive-batch".to_owned(),
        };
        // Comparative period: no boundary, no turnover, stored last so that
        // fetch order would put it first.
        store
            .upsert_nbb_financials(&fin(Some("2024-12-31"), Some(500_000), "2024-current"))
            .expect("current row should store");
        store
            .upsert_nbb_financials(&fin(None, None, "2024-comparative"))
            .expect("comparative row should store");

        let rows = store.nbb_financial_evidence_for("0123456789").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].source_ref, "2024-current");
        assert_eq!(rows[0].turnover, Some(Decimal::new(500_000, 0)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capacity_outliers_flag_large_awards_against_latest_turnover() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-capacity-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let lot = |lot_id: &str, winner: Option<&str>, value: i64| AwardLot {
            publication_number: "cap-1".to_owned(),
            lot_id: lot_id.to_owned(),
            winner_identifier: winner.map(str::to_owned),
            winner_name: winner.map(|_| "Example BV".to_owned()),
            value: Some(Decimal::new(value, 0)),
            lowest_value: None,
            highest_value: None,
            cpv_codes: vec![],
        };
        store
            .upsert_award_lots_many(
                "0123.456.789",
                "cap-1",
                [
                    // dotted KBO: join must normalize to the plain NBB key
                    lot("LOT-1", Some("0537.875.391"), 5_000_000),
                    lot("LOT-2", Some("0537875391"), 100_000),
                    // unknown winner: coverage gap, never an outlier
                    lot("LOT-3", Some("9999.999.999"), 9_000_000),
                    lot("LOT-4", None, 1_000_000),
                ]
                .iter(),
            )
            .expect("capacity lots should save");
        let fin = |year: i64, turnover: i64| NbbFinancials {
            enterprise_kbo: "0537875391".to_owned(),
            fiscal_year: year,
            period_start: None,
            period_end: None,
            turnover: Some(Decimal::new(turnover, 0)),
            staff_fte: Some(Decimal::new(148, 1)),
            equity: None,
            result: None,
            source_ref: format!("{year}-test"),
            source_format: "csv".to_owned(),
            acquisition_method: "manual-consult".to_owned(),
        };
        store
            .upsert_nbb_financials(&fin(2023, 13_261_683))
            .expect("older filing should save");
        store
            .upsert_nbb_financials(&fin(2024, 20_687_444))
            .expect("newer filing should save");
        let flags = store
            .capacity_outliers(0.1)
            .expect("outliers should compute");
        // LOT-1 (5M / 20.7M ~= 0.24) flags against the LATEST turnover;
        // the small lot, the unknown winner and the anonymous lot do not.
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].lot_id, "LOT-1");
        assert_eq!(flags[0].winner_kbo, "0537875391");
        assert_eq!(flags[0].fiscal_year, 2024);
        assert_eq!(flags[0].turnover, Decimal::new(20_687_444, 0));
        assert!((flags[0].value_to_turnover - 0.2417).abs() < 0.001);
        assert!(store
            .capacity_outliers(0.5)
            .expect("high threshold should compute")
            .is_empty());
        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn capacity_outliers_carry_the_filing_date_so_the_report_can_state_its_age() {
        // The report questions a company's capacity, so it must be able to say
        // how old the figure behind that question is.
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-age-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        store
            .upsert_award_lots_many(
                "0123.456.789",
                "age-1",
                [AwardLot {
                    publication_number: "age-1".to_owned(),
                    lot_id: "LOT-1".to_owned(),
                    winner_identifier: Some("0537875391".to_owned()),
                    winner_name: Some("Example BV".to_owned()),
                    value: Some(Decimal::new(5_000_000, 0)),
                    lowest_value: None,
                    highest_value: None,
                    cpv_codes: vec![],
                }]
                .iter(),
            )
            .expect("award lot should save");
        let fin = |year: i64, period_end: &str, turnover: i64| NbbFinancials {
            enterprise_kbo: "0537875391".to_owned(),
            fiscal_year: year,
            period_start: None,
            period_end: Some(period_end.to_owned()),
            turnover: Some(Decimal::new(turnover, 0)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: format!("{year}-test"),
            source_format: "csv".to_owned(),
            acquisition_method: "manual-consult".to_owned(),
        };
        store
            .upsert_nbb_financials(&fin(2023, "2023-12-31", 13_261_683))
            .expect("older filing should save");
        store
            .upsert_nbb_financials(&fin(2024, "2024-06-30", 20_687_444))
            .expect("newer filing should save");

        let flags = store
            .capacity_outliers(0.1)
            .expect("outliers should compute");
        assert_eq!(flags.len(), 1);
        // The date of the filing actually used, not the older one.
        assert_eq!(flags[0].period_end.as_deref(), Some("2024-06-30"));
        assert_eq!(
            filing_age_days(
                flags[0].period_end.as_deref(),
                NaiveDate::from_ymd_opt(2026, 11, 8).unwrap()
            ),
            Some(861)
        );

        // An age that cannot be computed stays unknown rather than zero.
        assert_eq!(
            filing_age_days(None, NaiveDate::from_ymd_opt(2026, 11, 8).unwrap()),
            None
        );
        assert_eq!(
            filing_age_days(
                Some("not-a-date"),
                NaiveDate::from_ymd_opt(2026, 11, 8).unwrap()
            ),
            None
        );

        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn tied_fiscal_year_rows_pick_the_dated_period_not_an_arbitrary_one() {
        // An XBRL export stores a current and a comparative period under one
        // fiscal year, so ordering by fiscal_year alone leaves the choice to
        // the engine. With disagreeing turnover that changes the ratio.
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-tie-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        store
            .upsert_award_lots_many(
                "0123.456.789",
                "tie-1",
                [AwardLot {
                    publication_number: "tie-1".to_owned(),
                    lot_id: "LOT-1".to_owned(),
                    winner_identifier: Some("0537875391".to_owned()),
                    winner_name: Some("Example BV".to_owned()),
                    value: Some(Decimal::new(1_000_000, 0)),
                    lowest_value: None,
                    highest_value: None,
                    cpv_codes: vec![],
                }]
                .iter(),
            )
            .expect("lot should save");
        let fin = |end: Option<&str>, turnover: i64, ref_: &str| NbbFinancials {
            enterprise_kbo: "0537875391".to_owned(),
            fiscal_year: 2024,
            period_start: end.map(|_| "2024-01-01".to_owned()),
            period_end: end.map(str::to_owned),
            turnover: Some(Decimal::new(turnover, 0)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: ref_.to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-archive-batch".to_owned(),
        };
        // Comparative row without a period, current row with one, same year.
        store
            .upsert_nbb_financials(&fin(None, 100_000, "2024-aaa"))
            .expect("comparative row should store");
        store
            .upsert_nbb_financials(&fin(Some("2024-12-31"), 500_000, "2024-zzz"))
            .expect("current row should store");

        let outliers = store
            .capacity_outliers(1.0)
            .expect("outliers should compute");
        assert_eq!(outliers.len(), 1);
        // 1_000_000 / 500_000 = 2.0 against the dated row, not 10.0 against
        // whichever row the engine happened to return first.
        assert_eq!(outliers[0].turnover, Decimal::new(500_000, 0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capacity_coverage_accounts_for_every_lot_that_was_not_assessed() {
        // The outlier list is an inner join on turnover, so a lot whose winner
        // filed nothing simply disappears. Coverage must still account for it,
        // or an empty report reads as a clean bill of health.
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-coverage-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let lot = |publication: &str, lot_id: &str, winner: Option<&str>, value: i64| AwardLot {
            publication_number: publication.to_owned(),
            lot_id: lot_id.to_owned(),
            winner_identifier: winner.map(str::to_owned),
            winner_name: winner.map(|_| "Example BV".to_owned()),
            value: Some(Decimal::new(value, 0)),
            lowest_value: None,
            highest_value: None,
            cpv_codes: vec![],
        };
        store
            .upsert_award_lots_many(
                "0123.456.789",
                "cov-1",
                [
                    lot("cov-1", "LOT-1", Some("0537.875.391"), 5_000_000), // assessed
                    lot("cov-1", "LOT-2", None, 1_000_000),                 // no identifier
                    lot("cov-1", "LOT-3", Some("9999.999.999"), 9_000_000), // no NBB filing
                ]
                .iter(),
            )
            .expect("capacity lots should save");
        store
            .upsert_award_lots_many(
                "0123.456.789",
                "cov-2",
                [
                    lot("cov-2", "LOT-4", Some("0401.848.135"), 2_000_000),
                    // A French SIREN: an identifier, but not one NBB can answer
                    // for. Counting it as a missing Belgian filing would invent a
                    // gap that does not exist.
                    lot("cov-2", "LOT-5", Some("12345678"), 3_000_000),
                ]
                .iter(),
            )
            .expect("second publication should save");
        let fin = |kbo: &str, turnover: Option<i64>| NbbFinancials {
            enterprise_kbo: kbo.to_owned(),
            fiscal_year: 2024,
            period_start: None,
            period_end: None,
            turnover: turnover.map(|value| Decimal::new(value, 0)),
            staff_fte: None,
            equity: Some(Decimal::new(10, 0)),
            result: None,
            source_ref: format!("2024-{kbo}"),
            source_format: "xbrl".to_owned(),
            acquisition_method: "archive".to_owned(),
        };
        store
            .upsert_nbb_financials(&fin("0537875391", Some(20_687_444)))
            .expect("turnover filing should save");
        // A filing that exists but discloses no turnover is its own bucket: the
        // common abbreviated-accounts case, not the same as having no filing.
        store
            .upsert_nbb_financials(&fin("0401848135", None))
            .expect("turnover-less filing should save");

        let coverage = store.capacity_coverage().expect("coverage should compute");
        assert_eq!(coverage.lots_with_value, 5);
        assert_eq!(coverage.assessed, 1);
        assert_eq!(coverage.without_winner_identifier, 1);
        assert_eq!(coverage.not_belgian_identifier, 1);
        assert_eq!(coverage.without_financials, 1);
        assert_eq!(coverage.without_turnover, 1);
        assert_eq!(coverage.unassessed(), 4);
        assert_eq!(
            coverage.lots_with_value,
            coverage.assessed + coverage.unassessed(),
            "every lot must be either assessed or accounted for as unknown"
        );
        let described = coverage.describe();
        assert!(described.contains("4 of 5"), "{described}");
        assert!(described.contains("no usable turnover"), "{described}");
        assert!(
            described.contains("without a Belgian enterprise number"),
            "{described}"
        );

        drop(store);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn nbb_period_coverage_counts_undatable_rows() {
        // A store from an older importer keeps both boundaries empty. That has
        // to be a number, not an empty error: every filing then reads as
        // "period unknown" and the cause stays invisible.
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("period-coverage-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        let dated = NbbFinancials {
            enterprise_kbo: "0414141414".to_owned(),
            fiscal_year: 2024,
            period_start: Some("2024-01-01".to_owned()),
            period_end: Some("2024-12-31".to_owned()),
            turnover: Some(Decimal::from(100)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: "2024-11111".to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "archive".to_owned(),
        };
        let undated = NbbFinancials {
            enterprise_kbo: "0424242424".to_owned(),
            fiscal_year: 2024,
            period_start: None,
            period_end: None,
            turnover: Some(Decimal::from(200)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: "2024-22222".to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "archive".to_owned(),
        };
        store
            .upsert_nbb_financials(&dated)
            .expect("dated row should store");
        store
            .upsert_nbb_financials(&undated)
            .expect("undated row should store");

        let coverage = store
            .nbb_period_coverage()
            .expect("coverage should compute");
        assert_eq!(coverage.rows, 2);
        assert_eq!(coverage.with_both_boundaries, 1);
        assert_eq!(coverage.undatable(), 1);
        assert_eq!(
            coverage.describe(),
            "nbb period coverage: 1 of 2 row(s) carry both boundaries (1 start, 1 end); 1 undatable"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn period_coverage_reports_zero_on_a_store_without_period_columns() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE nbb_financials (
                    enterprise_kbo VARCHAR, fiscal_year INTEGER,
                    turnover DECIMAL(18, 2), staff_fte DECIMAL(18, 1),
                    equity DECIMAL(18, 2), result DECIMAL(18, 2),
                    source_ref VARCHAR, source_format VARCHAR,
                    acquisition_method VARCHAR, fetched_at TIMESTAMPTZ
                );
                INSERT INTO nbb_financials VALUES (
                    '0123456789', 2023, 100000, 5.0, NULL, NULL,
                    'legacy', 'csv', 'manual-consult',
                    TIMESTAMPTZ '2025-01-01 00:00:00+00'
                );",
            )
            .unwrap();
        let schema = SchemaColumns::probe(&connection).unwrap();
        let store = TenderStore { connection, schema };
        let coverage = store
            .nbb_period_coverage()
            .expect("coverage should compute");
        assert_eq!(coverage.rows, 1);
        assert_eq!(coverage.with_both_boundaries, 0);
    }

    #[test]
    fn an_opt_in_age_limit_withholds_a_filing_that_cannot_be_shown_recent() {
        let financial = NbbFinancials {
            enterprise_kbo: "0123456789".to_owned(),
            fiscal_year: 2015,
            period_start: Some("2015-01-01".to_owned()),
            period_end: Some("2015-12-31".to_owned()),
            turnover: Some(rust_decimal::Decimal::new(850_000, 0)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: "filing/2015-ref".to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-authentic-query".to_owned(),
        };
        let as_of = NaiveDate::from_ymd_opt(2026, 11, 4).expect("valid date");

        // Unset limit: the historical behaviour, the newest filing is used.
        let (turnover, _) = nbb_capacity_turnover(std::slice::from_ref(&financial), as_of, None);
        assert_eq!(turnover, Some(850_000.0));

        // A 730-day limit withholds a 3,961-day-old figure, and the provenance
        // still comes back so the caller can say why it is missing.
        let (turnover, provenance) =
            nbb_capacity_turnover(std::slice::from_ref(&financial), as_of, Some(730));
        assert_eq!(turnover, None, "a stale figure must not be used as current");
        let provenance = provenance.expect("provenance explains the withholding");
        assert_eq!(provenance.fiscal_year, 2015);
        assert_eq!(provenance.age_days, Some(3961));

        // A limit the filing satisfies leaves it usable.
        let (turnover, _) =
            nbb_capacity_turnover(std::slice::from_ref(&financial), as_of, Some(4000));
        assert_eq!(turnover, Some(850_000.0));
    }

    #[test]
    fn an_opt_in_age_limit_withholds_an_undated_filing_too() {
        // An undated filing cannot be shown to be recent, so a limit fails
        // closed instead of assuming it is fresh.
        let financial = NbbFinancials {
            enterprise_kbo: "0123456789".to_owned(),
            fiscal_year: 2025,
            period_start: None,
            period_end: None,
            turnover: Some(rust_decimal::Decimal::new(700_000, 0)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: "undated".to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-authentic-query".to_owned(),
        };
        let as_of = NaiveDate::from_ymd_opt(2026, 11, 4).expect("valid date");

        let (turnover, _) = nbb_capacity_turnover(std::slice::from_ref(&financial), as_of, None);
        assert_eq!(turnover, Some(700_000.0));

        let (turnover, provenance) =
            nbb_capacity_turnover(std::slice::from_ref(&financial), as_of, Some(730));
        assert_eq!(turnover, None);
        assert_eq!(provenance.expect("provenance").age_days, None);
    }

    #[test]
    fn nbb_capacity_turnover_discloses_period_age_without_a_stale_verdict() {
        let financial = NbbFinancials {
            enterprise_kbo: "0123456789".to_owned(),
            fiscal_year: 2015,
            period_start: Some("2015-01-01".to_owned()),
            period_end: Some("2015-12-31".to_owned()),
            turnover: Some(rust_decimal::Decimal::new(850_000, 0)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: "filing/2015-ref".to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-authentic-query".to_owned(),
        };
        let (turnover, notice) = nbb_capacity_turnover(
            &[financial],
            NaiveDate::from_ymd_opt(2026, 11, 4).expect("valid date"),
            None,
        );
        assert_eq!(turnover, Some(850_000.0));
        let notice = notice.expect("stored turnover should carry provenance");
        assert_eq!(notice.fiscal_year, 2015);
        assert_eq!(notice.age_days, Some(3961));
        assert_eq!(notice.short_clause(), "boekjaar FY2015, 3961 dagen oud");
        // The default notice must not claim a limit is in force, and the
        // limited one must not claim there is none.
        assert!(notice
            .notice()
            .contains("no freshness threshold is applied"));
        let limited = notice.notice_with_limit(Some(730));
        assert!(limited.contains("730 day freshness limit was applied"));
        assert!(!limited.contains("no freshness threshold is applied"));
        let notice = notice.notice();
        assert!(notice.contains("fiscal_year=2015"));
        assert!(notice.contains("period_end=2015-12-31"));
        assert!(notice.contains("age_days=3961 as_of=2026-11-04"));
        assert!(notice.contains("source_ref=filing_2015-ref"));
        assert!(notice.contains("no freshness threshold is applied"));
        assert!(!notice.to_ascii_lowercase().contains("stale"));
    }

    #[test]
    fn nbb_capacity_turnover_keeps_latest_row_and_unknown_period_semantics() {
        let latest_without_turnover = NbbFinancials {
            enterprise_kbo: "0123456789".to_owned(),
            fiscal_year: 2025,
            period_start: None,
            period_end: None,
            turnover: None,
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: "latest".to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-authentic-query".to_owned(),
        };
        let older_with_turnover = NbbFinancials {
            fiscal_year: 2024,
            turnover: Some(rust_decimal::Decimal::new(700_000, 0)),
            source_ref: "older".to_owned(),
            ..latest_without_turnover.clone()
        };
        let (turnover, notice) = nbb_capacity_turnover(
            &[latest_without_turnover, older_with_turnover],
            NaiveDate::from_ymd_opt(2026, 11, 4).expect("valid date"),
            None,
        );
        assert_eq!(turnover, None);
        assert_eq!(notice, None);

        let latest_with_turnover = NbbFinancials {
            fiscal_year: 2025,
            turnover: Some(rust_decimal::Decimal::new(700_000, 0)),
            ..NbbFinancials {
                enterprise_kbo: "0123456789".to_owned(),
                fiscal_year: 2025,
                period_start: None,
                period_end: None,
                turnover: None,
                staff_fte: None,
                equity: None,
                result: None,
                source_ref: "latest".to_owned(),
                source_format: "xbrl".to_owned(),
                acquisition_method: "api-authentic-query".to_owned(),
            }
        };
        let (turnover, notice) = nbb_capacity_turnover(
            &[latest_with_turnover],
            NaiveDate::from_ymd_opt(2026, 11, 4).expect("valid date"),
            None,
        );
        assert_eq!(turnover, Some(700_000.0));
        let notice = notice.expect("turnover note");
        assert_eq!(notice.period_end, "unknown");
        assert_eq!(notice.age_days, None);
        assert_eq!(notice.short_clause(), "boekjaar FY2025, leeftijd onbekend");
        assert!(notice
            .notice()
            .contains("period_end=unknown, age_days=unknown"));
    }

    #[test]
    fn ranking_and_capacity_check_select_the_same_filing() {
        // Fetch time and filing reference disagree here on purpose. Ranking reads
        // the first row of the evidence listing and capacity-check runs its own
        // window query; if the two rank those keys differently they report
        // different turnover for the same company and year.
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-agree-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        store
            .connection
            .execute_batch(
                "INSERT INTO nbb_financials (
                    enterprise_kbo, fiscal_year, period_start, period_end, turnover,
                    staff_fte, equity, result, source_ref, source_format,
                    acquisition_method, fetched_at
                 ) VALUES
                 ('0537875391', 2024, '2024-01-01', '2024-12-31', 100000,
                  NULL, NULL, NULL, 'ref-a', 'xbrl', 'archive',
                  TIMESTAMPTZ '2026-06-01 00:00:00+00'),
                 ('0537875391', 2024, '2024-01-01', '2024-12-31', 500000,
                  NULL, NULL, NULL, 'ref-b', 'xbrl', 'archive',
                  TIMESTAMPTZ '2026-01-01 00:00:00+00');
                 INSERT INTO award_lots (
                    enterprise_number, publication_number, lot_id, winner_identifier,
                    winner_name, value, cpv_codes, fetched_at, lowest_value, highest_value
                 ) VALUES (
                    '0123456789', 'agree-1', 'LOT-1', '0537875391', 'Example BV',
                    1000000, NULL, TIMESTAMPTZ '2026-01-01 00:00:00+00', NULL, NULL
                 );",
            )
            .expect("fixture should load");

        let rows = store.nbb_financials_for("0537.875.391").expect("listing");
        assert_eq!(
            rows[0].source_ref, "ref-b",
            "the listing must rank the filing reference above fetch time"
        );
        let outliers = store.capacity_outliers(1.0).expect("outliers");
        assert_eq!(outliers.len(), 1);
        assert_eq!(
            outliers[0].turnover,
            Decimal::new(500_000, 0),
            "capacity-check must select the same filing as the listing"
        );
        assert_eq!(outliers[0].value_to_turnover, 2.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn capacity_queries_tolerate_a_store_without_period_columns() {
        // Legacy stores predate period_start/period_end. The tie-break must not
        // turn a working report into a binder error on those stores.
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE nbb_financials (
                    enterprise_kbo VARCHAR, fiscal_year INTEGER,
                    turnover DECIMAL(18, 2), staff_fte DECIMAL(18, 1),
                    equity DECIMAL(18, 2), result DECIMAL(18, 2),
                    source_ref VARCHAR, source_format VARCHAR,
                    acquisition_method VARCHAR, fetched_at TIMESTAMPTZ
                );
                INSERT INTO nbb_financials VALUES (
                    '0537875391', 2024, 500000, 10.0, NULL, NULL,
                    'legacy', 'csv', 'manual-consult',
                    TIMESTAMPTZ '2025-01-01 00:00:00+00'
                );
                CREATE TABLE award_lots (
                    enterprise_number VARCHAR, publication_number VARCHAR, lot_id VARCHAR,
                    winner_identifier VARCHAR, winner_name VARCHAR, value DECIMAL(18, 2),
                    cpv_codes VARCHAR[], fetched_at TIMESTAMPTZ,
                    lowest_value DECIMAL(18, 2), highest_value DECIMAL(18, 2)
                );
                INSERT INTO award_lots VALUES (
                    '0123456789', 'legacy-1', 'LOT-1', '0537875391', 'Example BV',
                    1000000, NULL, TIMESTAMPTZ '2025-01-01 00:00:00+00', NULL, NULL
                );",
            )
            .unwrap();
        let schema = SchemaColumns::probe(&connection).unwrap();
        let store = TenderStore { connection, schema };
        let outliers = store
            .capacity_outliers(1.0)
            .expect("outliers should compute");
        assert_eq!(outliers.len(), 1);
        assert_eq!(outliers[0].turnover, Decimal::new(500_000, 0));
        let coverage = store.capacity_coverage().expect("coverage should compute");
        assert_eq!(coverage.assessed, 1);
    }

    #[test]
    fn capacity_coverage_without_lots_says_unknown_rather_than_clean() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aanbestedingen-coverage-empty-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let coverage = store.capacity_coverage().expect("coverage should compute");
        assert_eq!(coverage.lots_with_value, 0);
        assert_eq!(
            coverage.describe(),
            "no awarded lots with a value: capacity is unknown"
        );
        drop(store);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn bosa_revision_order_normalizes_rfc3339_offsets_and_timezone_free_timestamps() {
        let tender = |created: &str| Tender {
            source: "bda".into(),
            external_id: "folder".into(),
            raw: serde_json::json!({
                "bosa_publication_workspace_id": "workspace",
                "bosa_activation_date": "2026-10-30",
                "bosa_version_created_at": created,
            }),
            ..Default::default()
        };
        let utc = tender("2026-10-30T08:00:00Z");
        let offset = tender("2026-10-30T09:00:00+01:00");
        let naive = tender("2026-10-30T08:00:00.000000");
        let later = tender("2026-10-30T08:00:01+00:00");
        assert_eq!(
            bosa_revision_order(&utc, &offset),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            bosa_revision_order(&utc, &naive),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            bosa_revision_order(&utc, &later),
            Some(std::cmp::Ordering::Less)
        );
    }

    #[test]
    fn bosa_corrections_are_ordered_by_publication_not_repeated_xml_version() {
        let store = TenderStore::open(":memory:").unwrap();
        let tender = |notice: &str, activation: &str, created: &str, deadline: &str| Tender {
            source: "bda".into(),
            external_id: "folder".into(),
            title: "Call".into(),
            deadline: Some(deadline.into()),
            notice_type: Some("cn-general".into()),
            raw: serde_json::json!({"contract_folder_id":"folder","notice_version":"01","bosa_publication_workspace_id":"workspace","bosa_notice_id":notice,"bosa_activation_date":activation,"bosa_version_created_at":created}),
            ..Default::default()
        };
        let older = tender(
            "older",
            "2026-10-20",
            "2026-10-20T13:40:41.224137",
            "2026-10-22",
        );
        let newer = tender(
            "newer",
            "2026-10-30",
            "2026-10-30T08:32:36.056209",
            "2026-11-12",
        );
        store.upsert(&newer).unwrap();
        store.upsert(&older).unwrap();
        assert_eq!(
            store
                .tender("bda", "folder")
                .unwrap()
                .unwrap()
                .deadline
                .as_deref(),
            Some("2026-11-12")
        );
        let evidence = crate::bosa::evidence::extract_evidence(
            uuid::Uuid::new_v4(),
            "file.pdf",
            &"a".repeat(64),
            "Certificat demandé",
        )
        .unwrap();
        assert!(store
            .replace_attachment_evidence(
                "bda",
                "folder",
                "workspace",
                "older",
                std::slice::from_ref(&evidence)
            )
            .is_err());
        store
            .replace_attachment_evidence(
                "bda",
                "folder",
                "workspace",
                "newer",
                std::slice::from_ref(&evidence),
            )
            .unwrap();
        store.upsert(&newer).unwrap();
        let saved = store.tender("bda", "folder").unwrap().unwrap();
        assert_eq!(
            saved.raw["bosa_attachment_evidence"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(saved.requirements.is_empty());
        let latest = tender("latest", "2026-11-01", "2026-11-01T09:00:00", "2026-11-20");
        store.upsert(&latest).unwrap();
        assert!(store
            .tender("bda", "folder")
            .unwrap()
            .unwrap()
            .raw
            .get("bosa_attachment_evidence")
            .is_none());
        let mut different_workspace = latest.clone();
        different_workspace.raw["bosa_publication_workspace_id"] = "another-workspace".into();
        different_workspace.notice_type = Some("can-general".into());
        store.upsert_many([&older, &different_workspace]).unwrap();
        store.upsert(&different_workspace).unwrap();
        assert_eq!(store.count().unwrap(), 2);
        let award = store
            .bosa_workspace_tender("bda", "another-workspace")
            .unwrap()
            .unwrap();
        assert_eq!(award.notice_type.as_deref(), Some("can-general"));
        assert_eq!(award.external_id, "bosa-workspace:another-workspace");
        let mut manual = older.clone();
        manual
            .raw
            .as_object_mut()
            .unwrap()
            .remove("bosa_publication_workspace_id");
        assert!(store.upsert(&manual).is_err());
        let retained = store.tender("bda", "folder").unwrap().unwrap();
        assert_eq!(retained.raw["bosa_notice_id"], "latest");
        assert_eq!(retained.notice_type.as_deref(), Some("cn-general"));
        let ambiguous = tender(
            "different",
            "2026-11-01",
            "2026-11-01T09:00:00",
            "2026-12-01",
        );
        assert!(store.upsert(&ambiguous).is_err());
    }
    #[test]
    fn bosa_distinct_workspaces_survive_both_orders_without_manual_alias_deletion() {
        for reverse in [false, true] {
            let store = TenderStore::open(":memory:").unwrap();
            let make = |workspace: &str, notice_type: &str| Tender {
                source: "bda".into(),
                external_id: "folder".into(),
                title: workspace.into(),
                notice_type: Some(notice_type.into()),
                raw: serde_json::json!({"contract_folder_id":"folder","notice_version":"01","bosa_publication_workspace_id":workspace,"bosa_notice_id":format!("notice-{workspace}"),"bosa_activation_date":"2026-10-30","bosa_version_created_at":"2026-10-30T08:00:00"}),
                ..Default::default()
            };
            let call = make("call", "cn-general");
            let award = make("award", "can-general");
            let items = if reverse {
                [&award, &call]
            } else {
                [&call, &award]
            };
            store.upsert_many(items).unwrap();
            let first = store
                .bosa_workspace_tender("bda", if reverse { "award" } else { "call" })
                .unwrap()
                .unwrap();
            let second = store
                .bosa_workspace_tender("bda", if reverse { "call" } else { "award" })
                .unwrap()
                .unwrap();
            assert_ne!(first.external_id, second.external_id);
            assert_eq!(store.count().unwrap(), 2);
            let related = store.bosa_folder_notices("bda", "folder").unwrap();
            assert_eq!(related.len(), 2);
            assert!(store.bosa_folder_notices("ted", "folder").is_err());
            assert!(store
                .bosa_folder_notices("bda", "missing")
                .unwrap()
                .is_empty());
            store.upsert_many(items).unwrap();
            assert_eq!(store.count().unwrap(), 2);
            let mut manual = call.clone();
            manual
                .raw
                .as_object_mut()
                .unwrap()
                .remove("bosa_publication_workspace_id");
            assert!(store.upsert(&manual).is_err());
            assert_eq!(store.count().unwrap(), 2);
            assert_eq!(
                store
                    .bosa_workspace_tender("bda", "call")
                    .unwrap()
                    .unwrap()
                    .notice_type
                    .as_deref(),
                Some("cn-general")
            );
            assert_eq!(
                store
                    .bosa_workspace_tender("bda", "award")
                    .unwrap()
                    .unwrap()
                    .notice_type
                    .as_deref(),
                Some("can-general")
            );
        }
    }
    #[test]
    fn bosa_int_collision_never_replaces_unrelated_workspace() {
        let store = TenderStore::open(":memory:").unwrap();
        let make = |workspace: &str| Tender {
            source: "bda-int".into(),
            external_id: "folder".into(),
            title: workspace.into(),
            raw: serde_json::json!({"bosa_publication_workspace_id":workspace,"bosa_notice_id":"same","bosa_activation_date":"2026-10-30","bosa_version_created_at":"2026-10-30T08:00:00"}),
            ..Default::default()
        };
        store.upsert(&make("a")).unwrap();
        assert!(store.upsert(&make("b")).is_err());
        assert_eq!(
            store.tender("bda-int", "folder").unwrap().unwrap().title,
            "a"
        );
    }
}
