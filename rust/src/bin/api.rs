use aanbestedingen_matcher::analysis::{
    analysis_cache_key, analyze, AnalysisDecision, Analyzer, ConfiguredAnalyzer,
    DETERMINISTIC_CACHE_IDENTITY,
};
use aanbestedingen_matcher::kbo::{normalize_enterprise_number, KboRepository};
use aanbestedingen_matcher::recognition;
use aanbestedingen_matcher::recognition_catalog::{CATEGORIES, CLASS_AMOUNT_GUIDANCE};
use aanbestedingen_matcher::recognized_contractors::{
    compare_requirement_to_approvals, RecognitionComparison, RecognitionEvidenceStatus,
    RecognitionSnapshot,
};
use aanbestedingen_matcher::recommendations::{
    apply_capacity_triage, apply_jev_decision, apply_prospect_decision,
    company_matches_region_filter, is_eligible, nace_prefixes_for_tender, rank, rank_prospects,
    sort_by_deadline, sort_by_score, sort_prospects, CapacityLimits, CompanyProspect,
    ProspectContext, DEFAULT_RECOMMENDATION_CANDIDATES, MAX_PROSPECT_CANDIDATES,
    MAX_RECOMMENDATION_CANDIDATES,
};
use aanbestedingen_matcher::store::{
    nbb_capacity_turnover, operator_label, validate_feedback_outcome as validate_outcome_value,
    FeedbackLabel, FeedbackUpdate, FieldUpdate, RefreshRun, TenderStore,
};
use aanbestedingen_matcher::{
    validate_value_limits, AwardRecord, CompanyPreferences, CompanyProfile, Tender,
};
use axum::{
    async_trait,
    extract::{
        rejection::JsonRejection, DefaultBodyLimit, FromRequest, Path, Query, Request, State,
    },
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use chrono::{NaiveDate, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    env,
    path::{Path as FilePath, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tower_http::services::ServeDir;

const MAX_ANALYSIS_CACHE_ENTRIES: usize = 256;
// Per API process; busy/duplicate misses fail fast instead of queuing blocking
// workers or multiplying paid provider calls. Cached requests remain available.
const MAX_CONCURRENT_ANALYSES: usize = 2;
const MAX_API_BODY_BYTES: usize = 64 * 1024;
/// A refresh older than this makes the recommendation data stale: the daily
/// window is 180 days, so a week without a refresh already means the newest
/// notices are missing.
const MAX_REFRESH_AGE_DAYS: i64 = 7;
const DEFAULT_WEB_DIR: &str = "web";

#[derive(Debug, PartialEq)]
struct ApiOptions {
    kbo_database: String,
    tender_database: String,
    address: String,
    /// `None` means "resolve the dashboard directory" (see
    /// [`resolve_web_dir`]); `Some` is an explicit `--web-dir`.
    web_dir: Option<String>,
    recognition_snapshot: Option<String>,
}

fn parse_args<I>(args: I) -> Result<Option<ApiOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut tender_database = "data/tenders-rust.duckdb".to_owned();
    let mut address = "127.0.0.1:3000".to_owned();
    let mut web_dir = None;
    let mut recognition_snapshot = None;
    let mut arguments = args.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?
            }
            "--tender-database" => {
                tender_database = arguments.next().ok_or("--tender-database needs a value")?
            }
            "--address" => address = arguments.next().ok_or("--address needs a value")?,
            "--web-dir" => web_dir = Some(arguments.next().ok_or("--web-dir needs a value")?),
            "--recognition-snapshot" => {
                recognition_snapshot = Some(
                    arguments
                        .next()
                        .ok_or("--recognition-snapshot needs a path")?,
                )
            }
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }

    Ok(Some(ApiOptions {
        kbo_database,
        tender_database,
        address,
        web_dir,
        recognition_snapshot,
    }))
}

/// The directory that serves the dashboard.
///
/// The Python reference resolves `web/` relative to its own source tree, so it
/// works from any working directory. A compiled binary has no source tree, so
/// the documented invocation (run from the repository root) is tried first and
/// `--web-dir` covers every other case. The remaining candidates recover the
/// checkout that produced the binary: walking up from the executable
/// (`rust/target/<profile>/api`) covers a build inside the checkout, and the
/// manifest directory Cargo compiled in covers `CARGO_TARGET_DIR` pointing
/// outside it.
fn resolve_web_dir(explicit: Option<String>) -> String {
    if let Some(directory) = explicit {
        return directory;
    }
    if FilePath::new(DEFAULT_WEB_DIR).is_dir() {
        return DEFAULT_WEB_DIR.to_owned();
    }
    checkout_web_dir()
        .map(|directory| directory.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_WEB_DIR.to_owned())
}

/// `web/` in the checkout that produced this binary, if it can be found.
fn checkout_web_dir() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(executable) = env::current_exe() {
        roots.extend(executable.ancestors().skip(1).map(FilePath::to_path_buf));
    }
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        if let Some(root) = FilePath::new(manifest).parent() {
            roots.push(root.to_path_buf());
        }
    }
    roots.into_iter().find_map(|root| {
        if !is_checkout_root(&root) {
            return None;
        }
        Some(root.join(DEFAULT_WEB_DIR))
    })
}

/// True when `root` is this repository's checkout: the dashboard, the
/// `justfile` and the Rust manifest must all be present, so an unrelated Rust
/// project that happens to contain a `web/` directory is not mistaken for it.
fn is_checkout_root(root: &FilePath) -> bool {
    root.join(DEFAULT_WEB_DIR).is_dir()
        && root.join("justfile").is_file()
        && root.join("rust").join("Cargo.toml").is_file()
}

#[derive(Default)]
struct AnalysisCache {
    entries: HashMap<String, AnalysisDecision>,
    order: VecDeque<String>,
}

impl AnalysisCache {
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn get(&mut self, key: &str) -> Option<AnalysisDecision> {
        let decision = self.entries.get(key).cloned();
        if decision.is_some() {
            self.touch(key);
        }
        decision
    }

    fn insert(&mut self, key: String, decision: AnalysisDecision) -> bool {
        if decision.validate_size().is_err() {
            return false;
        }
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), decision);
            self.touch(&key);
            return false;
        }
        let evicted = if self.entries.len() >= MAX_ANALYSIS_CACHE_ENTRIES {
            self.order
                .pop_front()
                .and_then(|evicted_key| self.entries.remove(&evicted_key))
                .is_some()
        } else {
            false
        };
        self.entries.insert(key.clone(), decision);
        self.order.push_back(key);
        evicted
    }

    fn touch(&mut self, key: &str) {
        if let Some(position) = self.order.iter().position(|entry| entry == key) {
            self.order.remove(position);
        }
        self.order.push_back(key.to_owned());
    }
}

#[derive(Clone, Default)]
struct AnalysisAdmission {
    active: Arc<Mutex<HashSet<String>>>,
}

struct AnalysisPermit {
    admission: AnalysisAdmission,
    key: String,
}

impl AnalysisAdmission {
    fn try_enter(&self, key: &str) -> Result<AnalysisPermit, ApiError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| ApiError::internal("analysis admission is unavailable"))?;
        if active.contains(key) {
            return Err(ApiError::service_unavailable(
                "analysis already in progress; retry later",
            ));
        }
        if active.len() >= MAX_CONCURRENT_ANALYSES {
            return Err(ApiError::service_unavailable(
                "analysis capacity reached; retry later",
            ));
        }
        active.insert(key.to_owned());
        Ok(AnalysisPermit {
            admission: self.clone(),
            key: key.to_owned(),
        })
    }
}

impl Drop for AnalysisPermit {
    fn drop(&mut self) {
        self.admission
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.key);
    }
}

/// One shared miss path for direct analysis and deep reranking. The permit
/// covers computation, persistence, and memory publication, closing the window
/// in which another miss could launch an identical request before caching it.
fn analyze_cache_miss(
    state: &ApiState,
    key: &str,
    compute: impl FnOnce() -> Result<AnalysisDecision, ApiError>,
    persist: impl FnOnce(&AnalysisDecision) -> Result<(), ApiError>,
) -> Result<AnalysisDecision, ApiError> {
    let _permit = state.analysis_admission.try_enter(key)?;
    // Do not remove this recheck: it closes the gap where a prior request
    // completed and published after the caller's initial cache miss.
    if let Some(mut decision) = state
        .analysis_cache
        .lock()
        .map_err(|_| ApiError::internal("analysis cache is unavailable"))?
        .get(key)
    {
        state
            .cache_metrics
            .memory_hits
            .fetch_add(1, Ordering::Relaxed);
        decision.cached = true;
        return Ok(decision);
    }
    state.cache_metrics.misses.fetch_add(1, Ordering::Relaxed);
    let decision = compute()?;
    decision.validate_size().map_err(ApiError::bad_gateway)?;
    persist(&decision)?;
    if state
        .analysis_cache
        .lock()
        .map_err(|_| ApiError::internal("analysis cache is unavailable"))?
        .insert(key.to_owned(), decision.clone())
    {
        state
            .cache_metrics
            .evictions
            .fetch_add(1, Ordering::Relaxed);
    }
    Ok(decision)
}

#[derive(Default)]
struct CacheMetrics {
    memory_hits: AtomicU64,
    persistent_hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

#[derive(Clone)]
struct ApiState {
    kbo_database: String,
    tender_database: String,
    analysis: Option<ConfiguredAnalyzer>,
    analysis_admission: AnalysisAdmission,
    analysis_cache: Arc<Mutex<AnalysisCache>>,
    cache_metrics: Arc<CacheMetrics>,
    api_token: Option<String>,
    recognition_snapshot: Option<Arc<RecognitionSnapshot>>,
}

/// Normalize the optional API token: unset, empty, or whitespace-only means
/// no token is configured. Returns the trimmed token.
fn normalize_api_token(raw: Option<String>) -> Option<String> {
    raw.map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
}

/// Refuse to serve anything but a loopback address without a token.
///
/// Without `AANBESTEDINGEN_API_TOKEN` the preferences, feedback and analysis
/// proxy routes are open to anyone who can reach the port. That is acceptable
/// for a loopback-only prototype and not for any other interface, and relying
/// on the operator to remember an environment variable is precisely the
/// mistake this check prevents. Hostnames are treated as non-loopback, because
/// what they resolve to is not knowable here.
fn check_bind_address(
    address: &str,
    token: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let loopback = address
        .parse::<std::net::SocketAddr>()
        .map(|parsed| parsed.ip().is_loopback())
        .unwrap_or(false);
    if !loopback && token.is_none() {
        return Err(format!(
            "refusing to bind {address} without AANBESTEDINGEN_API_TOKEN: without it the \
             preferences, feedback and analysis-proxy routes are open to anyone who can reach \
             this port. Bind 127.0.0.1 instead, or set the token."
        )
        .into());
    }
    Ok(())
}

/// Constant-time bearer comparison so the token cannot be guessed byte by
/// byte from response timing.
fn bearer_matches(header_value: &str, expected: &str) -> bool {
    let mut candidate = String::with_capacity("Bearer ".len() + expected.len());
    candidate.push_str("Bearer ");
    candidate.push_str(expected);
    let provided = header_value.as_bytes();
    let expected = candidate.as_bytes();
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .iter()
        .zip(expected.iter())
        .fold(0u8, |accumulated, (left, right)| {
            accumulated | (left ^ right)
        })
        == 0
}

fn is_authorized(headers: &header::HeaderMap, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| bearer_matches(value, expected))
}

async fn require_token(
    State(state): State<Arc<ApiState>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if !is_authorized(request.headers(), state.api_token.as_deref()) {
        return Err(ApiError::unauthorized());
    }
    Ok(next.run(request).await)
}

#[derive(Debug, Deserialize)]
struct InventoryQuery {
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct RecommendationQuery {
    limit: Option<usize>,
    candidates: Option<usize>,
    sort: Option<String>,
    due_within: Option<i64>,
    deep: Option<i64>,
    annual_turnover: Option<f64>,
    max_bid_capacity: Option<f64>,
    capacity_triage: Option<bool>,
    max_filing_age_days: Option<i64>,
    include_activity_text_similarity: Option<bool>,
    include_geographic_evidence: Option<bool>,
    include_company_kbo_evidence: Option<bool>,
    include_company_nbb_evidence: Option<bool>,
    include_lot_features: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PreferencesRequest {
    #[serde(default)]
    confirmed_activities: Vec<String>,
    #[serde(default)]
    excluded_terms: Vec<String>,
    #[serde(default)]
    preferred_regions: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
    minimum_value: Option<Decimal>,
    maximum_value: Option<Decimal>,
    /// Belgian works recognition class (1–8) and categories (`C`, `D17`); used
    /// only to compare with what a notice states, never to decide eligibility.
    #[serde(default, deserialize_with = "deserialize_optional_u8")]
    recognition_class: Option<u8>,
    #[serde(default)]
    recognition_categories: Vec<String>,
}

/// Accepts `"5"` as well as `5` for the recognition class, because an HTML
/// number input always hands the dashboard a string.
fn deserialize_optional_u8<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrText {
        Number(u64),
        Text(String),
    }
    match Option::<NumberOrText>::deserialize(deserializer)? {
        None => Ok(None),
        Some(NumberOrText::Number(value)) => u8::try_from(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(NumberOrText::Text(value)) => match value.trim() {
            "" => Ok(None),
            text => text
                .parse::<u8>()
                .map(Some)
                .map_err(serde::de::Error::custom),
        },
    }
}

#[derive(Debug, Deserialize)]
struct FeedbackRequest {
    source: String,
    external_id: String,
    relevant: bool,
    #[serde(default)]
    pursuable: FieldUpdate<bool>,
    #[serde(default)]
    outcome: FieldUpdate<String>,
    #[serde(default)]
    reason: FieldUpdate<String>,
}

fn validate_feedback_outcome(value: &FieldUpdate<String>) -> Result<(), ApiError> {
    match value {
        FieldUpdate::Set(outcome) => {
            validate_outcome_value(outcome).map_err(ApiError::unprocessable)
        }
        _ => Ok(()),
    }
}

fn normalized_enterprise_number(value: &str) -> Result<String, ApiError> {
    normalize_enterprise_number(value).map_err(ApiError::unprocessable)
}

fn feedback_target(request: &FeedbackRequest) -> Result<(String, String), ApiError> {
    let source = request.source.trim();
    if source.is_empty() {
        return Err(ApiError::unprocessable("feedback source must not be empty"));
    }
    let external_id = request.external_id.trim();
    if external_id.is_empty() {
        return Err(ApiError::unprocessable(
            "feedback external_id must not be empty",
        ));
    }
    Ok((source.to_owned(), external_id.to_owned()))
}

fn load_recognition_snapshot(
    path: Option<&str>,
) -> Result<Option<Arc<RecognitionSnapshot>>, Box<dyn std::error::Error>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let bytes = std::fs::read(path)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let schema_version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or("recognition snapshot is missing an integer schema_version")?;
    if !matches!(schema_version, 2 | 3) {
        return Err(
            format!("unsupported recognition snapshot schema version {schema_version}").into(),
        );
    }
    let snapshot: RecognitionSnapshot = serde_json::from_value(value)?;
    NaiveDate::parse_from_str(&snapshot.kbo_snapshot_as_of, "%Y-%m-%d")?;
    NaiveDate::parse_from_str(&snapshot.register_as_of, "%Y-%m-%d")?;
    eprintln!(
        "loaded {} recognition approval rows (register as of {}; KBO snapshot as of {})",
        snapshot.approval_rows.len(),
        snapshot.register_as_of,
        snapshot.kbo_snapshot_as_of
    );
    Ok(Some(Arc::new(snapshot)))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: api [--address HOST:PORT] [--kbo-database PATH] \
             [--tender-database PATH] [--web-dir PATH] [--recognition-snapshot PATH]"
        );
        return Ok(());
    };
    let ApiOptions {
        kbo_database,
        tender_database,
        address,
        web_dir,
        recognition_snapshot,
    } = options;
    let web_dir = resolve_web_dir(web_dir);
    let recognition_snapshot = load_recognition_snapshot(recognition_snapshot.as_deref())?;
    let api_token = normalize_api_token(env::var("AANBESTEDINGEN_API_TOKEN").ok());
    check_bind_address(&address, api_token.as_deref())?;

    let state = Arc::new(ApiState {
        kbo_database,
        tender_database,
        analysis: ConfiguredAnalyzer::from_env()?,
        analysis_admission: AnalysisAdmission::default(),
        analysis_cache: Arc::new(Mutex::new(AnalysisCache::default())),
        cache_metrics: Arc::new(CacheMetrics::default()),
        api_token,
        recognition_snapshot,
    });
    // Create and migrate the database once at startup so read endpoints can
    // use read-only connections without worrying about the file existing.
    TenderStore::open(&state.tender_database)?;
    // /health and the static dashboard stay open so status checks and the
    // login shell keep working; every /companies/... and /tenders/... route
    // requires the bearer token once AANBESTEDINGEN_API_TOKEN is set.
    let protected = Router::new()
        .route("/reference/recognition", get(recognition_reference))
        .route(
            "/companies/:enterprise_number",
            get(company).fallback(reference_not_found),
        )
        .route(
            "/companies/:enterprise_number/inventory-awards",
            get(inventory_awards).fallback(reference_not_found),
        )
        .route(
            "/companies/:enterprise_number/awards",
            get(company_awards).fallback(reference_not_found),
        )
        .route(
            "/companies/:enterprise_number/recommendations",
            get(recommendations).fallback(reference_not_found),
        )
        .route(
            "/companies/:enterprise_number/preferences",
            get(get_preferences)
                .put(save_preferences)
                .fallback(reference_not_found),
        )
        .route(
            "/companies/:enterprise_number/feedback",
            post(save_feedback).fallback(reference_not_found),
        )
        .route(
            "/tenders/:source/:external_id",
            get(tender_detail).fallback(reference_not_found),
        )
        .route(
            "/tenders/:source/:external_id/prospects",
            get(tender_prospects).fallback(reference_not_found),
        )
        .route(
            "/companies/:enterprise_number/tenders/:source/:external_id/analysis",
            get(tender_analysis).fallback(reference_not_found),
        )
        .route(
            "/companies/:enterprise_number/tenders/:source/:external_id/jev",
            get(tender_analysis).fallback(reference_not_found),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            require_token,
        ));
    let app = Router::new()
        .route("/health", get(health))
        .merge(protected)
        // A missing static file is a `detail`-bodied 404 like the reference's
        // StaticFiles mount instead of the empty 404 ServeDir returns.
        .fallback_service(ServeDir::new(&web_dir).not_found_service(get(reference_not_found)))
        .layer(DefaultBodyLimit::max(MAX_API_BODY_BYTES))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("Aanbestedingen Rust API listening on http://{address}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn recognition_reference() -> Json<Value> {
    Json(json!({
        "categories": CATEGORIES,
        "classes": CLASS_AMOUNT_GUIDANCE,
    }))
}

async fn run_blocking<T, F>(task: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(ApiError::internal)?
}

/// Whether the recommendation data should be trusted right now.
///
/// The refresh timestamp alone is not enough: a backfilled 2024 window fetched
/// yesterday looks fresh and offers 14 live opportunities, which is exactly what
/// an audit against that store tripped over. So the verdict also looks at what is
/// on offer (nothing open, or nothing whose publication date we can check) and at
/// how far the newest publication reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum FreshnessVerdict {
    Fresh,
    Stale,
    Unknown,
}

