use crate::{
    match_company_to_tender, match_signals, parse_content_length, qualification_risk, AwardRecord,
    CompanyPreferences, CompanyProfile, Tender,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::io::Read;
use ureq::AgentBuilder;
use url::{Host, Url};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScoreAnswer {
    pub score: f64,
    pub confidence: f64,
}

/// Structured analysis result. Providers may omit Jev answers; those fields
/// remain `None` and serialize as JSON `null` rather than failing the request.
///
/// The three numeric match fields do not share a scale across providers. The
/// OpenAI-compatible path asks for and validates `[0, 1]`. The TypeSafe path
/// returns the expected level of a three-point ordinal ("no / partial / strong
/// match"), validated as `[0, 2]`, so `1.62` is a legitimate TypeSafe answer and
/// an invalid OpenAI one. Ranking reads `relevance`, and the dashboard reads
/// `raw`, so nothing consumes these numbers as a probability today; a new
/// consumer must not start.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnalysisDecision {
    pub provider: String,
    pub activity_match: Option<f64>,
    pub geography_match: Option<f64>,
    pub previous_work_match: Option<f64>,
    pub qualification_risk: Option<String>,
    pub relevance: Option<String>,
    pub raw: BTreeMap<String, serde_json::Value>,
    /// Which prompt produced this decision. `None` on rows cached before
    /// prompt versions existed; those rows can no longer match a key, because
    /// every current cache identity embeds its prompt version.
    pub prompt_version: Option<String>,
    /// Whether this response came from a cache. Fresh analyzer output sets
    /// this to false; the API flips it on memory or persistent cache hits.
    /// Defaulted so rows cached before this field existed still deserialize.
    #[serde(default)]
    pub cached: bool,
}

/// Serialized decision budget, shared by provider parsing and both caches.
/// Entry count limits alone do not bound retained provider payloads.
pub const MAX_ANALYSIS_DECISION_BYTES: usize = 64 * 1024;

impl AnalysisDecision {
    pub fn validate_size(&self) -> Result<(), AnalysisError> {
        let encoded = serde_json::to_vec(self)
            .map_err(|_| AnalysisError("analysis decision could not be encoded".into()))?;
        if encoded.len() > MAX_ANALYSIS_DECISION_BYTES {
            return Err(AnalysisError(format!(
                "analysis decision exceeded the {MAX_ANALYSIS_DECISION_BYTES} byte limit"
            )));
        }
        Ok(())
    }
}

/// Cache identity for decisions from the deterministic analyzer. Bump the
/// version whenever deterministic scoring behavior changes (for example the
/// CPV-NACE table expansion changed the scores feeding the relevance
/// thresholds), so stale cached decisions stop matching the key and age out
/// instead of being served.
pub const DETERMINISTIC_CACHE_IDENTITY: &str = "deterministic-rust:v4";

/// Prompt versions for the two remote providers. They are part of each
/// analyzer's cache identity, so editing the questions or the system schema
/// without bumping the version would keep serving stale decisions — the same
/// staleness the deterministic identity bump guards against. Bump the version
/// whenever the corresponding prompt text changes.
pub const TYPESAFE_QUESTIONS_VERSION: &str = "typesafe-questions:v1";
pub const OPENAI_PROMPT_VERSION: &str = "openai-schema:v2";

const QUALIFICATION_RISK_OPTIONS: [&str; 4] =
    ["none_obvious", "certification", "capacity", "unclear"];
const RELEVANCE_OPTIONS: [&str; 3] = ["poor", "possible", "strong"];

fn openai_system_prompt() -> String {
    format!(
        "Return only one JSON object with activity_match, geography_match, and previous_work_match as finite numbers in [0, 1]; qualification_risk and relevance strings; and raw as an object. All numeric values, including raw score and confidence, must be finite numbers in [0, 1]. If raw has no normalized per-criterion values, use an empty object. Otherwise map each criterion to an object with score and confidence. qualification_risk must be exactly one of [{}]: none_obvious means no obvious risk in supplied information; certification means a certification or professional registration may be required; capacity means turnover, references, or delivery capacity may be a concern; unclear means insufficient information. relevance must be exactly one of [{}]: poor means probably not relevant; possible means possibly relevant and manual review is needed; strong means a strong candidate for manual review. Judge thematic relevance separately from capacity and qualification risk; a risk alone does not make a tender thematically irrelevant. Use only supplied facts, do not infer unknown qualifications or legal eligibility, and treat missing information as unknown.",
        QUALIFICATION_RISK_OPTIONS.join(", "),
        RELEVANCE_OPTIONS.join(", "),
    )
}

pub trait Analyzer {
    fn analyze(
        &self,
        company: &CompanyProfile,
        tender: &Tender,
        history: &[AwardRecord],
        preferences: &CompanyPreferences,
    ) -> Result<AnalysisDecision, AnalysisError>;
}

pub struct DeterministicAnalyzer;

const MAX_AI_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_AI_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct AnalysisError(String);

impl fmt::Display for AnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for AnalysisError {}

#[derive(Clone)]
pub struct RemoteAnalyzer {
    endpoint: String,
    api_key: String,
    model: String,
    timeout_seconds: u64,
    response_format: ResponseFormat,
}

/// Response format requested from an OpenAI-compatible provider.
///
/// `json_object` constrains syntax only: the model must emit JSON, not the
/// fields `parse_remote_decision` accepts. `json_schema` constrains the shape.
/// Measured on a local 1.5B model over 30 synthetic cases, same prompt and
/// model: 2 of 30 decisions parsed under `json_object`, 30 of 30 under the
/// schema in [`decision_schema`]. A provider that rejects the schema is retried
/// once with `json_object`, so the default costs one extra request only where
/// the feature is unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFormat {
    JsonObject,
    JsonSchema,
}

impl ResponseFormat {
    fn from_config(value: Option<&str>) -> Result<Self, AnalysisError> {
        match value.map(str::trim) {
            None | Some("") | Some("json_schema") => Ok(Self::JsonSchema),
            Some("json_object") => Ok(Self::JsonObject),
            Some(other) => Err(AnalysisError(format!(
                "AANBESTEDINGEN_AI_RESPONSE_FORMAT must be json_schema or json_object, not {other}"
            ))),
        }
    }
}

fn redacted_endpoint(endpoint: &str) -> String {
    let Ok(mut parsed) = Url::parse(endpoint) else {
        return "[invalid endpoint]".to_owned();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

impl fmt::Debug for RemoteAnalyzer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteAnalyzer")
            .field("endpoint", &redacted_endpoint(&self.endpoint))
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("response_format", &self.response_format)
            .finish()
    }
}

fn non_empty_trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn ai_timeout_seconds(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.parse().ok())
        .filter(|seconds| (1..=300).contains(seconds))
        .unwrap_or(60)
}

fn ai_model(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("gpt-4o-mini")
        .to_owned()
}

/// The JSON schema `parse_remote_decision` accepts, built from the same
/// constants the parser checks, so the two cannot drift apart.
pub fn decision_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "activity_match": {"type": "number", "minimum": 0, "maximum": 1},
            "geography_match": {"type": "number", "minimum": 0, "maximum": 1},
            "previous_work_match": {"type": "number", "minimum": 0, "maximum": 1},
            "qualification_risk": {"type": "string", "enum": QUALIFICATION_RISK_OPTIONS},
            "relevance": {"type": "string", "enum": RELEVANCE_OPTIONS},
            "raw": {
                "type": "object",
                "additionalProperties": {
                    "type": "object",
                    "properties": {
                        "score": {"type": "number", "minimum": 0, "maximum": 1},
                        "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                    },
                    "required": ["score", "confidence"],
                    "additionalProperties": false,
                },
            },
        },
        "required": [
            "activity_match",
            "geography_match",
            "previous_work_match",
            "qualification_risk",
            "relevance",
        ],
        "additionalProperties": false,
    })
}

fn remote_payload(model: &str, input: &str, format: ResponseFormat) -> serde_json::Value {
    let response_format = match format {
        ResponseFormat::JsonObject => serde_json::json!({"type": "json_object"}),
        ResponseFormat::JsonSchema => serde_json::json!({
            "type": "json_schema",
            "json_schema": {"name": "analysis_decision", "schema": decision_schema()},
        }),
    };
    serde_json::json!({
        "model": model,
        "temperature": 0,
        "response_format": response_format,
        "messages": [
            {"role": "system", "content": openai_system_prompt()},
            {"role": "user", "content": input},
        ],
    })
}

/// A failed chat request. The status is kept only to decide whether a schema
/// rejection is worth retrying; `ureq::Error` is too large to return by value.
struct ChatFailure {
    status: Option<u16>,
    error: AnalysisError,
}

fn chat_failure(error: ureq::Error) -> ChatFailure {
    match error {
        ureq::Error::Status(status, _) => ChatFailure {
            status: Some(status),
            error: AnalysisError(format!("AI request failed with HTTP {status}")),
        },
        // Transport error text can include the full credential-bearing
        // endpoint URL. Never send it to callers or logs.
        ureq::Error::Transport(_) => ChatFailure {
            status: None,
            error: AnalysisError("AI request failed (transport error)".into()),
        },
    }
}

fn send_chat(
    agent: &ureq::Agent,
    endpoint: &str,
    api_key: &str,
    payload: &[u8],
) -> Result<ureq::Response, ChatFailure> {
    agent
        .post(endpoint)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .send_bytes(payload)
        .map_err(chat_failure)
}

impl RemoteAnalyzer {
    pub fn from_env() -> Result<Option<Self>, AnalysisError> {
        Self::from_config(
            env::var("AANBESTEDINGEN_AI_ENDPOINT").ok(),
            env::var("AANBESTEDINGEN_AI_API_KEY").ok(),
            env::var("AANBESTEDINGEN_AI_MODEL").ok(),
            env::var("AANBESTEDINGEN_AI_TIMEOUT_SECONDS").ok(),
            env::var("AANBESTEDINGEN_AI_RESPONSE_FORMAT").ok(),
        )
    }

