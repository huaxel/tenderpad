use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "network")]
use std::env;
use std::error::Error;
use std::fmt;
#[cfg(feature = "network")]
use std::io::Read;
#[cfg(feature = "network")]
use std::time::{Duration, Instant};
#[cfg(feature = "network")]
use ureq::{Agent, AgentBuilder, Response};
#[cfg(feature = "network")]
use uuid::Uuid;

pub mod archive;
pub mod citation;
pub mod classify;
pub mod documents;
pub mod evidence;
pub mod ooxml_text;
pub mod pdf_text;

#[cfg(feature = "network")]
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Int,
    Prod,
}

impl Environment {
    pub fn parse(value: &str) -> Result<Self, BosaError> {
        match value {
            "int" => Ok(Self::Int),
            "prod" => Ok(Self::Prod),
            _ => Err(BosaError::Configuration(
                "environment must be int or prod".into(),
            )),
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            Self::Int => "bda-int",
            Self::Prod => "bda",
        }
    }
}

#[derive(Clone)]
#[cfg(feature = "network")]
pub struct BosaConfig {
    pub token_url: String,
    /// FSP origin, not the platform UI host or a service-specific base path.
    pub api_base_url: String,
    pub client_id: String,
    pub client_secret: String,
}

#[cfg(feature = "network")]
impl fmt::Debug for BosaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BosaConfig")
            .field("token_url", &self.token_url)
            .field("api_base_url", &self.api_base_url)
            .field("client_id", &"[REDACTED]")
            .field("client_secret", &"[REDACTED]")
            .finish()
    }
}

#[cfg(feature = "network")]
impl BosaConfig {
    pub fn from_env() -> Result<Self, BosaError> {
        Self::for_environment(Environment::Int)
    }

    pub fn for_environment(environment: Environment) -> Result<Self, BosaError> {
        let (prefix, host) = match environment {
            Environment::Int => ("BOSA_INT", "https://public.int.fedservices.be"),
            Environment::Prod => ("BOSA_PR", "https://public.pr.fedservices.be"),
        };
        let required = |suffix: &str| -> Result<String, BosaError> {
            let name = format!("{prefix}_{suffix}");
            env::var(&name)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| BosaError::Configuration(format!("{name} is not set")))
        };
        let config = Self {
            token_url: env::var(format!("{prefix}_TOKEN_URL"))
                .unwrap_or_else(|_| format!("{host}/api/oauth2/token")),
            api_base_url: env::var(format!("{prefix}_API_BASE_URL"))
                .unwrap_or_else(|_| host.to_owned()),
            client_id: required("CLIENT_ID")?,
            client_secret: required("CLIENT_SECRET")?,
        };
        // Fail closed on stale internal-host overrides and cross-environment credentials.
        if config.token_url != format!("{host}/api/oauth2/token")
            || config.api_base_url.trim_end_matches('/') != host
        {
            return Err(BosaError::Configuration(format!(
                "use the selected environment's FSP origin {host}; remove stale URL overrides"
            )));
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationSearch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_date_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_date_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice_subtypes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpv_codes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nuts_codes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_active_submission_deadline: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
}

impl PublicationSearch {
    #[cfg(feature = "network")]
    fn query(&self) -> Result<Vec<(String, String)>, BosaError> {
        if self.page == Some(0) || self.page_size.is_some_and(|n| !(1..=100).contains(&n)) {
            return Err(BosaError::Configuration(
                "page must be >= 1 and pageSize 1..100".into(),
            ));
        }
        let value =
            serde_json::to_value(self).map_err(|e| BosaError::Configuration(e.to_string()))?;
        let mut query = Vec::new();
        for (key, value) in value.as_object().expect("search is an object") {
            let text = match value {
                Value::String(s) => s.clone(),
                Value::Array(items) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
                other => other.to_string(),
            };
            query.push((key.clone(), text));
        }
        Ok(query)
    }
}

#[derive(Debug)]
pub enum BosaError {
    Configuration(String),
    Transport(String),
    CallGuard(String),
    Http { status: u16, detail: String },
    InvalidResponse(String),
}

impl fmt::Display for BosaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => write!(f, "BOSA configuration: {message}"),
            Self::Transport(message) => write!(f, "BOSA request failed: {message}"),
            Self::CallGuard(message) => write!(f, "{message}"),
            Self::Http { status, detail } => write!(f, "BOSA returned HTTP {status}: {detail}"),
            Self::InvalidResponse(message) => write!(f, "invalid BOSA response: {message}"),
        }
    }
}
impl Error for BosaError {}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BosaAwardLotEvidence {
    pub lot_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lot_id_kind: Option<String>,
    /// Populated only for `selec-w` when contract links identify one organization.
    pub winner_identifier: Option<String>,
    pub winner_name: Option<String>,
    pub winner_selection_status: Option<String>,
    /// Tenderers referenced by the lot result; this can include non-winners.
    pub linked_tenderers: Vec<crate::award_xml::LinkedTendererEvidence>,
    /// Subset whose tenders are linked to contracts cited by this lot result.
    pub contract_linked_tenderers: Vec<crate::award_xml::LinkedTendererEvidence>,
    pub settled_contract_references: Vec<String>,
    pub reported_value: Option<rust_decimal::Decimal>,
    pub lowest_tender_value: Option<rust_decimal::Decimal>,
    pub highest_tender_value: Option<rust_decimal::Decimal>,
    pub cpv_codes: Vec<String>,
}

/// Extract awardee and value evidence per lot without inferring lifecycle status.
pub fn parse_award_lot_evidence(xml: &[u8]) -> Result<Vec<BosaAwardLotEvidence>, BosaError> {
    crate::award_xml::parse_award_xml_with_tenderers(xml, "")
        .map(|lots| {
            lots.into_iter()
                .map(|evidence| {
                    let selected = crate::award_xml::WinnerSelectionState::from_source_code(
                        evidence.winner_selection_status.as_deref(),
                    )
                    .has_selected_winner();
                    BosaAwardLotEvidence {
                        lot_id: evidence.lot.lot_id,
                        lot_id_kind: evidence.lot_id_kind,
                        winner_identifier: selected
                            .then_some(evidence.lot.winner_identifier)
                            .flatten(),
                        winner_name: selected.then_some(evidence.lot.winner_name).flatten(),
                        winner_selection_status: evidence.winner_selection_status,
                        linked_tenderers: evidence.linked_tenderers,
                        contract_linked_tenderers: evidence.contract_linked_tenderers,
                        settled_contract_references: evidence.settled_contract_references,
                        reported_value: evidence.lot.value,
                        lowest_tender_value: evidence.lot.lowest_value,
                        highest_tender_value: evidence.lot.highest_value,
                        cpv_codes: evidence.lot.cpv_codes,
                    }
                })
                .collect()
        })
        .map_err(|_| BosaError::InvalidResponse("award lot evidence could not be parsed".into()))
}

#[cfg(feature = "network")]
pub struct BosaClient {
    config: BosaConfig,
    agent: Agent,
    token: Option<(String, Instant)>,
}

