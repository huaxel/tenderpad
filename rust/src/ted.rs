#[cfg(feature = "network")]
use crate::parse_content_length;
use crate::{parse_money, Tender, TenderRequirement};
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate};
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::error::Error;
#[cfg(feature = "network")]
use std::io::Read;
#[cfg(feature = "network")]
use std::time::Duration;
#[cfg(feature = "network")]
use ureq::AgentBuilder;

pub const TED_SEARCH_URL: &str = "https://api.ted.europa.eu/v3/notices/search";
pub const DEFAULT_QUERY: &str = "(buyer-country=BEL OR place-of-performance=BEL)";
pub const MAX_TED_SEARCH_RESULTS: usize = 10_000;
/// Page depth at which a search failure is reported as the page wall rather than as
/// a bare status. TED refuses sequences past roughly sixty pages
/// (`docs/mission.md`); this leaves room to reach a diagnosis before the wall.
pub const PAGE_WALL_WARN_PAGE: usize = 40;

/// The added explanation for a search that failed deep into its page sequence.
///
/// Kept pure so the wording is testable without forty sockets: the notices already
/// collected are dropped with the error, so the operator has to be told which page
/// failed, how much was collected, and that narrowing the range is the way forward.
///
/// The wall is a *status* refusal, so an error the provider never answered (a
/// transport failure, or a locally refused call) is left alone: blaming the wall
/// for it would send the operator to narrow a range that was never the problem.
pub fn page_wall_hint(pages: usize, notices: usize, error: &TedFetchError) -> Option<String> {
    (pages >= PAGE_WALL_WARN_PAGE && error.provider_answered).then(|| {
        format!(
            "TED search failed on page {pages} after {notices} notice(s): {error}\n\
             TED refuses deep page sequences, at roughly sixty pages. Narrow the range rather \
             than running one wide search: a shorter --days window (ted-search, ted-ingest), or \
             an explicit --from/--to range (ted-ingest, award-ingest)."
        )
    })
}
#[cfg(feature = "network")]
const MAX_TED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Aggregate budget across all pages of one search. Per-page limits alone
/// still allow a 10,000-notice search to retain hundreds of megabytes (every
/// page is cloned into `notices`, then again into `Tender.raw`).
#[cfg(feature = "network")]
const MAX_TED_TOTAL_BYTES: usize = 64 * 1024 * 1024;
/// Field names requested from the TED search endpoint. This list is a wire
/// contract: TED answers the whole request with HTTP 400
/// `Parameter 'fields' contains unsupported value` if *any* name is unknown,
/// so a single legacy alias fails every search. Thirteen aliases
/// (`notice-id`, `title`, `buyer`, `notice-description`, `description`, `cpv`,
/// `place`, `estimated-value`, `value`, `deadline-receipt-tender`,
/// `submission-deadline`, `notice-language`, `language`) were removed on
/// 2026-10-18 after TED stopped accepting them. The response normalizers below
/// still read those keys when a response contains them.
///
/// Before adding a name, prove the live endpoint accepts it, because no
/// offline test can:
///
/// ```text
/// curl -sS -X POST https://api.ted.europa.eu/v3/notices/search \
///   -H 'Accept: application/json' -H 'Content-Type: application/json' \
///   -d '{"query":"buyer-country=BEL","fields":["<name>"],"limit":1,"scope":"LATEST"}'
/// ```
#[cfg(feature = "network")]
const TENDER_FIELDS: &[&str] = &[
    "publication-number",
    "publication-date",
    "notice-title",
    "buyer-name",
    "description-proc",
    "classification-cpv",
    "place-of-performance",
    "estimated-value-proc",
    "estimated-value-glo",
    "estimated-value-lot",
    "estimated-value-cur-lot",
    "framework-maximum-value-lot",
    "framework-maximum-value-cur-lot",
    "identifier-lot",
    "main-classification-lot",
    "framework-maximum-value-glo",
    "total-value",
    "deadline-receipt-tender-date-lot",
    "deadline-receipt-request",
    "notice-type",
    "official-language",
    "links",
    "selection-criterion-name-lot",
    "selection-criterion-description-lot",
    "exclusion-grounds-description",
    "contract-conditions-description-lot",
    "guarantee-required-description-lot",
];

const LANGUAGE_KEYS: &[&str] = &["eng", "fra", "nld", "deu", "en", "fr", "nl", "de"];

/// Which slice of TED's population a search covers. `LATEST` is a single OJ S
/// issue, `ACTIVE` is TED's convenience population (planning notices from the
/// past 364 days, unexpired competitions, results from the past 99 days) and
/// `ALL` is the rolling ten-year searchable set; see TED help, "Search scope".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchScope {
    Latest,
    #[default]
    Active,
    All,
}

impl SearchScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchScope::Latest => "LATEST",
            SearchScope::Active => "ACTIVE",
            SearchScope::All => "ALL",
        }
    }

    pub fn parse(value: &str) -> Result<Self, Box<dyn Error>> {
        match value.trim().to_ascii_lowercase().as_str() {
            "latest" => Ok(SearchScope::Latest),
            "active" => Ok(SearchScope::Active),
            "all" => Ok(SearchScope::All),
            other => Err(format!("unknown TED search scope: {other}").into()),
        }
    }
}

/// Default lookback of an importer run, in days.
pub const DEFAULT_LOOKBACK_DAYS: i64 = 30;

/// How much of TED's population one search covers.
///
/// The importer must not default to [`SearchScope::Latest`]: that is a single
/// OJ S issue, so a run only sees the notices published that day, and `LATEST`
/// returns nothing for a historical date range, so the days between runs can
/// never be repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchWindow {
    pub scope: SearchScope,
    /// Inclusive lower bound on the publication date, when one is applied.
    pub published_since: Option<NaiveDate>,
    /// Inclusive upper bound on the publication date, used to walk history in
    /// chunks: a single call for a year exceeds the 64 MiB response budget, so a
    /// backfill asks for one month at a time.
    pub published_until: Option<NaiveDate>,
}

impl Default for SearchWindow {
    fn default() -> Self {
        Self::recent(DEFAULT_LOOKBACK_DAYS, Local::now().date_naive())
    }
}

impl SearchWindow {
    /// The last `days` days of TED's active population. `days <= 0` applies no
    /// date bound, which only makes sense for an explicit full-range backfill.
    pub fn recent(days: i64, today: NaiveDate) -> Self {
        Self {
            scope: SearchScope::Active,
            published_since: (days > 0).then(|| today - ChronoDuration::days(days)),
            published_until: None,
        }
    }

    /// A rolling refresh whose publication floor the caller computed (see
    /// [`incremental_floor`]), instead of the fixed `today - days`.
    pub fn rolling(since: Option<NaiveDate>, scope: SearchScope) -> Self {
        Self {
            scope,
            published_since: since,
            published_until: None,
        }
    }

    /// The whole searchable history (`ALL`, no publication bound).
    ///
    /// Award history must use this and never the ingest window: confining a
    /// `winner-identifier` search to the last 30 days silently returns no
    /// awards for a company that won contracts two years ago, which empties the
    /// matcher's award-history signal. Measured: an award search for one
    /// supplier returned 11 notices from 2023–2024 with no bound and 0 with the
    /// default window.
    pub fn history() -> Self {
        Self {
            scope: SearchScope::All,
            published_since: None,
            published_until: None,
        }
    }