    fn from_config(
        endpoint: Option<String>,
        api_key: Option<String>,
        model: Option<String>,
        timeout: Option<String>,
        response_format: Option<String>,
    ) -> Result<Option<Self>, AnalysisError> {
        let endpoint = non_empty_trimmed(endpoint);
        let api_key = non_empty_trimmed(api_key);
        match (endpoint, api_key) {
            (None, None) => Ok(None),
            (Some(endpoint), Some(api_key)) => {
                if !endpoint_allowed(&endpoint) {
                    return Err(AnalysisError(
                        "AANBESTEDINGEN_AI_ENDPOINT is not an allowed secure endpoint".to_owned(),
                    ));
                }
                let timeout_seconds = ai_timeout_seconds(timeout.as_deref());
                Ok(Some(Self {
                    endpoint,
                    api_key,
                    model: ai_model(model.as_deref()),
                    timeout_seconds,
                    response_format: ResponseFormat::from_config(response_format.as_deref())?,
                }))
            }
            _ => Err(AnalysisError(
                "AANBESTEDINGEN_AI_ENDPOINT and AANBESTEDINGEN_AI_API_KEY must be set together"
                    .to_owned(),
            )),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    pub fn cache_identity(&self) -> String {
        format!(
            "openai-compatible:{}:{}:{}",
            self.endpoint, self.model, OPENAI_PROMPT_VERSION
        )
    }

    pub fn prompt_version(&self) -> &'static str {
        OPENAI_PROMPT_VERSION
    }

    pub fn analyze(
        &self,
        company: &CompanyProfile,
        tender: &Tender,
        history: &[AwardRecord],
        preferences: &CompanyPreferences,
    ) -> Result<AnalysisDecision, AnalysisError> {
        let mut company_json = serde_json::to_value(company)
            .map_err(|error| AnalysisError(format!("company could not be encoded: {error}")))?;
        if company.primary_cpv_prefixes.is_empty() {
            let derived = crate::primary_cpv_prefixes_for_company(company);
            if !derived.is_empty() {
                if let Some(map) = company_json.as_object_mut() {
                    map.insert(
                        "primary_cpv_prefixes".to_owned(),
                        serde_json::to_value(&derived).map_err(|error| {
                            AnalysisError(format!(
                                "derived cpv prefixes could not be encoded: {error}"
                            ))
                        })?,
                    );
                }
            }
        }
        let input = serde_json::json!({
            "company": company_json,
            "tender": tender,
            "history": history,
            "preferences": preferences,
        });
        let input = input.to_string();
        let payload =
            encode_remote_payload(&remote_payload(&self.model, &input, self.response_format))?;
        let agent = AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(self.timeout_seconds))
            .build();
        let response = match send_chat(&agent, &self.endpoint, &self.api_key, &payload) {
            Ok(response) => response,
            // A provider that does not know json_schema rejects the request
            // before generating anything, so one retry with json_object is
            // cheaper than failing the analysis outright.
            Err(failure)
                if self.response_format == ResponseFormat::JsonSchema
                    && matches!(failure.status, Some(400 | 422)) =>
            {
                let fallback = encode_remote_payload(&remote_payload(
                    &self.model,
                    &input,
                    ResponseFormat::JsonObject,
                ))?;
                send_chat(&agent, &self.endpoint, &self.api_key, &fallback)
                    .map_err(|failure| failure.error)?
            }
            Err(failure) => return Err(failure.error),
        };
        let response = parse_chat_response(response)?;
        let content = response
            .choices
            .first()
            .map(|choice| choice.message.content.as_str())
            .ok_or_else(|| AnalysisError("AI response contained no choices".to_owned()))?;
        parse_remote_decision(content, &self.model)
    }
}

fn is_loopback_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain("localhost")) => true,
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    }
}

fn endpoint_allowed(endpoint: &str) -> bool {
    // The url crate skips extra slashes in special schemes ("https:///x"
    // becomes host "x"), so reject the written empty-authority form first.
    let Some((_, authority)) = endpoint.split_once("://") else {
        return false;
    };
    if authority.starts_with('/') {
        return false;
    }
    let Ok(parsed) = Url::parse(endpoint) else {
        return false;
    };
    if !matches!(parsed.host_str(), Some(host) if !host.is_empty()) {
        return false;
    }
    match parsed.scheme() {
        "https" => true,
        "http" => is_loopback_host(parsed.host()),
        _ => false,
    }
}

fn encode_remote_payload(payload: &serde_json::Value) -> Result<Vec<u8>, AnalysisError> {
    let body = serde_json::to_vec(payload)
        .map_err(|error| AnalysisError(format!("AI request could not be encoded: {error}")))?;
    if body.len() > MAX_AI_REQUEST_BYTES {
        return Err(AnalysisError(format!(
            "AI request exceeded the {MAX_AI_REQUEST_BYTES} byte limit"
        )));
    }
    Ok(body)
}

fn read_bounded_response(response: ureq::Response) -> Result<Vec<u8>, AnalysisError> {
    if parse_content_length(response.header("Content-Length"))
        .is_some_and(|length| length > MAX_AI_RESPONSE_BYTES)
    {
        return Err(AnalysisError(format!(
            "AI response exceeded the {MAX_AI_RESPONSE_BYTES} byte limit"
        )));
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take((MAX_AI_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| AnalysisError(format!("AI response could not be read: {error}")))?;
    if body.len() > MAX_AI_RESPONSE_BYTES {
        return Err(AnalysisError(format!(
            "AI response exceeded the {MAX_AI_RESPONSE_BYTES} byte limit"
        )));
    }
    Ok(body)
}

fn parse_chat_response(response: ureq::Response) -> Result<ChatResponse, AnalysisError> {
    let body = read_bounded_response(response)?;
    parse_chat_response_body(&body)
}

fn parse_chat_response_body(body: &[u8]) -> Result<ChatResponse, AnalysisError> {
    if body.len() > MAX_AI_RESPONSE_BYTES {
        return Err(AnalysisError(format!(
            "AI response exceeded the {MAX_AI_RESPONSE_BYTES} byte limit"
        )));
    }
    serde_json::from_slice(body)
        .map_err(|error| AnalysisError(format!("AI response was not valid JSON: {error}")))
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct RemoteDecision {
    activity_match: f64,
    geography_match: f64,
    previous_work_match: f64,
    qualification_risk: String,
    relevance: String,
    #[serde(default)]
    raw: BTreeMap<String, ScoreAnswer>,
}

fn parse_remote_decision(content: &str, model: &str) -> Result<AnalysisDecision, AnalysisError> {
    let content = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let start = content
        .find('{')
        .ok_or_else(|| AnalysisError("AI response contained no JSON object".to_owned()))?;
    let end = content
        .rfind('}')
        .ok_or_else(|| AnalysisError("AI response contained incomplete JSON".to_owned()))?;
    let decision: RemoteDecision = serde_json::from_str(&content[start..=end])
        .map_err(|error| AnalysisError(format!("AI decision JSON was invalid: {error}")))?;
    for (name, score) in [
        ("activity_match", decision.activity_match),
        ("geography_match", decision.geography_match),
        ("previous_work_match", decision.previous_work_match),
    ] {
        if !score.is_finite() || !(0.0..=1.0).contains(&score) {
            return Err(AnalysisError(format!("AI {name} must be between 0 and 1")));
        }
    }
    if decision.qualification_risk.trim().is_empty() || decision.relevance.trim().is_empty() {
        return Err(AnalysisError(
            "AI decision labels must not be empty".to_owned(),
        ));
    }
    if !QUALIFICATION_RISK_OPTIONS.contains(&decision.qualification_risk.as_str()) {
        return Err(AnalysisError(format!(
            "AI qualification_risk must be one of: {}",
            QUALIFICATION_RISK_OPTIONS.join(", ")
        )));
    }
    if !RELEVANCE_OPTIONS.contains(&decision.relevance.as_str()) {
        return Err(AnalysisError(format!(
            "AI relevance must be one of: {}",
            RELEVANCE_OPTIONS.join(", ")
        )));
    }
    let raw = decision.raw;
    for (name, answer) in &raw {
        if !answer.score.is_finite()
            || !answer.confidence.is_finite()
            || !(0.0..=1.0).contains(&answer.score)
            || !(0.0..=1.0).contains(&answer.confidence)
        {
            return Err(AnalysisError(format!(
                "AI raw answer {name} must contain scores between 0 and 1"
            )));
        }
    }
    let raw = if raw.is_empty() {
        let mut map = BTreeMap::new();
        for (name, score) in [
            ("activity_match", decision.activity_match),
            ("geography_match", decision.geography_match),
            ("previous_work_match", decision.previous_work_match),
        ] {
            map.insert(
                name.to_owned(),
                serde_json::to_value(ScoreAnswer {
                    score,
                    confidence: 0.5,
                })
                .expect("ScoreAnswer serialization cannot fail"),
            );
        }
        map
    } else {
        raw.into_iter()
            .map(|(k, v)| {
                (
                    k,
                    serde_json::to_value(v).expect("ScoreAnswer serialization cannot fail"),
                )
            })
            .collect()
    };
    let decision = AnalysisDecision {
        provider: format!("openai-compatible:{model}"),
        prompt_version: Some(OPENAI_PROMPT_VERSION.to_owned()),
        activity_match: Some(decision.activity_match),
        geography_match: Some(decision.geography_match),
        previous_work_match: Some(decision.previous_work_match),
        qualification_risk: Some(decision.qualification_risk),
        relevance: Some(decision.relevance),
        raw,
        cached: false,
    };
    decision.validate_size()?;
    Ok(decision)
}

impl Analyzer for DeterministicAnalyzer {
    fn analyze(
        &self,
        company: &CompanyProfile,
        tender: &Tender,
        history: &[AwardRecord],
        preferences: &CompanyPreferences,
    ) -> Result<AnalysisDecision, AnalysisError> {
        let result = match_company_to_tender(company, tender, history, preferences);
        let signals = match_signals(company, tender, history, preferences);
        let activity_match = if signals.activity >= 0.5 { 1.0 } else { 0.0 };
        // Read the geography from the matcher's own signal instead of
        // re-deciding it here: an "anywhere in the EEA" tender is neutral in
        // `region_match`, and the analyzer used to call it a mismatch because
        // its region list was non-empty.
        let geography_match = if signals.region >= 1.0 {
            1.0
        } else if signals.region >= 0.5 {
            0.5
        } else {
            0.0
        };
        let previous_work_match = if signals.history_match == 1.0 {
            1.0
        } else if history.is_empty() {
            0.5
        } else {
            0.0
        };
        let qualification_risk = qualification_risk(tender, history);
        let relevance = if result.score >= 70.0 {
            "strong"
        } else if result.score >= 45.0 {
            "possible"
        } else {
            "poor"
        };
        let mut raw = BTreeMap::new();
        raw.insert(
            "activity_match".to_owned(),
            serde_json::to_value(ScoreAnswer {
                score: activity_match,
                confidence: result.confidence,
            })
            .expect("ScoreAnswer serialization cannot fail"),
        );
        raw.insert(
            "geography_match".to_owned(),
            serde_json::to_value(ScoreAnswer {
                score: geography_match,
                confidence: result.confidence,
            })
            .expect("ScoreAnswer serialization cannot fail"),
        );
        raw.insert(
            "previous_work_match".to_owned(),
            serde_json::to_value(ScoreAnswer {
                score: previous_work_match,
                confidence: result.confidence,
            })
            .expect("ScoreAnswer serialization cannot fail"),
        );
        Ok(AnalysisDecision {
            provider: "deterministic-rust".to_owned(),
            prompt_version: Some(DETERMINISTIC_CACHE_IDENTITY.to_owned()),
            activity_match: Some(activity_match),
            geography_match: Some(geography_match),
            previous_work_match: Some(previous_work_match),
            qualification_risk: Some(qualification_risk.to_owned()),
            relevance: Some(relevance.to_owned()),
            raw,
            cached: false,
        })
    }
}

impl Analyzer for RemoteAnalyzer {
    fn analyze(
        &self,
        company: &CompanyProfile,
        tender: &Tender,
        history: &[AwardRecord],
        preferences: &CompanyPreferences,
    ) -> Result<AnalysisDecision, AnalysisError> {
        RemoteAnalyzer::analyze(self, company, tender, history, preferences)
    }
}

pub fn analyze(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
) -> AnalysisDecision {
    DeterministicAnalyzer
        .analyze(company, tender, history, preferences)
        .expect("deterministic analyzer cannot fail")
}

pub const DEFAULT_TYPESAFE_BASE_URL: &str = "https://api.typesafe.ai";
pub const DEFAULT_TYPESAFE_MODEL: &str = "jev-latest";

#[derive(Clone)]
pub struct TypeSafeAnalyzer {
    base_url: String,
    endpoint: String,
    api_key: String,
    model: String,
    timeout_seconds: u64,
}

impl fmt::Debug for TypeSafeAnalyzer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let clean_base = redacted_endpoint(&self.base_url);
        let clean_endpoint = redacted_endpoint(&self.endpoint);
        formatter
            .debug_struct("TypeSafeAnalyzer")
            .field("base_url", &clean_base.trim_end_matches('/'))
            .field("endpoint", &clean_endpoint.trim_end_matches('/'))
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("timeout_seconds", &self.timeout_seconds)
            .finish()
    }
}

impl TypeSafeAnalyzer {
    pub fn new(
        base_url: Option<&str>,
        api_key: &str,
        model: Option<&str>,
        timeout_seconds: Option<u64>,
    ) -> Result<Self, AnalysisError> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(AnalysisError(
                "TYPESAFE_API_KEY must not be empty".to_owned(),
            ));
        }
        let base_url = base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_TYPESAFE_BASE_URL)
            .trim_end_matches('/')
            .to_owned();
        let parsed_base = Url::parse(&base_url)
            .map_err(|_| AnalysisError("TYPESAFE_BASE_URL must be an absolute URL".to_owned()))?;
        if !parsed_base.username().is_empty()
            || parsed_base.password().is_some()
            || parsed_base.query().is_some()
            || parsed_base.fragment().is_some()
        {
            return Err(AnalysisError(
                "TYPESAFE_BASE_URL must not contain credentials, a query, or a fragment".to_owned(),
            ));
        }
        let model = model
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_TYPESAFE_MODEL)
            .to_owned();
        let endpoint = format!("{base_url}/v1/systemone");
        if !endpoint_allowed(&endpoint) {
            return Err(AnalysisError(format!(
                "TypeSafe endpoint is not allowed: {}",
                redacted_endpoint(&endpoint)
            )));
        }
        let timeout_seconds = timeout_seconds.unwrap_or(60);
        let timeout_seconds = if (1..=300).contains(&timeout_seconds) {
            timeout_seconds
        } else {
            60
        };
        Ok(Self {
            base_url,
            endpoint,
            api_key: api_key.to_owned(),
            model,
            timeout_seconds,
        })
    }

    pub fn from_env() -> Result<Option<Self>, AnalysisError> {
        let Some(api_key) = non_empty_trimmed(env::var("TYPESAFE_API_KEY").ok()) else {
            return Ok(None);
        };
        let base_url = env::var("TYPESAFE_BASE_URL").ok();
        let model = env::var("TYPESAFE_DEFAULT_MODEL").ok();
        let timeout_seconds = env::var("TYPESAFE_TIMEOUT_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        Self::new(
            base_url.as_deref(),
            &api_key,
            model.as_deref(),
            timeout_seconds,
        )
        .map(Some)
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    pub fn cache_identity(&self) -> String {
        let digest = Sha256::digest(self.base_url.as_bytes());
        let mut base_hash = String::with_capacity(64);
        for byte in digest {
            write!(&mut base_hash, "{byte:02x}").expect("writing to a String cannot fail");
        }
        format!(
            "typesafe:{}:{base_hash}:{}",
            self.model, TYPESAFE_QUESTIONS_VERSION
        )
    }

    pub fn prompt_version(&self) -> &'static str {
        TYPESAFE_QUESTIONS_VERSION
    }

    pub fn analyze(
        &self,
        company: &CompanyProfile,
        tender: &Tender,
        history: &[AwardRecord],
        _preferences: &CompanyPreferences,
    ) -> Result<AnalysisDecision, AnalysisError> {
        let state_json = build_typesafe_state(company, tender, history);
        let state_str = serde_json::to_string(&state_json).map_err(|error| {
            AnalysisError(format!("TypeSafe state could not be encoded: {error}"))
        })?;
        let payload = serde_json::json!({
            "state": state_str,
            "model": self.model,
            "questions": typesafe_questions(),
        });
        let payload_bytes = encode_remote_payload(&payload)?;
        let agent = AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(self.timeout_seconds))
            .build();
        let response = match agent
            .post(&self.endpoint)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_bytes(&payload_bytes)
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(status, resp)) => {
                let mut detail = String::new();
                let _ = resp.into_reader().take(1024).read_to_string(&mut detail);
                let safe_detail = sanitize_typesafe_error_message(
                    &detail,
                    &self.api_key,
                    &self.base_url,
                    &self.endpoint,
                );
                let safe_detail = safe_detail.trim();
                let message = if safe_detail.is_empty() {
                    format!("TypeSafe request failed with HTTP {status}")
                } else {
                    format!("TypeSafe request failed with HTTP {status}: {safe_detail}")
                };
                return Err(AnalysisError(message));
            }
            Err(ureq::Error::Transport(err)) => {
                let msg = sanitize_typesafe_error_message(
                    &err.to_string(),
                    &self.api_key,
                    &self.base_url,
                    &self.endpoint,
                );
                return Err(AnalysisError(format!("TypeSafe request failed: {msg}")));
            }
        };
        let body = read_bounded_response(response)?;
        parse_typesafe_response(&body, &self.model)
    }
}