#[cfg(feature = "network")]
impl BosaClient {
    pub fn new(config: BosaConfig) -> Self {
        let agent = AgentBuilder::new()
            // Disable pooled-connection resends: each HTTP send needs its own ledger reservation.
            .max_idle_connections(0)
            .user_agent(concat!(
                "ehb-fin-aanbestedingen/",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .timeout_write(Duration::from_secs(30))
            // Never forward credentials to a redirect target.
            .redirects(0)
            .build();
        Self {
            config,
            agent,
            token: None,
        }
    }

    fn audit_service(&self) -> crate::service_calls::Service {
        if self.config.api_base_url.trim_end_matches('/') == "https://public.pr.fedservices.be" {
            crate::service_calls::Service::BosaProd
        } else {
            crate::service_calls::Service::BosaInt
        }
    }

    fn access_token(&mut self) -> Result<String, BosaError> {
        if let Some((token, expires)) = &self.token {
            if Instant::now() < *expires {
                return Ok(token.clone());
            }
        }
        // The token exchange is the first call of every BOSA sequence, so a
        // transient failure here used to abort a whole dossier import before a
        // single publication was read. It is retried under its own policy: still
        // throttling/`5xx`/transport only, because a rejected credential (`400`,
        // `401`) is not transient and must surface as the configuration error it
        // is.
        let outcome = crate::http_retry::send_with_retry(
            &crate::http_retry::RetryPolicy::bosa(),
            || {
                crate::service_calls::http(
                    self.audit_service(),
                    crate::service_calls::Operation::BosaOauth,
                    || {
                        self.agent
                            .post(&self.config.token_url)
                            .set("Accept", "application/json")
                            .send_form(&[
                                ("grant_type", "client_credentials"),
                                ("client_id", self.config.client_id.as_str()),
                                ("client_secret", self.config.client_secret.as_str()),
                            ])
                    },
                )
                .map_err(|e| crate::http_retry::Attempt::Guard(e.to_string()))
                .and_then(crate::http_retry::Attempt::classify)
            },
            crate::http_retry::Attempt::signal,
        );
        let response = match outcome {
            Ok(response) => response,
            Err(stop) => return Err(bosa_attempt_error(stop.into_error())),
        };
        let token: TokenResponse = serde_json::from_slice(&read_body(response)?)
            .map_err(|_| BosaError::InvalidResponse("malformed token JSON".into()))?;
        if token.access_token.trim().is_empty() {
            return Err(BosaError::InvalidResponse("empty access_token".into()));
        }
        // Bound untrusted expiry before adding it to Instant.
        let ttl = token.expires_in.unwrap_or(60).min(86400).saturating_sub(30);
        self.token = Some((
            token.access_token.clone(),
            Instant::now() + Duration::from_secs(ttl),
        ));
        Ok(token.access_token)
    }

    fn get(&mut self, path: &str, query: &[(String, String)]) -> Result<Value, BosaError> {
        let url = format!("{}{path}", self.config.api_base_url.trim_end_matches('/'));
        for attempt in 0..2 {
            let token = self.access_token()?;
            let result = self.api_get(&url, query, &token);
            // A stale token is refreshed once. It is not a retry: the endpoint
            // answered, and the answer says the credential has to be renewed.
            if matches!(&result, Err(BosaError::Http { status: 401, .. })) && attempt == 0 {
                self.token = None;
                continue;
            }
            let response = result?;
            return serde_json::from_slice(&read_body(response)?)
                .map_err(|_| BosaError::InvalidResponse("malformed API JSON".into()));
        }
        unreachable!("second attempt returns")
    }

    /// One logical BOSA read, retried under the shared policy. Each retry is a
    /// separate ledger attempt, and the request is rebuilt per attempt because
    /// `ureq::Request::call` consumes it.
    fn api_get(
        &self,
        url: &str,
        query: &[(String, String)],
        token: &str,
    ) -> Result<Response, BosaError> {
        let service = self.audit_service();
        let outcome = crate::http_retry::send_with_retry(
            &crate::http_retry::RetryPolicy::bosa(),
            || {
                let mut request = self
                    .agent
                    .get(url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Accept", "application/json")
                    .set("BelGov-Trace-Id", &Uuid::new_v4().to_string());
                for (key, value) in query {
                    request = request.query(key, value);
                }
                crate::service_calls::http(
                    service,
                    crate::service_calls::Operation::BosaApi,
                    || request.call(),
                )
                .map_err(|e| crate::http_retry::Attempt::Guard(e.to_string()))
                .and_then(crate::http_retry::Attempt::classify)
            },
            crate::http_retry::Attempt::signal,
        );
        match outcome {
            Ok(response) => Ok(response),
            Err(stop) => Err(bosa_attempt_error(stop.into_error())),
        }
    }

    pub fn search_publications(&mut self, search: &PublicationSearch) -> Result<Value, BosaError> {
        let value = self.get(
            "/api/eProcurementSea/v1/search/publications",
            &search.query()?,
        )?;
        if !value.get("publications").is_some_and(Value::is_array)
            || !value.get("totalCount").is_some_and(Value::is_u64)
        {
            return Err(BosaError::InvalidResponse(
                "missing publications/totalCount".into(),
            ));
        }
        Ok(value)
    }

    pub fn publication_workspace(&mut self, id: &str) -> Result<Value, BosaError> {
        let id = Uuid::parse_str(id).map_err(|_| {
            BosaError::Configuration("publication workspace ID must be a UUID".into())
        })?;
        // Media consumers must explicitly exclude drafts. Do not send
        // enterpriseId; that parameter is ignored for Media consumers.
        self.get(
            &format!("/api/eProcurementDos/v1/publication-workspaces/{id}"),
            &[("includeDrafts".into(), "false".into())],
        )
    }

    /// Recover a missing legacy award XML only when explicitly requested by the
    /// importer. The document listing is scoped to the exact workspace; its XML
    /// must match the selected published notice's stable reference before use.
    pub fn publication_workspace_with_xml_fallback(
        &mut self,
        publication: &Value,
        environment: Environment,
    ) -> Result<Value, BosaError> {
        let invalid = |message: &str| BosaError::InvalidResponse(message.into());
        let workspace_id = publication
            .get("publicationWorkspaceId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("publication has no workspace ID"))?;
        let mut workspace = self.publication_workspace(workspace_id)?;
        if workspace.get("id").and_then(Value::as_str) != Some(workspace_id) {
            return Err(invalid(
                "workspace response ID does not match search result",
            ));
        }
        let version_index = selected_published_notice_version_index(publication, &workspace)?;
        let version = workspace["versions"][version_index].clone();
        if version
            .pointer("/notice/xmlContent")
            .and_then(Value::as_str)
            .is_some_and(|xml| !xml.trim().is_empty())
        {
            return Ok(workspace);
        }
        if !workspace
            .get("noticeType")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("can"))
        {
            return Ok(workspace);
        }
        if workspace.get("formType").and_then(Value::as_str) != Some("AWARD") {
            return Err(invalid("XML fallback is restricted to award workspaces"));
        }

        let attachments = self.attachments(workspace_id)?;
        let languages = publication["publicationLanguages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let selected_attachment = select_xml_attachment(&attachments, &languages)?;
        let downloaded =
            self.download_xml_attachment_bytes(selected_attachment, environment, true)?;
        let mut expected_references = Vec::new();
        for value in [
            workspace.get("referenceNumber"),
            workspace.get("dossierReferenceNumber"),
            publication.get("referenceNumber"),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        {
            expected_references.push(value);
        }
        for value in publication["publicationReferenceNumbersTED"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            expected_references.push(value);
        }
        crate::award_xml::validate_legacy_f03_award_xml(&downloaded.bytes, &expected_references)
            .map_err(|_| invalid("XML attachment failed F03 publication/reference validation"))?;
        let xml = std::str::from_utf8(&downloaded.bytes)
            .map_err(|_| invalid("XML attachment is not UTF-8"))?;
        workspace["versions"][version_index]["notice"]["xmlContent"] = xml.into();
        workspace["bosa_xml_attachment_fallback"] = serde_json::json!({
            "source": "workspace-documents",
            "workspace_id": workspace_id,
            "selected_notice_id": version["notice"]["id"],
            "document_version_id": downloaded.version_id,
            "original_name": downloaded.original_name,
            "bytes": downloaded.bytes.len(),
            "sha256": downloaded.sha256,
            "sha256_verified": downloaded.sha256_verified,
            "validation": "F03 form and stable publication reference matched",
        });
        Ok(workspace)
    }

    /// List published workspaces belonging to one BOSA dossier, without drafts.
    pub fn publication_workspaces_for_dossier(
        &mut self,
        dossier_id: u64,
    ) -> Result<Vec<Value>, BosaError> {
        if dossier_id == 0 {
            return Err(BosaError::Configuration(
                "dossier ID must be positive".into(),
            ));
        }
        let value = self.get(
            &format!("/api/eProcurementDos/v1/dossiers/{dossier_id}/publication-workspaces"),
            &[("includeDrafts".into(), "false".into())],
        )?;
        value.as_array().cloned().ok_or_else(|| {
            BosaError::InvalidResponse("dossier workspaces response is not an array".into())
        })
    }
}

/// Parse the latest published, search-listed notice through the existing BDA adapter.
/// Search metadata supplies current publication/deadline information after corrections.
#[cfg(feature = "network")]
fn select_xml_attachment<'a>(
    attachments: &'a [documents::Attachment],
    languages: &[String],
) -> Result<&'a documents::Attachment, BosaError> {
    let invalid = |message: &str| BosaError::InvalidResponse(message.into());
    let xml = attachments
        .iter()
        .filter(|attachment| {
            matches!(
                attachment.checksum_status,
                documents::ChecksumStatus::Verified | documents::ChecksumStatus::Unknown
            ) && std::path::Path::new(&attachment.original_name)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
        })
        .collect::<Vec<_>>();
    if xml.is_empty() {
        return Err(invalid("workspace has no XML attachment"));
    }
    if languages.is_empty() {
        return match xml.as_slice() {
            [only] => Ok(*only),
            _ => Err(invalid(
                "ambiguous XML attachments; refusing arbitrary language",
            )),
        };
    }
    let preferred = xml
        .into_iter()
        .filter(|attachment| {
            let language_tokens = attachment
                .original_name
                .split(|character: char| !character.is_ascii_alphabetic())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>();
            languages.iter().any(|language| {
                language.split(['-', '_']).next().is_some_and(|primary| {
                    let primary = primary.to_ascii_lowercase();
                    primary.len() == 2 && language_tokens.iter().any(|token| token == &primary)
                })
            })
        })
        .collect::<Vec<_>>();
    match preferred.as_slice() {
        [only] => Ok(*only),
        _ => Err(invalid(
            "XML attachments do not uniquely match publication language",
        )),
    }
}

fn selected_published_notice_version_index(
    publication: &Value,
    workspace: &Value,
) -> Result<usize, BosaError> {
    let invalid = |message: &str| BosaError::InvalidResponse(message.into());
    let notice_ids = publication
        .get("noticeIds")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("publication has no notice IDs"))?;
    let versions = workspace
        .get("versions")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("dossier has no versions"))?;
    versions
        .iter()
        .enumerate()
        .filter(|(_, version)| {
            version.get("status").and_then(Value::as_str) == Some("PUBLISHED")
                && version
                    .pointer("/notice/id")
                    .is_some_and(|id| notice_ids.contains(id))
        })
        .max_by_key(|(_, version)| {
            (
                version
                    .get("activationDate")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                version
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                version
                    .pointer("/notice/noticeVersion")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        })
        .map(|(index, _)| index)
        .ok_or_else(|| invalid("no published search-listed notice version"))
}

pub fn publication_to_tender(
    publication: &Value,
    workspace: &Value,
    environment: Environment,
) -> Result<crate::Tender, BosaError> {
    let invalid = |message: &str| BosaError::InvalidResponse(message.into());
    let id = publication
        .get("publicationWorkspaceId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("publication has no workspace ID"))?;
    if workspace.get("id").and_then(Value::as_str) != Some(id) {
        return Err(invalid("dossier ID does not match search result"));
    }
    let version_index = selected_published_notice_version_index(publication, workspace)?;
    let versions = workspace
        .get("versions")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("dossier has no versions"))?;
    let version = &versions[version_index];
    let xml = version
        .pointer("/notice/xmlContent")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid("published notice has no XML"))?;
    let mut tender = crate::bda::parse_xml(xml.as_bytes(), &format!("{id}.xml"))
        .map_err(|_| invalid("notice XML could not be parsed"))?;
    tender.source = environment.source().into();
    let texts = |value: &Value| -> Option<String> {
        let mut texts = Vec::new();
        for text in value
            .as_array()?
            .iter()
            .filter_map(|v| v.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !texts.contains(&text) {
                texts.push(text);
            }
        }
        (!texts.is_empty()).then(|| texts.join(" / "))
    };
    if let Some(title) = texts(&publication["dossier"]["titles"]) {
        tender.title = title;
    }
    if let Some(description) = texts(&publication["dossier"]["descriptions"]) {
        tender.description = description;
    }
    if let Some(buyer) = texts(&publication["organisation"]["organisationNames"]) {
        tender.buyer = Some(buyer);
    }
    let date = |value: &Value| -> Option<String> {
        let text = value.as_str()?.get(..10)?;
        chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
        Some(text.into())
    };
    if let Some(publication_date) = date(&publication["publicationDate"]) {
        tender.publication_date = Some(publication_date);
    }
    if let Some(deadline) = date(&publication["vaultSubmissionDeadline"]) {
        tender.deadline = Some(deadline);
    }
    // Real eForms XML contains many unrelated Name and EndDate nodes. The
    // search snapshot provides the current deadline/title, avoiding stale dates.
    for code in std::iter::once(&publication["cpvMainCode"]).chain(
        publication["cpvAdditionalCodes"]
            .as_array()
            .into_iter()
            .flatten(),
    ) {
        if let Some(code) = code.get("code").and_then(Value::as_str) {
            let code = code.split('-').next().unwrap_or(code).to_owned();
            if !code.is_empty() && !tender.cpv_codes.contains(&code) {
                tender.cpv_codes.push(code);
            }
        }
    }
    tender.notice_type = workspace
        .get("noticeType")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // Unknown API types must not turn an awarded contract into an opportunity.
    if tender.notice_type.is_none() {
        return Err(invalid("dossier has no notice type"));
    }
    let host = match environment {
        Environment::Int => "https://int.publicprocurement.be",
        Environment::Prod => "https://www.publicprocurement.be",
    };
    tender.document_urls = vec![format!("{host}/publication-workspaces/{id}")];
    tender.raw["bosa_publication_workspace_id"] = id.into();
    tender.raw["bosa_notice_id"] = version["notice"]["id"].clone();
    if !workspace["bosa_xml_attachment_fallback"].is_null() {
        tender.raw["bosa_xml_attachment_fallback"] =
            workspace["bosa_xml_attachment_fallback"].clone();
    }
    tender.raw["bosa_activation_date"] = version["activationDate"].clone();
    tender.raw["bosa_version_created_at"] = version["createdAt"].clone();
    tender.raw["bosa_publication_type"] = publication["publicationType"].clone();
    for (key, value) in [
        ("bosa_dossier_id", &workspace["dossierId"]),
        (
            "bosa_dossier_reference_number",
            &workspace["dossierReferenceNumber"],
        ),
        (
            "bosa_workspace_reference_number",
            &workspace["referenceNumber"],
        ),
        ("bosa_workspace_version", &workspace["version"]),
        (
            "bosa_search_reference_number",
            &publication["referenceNumber"],
        ),
        (
            "bosa_publication_reference_numbers_bda",
            &publication["publicationReferenceNumbersBDA"],
        ),
        (
            "bosa_procedure_type",
            &publication["dossier"]["procurementProcedureType"],
        ),
    ] {
        if !value.is_null() {
            tender.raw[key] = value.clone();
        }
    }
    if tender
        .notice_type
        .as_deref()
        .is_some_and(|kind| kind.starts_with("can"))
    {
        let outcomes = crate::award_xml::parse_lot_outcomes(xml.as_bytes())
            .map_err(|_| invalid("award lot outcomes could not be parsed"))?;
        tender.raw["bosa_lot_outcomes"] = serde_json::to_value(outcomes)
            .map_err(|_| invalid("award lot outcomes could not be serialized"))?;
        let awards = parse_award_lot_evidence(xml.as_bytes())
            .map_err(|_| invalid("award lot evidence could not be parsed"))?;
        tender.raw["bosa_award_lots"] = serde_json::to_value(awards)
            .map_err(|_| invalid("award lot evidence could not be serialized"))?;
    }
    // Only ACTIVE has been verified. Fail closed on other or missing lifecycle
    // states instead of ranking them as live calls until BOSA documents them.
    if publication.get("publicationType").and_then(Value::as_str) != Some("ACTIVE") {
        tender.notice_type = Some("bosa-status-unverified".into());
    }
    tender.raw["bosa_environment"] = match environment {
        Environment::Int => "int",
        Environment::Prod => "prod",
    }
    .into();
    Ok(tender)
}

#[derive(Deserialize)]
#[cfg(feature = "network")]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

/// One BOSA attempt's failure, before it becomes a [`BosaError`].
///
/// The counterpart of [`crate::http_retry::Attempt::classify`]: every BOSA call
/// now goes through the shared retry loop, so this is the single place the
/// attempt vocabulary becomes a BOSA error.
#[cfg(feature = "network")]
fn bosa_attempt_error(attempt: crate::http_retry::Attempt) -> BosaError {
    use crate::http_retry::Attempt;
    match attempt {
        Attempt::Guard(message) => BosaError::CallGuard(message),
        Attempt::Response(response) => http_error(response.status(), *response),
        Attempt::Transport => BosaError::Transport("connection or timeout failure".into()),
    }
}

#[cfg(feature = "network")]
fn read_body(response: Response) -> Result<Vec<u8>, BosaError> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| BosaError::Transport("response read failed".into()))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(BosaError::InvalidResponse(format!(
            "response exceeds {MAX_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

#[cfg(feature = "network")]
fn diagnostic(body: &Value) -> String {
    let error = body.get("error").unwrap_or(body);
    let message = error
        .pointer("/detail/detail")
        .and_then(Value::as_str)
        .or_else(|| error.get("detail").and_then(Value::as_str))
        .or_else(|| error.get("descriptionEN").and_then(Value::as_str))
        .or_else(|| error.pointer("/detail/title").and_then(Value::as_str))
        .unwrap_or("request was rejected");
    let message: String = message
        .chars()
        .filter(|c| !c.is_control())
        .take(500)
        .collect();
    match error.get("fsbTransactionId").and_then(Value::as_str) {
        Some(id) => format!(
            "{message}; FSB reference {}",
            id.chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect::<String>()
        ),
        None => message,
    }
}

#[cfg(feature = "network")]
fn http_error(status: u16, response: Response) -> BosaError {
    let detail = read_body(response)
        .ok()
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .map(|body| diagnostic(&body))
        .unwrap_or_else(|| "non-JSON rejection; check endpoint and access permissions".into());
    BosaError::Http { status, detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "network")]
    #[test]
    fn query_encodes_fields_and_rejects_invalid_pagination() {
        let mut search = PublicationSearch {
            page: Some(1),
            page_size: Some(100),
            terms: Some("roads & bridges".into()),
            cpv_codes: Some(vec!["45000000".into(), "72000000".into()]),
            ..Default::default()
        };
        let q = search.query().unwrap();
        assert!(q.contains(&("terms".into(), "roads & bridges".into())));
        assert!(q.contains(&("cpvCodes".into(), "45000000,72000000".into())));
        assert!(!q.iter().any(|(k, _)| k == "publicationDateFrom"));
        search.page = Some(0);
        assert!(search.query().is_err());
        search.page = Some(1);
        search.page_size = Some(101);
        assert!(search.query().is_err());
    }
    #[cfg(feature = "network")]
    #[test]
    fn errors_prefer_nested_detail_and_fall_back_to_english() {
        let body = serde_json::json!({"error":{"fsbTransactionId":"trace", "descriptionEN":"invalid request", "detail":{"detail":"bad CPV", "internal":"hidden"}}});
        assert_eq!(diagnostic(&body), "bad CPV; FSB reference trace");
        assert_eq!(
            diagnostic(&serde_json::json!({"error":{"descriptionEN":"expired"}})),
            "expired"
        );
    }
    #[cfg(feature = "network")]
    #[test]
    fn config_debug_never_discloses_credentials() {
        let config = BosaConfig {
            token_url: "https://identity.invalid/token".into(),
            api_base_url: "https://api.invalid".into(),
            client_id: "do-not-show-id".into(),
            client_secret: "do-not-show-secret".into(),
        };
        assert!(!format!("{config:?}").contains("do-not-show"));
        assert_eq!(Environment::Int.source(), "bda-int");
        assert_eq!(Environment::Prod.source(), "bda");
        assert!(Environment::parse("production").is_err());
    }
    #[cfg(feature = "network")]
    fn mock_client(
        replies: Vec<(u16, String)>,
    ) -> (BosaClient, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            for (status, body) in replies {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("mock accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            length = v.trim().parse().unwrap();
                        }
                    }
                    headers.push_str(&line);
                }
                let mut body_bytes = vec![0; length];
                reader.read_exact(&mut body_bytes).unwrap();
                requests.push(headers);
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            requests
        });
        let base = format!("http://{address}");
        (
            BosaClient::new(BosaConfig {
                token_url: format!("{base}/token"),
                api_base_url: base,
                client_id: "test".into(),
                client_secret: "secret".into(),
            }),
            handle,
        )
    }
    #[cfg(feature = "network")]
    #[test]
    fn gateway_get_refreshes_401_once_and_reuses_token_for_dossier() {
        let token = r#"{"access_token":"test-token","expires_in":3600}"#.to_owned();
        let (mut client, handle) = mock_client(vec![
            (200, token.clone()),
            (401, "{}".into()),
            (200, token),
            (200, r#"{"publications":[],"totalCount":0}"#.into()),
            (200, "{}".into()),
            (200, "[]".into()),
        ]);
        client
            .search_publications(&PublicationSearch {
                terms: Some("roads & bridges".into()),
                page: Some(1),
                page_size: Some(2),
                ..Default::default()
            })
            .unwrap();
        client
            .publication_workspace("7575ab87-0b8a-4440-be68-9a4d002e6310")
            .unwrap();
        assert!(client.publication_workspaces_for_dossier(0).is_err());
        assert!(client
            .publication_workspaces_for_dossier(1709882)
            .unwrap()
            .is_empty());
        let requests = handle.join().unwrap();
        let usage = crate::service_calls::test_ledger().summary().unwrap();
        assert_eq!(usage[0].attempts, 6); // two OAuth exchanges + four API attempts, including 401
        assert_eq!(usage[0].statuses[&401], 1);
        assert_eq!(usage[0].unknown, 0);
        assert!(requests[1].starts_with("GET /api/eProcurementSea/v1/search/publications?"));
        let target = requests[1].split_whitespace().nth(1).unwrap();
        let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert!(url
            .query_pairs()
            .any(|(k, v)| k == "terms" && v == "roads & bridges"));
        assert!(requests[4].starts_with("GET /api/eProcurementDos/v1/publication-workspaces/"));
        let target = requests[4].split_whitespace().nth(1).unwrap();
        let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "includeDrafts" && value == "false"));
        assert!(!url.query_pairs().any(|(key, _)| key == "enterpriseId"));
        assert!(requests[5]
            .starts_with("GET /api/eProcurementDos/v1/dossiers/1709882/publication-workspaces?"));
        let target = requests[5].split_whitespace().nth(1).unwrap();
        let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "includeDrafts" && value == "false"));
        let trace = |r: &str| {
            r.lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("BelGov-Trace-Id"))
                        .map(|(_, v)| v.trim().to_owned())
                })
                .unwrap()
        };
        assert_ne!(trace(&requests[1]), trace(&requests[3]));
        Uuid::parse_str(&trace(&requests[4])).unwrap();
    }
    #[cfg(feature = "network")]
    #[test]
    fn gateway_does_not_retry_403_and_rejects_malformed_success() {
        for (status, body) in [
            (
                403,
                r#"{"error":{"descriptionEN":"role denied","fsbTransactionId":"abc"}}"#,
            ),
            (200, r#"{"unexpected":[]}"#),
        ] {
            let (mut client, handle) = mock_client(vec![
                (200, r#"{"access_token":"t","expires_in":3600}"#.into()),
                (status, body.into()),
            ]);
            let error = client
                .search_publications(&PublicationSearch::default())
                .unwrap_err();
            if status == 403 {
                assert!(error.to_string().contains("role denied; FSB reference abc"));
            } else {
                assert!(matches!(error, BosaError::InvalidResponse(_)));
            }
            assert_eq!(handle.join().unwrap().len(), 2);
        }
    }
    /// A transient gateway error or throttle used to abandon the read and the
    /// whole dossier import with it. It is retried now, and each retry is a
    /// charged ledger attempt rather than a hidden extra request.
    #[cfg(feature = "network")]
    #[test]
    fn gateway_retries_a_transient_failure_before_giving_up() {
        let (mut client, handle) = mock_client(vec![
            (200, r#"{"access_token":"t","expires_in":3600}"#.into()),
            (503, r#"{"error":{"descriptionEN":"maintenance"}}"#.into()),
            (200, r#"{"publications":[],"totalCount":0}"#.into()),
        ]);
        let value = client
            .search_publications(&PublicationSearch::default())
            .expect("a transient gateway failure must be retried");
        assert_eq!(value["totalCount"], 0);
        assert_eq!(
            handle.join().unwrap().len(),
            3,
            "the retry must be a second real request"
        );
        let attempts: u64 = crate::service_calls::test_ledger()
            .summary()
            .unwrap()
            .iter()
            .map(|usage| usage.attempts)
            .sum();
        assert_eq!(attempts, 3, "a retried read is charged per attempt");
    }
    /// The token exchange is the first call of every BOSA sequence, so a
    /// transient failure there used to abort an import before a single
    /// publication was read. It is retried; a rejected credential is not.
    #[cfg(feature = "network")]
    #[test]
    fn token_exchange_survives_a_transient_failure_but_not_a_rejected_credential() {
        let (mut client, handle) = mock_client(vec![
            (503, r#"{"error":"temporarily_unavailable"}"#.into()),
            (200, r#"{"access_token":"t","expires_in":3600}"#.into()),
            (200, r#"{"publications":[],"totalCount":0}"#.into()),
        ]);
        client
            .search_publications(&PublicationSearch::default())
            .expect("a transient token failure must be retried");
        assert_eq!(handle.join().unwrap().len(), 3);

        // A rejected credential is configuration, not a blip: one attempt, and
        // the message still names the problem instead of retrying it.
        let (mut client, handle) = mock_client(vec![(
            401,
            r#"{"error":{"descriptionEN":"invalid_client"}}"#.into(),
        )]);
        let error = client
            .search_publications(&PublicationSearch::default())
            .expect_err("a rejected credential is final");
        assert!(error.to_string().contains("401"), "got: {error}");
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[cfg(feature = "network")]
    #[test]
    fn xml_attachment_selection_skips_missing_and_invalid_checksum_metadata() {
        let unavailable = |name: &str, status| documents::Attachment {
            version_id: Uuid::new_v4(),
            original_name: name.into(),
            sha256: String::new(),
            sha256_verified: false,
            checksum_status: status,
        };
        let verified = documents::Attachment {
            version_id: Uuid::new_v4(),
            original_name: "notice-nl.xml".into(),
            sha256: "a".repeat(64),
            sha256_verified: true,
            checksum_status: documents::ChecksumStatus::Verified,
        };
        let attachments = vec![
            unavailable("missing-nl.xml", documents::ChecksumStatus::Missing),
            unavailable("invalid-nl.xml", documents::ChecksumStatus::Invalid),
            verified.clone(),
        ];
        let selected = select_xml_attachment(&attachments, &["NL".into()]).unwrap();
        assert_eq!(selected.version_id, verified.version_id);
    }

    #[cfg(feature = "network")]
    #[test]
    fn xml_fallback_rejects_a_workspace_id_mismatch_before_listing_documents() {
        let workspace_id = "7575ab87-0b8a-4440-be68-9a4d002e6310";
        let (mut client, handle) = mock_client(vec![
            (200, r#"{"access_token":"t","expires_in":3600}"#.into()),
            (200, r#"{"id":"other-workspace","versions":[]}"#.into()),
        ]);
        let publication = serde_json::json!({"publicationWorkspaceId":workspace_id});
        let error = client
            .publication_workspace_with_xml_fallback(&publication, Environment::Prod)
            .unwrap_err();
        assert!(error.to_string().contains("workspace response ID"));
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /api/eProcurementDos/v1/publication-workspaces/"));
    }

    #[cfg(feature = "network")]
    #[test]
    fn xml_attachment_selection_requires_a_unique_published_language_match() {
        let attachment = |name: &str| documents::Attachment {
            version_id: Uuid::new_v4(),
            original_name: name.into(),
            sha256: "UNKNOWN HASH".into(),
            sha256_verified: false,
            checksum_status: documents::ChecksumStatus::Unknown,
        };
        let attachments = vec![
            attachment("279095_0_0_nl-NL.xml"),
            attachment("279095_0_0_fr-FR.xml"),
            attachment("279095_0_0_nl-NL.pdf"),
        ];
        assert_eq!(
            select_xml_attachment(&attachments, &["NL".into()])
                .unwrap()
                .original_name,
            "279095_0_0_nl-NL.xml"
        );
        assert!(select_xml_attachment(&attachments, &["DE".into()]).is_err());
        assert!(select_xml_attachment(&attachments, &[]).is_err());
        assert_eq!(
            select_xml_attachment(&attachments[..1], &[])
                .unwrap()
                .original_name,
            "279095_0_0_nl-NL.xml"
        );
    }

    #[test]
    fn legacy_f03_notice_preserves_award_item_marker_and_name_only_evidence() {
        let xml = include_bytes!("../tests/fixtures/bosa-2017-stad-gent-f03.xml");
        let xml = std::str::from_utf8(xml).unwrap();
        let publication = serde_json::json!({
            "publicationWorkspaceId":"workspace-id",
            "publicationType":"ACTIVE",
            "referenceNumber":"STAD GENT-PPP0HZ-4165/7087/ECO/2017-F03__3",
            "noticeIds":["notice-id"],
            "publicationDate":"2017-07-04",
            "dossier":{"titles":[{"text":"LED lighting framework"}]},
            "organisation":{"organisationNames":[{"text":"Stad Gent"}]},
            "cpvMainCode":{"code":"31000000"}
        });
        let workspace = serde_json::json!({
            "id":"workspace-id",
            "noticeType":"can-general",
            "formType":"AWARD",
            "versions":[{"status":"PUBLISHED","activationDate":"2017-07-04",
                "notice":{"id":"notice-id","xmlContent":xml}}]
        });
        let tender = publication_to_tender(&publication, &workspace, Environment::Prod).unwrap();
        assert_eq!(tender.raw["bosa_lot_outcomes"][0]["lot_id"], "AWARD-ITEM-1");
        assert_eq!(
            tender.raw["bosa_lot_outcomes"][0]["lot_id_kind"],
            "award-item"
        );
        assert_eq!(
            tender.raw["bosa_award_lots"][0]["lot_id_kind"],
            "award-item"
        );
        assert_eq!(
            tender.raw["bosa_award_lots"][0]["winner_name"],
            "nv Voorbeeld Leverancier"
        );
        assert!(tender.raw["bosa_award_lots"][0]["winner_identifier"].is_null());
        assert_eq!(tender.raw["bosa_award_lots"][0]["cpv_codes"][0], "31000000");
    }

    #[test]
    fn normalization_uses_latest_published_notice_and_current_search_metadata() {
        let xml = r#"<ContractNotice><ID>notice</ID><ContractFolderID>folder</ContractFolderID><VersionID>3</VersionID><FrameworkMaximumAmount currencyID="EUR">2600000.00</FrameworkMaximumAmount><IssueDate>2026-10-01</IssueDate><ProcurementProject><Name>XML title</Name><EstimatedOverallContractAmount>125000.50</EstimatedOverallContractAmount><ProcurementProjectLot><ID schemeName="lot">LOT-1</ID><ItemClassificationCode>45221100-4</ItemClassificationCode><FrameworkMaximumAmount currencyID="EUR">2600000.00</FrameworkMaximumAmount><ProcurementProject><RealizedLocation><Address><CountrySubentityCode>BE231</CountrySubentityCode></Address></RealizedLocation></ProcurementProject><TenderingProcess><FrameworkAgreement><SubsequentProcessTenderRequirement><Name>Second-stage proposal</Name></SubsequentProcessTenderRequirement></FrameworkAgreement></TenderingProcess><TenderingTerms><ContractExecutionRequirement><ExecutionRequirementCode listName="Synthetic BOSA execution types">SAFE_HANDLING</ExecutionRequirementCode></ContractExecutionRequirement><RequiredFinancialGuarantee><GuaranteeTypeCode listName="Synthetic guarantee types">PERFORMANCE_BOND<Description>Performance bond wording</Description></GuaranteeTypeCode></RequiredFinancialGuarantee><RequiredCurriculaCode listName="Synthetic curriculum types">CV_TEAM</RequiredCurriculaCode><TendererQualificationRequest><SpecificTendererRequirement><TendererRequirementTypeCode listName="Synthetic BOSA codes">TECHNICAL_ABILITY</TendererRequirementTypeCode></SpecificTendererRequirement></TendererQualificationRequest><UBLExtensions><UBLExtension><ExtensionContent><EformsExtension><SelectionCriteria><TendererRequirementTypeCode listName="Synthetic BOSA selection">FINANCIAL_STANDING</TendererRequirementTypeCode><Description>Relevant turnover</Description></SelectionCriteria></EformsExtension></ExtensionContent></UBLExtension></UBLExtensions><AwardingTerms><AwardingCriterion><SubordinateAwardingCriterion><AwardingCriterionTypeCode>QUALITY</AwardingCriterionTypeCode><Description>Service quality</Description></SubordinateAwardingCriterion></AwardingCriterion></AwardingTerms></TenderingTerms></ProcurementProjectLot><ProcurementProjectLot><ID schemeName="lot">LOT-2</ID><ItemClassificationCode>71300000-1</ItemClassificationCode></ProcurementProjectLot></ProcurementProject><EndDate>2026-10-15</EndDate></ContractNotice>"#;
        let p = serde_json::json!({"publicationWorkspaceId":"workspace","publicationType":"ACTIVE","referenceNumber":"PPP/123_003","publicationReferenceNumbersBDA":["PPP/123_003"],"noticeIds":["old","latest"],"publicationDate":"2026-10-30","vaultSubmissionDeadline":"2026-11-12T11:00:00","dossier":{"procurementProcedureType":"NEG_WO_CALL_24","titles":[{"text":"Current title"}]},"organisation":{"organisationNames":[{"text":"Buyer"},{"text":"Buyer"}]},"cpvMainCode":{"code":"45000000-7"}});
        let w = serde_json::json!({"id":"workspace","dossierId":12345,"dossierReferenceNumber":"PPP/123","referenceNumber":"003","version":2,"noticeType":"cn-general","versions":[
            {"status":"PUBLISHED","activationDate":"2026-10-01","notice":{"id":"old","xmlContent":"<ContractNotice><ID>old</ID></ContractNotice>"}},
            {"status":"PUBLISHED","activationDate":"2026-10-30","notice":{"id":"latest","xmlContent":xml}},
            {"status":"DRAFT","activationDate":"2026-11-01","notice":{"id":"future","xmlContent":"not XML"}}
        ]});
        let t = publication_to_tender(&p, &w, Environment::Prod).unwrap();
        assert_eq!(t.external_id, "folder");
        assert_eq!(t.source, "bda");
        assert_eq!(t.title, "Current title");
        assert_eq!(t.buyer.as_deref(), Some("Buyer"));
        assert_eq!(t.deadline.as_deref(), Some("2026-11-12"));
        assert_eq!(t.publication_date.as_deref(), Some("2026-10-30"));
        assert_eq!(
            t.estimated_value,
            Some(crate::parse_money("125000.50").unwrap())
        );
        assert!(t.cpv_codes.contains(&"45000000".to_owned()));
        assert_eq!(t.raw["bosa_notice_id"], "latest");
        assert_eq!(t.raw["bosa_dossier_id"], 12345);
        assert_eq!(t.raw["bosa_dossier_reference_number"], "PPP/123");
        assert_eq!(t.raw["bosa_workspace_reference_number"], "003");
        assert_eq!(t.raw["bosa_workspace_version"], 2);
        assert_eq!(t.raw["bosa_search_reference_number"], "PPP/123_003");
        assert_eq!(
            t.raw["bosa_publication_reference_numbers_bda"],
            serde_json::json!(["PPP/123_003"])
        );
        assert_eq!(t.raw["bosa_procedure_type"], "NEG_WO_CALL_24");
        assert_eq!(
            t.raw["framework_maximum_amounts"],
            serde_json::json!(["2600000.00"])
        );
        assert_eq!(
            t.raw["framework_maximum_evidence"],
            serde_json::json!([
                {"amount": "2600000.00", "currency_id": "EUR", "scope": "notice", "lot_id": null},
                {"amount": "2600000.00", "currency_id": "EUR", "scope": "lot", "lot_id": "LOT-1"}
            ])
        );
        assert_eq!(t.raw["notice_lot_ids_unique"], true);
        assert_eq!(
            t.raw["notice_lot_place_evidence"],
            serde_json::json!([
                {"lot_id": "LOT-1", "country_subentity_codes": ["BE231"]},
                {"lot_id": "LOT-2", "country_subentity_codes": []}
            ])
        );
        assert_eq!(
            t.raw["notice_lot_selection_evidence"],
            serde_json::json!([
                {"lot_id": "LOT-1", "selection_evidence": [
                    {"source_element": "SpecificTendererRequirement", "source_path": "TenderingTerms/TendererQualificationRequest/SpecificTendererRequirement", "source_ordinal_path": [0, 0], "code": "TECHNICAL_ABILITY", "code_list_name": "Synthetic BOSA codes", "descriptions": []},
                    {"source_element": "SelectionCriteria", "source_path": "TenderingTerms/UBLExtensions/UBLExtension/ExtensionContent/EformsExtension/SelectionCriteria", "source_ordinal_path": [0, 0], "code": "FINANCIAL_STANDING", "code_list_name": "Synthetic BOSA selection", "descriptions": ["Relevant turnover"]}
                ]},
                {"lot_id": "LOT-2", "selection_evidence": []}
            ])
        );
        assert_eq!(
            t.raw["notice_lot_contract_execution_evidence"],
            serde_json::json!([
                {"lot_id": "LOT-1", "conditions": [
                    {"source_element": "ContractExecutionRequirement", "source_path": "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode", "source_ordinal_path": [0], "code": "SAFE_HANDLING", "code_list_name": "Synthetic BOSA execution types"}
                ]},
                {"lot_id": "LOT-2", "conditions": []}
            ])
        );
        assert_eq!(
            t.raw["notice_lot_subsequent_process_evidence"],
            serde_json::json!([
                {"lot_id": "LOT-1", "requirements": [
                    {"source_element": "SubsequentProcessTenderRequirement", "source_path": "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name", "source_ordinal_path": [0], "name": "Second-stage proposal"}
                ]},
                {"lot_id": "LOT-2", "requirements": []}
            ])
        );
        assert_eq!(
            t.raw["notice_lot_guarantee_curricula_evidence"][0]["guarantees"][0]["code"],
            "PERFORMANCE_BOND"
        );
        assert_eq!(
            t.raw["notice_lot_guarantee_curricula_evidence"][0]["curricula"][0]["code_list_name"],
            "Synthetic curriculum types"
        );
        assert_eq!(
            t.raw["notice_lot_award_criteria_evidence"],
            serde_json::json!([
                {"lot_id": "LOT-1", "criteria": [
                    {"source_element": "AwardingCriterion", "source_ordinal_path": [0], "type_code": null, "description": null, "calculation_expression": null, "parameters": []},
                    {"source_element": "SubordinateAwardingCriterion", "source_ordinal_path": [0, 0], "type_code": "QUALITY", "description": "Service quality", "calculation_expression": null, "parameters": []}
                ]},
                {"lot_id": "LOT-2", "criteria": []}
            ])
        );
        assert_eq!(
            t.raw["notice_lot_cpv_evidence"],
            serde_json::json!([
                {"lot_id": "LOT-1", "cpv_codes": ["45221100"]},
                {"lot_id": "LOT-2", "cpv_codes": ["71300000"]}
            ])
        );
        let store = crate::store::TenderStore::open(":memory:").unwrap();
        store.upsert_many([&t]).unwrap();
        store.upsert_many([&t]).unwrap();
        assert_eq!(store.count().unwrap(), 1);
        let stored = store.tender("bda", "folder").unwrap().unwrap();
        assert_eq!(stored.title, t.title);
        assert_eq!(stored.estimated_value, t.estimated_value);
        assert_eq!(stored.deadline, t.deadline);
        assert_eq!(
            stored.raw["framework_maximum_evidence"],
            t.raw["framework_maximum_evidence"]
        );
        assert_eq!(
            stored.raw["notice_lot_cpv_evidence"],
            t.raw["notice_lot_cpv_evidence"]
        );
        assert_eq!(
            stored.raw["notice_lot_place_evidence"],
            t.raw["notice_lot_place_evidence"]
        );
        assert_eq!(
            stored.raw["notice_lot_award_criteria_evidence"],
            t.raw["notice_lot_award_criteria_evidence"]
        );
        assert_eq!(
            stored.raw["notice_lot_selection_evidence"],
            t.raw["notice_lot_selection_evidence"]
        );
        assert_eq!(
            stored.raw["notice_lot_contract_execution_evidence"],
            t.raw["notice_lot_contract_execution_evidence"]
        );
        assert_eq!(
            stored.raw["notice_lot_guarantee_curricula_evidence"],
            t.raw["notice_lot_guarantee_curricula_evidence"]
        );
        assert_eq!(
            publication_to_tender(&p, &w, Environment::Int)
                .unwrap()
                .source,
            "bda-int"
        );
        let mut unknown_status = p.clone();
        unknown_status["publicationType"] = "WITHDRAWN".into();
        assert!(
            !publication_to_tender(&unknown_status, &w, Environment::Prod)
                .unwrap()
                .is_opportunity()
        );
        let mut bad = w.clone();
        bad["id"] = "other".into();
        assert!(publication_to_tender(&p, &bad, Environment::Prod).is_err());
        bad = w.clone();
        bad["versions"] = serde_json::json!([]);
        assert!(publication_to_tender(&p, &bad, Environment::Prod).is_err());
        bad = w;
        bad["noticeType"] = "can-standard".into();
        assert!(!publication_to_tender(&p, &bad, Environment::Prod)
            .unwrap()
            .is_opportunity());
    }

    #[test]
    fn award_normalization_preserves_winner_identity_and_values_per_lot() {
        let xml = r#"<ContractAwardNotice><ContractFolderID>folder</ContractFolderID><ProcurementProject><ProcurementProjectLot><ID schemeName="lot">LOT-1</ID><ItemClassificationCode>72000000</ItemClassificationCode></ProcurementProjectLot></ProcurementProject><LotResult><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot><TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode><PayableAmount>368039.00</PayableAmount><LowerTenderAmount>350000</LowerTenderAmount><HigherTenderAmount>400000</HigherTenderAmount><LotTender><ID schemeName="tender">TEN-1</ID></LotTender><SettledContract><ID schemeName="contract">CON-1</ID></SettledContract></LotResult><SettledContract><ID schemeName="contract">CON-1</ID><LotTender><ID schemeName="tender">TEN-1</ID></LotTender></SettledContract><LotTender><ID schemeName="tender">TEN-1</ID><TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotTender><TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer></TenderingParty><Party><PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification><PartyName><Name>Example BV</Name></PartyName><PartyLegalEntity><CompanyID>0123456789</CompanyID></PartyLegalEntity></Party></ContractAwardNotice>"#;
        let publication = serde_json::json!({
            "publicationWorkspaceId": "workspace",
            "publicationType": "ACTIVE",
            "noticeIds": ["award-notice"],
            "dossier": {"titles": [{"text": "Award"}]},
            "organisation": {"organisationNames": [{"text": "Buyer"}]}
        });
        let workspace = serde_json::json!({
            "id": "workspace",
            "noticeType": "can-general",
            "versions": [{
                "status": "PUBLISHED",
                "activationDate": "2026-10-30",
                "notice": {"id": "award-notice", "xmlContent": xml}
            }]
        });

        let tender = publication_to_tender(&publication, &workspace, Environment::Prod)
            .expect("published BOSA award notice should normalize");
        assert_eq!(tender.raw["bosa_lot_outcomes"][0]["lot_id"], "LOT-1");
        assert_eq!(
            tender.raw["bosa_lot_outcomes"][0]["winner_selection_status"],
            "selec-w"
        );
        let lot = &tender.raw["bosa_award_lots"][0];
        assert_eq!(lot["lot_id"], "LOT-1");
        assert_eq!(lot["winner_identifier"], "0123456789");
        assert_eq!(lot["winner_name"], "Example BV");
        assert_eq!(lot["winner_selection_status"], "selec-w");
        assert_eq!(lot["linked_tenderers"].as_array().unwrap().len(), 1);
        assert_eq!(
            lot["contract_linked_tenderers"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            lot["contract_linked_tenderers"][0]["contract_references"],
            serde_json::json!(["CON-1"])
        );
        assert_eq!(
            lot["linked_tenderers"][0]["group_lead"],
            serde_json::Value::Null
        );
        assert_eq!(lot["reported_value"], "368039.00");
        assert_eq!(lot["lowest_tender_value"], "350000");
        assert_eq!(lot["highest_tender_value"], "400000");
        assert_eq!(lot["cpv_codes"], serde_json::json!(["72000000"]));

        let store = crate::store::TenderStore::open(":memory:").unwrap();
        store.upsert_many([&tender]).unwrap();
        let stored = store.tender("bda", "folder").unwrap().unwrap();
        assert_eq!(stored.raw["bosa_award_lots"], tender.raw["bosa_award_lots"]);
    }

    #[test]
    fn consortium_evidence_keeps_all_members_without_selecting_a_sole_winner() {
        let xml = r#"<ContractAwardNotice>
          <LotResult><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot><TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode><LotTender><ID schemeName="tender">TEN-1</ID></LotTender><LotTender><ID schemeName="tender">TEN-2</ID></LotTender><LotTender><ID schemeName="tender">TEN-3</ID></LotTender><SettledContract><ID schemeName="contract">CON-1</ID></SettledContract><SettledContract><ID schemeName="contract">CON-2</ID></SettledContract></LotResult>
          <SettledContract><ID schemeName="contract">CON-1</ID><LotTender><ID schemeName="tender">TEN-1</ID></LotTender></SettledContract>
          <SettledContract><ID schemeName="contract">CON-2</ID><LotTender><ID schemeName="tender">TEN-2</ID></LotTender></SettledContract>
          <LotTender><ID schemeName="tender">TEN-1</ID><TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotTender>
          <LotTender><ID schemeName="tender">TEN-2</ID><TenderingParty><ID schemeName="tendering-party">TPA-2</ID></TenderingParty><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotTender>
          <LotTender><ID schemeName="tender">TEN-3</ID><TenderingParty><ID schemeName="tendering-party">TPA-3</ID></TenderingParty><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotTender>
          <TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-1</ID><GroupLeadIndicator>true</GroupLeadIndicator></Tenderer><Tenderer><ID schemeName="organization">ORG-2</ID><GroupLeadIndicator>false</GroupLeadIndicator></Tenderer></TenderingParty>
          <TenderingParty><ID schemeName="tendering-party">TPA-2</ID><Tenderer><ID schemeName="organization">ORG-3</ID><GroupLeadIndicator>true</GroupLeadIndicator></Tenderer></TenderingParty>
          <TenderingParty><ID schemeName="tendering-party">TPA-3</ID><Tenderer><ID schemeName="organization">ORG-4</ID><GroupLeadIndicator>true</GroupLeadIndicator></Tenderer></TenderingParty>
          <Party><PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification><PartyName><Name>Member One</Name></PartyName><PartyLegalEntity><CompanyID>1111111111</CompanyID></PartyLegalEntity></Party>
          <Party><PartyIdentification><ID schemeName="organization">ORG-2</ID></PartyIdentification><PartyName><Name>Member Two</Name></PartyName><PartyLegalEntity><CompanyID>2222222222</CompanyID></PartyLegalEntity></Party>
          <Party><PartyIdentification><ID schemeName="organization">ORG-3</ID></PartyIdentification><PartyName><Name>Member Three</Name></PartyName><PartyLegalEntity><CompanyID>3333333333</CompanyID></PartyLegalEntity></Party>
          <Party><PartyIdentification><ID schemeName="organization">ORG-4</ID></PartyIdentification><PartyName><Name>Non-winner</Name></PartyName><PartyLegalEntity><CompanyID>4444444444</CompanyID></PartyLegalEntity></Party>
        </ContractAwardNotice>"#;

        let lots = parse_award_lot_evidence(xml.as_bytes())
            .expect("consortium award evidence should parse");
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].winner_identifier, None);
        assert_eq!(lots[0].winner_name, None);
        assert_eq!(lots[0].winner_selection_status.as_deref(), Some("selec-w"));
        assert_eq!(lots[0].linked_tenderers.len(), 4);
        assert_eq!(lots[0].contract_linked_tenderers.len(), 3);
        assert_eq!(lots[0].settled_contract_references, ["CON-1", "CON-2"]);
        assert_eq!(
            lots[0].linked_tenderers[0].identifier.as_deref(),
            Some("1111111111")
        );
        assert_eq!(lots[0].linked_tenderers[0].group_lead, Some(true));
        assert_eq!(
            lots[0].linked_tenderers[1].identifier.as_deref(),
            Some("2222222222")
        );
        assert_eq!(lots[0].linked_tenderers[1].group_lead, Some(false));
        assert_eq!(
            lots[0].linked_tenderers[2].identifier.as_deref(),
            Some("3333333333")
        );
        assert_eq!(
            lots[0].linked_tenderers[3].identifier.as_deref(),
            Some("4444444444")
        );
        assert!(lots[0].linked_tenderers[3].contract_references.is_empty());
        assert_eq!(lots[0].linked_tenderers[2].group_lead, Some(true));
    }

    #[test]
    fn ongoing_lot_keeps_linked_party_but_has_no_winner_identity() {
        let xml = br#"<ContractAwardNotice>
          <LotResult><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot><TenderResultCode listName="winner-selection-status">open-nw</TenderResultCode><LotTender><ID schemeName="tender">TEN-1</ID></LotTender></LotResult>
          <LotTender><ID schemeName="tender">TEN-1</ID><TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotTender>
          <TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer></TenderingParty>
          <Party><PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification><PartyName><Name>Example BV</Name></PartyName><PartyLegalEntity><CompanyID>1111111111</CompanyID></PartyLegalEntity></Party>
        </ContractAwardNotice>"#;

        let lots = parse_award_lot_evidence(xml).expect("open lot evidence should parse");
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].winner_selection_status.as_deref(), Some("open-nw"));
        assert_eq!(lots[0].winner_identifier, None);
        assert_eq!(lots[0].winner_name, None);
        assert_eq!(lots[0].linked_tenderers.len(), 1);
        assert_eq!(
            lots[0].linked_tenderers[0].identifier.as_deref(),
            Some("1111111111")
        );
    }
}