    /// A closed publication range over the whole archive, for a backfill chunk.
    pub fn range(from: NaiveDate, until: NaiveDate) -> Self {
        Self {
            scope: SearchScope::All,
            published_since: Some(from),
            published_until: Some(until),
        }
    }

    pub fn with_scope(mut self, scope: SearchScope) -> Self {
        self.scope = scope;
        self
    }

    pub fn with_days(mut self, days: i64, today: NaiveDate) -> Self {
        self.published_since = (days > 0).then(|| today - ChronoDuration::days(days));
        self.published_until = None;
        self
    }

    /// The expert query with the publication-date bound appended. TED's query
    /// language takes dates as `YYYYMMDD`.
    ///
    /// The caller's query is parenthesised first: TED's query language has no
    /// precedence rule that survives an appended `AND`, so without this an
    /// `a OR b` query means `a OR (b AND publication-date>=…)` and silently
    /// widens the population (measured: 18,947 notices instead of 1,877).
    pub fn query_for(&self, query: &str) -> String {
        match (self.published_since, self.published_until) {
            (None, None) => query.to_owned(),
            (since, until) => {
                let mut bound = String::new();
                if let Some(date) = since {
                    bound.push_str(&format!(" AND publication-date>={}", date.format("%Y%m%d")));
                }
                if let Some(date) = until {
                    bound.push_str(&format!(" AND publication-date<={}", date.format("%Y%m%d")));
                }
                format!("({query}){bound}")
            }
        }
    }
}

pub fn localized(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Object(object)) => LANGUAGE_KEYS
            .iter()
            .find_map(|language| object.get(*language).map(|child| localized(Some(child))))
            .filter(|text| !text.is_empty())
            .or_else(|| {
                object
                    .values()
                    .map(|child| localized(Some(child)))
                    .find(|text| !text.is_empty())
            })
            .unwrap_or_default(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| localized(Some(item)))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

pub fn strings(value: Option<&Value>) -> Vec<String> {
    let mut result = Vec::new();
    collect_strings(value, &mut result);
    deduplicate(&mut result);
    result
}

fn collect_strings(value: Option<&Value>, result: &mut Vec<String>) {
    match value {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => result.push(text.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                collect_strings(Some(item), result);
            }
        }
        Some(Value::Object(object)) => {
            // Mirror the Python reference: a preferred language key holding
            // a falsy value (null, "", [], {}) falls through to every
            // value instead of winning with nothing.
            let preferred = LANGUAGE_KEYS.iter().find_map(|language| {
                object
                    .get(*language)
                    .filter(|value| value_is_present(value))
            });
            if let Some(value) = preferred {
                collect_strings(Some(value), result);
            } else {
                for value in object.values() {
                    collect_strings(Some(value), result);
                }
            }
        }
        Some(other) => result.push(other.to_string()),
    }
}

fn value_is_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn deduplicate(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

pub fn urls(value: Option<&Value>) -> Vec<String> {
    let mut result = Vec::new();
    collect_urls(value, &mut result);
    deduplicate(&mut result);
    result
}

fn collect_urls(value: Option<&Value>, result: &mut Vec<String>) {
    match value {
        Some(Value::String(text)) if crate::is_web_link(text) => result.push(text.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                collect_urls(Some(item), result);
            }
        }
        Some(Value::Object(object)) => {
            for value in object.values() {
                collect_urls(Some(value), result);
            }
        }
        _ => {}
    }
}

fn optional_text(value: Option<&Value>) -> Option<String> {
    let text = localized(value);
    (!text.is_empty()).then_some(text)
}

fn first<'a>(notice: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| {
        notice.get(*key).filter(|value| match value {
            Value::Null => false,
            Value::String(text) => !text.is_empty(),
            Value::Array(items) => !items.is_empty(),
            _ => true,
        })
    })
}

fn decimal(value: Option<&Value>) -> Option<Decimal> {
    match value {
        Some(Value::Array(items)) => {
            let values = items
                .iter()
                .filter_map(|item| decimal(Some(item)))
                .collect::<Vec<_>>();
            (!values.is_empty()).then(|| values.iter().sum())
        }
        Some(Value::Object(object)) => {
            // Mirror the Python reference: keys holding null, "", or []
            // fall through to the next candidate instead of winning with
            // a value that parses to nothing.
            let present = |key: &str| {
                object
                    .get(key)
                    .filter(|value| !matches!(value, Value::Null))
                    .filter(|value| !matches!(value, Value::String(text) if text.is_empty()))
                    .filter(|value| !matches!(value, Value::Array(items) if items.is_empty()))
            };
            decimal(present("amount").or_else(|| present("value")))
        }
        Some(Value::Number(number)) => parse_money(&number.to_string()),
        Some(Value::String(text)) => parse_money(text),
        _ => None,
    }
}

fn date(value: Option<&Value>) -> Option<String> {
    let text = localized(value);
    let candidate = text.get(..10)?;
    let bytes = candidate.as_bytes();
    if bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        NaiveDate::parse_from_str(candidate, "%Y-%m-%d")
            .ok()
            .map(|_| candidate.to_owned())
    } else {
        None
    }
}

/// Per-lot values in notice order, without the summing `decimal` applies
/// to arrays: one entry per lot for size assessment. Non-scalar entries
/// are skipped, never zero-filled, so a ragged array shortens the lots
/// rather than inventing a free one.
fn lot_decimals(value: Option<&Value>) -> Vec<Decimal> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::Array(_) | Value::Object(_) | Value::Null => None,
                single => decimal(Some(single)),
            })
            .collect(),
        single => decimal(single).into_iter().collect(),
    }
}

/// Retain only source cardinality and a value when it is a single scalar.
/// Multi-lot TED fields are separate arrays, so callers must never zip them by
/// index. The original notice remains intact alongside this bounded summary.
fn ted_lot_field_evidence(value: Option<&Value>) -> Value {
    let (count, singleton) = match value {
        None | Some(Value::Null) => (0, None),
        Some(Value::Array(items)) => (
            items.len(),
            (items.len() == 1)
                .then(|| source_scalar(&items[0]))
                .flatten(),
        ),
        Some(single) => (1, source_scalar(single)),
    };
    let status = match (count, singleton.is_some()) {
        (0, _) => "not_recorded",
        (1, true) => "singleton",
        (1, false) => "invalid_singleton",
        (_, _) => "multiple_values",
    };
    json!({
        "source_cardinality": count,
        "singleton_value": singleton,
        "status": status,
    })
}

fn source_scalar(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty() && text.chars().count() <= 256).then(|| text.to_owned())
}

fn ted_lot_evidence(object: &Map<String, Value>) -> Value {
    json!({
        "association_rule": "singleton_field_cardinality_only_no_positional_join",
        "fields": {
            "lot_id": ted_lot_field_evidence(object.get("identifier-lot")),
            "main_classification": ted_lot_field_evidence(object.get("main-classification-lot")),
            "estimated_value": ted_lot_field_evidence(object.get("estimated-value-lot")),
            "estimated_currency": ted_lot_field_evidence(object.get("estimated-value-cur-lot")),
            "framework_maximum": ted_lot_field_evidence(object.get("framework-maximum-value-lot")),
            "framework_currency": ted_lot_field_evidence(object.get("framework-maximum-value-cur-lot")),
        }
    })
}