fn sanitize_error_message(message: &str, api_key: &str) -> String {
    let mut sanitized = message.to_owned();
    let key = api_key.trim();
    if !key.is_empty() {
        sanitized = sanitized.replace(key, "[REDACTED]");
    }
    sanitized
}

fn sanitize_typesafe_error_message(
    message: &str,
    api_key: &str,
    base_url: &str,
    endpoint: &str,
) -> String {
    let mut sanitized = sanitize_error_message(message, api_key);
    for url in [base_url, endpoint] {
        if !url.is_empty() {
            sanitized = sanitized.replace(url, &redacted_endpoint(url));
        }
    }
    sanitized
}

pub fn build_typesafe_state(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
) -> serde_json::Value {
    let activities: Vec<serde_json::Value> = company
        .activities
        .iter()
        .map(|a| {
            serde_json::json!({
                "code": a.code,
                "french": a.french,
                "dutch": a.dutch,
            })
        })
        .collect();

    let regions: Vec<Option<String>> = company
        .addresses
        .iter()
        .map(|a| {
            a.municipality_nl
                .as_ref()
                .filter(|s| !s.is_empty())
                .cloned()
                .or_else(|| {
                    a.municipality_fr
                        .as_ref()
                        .filter(|s| !s.is_empty())
                        .cloned()
                })
        })
        .collect();

    let previous_awarded_cpvs: Vec<String> = history
        .iter()
        .flat_map(|award| award.cpv_codes.iter().cloned())
        .collect();

    let value = tender.estimated_value.map(|v| v.to_string());
    let requirements: Vec<&str> = tender
        .requirements
        .iter()
        .map(|r| r.text.as_str())
        .collect();

    serde_json::json!({
        "company": {
            "activities": activities,
            "regions": regions,
            "previous_awarded_cpvs": previous_awarded_cpvs,
        },
        "tender": {
            "title": &tender.title,
            "description": &tender.description,
            "cpv_codes": &tender.cpv_codes,
            "regions": &tender.regions,
            "value": value,
            "requirements": requirements,
        }
    })
}

pub fn typesafe_questions() -> serde_json::Value {
    serde_json::json!({
        "activity_match": {
            "type": "score",
            "instructions": "How well do the company's actual activities match this tender?",
            "criteria": [
                "No match",
                "Partial match",
                "Strong match"
            ]
        },
        "geography_match": {
            "type": "score",
            "instructions": "How well does the tender location fit the company locations?",
            "criteria": [
                "No match",
                "Possible match",
                "Strong match"
            ]
        },
        "previous_work_match": {
            "type": "score",
            "instructions": "How similar is this tender to the company's previous awarded work?",
            "criteria": [
                "No similar work",
                "Somewhat similar",
                "Very similar"
            ]
        },
        "qualification_risk": {
            "type": "choice",
            "instructions": "What is the main qualification risk to verify?",
            "criteria": {
                "none_obvious": "No obvious qualification risk in the supplied state",
                "certification": "A certification or professional registration may be required",
                "capacity": "Turnover, references or delivery capacity may be a concern",
                "unclear": "The supplied information is insufficient to assess requirements"
            }
        },
        "relevance": {
            "type": "choice",
            "instructions": "How relevant is this tender for the company?",
            "criteria": {
                "poor": "Probably not relevant",
                "possible": "Possibly relevant; manual review needed",
                "strong": "Strong candidate for manual review"
            }
        }
    })
}

#[derive(Debug, Deserialize)]
struct TypeSafeResponse {
    #[serde(default)]
    #[allow(dead_code)]
    model: Option<String>,
    answers: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    usage: Option<serde_json::Value>,
}

fn validate_answer_type(
    answer: &serde_json::Value,
    name: &str,
    expected_type: &str,
) -> Result<(), AnalysisError> {
    if answer.get("type").and_then(|value| value.as_str()) != Some(expected_type) {
        return Err(AnalysisError(format!(
            "TypeSafe {name} answer type must be {expected_type}"
        )));
    }
    Ok(())
}

fn validate_confidence(answer: &serde_json::Value, name: &str) -> Result<(), AnalysisError> {
    let confidence = answer
        .get("confidence")
        .and_then(|value| value.as_f64())
        .ok_or_else(|| AnalysisError(format!("TypeSafe {name} answer missing confidence")))?;
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(AnalysisError(format!(
            "TypeSafe {name} confidence must be between 0 and 1"
        )));
    }
    Ok(())
}

fn validate_probability_distribution(
    answer: &serde_json::Value,
    name: &str,
    expected_keys: &[&str],
) -> Result<(), AnalysisError> {
    let probabilities = answer
        .get("probabilities")
        .and_then(|value| value.as_object())
        .ok_or_else(|| AnalysisError(format!("TypeSafe {name} answer missing probabilities")))?;
    if probabilities.len() != expected_keys.len()
        || expected_keys
            .iter()
            .any(|key| !probabilities.contains_key(*key))
    {
        return Err(AnalysisError(format!(
            "TypeSafe {name} probabilities must cover exactly {expected_keys:?}"
        )));
    }
    let mut total = 0.0;
    for key in expected_keys {
        let probability = probabilities[*key].as_f64().ok_or_else(|| {
            AnalysisError(format!(
                "TypeSafe {name} probability for {key} must be numeric"
            ))
        })?;
        if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
            return Err(AnalysisError(format!(
                "TypeSafe {name} probability for {key} must be between 0 and 1"
            )));
        }
        total += probability;
    }
    if (total - 1.0).abs() > 0.001 {
        return Err(AnalysisError(format!(
            "TypeSafe {name} probabilities must sum to 1"
        )));
    }
    Ok(())
}