fn freshness_verdict(
    last_fetched: Option<&str>,
    open_procurements: i64,
    publication_range: (Option<&str>, Option<&str>, i64),
    total_tenders: i64,
    today: NaiveDate,
) -> FreshnessVerdict {
    if open_procurements == 0 {
        return FreshnessVerdict::Stale;
    }
    let (_, newest, unknown_rows) = publication_range;
    let parsed = |value: Option<&str>| {
        value.and_then(|value| NaiveDate::parse_from_str(value.get(..10)?, "%Y-%m-%d").ok())
    };
    // Freshness that cannot be verified must not read as fresh: if most of the
    // store predates the publication-date column, the only honest verdict is
    // "unknown", which callers treat the same as stale.
    let unknown_share = if total_tenders > 0 {
        unknown_rows as f64 / total_tenders as f64
    } else {
        1.0
    };
    if unknown_share > 0.5 {
        // Not stale, but not verifiable either: the store predates the
        // publication-date column, so nothing here can vouch for how recent the
        // notices are. Callers must not read that as fresh.
        return FreshnessVerdict::Unknown;
    }
    match (parsed(last_fetched), parsed(newest)) {
        (Some(fetched), Some(newest_publication)) => {
            if (today - fetched).num_days() > MAX_REFRESH_AGE_DAYS
                || (today - newest_publication).num_days() > MAX_REFRESH_AGE_DAYS
            {
                FreshnessVerdict::Stale
            } else {
                FreshnessVerdict::Fresh
            }
        }
        _ => FreshnessVerdict::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ServiceHealthStatus {
    Ok,
    Degraded,
}

fn health_status(kbo_database_readable: bool) -> ServiceHealthStatus {
    if kbo_database_readable {
        ServiceHealthStatus::Ok
    } else {
        ServiceHealthStatus::Degraded
    }
}

/// Readings of the tender store used by `/health`, kept as a struct because
/// the flat form grew past a readable tuple shape. `Default` is the
/// unreadable-database reading: zero counts and no timestamps.
#[derive(Default)]
struct StoreHealth {
    readable: bool,
    tenders: i64,
    tenders_last_fetched: Option<String>,
    tenders_publication_range: (Option<String>, Option<String>, i64),
    awards: i64,
    awards_last_fetched: Option<String>,
    award_lots: i64,
    award_lots_with_winner: i64,
    persistent_analysis_cache_entries: i64,
    /// `(open notices, open procurements)` — what a company can be matched
    /// against today, before and after the re-publication collapse.
    open_opportunities: (i64, i64),
    /// `(awards, companies with awards)`. Companies without history score the
    /// `history_match` term on its neutral default.
    award_history: (i64, i64),
    /// Which population the last ingest run drew from (scope, window, counts).
    /// `None` on databases written before provenance was recorded.
    last_refresh: Option<RefreshRun>,
}

impl StoreHealth {
    /// Read-only so health keeps answering while an ingest job in another
    /// process briefly holds the write lock; a long-running ingest degrades
    /// health instead of failing the endpoint. The reported cache count
    /// deliberately does not prune.
    fn read(database: &str) -> Result<Self, ApiError> {
        let Ok(store) = TenderStore::open_read_only(database) else {
            return Ok(Self::default());
        };
        let (tenders, tenders_last_fetched) = store.tender_stats().map_err(ApiError::internal)?;
        let tenders_publication_range = store
            .tender_publication_range()
            .map_err(ApiError::internal)?;
        let (awards, awards_last_fetched) = store.award_stats().map_err(ApiError::internal)?;
        let (award_lots, award_lots_with_winner) =
            store.award_lot_stats().map_err(ApiError::internal)?;
        let today = chrono::Utc::now().date_naive();
        let open_opportunities = store
            .open_opportunity_counts(today)
            .map_err(ApiError::internal)?;
        let award_history = store.award_history_coverage().map_err(ApiError::internal)?;
        let last_refresh = store.latest_refresh_run().map_err(ApiError::internal)?;
        let persistent_analysis_cache_entries =
            store.analysis_cache_len().map_err(ApiError::internal)?;
        Ok(Self {
            readable: true,
            tenders,
            tenders_last_fetched,
            tenders_publication_range,
            awards,
            awards_last_fetched,
            award_lots,
            award_lots_with_winner,
            persistent_analysis_cache_entries,
            open_opportunities,
            award_history,
            last_refresh,
        })
    }
}

/// Open the company register, mapping an outage (missing file, or the write
/// lock held by `kbo-rebuild`) to 503 like the tender store's read failures.
fn company_register(state: &ApiState) -> Result<KboRepository, ApiError> {
    KboRepository::open(&state.kbo_database).map_err(|error| {
        eprintln!("company register open failed: {error}");
        ApiError::service_unavailable("company register is unavailable")
    })
}

async fn health(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    let state = state.as_ref().clone();
    run_blocking(move || {
        let kbo_database_readable = KboRepository::open(&state.kbo_database)
            .and_then(|repository| repository.health_check())
            .is_ok();
        let store_health = StoreHealth::read(&state.tender_database)?;
        let status = health_status(
            kbo_database_readable && store_health.readable,
        );
        let analysis_cache_entries = state
            .analysis_cache
            .lock()
            .map_err(|_| ApiError::internal("analysis cache is unavailable"))?
            .len();
        Ok(Json(json!({
            "status": status,
            "kbo_database": FilePath::new(&state.kbo_database).exists(),
            "kbo_database_readable": kbo_database_readable,
            "tender_database_readable": store_health.readable,
            "tenders": store_health.tenders,
            "tenders_last_fetched": store_health.tenders_last_fetched,
            // Coverage, not freshness: the refresh timestamp says when we last
            // ran, these say how far the data reaches. `unknown_rows` counts
            // rows stored before the column existed.
            "tenders_oldest_publication_date": store_health.tenders_publication_range.0,
            "tenders_newest_publication_date": store_health.tenders_publication_range.1,
            "tenders_publication_unknown_rows": store_health.tenders_publication_range.2,
            // What a company can actually be matched against today. A store can
            // be large and still offer almost nothing, which is what an audit
            // against a stale store looked like: 2,889 notices, 23 open.
            "tenders_open_notices": store_health.open_opportunities.0,
            "tenders_open_procurements": store_health.open_opportunities.1,
            // Which population the last run drew from: a reader can tell a
            // 30-day refresh from a backfill. Null on stores written before
            // provenance was recorded.
            "last_refresh": store_health.last_refresh.as_ref().map(RefreshRun::as_json),
            "tenders_freshness": freshness_verdict(
                store_health.tenders_last_fetched.as_deref(),
                store_health.open_opportunities.1,
                (
                    store_health.tenders_publication_range.0.as_deref(),
                    store_health.tenders_publication_range.1.as_deref(),
                    store_health.tenders_publication_range.2,
                ),
                store_health.tenders,
                chrono::Utc::now().date_naive(),
            ),
            "awards": store_health.awards,
            // Without award history the 15% history term scores every company on
            // its neutral default, so the count of covered companies matters as
            // much as the number of awards.
            "award_history_companies": store_health.award_history.1,
            "awards_last_fetched": store_health.awards_last_fetched,
            "award_lots": store_health.award_lots,
            "award_lots_with_winner": store_health.award_lots_with_winner,
            "recognition_register": state.recognition_snapshot.as_ref().map(|snapshot| json!({
                "approval_rows": snapshot.approval_rows.len(),
                "exactly_matched_rows": snapshot.report.rows_exactly_matched,
                "unmatched_enterprises": snapshot.report.distinct_ids_unmatched,
                "register_as_of": snapshot.register_as_of,
                "kbo_snapshot_as_of": snapshot.kbo_snapshot_as_of,
            })),
            "analysis_provider": if let Some(ref analyzer) = state.analysis {
                analyzer.provider_name()
            } else {
                "deterministic-rust"
            },
            "analysis_model": state.analysis.as_ref().map(ConfiguredAnalyzer::model),
            "analysis_prompt_version": state.analysis.as_ref().map(ConfiguredAnalyzer::prompt_version),
            "analysis_timeout_seconds": state
                .analysis
                .as_ref()
                .map(ConfiguredAnalyzer::timeout_seconds),
            "analysis_cache_entries": analysis_cache_entries,
            "persistent_analysis_cache_entries": store_health.persistent_analysis_cache_entries,
            "analysis_cache_memory_hits": state.cache_metrics.memory_hits.load(Ordering::Relaxed),
            "analysis_cache_persistent_hits": state.cache_metrics.persistent_hits.load(Ordering::Relaxed),
            "analysis_cache_misses": state.cache_metrics.misses.load(Ordering::Relaxed),
            "analysis_cache_evictions": state.cache_metrics.evictions.load(Ordering::Relaxed),
        })))
    })
    .await
}

/// A company's own awarded contracts as recorded in the Brussels public-contract
/// inventories.
///
/// The inventories were imported but consulted by nothing: `inventory_awards`
/// held 5,304 accounting rows and no endpoint, report or evaluation read them.
/// This exposes what they say about one company — engagements, not accounting
/// rows, because one contract is repeated on every liquidation (a single €726
/// training appears on eight rows).
async fn inventory_awards(
    State(state): State<Arc<ApiState>>,
    Path(enterprise_number): Path<String>,
    Query(query): Query<InventoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let enterprise_number = normalized_enterprise_number(&enterprise_number)?;
    let limit = match query.limit {
        Some(0) => return Err(ApiError::unprocessable("limit must be at least 1")),
        Some(limit) if limit > 100 => {
            return Err(ApiError::unprocessable("limit must not exceed 100"))
        }
        Some(limit) => limit,
        None => 20,
    };
    let state = state.as_ref().clone();
    run_blocking(move || {
        let store =
            TenderStore::open_read_only(&state.tender_database).map_err(ApiError::internal)?;
        let engagements = store
            .inventory_engagements_for_awardee(&enterprise_number, limit)
            .map_err(ApiError::internal)?;
        let accounting_rows: i64 = engagements
            .iter()
            .map(|engagement| engagement.accounting_rows)
            .sum();
        Ok(Json(json!({
            "engagements": engagements,
            "accounting_rows": accounting_rows,
            // Say what an empty list means, because "no rows" here is not
            // "you never won anything": only the Brussels inventories are loaded.
            "note": if engagements.is_empty() {
                "no imported Brussels inventory records this company"
            } else {
                "awarded contracts as recorded in the imported Brussels inventories"
            },
        })))
    })
    .await
}

/// A company's tender award records from TED and BOSA. BOSA lot evidence is
/// returned without amounts: the import deliberately excludes values that
/// cannot be attributed reliably to an individual supplier.
async fn company_awards(
    State(state): State<Arc<ApiState>>,
    Path(enterprise_number): Path<String>,
    Query(query): Query<InventoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let enterprise_number = normalized_enterprise_number(&enterprise_number)?;
    let limit = match query.limit {
        Some(0) => return Err(ApiError::unprocessable("limit must be at least 1")),
        Some(limit) if limit > 100 => {
            return Err(ApiError::unprocessable("limit must not exceed 100"))
        }
        Some(limit) => limit,
        None => 20,
    };
    let state = state.as_ref().clone();
    run_blocking(move || {
        let store =
            TenderStore::open_read_only(&state.tender_database).map_err(ApiError::internal)?;
        let mut records = store
            .awards_for(&enterprise_number)
            .map_err(ApiError::internal)?;
        // Make the newly imported BOSA evidence visible before the bounded
        // response limit can be consumed by older TED history.
        records.sort_by_key(|award| {
            award.raw.get("source").and_then(Value::as_str) != Some("bosa")
        });
        let awards: Vec<Value> = records
            .into_iter()
            .take(limit)
            .map(|award| {
                let is_bosa = award.raw.get("source").and_then(Value::as_str) == Some("bosa");
                let lot_evidence = if is_bosa {
                    award.raw.get("winning_lots").cloned().unwrap_or_else(|| json!([]))
                } else {
                    Value::Null
                };
                let publication_references = if is_bosa {
                    award
                        .raw
                        .get("bosa_publication_reference_numbers_bda")
                        .cloned()
                        .unwrap_or_else(|| json!([]))
                } else {
                    Value::Null
                };
                let amount_note = if is_bosa {
                    "BOSA amounts are omitted because supplier-level attribution is not reliable"
                } else if award.values.is_empty() {
                    "no source-reported TED award value is available"
                } else {
                    "source-reported TED award value; currency is not included in this record"
                };
                let values = if is_bosa {
                    json!([])
                } else {
                    json!(award.values)
                };
                json!({
                    "source": if is_bosa { "BOSA" } else { "TED" },
                    "publication_number": award.publication_number,
                    "title": award.title,
                    "buyer": award.buyer,
                    "winner_names": award.winner_names,
                    "cpv_codes": award.cpv_codes,
                    "values": values,
                    "links": award.links,
                    "lot_evidence": lot_evidence,
                    "publication_references": publication_references,
                    "amount_note": amount_note,
                })
            })
            .collect();
        let has_bosa = awards
            .iter()
            .any(|award| award["source"].as_str() == Some("BOSA"));
        let has_ted = awards
            .iter()
            .any(|award| award["source"].as_str() == Some("TED"));
        let note = match (has_bosa, has_ted) {
            (true, true) => "BOSA records are listed first, then TED; BOSA lot evidence is shown without supplier-level amounts",
            (true, false) => "BOSA lot evidence is shown without supplier-level amounts",
            (false, true) => "TED award records; values are source-reported and may not include currency metadata",
            (false, false) => "no imported TED or BOSA award records for this company",
        };
        Ok(Json(json!({
            "awards": awards,
            "note": note,
        })))
    })
    .await
}

async fn company(
    State(state): State<Arc<ApiState>>,
    Path(enterprise_number): Path<String>,
) -> Result<Json<aanbestedingen_matcher::CompanyProfile>, ApiError> {
    let enterprise_number = normalized_enterprise_number(&enterprise_number)?;
    let state = state.as_ref().clone();
    run_blocking(move || {
        let repository = company_register(&state)?;
        let profile = repository
            .lookup(&enterprise_number)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("company not found"))?;
        Ok(Json(profile))
    })
    .await
}

fn recommendation_candidate_limit(query: &RecommendationQuery) -> Result<usize, ApiError> {
    match query.candidates {
        Some(0) => Err(ApiError::unprocessable("candidates must be at least 1")),
        Some(limit) if limit > MAX_RECOMMENDATION_CANDIDATES => Err(ApiError::unprocessable(
            format!("candidates must not exceed {MAX_RECOMMENDATION_CANDIDATES}"),
        )),
        Some(limit) => Ok(limit),
        None => Ok(DEFAULT_RECOMMENDATION_CANDIDATES),
    }
}

fn recommendation_limit(query: &RecommendationQuery) -> Result<usize, ApiError> {
    if query
        .due_within
        .is_some_and(|days| !(0..=365).contains(&days))
    {
        return Err(ApiError::unprocessable(
            "due_within must be between 0 and 365 days",
        ));
    }
    if query
        .sort
        .as_deref()
        .is_some_and(|sort| !matches!(sort, "score" | "deadline"))
    {
        return Err(ApiError::unprocessable("sort must be score or deadline"));
    }
    let limit = match query.limit {
        Some(0) => return Err(ApiError::unprocessable("limit must be at least 1")),
        Some(limit) if limit > 100 => {
            return Err(ApiError::unprocessable("limit must not exceed 100"))
        }
        Some(limit) => limit,
        None => 20,
    };
    // The candidate window bounds how many notices are scored, and the ladder is
    // collapsed before it is returned, so asking for more items than candidates
    // cannot be satisfied: measured 2026-10-19, `candidates=50&limit=60` returned
    // 43 items with no explanation. Name the contradiction instead.
    if let Some(candidates) = query.candidates {
        if candidates < limit {
            return Err(ApiError::unprocessable(
                "candidates must be at least the limit: the ladder is collapsed before it is returned",
            ));
        }
    }
    Ok(limit)
}

fn recommendation_deep_limit(query: &RecommendationQuery) -> Result<usize, ApiError> {
    match query.deep {
        Some(deep) if !(0..=10).contains(&deep) => {
            Err(ApiError::unprocessable("deep must be between 0 and 10"))
        }
        Some(deep) => Ok(deep as usize),
        None => Ok(0),
    }
}

/// Rejects a negative limit, which would withhold every filing and silently
/// turn all capacity evidence into unknown.
fn validate_filing_age_limit(query: &RecommendationQuery) -> Result<(), ApiError> {
    if query.max_filing_age_days.is_some_and(|days| days < 0) {
        return Err(ApiError::unprocessable(
            "max_filing_age_days must not be negative",
        ));
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct RecognitionEvidenceContext {
    register_as_of: String,
    kbo_snapshot_as_of: String,
    statement_source: aanbestedingen_matcher::recognition::RecognitionTextSource,
    lot_aggregation_needs_review: bool,
    comparison: RecognitionComparison,
}

#[derive(Debug, Serialize)]
struct ApiRecommendation {
    #[serde(flatten)]
    recommendation: aanbestedingen_matcher::recommendations::Recommendation,
    saved_feedback: Option<FeedbackLabel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recognition_evidence: Option<RecognitionEvidenceContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_company_kbo_evidence: Option<ShadowCompanyKboEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_company_nbb_coverage: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_company_nbb_filings: Option<Vec<aanbestedingen_matcher::store::NbbFinancialEvidence>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_company_nbb_filings_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_company_nbb_period_age: Option<ShadowNbbPeriodAge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_company_nbb_reconciliation: Option<ShadowNbbReconciliation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shadow_lot_features:
        Option<aanbestedingen_matcher::recommendations::ShadowLotFeatureProjection>,
}

#[derive(Debug, Clone, Serialize)]
struct ShadowCompanyKboEvidence {
    source: &'static str,
    snapshot: Option<aanbestedingen_matcher::kbo::KboSnapshotMetadata>,
    status: Option<String>,
    juridical_situation: Option<String>,
    juridical_form: Option<String>,
    start_date: Option<String>,
    activities: Vec<aanbestedingen_matcher::Activity>,
    snapshot_age: KboSnapshotAge,
}

#[derive(Debug, Clone, Serialize)]
struct KboSnapshotAge {
    as_of_date: String,
    age_days: Option<i64>,
    status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct ShadowNbbPeriodAge {
    as_of_date: String,
    filings: Vec<NbbFilingPeriodAge>,
}

#[derive(Debug, Clone, Serialize)]
struct NbbFilingPeriodAge {
    source_ref: String,
    fiscal_year: i64,
    period_end: Option<String>,
    days_since_period_end: Option<i64>,
    status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct ShadowNbbReconciliation {
    company_years: Vec<NbbCompanyYearReconciliation>,
}

#[derive(Debug, Clone, Serialize)]
struct NbbCompanyYearReconciliation {
    fiscal_year: i64,
    evidence_count: usize,
    period_scope_status: &'static str,
    status: &'static str,
    conflicting_fields: Vec<&'static str>,
    unresolved_difference_fields: Vec<&'static str>,
    incomplete_fields: Vec<&'static str>,
}

fn nbb_reconciliation(
    filings: &[aanbestedingen_matcher::store::NbbFinancialEvidence],
) -> ShadowNbbReconciliation {
    let mut by_year = BTreeMap::<i64, Vec<_>>::new();
    for filing in filings {
        by_year.entry(filing.fiscal_year).or_default().push(filing);
    }

    let company_years = by_year
        .into_iter()
        .rev()
        .map(|(fiscal_year, filings)| {
            let mut by_period = BTreeMap::<(Option<String>, Option<String>), Vec<_>>::new();
            for filing in &filings {
                by_period
                    .entry((filing.period_start.clone(), filing.period_end.clone()))
                    .or_default()
                    .push(*filing);
            }
            let explicit_period_groups = by_period
                .iter()
                .filter(|((start, end), _)| start.is_some() && end.is_some())
                .count();
            let unknown_period_rows = by_period
                .iter()
                .filter(|((start, end), _)| start.is_none() || end.is_none())
                .map(|(_, group)| group.len())
                .sum::<usize>();
            let period_scope_status = if explicit_period_groups == 0 {
                "period_boundaries_unknown"
            } else if unknown_period_rows > 0 {
                "mixed_known_and_unknown_period_boundaries"
            } else if explicit_period_groups == 1 {
                "single_explicit_period"
            } else {
                "multiple_explicit_periods"
            };

            let mut conflicting_fields = BTreeSet::new();
            let mut comparable_duplicate_groups = 0;
            for ((start, end), group) in &by_period {
                if start.is_none() || end.is_none() || group.len() < 2 {
                    continue;
                }
                comparable_duplicate_groups += 1;
                macro_rules! inspect_values {
                    ($field:ident, $field_name:literal) => {{
                        let observed = group
                            .iter()
                            .filter_map(|filing| filing.$field)
                            .collect::<BTreeSet<_>>();
                        if observed.len() > 1 {
                            conflicting_fields.insert($field_name);
                        }
                    }};
                }
                inspect_values!(turnover, "turnover");
                inspect_values!(staff_fte, "staff_fte");
                inspect_values!(equity, "equity");
                inspect_values!(result, "result");
            }
            let mut incomplete_fields = Vec::new();
            macro_rules! inspect_missing {
                ($field:ident, $field_name:literal) => {
                    if filings.iter().any(|filing| filing.$field.is_none()) {
                        incomplete_fields.push($field_name);
                    }
                };
            }
            inspect_missing!(turnover, "turnover");
            inspect_missing!(staff_fte, "staff_fte");
            inspect_missing!(equity, "equity");
            inspect_missing!(result, "result");
            let mut unresolved_difference_fields = BTreeSet::new();
            macro_rules! find_unresolved_differences {
                ($field:ident, $field_name:literal) => {{
                    let observed = filings
                        .iter()
                        .filter_map(|filing| filing.$field)
                        .collect::<BTreeSet<_>>();
                    if observed.len() > 1 && !conflicting_fields.contains($field_name) {
                        unresolved_difference_fields.insert($field_name);
                    }
                }};
            }
            find_unresolved_differences!(turnover, "turnover");
            find_unresolved_differences!(staff_fte, "staff_fte");
            find_unresolved_differences!(equity, "equity");
            find_unresolved_differences!(result, "result");
            let has_confirmed_conflict = !conflicting_fields.is_empty();
            let conflicting_fields = conflicting_fields.into_iter().collect::<Vec<_>>();
            let unresolved_difference_fields =
                unresolved_difference_fields.into_iter().collect::<Vec<_>>();

            let status = if has_confirmed_conflict {
                "conflicting_nonnull_values"
            } else if !unresolved_difference_fields.is_empty() {
                "differences_unresolved_by_period_scope"
            } else if comparable_duplicate_groups > 0 {
                "no_nonnull_disagreement_observed"
            } else if unknown_period_rows > 0 {
                "period_scope_unknown"
            } else if explicit_period_groups > 1 {
                "different_periods_only"
            } else {
                "single_reference"
            };
            NbbCompanyYearReconciliation {
                fiscal_year,
                evidence_count: filings.len(),
                period_scope_status,
                status,
                conflicting_fields,
                unresolved_difference_fields,
                incomplete_fields,
            }
        })
        .collect();

    ShadowNbbReconciliation { company_years }
}

fn nbb_period_age(
    filings: &[aanbestedingen_matcher::store::NbbFinancialEvidence],
    as_of: NaiveDate,
) -> ShadowNbbPeriodAge {
    let filings = filings
        .iter()
        .map(|filing| {
            let mut age = NbbFilingPeriodAge {
                source_ref: filing.source_ref.clone(),
                fiscal_year: filing.fiscal_year,
                period_end: filing.period_end.clone(),
                days_since_period_end: None,
                status: "period_end_missing",
            };
            match filing.period_end.as_deref() {
                None => {}
                Some(period_end) => match NaiveDate::parse_from_str(period_end, "%Y-%m-%d") {
                    Ok(period_end) => {
                        age.days_since_period_end = Some((as_of - period_end).num_days());
                        age.status = if period_end > as_of {
                            "period_end_after_as_of"
                        } else {
                            "period_end_available"
                        };
                    }
                    Err(_) => age.status = "period_end_unparseable",
                },
            }
            age
        })
        .collect();
    ShadowNbbPeriodAge {
        as_of_date: as_of.format("%Y-%m-%d").to_string(),
        filings,
    }
}

fn kbo_snapshot_age(
    snapshot: Option<&aanbestedingen_matcher::kbo::KboSnapshotMetadata>,
    as_of: NaiveDate,
) -> KboSnapshotAge {
    let mut age = KboSnapshotAge {
        as_of_date: as_of.format("%Y-%m-%d").to_string(),
        age_days: None,
        status: "metadata_table_missing",
    };
    let Some(snapshot) = snapshot else {
        return age;
    };
    let Some(snapshot_date) = snapshot.snapshot_date.as_deref() else {
        age.status = "snapshot_date_missing";
        return age;
    };
    match NaiveDate::parse_from_str(snapshot_date, "%d-%m-%Y") {
        Ok(snapshot_date) => {
            age.age_days = Some((as_of - snapshot_date).num_days());
            age.status = "available";
        }
        Err(_) => age.status = "snapshot_date_unparseable",
    }
    age
}

fn exact_recognition_approvals(
    snapshot: &RecognitionSnapshot,
    enterprise_number: &str,
) -> Vec<aanbestedingen_matcher::recognized_contractors::RecognizedApproval> {
    snapshot
        .approval_rows
        .iter()
        .filter(|approval| {
            approval.kbo_match.is_exact()
                && approval.enterprise_number_normalized.as_deref() == Some(enterprise_number)
        })
        .cloned()
        .collect()
}

fn recognition_evidence_context(
    snapshot: &RecognitionSnapshot,
    approvals: &[aanbestedingen_matcher::recognized_contractors::RecognizedApproval],
    tender: &Tender,
) -> Option<RecognitionEvidenceContext> {
    let statement = recognition::recognition_statement(tender);
    let comparison = compare_requirement_to_approvals(&statement, approvals);
    if comparison.status == RecognitionEvidenceStatus::NoRequirement {
        return None;
    }
    Some(RecognitionEvidenceContext {
        register_as_of: snapshot.register_as_of.clone(),
        kbo_snapshot_as_of: snapshot.kbo_snapshot_as_of.clone(),
        statement_source: statement.source?,
        lot_aggregation_needs_review: statement.lot_aggregation_needs_review,
        comparison,
    })
}

async fn recommendations(
    State(state): State<Arc<ApiState>>,
    Path(enterprise_number): Path<String>,
    Query(query): Query<RecommendationQuery>,
) -> Result<Json<Vec<ApiRecommendation>>, ApiError> {
    let output_limit = recommendation_limit(&query)?;
    let candidate_limit = recommendation_candidate_limit(&query)?;
    let deep_limit = recommendation_deep_limit(&query)?;
    validate_filing_age_limit(&query)?;
    let enterprise_number = normalized_enterprise_number(&enterprise_number)?;
    let state = state.as_ref().clone();
    run_blocking(move || {
        let repository = company_register(&state)?;
        let company = repository
            .lookup(&enterprise_number)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("company not found"))?;
        let store =
            TenderStore::open_read_only(&state.tender_database).map_err(ApiError::internal)?;
        let today = Utc::now().date_naive();
        // Keep normal dashboard requests bounded; callers may opt into a
        // larger explicit candidate window when they need broader recall.
        let mut tenders = store
            .open_tenders(candidate_limit, today, query.due_within)
            .map_err(ApiError::internal)?;
        tenders.retain(|tender| is_eligible(tender, today, query.due_within));
        let preferences = store
            .preferences_for(&company.enterprise_number)
            .map_err(ApiError::internal)?;
        let history = store
            .awards_for(&company.enterprise_number)
            .map_err(ApiError::internal)?;
        let feedback = store
            .feedback_for(&company.enterprise_number)
            .map_err(ApiError::internal)?;
        let saved_feedback = store
            .feedback_details_for(&company.enterprise_number)
            .map_err(ApiError::internal)?;
        let (
            shadow_company_nbb_coverage,
            shadow_company_nbb_filings,
            shadow_company_nbb_filings_truncated,
            shadow_company_nbb_period_age,
            shadow_company_nbb_reconciliation,
        ) = if query.include_company_nbb_evidence == Some(true) {
            let lookup = store
                .nbb_financial_evidence_lookup_for(&company.enterprise_number)
                .map_err(ApiError::internal)?;
            let period_age = nbb_period_age(&lookup.filings, today);
            let reconciliation = nbb_reconciliation(&lookup.filings);
            (
                Some(lookup.coverage),
                Some(lookup.filings),
                Some(lookup.truncated),
                Some(period_age),
                Some(reconciliation),
            )
        } else {
            (None, None, None, None, None)
        };
        let shadow_company_kbo_evidence = if query.include_company_kbo_evidence == Some(true) {
            let snapshot = repository.snapshot_metadata().map_err(ApiError::internal)?;
            Some(ShadowCompanyKboEvidence {
                source: "kbo_bce",
                snapshot_age: kbo_snapshot_age(snapshot.as_ref(), today),
                snapshot,
                status: company.status.clone(),
                juridical_situation: company.juridical_situation.clone(),
                juridical_form: company.juridical_form.clone(),
                start_date: company.start_date.clone(),
                activities: company.activities.clone(),
            })
        } else {
            None
        };
        let mut results = rank(
            &company,
            &tenders,
            &history,
            &preferences,
            &feedback,
            tenders.len(),
        );
        // Shared with the CLI so the two cannot select different filings, and so
        // a future freshness policy has one place to change.
        // One selection for both the figure and its provenance, so the two can
        // never describe different filings.
        let nbb_capacity = if query.capacity_triage == Some(true) {
            store
                .nbb_financials_for(&company.enterprise_number)
                .ok()
                .map(|fins| {
                    nbb_capacity_turnover(&fins, Utc::now().date_naive(), query.max_filing_age_days)
                })
        } else {
            None
        };
        let effective_turnover = query
            .annual_turnover
            .or_else(|| nbb_capacity.as_ref().and_then(|(turnover, _)| *turnover));
        let effective_bid_cap = query.max_bid_capacity.or_else(|| {
            if query.capacity_triage == Some(true) {
                preferences.maximum_value.and_then(|m| m.to_f64())
            } else {
                None
            }
        });
        // The response used to carry the turnover figure without saying which
        // filing it came from, so a capacity warning asserted an unsourced amount.
        let turnover_provenance = query
            .annual_turnover
            .is_none()
            .then(|| nbb_capacity.and_then(|(_, provenance)| provenance))
            .flatten();
        let capacity_limits = CapacityLimits::new(effective_turnover, effective_bid_cap);
        let triage_requested = query.capacity_triage == Some(true);
        if triage_requested || !capacity_limits.is_empty() {
            let tenders_by_key: HashMap<(String, String), &Tender> = tenders
                .iter()
                .map(|t| ((t.source.clone(), t.external_id.clone()), t))
                .collect();
            apply_capacity_triage(
                &mut results,
                &tenders_by_key,
                capacity_limits,
                turnover_provenance.as_ref(),
                triage_requested,
            );
            if query.sort.as_deref() != Some("deadline") {
                sort_by_score(&mut results);
            }
        }
        if query.sort.as_deref() == Some("deadline") {
            sort_by_deadline(&mut results);
        }
        if deep_limit > 0 {
            drop(store);
            deep_rerank(
                &state,
                &DeepRerankInputs {
                    company: &company,
                    history: &history,
                    preferences: &preferences,
                    tenders: &tenders,
                },
                &mut results,
                deep_limit,
            )?;
            if query.sort.as_deref() == Some("deadline") {
                sort_by_deadline(&mut results);
            } else {
                sort_by_score(&mut results);
            }
        }
        results.truncate(output_limit);
        let exact_approvals = state
            .recognition_snapshot
            .as_ref()
            .map(|snapshot| exact_recognition_approvals(snapshot, &company.enterprise_number))
            .unwrap_or_default();
        let response = results
            .into_iter()
            .map(|mut recommendation| {
                let tender = tenders.iter().find(|tender| {
                    tender.source == recommendation.source
                        && tender.external_id == recommendation.external_id
                });
                if query.include_activity_text_similarity == Some(true) {
                    if let Some(tender) = tender {
                        recommendation.shadow_activity_text_similarity = Some(
                            aanbestedingen_matcher::score_breakdown(
                                &company,
                                tender,
                                &history,
                                &preferences,
                            )
                            .activity_text_similarity,
                        );
                        recommendation.shadow_activity_text_similarity_coverage =
                            Some(aanbestedingen_matcher::activity_text_coverage_with(
                                &company,
                                tender,
                                &preferences,
                            ));
                    }
                }
                if query.include_geographic_evidence == Some(true) {
                    if let Some(tender) = tender {
                        recommendation.shadow_geographic_evidence =
                            Some(aanbestedingen_matcher::geographic_match_evidence(
                                &company,
                                tender,
                                &preferences,
                            ));
                    }
                }
                let shadow_lot_features = if query.include_lot_features == Some(true) {
                    tender.map(|tender| {
                        aanbestedingen_matcher::recommendations::shadow_lot_feature_projection(
                            &company, tender,
                        )
                    })
                } else {
                    None
                };
                let recognition_evidence =
                    state.recognition_snapshot.as_ref().and_then(|snapshot| {
                        let tender = tender?;
                        recognition_evidence_context(snapshot, &exact_approvals, tender)
                    });
                let saved_feedback = saved_feedback
                    .get(&(
                        recommendation.source.clone(),
                        recommendation.external_id.clone(),
                    ))
                    .cloned();
                ApiRecommendation {
                    recommendation,
                    saved_feedback,
                    recognition_evidence,
                    shadow_company_kbo_evidence: shadow_company_kbo_evidence.clone(),
                    shadow_company_nbb_coverage,
                    shadow_company_nbb_filings: shadow_company_nbb_filings.clone(),
                    shadow_company_nbb_filings_truncated,
                    shadow_company_nbb_period_age: shadow_company_nbb_period_age.clone(),
                    shadow_company_nbb_reconciliation: shadow_company_nbb_reconciliation.clone(),
                    shadow_lot_features,
                }
            })
            .collect();
        Ok(Json(response))
    })
    .await
}

async fn get_preferences(
    State(state): State<Arc<ApiState>>,
    Path(enterprise_number): Path<String>,
) -> Result<Json<CompanyPreferences>, ApiError> {
    let normalized = normalized_enterprise_number(&enterprise_number)?;
    let state = state.as_ref().clone();
    run_blocking(move || {
        let store =
            TenderStore::open_read_only(&state.tender_database).map_err(ApiError::internal)?;
        Ok(Json(
            store
                .preferences_for(&normalized)
                .map_err(ApiError::internal)?,
        ))
    })
    .await
}

async fn save_preferences(
    State(state): State<Arc<ApiState>>,
    Path(enterprise_number): Path<String>,
    ApiJson(request): ApiJson<PreferencesRequest>,
) -> Result<Json<Value>, ApiError> {
    let normalized = normalized_enterprise_number(&enterprise_number)?;
    let preferences = CompanyPreferences {
        confirmed_activities: request.confirmed_activities,
        excluded_terms: request.excluded_terms,
        preferred_regions: request.preferred_regions,
        languages: request.languages,
        minimum_value: request.minimum_value,
        maximum_value: request.maximum_value,
        recognition_class: request.recognition_class,
        recognition_categories: request.recognition_categories,
    };
    validate_value_limits(preferences.minimum_value, preferences.maximum_value)
        .map_err(ApiError::unprocessable)?;
    recognition::validate_recognition(
        preferences.recognition_class,
        &preferences.recognition_categories,
    )
    .map_err(ApiError::unprocessable)?;
    let state = state.as_ref().clone();
    run_blocking(move || {
        let store =
            TenderStore::open_initialized(&state.tender_database).map_err(ApiError::internal)?;
        store
            .save_preferences_audited(&operator_label(), &normalized, &preferences)
            .map_err(ApiError::internal)?;
        Ok(Json(json!({"status": "saved"})))
    })
    .await
}

async fn save_feedback(
    State(state): State<Arc<ApiState>>,
    Path(enterprise_number): Path<String>,
    ApiJson(request): ApiJson<FeedbackRequest>,
) -> Result<Json<Value>, ApiError> {
    let normalized = normalized_enterprise_number(&enterprise_number)?;
    let (source, external_id) = feedback_target(&request)?;
    let state = state.as_ref().clone();
    run_blocking(move || {
        let repository = company_register(&state)?;
        if repository
            .lookup(&normalized)
            .map_err(ApiError::internal)?
            .is_none()
        {
            return Err(ApiError::not_found("company not found"));
        }
        let store =
            TenderStore::open_initialized(&state.tender_database).map_err(ApiError::internal)?;
        if store
            .tender(&source, &external_id)
            .map_err(ApiError::internal)?
            .is_none()
        {
            return Err(ApiError::not_found("tender not found"));
        }
        validate_feedback_outcome(&request.outcome)?;
        let feedback = store
            .update_feedback(
                &operator_label(),
                &normalized,
                &source,
                &external_id,
                &FeedbackUpdate {
                    relevant: request.relevant,
                    pursuable: request.pursuable,
                    outcome: request.outcome,
                    reason: request.reason,
                },
            )
            .map_err(ApiError::internal)?;
        Ok(Json(json!({"status": "saved", "feedback": feedback})))
    })
    .await
}

/// Deep rerank: fuse analyzer decisions into the top `deep` items, sharing
/// the analysis endpoint's memory and persistent caches so repeated runs skip
/// unchanged analyzer calls. Mirrors the Python reference's read-only deep
/// mode: hits are reused, but fresh decisions are not persisted from the
/// API; the CLI persists them instead.
struct DeepRerankInputs<'a> {
    company: &'a aanbestedingen_matcher::CompanyProfile,
    history: &'a [aanbestedingen_matcher::AwardRecord],
    preferences: &'a CompanyPreferences,
    tenders: &'a [Tender],
}

fn deep_rerank(
    state: &ApiState,
    inputs: &DeepRerankInputs,
    results: &mut [aanbestedingen_matcher::recommendations::Recommendation],
    deep: usize,
) -> Result<(), ApiError> {
    let provider = state
        .analysis
        .as_ref()
        .map(ConfiguredAnalyzer::cache_identity)
        .unwrap_or_else(|| DETERMINISTIC_CACHE_IDENTITY.to_owned());
    deep_rerank_with_analyzer(
        state,
        inputs,
        results,
        deep,
        &provider,
        |key| {
            // This short-lived reader is dropped before admission/computation.
            let store =
                TenderStore::open_read_only(&state.tender_database).map_err(ApiError::internal)?;
            store.cached_analysis(key).map_err(ApiError::internal)
        },
        |tender| {
            if let Some(analyzer) = &state.analysis {
                Analyzer::analyze(
                    analyzer,
                    inputs.company,
                    tender,
                    inputs.history,
                    inputs.preferences,
                )
                .map_err(|error| error.to_string())
            } else {
                Ok(analyze(
                    inputs.company,
                    tender,
                    inputs.history,
                    inputs.preferences,
                ))
            }
        },
    )
}

fn deep_rerank_with_analyzer(
    state: &ApiState,
    inputs: &DeepRerankInputs,
    results: &mut [aanbestedingen_matcher::recommendations::Recommendation],
    deep: usize,
    provider: &str,
    cached_lookup: impl Fn(&str) -> Result<Option<AnalysisDecision>, ApiError>,
    analyze_tender: impl Fn(&Tender) -> Result<AnalysisDecision, String>,
) -> Result<(), ApiError> {
    let DeepRerankInputs {
        company,
        history,
        preferences,
        tenders,
    } = inputs;
    let tenders_by_key: HashMap<(String, String), &Tender> = tenders
        .iter()
        .map(|tender| ((tender.source.clone(), tender.external_id.clone()), tender))
        .collect();
    for item in results.iter_mut().take(deep) {
        let Some(tender) = tenders_by_key
            .get(&(item.source.clone(), item.external_id.clone()))
            .copied()
        else {
            continue;
        };
        let cache_key = analysis_cache_key(provider, company, tender, history, preferences);
        let mut decision = {
            let mut cache = state
                .analysis_cache
                .lock()
                .map_err(|_| ApiError::internal("analysis cache is unavailable"))?;
            cache.get(&cache_key)
        };
        if decision.is_some() {
            state
                .cache_metrics
                .memory_hits
                .fetch_add(1, Ordering::Relaxed);
        }
        if decision.is_none() {
            if let Some(cached) = cached_lookup(&cache_key)? {
                let mut cache = state
                    .analysis_cache
                    .lock()
                    .map_err(|_| ApiError::internal("analysis cache is unavailable"))?;
                if cache.insert(cache_key.clone(), cached.clone()) {
                    state
                        .cache_metrics
                        .evictions
                        .fetch_add(1, Ordering::Relaxed);
                }
                state
                    .cache_metrics
                    .persistent_hits
                    .fetch_add(1, Ordering::Relaxed);
                decision = Some(cached);
            }
        }
        if decision.is_none() {
            decision = Some(analyze_cache_miss(
                state,
                &cache_key,
                || analyze_tender(tender).map_err(ApiError::service_unavailable),
                |_| Ok(()), // Deep reranks retain their existing memory-only policy.
            )?);
        }
        apply_jev_decision(item, &decision.expect("a decision is set on every path"));
    }
    Ok(())
}

async fn tender_analysis(
    State(state): State<Arc<ApiState>>,
    Path((enterprise_number, source, external_id)): Path<(String, String, String)>,
) -> Result<Json<aanbestedingen_matcher::analysis::AnalysisDecision>, ApiError> {
    let enterprise_number = normalized_enterprise_number(&enterprise_number)?;
    let state = state.as_ref().clone();
    run_blocking(move || {
        let repository = company_register(&state)?;
        let company = repository
            .lookup(&enterprise_number)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("company not found"))?;
        let store =
            TenderStore::open_read_only(&state.tender_database).map_err(ApiError::internal)?;
        let tender = store
            .tender(&source, &external_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("tender not found"))?;
        let preferences = store
            .preferences_for(&company.enterprise_number)
            .map_err(ApiError::internal)?;
        let history = store
            .awards_for(&company.enterprise_number)
            .map_err(ApiError::internal)?;
        let provider = state
            .analysis
            .as_ref()
            .map(ConfiguredAnalyzer::cache_identity)
            .unwrap_or_else(|| DETERMINISTIC_CACHE_IDENTITY.to_owned());
        let cache_key = analysis_cache_key(&provider, &company, &tender, &history, &preferences);
        let memory_decision = {
            let mut cache = state
                .analysis_cache
                .lock()
                .map_err(|_| ApiError::internal("analysis cache is unavailable"))?;
            cache.get(&cache_key)
        };
        if let Some(mut decision) = memory_decision {
            state
                .cache_metrics
                .memory_hits
                .fetch_add(1, Ordering::Relaxed);
            decision.cached = true;
            return Ok(Json(decision));
        }
        if let Some(mut decision) = store
            .cached_analysis(&cache_key)
            .map_err(ApiError::internal)?
        {
            let mut cache = state
                .analysis_cache
                .lock()
                .map_err(|_| ApiError::internal("analysis cache is unavailable"))?;
            if cache.insert(cache_key.clone(), decision.clone()) {
                state
                    .cache_metrics
                    .evictions
                    .fetch_add(1, Ordering::Relaxed);
            }
            state
                .cache_metrics
                .persistent_hits
                .fetch_add(1, Ordering::Relaxed);
            decision.cached = true;
            return Ok(Json(decision));
        }
        // Never hold read-only/write access while provider calls run.
        drop(store);
        let decision = analyze_cache_miss(
            &state,
            &cache_key,
            || {
                if let Some(analyzer) = &state.analysis {
                    Analyzer::analyze(analyzer, &company, &tender, &history, &preferences)
                        .map_err(ApiError::bad_gateway)
                } else {
                    Ok(analyze(&company, &tender, &history, &preferences))
                }
            },
            |decision| {
                let store = TenderStore::open_initialized(&state.tender_database)
                    .map_err(ApiError::internal)?;
                store
                    .save_analysis(&cache_key, decision)
                    .map_err(ApiError::internal)
            },
        )?;
        Ok(Json(decision))
    })
    .await
}

/// Candidate window a prospects request scores when the caller does not ask
/// for one. Scoring is in-memory per company, so the default stays well
/// below the CLI's: raise `candidates` explicitly for full recall.
const DEFAULT_API_PROSPECT_CANDIDATES: usize = 5_000;

#[derive(Debug, Deserialize)]
struct ProspectQuery {
    limit: Option<usize>,
    candidates: Option<usize>,
    nace: Option<String>,
    region: Option<String>,
    deep: Option<i64>,
}

fn parse_prospect_nace(value: &str) -> Result<Vec<String>, ApiError> {
    let mut prefixes = Vec::new();
    for part in value.split(',') {
        let prefix = part.trim().to_owned();
        if prefix.is_empty()
            || prefix.len() > 5
            || !prefix.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(ApiError::unprocessable(
                "nace takes 1-5 digit prefixes, e.g. ?nace=62,63",
            ));
        }
        prefixes.push(prefix);
    }
    if prefixes.is_empty() {
        return Err(ApiError::unprocessable(
            "nace takes 1-5 digit prefixes, e.g. ?nace=62,63",
        ));
    }
    Ok(prefixes)
}

fn prospect_limits(query: &ProspectQuery) -> Result<(usize, usize), ApiError> {
    let limit = match query.limit {
        Some(0) => return Err(ApiError::unprocessable("limit must be at least 1")),
        Some(limit) if limit > 100 => {
            return Err(ApiError::unprocessable("limit must not exceed 100"))
        }
        Some(limit) => limit,
        None => 20,
    };
    let candidates = match query.candidates {
        Some(0) => return Err(ApiError::unprocessable("candidates must be at least 1")),
        Some(candidates) if candidates > MAX_PROSPECT_CANDIDATES => {
            return Err(ApiError::unprocessable(format!(
                "candidates must not exceed {MAX_PROSPECT_CANDIDATES}"
            )))
        }
        Some(candidates) => candidates,
        None => DEFAULT_API_PROSPECT_CANDIDATES,
    };
    if candidates < limit {
        return Err(ApiError::unprocessable(
            "candidates must be at least the limit: the prospect list is collapsed before it is returned",
        ));
    }
    if query.deep.is_some_and(|deep| !(0..=10).contains(&deep)) {
        return Err(ApiError::unprocessable("deep must be between 0 and 10"));
    }
    Ok((limit, candidates))
}

/// Buyer-side reverse search: rank KBO companies for one stored tender.
/// Scores are exactly what the forward ranking would compute per pair; the
/// order is deterministic (score, topical similarity, region signal,
/// enterprise number). Expensive by design — every candidate loads a full
/// company profile — so bound the work with the `candidates` cap.
///
/// The bare-array shape is intentional, matching the recommendations
/// endpoint: pool recall metadata (`pool_total`, cap notes) lives in the
/// CLI output, where the unbounded interactive use happens (review I4).
async fn tender_prospects(
    State(state): State<Arc<ApiState>>,
    Path((source, external_id)): Path<(String, String)>,
    Query(query): Query<ProspectQuery>,
) -> Result<Json<Vec<CompanyProspect>>, ApiError> {
    let (output_limit, candidate_limit) = prospect_limits(&query)?;
    let nace = query.nace.as_deref().map(parse_prospect_nace).transpose()?;
    let region = query
        .region
        .as_deref()
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .map(str::to_owned);
    let state = state.as_ref().clone();
    run_blocking(move || {
        let store =
            TenderStore::open_read_only(&state.tender_database).map_err(ApiError::internal)?;
        let tender = store
            .tender(&source, &external_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("tender not found"))?;
        let prefixes = match nace {
            Some(prefixes) => prefixes,
            None => {
                let derived = nace_prefixes_for_tender(&tender);
                if derived.is_empty() {
                    return Err(ApiError::unprocessable(
                        "the tender carries no CPV codes, so no NACE pool can be derived: pass ?nace= explicitly (e.g. ?nace=62,63)",
                    ));
                }
                derived
            }
        };
        let repository = company_register(&state)?;
        let (pool, _) = repository
            .enterprises_for_nace(&prefixes, candidate_limit)
            .map_err(ApiError::internal)?;
        let mut companies = repository.lookup_batch(&pool).map_err(ApiError::internal)?;
        if let Some(region) = &region {
            companies.retain(|company| company_matches_region_filter(company, region));
        }
        let judged: HashSet<String> = store
            .enterprises_with_context()
            .map_err(ApiError::internal)?;
        let mut contexts: HashMap<String, ProspectContext> = HashMap::new();
        for company in &companies {
            if judged.contains(&company.enterprise_number) {
                let preferences: CompanyPreferences = store
                    .preferences_for(&company.enterprise_number)
                    .map_err(ApiError::internal)?;
                let history: Vec<AwardRecord> = store
                    .awards_for(&company.enterprise_number)
                    .map_err(ApiError::internal)?;
                let feedback = store
                    .feedback_for(&company.enterprise_number)
                    .map_err(ApiError::internal)?;
                contexts.insert(
                    company.enterprise_number.clone(),
                    (preferences, history, feedback),
                );
            }
        }
        let mut prospects = rank_prospects(&companies, &tender, &contexts);
        let deep = query.deep.unwrap_or(0).max(0) as usize;
        if deep > 0 {
            // Read-only, unlike the CLI: reuse cached analyzer decisions from
            // memory or the store, never compute fresh ones per request — a
            // prospects call already loads thousands of company profiles.
            let provider = state
                .analysis
                .as_ref()
                .map(ConfiguredAnalyzer::cache_identity)
                .unwrap_or_else(|| DETERMINISTIC_CACHE_IDENTITY.to_owned());
            let companies_by_id: HashMap<&str, &CompanyProfile> = companies
                .iter()
                .map(|company| (company.enterprise_number.as_str(), company))
                .collect();
            let default_context: ProspectContext =
                (CompanyPreferences::default(), Vec::new(), HashMap::new());
            for prospect in prospects.iter_mut().take(deep) {
                let Some(company) = companies_by_id.get(prospect.enterprise_number.as_str())
                else {
                    continue;
                };
                let (preferences, history, _) = contexts
                    .get(&company.enterprise_number)
                    .unwrap_or(&default_context);
                let cache_key =
                    analysis_cache_key(&provider, company, &tender, history, preferences);
                let mut decision = state
                    .analysis_cache
                    .lock()
                    .map_err(|_| ApiError::internal("analysis cache is unavailable"))?
                    .get(&cache_key);
                if decision.is_none() {
                    decision = store.cached_analysis(&cache_key).map_err(ApiError::internal)?;
                }
                if let Some(decision) = decision {
                    apply_prospect_decision(prospect, &decision);
                }
            }
            sort_prospects(&mut prospects);
        }
        prospects.truncate(output_limit);
        Ok(Json(prospects))
    })
    .await
}

async fn tender_detail(
    State(state): State<Arc<ApiState>>,
    Path((source, external_id)): Path<(String, String)>,
) -> Result<Json<Tender>, ApiError> {
    let state = state.as_ref().clone();
    run_blocking(move || {
        let store =
            TenderStore::open_read_only(&state.tender_database).map_err(ApiError::internal)?;
        let tender = store
            .tender(&source, &external_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("tender not found"))?;
        Ok(Json(tender))
    })
    .await
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl ApiError {
    fn internal(error: impl std::fmt::Display) -> Self {
        eprintln!("API internal error: {error}");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error".to_owned(),
        )
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, message.into())
    }

    fn unprocessable(error: impl std::fmt::Display) -> Self {
        Self(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
    }

    fn unauthorized() -> Self {
        Self(
            StatusCode::UNAUTHORIZED,
            "invalid or missing API token".to_owned(),
        )
    }

    fn bad_gateway(error: impl std::fmt::Display) -> Self {
        Self(StatusCode::BAD_GATEWAY, error.to_string())
    }

    fn service_unavailable(error: impl std::fmt::Display) -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        // `detail` is the Python reference's (FastAPI) error key; clients
        // written against either API read the same field.
        let mut response = (self.0, Json(json!({ "detail": self.1 }))).into_response();
        if self.0 == StatusCode::UNAUTHORIZED {
            // RFC 7235 requires a challenge on 401; the Python reference's
            // auth middleware sends the same header.
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// JSON extractor that keeps axum's own rejections on the reference's error
/// contract.
///
/// axum answers a malformed body with a plain-text 400 and a missing JSON
/// content type with a plain-text 415, while the Python reference answers 422
/// with the `detail` envelope for both; unwrapping `Json` here means a client
/// reads the same shape from either implementation. The one exception is the
/// body limit, which the reference's own middleware also answers as a
/// plain-text 413, so that status is preserved with its plain-text body.
struct ApiJson<T>(T);

enum ApiJsonRejection {
    TooLarge(String),
    Invalid(String),
}

impl axum::response::IntoResponse for ApiJsonRejection {
    fn into_response(self) -> Response {
        match self {
            Self::TooLarge(message) => (StatusCode::PAYLOAD_TOO_LARGE, message).into_response(),
            Self::Invalid(message) => ApiError::unprocessable(message).into_response(),
        }
    }
}

#[async_trait]
impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = ApiJsonRejection;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                Err(ApiJsonRejection::TooLarge(rejection.body_text()))
            }
            Err(rejection) => Err(ApiJsonRejection::Invalid(rejection.body_text())),
        }
    }
}