pub fn try_notice_to_tender(notice: &Value) -> Result<Tender, Box<dyn Error>> {
    let object = notice
        .as_object()
        .ok_or("TED notice must be a JSON object")?;
    let requirement_fields = [
        ("selection-criterion-name-lot", "selection"),
        ("selection-criterion-description-lot", "selection"),
        ("exclusion-grounds-description", "exclusion"),
        ("contract-conditions-description-lot", "contract_condition"),
        ("guarantee-required-description-lot", "guarantee"),
    ];
    let mut seen = HashSet::new();
    let mut requirements = Vec::new();
    for (field, kind) in requirement_fields {
        for text in strings(object.get(field)) {
            if !text.is_empty() && seen.insert((kind, text.clone())) {
                requirements.push(TenderRequirement {
                    text,
                    kind: kind.to_owned(),
                    mandatory: true,
                    source_page: None,
                });
            }
        }
    }

    let external_id = localized(first(object, &["publication-number", "notice-id"]));
    if external_id.trim().is_empty() {
        return Err("TED notice has no publication number or notice id".into());
    }
    let title = localized(first(object, &["notice-title", "title"]));
    let lot_fields_present = [
        "identifier-lot",
        "main-classification-lot",
        "estimated-value-lot",
        "estimated-value-cur-lot",
        "framework-maximum-value-lot",
        "framework-maximum-value-cur-lot",
    ]
    .iter()
    .any(|field| object.contains_key(*field));
    let mut raw = notice.clone();
    if lot_fields_present {
        if let Some(raw_object) = raw.as_object_mut() {
            raw_object.insert(
                "notice_lot_ted_evidence".to_owned(),
                ted_lot_evidence(object),
            );
        }
    }
    Ok(Tender {
        source: "ted".to_owned(),
        external_id,
        title,
        buyer: optional_text(first(object, &["buyer-name", "buyer"])),
        description: localized(first(
            object,
            &["description-proc", "notice-description", "description"],
        )),
        cpv_codes: crate::clean_codes(strings(first(object, &["classification-cpv", "cpv"]))),
        regions: strings(first(object, &["place-of-performance", "place"])),
        estimated_value: decimal(first(
            object,
            &[
                "estimated-value-proc",
                "estimated-value-glo",
                "estimated-value-lot",
                "framework-maximum-value-lot",
                "framework-maximum-value-glo",
                "estimated-value",
                "total-value",
                "value",
            ],
        )),
        lot_values: {
            // Estimated lot values first; framework maxima only when no
            // estimated values exist (never mixed: the arrays describe
            // different totals and concatenating them double-counts lots).
            let estimated = lot_decimals(object.get("estimated-value-lot"));
            if estimated.is_empty() {
                lot_decimals(object.get("framework-maximum-value-lot"))
            } else {
                estimated
            }
        },
        deadline: date(first(
            object,
            &[
                "deadline-receipt-tender-date-lot",
                "deadline-receipt-tender",
                "deadline-receipt-request",
                "submission-deadline",
            ],
        )),
        publication_date: date(first(object, &["publication-date"])),
        languages: strings(first(
            object,
            &["official-language", "notice-language", "language"],
        ))
        .iter()
        // TED returns three-letter codes (`NLD`, `FRA`); the matcher normalises
        // through the alias table, which is keyed in lower case.
        .map(|language| language.to_lowercase())
        .collect(),
        notice_type: optional_text(first(object, &["notice-type", "form-type"]))
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty()),
        requirements,
        document_urls: urls(object.get("links")),
        raw,
    })
}

pub fn notice_to_tender(notice: &Value) -> Tender {
    try_notice_to_tender(notice).expect("TED notice should be valid")
}

#[derive(Debug, Clone)]
#[cfg(feature = "network")]
pub struct TedClient {
    pub endpoint: String,
    pub timeout: Duration,
}

#[cfg(feature = "network")]
impl Default for TedClient {
    fn default() -> Self {
        Self {
            endpoint: TED_SEARCH_URL.to_owned(),
            timeout: Duration::from_secs(30),
        }
    }
}