fn parse_score_value(answer: &serde_json::Value, name: &str) -> Result<Option<f64>, AnalysisError> {
    validate_answer_type(answer, name, "score")?;
    let Some(score_value) = answer.get("score") else {
        return Ok(None);
    };
    if score_value.is_null() {
        return Ok(None);
    }
    let score = score_value
        .as_f64()
        .ok_or_else(|| AnalysisError(format!("TypeSafe {name} score must be numeric")))?;
    validate_confidence(answer, name)?;
    validate_probability_distribution(answer, name, &["0", "1", "2"])?;
    let legend = answer
        .get("legend")
        .and_then(|value| value.as_object())
        .ok_or_else(|| AnalysisError(format!("TypeSafe {name} answer missing legend")))?;
    if legend.len() != 3
        || ["0", "1", "2"]
            .iter()
            .any(|key| legend.get(*key).and_then(|value| value.as_str()).is_none())
    {
        return Err(AnalysisError(format!(
            "TypeSafe {name} legend must define levels 0, 1, and 2"
        )));
    }
    if !score.is_finite() || !(0.0..=2.0).contains(&score) {
        return Err(AnalysisError(format!(
            "TypeSafe {name} score must be between 0 and 2"
        )));
    }
    Ok(Some(score))
}

fn parse_choice_value(
    answer: &serde_json::Value,
    name: &str,
    valid_options: &[&str],
) -> Result<Option<String>, AnalysisError> {
    validate_answer_type(answer, name, "choice")?;
    let Some(choice_value) = answer.get("choice") else {
        return Ok(None);
    };
    if choice_value.is_null() {
        return Ok(None);
    }
    let choice = choice_value
        .as_str()
        .ok_or_else(|| AnalysisError(format!("TypeSafe {name} choice must be a string")))?;
    validate_confidence(answer, name)?;
    validate_probability_distribution(answer, name, valid_options)?;
    let trimmed = choice.trim();
    if trimmed.is_empty() || !valid_options.contains(&trimmed) {
        return Err(AnalysisError(format!(
            "TypeSafe {name} choice must be one of {valid_options:?}"
        )));
    }
    Ok(Some(trimmed.to_owned()))
}

pub fn parse_typesafe_response(
    body: &[u8],
    configured_model: &str,
) -> Result<AnalysisDecision, AnalysisError> {
    if body.len() > MAX_AI_RESPONSE_BYTES {
        return Err(AnalysisError(format!(
            "TypeSafe response exceeded the {MAX_AI_RESPONSE_BYTES} byte limit"
        )));
    }
    let response: TypeSafeResponse = serde_json::from_slice(body)
        .map_err(|error| AnalysisError(format!("TypeSafe response was not valid JSON: {error}")))?;

    if response.answers.keys().any(|key| {
        !matches!(
            key.as_str(),
            "activity_match"
                | "geography_match"
                | "previous_work_match"
                | "qualification_risk"
                | "relevance"
        )
    }) {
        return Err(AnalysisError(
            "TypeSafe response contained an unrecognized answer key".into(),
        ));
    }

    let activity_match = response
        .answers
        .get("activity_match")
        .filter(|answer| !answer.is_null())
        .map(|answer| parse_score_value(answer, "activity_match"))
        .transpose()?
        .flatten();
    let geography_match = response
        .answers
        .get("geography_match")
        .filter(|answer| !answer.is_null())
        .map(|answer| parse_score_value(answer, "geography_match"))
        .transpose()?
        .flatten();
    let previous_work_match = response
        .answers
        .get("previous_work_match")
        .filter(|answer| !answer.is_null())
        .map(|answer| parse_score_value(answer, "previous_work_match"))
        .transpose()?
        .flatten();

    let qualification_risk = response
        .answers
        .get("qualification_risk")
        .filter(|answer| !answer.is_null())
        .map(|answer| parse_choice_value(answer, "qualification_risk", &QUALIFICATION_RISK_OPTIONS))
        .transpose()?
        .flatten();
    let relevance = response
        .answers
        .get("relevance")
        .filter(|answer| !answer.is_null())
        .map(|answer| parse_choice_value(answer, "relevance", &RELEVANCE_OPTIONS))
        .transpose()?
        .flatten();

    let decision = AnalysisDecision {
        provider: format!("typesafe:{configured_model}"),
        prompt_version: Some(TYPESAFE_QUESTIONS_VERSION.to_owned()),
        activity_match,
        geography_match,
        previous_work_match,
        qualification_risk,
        relevance,
        raw: response.answers,
        cached: false,
    };
    decision.validate_size()?;
    Ok(decision)
}

impl Analyzer for TypeSafeAnalyzer {
    fn analyze(
        &self,
        company: &CompanyProfile,
        tender: &Tender,
        history: &[AwardRecord],
        preferences: &CompanyPreferences,
    ) -> Result<AnalysisDecision, AnalysisError> {
        TypeSafeAnalyzer::analyze(self, company, tender, history, preferences)
    }
}

/// Unified analyzer combining optional TypeSafe and OpenAI-compatible providers
/// with deterministic fallback and precedence.
#[derive(Clone, Debug)]
pub enum ConfiguredAnalyzer {
    TypeSafe(TypeSafeAnalyzer),
    OpenAi(RemoteAnalyzer),
}

impl ConfiguredAnalyzer {
    /// Loads the active analyzer from environment variables according to
    /// deterministic precedence:
    /// 1. TypeSafe/Jev (`TYPESAFE_API_KEY` set and valid base URL)
    /// 2. OpenAI-compatible (`AANBESTEDINGEN_AI_API_KEY` and `AANBESTEDINGEN_AI_ENDPOINT` set and valid)
    /// 3. None (deterministic analysis)
    ///
    /// If an optional provider is configured but fails at runtime, analysis returns
    /// an error rather than silently falling back to another provider.
    pub fn from_env() -> Result<Option<Self>, AnalysisError> {
        if let Some(typesafe) = TypeSafeAnalyzer::from_env()? {
            return Ok(Some(Self::TypeSafe(typesafe)));
        }
        Ok(Self::select(None, RemoteAnalyzer::from_env()?))
    }

    fn select(typesafe: Option<TypeSafeAnalyzer>, openai: Option<RemoteAnalyzer>) -> Option<Self> {
        if let Some(typesafe) = typesafe {
            Some(Self::TypeSafe(typesafe))
        } else {
            openai.map(Self::OpenAi)
        }
    }

    pub fn provider_name(&self) -> &'static str {
        match self {
            Self::TypeSafe(_) => "typesafe",
            Self::OpenAi(_) => "openai-compatible",
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Self::TypeSafe(analyzer) => analyzer.model(),
            Self::OpenAi(analyzer) => analyzer.model(),
        }
    }

    pub fn timeout_seconds(&self) -> u64 {
        match self {
            Self::TypeSafe(analyzer) => analyzer.timeout_seconds(),
            Self::OpenAi(analyzer) => analyzer.timeout_seconds(),
        }
    }

    pub fn cache_identity(&self) -> String {
        match self {
            Self::TypeSafe(analyzer) => analyzer.cache_identity(),
            Self::OpenAi(analyzer) => analyzer.cache_identity(),
        }
    }

    pub fn prompt_version(&self) -> &'static str {
        match self {
            Self::TypeSafe(analyzer) => analyzer.prompt_version(),
            Self::OpenAi(analyzer) => analyzer.prompt_version(),
        }
    }

    pub fn analyze(
        &self,
        company: &CompanyProfile,
        tender: &Tender,
        history: &[AwardRecord],
        preferences: &CompanyPreferences,
    ) -> Result<AnalysisDecision, AnalysisError> {
        match self {
            Self::TypeSafe(analyzer) => analyzer.analyze(company, tender, history, preferences),
            Self::OpenAi(analyzer) => analyzer.analyze(company, tender, history, preferences),
        }
    }
}

impl Analyzer for ConfiguredAnalyzer {
    fn analyze(
        &self,
        company: &CompanyProfile,
        tender: &Tender,
        history: &[AwardRecord],
        preferences: &CompanyPreferences,
    ) -> Result<AnalysisDecision, AnalysisError> {
        self.analyze(company, tender, history, preferences)
    }
}