/// The reference's answer when nothing matches: FastAPI falls through to the
/// static mount, so an unserved method on an API route or a missing static
/// file is a `detail`-bodied 404 rather than axum's plain-text 405 or the
/// empty 404 `ServeDir` returns.
async fn reference_not_found() -> ApiError {
    ApiError::not_found("Not Found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aanbestedingen_matcher::recognized_contractors::KboMatch;
    use aanbestedingen_matcher::store::{NbbFinancials, NBB_EVIDENCE_PROJECTION_LIMIT};

    fn recognition_snapshot_fixture() -> RecognitionSnapshot {
        RecognitionSnapshot {
            schema_version: 2,
            generated_at_utc: "2026-10-31T00:00:00Z".to_owned(),
            source_file: "fixture.xlsx".to_owned(),
            source_sha256: "fixture-hash".to_owned(),
            source_worksheet: "Sheet1".to_owned(),
            kbo_database: "fixture.duckdb".to_owned(),
            kbo_snapshot_as_of: "2026-06-24".to_owned(),
            register_as_of: "2026-10-31".to_owned(),
            approval_rows: vec![
                aanbestedingen_matcher::recognized_contractors::RecognizedApproval {
                    source_row: 12,
                    validity_as_of: aanbestedingen_matcher::recognized_contractors::ApprovalValidity::CurrentAsOf,
                    enterprise_number_raw: Some("0404484060".to_owned()),
                    enterprise_number_normalized: Some("0404.484.060".to_owned()),
                    vat_number: None,
                    enterprise_name: Some("Example".to_owned()),
                    approval_number: Some("A-12".to_owned()),
                    street: None,
                    postcode: None,
                    municipality: None,
                    decision_date_raw: Some("01/01/2025".to_owned()),
                    expiration_date_raw: Some("31/12/2027".to_owned()),
                    category_class_raw: Some("C (8)".to_owned()),
                    category_class_parse_status: aanbestedingen_matcher::recognized_contractors::CategoryClassParseStatus::Complete,
                    recognition_credentials: vec![aanbestedingen_matcher::recognized_contractors::RecognitionCredential {
                        category: "C".to_owned(),
                        category_description_nl: Some("Algemene aannemingen van wegenbouwkundige werken".to_owned()),
                        class: 8,
                        class_amount_guidance: None,
                    }],
                    kbo_match: KboMatch::ExactLocalSnapshot,
                },
            ],
            report: aanbestedingen_matcher::recognized_contractors::JoinReport::default(),
        }
    }

    #[test]
    fn recommendation_register_evidence_uses_exact_join_and_carries_snapshot_dates() {
        let snapshot = recognition_snapshot_fixture();
        let tender = Tender {
            requirements: vec![aanbestedingen_matcher::TenderRequirement {
                text: "Erkenning: C (Wegenbouwkundige werken), Klasse 7".to_owned(),
                kind: "eligibility".to_owned(),
                mandatory: true,
                source_page: None,
            }],
            ..Tender::default()
        };
        let approvals = exact_recognition_approvals(&snapshot, "0404.484.060");
        assert_eq!(approvals.len(), 1);
        let evidence = recognition_evidence_context(&snapshot, &approvals, &tender)
            .expect("stated recognition requirement should return register evidence");
        assert_eq!(evidence.register_as_of, "2026-10-31");
        assert_eq!(evidence.kbo_snapshot_as_of, "2026-06-24");
        assert_eq!(
            evidence.comparison.status,
            aanbestedingen_matcher::recognized_contractors::RecognitionEvidenceStatus::CurrentDatedApprovalFound
        );
        assert_eq!(evidence.comparison.evidence[0].source_row, 12);
        assert_eq!(
            evidence.statement_source,
            aanbestedingen_matcher::recognition::RecognitionTextSource::Requirements
        );

        let description_notice = Tender {
            description: "Agréation requise D classe 5. En cas d'attribution de plusieurs lots, le soumissionnaire doit posséder la classe d'agréation correspondant au montant cumulé des lots.".to_owned(),
            ..Tender::default()
        };
        let description_evidence =
            recognition_evidence_context(&snapshot, &approvals, &description_notice)
                .expect("explicit description clause should be reported");
        assert_eq!(
            description_evidence.statement_source,
            aanbestedingen_matcher::recognition::RecognitionTextSource::Description
        );
        assert_eq!(
            description_evidence.comparison.required_category.as_deref(),
            Some("D")
        );
        assert_eq!(description_evidence.comparison.required_class, Some(5));
        assert!(description_evidence.lot_aggregation_needs_review);
        assert_eq!(
            serde_json::to_value(&description_evidence).unwrap()["statement_source"],
            "description"
        );

        let mut live_verified = snapshot.clone();
        live_verified.approval_rows[0].kbo_match = KboMatch::ExactLiveApi {
            verified_at_utc: "2026-10-31T10:00:00Z".to_owned(),
            current_status: Some("AC".to_owned()),
        };
        let live_approvals = exact_recognition_approvals(&live_verified, "0404.484.060");
        assert_eq!(live_approvals.len(), 1);
        let live_evidence = recognition_evidence_context(&live_verified, &live_approvals, &tender)
            .expect("live exact match should yield evidence");
        assert!(matches!(
            &live_evidence.comparison.evidence[0].kbo_match,
            KboMatch::ExactLiveApi { .. }
        ));

        let wrong_company = exact_recognition_approvals(&snapshot, "0402.123.456");
        assert!(
            recognition_evidence_context(&snapshot, &wrong_company, &tender)
                .unwrap()
                .comparison
                .evidence
                .is_empty()
        );
        let mut unmatched = snapshot;
        unmatched.approval_rows[0].kbo_match = KboMatch::UnmatchedLocalSnapshot;
        let unmatched_approvals = exact_recognition_approvals(&unmatched, "0404.484.060");
        assert!(
            recognition_evidence_context(&unmatched, &unmatched_approvals, &tender)
                .unwrap()
                .comparison
                .evidence
                .is_empty()
        );
    }

    #[tokio::test]
    async fn recommendation_response_exposes_register_evidence_without_changing_score() {
        let (state, tender_path, kbo_path) = handler_test_state("recognition-response");
        let mut tender = Tender {
            source: "test".to_owned(),
            external_id: "t1".to_owned(),
            title: "Example works".to_owned(),
            ..Tender::default()
        };
        tender.requirements = vec![aanbestedingen_matcher::TenderRequirement {
            text: "Erkenning: C (Wegenbouwkundige werken), Klasse 7".to_owned(),
            kind: "eligibility".to_owned(),
            mandatory: true,
            source_page: None,
        }];
        tender.raw = serde_json::json!({"framework_maximum_amounts": ["2600000.00"]});
        TenderStore::open(&tender_path)
            .unwrap()
            .upsert(&tender)
            .unwrap();
        let query = || {
            Query(RecommendationQuery {
                limit: Some(1),
                ..Default::default()
            })
        };
        let without_snapshot = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            query(),
        )
        .await
        .unwrap();
        let mut snapshot_state = (*state).clone();
        snapshot_state.recognition_snapshot = Some(Arc::new(recognition_snapshot_fixture()));
        let with_snapshot = recommendations(
            State(Arc::new(snapshot_state)),
            Path("0404.484.060".to_owned()),
            query(),
        )
        .await
        .unwrap();
        let before = serde_json::to_value(without_snapshot.0).unwrap();
        let after = serde_json::to_value(with_snapshot.0).unwrap();
        assert_eq!(before[0]["score"], after[0]["score"]);
        assert_eq!(before[0]["estimated_value"], serde_json::Value::Null);
        assert_eq!(before[0]["framework_maximum_amounts"][0], "2600000.00");
        assert!(before[0]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("Currency and lot scope may be available")));
        assert_eq!(after[0]["external_id"], "t1");
        assert_eq!(
            after[0]["recognition_evidence"]["comparison"]["status"],
            "current_dated_approval_found"
        );
        assert_eq!(
            after[0]["recognition_evidence"]["register_as_of"],
            "2026-10-31"
        );
        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn combined_shadow_features_are_opt_in_and_do_not_change_recommendation_order_or_scores()
    {
        let (state, tender_path, kbo_path) = handler_test_state("text-shadow");
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "t1".to_owned(),
            title: "Elektrische installatie en onderhoud gebouwen".to_owned(),
            description: "Uitvoering van elektrische installaties en onderhoud van gebouwen"
                .to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            ..Tender::default()
        };
        let unrelated = Tender {
            source: "test".to_owned(),
            external_id: "t2".to_owned(),
            title: "Office furniture supply".to_owned(),
            ..Tender::default()
        };
        {
            let store = TenderStore::open(&tender_path).unwrap();
            store.upsert(&tender).unwrap();
            store.upsert(&unrelated).unwrap();
        }
        {
            let kbo = duckdb::Connection::open(&kbo_path).unwrap();
            kbo.execute(
                "INSERT INTO kbo_activity VALUES ('0404.484.060', '43210', 'MAIN')",
                [],
            )
            .unwrap();
            kbo.execute(
                "INSERT INTO kbo_code VALUES ('NACE2025', '43210', 'NL', 'Elektrische installatie')",
                [],
            )
            .unwrap();
            kbo.execute(
                "INSERT INTO kbo_code VALUES ('NACE2025', '43210', 'FR', 'Installation électrique')",
                [],
            )
            .unwrap();
        }

        let invoke = |include_activity_text_similarity| {
            recommendations(
                State(Arc::clone(&state)),
                Path("0404.484.060".to_owned()),
                Query(RecommendationQuery {
                    limit: Some(5),
                    include_activity_text_similarity,
                    include_geographic_evidence: include_activity_text_similarity,
                    include_company_kbo_evidence: include_activity_text_similarity,
                    include_company_nbb_evidence: include_activity_text_similarity,
                    include_lot_features: include_activity_text_similarity,
                    ..Default::default()
                }),
            )
        };
        let baseline = invoke(None).await.unwrap().0;
        let shadow = invoke(Some(true)).await.unwrap().0;
        let baseline_json = serde_json::to_value(&baseline).unwrap();
        let shadow_json = serde_json::to_value(&shadow).unwrap();

        let ids = |value: &serde_json::Value| {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["external_id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&baseline_json), ids(&shadow_json));
        for (before, after) in baseline_json
            .as_array()
            .unwrap()
            .iter()
            .zip(shadow_json.as_array().unwrap())
        {
            assert_eq!(before["score"], after["score"]);
            assert!(before.get("shadow_activity_text_similarity").is_none());
            assert!(before
                .get("shadow_activity_text_similarity_coverage")
                .is_none());
            assert!(before.get("shadow_geographic_evidence").is_none());
            assert!(before.get("shadow_company_kbo_evidence").is_none());
            assert!(before.get("shadow_company_nbb_coverage").is_none());
            assert!(before.get("shadow_company_nbb_filings").is_none());
            assert!(before.get("shadow_company_nbb_period_age").is_none());
            assert!(before.get("shadow_company_nbb_reconciliation").is_none());
            assert!(before.get("shadow_lot_features").is_none());
            assert!(after["shadow_geographic_evidence"].is_object());
            assert!(after["shadow_company_kbo_evidence"].is_object());
            assert_eq!(after["shadow_company_nbb_coverage"], "no_filing_stored");
            assert_eq!(after["shadow_company_nbb_filings"], serde_json::json!([]));
            assert!(after["shadow_company_nbb_period_age"].is_object());
            assert!(after["shadow_company_nbb_reconciliation"].is_object());
            assert_eq!(
                after["shadow_lot_features"]["status"],
                "source_not_supported"
            );
        }
        let t1 = shadow_json
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["external_id"] == "t1")
            .unwrap();
        assert!(t1["shadow_activity_text_similarity"]
            .as_f64()
            .is_some_and(|score| score > 0.0));
        assert_eq!(t1["shadow_activity_text_similarity_coverage"], "covered");

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn shadow_features_leave_feedback_adjusted_scores_and_order_unchanged() {
        let (state, tender_path, kbo_path) = handler_test_state("shadow-feedback");
        {
            let store = TenderStore::open(&tender_path).unwrap();
            for (id, title) in [
                ("f1", "Elektrische installatie en onderhoud gebouwen"),
                ("f2", "Elektrische installatie schoolgebouwen"),
            ] {
                store
                    .upsert(&Tender {
                        source: "test".to_owned(),
                        external_id: id.to_owned(),
                        title: title.to_owned(),
                        cpv_codes: vec!["45310000".to_owned()],
                        ..Tender::default()
                    })
                    .unwrap();
            }
        }
        {
            let kbo = duckdb::Connection::open(&kbo_path).unwrap();
            kbo.execute(
                "INSERT INTO kbo_activity VALUES ('0404.484.060', '43210', 'MAIN')",
                [],
            )
            .unwrap();
        }
        let invoke = |shadow: Option<bool>| {
            recommendations(
                State(Arc::clone(&state)),
                Path("0404.484.060".to_owned()),
                Query(RecommendationQuery {
                    limit: Some(5),
                    include_activity_text_similarity: shadow,
                    include_geographic_evidence: shadow,
                    include_company_kbo_evidence: shadow,
                    include_company_nbb_evidence: shadow,
                    include_lot_features: shadow,
                    ..Default::default()
                }),
            )
        };
        let rows = |value: &serde_json::Value| {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|item| (item["external_id"].clone(), item["score"].clone()))
                .collect::<Vec<_>>()
        };
        let before_feedback = serde_json::to_value(invoke(None).await.unwrap().0).unwrap();
        {
            let store = TenderStore::open(&tender_path).unwrap();
            store
                .record_feedback("0404.484.060", "test", "f2", false, None, None, None)
                .unwrap();
        }
        let baseline = serde_json::to_value(invoke(None).await.unwrap().0).unwrap();
        let shadow = serde_json::to_value(invoke(Some(true)).await.unwrap().0).unwrap();
        // The feedback must actually change the baseline, otherwise the
        // comparison below would prove nothing about feedback handling.
        assert_ne!(rows(&before_feedback), rows(&baseline));
        assert_eq!(rows(&baseline), rows(&shadow));
        // The flags must have done something, or equality above proves nothing:
        // every shadow row carries populated evidence that the baseline lacks.
        for (base_row, shadow_row) in baseline
            .as_array()
            .unwrap()
            .iter()
            .zip(shadow.as_array().unwrap())
        {
            assert!(base_row.get("shadow_company_kbo_evidence").is_none());
            assert!(base_row.get("shadow_geographic_evidence").is_none());
            assert!(shadow_row["shadow_company_kbo_evidence"].is_object());
            assert!(shadow_row["shadow_geographic_evidence"].is_object());
            assert!(shadow_row["shadow_activity_text_similarity"].is_number());
        }

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn company_nbb_evidence_is_opt_in_provenance_rich_and_rank_neutral() {
        let (state, tender_path, kbo_path) = handler_test_state("nbb-shadow");
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "t_nbb".to_owned(),
            title: "ICT dienstverlening".to_owned(),
            ..Tender::default()
        };
        let mut second_tender = tender.clone();
        second_tender.external_id = "t_nbb_2".to_owned();
        second_tender.title = "Andere dienstverlening".to_owned();
        let store = TenderStore::open(&tender_path).unwrap();
        store.upsert(&tender).unwrap();
        store.upsert(&second_tender).unwrap();
        drop(store);

        let invoke = |include_company_nbb_evidence| {
            recommendations(
                State(Arc::clone(&state)),
                Path("0404.484.060".to_owned()),
                Query(RecommendationQuery {
                    limit: Some(5),
                    include_company_nbb_evidence,
                    ..Default::default()
                }),
            )
        };
        let baseline = invoke(None).await.unwrap().0;
        let empty_shadow = invoke(Some(true)).await.unwrap().0;
        let empty_json = serde_json::to_value(&empty_shadow).unwrap();
        assert_eq!(
            empty_json[0]["shadow_company_nbb_filings"],
            serde_json::json!([])
        );
        assert_eq!(
            empty_json[0]["shadow_company_nbb_coverage"],
            "no_filing_stored"
        );
        assert_eq!(empty_json[0]["shadow_company_nbb_filings_truncated"], false);
        assert_eq!(
            empty_json[0]["shadow_company_nbb_period_age"]["filings"],
            serde_json::json!([])
        );
        assert_eq!(
            empty_json[0]["shadow_company_nbb_reconciliation"]["company_years"],
            serde_json::json!([])
        );

        TenderStore::open(&tender_path)
            .unwrap()
            .upsert_nbb_financials(&NbbFinancials {
                enterprise_kbo: "0404484060".to_owned(),
                fiscal_year: 2025,
                period_start: Some("2024-01-01".to_owned()),
                period_end: Some("2024-12-31".to_owned()),
                turnover: Some(Decimal::new(250_000, 0)),
                staff_fte: Some(Decimal::new(12, 0)),
                equity: None,
                result: None,
                source_ref: "2025-filing-ref".to_owned(),
                source_format: "xbrl".to_owned(),
                acquisition_method: "api-authentic-query".to_owned(),
            })
            .unwrap();

        let shadow = invoke(Some(true)).await.unwrap().0;
        let baseline_json = serde_json::to_value(&baseline).unwrap();
        let shadow_json = serde_json::to_value(&shadow).unwrap();
        let baseline_rows = baseline_json.as_array().unwrap();
        let shadow_rows = shadow_json.as_array().unwrap();
        assert_eq!(baseline_rows.len(), shadow_rows.len());
        for (baseline_row, shadow_row) in baseline_rows.iter().zip(shadow_rows) {
            assert_eq!(baseline_row["score"], shadow_row["score"]);
            assert_eq!(baseline_row["external_id"], shadow_row["external_id"]);
            assert!(baseline_row.get("shadow_company_nbb_filings").is_none());
            assert!(baseline_row
                .get("shadow_company_nbb_filings_truncated")
                .is_none());
            assert!(baseline_row.get("shadow_company_nbb_coverage").is_none());
            assert!(baseline_row.get("shadow_company_nbb_period_age").is_none());
            assert!(baseline_row
                .get("shadow_company_nbb_reconciliation")
                .is_none());
            assert_eq!(shadow_row["shadow_company_nbb_coverage"], "filings_stored");
        }

        let filing = &shadow_json[0]["shadow_company_nbb_filings"][0];
        assert_eq!(filing["enterprise_kbo"], "0404484060");
        assert_eq!(filing["fiscal_year"], 2025);
        assert_eq!(filing["period_start"], "2024-01-01");
        assert_eq!(filing["period_end"], "2024-12-31");
        assert_eq!(filing["turnover"], "250000.00");
        assert_eq!(filing["staff_fte"], "12.0");
        assert!(filing["equity"].is_null());
        assert!(filing["result"].is_null());
        assert_eq!(filing["source_ref"], "2025-filing-ref");
        assert_eq!(filing["source_format"], "xbrl");
        assert_eq!(filing["acquisition_method"], "api-authentic-query");
        assert!(!filing["stored_at"].as_str().unwrap().is_empty());
        let reconciliation = &shadow_json[0]["shadow_company_nbb_reconciliation"]["company_years"];
        assert_eq!(reconciliation[0]["fiscal_year"], 2025);
        assert_eq!(reconciliation[0]["evidence_count"], 1);
        assert_eq!(
            reconciliation[0]["period_scope_status"],
            "single_explicit_period"
        );
        assert_eq!(reconciliation[0]["status"], "single_reference");
        assert_eq!(
            reconciliation[0]["unresolved_difference_fields"],
            serde_json::json!([])
        );
        assert_eq!(
            reconciliation[0]["incomplete_fields"],
            serde_json::json!(["equity", "result"])
        );
        let period_age = &shadow_json[0]["shadow_company_nbb_period_age"];
        assert_eq!(period_age["as_of_date"].as_str().unwrap().len(), 10);
        assert_eq!(period_age["filings"][0]["source_ref"], "2025-filing-ref");
        assert_eq!(period_age["filings"][0]["fiscal_year"], 2025);
        assert_eq!(period_age["filings"][0]["period_end"], "2024-12-31");
        assert_eq!(period_age["filings"][0]["status"], "period_end_available");
        let as_of =
            NaiveDate::parse_from_str(period_age["as_of_date"].as_str().unwrap(), "%Y-%m-%d")
                .unwrap();
        let period_end = NaiveDate::from_ymd_opt(2024, 12, 31).unwrap();
        assert_eq!(
            period_age["filings"][0]["days_since_period_end"],
            (as_of - period_end).num_days()
        );

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn company_nbb_shadow_projection_is_bounded_and_rank_neutral() {
        let (state, tender_path, kbo_path) = handler_test_state("nbb-shadow-bound");
        let first = Tender {
            source: "test".to_owned(),
            external_id: "nbb_bound_1".to_owned(),
            title: "First recommendation".to_owned(),
            ..Tender::default()
        };
        let mut second = first.clone();
        second.external_id = "nbb_bound_2".to_owned();
        let store = TenderStore::open(&tender_path).unwrap();
        store.upsert(&first).unwrap();
        store.upsert(&second).unwrap();
        for index in 0..=NBB_EVIDENCE_PROJECTION_LIMIT {
            store
                .upsert_nbb_financials(&NbbFinancials {
                    enterprise_kbo: "0404484060".to_owned(),
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
        drop(store);

        let invoke = |include_company_nbb_evidence| {
            recommendations(
                State(Arc::clone(&state)),
                Path("0404.484.060".to_owned()),
                Query(RecommendationQuery {
                    limit: Some(5),
                    include_company_nbb_evidence,
                    ..Default::default()
                }),
            )
        };
        let baseline = serde_json::to_value(&invoke(None).await.unwrap().0).unwrap();
        let shadow = serde_json::to_value(&invoke(Some(true)).await.unwrap().0).unwrap();
        assert_eq!(baseline.as_array().unwrap().len(), 3);
        assert_eq!(
            baseline.as_array().unwrap().len(),
            shadow.as_array().unwrap().len()
        );
        for (before, after) in baseline
            .as_array()
            .unwrap()
            .iter()
            .zip(shadow.as_array().unwrap())
        {
            assert_eq!(before["external_id"], after["external_id"]);
            assert_eq!(before["score"], after["score"]);
            assert!(before.get("shadow_company_nbb_filings_truncated").is_none());
            assert_eq!(
                after["shadow_company_nbb_filings"]
                    .as_array()
                    .unwrap()
                    .len(),
                NBB_EVIDENCE_PROJECTION_LIMIT
            );
            assert_eq!(after["shadow_company_nbb_filings_truncated"], true);
            assert_eq!(
                after["shadow_company_nbb_period_age"]["filings"]
                    .as_array()
                    .unwrap()
                    .len(),
                NBB_EVIDENCE_PROJECTION_LIMIT
            );
            assert_eq!(
                after["shadow_company_nbb_reconciliation"]["company_years"]
                    .as_array()
                    .unwrap()
                    .len(),
                NBB_EVIDENCE_PROJECTION_LIMIT
            );
            assert_eq!(after["shadow_company_nbb_filings"][0]["fiscal_year"], 2025);
            assert_eq!(
                after["shadow_company_nbb_filings"][NBB_EVIDENCE_PROJECTION_LIMIT - 1]
                    ["fiscal_year"],
                2026 - NBB_EVIDENCE_PROJECTION_LIMIT as i64
            );
        }

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn geographic_evidence_is_opt_in_coverage_explaining_and_rank_neutral() {
        let (state, tender_path, kbo_path) = handler_test_state("geography-shadow");
        let matched = Tender {
            source: "test".to_owned(),
            external_id: "geo_match".to_owned(),
            title: "Regional works".to_owned(),
            regions: vec!["Gent".to_owned()],
            ..Tender::default()
        };
        let mut missed = matched.clone();
        missed.external_id = "geo_miss".to_owned();
        missed.regions = vec!["Antwerpen".to_owned()];
        let mut unknown = matched.clone();
        unknown.external_id = "geo_unknown".to_owned();
        unknown.regions.clear();
        let store = TenderStore::open(&tender_path).unwrap();
        store.upsert(&matched).unwrap();
        store.upsert(&missed).unwrap();
        store.upsert(&unknown).unwrap();
        store
            .save_preferences(
                "0404.484.060",
                &CompanyPreferences {
                    preferred_regions: vec!["Gent".to_owned()],
                    ..CompanyPreferences::default()
                },
            )
            .unwrap();
        drop(store);

        let invoke = |include_geographic_evidence| {
            recommendations(
                State(Arc::clone(&state)),
                Path("0404.484.060".to_owned()),
                Query(RecommendationQuery {
                    limit: Some(5),
                    include_geographic_evidence,
                    ..Default::default()
                }),
            )
        };
        let baseline = serde_json::to_value(&invoke(None).await.unwrap().0).unwrap();
        let shadow = serde_json::to_value(&invoke(Some(true)).await.unwrap().0).unwrap();
        let ids = |items: &serde_json::Value| {
            items
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["external_id"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&baseline), ids(&shadow));
        for (before, after) in baseline
            .as_array()
            .unwrap()
            .iter()
            .zip(shadow.as_array().unwrap())
        {
            assert_eq!(before["score"], after["score"]);
            assert!(before.get("shadow_geographic_evidence").is_none());
        }
        let evidence_for = |id: &str| {
            shadow
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["external_id"] == id)
                .unwrap()["shadow_geographic_evidence"]
                .clone()
        };
        assert_eq!(evidence_for("geo_match")["signal"], 1.0);
        assert_eq!(
            evidence_for("geo_match")["status"],
            "preferred_region_match"
        );
        assert_eq!(evidence_for("geo_miss")["signal"], 0.0);
        assert_eq!(
            evidence_for("geo_miss")["status"],
            "preferred_region_no_match"
        );
        assert_eq!(evidence_for("geo_unknown")["signal"], 0.5);
        assert_eq!(evidence_for("geo_unknown")["status"], "no_tender_location");

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn lot_features_are_opt_in_provenance_rich_and_rank_neutral() {
        let (state, tender_path, kbo_path) = handler_test_state("lot-features-shadow");
        let tender = Tender {
            source: "bda".to_owned(),
            external_id: "bda-lot-feature".to_owned(),
            title: "Bridge repair".to_owned(),
            deadline: Some("2099-12-31".to_owned()),
            notice_type: Some("cn-general".to_owned()),
            raw: serde_json::json!({
                "notice_lot_ids_unique": true,
                "framework_maximum_amounts": ["900000.00", "350000.00"],
                "framework_maximum_evidence": [
                    {"amount": "900000.00", "currency_id": "EUR", "scope": "notice", "lot_id": null},
                    {"amount": "350000.00", "currency_id": "EUR", "scope": "lot", "lot_id": "LOT-1"}
                ],
                "notice_lot_cpv_evidence": [
                    {"lot_id": "LOT-1", "cpv_codes": ["45221100"]},
                    {"lot_id": "LOT-2", "cpv_codes": ["71300000"]}
                ],
                "notice_lot_place_evidence": [
                    {"lot_id": "LOT-1", "country_subentity_codes": ["BE231"]},
                    {"lot_id": "LOT-2", "country_subentity_codes": []}
                ],
                "notice_lot_place_evidence_truncated": false,
                "notice_lot_award_criteria_evidence": [
                    {"lot_id": "LOT-1", "criteria": [
                        {"source_element": "AwardingCriterion", "source_ordinal_path": [0], "type_code": null, "description": null, "calculation_expression": null, "parameters": []},
                        {"source_element": "SubordinateAwardingCriterion", "source_ordinal_path": [0, 0], "type_code": "QUALITY", "description": "Service quality", "calculation_expression": null, "parameters": [{"code": "WEIGHT", "numeric": "35"}]}
                    ]},
                    {"lot_id": "LOT-2", "criteria": []}
                ],
                "notice_lot_award_criteria_evidence_truncated": false,
                "notice_lot_selection_evidence": [
                    {"lot_id": "LOT-1", "selection_evidence": [
                        {"source_element": "SpecificTendererRequirement", "source_path": "TenderingTerms/TendererQualificationRequest/SpecificTendererRequirement", "source_ordinal_path": [0, 0], "code": "TECHNICAL_ABILITY", "code_list_name": "UBL selection types", "descriptions": []},
                        {"source_element": "SelectionCriteria", "source_path": "TenderingTerms/UBLExtensions/UBLExtension/ExtensionContent/EformsExtension/SelectionCriteria", "source_ordinal_path": [0, 0], "code": "FINANCIAL_STANDING", "code_list_name": "eForms selection", "descriptions": ["Relevant turnover"]}
                    ]},
                    {"lot_id": "LOT-2", "selection_evidence": []}
                ],
                "notice_lot_selection_evidence_truncated": false,
                "notice_lot_contract_execution_evidence": [
                    {"lot_id": "LOT-1", "conditions": [
                        {"source_element": "ContractExecutionRequirement", "source_path": "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode", "source_ordinal_path": [0], "code": "SAFE_HANDLING", "code_list_name": "Execution condition types"}
                    ]},
                    {"lot_id": "LOT-2", "conditions": []}
                ],
                "notice_lot_contract_execution_evidence_truncated": false,
                "notice_lot_subsequent_process_evidence": [
                    {"lot_id": "LOT-1", "requirements": [{"source_element": "SubsequentProcessTenderRequirement", "source_path": "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name", "source_ordinal_path": [0], "name": "Second-stage technical proposal"}]},
                    {"lot_id": "LOT-2", "requirements": []}
                ],
                "notice_lot_subsequent_process_evidence_truncated": false,
                "notice_lot_guarantee_curricula_evidence": [
                    {"lot_id": "LOT-1", "guarantees": [{"source_element": "RequiredFinancialGuarantee", "source_path": "TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode", "source_ordinal_path": [0], "code": "PERFORMANCE_BOND", "code_list_name": "guarantee types", "description": "Performance guarantee"}], "curricula": [{"source_element": "RequiredCurriculaCode", "source_path": "TenderingTerms/RequiredCurriculaCode", "source_ordinal_path": [0], "code": "CV_TEAM", "code_list_name": "curriculum codes"}]},
                    {"lot_id": "LOT-2", "guarantees": [], "curricula": []}
                ],
                "notice_lot_guarantee_curricula_evidence_truncated": false
            }),
            ..Tender::default()
        };
        {
            let store = TenderStore::open(&tender_path).unwrap();
            store.upsert(&tender).unwrap();
        }
        {
            let connection = duckdb::Connection::open(&kbo_path).unwrap();
            connection
                .execute(
                    "INSERT INTO kbo_activity VALUES ('0404.484.060', '42110', 'MAIN')",
                    [],
                )
                .unwrap();
        }
        let invoke = |include_lot_features| {
            recommendations(
                State(Arc::clone(&state)),
                Path("0404.484.060".to_owned()),
                Query(RecommendationQuery {
                    limit: Some(5),
                    include_lot_features,
                    ..Default::default()
                }),
            )
        };
        let baseline = serde_json::to_value(&invoke(None).await.unwrap().0).unwrap();
        let shadow = serde_json::to_value(&invoke(Some(true)).await.unwrap().0).unwrap();
        let baseline_items = baseline.as_array().unwrap();
        let shadow_items = shadow.as_array().unwrap();
        assert_eq!(baseline_items.len(), shadow_items.len());
        for (before, after) in baseline_items.iter().zip(shadow_items) {
            assert_eq!(before["external_id"], after["external_id"]);
            assert_eq!(before["score"], after["score"]);
            assert!(before.get("shadow_lot_features").is_none());
        }
        let recommendation = shadow_items
            .iter()
            .find(|item| item["external_id"] == "bda-lot-feature")
            .expect("synthetic BDA tender should be returned");
        let evidence = &recommendation["shadow_lot_features"];
        assert_eq!(evidence["status"], "available", "{evidence:#}");
        assert_eq!(evidence["association_rule"], "exact_source_lot_id");
        assert_eq!(evidence["lots"][0]["lot_id"], "LOT-1");
        assert_eq!(
            evidence["lots"][0]["coverage"],
            "cpv_framework_maximum_and_place"
        );
        assert_eq!(evidence["lots"][0]["country_subentity_codes"][0], "BE231");
        assert_eq!(evidence["lots"][0]["place_coverage"], "region_code_present");
        assert_eq!(evidence["lots"][1]["place_coverage"], "no_region_code");
        assert_eq!(
            evidence["lots"][0]["award_criteria"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            evidence["lots"][0]["award_criteria"][1]["description"],
            "Service quality"
        );
        assert_eq!(
            evidence["lots"][0]["award_criteria"][1]["parameters"][0]["numeric"],
            "35"
        );
        assert_eq!(
            evidence["lots"][1]["award_criteria_coverage"],
            "no_criterion_record_retained"
        );
        assert_eq!(
            evidence["lots"][0]["selection_evidence"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            evidence["lots"][0]["selection_evidence"][0]["source_element"],
            "SpecificTendererRequirement"
        );
        assert_eq!(
            evidence["lots"][0]["selection_evidence"][0]["source_path"],
            "TenderingTerms/TendererQualificationRequest/SpecificTendererRequirement"
        );
        assert_eq!(
            evidence["lots"][0]["selection_evidence"][1]["code_list_name"],
            "eForms selection"
        );
        assert_eq!(
            evidence["lots"][0]["selection_evidence"][1]["descriptions"][0],
            "Relevant turnover"
        );
        assert_eq!(
            evidence["lots"][0]["selection_evidence_coverage"],
            "selection_evidence_retained"
        );
        assert_eq!(
            evidence["lots"][1]["selection_evidence_coverage"],
            "no_selection_element_retained"
        );
        assert_eq!(
            evidence["lots"][0]["contract_execution_conditions"][0]["code"],
            "SAFE_HANDLING"
        );
        assert_eq!(
            evidence["lots"][0]["contract_execution_conditions"][0]["source_path"],
            "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode"
        );
        assert_eq!(
            evidence["lots"][0]["contract_execution_conditions_coverage"],
            "execution_conditions_retained"
        );
        assert_eq!(
            evidence["lots"][1]["contract_execution_conditions_coverage"],
            "no_execution_condition_retained"
        );
        assert_eq!(
            evidence["lots"][0]["subsequent_process_requirements"][0]["name"],
            "Second-stage technical proposal"
        );
        assert_eq!(
            evidence["lots"][0]["subsequent_process_requirements_coverage"],
            "subsequent_process_requirements_retained"
        );
        assert_eq!(
            evidence["lots"][1]["subsequent_process_requirements_coverage"],
            "no_subsequent_process_requirement_retained"
        );
        assert_eq!(
            evidence["lots"][0]["required_financial_guarantees"][0]["description"],
            "Performance guarantee"
        );
        assert_eq!(
            evidence["lots"][0]["financial_guarantees_coverage"],
            "financial_guarantees_retained"
        );
        assert_eq!(
            evidence["lots"][0]["required_curricula"][0]["code"],
            "CV_TEAM"
        );
        assert_eq!(
            evidence["lots"][0]["curricula_coverage"],
            "curricula_retained"
        );
        assert_eq!(
            evidence["lots"][1]["financial_guarantees_coverage"],
            "no_financial_guarantee_retained"
        );
        assert_eq!(
            evidence["lots"][1]["curricula_coverage"],
            "no_curriculum_code_retained"
        );
        assert_eq!(evidence["lots"][0]["framework_maximums"][0]["scope"], "lot");
        assert_eq!(
            evidence["lots"][0]["framework_maximums"][0]["amount"],
            "350000.00"
        );
        assert_eq!(
            evidence["lots"][0]["framework_maximums"][0]["currency_id"],
            "EUR"
        );
        assert_eq!(
            evidence["lots"][0]["cpv_nace_overlaps"][0]["nace_codes"][0],
            "4211"
        );
        assert_eq!(evidence["notice_level_framework_maximum_records"], 1);
        assert_eq!(evidence["unprojected_dimensions"], serde_json::json!([]));

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn company_kbo_evidence_is_opt_in_provenance_rich_and_rank_neutral() {
        let (state, tender_path, kbo_path) = handler_test_state("kbo-shadow");
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "t_kbo".to_owned(),
            title: "ICT dienstverlening".to_owned(),
            ..Tender::default()
        };
        let mut other_tender = tender.clone();
        other_tender.external_id = "t_kbo_other".to_owned();
        other_tender.title = "IT support contract".to_owned();
        let store = TenderStore::open(&tender_path).unwrap();
        store.upsert(&tender).unwrap();
        store.upsert(&other_tender).unwrap();
        drop(store);
        {
            let connection = duckdb::Connection::open(&kbo_path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE kbo_source_metadata (Variable VARCHAR, Value VARCHAR);",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO kbo_source_metadata VALUES \
                     ('SnapshotDate', '24-06-2026'), \
                     ('ExtractTimestamp', '25-06-2026 09:17:28'), \
                     ('ExtractType', 'full'), ('ExtractNumber', '401'), ('Version', '1.0.0')",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO kbo_activity VALUES ('0404.484.060', '62010', 'MAIN')",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO kbo_code VALUES ('Nace2025', '62010', 'NL', 'computerprogrammeren')",
                    [],
                )
                .unwrap();
        }

        let invoke = |include_company_kbo_evidence| {
            recommendations(
                State(Arc::clone(&state)),
                Path("0404.484.060".to_owned()),
                Query(RecommendationQuery {
                    limit: Some(5),
                    include_company_kbo_evidence,
                    ..Default::default()
                }),
            )
        };
        let baseline = invoke(None).await.unwrap().0;
        let shadow = invoke(Some(true)).await.unwrap().0;
        let baseline_json = serde_json::to_value(&baseline).unwrap();
        let shadow_json = serde_json::to_value(&shadow).unwrap();
        let baseline_order: Vec<_> = baseline_json
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["external_id"].clone())
            .collect();
        let shadow_order: Vec<_> = shadow_json
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["external_id"].clone())
            .collect();
        assert_eq!(baseline_order, shadow_order);
        for (baseline_item, shadow_item) in baseline_json
            .as_array()
            .unwrap()
            .iter()
            .zip(shadow_json.as_array().unwrap())
        {
            assert_eq!(baseline_item["score"], shadow_item["score"]);
            assert!(baseline_item.get("shadow_company_kbo_evidence").is_none());
        }

        let evidence = &shadow_json[0]["shadow_company_kbo_evidence"];
        assert_eq!(evidence["source"], "kbo_bce");
        assert_eq!(evidence["snapshot"]["snapshot_date"], "24-06-2026");
        assert_eq!(evidence["snapshot"]["extract_number"], "401");
        assert_eq!(evidence["snapshot_age"]["status"], "available");
        let as_of = NaiveDate::parse_from_str(
            evidence["snapshot_age"]["as_of_date"].as_str().unwrap(),
            "%Y-%m-%d",
        )
        .unwrap();
        let source_date = NaiveDate::parse_from_str("24-06-2026", "%d-%m-%Y").unwrap();
        assert_eq!(
            evidence["snapshot_age"]["age_days"],
            (as_of - source_date).num_days()
        );
        assert_eq!(evidence["status"], "AC");
        assert_eq!(evidence["activities"][0]["code"], "62010");
        assert_eq!(evidence["activities"][0]["dutch"], "computerprogrammeren");
        assert!(evidence.get("addresses").is_none());

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn recommendations_report_when_capacity_cannot_be_checked() {
        // Triage requested, no NBB turnover stored, and the tender states a
        // turnover requirement. The response must say the requirement was not
        // checked rather than leaving the reader to infer that it was met.
        let (state, tender_path, kbo_path) = handler_test_state("capacity-unknown-test");
        let mut tender = Tender {
            source: "test".to_owned(),
            external_id: "t_unknown".to_owned(),
            title: "Large IT contract met omzetvereiste".to_owned(),
            ..Tender::default()
        };
        tender.estimated_value = Some(Decimal::new(500_000, 0));
        TenderStore::open(&tender_path)
            .unwrap()
            .upsert(&tender)
            .unwrap();

        let response = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(RecommendationQuery {
                limit: Some(5),
                capacity_triage: Some(true),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let item = response
            .0
            .iter()
            .find(|r| r.recommendation.external_id == "t_unknown")
            .expect("tender found");
        assert!(
            item.recommendation
                .warnings
                .iter()
                .any(|w| w.contains("Capaciteit onbekend")),
            "warnings: {:?}",
            item.recommendation.warnings
        );

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn recommendations_applies_capacity_aware_triage() {
        let (state, tender_path, kbo_path) = handler_test_state("capacity-triage-test");
        let mut tender = Tender {
            source: "test".to_owned(),
            external_id: "t_cap".to_owned(),
            title: "Large IT contract met omzetvereiste".to_owned(),
            ..Tender::default()
        };
        tender.estimated_value = Some(Decimal::new(500_000, 0));
        TenderStore::open(&tender_path)
            .unwrap()
            .upsert(&tender)
            .unwrap();

        // 1. Without capacity: t_cap is retrieved with baseline score
        let baseline = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(RecommendationQuery {
                limit: Some(5),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let baseline_item = baseline
            .0
            .iter()
            .find(|r| r.recommendation.external_id == "t_cap")
            .expect("t_cap found in baseline");
        let baseline_score = baseline_item.recommendation.score;

        // NBB may store a digits-only enterprise number; lookup by the dotted
        // KBO profile number must still find and apply the same-year turnover.
        let nbb = NbbFinancials {
            enterprise_kbo: "0404484060".to_owned(),
            fiscal_year: 2025,
            period_start: None,
            period_end: None,
            turnover: Some(Decimal::new(250_000, 0)),
            staff_fte: Some(Decimal::new(12, 0)),
            equity: Some(Decimal::new(90_000, 0)),
            result: Some(Decimal::new(15_000, 0)),
            source_ref: "2025-test-filing".to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-authentic-query".to_owned(),
        };
        TenderStore::open(&tender_path)
            .unwrap()
            .upsert_nbb_financials(&nbb)
            .unwrap();
        let nbb_triaged = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(RecommendationQuery {
                limit: Some(5),
                capacity_triage: Some(true),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let nbb_item = nbb_triaged
            .0
            .iter()
            .find(|r| r.recommendation.external_id == "t_cap")
            .expect("t_cap found with NBB capacity triage");
        assert_eq!(
            nbb_item.recommendation.score,
            (baseline_score - 30.0).max(0.0)
        );
        assert!(nbb_item
            .recommendation
            .warnings
            .iter()
            .any(|warning| warning.contains("jaaromzet van de kmo")));

        // 2. With max_bid_capacity below 500k: score dropped by 30 and warning added
        let penalized = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(RecommendationQuery {
                limit: Some(5),
                max_bid_capacity: Some(250_000.0),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let item = penalized
            .0
            .iter()
            .find(|r| r.recommendation.external_id == "t_cap")
            .expect("t_cap found in penalized results");
        assert_eq!(item.recommendation.score, (baseline_score - 30.0).max(0.0));
        assert!(item
            .recommendation
            .warnings
            .iter()
            .any(|w| w.contains("Capaciteitsbelemmering: geraamde waarde")));

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn recommendations_refuses_a_negative_filing_age_limit() {
        // The validator is unit-tested; this checks the handler actually calls
        // it before doing any work, rather than withholding every filing.
        let (state, tender_path, kbo_path) = handler_test_state("capacity-negative-limit");
        let result = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(RecommendationQuery {
                limit: Some(5),
                capacity_triage: Some(true),
                max_filing_age_days: Some(-1),
                ..Default::default()
            }),
        )
        .await;
        match result {
            Ok(_) => panic!("a negative limit must be refused"),
            Err(error) => {
                assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
                assert!(error.1.contains("must not be negative"));
            }
        }
        let _ = std::fs::remove_file(tender_path);
        let _ = std::fs::remove_file(kbo_path);
    }

    #[tokio::test]
    async fn recommendations_withholds_turnover_older_than_the_configured_limit() {
        let (state, tender_path, kbo_path) = handler_test_state("capacity-age-limit-test");
        let mut tender = Tender {
            source: "test".to_owned(),
            external_id: "t_age".to_owned(),
            title: "Large IT contract met omzetvereiste".to_owned(),
            ..Tender::default()
        };
        tender.estimated_value = Some(Decimal::new(500_000, 0));
        TenderStore::open(&tender_path)
            .unwrap()
            .upsert(&tender)
            .unwrap();

        // A dated filing, so its age is computable and a limit can act on it.
        let nbb = NbbFinancials {
            enterprise_kbo: "0404484060".to_owned(),
            fiscal_year: 2019,
            period_start: Some("2019-01-01".to_owned()),
            period_end: Some("2019-12-31".to_owned()),
            turnover: Some(Decimal::new(250_000, 0)),
            staff_fte: Some(Decimal::new(12, 0)),
            equity: Some(Decimal::new(90_000, 0)),
            result: Some(Decimal::new(15_000, 0)),
            source_ref: "2019-test-filing".to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-authentic-query".to_owned(),
        };
        TenderStore::open(&tender_path)
            .unwrap()
            .upsert_nbb_financials(&nbb)
            .unwrap();

        let unlimited = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(RecommendationQuery {
                limit: Some(5),
                capacity_triage: Some(true),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let unlimited_item = unlimited
            .0
            .iter()
            .find(|r| r.recommendation.external_id == "t_age")
            .expect("t_age found without a limit");
        assert!(
            unlimited_item
                .recommendation
                .warnings
                .iter()
                .any(|w| w.contains("jaaromzet van de kmo")),
            "unset limit keeps comparing against the stored figure"
        );

        let limited = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(RecommendationQuery {
                limit: Some(5),
                capacity_triage: Some(true),
                max_filing_age_days: Some(730),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let limited_item = limited
            .0
            .iter()
            .find(|r| r.recommendation.external_id == "t_age")
            .expect("t_age found with a limit");
        assert!(
            !limited_item
                .recommendation
                .warnings
                .iter()
                .any(|w| w.contains("jaaromzet van de kmo")),
            "a withheld figure must not produce a comparison"
        );
        assert!(limited_item
            .recommendation
            .warnings
            .iter()
            .any(|w| w.contains("te oud voor de ingestelde drempel")));
        assert!(
            limited_item.recommendation.score > unlimited_item.recommendation.score,
            "lifting the demotion must raise the score"
        );

        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[test]
    fn recognition_snapshot_loader_checks_schema_and_date_metadata() {
        let path = std::env::temp_dir().join(format!(
            "recognition-snapshot-loader-{}-{}.json",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let snapshot = recognition_snapshot_fixture();
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let loaded = load_recognition_snapshot(path.to_str())
            .unwrap()
            .expect("configured snapshot should load");
        assert_eq!(loaded.approval_rows.len(), 1);
        assert_eq!(loaded.kbo_snapshot_as_of, "2026-06-24");

        let mut live_schema = recognition_snapshot_fixture();
        live_schema.schema_version = 3;
        live_schema.approval_rows[0].kbo_match = KboMatch::ExactLiveApi {
            verified_at_utc: "2026-10-31T10:00:00Z".to_owned(),
            current_status: Some("AC".to_owned()),
        };
        std::fs::write(&path, serde_json::to_vec(&live_schema).unwrap()).unwrap();
        assert!(load_recognition_snapshot(path.to_str()).is_ok());

        let mut unsupported = recognition_snapshot_fixture();
        unsupported.schema_version = 4;
        std::fs::write(&path, serde_json::to_vec(&unsupported).unwrap()).unwrap();
        assert!(load_recognition_snapshot(path.to_str()).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn recognition_reference_returns_codes_and_open_top_class_guidance() {
        let response = recognition_reference().await.0;
        assert_eq!(
            response["categories"].as_array().unwrap().len(),
            CATEGORIES.len()
        );
        assert_eq!(
            response["categories"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["code"] == "D17")
                .unwrap()["description_nl"],
            "Centrale verwarming, thermische installaties"
        );
        let class_eight = response["classes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["class"] == 8)
            .unwrap();
        assert!(class_eight["maximum_amount_eur"].is_null());
        assert_eq!(class_eight["above_amount_eur"], 5_330_000);
    }

    /// Staleness is what a large-but-dead store looks like: 2,889 notices with
    /// 23 open ones was reported as healthy before this existed.
    /// The ladder is collapsed before it is returned, so a candidate window
    /// smaller than the requested limit cannot be satisfied — it used to answer
    /// with a short list and no explanation.
    #[test]
    fn recommendation_limit_rejects_a_candidates_window_below_it() {
        let query = |limit, candidates| RecommendationQuery {
            limit,
            candidates,
            ..Default::default()
        };
        let error = recommendation_limit(&query(Some(60), Some(50))).expect_err("short window");
        assert!(
            error.1.contains("candidates must be at least the limit"),
            "got {}",
            error.1
        );
        assert_eq!(
            recommendation_limit(&query(Some(60), Some(100))).unwrap(),
            60
        );
        assert_eq!(recommendation_limit(&query(Some(20), None)).unwrap(), 20);
        assert!(recommendation_limit(&query(Some(0), None)).is_err());
        assert!(recommendation_limit(&query(Some(101), None)).is_err());
    }

    #[test]
    fn prospect_limits_validate_like_recommendation_limits() {
        let query = |limit, candidates| ProspectQuery {
            limit,
            candidates,
            nace: None,
            region: None,
            deep: None,
        };
        assert_eq!(
            prospect_limits(&query(Some(10), Some(50))).unwrap(),
            (10, 50)
        );
        assert_eq!(
            prospect_limits(&query(None, None)).unwrap(),
            (20, DEFAULT_API_PROSPECT_CANDIDATES)
        );
        assert!(prospect_limits(&query(Some(0), None)).is_err());
        assert!(prospect_limits(&query(Some(101), None)).is_err());
        assert!(prospect_limits(&query(None, Some(0))).is_err());
        assert!(prospect_limits(&query(None, Some(MAX_PROSPECT_CANDIDATES + 1))).is_err());
        let error = prospect_limits(&query(Some(60), Some(50))).expect_err("short window");
        assert!(
            error.1.contains("candidates must be at least the limit"),
            "got {}",
            error.1
        );
    }

    #[test]
    fn prospect_nace_parsing_trims_and_validates() {
        assert_eq!(
            parse_prospect_nace("62, 6310").expect("valid prefixes"),
            vec!["62".to_owned(), "6310".to_owned()]
        );
        assert!(parse_prospect_nace("").is_err());
        assert!(parse_prospect_nace("62,123456").is_err());
        assert!(parse_prospect_nace("AB").is_err());
    }

    /// Reverse-search fixtures: one hosting tender (bda/host-1, CPV 72415000)
    /// and two Brussels companies, one IT (63100) and one retail (47110).
    fn prospects_test_state(tag: &str) -> (Arc<ApiState>, std::path::PathBuf, std::path::PathBuf) {
        let tag = format!("aanbestedingen-{tag}-{}", std::process::id());
        let tender_path = std::env::temp_dir().join(format!("{tag}-tenders.duckdb"));
        let kbo_path = std::env::temp_dir().join(format!("{tag}-kbo.duckdb"));
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
        {
            let store = TenderStore::open(&tender_path).expect("tender database should open");
            store
                .upsert(&Tender {
                    source: "bda".to_owned(),
                    external_id: "host-1".to_owned(),
                    title: "Hosting en onderhoud website".to_owned(),
                    description: "hosting onderhoud website ondersteuning".to_owned(),
                    cpv_codes: vec!["72415000".to_owned()],
                    regions: vec!["BE1".to_owned()],
                    ..Tender::default()
                })
                .expect("tender should upsert");
        }
        {
            let connection = duckdb::Connection::open(&kbo_path).expect("kbo database should open");
            connection
                .execute_batch(
                    "CREATE TABLE kbo_enterprise (EnterpriseNumber VARCHAR, Status VARCHAR, JuridicalSituation VARCHAR, JuridicalForm VARCHAR, StartDate DATE);
                     CREATE TABLE kbo_denomination (EntityNumber VARCHAR, Denomination VARCHAR);
                     CREATE TABLE kbo_activity (EntityNumber VARCHAR, NaceCode VARCHAR, Classification VARCHAR);
                     CREATE TABLE kbo_code (Category VARCHAR, Code VARCHAR, Language VARCHAR, Description VARCHAR);
                     CREATE TABLE kbo_address (EntityNumber VARCHAR, TypeOfAddress VARCHAR, Zipcode VARCHAR, MunicipalityNL VARCHAR, MunicipalityFR VARCHAR, StreetNL VARCHAR, StreetFR VARCHAR, HouseNumber VARCHAR, Box VARCHAR, DateStrikingOff DATE);
                     CREATE TABLE kbo_establishment (EnterpriseNumber VARCHAR, EstablishmentNumber VARCHAR);",
                )
                .expect("kbo schema should create");
            connection
                .execute(
                    "INSERT INTO kbo_enterprise VALUES ('0631.997.362', 'AC', NULL, NULL, NULL), ('0471.100.001', 'AC', NULL, NULL, NULL)",
                    [],
                )
                .expect("enterprises should insert");
            connection
                .execute(
                    "INSERT INTO kbo_denomination VALUES ('0631.997.362', 'Hoster'), ('0471.100.001', 'Shop')",
                    [],
                )
                .expect("names should insert");
            connection
                .execute(
                    "INSERT INTO kbo_activity VALUES ('0631.997.362', '63100', 'MAIN'), ('0471.100.001', '47110', 'MAIN')",
                    [],
                )
                .expect("activities should insert");
            connection
                .execute(
                    "INSERT INTO kbo_code VALUES ('Nace2025', '63100', 'NL', 'hosting en gegevensverwerking'), ('Nace2025', '47110', 'NL', 'detailhandel in voeding')",
                    [],
                )
                .expect("codes should insert");
            connection
                .execute(
                    "INSERT INTO kbo_address VALUES ('0631.997.362', 'REGO', '1000', 'Brussel', 'Bruxelles', NULL, NULL, NULL, NULL, NULL), ('0471.100.001', 'REGO', '9000', 'Gent', 'Gand', NULL, NULL, NULL, NULL, NULL)",
                    [],
                )
                .expect("addresses should insert");
        }
        (fresh_state(&tender_path, &kbo_path), tender_path, kbo_path)
    }

    fn prospect_query(limit: Option<usize>, nace: Option<&str>) -> ProspectQuery {
        ProspectQuery {
            limit,
            candidates: None,
            nace: nace.map(str::to_owned),
            region: None,
            deep: None,
        }
    }

    #[tokio::test]
    async fn prospects_rank_it_above_retail_and_reject_bad_queries() {
        let (state, tender_path, kbo_path) = prospects_test_state("prospects");
        let call = |query: ProspectQuery| {
            tender_prospects(
                State(Arc::clone(&state)),
                Path(("bda".to_owned(), "host-1".to_owned())),
                Query(query),
            )
        };
        let prospects = call(prospect_query(Some(10), None))
            .await
            .expect("prospects should succeed")
            .0;
        assert_eq!(
            prospects.len(),
            1,
            "only the IT company pools: {prospects:?}"
        );
        assert_eq!(prospects[0].enterprise_number, "0631.997.362");
        assert!(
            prospects[0].score > 50.0,
            "division match should score: {}",
            prospects[0].score
        );
        // Unknown tender is a 404.
        let missing = tender_prospects(
            State(Arc::clone(&state)),
            Path(("bda".to_owned(), "nope".to_owned())),
            Query(prospect_query(None, None)),
        )
        .await
        .expect_err("unknown tender should fail");
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
        // Bad windows and NACE lists are 422, like the recommendations endpoint.
        let bad = call(ProspectQuery {
            limit: Some(0),
            candidates: None,
            nace: None,
            region: None,
            deep: None,
        })
        .await
        .expect_err("limit 0 should fail");
        assert_eq!(bad.0, StatusCode::UNPROCESSABLE_ENTITY);
        let bad = call(prospect_query(None, Some("62,XX")))
            .await
            .expect_err("bad nace should fail");
        assert_eq!(bad.0, StatusCode::UNPROCESSABLE_ENTITY);
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[tokio::test]
    async fn prospects_deep_reuses_cached_hits_without_calling_out() {
        let (state, tender_path, kbo_path) = prospects_test_state("prospects-deep");
        let (company, tender) = {
            let repository = KboRepository::open(&kbo_path).unwrap();
            let company = repository.lookup("0631.997.362").unwrap().unwrap();
            let store = TenderStore::open_read_only(&tender_path).unwrap();
            let tender = store.tender("bda", "host-1").unwrap().unwrap();
            (company, tender)
        };
        let key = analysis_cache_key(
            DETERMINISTIC_CACHE_IDENTITY,
            &company,
            &tender,
            &[],
            &CompanyPreferences::default(),
        );
        {
            let store = TenderStore::open(&tender_path).unwrap();
            let mut decision = admission_test_decision();
            decision.relevance = Some("strong".to_owned());
            store.save_analysis(&key, &decision).unwrap();
        }
        let plain = tender_prospects(
            State(Arc::clone(&state)),
            Path(("bda".to_owned(), "host-1".to_owned())),
            Query(prospect_query(Some(10), None)),
        )
        .await
        .expect("plain prospects should succeed")
        .0;
        let mut deep_query = prospect_query(Some(10), None);
        deep_query.deep = Some(1);
        let reranked = tender_prospects(
            State(Arc::clone(&state)),
            Path(("bda".to_owned(), "host-1".to_owned())),
            Query(deep_query),
        )
        .await
        .expect("deep prospects should succeed")
        .0;
        assert_eq!(reranked.len(), plain.len());
        assert_eq!(reranked[0].score, plain[0].score + 15.0);
        assert!(reranked[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("strong")));
        // Uncached companies keep their baseline score: the endpoint never
        // computes fresh decisions per request.
        let mut out_of_range = prospect_query(None, None);
        out_of_range.deep = Some(11);
        let bad = tender_prospects(
            State(Arc::clone(&state)),
            Path(("bda".to_owned(), "host-1".to_owned())),
            Query(out_of_range),
        )
        .await
        .expect_err("deep 11 should fail");
        assert_eq!(bad.0, StatusCode::UNPROCESSABLE_ENTITY);
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    /// A cold cache makes `?deep=` a documented no-op: baseline scores, no
    /// error, no marker. Callers who need the rerank warm the cache first
    /// (CLI `--deep` or past analysis calls); review I3.
    #[tokio::test]
    async fn prospects_deep_misses_keep_baseline_scores() {
        let (state, tender_path, kbo_path) = prospects_test_state("prospects-deep-miss");
        let plain = tender_prospects(
            State(Arc::clone(&state)),
            Path(("bda".to_owned(), "host-1".to_owned())),
            Query(prospect_query(Some(10), None)),
        )
        .await
        .expect("plain prospects should succeed")
        .0;
        let mut deep_query = prospect_query(Some(10), None);
        deep_query.deep = Some(5);
        let reranked = tender_prospects(
            State(Arc::clone(&state)),
            Path(("bda".to_owned(), "host-1".to_owned())),
            Query(deep_query),
        )
        .await
        .expect("deep on a cold cache should succeed")
        .0;
        // Contract comparison, not bitwise: the similarity measure sums in
        // hash order, so text_scores can differ by 1 ulp between runs.
        let summary = |prospects: &[CompanyProspect]| {
            prospects
                .iter()
                .map(|prospect| {
                    (
                        prospect.enterprise_number.clone(),
                        prospect.score,
                        prospect.reasons.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(summary(&reranked), summary(&plain));
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[tokio::test]
    async fn prospects_without_cpv_need_an_explicit_nace_pool() {
        let (state, tender_path, kbo_path) = handler_test_state("prospects-no-cpv");
        // The shared fixture tender (test/t1) carries no CPV codes.
        let missing_pool = tender_prospects(
            State(Arc::clone(&state)),
            Path(("test".to_owned(), "t1".to_owned())),
            Query(prospect_query(None, None)),
        )
        .await
        .expect_err("CPV-less tender without ?nace should fail");
        assert_eq!(missing_pool.0, StatusCode::UNPROCESSABLE_ENTITY);
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[test]
    fn nbb_period_age_reports_source_date_missingness_without_stale_verdicts() {
        let evidence = |source_ref: &str, period_end: Option<&str>| {
            aanbestedingen_matcher::store::NbbFinancialEvidence {
                enterprise_kbo: "0123456789".to_owned(),
                fiscal_year: 2024,
                period_start: None,
                period_end: period_end.map(str::to_owned),
                turnover: None,
                staff_fte: None,
                equity: None,
                result: None,
                source_ref: source_ref.to_owned(),
                source_format: "xbrl".to_owned(),
                acquisition_method: "synthetic-test".to_owned(),
                stored_at: "2025-01-01 00:00:00+00".to_owned(),
            }
        };
        let filings = vec![
            evidence("available", Some("2024-12-31")),
            evidence("missing", None),
            evidence("unparseable", Some("31-12-2024")),
            evidence("after-as-of", Some("2025-01-17")),
        ];
        let as_of = NaiveDate::from_ymd_opt(2025, 1, 15).unwrap();
        let age = nbb_period_age(&filings, as_of);
        assert_eq!(age.as_of_date, "2025-01-15");
        assert_eq!(age.filings[0].days_since_period_end, Some(15));
        assert_eq!(age.filings[0].status, "period_end_available");
        assert_eq!(age.filings[1].days_since_period_end, None);
        assert_eq!(age.filings[1].status, "period_end_missing");
        assert_eq!(age.filings[2].days_since_period_end, None);
        assert_eq!(age.filings[2].status, "period_end_unparseable");
        assert_eq!(age.filings[3].days_since_period_end, Some(-2));
        assert_eq!(age.filings[3].status, "period_end_after_as_of");
        assert!(age
            .filings
            .iter()
            .all(|filing| !filing.status.contains("stale")));
    }

    #[test]
    fn nbb_reconciliation_flags_conflicts_without_treating_nulls_as_values() {
        use aanbestedingen_matcher::store::NbbFinancialEvidence;

        let evidence =
            |source_ref: &str, fiscal_year: i64, turnover, equity, result| NbbFinancialEvidence {
                enterprise_kbo: "0123456789".to_owned(),
                fiscal_year,
                period_start: Some("2024-01-01".to_owned()),
                period_end: Some("2024-12-31".to_owned()),
                turnover,
                staff_fte: Some(Decimal::new(5, 0)),
                equity,
                result,
                source_ref: source_ref.to_owned(),
                source_format: "xbrl".to_owned(),
                acquisition_method: "synthetic-test".to_owned(),
                stored_at: "2025-01-01 00:00:00+00".to_owned(),
            };
        let filings = vec![
            evidence(
                "conflict-a",
                2025,
                Some(Decimal::new(100, 0)),
                Some(Decimal::new(40, 0)),
                Some(Decimal::new(8, 0)),
            ),
            evidence(
                "conflict-b",
                2025,
                Some(Decimal::new(100, 0)),
                None,
                Some(Decimal::new(9, 0)),
            ),
            evidence("single", 2024, None, None, None),
        ];

        let reconciliation = nbb_reconciliation(&filings);
        assert_eq!(reconciliation.company_years.len(), 2);
        let current = &reconciliation.company_years[0];
        assert_eq!(current.fiscal_year, 2025);
        assert_eq!(current.evidence_count, 2);
        assert_eq!(current.period_scope_status, "single_explicit_period");
        assert_eq!(current.status, "conflicting_nonnull_values");
        assert_eq!(current.conflicting_fields, vec!["result"]);
        assert!(current.unresolved_difference_fields.is_empty());
        assert_eq!(current.incomplete_fields, vec!["equity"]);

        let prior = &reconciliation.company_years[1];
        assert_eq!(prior.fiscal_year, 2024);
        assert_eq!(prior.evidence_count, 1);
        assert_eq!(prior.period_scope_status, "single_explicit_period");
        assert_eq!(prior.status, "single_reference");
        assert!(prior.conflicting_fields.is_empty());
        assert_eq!(
            prior.incomplete_fields,
            vec!["turnover", "equity", "result"]
        );

        let mut earlier_period = evidence(
            "period-a",
            2023,
            Some(Decimal::new(100, 0)),
            None,
            Some(Decimal::new(8, 0)),
        );
        earlier_period.period_start = Some("2022-07-01".to_owned());
        earlier_period.period_end = Some("2023-06-30".to_owned());
        let later_period = evidence(
            "period-b",
            2023,
            Some(Decimal::new(120, 0)),
            None,
            Some(Decimal::new(10, 0)),
        );
        let different_periods = nbb_reconciliation(&[earlier_period, later_period]);
        assert_eq!(
            different_periods.company_years[0].period_scope_status,
            "multiple_explicit_periods"
        );
        assert_eq!(
            different_periods.company_years[0].status,
            "differences_unresolved_by_period_scope"
        );
        assert!(different_periods.company_years[0]
            .conflicting_fields
            .is_empty());
        assert_eq!(
            different_periods.company_years[0].unresolved_difference_fields,
            vec!["result", "turnover"]
        );

        let mut unknown_a = evidence(
            "unknown-a",
            2022,
            Some(Decimal::new(100, 0)),
            None,
            Some(Decimal::new(8, 0)),
        );
        unknown_a.period_start = None;
        unknown_a.period_end = None;
        let mut unknown_b = evidence(
            "unknown-b",
            2022,
            Some(Decimal::new(120, 0)),
            None,
            Some(Decimal::new(10, 0)),
        );
        unknown_b.period_start = None;
        unknown_b.period_end = None;
        let unknown_periods = nbb_reconciliation(&[unknown_a, unknown_b]);
        assert_eq!(
            unknown_periods.company_years[0].period_scope_status,
            "period_boundaries_unknown"
        );
        assert_eq!(
            unknown_periods.company_years[0].status,
            "differences_unresolved_by_period_scope"
        );
        assert!(unknown_periods.company_years[0]
            .conflicting_fields
            .is_empty());
        assert_eq!(
            unknown_periods.company_years[0].unresolved_difference_fields,
            vec!["result", "turnover"]
        );
    }

    #[test]
    fn kbo_snapshot_age_reports_source_age_and_missingness_without_verdicts() {
        let as_of = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let missing_table = kbo_snapshot_age(None, as_of);
        assert_eq!(missing_table.status, "metadata_table_missing");
        assert_eq!(missing_table.age_days, None);

        let missing_date = aanbestedingen_matcher::kbo::KboSnapshotMetadata {
            snapshot_date: None,
            extract_timestamp: Some("timestamp exists".to_owned()),
            extract_type: None,
            extract_number: None,
            version: None,
        };
        assert_eq!(
            kbo_snapshot_age(Some(&missing_date), as_of).status,
            "snapshot_date_missing"
        );

        let unparseable = aanbestedingen_matcher::kbo::KboSnapshotMetadata {
            snapshot_date: Some("not-a-date".to_owned()),
            extract_timestamp: None,
            extract_type: None,
            extract_number: None,
            version: None,
        };
        let invalid = kbo_snapshot_age(Some(&unparseable), as_of);
        assert_eq!(invalid.status, "snapshot_date_unparseable");
        assert_eq!(invalid.age_days, None);

        let available = aanbestedingen_matcher::kbo::KboSnapshotMetadata {
            snapshot_date: Some("24-06-2026".to_owned()),
            extract_timestamp: None,
            extract_type: None,
            extract_number: None,
            version: None,
        };
        let age = kbo_snapshot_age(Some(&available), as_of);
        assert_eq!(age.status, "available");
        assert_eq!(age.as_of_date, "2026-07-01");
        assert_eq!(age.age_days, Some(7));
    }

    #[test]
    fn freshness_is_fresh_stale_or_unknown_by_evidence() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 19).expect("valid date");
        let fresh = (Some("2026-08-01"), Some("2026-10-18"), 0);
        assert_eq!(
            freshness_verdict(Some("2026-10-18 06:00:00+00"), 980, fresh, 6000, today),
            FreshnessVerdict::Fresh
        );
        // Old fetch, old coverage, empty ladder, unparseable or absent times.
        assert_eq!(
            freshness_verdict(Some("2026-10-01 06:00:00+00"), 980, fresh, 6000, today),
            FreshnessVerdict::Stale
        );
        assert_eq!(
            freshness_verdict(
                Some("2026-10-18 06:00:00+00"),
                980,
                (Some("2026-08-01"), Some("2026-08-02"), 0),
                6000,
                today
            ),
            FreshnessVerdict::Stale
        );
        assert_eq!(
            freshness_verdict(Some("2026-10-18 06:00:00+00"), 0, fresh, 6000, today),
            FreshnessVerdict::Stale
        );
        assert_eq!(
            freshness_verdict(None, 980, fresh, 6000, today),
            FreshnessVerdict::Unknown
        );
        assert_eq!(
            freshness_verdict(
                Some("2026-10-18 06:00:00+00"),
                980,
                (None, None, 2889),
                2889,
                today
            ),
            FreshnessVerdict::Unknown
        );
        assert_eq!(
            serde_json::to_value(FreshnessVerdict::Fresh).unwrap(),
            "fresh"
        );
        assert_eq!(
            serde_json::to_value(FreshnessVerdict::Stale).unwrap(),
            "stale"
        );
        assert_eq!(
            serde_json::to_value(FreshnessVerdict::Unknown).unwrap(),
            "unknown"
        );
    }

    #[test]
    fn only_loopback_may_serve_without_a_token() {
        for address in ["127.0.0.1:3000", "[::1]:3000", "127.5.5.5:8080"] {
            check_bind_address(address, None)
                .unwrap_or_else(|error| panic!("{address} is loopback: {error}"));
        }
        for address in [
            "0.0.0.0:3000",
            ":::3000",
            "[::]:3000",
            "100.116.127.30:18000",
            "192.168.1.158:18000",
            "localhost:3000",
            "example.invalid:80",
        ] {
            let error = check_bind_address(address, None)
                .expect_err("a reachable address must not serve without a token");
            assert!(
                error.to_string().contains("AANBESTEDINGEN_API_TOKEN"),
                "unexpected error for {address}: {error}"
            );
            check_bind_address(address, Some("token")).unwrap_or_else(|error| {
                panic!("{address} with a token should be allowed: {error}")
            });
        }
    }

    fn arguments(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn blank_api_token_means_open_api() {
        assert_eq!(normalize_api_token(None), None);
        assert_eq!(normalize_api_token(Some(String::new())), None);
        assert_eq!(normalize_api_token(Some("   ".to_owned())), None);
        assert_eq!(
            normalize_api_token(Some("  secret  ".to_owned())),
            Some("secret".to_owned())
        );
    }

    #[test]
    fn bearer_check_is_exact_and_open_without_token() {
        use axum::http::HeaderMap;
        let open = HeaderMap::new();
        assert!(is_authorized(&open, None));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer secret".parse().expect("header should parse"),
        );
        assert!(is_authorized(&headers, Some("secret")));
        assert!(!is_authorized(&headers, Some("other")));
        assert!(!is_authorized(&open, Some("secret")));
        let mut wrong_scheme = HeaderMap::new();
        wrong_scheme.insert(
            header::AUTHORIZATION,
            "Token secret".parse().expect("header should parse"),
        );
        assert!(!is_authorized(&wrong_scheme, Some("secret")));
    }

    #[test]
    fn unauthorized_response_carries_a_bearer_challenge() {
        use axum::response::IntoResponse;

        let response = ApiError::unauthorized().into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer")
        );
        // Only 401s challenge the client.
        let missing = ApiError::not_found("company not found").into_response();
        assert!(!missing.headers().contains_key(header::WWW_AUTHENTICATE));
    }

    #[tokio::test]
    async fn error_bodies_use_the_reference_detail_key() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse as _;

        let response = ApiError::not_found("company not found").into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let value: Value = serde_json::from_slice(&body).expect("body should be json");
        assert_eq!(value, json!({"detail": "company not found"}));
    }

    #[test]
    fn uses_default_options() {
        assert_eq!(
            parse_args(arguments(&[])).expect("arguments should parse"),
            Some(ApiOptions {
                kbo_database: "data/kbo/kbo_enrich.duckdb".to_owned(),
                tender_database: "data/tenders-rust.duckdb".to_owned(),
                address: "127.0.0.1:3000".to_owned(),
                web_dir: None,
                recognition_snapshot: None,
            })
        );
    }

    #[test]
    fn accepts_explicit_options() {
        assert_eq!(
            parse_args(arguments(&[
                "--address",
                "127.0.0.1:4000",
                "--kbo-database",
                "custom-kbo.duckdb",
                "--tender-database",
                "custom-tenders.duckdb",
                "--recognition-snapshot",
                "recognized.json",
            ]))
            .expect("arguments should parse"),
            Some(ApiOptions {
                kbo_database: "custom-kbo.duckdb".to_owned(),
                tender_database: "custom-tenders.duckdb".to_owned(),
                address: "127.0.0.1:4000".to_owned(),
                web_dir: None,
                recognition_snapshot: Some("recognized.json".to_owned()),
            })
        );
    }

    #[test]
    fn accepts_an_explicit_web_dir() {
        assert_eq!(
            parse_args(arguments(&["--web-dir", "site"])).expect("arguments should parse"),
            Some(ApiOptions {
                kbo_database: "data/kbo/kbo_enrich.duckdb".to_owned(),
                tender_database: "data/tenders-rust.duckdb".to_owned(),
                address: "127.0.0.1:3000".to_owned(),
                web_dir: Some("site".to_owned()),
                recognition_snapshot: None,
            })
        );
    }

    #[test]
    fn accepts_help_without_options() {
        for flag in ["--help", "-h"] {
            assert!(parse_args(arguments(&[flag]))
                .expect("help should parse")
                .is_none());
        }
    }

    #[test]
    fn rejects_missing_values() {
        assert!(parse_args(arguments(&["--address"])).is_err());
        assert!(parse_args(arguments(&["--kbo-database"])).is_err());
        assert!(parse_args(arguments(&["--tender-database"])).is_err());
        assert!(parse_args(arguments(&["--web-dir"])).is_err());
        assert!(parse_args(arguments(&["--recognition-snapshot"])).is_err());
    }

    #[test]
    fn an_explicit_web_dir_is_used_verbatim() {
        assert_eq!(
            resolve_web_dir(Some("site".to_owned())),
            "site",
            "an explicit directory must not be overridden by a checkout probe"
        );
    }

    #[test]
    fn the_checkout_that_ships_the_dashboard_is_recognised() {
        let root = FilePath::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the manifest lives in rust/");
        assert!(
            is_checkout_root(root),
            "{} should be recognised as the checkout",
            root.display()
        );
        assert!(!is_checkout_root(FilePath::new("web")));
    }

    #[test]
    fn a_partial_tree_is_not_mistaken_for_the_checkout() {
        let base = env::temp_dir().join(format!("api-web-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("web")).expect("web dir should be creatable");
        std::fs::create_dir_all(base.join("rust")).expect("rust dir should be creatable");
        std::fs::write(base.join("rust").join("Cargo.toml"), "# stub")
            .expect("manifest should be writable");
        assert!(
            !is_checkout_root(&base),
            "a web/ and rust/Cargo.toml without the justfile is not the checkout"
        );
        std::fs::write(base.join("justfile"), "# stub").expect("justfile should be writable");
        assert!(is_checkout_root(&base), "all three markers must qualify");
        std::fs::remove_dir_all(&base).expect("temp tree should be removable");
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(arguments(&["--unknown"])).is_err());
        assert!(parse_args(arguments(&["positional"])).is_err());
    }

    fn admission_test_decision() -> AnalysisDecision {
        AnalysisDecision {
            provider: "test".into(),
            activity_match: None,
            geography_match: None,
            previous_work_match: None,
            qualification_risk: None,
            relevance: None,
            raw: Default::default(),
            prompt_version: None,
            cached: false,
        }
    }

    #[test]
    fn analysis_admission_rejects_duplicates_and_bounds_work_through_persistence() {
        use std::sync::mpsc;
        use std::time::Duration;
        let state = fresh_state(FilePath::new("unused"), FilePath::new("unused"));
        let calls = Arc::new(AtomicU64::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let mut releases = Vec::new();
        let mut workers = Vec::new();
        for index in 0..MAX_CONCURRENT_ANALYSES {
            let state = state.clone();
            let calls = calls.clone();
            let entered_tx = entered_tx.clone();
            let (release_tx, release_rx) = mpsc::channel();
            releases.push(release_tx);
            workers.push(std::thread::spawn(move || {
                analyze_cache_miss(
                    &state,
                    &format!("key-{index}"),
                    || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(admission_test_decision())
                    },
                    |_| {
                        entered_tx.send(index).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(())
                    },
                )
                .unwrap()
            }));
        }
        for _ in 0..MAX_CONCURRENT_ANALYSES {
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        let duplicate = analyze_cache_miss(
            &state,
            "key-0",
            || panic!("duplicate computation"),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(duplicate.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(duplicate.1.contains("already in progress"));
        let busy = analyze_cache_miss(
            &state,
            "another",
            || panic!("over-capacity computation"),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(busy.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(busy.1.contains("capacity"));
        assert_eq!(calls.load(Ordering::SeqCst), MAX_CONCURRENT_ANALYSES as u64);
        for release in releases {
            release.send(()).unwrap();
        }
        for worker in workers {
            assert!(!worker.join().unwrap().cached);
        }
        let reused = analyze_cache_miss(
            &state,
            "key-0",
            || panic!("cached computation"),
            |_| panic!("cached persistence"),
        )
        .unwrap();
        assert!(reused.cached);
        assert!(state.analysis_admission.active.lock().unwrap().is_empty());
    }

    #[test]
    fn analysis_admission_releases_after_errors_and_panics() {
        let state = fresh_state(FilePath::new("unused"), FilePath::new("unused"));
        let error = analyze_cache_miss(
            &state,
            "retry",
            || Err(ApiError::bad_gateway("provider failure")),
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
        assert!(state.analysis_admission.active.lock().unwrap().is_empty());
        let error = analyze_cache_miss(
            &state,
            "retry",
            || Ok(admission_test_decision()),
            |_| Err(ApiError::internal("persistence failure")),
        )
        .unwrap_err();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(state.analysis_admission.active.lock().unwrap().is_empty());
        assert!(state.analysis_cache.lock().unwrap().get("retry").is_none());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            analyze_cache_miss(&state, "retry", || panic!("provider panic"), |_| Ok(()))
        }));
        assert!(panicked.is_err());
        assert!(state.analysis_admission.active.lock().unwrap().is_empty());
        let result = analyze_cache_miss(
            &state,
            "retry",
            || Ok(admission_test_decision()),
            |_| Ok(()),
        )
        .unwrap();
        assert!(!result.cached);
    }

    #[test]
    fn in_memory_cache_stays_bounded() {
        let decision = AnalysisDecision {
            provider: "test".to_owned(),
            activity_match: Some(0.0),
            geography_match: Some(0.0),
            previous_work_match: Some(0.0),
            qualification_risk: Some("none".to_owned()),
            relevance: Some("possible".to_owned()),
            raw: std::collections::BTreeMap::new(),
            prompt_version: None,
            cached: false,
        };
        let mut cache = AnalysisCache::default();
        cache.insert("frequently-used".to_owned(), decision.clone());
        for index in 0..MAX_ANALYSIS_CACHE_ENTRIES - 1 {
            cache.insert(index.to_string(), decision.clone());
        }
        assert_eq!(cache.len(), MAX_ANALYSIS_CACHE_ENTRIES);
        assert!(cache.get("frequently-used").is_some());
        cache.insert("new-entry".to_owned(), decision);
        assert_eq!(cache.len(), MAX_ANALYSIS_CACHE_ENTRIES);
        assert!(cache.get("frequently-used").is_some());
        assert!(cache.get("0").is_none());
        let mut large = cache.get("frequently-used").unwrap();
        large.raw.insert("extra".into(), json!("x".repeat(70_000)));
        cache.insert("oversized".into(), large.clone());
        assert!(cache.get("oversized").is_none());
        cache.insert("frequently-used".into(), large);
        assert!(cache.get("frequently-used").unwrap().raw.is_empty());
        assert_eq!(cache.len(), MAX_ANALYSIS_CACHE_ENTRIES);
    }

    #[test]
    fn hides_internal_error_details_from_responses() {
        let error = ApiError::internal("database path and SQL details");
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.1, "internal server error");
    }

    #[test]
    fn reports_degraded_health_when_kbo_is_unavailable() {
        assert_eq!(health_status(true), ServiceHealthStatus::Ok);
        assert_eq!(health_status(false), ServiceHealthStatus::Degraded);
        assert_eq!(serde_json::to_value(ServiceHealthStatus::Ok).unwrap(), "ok");
        assert_eq!(
            serde_json::to_value(ServiceHealthStatus::Degraded).unwrap(),
            "degraded"
        );
    }

    #[tokio::test]
    async fn analysis_responses_report_cache_hits() {
        let tag = format!("aanbestedingen-analysis-cached-{}", std::process::id());
        let tender_path = std::env::temp_dir().join(format!("{tag}-tenders.duckdb"));
        let kbo_path = std::env::temp_dir().join(format!("{tag}-kbo.duckdb"));
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
        {
            let store = TenderStore::open(&tender_path).expect("tender database should open");
            store
                .upsert(&Tender {
                    source: "test".to_owned(),
                    external_id: "t1".to_owned(),
                    title: "Example works".to_owned(),
                    ..Tender::default()
                })
                .expect("tender should upsert");
        }
        {
            let connection = duckdb::Connection::open(&kbo_path).expect("kbo database should open");
            connection
                .execute_batch(
                    "CREATE TABLE kbo_enterprise (EnterpriseNumber VARCHAR, Status VARCHAR, JuridicalSituation VARCHAR, JuridicalForm VARCHAR, StartDate DATE);
                     CREATE TABLE kbo_denomination (EntityNumber VARCHAR, Denomination VARCHAR);
                     CREATE TABLE kbo_activity (EntityNumber VARCHAR, NaceCode VARCHAR, Classification VARCHAR);
                     CREATE TABLE kbo_code (Category VARCHAR, Code VARCHAR, Language VARCHAR, Description VARCHAR);
                     CREATE TABLE kbo_address (EntityNumber VARCHAR, TypeOfAddress VARCHAR, Zipcode VARCHAR, MunicipalityNL VARCHAR, MunicipalityFR VARCHAR, StreetNL VARCHAR, StreetFR VARCHAR, HouseNumber VARCHAR, Box VARCHAR, DateStrikingOff DATE);
                     CREATE TABLE kbo_establishment (EnterpriseNumber VARCHAR, EstablishmentNumber VARCHAR);",
                )
                .expect("kbo schema should create");
            connection
                .execute(
                    "INSERT INTO kbo_enterprise VALUES ('0404.484.060', 'AC', NULL, NULL, '2020-01-01')",
                    [],
                )
                .expect("kbo enterprise should insert");
            connection
                .execute(
                    "INSERT INTO kbo_denomination VALUES ('0404.484.060', 'Example')",
                    [],
                )
                .expect("kbo denomination should insert");
        }
        let state = Arc::new(ApiState {
            kbo_database: kbo_path.to_string_lossy().into_owned(),
            tender_database: tender_path.to_string_lossy().into_owned(),
            api_token: None,
            recognition_snapshot: None,
            analysis: None,
            analysis_admission: AnalysisAdmission::default(),
            analysis_cache: Arc::new(Mutex::new(AnalysisCache::default())),
            cache_metrics: Arc::new(CacheMetrics {
                memory_hits: AtomicU64::new(0),
                persistent_hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                evictions: AtomicU64::new(0),
            }),
        });
        let analyze = || {
            tender_analysis(
                State(Arc::clone(&state)),
                Path((
                    "0404.484.060".to_owned(),
                    "test".to_owned(),
                    "t1".to_owned(),
                )),
            )
        };
        let first = analyze().await.expect("first analysis should succeed");
        assert!(!first.0.cached);
        let second = analyze().await.expect("cached analysis should succeed");
        assert!(second.0.cached);
        assert_eq!(first.0.provider, second.0.provider);
        assert_eq!(first.0.relevance, second.0.relevance);
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    /// A fresh [`ApiState`] over the given databases with empty caches and
    /// metrics, shared by the handler tests below.
    fn fresh_state(tender_path: &std::path::Path, kbo_path: &std::path::Path) -> Arc<ApiState> {
        Arc::new(ApiState {
            kbo_database: kbo_path.to_string_lossy().into_owned(),
            tender_database: tender_path.to_string_lossy().into_owned(),
            api_token: None,
            recognition_snapshot: None,
            analysis: None,
            analysis_admission: AnalysisAdmission::default(),
            analysis_cache: Arc::new(Mutex::new(AnalysisCache::default())),
            cache_metrics: Arc::new(CacheMetrics {
                memory_hits: AtomicU64::new(0),
                persistent_hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                evictions: AtomicU64::new(0),
            }),
        })
    }

    /// Handler-test databases seeded with one company (0404.484.060) and one
    /// tender (test/t1), returned with their paths so callers can clean up.
    /// Running the real handlers against them keeps these tests honest about
    /// the write path instead of mocking it.
    fn handler_test_state(tag: &str) -> (Arc<ApiState>, std::path::PathBuf, std::path::PathBuf) {
        let tag = format!("aanbestedingen-{tag}-{}", std::process::id());
        let tender_path = std::env::temp_dir().join(format!("{tag}-tenders.duckdb"));
        let kbo_path = std::env::temp_dir().join(format!("{tag}-kbo.duckdb"));
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
        {
            let store = TenderStore::open(&tender_path).expect("tender database should open");
            store
                .upsert(&Tender {
                    source: "test".to_owned(),
                    external_id: "t1".to_owned(),
                    title: "Example works".to_owned(),
                    ..Tender::default()
                })
                .expect("tender should upsert");
        }
        {
            let connection = duckdb::Connection::open(&kbo_path).expect("kbo database should open");
            connection
                .execute_batch(
                    "CREATE TABLE kbo_enterprise (EnterpriseNumber VARCHAR, Status VARCHAR, JuridicalSituation VARCHAR, JuridicalForm VARCHAR, StartDate DATE);
                     CREATE TABLE kbo_denomination (EntityNumber VARCHAR, Denomination VARCHAR);
                     CREATE TABLE kbo_activity (EntityNumber VARCHAR, NaceCode VARCHAR, Classification VARCHAR);
                     CREATE TABLE kbo_code (Category VARCHAR, Code VARCHAR, Language VARCHAR, Description VARCHAR);
                     CREATE TABLE kbo_address (EntityNumber VARCHAR, TypeOfAddress VARCHAR, Zipcode VARCHAR, MunicipalityNL VARCHAR, MunicipalityFR VARCHAR, StreetNL VARCHAR, StreetFR VARCHAR, HouseNumber VARCHAR, Box VARCHAR, DateStrikingOff DATE);
                     CREATE TABLE kbo_establishment (EnterpriseNumber VARCHAR, EstablishmentNumber VARCHAR);",
                )
                .expect("kbo schema should create");
            connection
                .execute(
                    "INSERT INTO kbo_enterprise VALUES ('0404.484.060', 'AC', NULL, NULL, '2020-01-01')",
                    [],
                )
                .expect("kbo enterprise should insert");
            connection
                .execute(
                    "INSERT INTO kbo_denomination VALUES ('0404.484.060', 'Example')",
                    [],
                )
                .expect("kbo denomination should insert");
        }
        (fresh_state(&tender_path, &kbo_path), tender_path, kbo_path)
    }

    #[tokio::test]
    async fn company_awards_distinguishes_bosa_lot_evidence_from_ted_values() {
        let (state, tender_path, kbo_path) = handler_test_state("company-awards");
        let bosa = AwardRecord {
            publication_number: "bosa:workspace:notice".to_owned(),
            title: "Road works".to_owned(),
            buyer: Some("Buyer".to_owned()),
            winner_identifiers: vec!["0404.484.060".to_owned()],
            winner_names: vec!["Example NV".to_owned()],
            cpv_codes: vec!["45233100".to_owned()],
            values: vec![Decimal::new(12500, 2)],
            links: vec![],
            raw: json!({
                "source": "bosa",
                "winning_lots": [{"lot_id": "LOT-0001", "participation": "contract-linked-winner"}],
                "bosa_publication_reference_numbers_bda": ["BDA-123_pub01"]
            }),
        };
        let ted = AwardRecord {
            publication_number: "642950-2026".to_owned(),
            title: "IT services".to_owned(),
            buyer: Some("TED buyer".to_owned()),
            winner_identifiers: vec!["0404484060".to_owned()],
            winner_names: vec!["Example NV".to_owned()],
            cpv_codes: vec!["72000000".to_owned()],
            values: vec![Decimal::new(584550, 0)],
            links: vec!["https://ted.europa.eu/en/notice/642950-2026/xml".to_owned()],
            raw: json!({"publication-number": "642950-2026"}),
        };
        {
            let store = TenderStore::open(&tender_path).unwrap();
            assert_eq!(
                store
                    .upsert_awards_many("0404.484.060", [&bosa, &ted])
                    .unwrap(),
                2
            );
        }

        let response = company_awards(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(InventoryQuery { limit: Some(10) }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(
            response["note"],
            "BOSA records are listed first, then TED; BOSA lot evidence is shown without supplier-level amounts"
        );
        let records = response["awards"].as_array().unwrap();
        let bosa = records
            .iter()
            .find(|record| record["source"] == "BOSA")
            .unwrap();
        assert_eq!(bosa["values"], json!([]));
        assert_eq!(bosa["lot_evidence"][0]["lot_id"], "LOT-0001");
        assert_eq!(bosa["publication_references"][0], "BDA-123_pub01");
        assert!(bosa["amount_note"].as_str().unwrap().contains("omitted"));
        let ted = records
            .iter()
            .find(|record| record["source"] == "TED")
            .unwrap();
        assert_eq!(ted["values"][0], json!("584550"));
        assert_eq!(
            ted["links"][0],
            "https://ted.europa.eu/en/notice/642950-2026/xml"
        );

        let limited = company_awards(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            Query(InventoryQuery { limit: Some(1) }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(limited["awards"].as_array().unwrap().len(), 1);
        assert_eq!(limited["awards"][0]["source"], "BOSA");
        assert_eq!(
            limited["note"],
            "BOSA lot evidence is shown without supplier-level amounts"
        );
        drop(state);
        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[test]
    fn deep_analysis_lookup_releases_reader_before_compute() {
        let (state, tender_path, kbo_path) = handler_test_state("deep-reader-release");
        let repository = KboRepository::open(&kbo_path).unwrap();
        let company = repository.lookup("0404.484.060").unwrap().unwrap();
        let tender = TenderStore::open_read_only(&tender_path)
            .unwrap()
            .tender("test", "t1")
            .unwrap()
            .unwrap();
        let tenders = vec![tender];
        let preferences = CompanyPreferences::default();
        let mut results = rank(&company, &tenders, &[], &preferences, &HashMap::new(), 1);
        deep_rerank_with_analyzer(
            &state,
            &DeepRerankInputs {
                company: &company,
                history: &[],
                preferences: &preferences,
                tenders: &tenders,
            },
            &mut results,
            1,
            "test-reader-release",
            |key| {
                let store =
                    TenderStore::open_read_only(&tender_path).map_err(ApiError::internal)?;
                store.cached_analysis(key).map_err(ApiError::internal)
            },
            |_| {
                // A lingering read-only connection would make this write-mode
                // open fail in this process, without making any provider call.
                let writer =
                    TenderStore::open_initialized(&tender_path).map_err(|e| e.to_string())?;
                writer
                    .save_preferences(&company.enterprise_number, &preferences)
                    .map_err(|e| e.to_string())?;
                Ok(admission_test_decision())
            },
        )
        .unwrap();
        std::fs::remove_file(tender_path).unwrap();
        drop(repository);
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn analysis_routes_share_admission_and_cached_hits_bypass_capacity() {
        let (state, tender_path, kbo_path) = handler_test_state("admission-routes");
        let key = {
            let repository = KboRepository::open(&kbo_path).unwrap();
            let company = repository.lookup("0404.484.060").unwrap().unwrap();
            let store = TenderStore::open_read_only(&tender_path).unwrap();
            let tender = store.tender("test", "t1").unwrap().unwrap();
            let preferences = store.preferences_for(&company.enterprise_number).unwrap();
            let history = store.awards_for(&company.enterprise_number).unwrap();
            analysis_cache_key(
                DETERMINISTIC_CACHE_IDENTITY,
                &company,
                &tender,
                &history,
                &preferences,
            )
        };
        let call = || {
            tender_analysis(
                State(state.clone()),
                Path(("0404.484.060".into(), "test".into(), "t1".into())),
            )
        };
        let recommend = |deep| {
            recommendations(
                State(state.clone()),
                Path("0404.484.060".into()),
                Query(RecommendationQuery {
                    limit: Some(1),
                    deep: Some(deep),
                    ..Default::default()
                }),
            )
        };
        let active = state.analysis_admission.try_enter(&key).unwrap();
        assert!(call().await.unwrap_err().1.contains("already in progress"));
        assert!(recommend(1)
            .await
            .unwrap_err()
            .1
            .contains("already in progress"));
        assert!(recommend(0).await.is_ok());
        assert_eq!(state.cache_metrics.misses.load(Ordering::Relaxed), 0);
        drop(active);
        assert!(!call().await.unwrap().0.cached);
        let permits: Vec<_> = (0..MAX_CONCURRENT_ANALYSES)
            .map(|index| {
                state
                    .analysis_admission
                    .try_enter(&format!("occupied-{index}"))
                    .unwrap()
            })
            .collect();
        assert!(call().await.unwrap().0.cached);
        assert!(recommend(1).await.is_ok());
        assert_eq!(state.cache_metrics.misses.load(Ordering::Relaxed), 1);
        drop(permits);
        let restarted = fresh_state(&tender_path, &kbo_path);
        let held: Vec<_> = (0..MAX_CONCURRENT_ANALYSES)
            .map(|index| {
                restarted
                    .analysis_admission
                    .try_enter(&format!("occupied-{index}"))
                    .unwrap()
            })
            .collect();
        let persisted = tender_analysis(
            State(restarted.clone()),
            Path(("0404.484.060".into(), "test".into(), "t1".into())),
        )
        .await
        .unwrap();
        assert!(persisted.0.cached);
        assert_eq!(
            restarted
                .cache_metrics
                .persistent_hits
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(restarted.cache_metrics.misses.load(Ordering::Relaxed), 0);
        drop(held);
        std::fs::remove_file(tender_path).unwrap();
        std::fs::remove_file(kbo_path).unwrap();
    }

    #[tokio::test]
    async fn analysis_cache_survives_a_restart() {
        let (state, tender_path, kbo_path) = handler_test_state("analysis-restart");
        let call = |state: &Arc<ApiState>| {
            tender_analysis(
                State(Arc::clone(state)),
                Path((
                    "0404.484.060".to_owned(),
                    "test".to_owned(),
                    "t1".to_owned(),
                )),
            )
        };
        let first = call(&state).await.expect("first analysis should succeed");
        assert!(!first.0.cached);
        // A restart drops the in-memory analysis caches, but the persistent
        // row written by the first call stays: the next answer must still be
        // a hit, and of the persistent kind the smoke test pins.
        let restarted = fresh_state(&tender_path, &kbo_path);
        let second = call(&restarted)
            .await
            .expect("restarted analysis should succeed");
        assert!(second.0.cached);
        assert_eq!(first.0.provider, second.0.provider);
        assert_eq!(first.0.relevance, second.0.relevance);
        assert_eq!(
            restarted
                .cache_metrics
                .persistent_hits
                .load(Ordering::Relaxed),
            1
        );
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[tokio::test]
    async fn health_reports_last_refresh_provenance() {
        use aanbestedingen_matcher::recommendations::CorpusEvidence;
        let (state, tender_path, kbo_path) = handler_test_state("health-provenance");
        let run_id = TenderStore::open(&tender_path)
            .expect("store should open")
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
        TenderStore::open(&tender_path)
            .expect("store should open")
            .record_refresh_evidence(
                run_id,
                &CorpusEvidence {
                    opportunities: 1877,
                    with_deadline: 1200,
                    with_value: 500,
                    with_requirements: 400,
                    stated: 300,
                    hinted: 20,
                    unknown: 1557,
                },
            )
            .expect("evidence should record");
        let body = health(State(Arc::clone(&state)))
            .await
            .expect("health should answer")
            .0;
        assert!(body["recognition_register"].is_null());
        assert_eq!(body["last_refresh"]["scope"], json!("ACTIVE"));
        assert_eq!(body["last_refresh"]["window"], json!("days=30"));
        assert_eq!(body["last_refresh"]["fetched"], json!(1877));
        assert_eq!(body["last_refresh"]["total_notice_count"], json!(19093));
        assert_eq!(body["last_refresh"]["evidence"]["stated"], json!(300));
        assert_eq!(body["last_refresh"]["evidence"]["unknown"], json!(1557));
        assert_eq!(
            body["last_refresh"]["evidence"]["opportunities"],
            json!(1877)
        );
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[tokio::test]
    async fn save_feedback_rejects_an_unknown_company() {
        let (state, tender_path, kbo_path) = handler_test_state("feedback-company");
        let error = save_feedback(
            State(Arc::clone(&state)),
            Path("0999.999.999".to_owned()),
            ApiJson(FeedbackRequest {
                source: "test".to_owned(),
                external_id: "t1".to_owned(),
                relevant: true,
                pursuable: FieldUpdate::Keep,
                outcome: FieldUpdate::Keep,
                reason: FieldUpdate::Keep,
            }),
        )
        .await
        .expect_err("feedback for an unknown company must fail");
        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(error.1, "company not found");
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[tokio::test]
    async fn save_feedback_rejects_an_unknown_tender() {
        let (state, tender_path, kbo_path) = handler_test_state("feedback-tender");
        let error = save_feedback(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            ApiJson(FeedbackRequest {
                source: "test".to_owned(),
                external_id: "missing".to_owned(),
                relevant: true,
                pursuable: FieldUpdate::Keep,
                outcome: FieldUpdate::Keep,
                reason: FieldUpdate::Keep,
            }),
        )
        .await
        .expect_err("feedback for an unknown tender must fail");
        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(error.1, "tender not found");
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[tokio::test]
    async fn save_feedback_records_pursuable_and_outcome() {
        // The dashboard judging UI posts exactly this shape: topic first,
        // then pursuable tri-state plus an outcome. It must round-trip into
        // the labels eval-ranking grades and counts.
        let (state, tender_path, kbo_path) = handler_test_state("feedback-split");
        let saved = save_feedback(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            ApiJson(FeedbackRequest {
                source: "test".to_owned(),
                external_id: "t1".to_owned(),
                relevant: true,
                pursuable: FieldUpdate::Set(true),
                outcome: FieldUpdate::Set("bid".to_owned()),
                reason: FieldUpdate::Keep,
            }),
        )
        .await
        .expect("pursuable feedback should save");
        assert_eq!(saved.0["status"], json!("saved"));
        let enterprise =
            normalize_enterprise_number("0404.484.060").expect("enterprise should normalize");
        let labels = TenderStore::open(&tender_path)
            .expect("store should open")
            .feedback_details_for(&enterprise)
            .expect("feedback should be readable");
        let label = labels
            .get(&("test".to_owned(), "t1".to_owned()))
            .expect("label should be recorded");
        assert!(label.relevant);
        assert_eq!(label.pursuable, Some(true));
        assert_eq!(label.outcome.as_deref(), Some("bid"));
        let error = save_feedback(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            ApiJson(FeedbackRequest {
                source: "test".to_owned(),
                external_id: "t1".to_owned(),
                relevant: true,
                pursuable: FieldUpdate::Keep,
                outcome: FieldUpdate::Set("maybe".to_owned()),
                reason: FieldUpdate::Keep,
            }),
        )
        .await
        .expect_err("an unknown outcome must be rejected");
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[tokio::test]
    async fn feedback_partial_save_round_trips_and_explicit_null_clears() {
        let (state, tender_path, kbo_path) = handler_test_state("feedback-round-trip");
        for payload in [
            json!({"source":"test","external_id":"t1","relevant":true,"pursuable":true,"outcome":"won","reason":"retain"}),
            json!({"source":"test","external_id":"t1","relevant":false}),
        ] {
            let saved = save_feedback(
                State(Arc::clone(&state)),
                Path("0404.484.060".into()),
                ApiJson(serde_json::from_value(payload).unwrap()),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(saved["feedback"]["pursuable"], true);
            assert_eq!(saved["feedback"]["outcome"], "won");
            assert_eq!(saved["feedback"]["reason"], "retain");
        }
        let response = recommendations(
            State(Arc::clone(&state)),
            Path("0404.484.060".into()),
            Query(RecommendationQuery::default()),
        )
        .await
        .unwrap()
        .0;
        let item = response
            .iter()
            .find(|i| i.recommendation.external_id == "t1")
            .unwrap();
        assert_eq!(
            item.saved_feedback.as_ref().unwrap().outcome.as_deref(),
            Some("won")
        );
        let cleared = save_feedback(State(state), Path("0404.484.060".into()), ApiJson(serde_json::from_value(
            json!({"source":"test","external_id":"t1","relevant":true,"pursuable":null,"outcome":null})
        ).unwrap())).await.unwrap().0;
        assert!(cleared["feedback"]["pursuable"].is_null());
        assert!(cleared["feedback"]["outcome"].is_null());
        assert_eq!(cleared["feedback"]["reason"], "retain");
        let _ = std::fs::remove_file(tender_path);
        let _ = std::fs::remove_file(kbo_path);
    }

    #[tokio::test]
    async fn save_feedback_records_relevant_marks() {
        let (state, tender_path, kbo_path) = handler_test_state("feedback-records");
        let saved = save_feedback(
            State(Arc::clone(&state)),
            Path("0404.484.060".to_owned()),
            ApiJson(FeedbackRequest {
                source: "test".to_owned(),
                external_id: "t1".to_owned(),
                relevant: true,
                pursuable: FieldUpdate::Keep,
                outcome: FieldUpdate::Keep,
                reason: FieldUpdate::Set("fits the team".to_owned()),
            }),
        )
        .await
        .expect("feedback for a known tender should save");
        assert_eq!(saved.0["status"], json!("saved"));
        let enterprise =
            normalize_enterprise_number("0404.484.060").expect("enterprise should normalize");
        let recorded = TenderStore::open(&tender_path)
            .expect("store should open")
            .feedback_for(&enterprise)
            .expect("feedback should be readable");
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded.get(&("test".to_owned(), "t1".to_owned())),
            Some(&true)
        );
        let trail = TenderStore::open(&tender_path)
            .expect("store should open")
            .audit_log(10)
            .expect("audit should be readable");
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].action, "feedback.save");
        assert_eq!(trail[0].enterprise_number.as_deref(), Some(&enterprise[..]));
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[test]
    fn a_negative_filing_age_limit_is_rejected_not_treated_as_withhold_all() {
        // A negative limit satisfies no filing, so accepting it would quietly
        // turn every capacity assessment into unknown.
        let query = |days: Option<i64>| RecommendationQuery {
            max_filing_age_days: days,
            ..Default::default()
        };
        assert!(validate_filing_age_limit(&query(None)).is_ok());
        assert!(validate_filing_age_limit(&query(Some(0))).is_ok());
        assert!(validate_filing_age_limit(&query(Some(730))).is_ok());
        let error = validate_filing_age_limit(&query(Some(-1))).expect_err("negative rejected");
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.1.contains("must not be negative"));
    }

    #[test]
    fn recommendation_deep_limit_is_validated() {
        let query = |deep: Option<i64>| RecommendationQuery {
            deep,
            ..Default::default()
        };
        assert_eq!(
            recommendation_deep_limit(&query(None)).expect("deep default should be valid"),
            0
        );
        assert_eq!(
            recommendation_deep_limit(&query(Some(10))).expect("maximum deep should be valid"),
            10
        );
        for deep in [-1, 11] {
            assert!(recommendation_deep_limit(&query(Some(deep))).is_err());
        }
    }

    #[tokio::test]
    async fn deep_recommendations_fuse_analysis_decisions() {
        let tag = format!("aanbestedingen-deep-{}", std::process::id());
        let tender_path = std::env::temp_dir().join(format!("{tag}-tenders.duckdb"));
        let kbo_path = std::env::temp_dir().join(format!("{tag}-kbo.duckdb"));
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
        {
            let store = TenderStore::open(&tender_path).expect("tender database should open");
            store
                .upsert(&Tender {
                    source: "test".to_owned(),
                    external_id: "t1".to_owned(),
                    title: "Example works".to_owned(),
                    ..Tender::default()
                })
                .expect("tender should upsert");
        }
        {
            let connection = duckdb::Connection::open(&kbo_path).expect("kbo database should open");
            connection
                .execute_batch(
                    "CREATE TABLE kbo_enterprise (EnterpriseNumber VARCHAR, Status VARCHAR, JuridicalSituation VARCHAR, JuridicalForm VARCHAR, StartDate DATE);
                     CREATE TABLE kbo_denomination (EntityNumber VARCHAR, Denomination VARCHAR);
                     CREATE TABLE kbo_activity (EntityNumber VARCHAR, NaceCode VARCHAR, Classification VARCHAR);
                     CREATE TABLE kbo_code (Category VARCHAR, Code VARCHAR, Language VARCHAR, Description VARCHAR);
                     CREATE TABLE kbo_address (EntityNumber VARCHAR, TypeOfAddress VARCHAR, Zipcode VARCHAR, MunicipalityNL VARCHAR, MunicipalityFR VARCHAR, StreetNL VARCHAR, StreetFR VARCHAR, HouseNumber VARCHAR, Box VARCHAR, DateStrikingOff DATE);
                     CREATE TABLE kbo_establishment (EnterpriseNumber VARCHAR, EstablishmentNumber VARCHAR);",
                )
                .expect("kbo schema should create");
            connection
                .execute(
                    "INSERT INTO kbo_enterprise VALUES ('0404.484.060', 'AC', NULL, NULL, '2020-01-01')",
                    [],
                )
                .expect("kbo enterprise should insert");
            connection
                .execute(
                    "INSERT INTO kbo_denomination VALUES ('0404.484.060', 'Example')",
                    [],
                )
                .expect("kbo denomination should insert");
        }
        let state = Arc::new(ApiState {
            kbo_database: kbo_path.to_string_lossy().into_owned(),
            tender_database: tender_path.to_string_lossy().into_owned(),
            api_token: None,
            recognition_snapshot: None,
            analysis: None,
            analysis_admission: AnalysisAdmission::default(),
            analysis_cache: Arc::new(Mutex::new(AnalysisCache::default())),
            cache_metrics: Arc::new(CacheMetrics {
                memory_hits: AtomicU64::new(0),
                persistent_hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                evictions: AtomicU64::new(0),
            }),
        });
        let recommend = |deep: Option<i64>| {
            recommendations(
                State(Arc::clone(&state)),
                Path("0404.484.060".to_owned()),
                Query(RecommendationQuery {
                    limit: Some(5),
                    deep,
                    ..Default::default()
                }),
            )
        };
        let plain = recommend(None)
            .await
            .expect("plain recommendations should succeed");
        let item = &plain
            .0
            .first()
            .expect("at least one recommendation should exist")
            .recommendation;
        assert!(item.jev_relevance.is_none());
        assert!(item
            .reasons
            .iter()
            .all(|reason| !reason.starts_with("Jev relevance: ")));
        let plain_score = item.score;

        let deep = recommend(Some(1))
            .await
            .expect("deep recommendations should succeed");
        let deep_item = &deep
            .0
            .first()
            .expect("at least one deep recommendation should exist")
            .recommendation;
        let relevance = deep_item
            .jev_relevance
            .as_ref()
            .and_then(Option::as_deref)
            .expect("deterministic deep rerank should set jev_relevance");
        let expected_score = match relevance {
            "strong" => (plain_score + 15.0).min(100.0),
            "poor" => (plain_score - 15.0).max(0.0),
            _ => plain_score,
        };
        assert!((deep_item.score - expected_score).abs() < 1e-9);
        assert!(deep_item.jev_qualification_risk.is_some());
        assert!(deep_item
            .reasons
            .iter()
            .any(|reason| reason == &format!("Jev relevance: {relevance}")));

        // The second deep run must reuse the cached decision instead of
        // re-analyzing.
        let repeat = recommend(Some(1))
            .await
            .expect("repeat deep recommendations should succeed");
        assert!(repeat
            .0
            .first()
            .expect("repeat call should still return the tender")
            .recommendation
            .jev_relevance
            .is_some());
        assert_eq!(state.cache_metrics.misses.load(Ordering::Relaxed), 1);
        assert_eq!(state.cache_metrics.memory_hits.load(Ordering::Relaxed), 1);

        // A cached first decision must not mask a failure on the next
        // tender or let the endpoint return a partially reranked response.
        {
            let repository = KboRepository::open(&state.kbo_database).unwrap();
            let company = repository.lookup("0404.484.060").unwrap().unwrap();
            let store = TenderStore::open(&tender_path).unwrap();
            let first = store.tender("test", "t1").unwrap().unwrap();
            let second = Tender {
                external_id: "t2".to_owned(),
                ..first.clone()
            };
            let tenders = vec![first, second];
            let preferences = CompanyPreferences::default();
            let mut results = vec![item.clone(), item.clone()];
            results[1].external_id = "t2".to_owned();
            let calls = std::cell::Cell::new(0);
            let error = deep_rerank_with_analyzer(
                &state,
                &DeepRerankInputs {
                    company: &company,
                    history: &[],
                    preferences: &preferences,
                    tenders: &tenders,
                },
                &mut results,
                2,
                DETERMINISTIC_CACHE_IDENTITY,
                |key| store.cached_analysis(key).map_err(ApiError::internal),
                |_| {
                    calls.set(calls.get() + 1);
                    Err("analyzer unavailable".to_owned())
                },
            )
            .expect_err("a mid-deep failure must abort the response");
            assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(calls.get(), 1);
            assert!(results[0].jev_relevance.is_some());
            assert!(results[1].jev_relevance.is_none());
            assert_eq!(state.cache_metrics.memory_hits.load(Ordering::Relaxed), 2);
            assert_eq!(state.cache_metrics.misses.load(Ordering::Relaxed), 2);
        }
        let _ = std::fs::remove_file(&tender_path);
        let _ = std::fs::remove_file(&kbo_path);
    }

    #[test]
    fn normalizes_enterprise_numbers_and_rejects_invalid_values() {
        assert_eq!(
            normalized_enterprise_number("BE0200.065.765")
                .expect("Belgian enterprise number should normalize"),
            "0200.065.765"
        );
        let error = normalized_enterprise_number("not-an-enterprise")
            .expect_err("invalid enterprise number should be rejected");
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn rejects_empty_feedback_identifiers() {
        let request = FeedbackRequest {
            source: " ".to_owned(),
            external_id: "tender-1".to_owned(),
            relevant: true,
            pursuable: FieldUpdate::Keep,
            outcome: FieldUpdate::Keep,
            reason: FieldUpdate::Keep,
        };
        let error = feedback_target(&request).expect_err("empty source should be rejected");
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);

        let request = FeedbackRequest {
            source: "ted".to_owned(),
            external_id: "".to_owned(),
            relevant: true,
            pursuable: FieldUpdate::Keep,
            outcome: FieldUpdate::Keep,
            reason: FieldUpdate::Keep,
        };
        let error =
            feedback_target(&request).expect_err("empty external identifier should be rejected");
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn recommendation_candidate_limit_is_bounded() {
        let default_query = RecommendationQuery::default();
        assert_eq!(
            recommendation_candidate_limit(&default_query)
                .expect("candidate default should be valid"),
            10_000
        );

        let maximum_query = RecommendationQuery {
            candidates: Some(MAX_RECOMMENDATION_CANDIDATES),
            ..default_query
        };
        assert_eq!(
            recommendation_candidate_limit(&maximum_query)
                .expect("maximum candidate value should be accepted"),
            MAX_RECOMMENDATION_CANDIDATES
        );

        for candidates in [0, MAX_RECOMMENDATION_CANDIDATES + 1] {
            let invalid_query = RecommendationQuery {
                candidates: Some(candidates),
                ..Default::default()
            };
            assert!(recommendation_candidate_limit(&invalid_query).is_err());
        }
    }

    #[test]
    fn trims_valid_feedback_identifiers() {
        let request = FeedbackRequest {
            source: " ted ".to_owned(),
            external_id: " tender-1 ".to_owned(),
            relevant: true,
            pursuable: FieldUpdate::Keep,
            outcome: FieldUpdate::Keep,
            reason: FieldUpdate::Keep,
        };
        assert_eq!(
            feedback_target(&request).expect("valid feedback target should trim"),
            ("ted".to_owned(), "tender-1".to_owned())
        );
    }

    #[test]
    fn recommendation_defaults_are_valid() {
        let query = RecommendationQuery::default();
        assert_eq!(
            recommendation_limit(&query).expect("defaults should be valid"),
            20
        );
    }

    #[test]
    fn recommendation_limit_accepts_maximum() {
        let query = RecommendationQuery {
            limit: Some(100),
            sort: Some("score".to_owned()),
            due_within: Some(365),
            ..Default::default()
        };
        assert_eq!(
            recommendation_limit(&query).expect("maximum values should be valid"),
            100
        );
    }

    #[test]
    fn recommendation_rejects_invalid_values() {
        for query in [
            RecommendationQuery {
                limit: Some(0),
                ..Default::default()
            },
            RecommendationQuery {
                limit: Some(101),
                ..Default::default()
            },
            RecommendationQuery {
                limit: Some(20),
                sort: Some("unknown".to_owned()),
                due_within: Some(366),
                ..Default::default()
            },
        ] {
            let error =
                recommendation_limit(&query).expect_err("invalid values should be rejected");
            assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        }
    }
}