#[cfg(feature = "network")]
fn parse_search_response(response: ureq::Response) -> Result<(Value, usize), Box<dyn Error>> {
    if parse_content_length(response.header("Content-Length"))
        .is_some_and(|length| length > MAX_TED_RESPONSE_BYTES)
    {
        return Err(
            format!("TED response exceeded the {MAX_TED_RESPONSE_BYTES} byte limit").into(),
        );
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take((MAX_TED_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    let len = body.len();
    Ok((parse_search_response_body(&body)?, len))
}

#[cfg(feature = "network")]
fn parse_search_response_body(body: &[u8]) -> Result<Value, Box<dyn Error>> {
    if body.len() > MAX_TED_RESPONSE_BYTES {
        return Err(
            format!("TED response exceeded the {MAX_TED_RESPONSE_BYTES} byte limit").into(),
        );
    }
    Ok(serde_json::from_slice(body)?)
}

#[cfg(feature = "network")]
fn account_search_bytes(
    total: &mut usize,
    page_bytes: usize,
    limit: usize,
) -> Result<(), Box<dyn Error>> {
    let updated = total
        .checked_add(page_bytes)
        .ok_or("TED total byte count overflow")?;
    if updated > limit {
        return Err(format!("TED search exceeded the {limit} total byte limit").into());
    }
    *total = updated;
    Ok(())
}

#[cfg(feature = "network")]
fn convert_notices(notices: &[Value]) -> Vec<Tender> {
    notices
        .iter()
        .filter_map(|notice| try_notice_to_tender(notice).ok())
        .collect()
}

/// One page sequence's result.
///
/// `arrived` is the number of notices TED delivered, while `tenders` holds those
/// that converted. Callers need both: a walk that fetched the whole population
/// must not look short because one notice was malformed, and a walk that really was
/// cut short must not look complete.
pub struct SearchPage {
    pub tenders: Vec<Tender>,
    /// TED's own population for the query it answered, when it reported one.
    pub total_notice_count: Option<i64>,
    pub arrived: usize,
}

/// The most months one backfill may request, so a typo cannot start an
/// unbounded run over the ten-year archive.
pub const MAX_BACKFILL_MONTHS: usize = 36;

/// How much of an incremental refresh is re-read even when the previous run was
/// recent.
///
/// The overlap is about *dates*, not about corrections, and the distinction is
/// worth stating because it bounds what narrowing can lose:
///
/// - TED filters on `publication-date`, which is a calendar day, and the recorded
///   watermark is also a calendar day. A run at 00:05 has to cover the previous
///   day's publications, so a floor of exactly "last run" would skip them.
/// - It also absorbs a few days of publication/indexing lag and a scheduler that
///   fires late.
///
/// A *correction* is not covered by re-reading anything: a corrigendum is a new
/// publication with its own number for the same procurement, so it carries a
/// recent publication date and falls inside any recent window on its own. The
/// store keeps both rows and the read paths collapse them by procurement
/// (`collapse_notice_versions`, pinned by `health_counts_agree_with_the_ladder`).
///
/// What narrowing genuinely cannot see is an in-place edit to an *already
/// published* number: `upsert_with_identity_at` replaces a TED row wholesale, so
/// such an edit is picked up only while the notice is still inside whatever
/// window a run covers. That is the residual risk, and no overlap removes it —
/// it only shortens how long it stays repairable.
///
/// Seven days is deliberately generous for a date-boundary concern: it costs a
/// few extra pages on a daily refresh and covers a missed weekend.
pub const INCREMENTAL_OVERLAP_DAYS: i64 = 7;

/// The publication-date floor for a rolling refresh.
///
/// `days` only decides where a run starts when nothing usable was recorded: with a
/// watermark, the floor is `last - overlap_days`, wherever that lies, and `--days 0`
/// is therefore a valid way to say "from where I left off". So:
///
/// - a store that ran yesterday is re-read from the overlap instead of the whole
///   window, which turns a daily run into a delta;
/// - a store that was down longer than `days` is read from its watermark, so the
///   idle period becomes work rather than a hole. Clamping the result *up* to
///   `today - days` skips every notice published between the last run and that
///   floor, silently: a live incremental run announced a seven-day overlap while
///   TED was asked for the two-day window. Clamping *down* to it would instead make
///   every run a full window, so neither clamp is right;
/// - the widening is bounded in practice by the caller, which refuses ranges
///   wider than [`MAX_BACKFILL_MONTHS`] rather than running unbounded.
///
/// A watermark ahead of `today` is a clock disagreement rather than coverage, so
/// it falls back to the ordinary window: shrinking to a one-day window on a bad
/// clock would silently lose notices.
pub fn incremental_floor(
    days: i64,
    today: NaiveDate,
    last_refresh: Option<NaiveDate>,
    overlap_days: i64,
) -> Option<NaiveDate> {
    let window = || (days > 0).then(|| today - ChronoDuration::days(days));
    match last_refresh {
        // A recorded run bounds the window on its own, so `days` is irrelevant here —
        // including `--days 0`, which is how a caller says "from where I left off".
        // Earlier than the ordinary window when the store was idle, later when it ran
        // recently; clamping towards `window` in either direction loses notices,
        // upwards by skipping the idle period and downwards by making every
        // incremental run a full window again.
        Some(last) if last <= today => Some(last - ChronoDuration::days(overlap_days.max(0))),
        // A watermark ahead of today is a clock disagreement rather than coverage, so
        // it cannot bound anything and the ordinary window applies.
        Some(_) => window(),
        None => window(),
    }
}

/// Split a closed date range into per-month chunks, oldest first. A chunk is
/// always fully inside the requested range, so the first and last months keep
/// the caller's bounds.
pub fn month_chunks(from: NaiveDate, until: NaiveDate) -> Vec<(NaiveDate, NaiveDate)> {
    let mut chunks = Vec::new();
    let mut start = from;
    while start <= until {
        let (year, month) = (start.year(), start.month());
        let next_month = if month == 12 {
            NaiveDate::from_ymd_opt(year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)
        };
        let Some(next_month) = next_month else {
            break;
        };
        let end = (next_month - ChronoDuration::days(1)).min(until);
        chunks.push((start, end));
        start = next_month;
    }
    chunks
}

/// Retries a throttled or transient page.
///
/// The attempts, backoff and `Retry-After` rules live in
/// [`crate::http_retry`], so TED and the audited clients cannot drift into
/// different ideas of what a retry is; this wrapper only supplies TED's own
/// give-up message. Crate-visible so the award-XML fetch shares the rule set
/// instead of inventing a second one.
#[cfg(feature = "network")]
pub(crate) fn send_with_retry<F>(mut send: F) -> Result<ureq::Response, TedFetchError>
where
    F: FnMut() -> Result<ureq::Response, ureq::Error>,
{
    use crate::http_retry::{send_with_retry as retrying, RetryPolicy, RetryStop};
    match retrying(
        &RetryPolicy::ted_search(),
        &mut send,
        crate::http_retry::ureq_signal,
    ) {
        Ok(response) => Ok(response),
        Err(RetryStop::Failed(error)) => Err(TedFetchError {
            provider_answered: matches!(error, ureq::Error::Status(..)),
            message: error.to_string(),
        }),
        Err(stop @ RetryStop::Exhausted { status, budget, .. }) => Err(TedFetchError {
            provider_answered: status.is_some(),
            message: format!(
                "TED search gave up after {} retries ({}) to stay within {} seconds of backoff",
                stop.attempts().saturating_sub(1),
                status.map_or_else(|| "no status".to_owned(), |status| status.to_string()),
                budget.as_secs()
            ),
        }),
    }
}

/// Why one TED request stopped.
///
/// The message is what the operator reads; `provider_answered` is the one extra
/// fact the caller needs, because only a status refusal can be the page wall.
/// Ungated: the type itself touches no network code, and the public
/// `page_wall_hint` takes it in both feature configurations.
#[derive(Debug)]
pub struct TedFetchError {
    message: String,
    provider_answered: bool,
}

impl std::fmt::Display for TedFetchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TedFetchError {}

#[cfg(feature = "network")]
impl TedClient {
    pub fn search_raw(
        &self,
        query: &str,
        fields: &[&str],
        limit: usize,
    ) -> Result<Vec<Value>, Box<dyn Error>> {
        self.search_raw_window(query, fields, limit, &SearchWindow::default())
    }

    /// Search a specific population window instead of the default one.
    pub fn search_raw_window(
        &self,
        query: &str,
        fields: &[&str],
        limit: usize,
        window: &SearchWindow,
    ) -> Result<Vec<Value>, Box<dyn Error>> {
        Ok(self
            .search_raw_window_with_total(query, fields, limit, window)?
            .0)
    }

    /// Same as [`TedClient::search_raw_window`], but also returns TED's own
    /// `totalNoticeCount` for the population (first page), so a refresh can
    /// record which population it drew from. `None` when TED omits it.
    pub fn search_raw_window_with_total(
        &self,
        query: &str,
        fields: &[&str],
        limit: usize,
        window: &SearchWindow,
    ) -> Result<(Vec<Value>, Option<i64>), Box<dyn Error>> {
        self.search_raw_with_budget(query, fields, limit, window, MAX_TED_TOTAL_BYTES)
    }

    fn search_raw_with_budget(
        &self,
        query: &str,
        fields: &[&str],
        limit: usize,
        window: &SearchWindow,
        byte_limit: usize,
    ) -> Result<(Vec<Value>, Option<i64>), Box<dyn Error>> {
        if limit > MAX_TED_SEARCH_RESULTS {
            return Err(
                format!("TED search limit must not exceed {MAX_TED_SEARCH_RESULTS}").into(),
            );
        }
        let client = AgentBuilder::new().timeout(self.timeout).build();
        let mut notices = Vec::new();
        let mut next_token: Option<String> = None;
        let mut total_bytes = 0_usize;
        let mut pages = 0_usize;
        // TED's own count for the whole population, read from the first page
        // so a refresh can say which population it drew from.
        let mut total_notice_count: Option<i64> = None;

        while notices.len() < limit {
            let page_limit = 100.min(limit - notices.len());
            let mut payload = json!({
                "query": window.query_for(query),
                "fields": fields,
                "limit": page_limit,
                "scope": window.scope.as_str(),
                "paginationMode": "ITERATION"
            });
            if let Some(token) = &next_token {
                payload["iterationNextToken"] = Value::String(token.clone());
            }
            pages += 1;
            let response = send_with_retry(|| {
                let request = client
                    .post(&self.endpoint)
                    .set("Accept", "application/json")
                    .set("Content-Type", "application/json");
                request.send_json(payload.clone())
            })
            .map_err(|error| match page_wall_hint(pages, notices.len(), &error) {
                // A failure deep into a page sequence is TED's page wall, and a bare
                // `400` says nothing about how far the walk got. Anything the provider
                // never answered keeps its own message.
                Some(hint) => hint,
                None => error.to_string(),
            })?;
            let (response, body_len) = parse_search_response(response)?;
            account_search_bytes(&mut total_bytes, body_len, byte_limit)?;
            if total_notice_count.is_none() {
                total_notice_count = response.get("totalNoticeCount").and_then(Value::as_i64);
            }
            let batch = response
                .get("notices")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let response_token = response
                .get("iterationNextToken")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            notices.extend(batch.iter().cloned());
            next_token = response_token;
            if batch.is_empty() || next_token.is_none() {
                break;
            }
        }
        notices.truncate(limit);
        Ok((notices, total_notice_count))
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Tender>, Box<dyn Error>> {
        self.search_window(query, limit, &SearchWindow::default())
    }

    pub fn search_window(
        &self,
        query: &str,
        limit: usize,
        window: &SearchWindow,
    ) -> Result<Vec<Tender>, Box<dyn Error>> {
        Ok(convert_notices(&self.search_raw_window(
            query,
            TENDER_FIELDS,
            limit,
            window,
        )?))
    }

    /// Same as [`TedClient::search_window`], plus TED's population total.
    pub fn search_window_with_total(
        &self,
        query: &str,
        limit: usize,
        window: &SearchWindow,
    ) -> Result<SearchPage, Box<dyn Error>> {
        let (notices, total) =
            self.search_raw_window_with_total(query, TENDER_FIELDS, limit, window)?;
        let arrived = notices.len();
        Ok(SearchPage {
            tenders: convert_notices(&notices),
            total_notice_count: total,
            arrived,
        })
    }

    pub fn search_default(&self, limit: usize) -> Result<Vec<Tender>, Box<dyn Error>> {
        self.search(DEFAULT_QUERY, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "network")]
    #[test]
    fn paginated_search_enforces_total_budget() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let body = r#"{"notices":[{"publication-number":"1"}],"iterationNextToken":"next"}"#;
        for budget in [body.len() * 2, body.len() * 2 - 1] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(stream);
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((key, value)) = line.split_once(':') {
                            if key.eq_ignore_ascii_case("Content-Length") {
                                length = value.trim().parse::<usize>().unwrap();
                            }
                        }
                    }
                    let mut request_body = vec![0; length];
                    reader.read_exact(&mut request_body).unwrap();
                    write!(
                        reader.get_mut(),
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            });
            let client = TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            };
            let result =
                client.search_raw_with_budget("test", &[], 2, &SearchWindow::default(), budget);
            server.join().unwrap();
            if budget == body.len() * 2 {
                assert_eq!(result.unwrap().0.len(), 2);
            } else {
                assert!(result.unwrap_err().to_string().contains("total byte limit"));
            }
        }
    }

    #[cfg(feature = "network")]
    #[test]
    fn search_reports_teds_population_total() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let body = r#"{"totalNoticeCount":2889,"notices":[{"publication-number":"1"}]}"#;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut content_length = 0_usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim().to_owned();
                if line.is_empty() {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length:") {
                    content_length = value.trim().parse().unwrap();
                }
            }
            let mut request_body = vec![0u8; content_length];
            use std::io::Read;
            reader.read_exact(&mut request_body).unwrap();
            let mut stream = reader.into_inner();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        let (notices, total) = client
            .search_raw_with_budget("test", &[], 10, &SearchWindow::default(), 1 << 20)
            .expect("mock search should succeed");
        server.join().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(total, Some(2889), "first-page total must be captured");
    }

    #[cfg(feature = "network")]
    #[test]
    fn aggregate_search_budget_applies_across_pages() {
        let mut total = 0;
        account_search_bytes(&mut total, 4, 8).unwrap();
        account_search_bytes(&mut total, 4, 8).unwrap();
        assert!(account_search_bytes(&mut total, 1, 8)
            .unwrap_err()
            .to_string()
            .contains("total byte limit"));
        assert_eq!(total, 8);
        let mut overflowing = usize::MAX;
        assert!(account_search_bytes(&mut overflowing, 1, usize::MAX).is_err());
    }

    #[test]
    fn urls_keep_only_absolute_web_links() {
        let value = json!([
            "https://ted.europa.eu/en/notice/1",
            "http://ted.europa.eu/nl/notice/2",
            "http-evil:boom",
            "http:foo",
            "javascript:alert(1)",
            "mailto:buyer@example.com",
            "https://ted.europa.eu/en/notice/1",
        ]);
        assert_eq!(
            urls(Some(&value)),
            [
                "https://ted.europa.eu/en/notice/1",
                "http://ted.europa.eu/nl/notice/2"
            ]
        );
    }

    /// Pins the wire contract: these names are the ones TED answered 200 for on
    /// 2026-10-18. Re-verify against the live endpoint before changing this
    /// list; the aliases it replaced made every search fail with HTTP 400.
    #[cfg(feature = "network")]
    #[test]
    fn requests_only_ted_verified_fields() {
        assert_eq!(
            TENDER_FIELDS,
            &[
                "publication-number",
                "publication-date",
                "notice-title",
                "buyer-name",
                "description-proc",
                "classification-cpv",
                "place-of-performance",
                "estimated-value-proc",
                "estimated-value-glo",
                "estimated-value-lot",
                "estimated-value-cur-lot",
                "framework-maximum-value-lot",
                "framework-maximum-value-cur-lot",
                "identifier-lot",
                "main-classification-lot",
                "framework-maximum-value-glo",
                "total-value",
                "deadline-receipt-tender-date-lot",
                "deadline-receipt-request",
                "notice-type",
                "official-language",
                "links",
                "selection-criterion-name-lot",
                "selection-criterion-description-lot",
                "exclusion-grounds-description",
                "contract-conditions-description-lot",
                "guarantee-required-description-lot",
            ]
        );
    }

    /// The normalizers can only read what the search actually requests, so the
    /// primary key of every mapped field must stay in the list.
    #[cfg(feature = "network")]
    #[test]
    fn requests_fields_the_normalizers_read() {
        for field in [
            "publication-number",
            "notice-title",
            "buyer-name",
            "description-proc",
            "classification-cpv",
            "place-of-performance",
            "estimated-value-proc",
            "estimated-value-lot",
            "estimated-value-cur-lot",
            "framework-maximum-value-lot",
            "framework-maximum-value-cur-lot",
            "identifier-lot",
            "main-classification-lot",
            "classification-cpv",
            "deadline-receipt-tender-date-lot",
            "deadline-receipt-request",
            "notice-type",
            "official-language",
            "links",
            "selection-criterion-description-lot",
            "exclusion-grounds-description",
        ] {
            assert!(TENDER_FIELDS.contains(&field), "missing field {field}");
        }
    }

    /// The importer must ask for a population, not a single issue: a `LATEST`
    /// window is one OJ S issue and cannot backfill (a historical date range
    /// with `LATEST` returns nothing).
    #[test]
    fn default_window_is_the_active_lookback_not_one_issue() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
        let window = SearchWindow::recent(DEFAULT_LOOKBACK_DAYS, today);
        assert_eq!(window.scope, SearchScope::Active);
        assert_eq!(window.scope.as_str(), "ACTIVE");
        assert_eq!(
            window.published_since,
            Some(NaiveDate::from_ymd_opt(2026, 9, 18).unwrap())
        );
        assert_eq!(
            window.query_for("(buyer-country=BEL OR place-of-performance=BEL)"),
            "((buyer-country=BEL OR place-of-performance=BEL)) AND publication-date>=20260918"
        );
        // A caller's unparenthesised OR must not leak into the date bound.
        assert_eq!(
            window.query_for("buyer-country=BEL OR place-of-performance=BEL"),
            "(buyer-country=BEL OR place-of-performance=BEL) AND publication-date>=20260918"
        );
        assert_eq!(window.with_scope(SearchScope::All).scope.as_str(), "ALL");
        assert_eq!(SearchWindow::recent(0, today).published_since, None);
        assert_eq!(
            SearchWindow::recent(0, today).query_for("buyer-country=BEL"),
            "buyer-country=BEL"
        );
    }

    /// An incremental refresh narrows the window to the overlap, and widens it back
    /// out when the store was idle longer than the ordinary window — the case where
    /// narrowing would leave a hole. A watermark in the future is not trusted.
    #[test]
    fn incremental_floor_narrows_by_overlap_and_never_hides_a_gap() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let days_ago = |days: i64| today - ChronoDuration::days(days);
        // No recorded run: the ordinary full window.
        assert_eq!(incremental_floor(30, today, None, 7), Some(days_ago(30)));
        // Ran yesterday: the overlap is re-read, not the whole window.
        assert_eq!(
            incremental_floor(30, today, Some(days_ago(1)), 7),
            Some(days_ago(8))
        );
        // Down for longer than the ordinary window: the floor reaches back to the
        // watermark, so the idle months are fetched instead of skipped.
        assert_eq!(
            incremental_floor(30, today, Some(days_ago(60)), 7),
            Some(days_ago(67)),
            "the gap between a stale watermark and today must be part of the window"
        );
        // A live run announced a seven-day overlap while asking TED for the
        // two-day window, because the watermark was clamped up to it.
        assert_eq!(
            incremental_floor(2, today, Some(today), 7),
            Some(days_ago(7)),
            "a run this morning must still re-read the overlap it announces"
        );
        // A watermark in the future is a clock disagreement, not coverage.
        assert_eq!(
            incremental_floor(30, today, Some(today + ChronoDuration::days(2)), 7),
            Some(days_ago(30)),
            "a future watermark must not shrink the window"
        );
        // A deliberately wide overlap is honoured: it is a request to re-read that
        // much, not a suggestion to be capped by the window.
        assert_eq!(
            incremental_floor(3, today, Some(days_ago(1)), 30),
            Some(days_ago(31))
        );
        // Zero overlap is the watermark exactly, and no bound stays no bound.
        assert_eq!(
            incremental_floor(30, today, Some(days_ago(5)), 0),
            Some(days_ago(5))
        );
        // No window and no watermark: nothing bounds the walk, so nothing is returned.
        assert_eq!(incremental_floor(0, today, None, 7), None);
        // No window but a watermark: the watermark is the bound. This is how a caller
        // asks for "everything since my last run".
        assert_eq!(
            incremental_floor(0, today, Some(days_ago(1)), 7),
            Some(days_ago(8))
        );
    }

    /// The computed floor must reach TED's query: a window that is calculated
    /// but not sent would re-fetch the whole active population.
    #[test]
    fn incremental_window_bounds_the_query() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let since = incremental_floor(30, today, Some(today - ChronoDuration::days(1)), 7)
            .expect("a bounded window");
        let window = SearchWindow::rolling(Some(since), SearchScope::Active);
        assert_eq!(
            window.query_for("buyer-country=BEL"),
            "(buyer-country=BEL) AND publication-date>=20261029"
        );
    }

    /// Backfill chunks cover the requested range exactly, month by month, so the
    /// 64 MiB response budget is never asked for a whole year at once.
    #[test]
    fn month_chunks_cover_the_range_without_gaps_or_overlap() {
        let from = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let until = NaiveDate::from_ymd_opt(2024, 4, 10).unwrap();
        let chunks = month_chunks(from, until);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].0, from);
        assert_eq!(chunks[0].1, NaiveDate::from_ymd_opt(2024, 1, 31).unwrap());
        assert_eq!(chunks[1].0, NaiveDate::from_ymd_opt(2024, 2, 1).unwrap());
        assert_eq!(chunks[1].1, NaiveDate::from_ymd_opt(2024, 2, 29).unwrap());
        assert_eq!(chunks[3].0, NaiveDate::from_ymd_opt(2024, 4, 1).unwrap());
        assert_eq!(chunks[3].1, until);
        for pair in chunks.windows(2) {
            assert_eq!(pair[1].0, pair[0].1 + ChronoDuration::days(1));
        }
        // A single day, and a range crossing a year boundary.
        assert_eq!(month_chunks(from, from), vec![(from, from)]);
        let crossing = month_chunks(
            NaiveDate::from_ymd_opt(2024, 12, 20).unwrap(),
            NaiveDate::from_ymd_opt(2025, 1, 5).unwrap(),
        );
        assert_eq!(crossing.len(), 2);
        assert_eq!(crossing[1].0, NaiveDate::from_ymd_opt(2025, 1, 1).unwrap());
    }

    #[test]
    fn a_range_window_bounds_both_ends() {
        let window = SearchWindow::range(
            NaiveDate::from_ymd_opt(2024, 8, 1).unwrap(),
            NaiveDate::from_ymd_opt(2024, 8, 31).unwrap(),
        );
        assert_eq!(window.scope, SearchScope::All);
        assert_eq!(
            window.query_for("buyer-country=BEL"),
            "(buyer-country=BEL) AND publication-date>=20240801 AND publication-date<=20240831"
        );
        // A lower bound alone keeps the previous shape.
        let open_ended = SearchWindow::recent(30, NaiveDate::from_ymd_opt(2026, 10, 18).unwrap());
        assert_eq!(
            open_ended.query_for("buyer-country=BEL"),
            "(buyer-country=BEL) AND publication-date>=20260918"
        );
    }

    #[test]
    fn scope_names_round_trip_and_reject_unknown_values() {
        for scope in [SearchScope::Latest, SearchScope::Active, SearchScope::All] {
            assert_eq!(SearchScope::parse(scope.as_str()).unwrap(), scope);
        }
        assert!(SearchScope::parse("everything").is_err());
        assert_eq!(SearchScope::parse(" active ").unwrap(), SearchScope::Active);
    }

    /// The search must send the window it was asked for; a silent fallback to
    /// one issue is the defect this replaces.
    #[cfg(feature = "network")]
    #[test]
    fn search_request_carries_scope_and_publication_bound() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let body = r#"{"notices":[]}"#;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("Content-Length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
            }
            let mut request_body = vec![0; length];
            reader.read_exact(&mut request_body).unwrap();
            write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            String::from_utf8(request_body).unwrap()
        });
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        let window = SearchWindow::recent(7, NaiveDate::from_ymd_opt(2026, 10, 18).unwrap());
        client
            .search_raw_window("buyer-country=BEL", &["publication-number"], 5, &window)
            .unwrap();
        let sent = server.join().unwrap();
        assert!(
            sent.contains("\"scope\":\"ACTIVE\""),
            "scope missing: {sent}"
        );
        assert!(
            sent.contains("publication-date>=20261011"),
            "publication bound missing: {sent}"
        );
        assert!(
            !sent.contains("LATEST"),
            "must not fall back to one issue: {sent}"
        );
    }

    #[test]
    fn notice_type_is_normalized_and_missing_types_stay_unknown() {
        let notice = json!({
            "publication-number": "1",
            "notice-title": "Works",
            "notice-type": "  cn-standard "
        });
        assert_eq!(
            notice_to_tender(&notice).notice_type.as_deref(),
            Some("cn-standard")
        );
        let without = json!({"publication-number": "2", "notice-title": "Works"});
        assert_eq!(notice_to_tender(&without).notice_type, None);
    }

    /// The publication date is what lets the store report coverage as a range;
    /// an absent or malformed value must stay `None` rather than inventing one.
    #[test]
    fn publication_date_is_mapped_and_validated() {
        let notice = notice_to_tender(&json!({
            "publication-number": "1",
            "notice-title": "Works",
            "publication-date": "2026-10-16"
        }));
        assert_eq!(notice.publication_date.as_deref(), Some("2026-10-16"));
        let malformed = notice_to_tender(&json!({
            "publication-number": "2",
            "notice-title": "Works",
            "publication-date": "16/10/2026"
        }));
        assert_eq!(malformed.publication_date, None);
        let missing =
            notice_to_tender(&json!({"publication-number": "3", "notice-title": "Works"}));
        assert_eq!(missing.publication_date, None);
    }

    /// The notice language comes back as a three-letter code; it has to reach
    /// the matcher in the form its alias table is keyed by, or the language
    /// bonus and penalty can never fire.
    #[test]
    fn official_language_becomes_matchable_language_codes() {
        let dutch = notice_to_tender(&json!({
            "publication-number": "1",
            "notice-title": "Werken",
            "official-language": ["NLD"]
        }));
        assert_eq!(dutch.languages, vec!["nld"]);
        let french = notice_to_tender(&json!({
            "publication-number": "2",
            "notice-title": "Travaux",
            "official-language": {"MUL": "FRA"}
        }));
        assert_eq!(french.languages, vec!["fra"]);
    }

    /// Award history must search the whole archive: the ingest window is a
    /// recent-publication window, and confining a `winner-identifier` search to
    /// it silently returns no awards for contracts won a year earlier.
    #[test]
    fn history_window_drops_the_publication_bound_and_uses_all() {
        let window = SearchWindow::history();
        assert_eq!(window.scope, SearchScope::All);
        assert_eq!(window.scope.as_str(), "ALL");
        assert_eq!(window.published_since, None);
        assert_eq!(
            window.query_for("notice-type=can-standard AND winner-identifier=0429366144"),
            "notice-type=can-standard AND winner-identifier=0429366144"
        );
    }

    /// The history window must reach the request as scope ALL with no date
    /// bound, not as the importer's recent window.
    #[cfg(feature = "network")]
    #[test]
    fn history_search_sends_no_publication_bound() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let body = r#"{"notices":[]}"#;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("Content-Length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
            }
            let mut request_body = vec![0; length];
            reader.read_exact(&mut request_body).unwrap();
            write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            String::from_utf8(request_body).unwrap()
        });
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        client
            .search_raw_window(
                "notice-type=can-standard AND winner-identifier=0429366144",
                &["publication-number"],
                5,
                &SearchWindow::history(),
            )
            .unwrap();
        let sent = server.join().unwrap();
        assert!(sent.contains("\"scope\":\"ALL\""), "scope: {sent}");
        assert!(
            !sent.contains("publication-date"),
            "history search must not be date-bounded: {sent}"
        );
    }

    /// A windowed refresh fetches tens of pages, so a throttled page must be
    /// retried instead of failing the whole ingest.
    #[cfg(feature = "network")]
    #[test]
    fn retries_a_throttled_page_then_succeeds() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let body = r#"{"notices":[{"publication-number":"1"}]}"#;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for (index, status) in ["429 Too Many Requests", "200 OK"].iter().enumerate() {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("Content-Length") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                }
                let mut request_body = vec![0; length];
                reader.read_exact(&mut request_body).unwrap();
                let payload = if index == 0 { "" } else { body };
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                )
                .unwrap();
            }
        });
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(10),
        };
        let notices = client.search_raw("buyer-country=BEL", &[], 1).unwrap();
        server.join().unwrap();
        assert_eq!(notices.len(), 1);
    }

    /// The diagnosis for a walk that dies at the wall: which page, how much was
    /// collected, and that the range is what has to change. A failure before the wall
    /// is left alone, because there the bare status is the whole story — and so is a
    /// failure the provider never answered, which the wall cannot explain.
    #[test]
    fn a_failure_deep_in_the_walk_names_the_page_and_the_way_forward() {
        let answered = TedFetchError {
            message: "status code 400".to_owned(),
            provider_answered: true,
        };
        assert_eq!(
            page_wall_hint(PAGE_WALL_WARN_PAGE - 1, 390, &answered),
            None,
            "an early failure is not blamed on the wall"
        );
        let unanswered = TedFetchError {
            message: "connection reset".to_owned(),
            provider_answered: false,
        };
        assert_eq!(
            page_wall_hint(PAGE_WALL_WARN_PAGE + 1, 410, &unanswered),
            None,
            "a transport failure at the same depth is not the wall either"
        );
        let hint = page_wall_hint(PAGE_WALL_WARN_PAGE + 1, 410, &answered)
            .expect("a deep status failure is explained");
        assert!(
            hint.contains("page 41") && hint.contains("410 notice(s)"),
            "the hint names the page and what was collected: {hint}"
        );
        assert!(
            hint.contains("status code 400"),
            "the underlying failure is preserved: {hint}"
        );
        assert!(
            hint.contains("--from") && hint.contains("sixty pages"),
            "the hint says what to do and why: {hint}"
        );
        // Every caller can act on it: `ted-search` has no --from/--to, so advice that
        // named only that flag sent its operators looking for a flag it does not parse.
        assert!(
            hint.contains("--days"),
            "the hint also names the window flag every caller has: {hint}"
        );
    }

    #[test]
    fn audit_large_string_arrays_keep_first_seen_order() {
        let mut items: Vec<Value> = (0..10_000).map(|i| json!(format!("code-{i}"))).collect();
        items.extend([json!(["code-0", "code-9999"]), json!({"fra": "code-3"})]);
        let values = strings(Some(&Value::Array(items)));
        assert_eq!(values.len(), 10_000);
        for (i, value) in values.iter().enumerate() {
            assert_eq!(value, &format!("code-{i}"));
        }
    }

    #[test]
    fn empty_preferred_language_falls_back_to_all_values() {
        assert_eq!(
            strings(Some(&json!({"fra": "", "other": "Gent"}))),
            ["", "Gent"]
        );
    }

    #[test]
    fn null_amount_falls_back_to_value() {
        assert_eq!(
            decimal(Some(&json!({"amount": Value::Null, "value": "100"}))),
            Some(Decimal::new(100, 0))
        );
        assert_eq!(
            decimal(Some(&json!({"amount": "", "value": "100"}))),
            Some(Decimal::new(100, 0))
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn skips_malformed_notices_in_batches() {
        let notices = vec![
            json!({"publication-number": "valid-1"}),
            json!({"notice-title": "Missing id"}),
        ];
        let tenders = convert_notices(&notices);
        assert_eq!(tenders.len(), 1);
        assert_eq!(tenders[0].external_id, "valid-1");
    }

    #[test]
    fn rejects_malformed_notices() {
        assert!(try_notice_to_tender(&json!({"notice-title": "Missing id"})).is_err());
        assert!(try_notice_to_tender(&json!("not an object")).is_err());
    }

    #[test]
    fn rejects_invalid_iso_dates() {
        assert_eq!(date(Some(&json!("2026-02-30"))), None);
        assert_eq!(date(Some(&json!("2026-13-01"))), None);
    }

    #[test]
    fn normalizes_multilingual_notice() {
        let notice = json!({
            "publication-number": "123456-2026",
            "notice-title": {"eng": "Electrical works", "fra": "Travaux électriques"},
            "buyer-name": {"eng": "City of Ghent"},
            "classification-cpv": ["45310000", "45311000"],
            "place-of-performance": ["BE234"],
            "estimated-value": {"amount": "65000.00"},
            "deadline-receipt-tender": "2026-10-15",
            "selection-criterion-description-lot": {"nld": ["ISO 9001 vereist"]},
            "links": ["https://ted.europa.eu/notice/123456-2026"]
        });
        let tender = notice_to_tender(&notice);
        assert_eq!(tender.external_id, "123456-2026");
        assert_eq!(tender.title, "Electrical works");
        assert_eq!(tender.estimated_value, Some(Decimal::new(65_000, 0)));
        assert_eq!(tender.deadline.as_deref(), Some("2026-10-15"));
        assert_eq!(tender.cpv_codes, vec!["45310000", "45311000"]);
        assert_eq!(tender.requirements.len(), 1);
        assert_eq!(tender.requirements[0].kind, "selection");
    }

    #[cfg(feature = "network")]
    #[test]
    fn rejects_oversized_search_response() {
        let body = vec![b'{'; MAX_TED_RESPONSE_BYTES + 1];
        let error = parse_search_response_body(&body)
            .expect_err("oversized TED response should be rejected");
        assert!(error.to_string().contains("exceeded"));
    }

    #[cfg(feature = "network")]
    #[test]
    fn rejects_oversized_search_limit() {
        let error = TedClient::default()
            .search_raw("", &[], MAX_TED_SEARCH_RESULTS + 1)
            .expect_err("oversized TED search should be rejected");
        assert!(error.to_string().contains("must not exceed"));
    }

    #[test]
    fn sums_lot_values_and_deduplicates_strings() {
        let notice = json!({
            "publication-number": "1",
            "notice-title": "Works",
            "estimated-value": [{"amount": "10"}, {"amount": "20"}],
            "classification-cpv": ["45", "45"]
        });
        let tender = notice_to_tender(&notice);
        assert_eq!(tender.estimated_value, Some(Decimal::new(30, 0)));
        assert_eq!(tender.cpv_codes, vec!["45"]);
    }

    #[test]
    fn lot_values_parse_in_order_without_summing() {
        let notice = json!({
            "publication-number": "654610-2026",
            "notice-title": "Soil analyses",
            "estimated-value-proc": "1238890",
            "estimated-value-lot": ["384390", "164580", "181400", "200600", "307920"],
            "framework-maximum-value-lot": ["1", "2"],
        });
        let tender = notice_to_tender(&notice);
        // Estimated lots win over framework maxima: mixing the two arrays
        // would double-count the lots.
        assert_eq!(
            tender.lot_values,
            vec![
                Decimal::new(384_390, 0),
                Decimal::new(164_580, 0),
                Decimal::new(181_400, 0),
                Decimal::new(200_600, 0),
                Decimal::new(307_920, 0),
            ]
        );
        assert_eq!(tender.assessed_value(), Some(Decimal::new(164_580, 0)));
    }

    #[test]
    fn ted_lot_evidence_keeps_only_singleton_values_and_preserves_cardinality() {
        let single = notice_to_tender(&json!({
            "publication-number": "single-lot",
            "identifier-lot": ["LOT-1"],
            "main-classification-lot": ["45233100"],
            "estimated-value-lot": ["123.45"],
            "estimated-value-cur-lot": ["EUR"],
            "framework-maximum-value-lot": ["250.00"],
            "framework-maximum-value-cur-lot": ["EUR"],
        }));
        let evidence = &single.raw["notice_lot_ted_evidence"];
        assert_eq!(
            evidence["association_rule"],
            "singleton_field_cardinality_only_no_positional_join"
        );
        assert_eq!(evidence["fields"]["lot_id"]["source_cardinality"], 1);
        assert_eq!(evidence["fields"]["lot_id"]["singleton_value"], "LOT-1");
        assert_eq!(
            evidence["fields"]["estimated_value"]["singleton_value"],
            "123.45"
        );
        assert_eq!(
            evidence["fields"]["framework_maximum"]["singleton_value"],
            "250.00"
        );

        let multiple = notice_to_tender(&json!({
            "publication-number": "multi-lot",
            "identifier-lot": ["LOT-1", "LOT-2"],
            "main-classification-lot": ["45233100", "45233200"],
            "estimated-value-lot": ["100.00", "200.00"],
            "estimated-value-cur-lot": ["EUR"],
        }));
        let fields = &multiple.raw["notice_lot_ted_evidence"]["fields"];
        for field in ["lot_id", "main_classification", "estimated_value"] {
            assert_eq!(fields[field]["status"], "multiple_values");
            assert!(fields[field]["singleton_value"].is_null());
        }
        assert_eq!(fields["estimated_currency"]["source_cardinality"], 1);
        assert_eq!(multiple.raw["identifier-lot"], json!(["LOT-1", "LOT-2"]));
    }

    #[test]
    fn framework_lots_fill_in_only_when_estimated_absent() {
        let notice = json!({
            "publication-number": "2",
            "notice-title": "Framework",
            "framework-maximum-value-lot": ["100000", "200000"],
        });
        let tender = notice_to_tender(&notice);
        assert_eq!(
            tender.lot_values,
            vec![Decimal::new(100_000, 0), Decimal::new(200_000, 0)]
        );
    }
}