/// Persistent-cache key for analysis decisions: sha256 over the provider
/// identity (which embeds the prompt version) and the full decision inputs
/// (company, tender, history, preferences). Keep the serialized shape stable: rows in the
/// analysis_cache table depend on it.
pub fn analysis_cache_key(
    provider: &str,
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
) -> String {
    let serialized = serde_json::to_string(&(provider, company, tender, history, preferences))
        .unwrap_or_else(|_| format!("{}:{}", company.enterprise_number, tender.external_id));
    let digest = Sha256::digest(serialized.as_bytes());
    let mut key = String::with_capacity(64);
    for byte in digest {
        write!(&mut key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Activity, Address};
    use std::collections::HashMap;

    #[test]
    fn fresh_analyzer_decisions_are_uncached() {
        let company = crate::CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            primary_cpv_prefixes: vec!["72000000".to_owned()],
            ..Default::default()
        };
        let decision = DeterministicAnalyzer
            .analyze(
                &company,
                &Tender::default(),
                &[],
                &CompanyPreferences::default(),
            )
            .expect("deterministic analysis cannot fail");
        assert!(!decision.cached);
    }

    #[test]
    fn cache_rows_predating_the_cached_flag_still_deserialize() {
        let old_row = serde_json::json!({
            "provider": "deterministic-rust",
            "activity_match": 1.0,
            "geography_match": 0.5,
            "previous_work_match": 0.0,
            "qualification_risk": "none_obvious",
            "relevance": "possible",
            "raw": {},
        });
        let decision: AnalysisDecision =
            serde_json::from_value(old_row).expect("old cache rows must deserialize");
        assert!(!decision.cached);
    }

    #[test]
    fn enforces_secure_remote_endpoints() {
        assert!(endpoint_allowed(
            "https://api.example.com/v1/chat/completions"
        ));
        assert!(endpoint_allowed("http://127.0.0.1:8080/mock"));
        assert!(endpoint_allowed("http://[::1]:8080/mock"));
        assert!(!endpoint_allowed(
            "http://api.example.com/v1/chat/completions"
        ));
        assert!(!endpoint_allowed("not-a-url"));
        assert!(!endpoint_allowed("https:///v1/chat/completions"));
        assert!(!endpoint_allowed("ftp://localhost/v1/chat/completions"));
    }

    #[test]
    fn trims_required_remote_configuration_values() {
        assert_eq!(
            non_empty_trimmed(Some(" https://example.test ".to_owned())),
            Some("https://example.test".to_owned())
        );
        assert_eq!(non_empty_trimmed(Some("   ".to_owned())), None);
        assert_eq!(non_empty_trimmed(None), None);
    }

    #[test]
    fn remote_configuration_fails_closed_when_partial_or_unsafe() {
        assert!(RemoteAnalyzer::from_config(None, None, None, None, None)
            .expect("unset provider should be absent")
            .is_none());

        let partial = RemoteAnalyzer::from_config(
            Some("https://example.test/v1/chat/completions".to_owned()),
            None,
            None,
            None,
            None,
        )
        .expect_err("an endpoint without a key must be rejected")
        .to_string();
        assert!(partial.contains("must be set together"));

        let unsafe_endpoint = RemoteAnalyzer::from_config(
            Some("http://example.test/v1/chat/completions".to_owned()),
            Some("secret-key".to_owned()),
            None,
            None,
            None,
        )
        .expect_err("non-loopback HTTP must be rejected")
        .to_string();
        assert!(unsafe_endpoint.contains("not an allowed secure endpoint"));
        assert!(!unsafe_endpoint.contains("secret-key"));

        let configured = RemoteAnalyzer::from_config(
            Some("https://example.test/v1/chat/completions".to_owned()),
            Some("secret-key".to_owned()),
            None,
            None,
            None,
        )
        .expect("complete secure settings should be accepted")
        .expect("provider should be enabled");
        assert_eq!(configured.timeout_seconds(), 60);
        assert_eq!(configured.response_format, ResponseFormat::JsonSchema);
    }

    #[test]
    fn timeout_configuration_is_bounded() {
        assert_eq!(ai_timeout_seconds(None), 60);
        assert_eq!(ai_timeout_seconds(Some("1")), 1);
        assert_eq!(ai_timeout_seconds(Some("300")), 300);
        assert_eq!(ai_timeout_seconds(Some("0")), 60);
        assert_eq!(ai_timeout_seconds(Some("301")), 60);
        assert_eq!(ai_timeout_seconds(Some("invalid")), 60);
    }

    #[test]
    fn model_configuration_has_a_non_empty_default() {
        assert_eq!(ai_model(None), "gpt-4o-mini");
        assert_eq!(ai_model(Some("")), "gpt-4o-mini");
        assert_eq!(ai_model(Some("  ")), "gpt-4o-mini");
        assert_eq!(ai_model(Some("  local-model  ")), "local-model");
    }

    #[test]
    fn exposes_effective_remote_configuration() {
        let analyzer = RemoteAnalyzer {
            endpoint: "https://example.test/v1/chat/completions".to_owned(),
            api_key: "secret".to_owned(),
            model: "test-model".to_owned(),
            timeout_seconds: 17,
            response_format: ResponseFormat::JsonSchema,
        };
        assert_eq!(analyzer.model(), "test-model");
        assert_eq!(analyzer.timeout_seconds(), 17);
    }

    #[test]
    fn redacts_api_keys_from_debug_output() {
        let analyzer = RemoteAnalyzer {
            endpoint: "https://example.test/v1/chat/completions".to_owned(),
            api_key: "super-secret-key".to_owned(),
            model: "test-model".to_owned(),
            timeout_seconds: 17,
            response_format: ResponseFormat::JsonSchema,
        };
        let debug = format!("{analyzer:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("super-secret-key"));
        assert!(debug.contains("test-model"));

        let analyzer = RemoteAnalyzer {
            endpoint: "https://user:password@example.test/v1?token=query-secret#fragment"
                .to_owned(),
            api_key: "secret".to_owned(),
            model: "test-model".to_owned(),
            timeout_seconds: 17,
            response_format: ResponseFormat::JsonSchema,
        };
        let debug = format!("{analyzer:?}");
        assert!(debug.contains("https://example.test/v1"));
        assert!(!debug.contains("password"));
        assert!(!debug.contains("query-secret"));
        assert!(!debug.contains("fragment"));
    }

    #[test]
    fn rejects_oversized_remote_requests() {
        let payload = serde_json::json!({"content": "x".repeat(MAX_AI_REQUEST_BYTES) });
        let error =
            encode_remote_payload(&payload).expect_err("oversized AI request should be rejected");
        assert!(error.to_string().contains("exceeded"));
    }

    #[test]
    fn rejects_oversized_remote_responses() {
        let body = vec![b'{'; MAX_AI_RESPONSE_BYTES + 1];
        let error =
            parse_chat_response_body(&body).expect_err("oversized AI response should be rejected");
        assert!(error.to_string().contains("exceeded"));
    }

    #[test]
    fn parses_valid_remote_decision() {
        let decision = parse_remote_decision(
            r#"```json
            {"activity_match":0.8,"geography_match":0.5,"previous_work_match":0.2,"qualification_risk":"capacity","relevance":"possible"}
            ```"#,
            "test-model",
        )
        .expect("valid AI JSON should parse");
        assert_eq!(decision.provider, "openai-compatible:test-model");
        assert_eq!(decision.activity_match, Some(0.8));
        assert!(decision.raw.contains_key("activity_match"));
    }

    #[test]
    fn openai_prompt_matches_typed_decision_contract() {
        let prompt = openai_system_prompt();
        for option in QUALIFICATION_RISK_OPTIONS {
            assert!(prompt.contains(option), "missing risk option {option}");
        }
        for option in RELEVANCE_OPTIONS {
            assert!(prompt.contains(option), "missing relevance option {option}");
        }
        assert!(prompt.contains("finite numbers in [0, 1]"));
        assert!(prompt.contains("Judge thematic relevance separately"));
        assert!(prompt.contains("do not infer unknown qualifications or legal eligibility"));
    }

    #[test]
    fn rejects_unknown_remote_decision_labels() {
        let base = r#"{"activity_match":0.8,"geography_match":0.5,"previous_work_match":0.2,"qualification_risk":"capacity","relevance":"strong"}"#;
        let invalid_risk = base.replace("capacity", "low");
        let error = parse_remote_decision(&invalid_risk, "test-model")
            .expect_err("unknown qualification risk must be rejected");
        assert!(error
            .to_string()
            .contains("qualification_risk must be one of"));

        let invalid_relevance = base.replace("strong", "high");
        let error = parse_remote_decision(&invalid_relevance, "test-model")
            .expect_err("unknown relevance must be rejected");
        assert!(error.to_string().contains("relevance must be one of"));
    }

    #[test]
    fn audit_rejects_oversized_openai_decisions() {
        let mut raw = BTreeMap::new();
        raw.insert(
            "x".repeat(70_000),
            ScoreAnswer {
                score: 0.5,
                confidence: 0.5,
            },
        );
        let body = serde_json::json!({
            "activity_match": 0.5,
            "geography_match": 0.5,
            "previous_work_match": 0.5,
            "qualification_risk": "none_obvious",
            "relevance": "possible",
            "raw": raw,
        });
        assert!(parse_remote_decision(&body.to_string(), "test")
            .unwrap_err()
            .to_string()
            .contains("byte limit"));
    }

    #[test]
    fn rejects_remote_scores_outside_range() {
        let error = parse_remote_decision(
            r#"{"activity_match":1.2,"geography_match":0.5,"previous_work_match":0.2,"qualification_risk":"none_obvious","relevance":"poor"}"#,
            "test-model",
        )
        .expect_err("out-of-range AI score should be rejected");
        assert!(error.to_string().contains("activity_match"));
        let error = parse_remote_decision(
            r#"{"activity_match":0.8,"geography_match":0.5,"previous_work_match":0.2,"qualification_risk":"none_obvious","relevance":"possible","raw":{"activity_match":{"score":0.8,"confidence":1.2}}}"#,
            "test-model",
        )
        .expect_err("out-of-range raw confidence should be rejected");
        assert!(error.to_string().contains("raw answer"));
    }

    /// Read one HTTP request completely, including any declared body, so the
    /// mock never closes a socket with unread data.
    fn read_full_request(stream: &mut std::net::TcpStream) {
        use std::io::Read;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("mock read timeout should be set");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let size = stream.read(&mut chunk).expect("request should read");
            if size == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..size]);
            let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let header_end = header_end + 4;
            let content_length = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if request.len() >= header_end + content_length {
                break;
            }
        }
    }

    /// Half-close the response side and drain the peer before dropping, so the
    /// connection ends gracefully instead of with a reset.
    fn finish_response(mut stream: std::net::TcpStream) {
        use std::io::Read;
        use std::net::Shutdown;
        let _ = stream.shutdown(Shutdown::Write);
        let mut sink = [0_u8; 1024];
        while matches!(stream.read(&mut sink), Ok(size) if size > 0) {}
    }

    #[test]
    fn audit_remote_http_errors_never_echo_endpoint_secrets() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
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
            reader.read_exact(&mut vec![0; length]).unwrap();
            write!(
                reader.get_mut(),
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        let analyzer = RemoteAnalyzer {
            endpoint: format!("http://dummy-user:dummy-password@{address}/test?token=query-secret"),
            api_key: "bearer-secret".into(),
            model: "test".into(),
            timeout_seconds: 5,
            response_format: ResponseFormat::JsonSchema,
        };
        let company: CompanyProfile = serde_json::from_value(serde_json::json!({
            "enterprise_number": "0123.456.789", "names": [], "activities": [], "addresses": [], "establishments": []
        })).unwrap();
        let message = analyzer
            .analyze(
                &company,
                &Tender::default(),
                &[],
                &CompanyPreferences::default(),
            )
            .unwrap_err()
            .to_string();
        server.join().unwrap();
        assert_eq!(message, "AI request failed with HTTP 401");
        for secret in [
            "dummy-user",
            "dummy-password",
            "query-secret",
            "bearer-secret",
            &address.to_string(),
        ] {
            assert!(!message.contains(secret));
        }
        let transport = RemoteAnalyzer {
            endpoint: "http://dummy-user:dummy-password@127.0.0.1:0/test?token=query-secret".into(),
            ..analyzer
        };
        let error = transport
            .analyze(
                &company,
                &Tender::default(),
                &[],
                &CompanyPreferences::default(),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "AI request failed (transport error)");
    }

    #[test]
    fn calls_openai_compatible_endpoint() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock request should arrive");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let size = stream.read(&mut chunk).expect("request should read");
                if size == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..size]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let header_end = header_end + 4;
                let header = String::from_utf8_lossy(&request[..header_end]);
                let content_length = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= header_end + content_length {
                    break;
                }
            }
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .expect("request headers should end")
                + 4;
            let payload: serde_json::Value = serde_json::from_slice(&request[header_end..])
                .expect("OpenAI request body should be JSON");
            let prompt = payload["messages"][0]["content"]
                .as_str()
                .expect("system prompt should be a string");
            assert!(prompt.contains("none_obvious, certification, capacity, unclear"));
            assert!(prompt.contains("poor, possible, strong"));
            assert!(prompt.contains("finite numbers in [0, 1]"));
            let user_content: serde_json::Value = serde_json::from_str(
                payload["messages"][1]["content"]
                    .as_str()
                    .expect("user message content should be a string"),
            )
            .expect("user content should be JSON");
            assert_eq!(
                user_content["company"]["primary_cpv_prefixes"],
                serde_json::json!(["72000000"])
            );
            let request = String::from_utf8_lossy(&request);
            assert!(request.contains("Authorization: Bearer test-key"));
            let body = r#"{"choices":[{"message":{"content":"{\"activity_match\":0.9,\"geography_match\":0.5,\"previous_work_match\":0.4,\"qualification_risk\":\"none_obvious\",\"relevance\":\"strong\"}"}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("mock response should write");
        });
        let analyzer = RemoteAnalyzer {
            endpoint: format!("http://{address}"),
            api_key: "test-key".to_owned(),
            model: "test-model".to_owned(),
            timeout_seconds: 60,
            response_format: ResponseFormat::JsonSchema,
        };
        let company = CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            primary_cpv_prefixes: vec!["72000000".to_owned()],
            ..Default::default()
        };
        let decision = analyzer
            .analyze(
                &company,
                &Tender::default(),
                &[],
                &CompanyPreferences::default(),
            )
            .expect("mock AI response should parse");
        server.join().expect("mock server should finish");
        assert_eq!(decision.provider, "openai-compatible:test-model");
        assert_eq!(decision.relevance.as_deref(), Some("strong"));
        assert_eq!(decision.activity_match, Some(0.9));
    }

    #[test]
    fn exposes_structured_relevance_without_ai() {
        let company = CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![Activity {
                code: "43210".to_owned(),
                classification: "MAIN".to_owned(),
                french: Some("Installation électrique".to_owned()),
                dutch: Some("Elektrische installatie".to_owned()),
            }],
            addresses: vec![Address {
                municipality_nl: Some("Gent".to_owned()),
                ..Address::default()
            }],
            establishments: vec![],
            ..Default::default()
        };
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "1".to_owned(),
            title: "Elektrische installatie in Gent".to_owned(),
            regions: vec!["Gent".to_owned()],
            ..Tender::default()
        };
        let decision = analyze(&company, &tender, &[], &CompanyPreferences::default());
        assert_eq!(decision.provider, "deterministic-rust");
        assert_eq!(decision.relevance.as_deref(), Some("possible"));
        assert!(decision.raw.contains_key("activity_match"));
        let _: HashMap<_, _> = decision.raw.into_iter().collect();
    }

    #[test]
    fn analysis_cache_key_includes_preferences() {
        let company = CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            primary_cpv_prefixes: vec!["72000000".to_owned()],
            ..Default::default()
        };
        let tender = Tender::default();
        let history = Vec::new();
        let first = CompanyPreferences::default();
        let mut second = CompanyPreferences::default();
        second.excluded_terms.push("night work".to_owned());
        assert_ne!(
            analysis_cache_key(
                DETERMINISTIC_CACHE_IDENTITY,
                &company,
                &tender,
                &history,
                &first
            ),
            analysis_cache_key(
                DETERMINISTIC_CACHE_IDENTITY,
                &company,
                &tender,
                &history,
                &second
            )
        );
    }

    #[test]
    fn analysis_cache_key_is_deterministic_and_provider_scoped() {
        let company = CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            primary_cpv_prefixes: vec!["72000000".to_owned()],
            ..Default::default()
        };
        let tender = Tender::default();
        let history = Vec::new();
        let preferences = CompanyPreferences::default();
        let first = analysis_cache_key(
            DETERMINISTIC_CACHE_IDENTITY,
            &company,
            &tender,
            &history,
            &preferences,
        );
        assert_eq!(
            first,
            analysis_cache_key(
                DETERMINISTIC_CACHE_IDENTITY,
                &company,
                &tender,
                &history,
                &preferences,
            )
        );
        assert_ne!(
            first,
            analysis_cache_key(
                "openai-compatible:https://example.test:model",
                &company,
                &tender,
                &history,
                &preferences,
            )
        );
    }

    #[test]
    fn typesafe_request_payload_matches_jev_specification() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock request should arrive");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let size = stream.read(&mut chunk).expect("request should read");
                if size == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..size]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let header_end = header_end + 4;
                let header = String::from_utf8_lossy(&request[..header_end]);
                let content_length = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= header_end + content_length {
                    break;
                }
            }
            let request_str = String::from_utf8_lossy(&request);
            assert!(request_str.starts_with("POST /v1/systemone HTTP/1.1"));
            assert!(
                request_str.contains("authorization: Bearer test-typesafe-key")
                    || request_str.contains("Authorization: Bearer test-typesafe-key")
            );
            assert!(
                request_str.contains("content-type: application/json")
                    || request_str.contains("Content-Type: application/json")
            );

            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body_json: serde_json::Value =
                serde_json::from_slice(&request[header_end..]).expect("body must be json");
            assert_eq!(body_json["model"], "jev-latest");

            let state_str = body_json["state"]
                .as_str()
                .expect("state must be json string");
            let state: serde_json::Value =
                serde_json::from_str(state_str).expect("state must parse as json");
            assert_eq!(state["company"]["activities"][0]["code"], "43210");
            assert_eq!(
                state["company"]["activities"][0]["french"],
                "Installation électrique"
            );
            assert_eq!(
                state["company"]["activities"][0]["dutch"],
                "Elektrische installatie"
            );
            assert_eq!(state["company"]["regions"][0], "Gent");
            assert_eq!(state["company"]["previous_awarded_cpvs"][0], "45310000");

            assert_eq!(state["tender"]["title"], "Elektrische werken in Gent");
            assert_eq!(state["tender"]["description"], "Plaatsen van installaties");
            assert_eq!(state["tender"]["cpv_codes"][0], "45310000");
            assert_eq!(state["tender"]["regions"][0], "Gent");
            assert_eq!(state["tender"]["value"], "150000");
            assert_eq!(state["tender"]["requirements"][0], "ISO 9001");

            let questions = &body_json["questions"];
            assert_eq!(questions["activity_match"]["type"], "score");
            assert_eq!(
                questions["activity_match"]["criteria"],
                serde_json::json!(["No match", "Partial match", "Strong match"])
            );
            assert_eq!(questions["geography_match"]["type"], "score");
            assert_eq!(
                questions["geography_match"]["criteria"],
                serde_json::json!(["No match", "Possible match", "Strong match"])
            );
            assert_eq!(questions["previous_work_match"]["type"], "score");
            assert_eq!(
                questions["previous_work_match"]["criteria"],
                serde_json::json!(["No similar work", "Somewhat similar", "Very similar"])
            );

            assert_eq!(questions["qualification_risk"]["type"], "choice");
            assert_eq!(
                questions["qualification_risk"]["criteria"]["none_obvious"],
                "No obvious qualification risk in the supplied state"
            );
            assert_eq!(
                questions["qualification_risk"]["criteria"]["certification"],
                "A certification or professional registration may be required"
            );
            assert_eq!(
                questions["qualification_risk"]["criteria"]["capacity"],
                "Turnover, references or delivery capacity may be a concern"
            );
            assert_eq!(
                questions["qualification_risk"]["criteria"]["unclear"],
                "The supplied information is insufficient to assess requirements"
            );

            assert_eq!(questions["relevance"]["type"], "choice");
            assert_eq!(
                questions["relevance"]["criteria"]["poor"],
                "Probably not relevant"
            );
            assert_eq!(
                questions["relevance"]["criteria"]["possible"],
                "Possibly relevant; manual review needed"
            );
            assert_eq!(
                questions["relevance"]["criteria"]["strong"],
                "Strong candidate for manual review"
            );

            let response_body = serde_json::json!({
                "model": "jev-latest",
                "answers": {
                    "activity_match": {
                        "type": "score",
                        "score": 1.75,
                        "legend": { "0": "No match", "1": "Partial match", "2": "Strong match" },
                        "probabilities": { "0": 0.05, "1": 0.15, "2": 0.8 },
                        "confidence": 0.85
                    },
                    "geography_match": {
                        "type": "score",
                        "score": 1.5,
                        "legend": { "0": "No match", "1": "Possible match", "2": "Strong match" },
                        "probabilities": { "0": 0.1, "1": 0.3, "2": 0.6 },
                        "confidence": 0.7
                    },
                    "previous_work_match": {
                        "type": "score",
                        "score": 1.25,
                        "legend": { "0": "No similar work", "1": "Somewhat similar", "2": "Very similar" },
                        "probabilities": { "0": 0.1, "1": 0.55, "2": 0.35 },
                        "confidence": 0.65
                    },
                    "qualification_risk": {
                        "type": "choice",
                        "choice": "none_obvious",
                        "probabilities": {
                            "none_obvious": 0.9,
                            "certification": 0.05,
                            "capacity": 0.03,
                            "unclear": 0.02
                        },
                        "confidence": 0.9
                    },
                    "relevance": {
                        "type": "choice",
                        "choice": "strong",
                        "probabilities": {
                            "poor": 0.05,
                            "possible": 0.15,
                            "strong": 0.8
                        },
                        "confidence": 0.8
                    }
                },
                "usage": { "input_tokens": 150, "output_tokens": 40 }
            });
            let body_str = response_body.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body_str.len(),
                body_str
            )
            .expect("mock response should write");
        });

        let analyzer = TypeSafeAnalyzer::new(
            Some(&format!("http://{address}")),
            "test-typesafe-key",
            Some("jev-latest"),
            Some(30),
        )
        .expect("analyzer should initialize");

        let company = CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![Activity {
                code: "43210".to_owned(),
                classification: "MAIN".to_owned(),
                french: Some("Installation électrique".to_owned()),
                dutch: Some("Elektrische installatie".to_owned()),
            }],
            addresses: vec![Address {
                municipality_nl: Some("Gent".to_owned()),
                ..Address::default()
            }],
            establishments: vec![],
            ..Default::default()
        };
        let tender = Tender {
            source: "bda".to_owned(),
            external_id: "t1".to_owned(),
            title: "Elektrische werken in Gent".to_owned(),
            description: "Plaatsen van installaties".to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            regions: vec!["Gent".to_owned()],
            estimated_value: Some(rust_decimal::Decimal::new(150000, 0)),
            requirements: vec![crate::TenderRequirement {
                text: "ISO 9001".to_owned(),
                kind: "TECHNICAL".to_owned(),
                mandatory: true,
                source_page: None,
            }],
            ..Tender::default()
        };
        let history = vec![AwardRecord {
            publication_number: "a1".to_owned(),
            buyer: Some("Gent".to_owned()),
            title: "Elektriciteit".to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            ..AwardRecord::default()
        }];

        let decision = analyzer
            .analyze(&company, &tender, &history, &CompanyPreferences::default())
            .expect("TypeSafe analysis should succeed");
        server.join().expect("server thread should join");

        assert_eq!(decision.provider, "typesafe:jev-latest");
        assert_eq!(decision.activity_match, Some(1.75));
        assert_eq!(decision.geography_match, Some(1.5));
        assert_eq!(decision.previous_work_match, Some(1.25));
        assert_eq!(decision.qualification_risk.as_deref(), Some("none_obvious"));
        assert_eq!(decision.relevance.as_deref(), Some("strong"));
        assert!(!decision.cached);
        assert_eq!(decision.raw.len(), 5);
        assert_eq!(
            decision.raw["activity_match"]["legend"]["2"],
            "Strong match"
        );
        assert_eq!(decision.raw["qualification_risk"]["choice"], "none_obvious");
    }

    #[test]
    fn parses_all_five_typed_typesafe_answers() {
        let json_body = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "activity_match": {
                    "type": "score",
                    "score": 0.0,
                    "legend": { "0": "No match", "1": "Partial match", "2": "Strong match" },
                    "probabilities": { "0": 1.0, "1": 0.0, "2": 0.0 },
                    "confidence": 0.99
                },
                "geography_match": {
                    "type": "score",
                    "score": 2.0,
                    "legend": { "0": "No match", "1": "Possible match", "2": "Strong match" },
                    "probabilities": { "0": 0.0, "1": 0.0, "2": 1.0 },
                    "confidence": 0.95
                },
                "previous_work_match": {
                    "type": "score",
                    "score": 1.0,
                    "legend": { "0": "No similar work", "1": "Somewhat similar", "2": "Very similar" },
                    "probabilities": { "0": 0.1, "1": 0.8, "2": 0.1 },
                    "confidence": 0.8
                },
                "qualification_risk": {
                    "type": "choice",
                    "choice": "capacity",
                    "probabilities": {
                        "none_obvious": 0.1,
                        "certification": 0.05,
                        "capacity": 0.8,
                        "unclear": 0.05
                    },
                    "confidence": 0.8
                },
                "relevance": {
                    "type": "choice",
                    "choice": "poor",
                    "probabilities": { "poor": 0.9, "possible": 0.1, "strong": 0.0 },
                    "confidence": 0.9
                }
            }
        });
        let decision = parse_typesafe_response(json_body.to_string().as_bytes(), "jev-latest")
            .expect("valid typed answers should parse");
        assert_eq!(decision.provider, "typesafe:jev-latest");
        assert_eq!(decision.activity_match, Some(0.0));
        assert_eq!(decision.geography_match, Some(2.0));
        assert_eq!(decision.previous_work_match, Some(1.0));
        assert_eq!(decision.qualification_risk.as_deref(), Some("capacity"));
        assert_eq!(decision.relevance.as_deref(), Some("poor"));
        assert_eq!(decision.raw["relevance"]["choice"], "poor");
    }

    #[test]
    fn audit_rejects_unrecognized_typesafe_answers() {
        let body = serde_json::json!({"answers": {"unrecognized": "ignored previously"}});
        assert!(parse_typesafe_response(body.to_string().as_bytes(), "test").is_err());
    }

    #[test]
    fn audit_rejects_large_abstaining_answers() {
        let body = serde_json::json!({"answers": {"activity_match": {"type": "score", "score": null, "extra": "x".repeat(70_000)}}});
        assert!(parse_typesafe_response(body.to_string().as_bytes(), "test").is_err());
    }

    #[test]
    fn preserves_missing_and_null_typesafe_answers() {
        let partial = serde_json::json!({
            "answers": {
                "activity_match": { "type": "score", "score": null },
                "relevance": { "type": "choice", "choice": null }
            }
        });
        let decision = parse_typesafe_response(partial.to_string().as_bytes(), "jev-latest")
            .expect("unanswered TypeSafe questions should remain nullable");
        assert_eq!(decision.activity_match, None);
        assert_eq!(decision.geography_match, None);
        assert_eq!(decision.previous_work_match, None);
        assert_eq!(decision.qualification_risk, None);
        assert_eq!(decision.relevance, None);
        assert!(decision.raw.contains_key("activity_match"));
        assert!(!decision.raw.contains_key("geography_match"));
        let serialized = serde_json::to_value(decision).expect("decision should serialize");
        for field in [
            "activity_match",
            "geography_match",
            "previous_work_match",
            "qualification_risk",
            "relevance",
        ] {
            assert!(serialized[field].is_null());
        }
    }

    #[test]
    fn rejects_malformed_typesafe_answers() {
        // Score > 2.0
        let json_high_score = serde_json::json!({
            "answers": {
                "activity_match": { "score": 2.1 },
                "geography_match": { "score": 1.0 },
                "previous_work_match": { "score": 1.0 },
                "qualification_risk": { "choice": "none_obvious" },
                "relevance": { "choice": "strong" }
            }
        });
        assert!(
            parse_typesafe_response(json_high_score.to_string().as_bytes(), "jev-latest").is_err()
        );

        // Score < 0.0
        let json_neg_score = serde_json::json!({
            "answers": {
                "activity_match": { "score": -0.1 },
                "geography_match": { "score": 1.0 },
                "previous_work_match": { "score": 1.0 },
                "qualification_risk": { "choice": "none_obvious" },
                "relevance": { "choice": "strong" }
            }
        });
        assert!(
            parse_typesafe_response(json_neg_score.to_string().as_bytes(), "jev-latest").is_err()
        );

        // Choice empty
        let json_empty_choice = serde_json::json!({
            "answers": {
                "activity_match": { "score": 1.0 },
                "geography_match": { "score": 1.0 },
                "previous_work_match": { "score": 1.0 },
                "qualification_risk": { "choice": "   " },
                "relevance": { "choice": "strong" }
            }
        });
        assert!(
            parse_typesafe_response(json_empty_choice.to_string().as_bytes(), "jev-latest")
                .is_err()
        );

        // Choice unexpected option
        let json_bad_choice = serde_json::json!({
            "answers": {
                "activity_match": { "score": 1.0 },
                "geography_match": { "score": 1.0 },
                "previous_work_match": { "score": 1.0 },
                "qualification_risk": { "choice": "none_obvious" },
                "relevance": { "choice": "invalid_option" }
            }
        });
        assert!(
            parse_typesafe_response(json_bad_choice.to_string().as_bytes(), "jev-latest").is_err()
        );

        // Wrong answer type
        let json_wrong_type = serde_json::json!({
            "answers": {
                "activity_match": { "type": "choice", "score": 1.0 },
                "geography_match": { "type": "score", "score": 1.0 },
                "previous_work_match": { "type": "score", "score": 1.0 },
                "qualification_risk": { "type": "choice", "choice": "none_obvious" },
                "relevance": { "type": "choice", "choice": "strong" }
            }
        });
        assert!(
            parse_typesafe_response(json_wrong_type.to_string().as_bytes(), "jev-latest").is_err()
        );

        // Invalid JSON
        assert!(parse_typesafe_response(b"not json", "jev-latest").is_err());
    }

    #[test]
    fn typesafe_size_limits_and_error_handling() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::thread;

        // Oversized response rejection
        let oversized = vec![b'{'; MAX_AI_RESPONSE_BYTES + 1];
        assert!(parse_typesafe_response(&oversized, "jev-latest").is_err());

        // HTTP error status response redacts API key
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock request should arrive");
            read_full_request(&mut stream);
            let body = r#"{"error": "Unauthorized: invalid api key super-secret-key-12345"}"#;
            write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("mock response should write");
            // Closing with unread data queued makes the kernel send a reset
            // instead of the response, which the client surfaces as a network
            // error rather than the status this test asserts.
            finish_response(stream);
        });

        let analyzer = TypeSafeAnalyzer::new(
            Some(&format!("http://{address}")),
            "super-secret-key-12345",
            Some("jev-latest"),
            Some(5),
        )
        .expect("analyzer should initialize");

        let err = analyzer
            .analyze(
                &CompanyProfile {
                    enterprise_number: "0123.456.789".to_owned(),
                    status: None,
                    juridical_situation: None,
                    juridical_form: None,
                    start_date: None,
                    names: vec![],
                    activities: vec![],
                    addresses: vec![],
                    establishments: vec![],
                    ..Default::default()
                },
                &Tender::default(),
                &[],
                &CompanyPreferences::default(),
            )
            .expect_err("401 response must return error");
        server.join().expect("server thread should join");

        let err_msg = err.to_string();
        assert!(
            err_msg.contains("401"),
            "unexpected TypeSafe HTTP error: {err_msg}"
        );
        assert!(!err_msg.contains("super-secret-key-12345"));
        assert_eq!(
            sanitize_error_message(
                "invalid key super-secret-key-12345",
                "super-secret-key-12345"
            ),
            "invalid key [REDACTED]"
        );
    }

    #[test]
    fn typesafe_config_defaults_and_redaction() {
        let analyzer =
            TypeSafeAnalyzer::new(None, "my-key", None, None).expect("defaults should be accepted");
        assert_eq!(analyzer.base_url(), "https://api.typesafe.ai");
        assert_eq!(analyzer.model(), "jev-latest");
        assert_eq!(analyzer.timeout_seconds(), 60);

        // Bounded timeout
        let clamped = TypeSafeAnalyzer::new(None, "my-key", None, Some(999))
            .expect("clamped timeout should be accepted");
        assert_eq!(clamped.timeout_seconds(), 60);
        let valid = TypeSafeAnalyzer::new(None, "my-key", None, Some(120))
            .expect("valid timeout should be accepted");
        assert_eq!(valid.timeout_seconds(), 120);

        // Insecure endpoint rejected
        assert!(
            TypeSafeAnalyzer::new(Some("http://api.typesafe.ai"), "my-key", None, None).is_err()
        );
        assert!(
            TypeSafeAnalyzer::new(Some("ftp://api.typesafe.ai"), "my-key", None, None).is_err()
        );
        // Loopback http allowed
        assert!(TypeSafeAnalyzer::new(Some("http://127.0.0.1:8080"), "my-key", None, None).is_ok());

        // Debug output redacts the API key; invalid credential-bearing URLs are rejected.
        let analyzer = TypeSafeAnalyzer::new(
            Some("https://api.typesafe.ai/custom"),
            "super-secret-key",
            Some("jev-latest"),
            Some(45),
        )
        .expect("analyzer should parse");
        let debug_str = format!("{analyzer:?}");
        assert!(debug_str.contains("[REDACTED]"));
        assert!(!debug_str.contains("super-secret-key"));
        for unsafe_url in [
            "https://user:password@api.typesafe.ai",
            "https://api.typesafe.ai?secret=1",
            "https://api.typesafe.ai#frag",
        ] {
            let error = TypeSafeAnalyzer::new(Some(unsafe_url), "key", None, None)
                .expect_err("URLs with secrets or fragments must be rejected");
            assert!(!error.to_string().contains("password"));
            assert!(!error.to_string().contains("secret=1"));
            assert!(!error.to_string().contains("#frag"));
        }
        let error = sanitize_typesafe_error_message(
            "request failed: https://user:password@api.typesafe.ai:443/custom?secret=1#frag with super-secret-key",
            "super-secret-key",
            "https://user:password@api.typesafe.ai:443/custom?secret=1#frag",
            "https://user:password@api.typesafe.ai:443/custom?secret=1#frag/v1/systemone",
        );
        assert!(!error.contains("password"));
        assert!(!error.contains("secret=1"));
        assert!(!error.contains("super-secret-key"));
    }

    #[test]
    fn typesafe_cache_identity_and_separation() {
        let first = TypeSafeAnalyzer::new(
            Some("https://api.typesafe.ai"),
            "key1",
            Some("jev-latest"),
            None,
        )
        .unwrap();
        let second = TypeSafeAnalyzer::new(
            Some("https://api.typesafe.ai/"),
            "key2",
            Some("jev-latest"),
            None,
        )
        .unwrap();
        assert_eq!(first.cache_identity(), second.cache_identity());
        assert!(first.cache_identity().starts_with("typesafe:jev-latest:"));
        assert!(!first.cache_identity().contains("api.typesafe.ai"));
        // Prompt edits must invalidate the cache: the questions version is
        // part of the identity, so a bump stops stale decisions matching.
        assert!(
            first.cache_identity().ends_with(TYPESAFE_QUESTIONS_VERSION),
            "identity must embed the questions version"
        );
        assert_eq!(first.prompt_version(), TYPESAFE_QUESTIONS_VERSION);

        // Model difference separates cache identity
        let model_diff = TypeSafeAnalyzer::new(
            Some("https://api.typesafe.ai"),
            "key1",
            Some("jev-1.13.0"),
            None,
        )
        .unwrap();
        assert_ne!(first.cache_identity(), model_diff.cache_identity());

        // Base URL difference separates cache identity
        let url_diff = TypeSafeAnalyzer::new(
            Some("http://127.0.0.1:8080"),
            "key1",
            Some("jev-latest"),
            None,
        )
        .unwrap();
        assert_ne!(first.cache_identity(), url_diff.cache_identity());

        // API credentials are never exposed or included in cache identity.
        assert!(!first.cache_identity().contains("key1"));
        assert!(!second.cache_identity().contains("key2"));

        // analysis_cache_key separation
        let company = CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            primary_cpv_prefixes: vec!["72000000".to_owned()],
            ..Default::default()
        };
        let tender = Tender::default();
        let history = Vec::new();
        let preferences = CompanyPreferences::default();

        let typesafe_key = analysis_cache_key(
            &first.cache_identity(),
            &company,
            &tender,
            &history,
            &preferences,
        );
        let openai_key = analysis_cache_key(
            "openai-compatible:https://api.openai.com/v1:gpt-4o-mini",
            &company,
            &tender,
            &history,
            &preferences,
        );
        let deterministic_key = analysis_cache_key(
            DETERMINISTIC_CACHE_IDENTITY,
            &company,
            &tender,
            &history,
            &preferences,
        );

        assert_ne!(typesafe_key, openai_key);
        assert_ne!(typesafe_key, deterministic_key);
    }

    #[test]
    fn configured_analyzer_precedence_and_error_propagation() {
        let typesafe = TypeSafeAnalyzer::new(None, "ts-key", Some("jev-latest"), None).unwrap();
        let openai = RemoteAnalyzer {
            endpoint: "http://127.0.0.1:8080".to_owned(),
            api_key: "oa-key".to_owned(),
            model: "gpt-4o-mini".to_owned(),
            timeout_seconds: 60,
            response_format: ResponseFormat::JsonSchema,
        };

        let configured_ts =
            ConfiguredAnalyzer::select(Some(typesafe.clone()), Some(openai.clone()))
                .expect("TypeSafe should take precedence when both providers are configured");
        assert_eq!(configured_ts.provider_name(), "typesafe");
        assert_eq!(configured_ts.model(), "jev-latest");
        assert!(configured_ts
            .cache_identity()
            .starts_with("typesafe:jev-latest:"));

        let configured_oa = ConfiguredAnalyzer::select(None, Some(openai))
            .expect("OpenAI-compatible provider should be selected when TypeSafe is absent");
        assert_eq!(configured_oa.provider_name(), "openai-compatible");
        assert_eq!(configured_oa.model(), "gpt-4o-mini");
        assert_eq!(
            configured_oa.cache_identity(),
            format!("openai-compatible:http://127.0.0.1:8080:gpt-4o-mini:{OPENAI_PROMPT_VERSION}")
        );
        assert_ne!(
            configured_oa.cache_identity(),
            "openai-compatible:http://127.0.0.1:8080:gpt-4o-mini:openai-schema:v1"
        );
        assert_eq!(configured_oa.prompt_version(), OPENAI_PROMPT_VERSION);
        assert_eq!(configured_ts.prompt_version(), TYPESAFE_QUESTIONS_VERSION);
    }

    #[test]
    fn decisions_carry_the_prompt_version_that_produced_them() {
        let company = CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            primary_cpv_prefixes: vec!["72000000".to_owned()],
            ..Default::default()
        };
        let decision = DeterministicAnalyzer
            .analyze(
                &company,
                &Tender::default(),
                &[],
                &CompanyPreferences::default(),
            )
            .expect("deterministic analysis cannot fail");
        assert_eq!(
            decision.prompt_version.as_deref(),
            Some(DETERMINISTIC_CACHE_IDENTITY)
        );
        // Rows cached before prompt versions existed still deserialize, with
        // no version — and they can never match a current key, because every
        // current identity embeds its prompt version.
        let old_row = serde_json::json!({
            "provider": "deterministic-rust",
            "activity_match": 1.0,
            "geography_match": 0.5,
            "previous_work_match": 0.0,
            "qualification_risk": "none_obvious",
            "relevance": "possible",
            "raw": {},
        });
        let old: AnalysisDecision =
            serde_json::from_value(old_row).expect("old cache rows must deserialize");
        assert_eq!(old.prompt_version, None);
    }
    /// An "anywhere in the EEA" notice is neutral geography in the analyzer too:
    /// it used to be reported as a mismatch because its region list was
    /// non-empty, contradicting the matcher's own signal.
    #[test]
    fn analyzer_reports_anywhere_regions_as_neutral_geography() {
        let company = CompanyProfile {
            enterprise_number: "0404.484.060".to_owned(),
            names: vec!["Test BV".to_owned()],
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            primary_cpv_prefixes: vec!["72000000".to_owned()],
            ..Default::default()
        };
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "anywhere".to_owned(),
            title: "Software services".to_owned(),
            regions: vec!["anyw-eea".to_owned()],
            ..Tender::default()
        };
        let decision = DeterministicAnalyzer
            .analyze(&company, &tender, &[], &CompanyPreferences::default())
            .expect("deterministic analysis should succeed");
        assert_eq!(decision.geography_match, Some(0.5));
    }

    const VALID_REMOTE_DECISION: &str = r#"{"activity_match":0.8,"geography_match":0.5,"previous_work_match":0.2,"qualification_risk":"capacity","relevance":"possible"}"#;

    /// Answer the given responses in order, recording every request body.
    fn serve_chat(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("the fixture should bind");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("an address")
        );
        let handle = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for (status, body) in responses {
                let (stream, _) = listener.accept().expect("a connection");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .expect("a read timeout");
                let mut reader = BufReader::new(stream);
                let mut length = 0_usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).expect("a header") == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("Content-Length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body_bytes = vec![0; length];
                let _ = reader.read_exact(&mut body_bytes);
                bodies.push(String::from_utf8_lossy(&body_bytes).into_owned());
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status} \r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("a response");
            }
            bodies
        });
        (endpoint, handle)
    }

    fn chat_completion(content: &str) -> String {
        serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string()
    }

    fn remote_analyzer(endpoint: String) -> RemoteAnalyzer {
        RemoteAnalyzer {
            endpoint,
            api_key: "test-key".to_owned(),
            model: "test-model".to_owned(),
            timeout_seconds: 10,
            response_format: ResponseFormat::JsonSchema,
        }
    }

    fn fixture_company() -> CompanyProfile {
        CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn decision_schema_matches_what_the_parser_accepts() {
        let schema = decision_schema();
        let properties = &schema["properties"];
        let listed = |field: &str| -> Vec<String> {
            properties[field]["enum"]
                .as_array()
                .expect("an enum")
                .iter()
                .map(|value| value.as_str().expect("a string").to_owned())
                .collect()
        };
        assert_eq!(listed("qualification_risk"), QUALIFICATION_RISK_OPTIONS);
        assert_eq!(listed("relevance"), RELEVANCE_OPTIONS);
        let required: Vec<String> = schema["required"]
            .as_array()
            .expect("required fields")
            .iter()
            .map(|value| value.as_str().expect("a string").to_owned())
            .collect();
        for field in [
            "activity_match",
            "geography_match",
            "previous_work_match",
            "qualification_risk",
            "relevance",
        ] {
            assert!(
                required.contains(&field.to_owned()),
                "{field} must be required"
            );
        }
        // `raw` is a map of criterion -> {score, confidence}, which is what
        // `parse_remote_decision` deserializes; a bare object is not enough.
        assert_eq!(
            properties["raw"]["additionalProperties"]["required"],
            serde_json::json!(["score", "confidence"])
        );
        assert_eq!(
            properties["raw"]["additionalProperties"]["additionalProperties"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn payload_requests_the_schema_unless_told_otherwise() {
        let schema = remote_payload("m", "{}", ResponseFormat::JsonSchema);
        assert_eq!(schema["response_format"]["type"], "json_schema");
        assert_eq!(
            schema["response_format"]["json_schema"]["schema"],
            decision_schema()
        );
        let legacy = remote_payload("m", "{}", ResponseFormat::JsonObject);
        assert_eq!(
            legacy["response_format"],
            serde_json::json!({"type": "json_object"})
        );
    }

    #[test]
    fn response_format_configuration_is_explicit() {
        assert_eq!(
            ResponseFormat::from_config(None).expect("default"),
            ResponseFormat::JsonSchema
        );
        assert_eq!(
            ResponseFormat::from_config(Some("json_object")).expect("opt out"),
            ResponseFormat::JsonObject
        );
        assert!(ResponseFormat::from_config(Some("yaml")).is_err());
    }

    #[test]
    fn a_provider_that_rejects_the_schema_is_retried_with_json_object() {
        let (endpoint, handle) = serve_chat(vec![
            (400, "{\"error\":\"unknown response_format\"}".to_owned()),
            (200, chat_completion(VALID_REMOTE_DECISION)),
        ]);
        let decision = remote_analyzer(endpoint)
            .analyze(
                &fixture_company(),
                &Tender::default(),
                &[],
                &CompanyPreferences::default(),
            )
            .expect("the retry should succeed");
        assert_eq!(decision.relevance.as_deref(), Some("possible"));
        let bodies = handle.join().expect("the server thread");
        assert_eq!(bodies.len(), 2, "one rejected attempt and one retry");
        assert!(
            bodies[0].contains("\"json_schema\""),
            "the first attempt asks for a schema"
        );
        assert!(
            bodies[1].contains("\"json_object\""),
            "the retry falls back"
        );
    }

    #[test]
    fn a_rejection_is_not_retried_when_json_object_was_requested() {
        let (endpoint, handle) = serve_chat(vec![(400, "{}".to_owned())]);
        let mut analyzer = remote_analyzer(endpoint);
        analyzer.response_format = ResponseFormat::JsonObject;
        let error = analyzer
            .analyze(
                &fixture_company(),
                &Tender::default(),
                &[],
                &CompanyPreferences::default(),
            )
            .expect_err("a rejected request must surface");
        assert!(error.to_string().contains("HTTP 400"));
        assert_eq!(handle.join().expect("the server thread").len(), 1);
    }
}
