//! Live KBO Public Search webservice client (SOAP + WS-Security).
//!
//! The snapshot database (`kbo_enrich.duckdb`) is frozen at its extract date;
//! this client reads yesterday's state per company: live legal status,
//! NACEBEL activities (incl. the 2025 nomenclature), names and the seat
//! address. Same [`CompanyProfile`] shape as the snapshot lookup, so callers
//! can fall back transparently.
//!
//! Auth is a WS-Security UsernameToken with a SHA-1 password digest computed
//! per request — the plaintext secret only ever lives in the process env
//! (`KBO_PUB_USER` / `KBO_PUB_PASSWORD`, optional `KBO_PUB_ENDPOINT`) and is
//! never logged, persisted, or included in error output. Cookbook:
//! `Cookbook-KBO-Public-Search-Webservice` R018.08 (free tier, 3,000
//! requests/day, balance returned per reply).
//!
//! What the live reply cannot provide: establishments (a separate
//! `ReadEstablishmentByEnterpriseNumber` operation — one extra request per
//! company, deliberately not spent here) and the snapshot's filtered activity
//! history. Live activities keep `MAIN`-classified rows only, matching the
//! snapshot's current-activity semantics; struck-off addresses are skipped
//! like the snapshot's `DateStrikingOff IS NULL` filter.

use crate::{Activity, Address, CompanyProfile};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use sha1::{Digest, Sha1};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_ENDPOINT: &str =
    "https://kbopub.economie.fgov.be/kbopubws180000/services/wsKBOPub";
pub const TEST_ENDPOINT: &str =
    "https://kbopub-acc.economie.fgov.be/kbopubws180000/services/wsKBOPub";

const MES_NS: &str = "http://economie.fgov.be/kbopub/webservices/v1/messages";

/// Credentials resolve from the process environment only — never from files,
/// so there is no credential path to misconfigure or commit.
#[derive(Clone)]
pub struct LiveCredentials {
    pub user: String,
    pub password: String,
    pub endpoint: String,
}

impl std::fmt::Debug for LiveCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveCredentials")
            .field("user", &"[REDACTED]")
            .field("password", &"[REDACTED]")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl LiveCredentials {
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let var = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    format!("{name} is not set (see docs/runbook.md, KBO live lookup)").into()
                })
        };
        let endpoint =
            std::env::var("KBO_PUB_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned());
        if endpoint != DEFAULT_ENDPOINT && endpoint != TEST_ENDPOINT {
            return Err(
                "KBO_PUB_ENDPOINT must be the official production or acceptance endpoint".into(),
            );
        }
        Ok(Self {
            user: var("KBO_PUB_USER")?,
            password: var("KBO_PUB_PASSWORD")?,
            endpoint,
        })
    }
}

pub struct LiveClient {
    credentials: LiveCredentials,
    timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct LiveEnterpriseLookup {
    pub profile: Option<CompanyProfile>,
    pub reported_enterprise_number: Option<String>,
    pub remaining_requests: Option<i64>,
}

impl LiveClient {
    pub fn new(credentials: LiveCredentials) -> Self {
        Self {
            credentials,
            timeout_secs: 30,
        }
    }

    /// Look up one enterprise, returning the profile and the remaining daily
    /// request balance. `Ok(None)` = unknown/cancelled number, not an error.
    pub fn read_enterprise(
        &self,
        enterprise_number: &str,
    ) -> Result<(Option<CompanyProfile>, Option<i64>), Box<dyn std::error::Error>> {
        let result = self.read_enterprise_with_raw_number(enterprise_number)?;
        Ok((result.profile, result.remaining_requests))
    }

    /// Same lookup, additionally returning the raw enterprise number from KBO.
    /// Identity joins should use this value rather than a normalized profile ID.
    pub fn read_enterprise_with_raw_number(
        &self,
        enterprise_number: &str,
    ) -> Result<LiveEnterpriseLookup, Box<dyn std::error::Error>> {
        let digits: String = enterprise_number
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
        if digits.len() != 10 {
            return Err(format!("not a 10-digit enterprise number: {enterprise_number}").into());
        }
        let body = build_request(&self.credentials.user, &self.credentials.password, &digits);
        // The service answers transport failures AND SOAP faults over HTTP;
        // a 500 usually carries a faultstring (bad credentials, empty
        // balance, first-use terms acceptance) that diagnoses the problem.
        let agent = ureq::AgentBuilder::new()
            // Disable pooled-connection resends: each HTTP send needs its own ledger reservation.
            .max_idle_connections(0)
            .redirects(0)
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .user_agent(concat!(
                "ehb-fin-aanbestedingen/",
                env!("CARGO_PKG_VERSION")
            ))
            .build();
        let outcome = crate::http_retry::send_with_retry(
            &crate::http_retry::RetryPolicy::kbo(),
            || {
                crate::service_calls::http(
                    crate::service_calls::Service::Kbo,
                    crate::service_calls::Operation::KboEnterprise,
                    || {
                        agent
                            .post(&self.credentials.endpoint)
                            .set("Content-Type", "text/xml; charset=utf-8")
                            // Exact soapAction per the service WSDL (note fgov.economie.be,
                            // not economie.fgov.be — a wrong value faults before auth runs).
                            .set(
                                "SOAPAction",
                                "http://fgov.economie.be/kbopub/ReadEnterprise",
                            )
                            .timeout(std::time::Duration::from_secs(self.timeout_secs))
                            .send_string(&body)
                    },
                )
                .map_err(|e| crate::http_retry::Attempt::Guard(e.to_string()))
                .and_then(crate::http_retry::Attempt::classify)
            },
            crate::http_retry::Attempt::signal,
        );
        // KBO answers SOAP faults as HTTP 500 with a faultstring, so a status the
        // policy will not retry is *not* an error here: its body is the diagnosis
        // and is read below. Only throttling past the budget and transport
        // failures end the lookup without a response.
        let response = match outcome {
            Ok(response) => response,
            Err(crate::http_retry::RetryStop::Failed(crate::http_retry::Attempt::Response(
                response,
            ))) => *response,
            Err(stop) => match stop.into_error() {
                crate::http_retry::Attempt::Guard(message) => return Err(message.into()),
                crate::http_retry::Attempt::Transport => {
                    return Err("KBO webservice transport failure".into())
                }
                crate::http_retry::Attempt::Response(response) => {
                    return Err(format!(
                        "KBO still throttled (HTTP {}) after the retry budget",
                        response.status()
                    )
                    .into())
                }
            },
        };
        let status = response.status();
        if (300..400).contains(&status) {
            return Err(format!("KBO redirect refused (HTTP {status})").into());
        }
        use std::io::Read;
        const MAX_BYTES: u64 = 4 * 1024 * 1024;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "KBO response read failed")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("KBO response exceeds 4 MiB".into());
        }
        let text = String::from_utf8(bytes).map_err(|_| "KBO response is not UTF-8")?;
        let result = parse_reply_with_raw_number(&text, &digits).map_err(|error| {
            let mut detail = error.to_string();
            for secret in [&self.credentials.user, &self.credentials.password] {
                if !secret.is_empty() {
                    detail = detail.replace(secret, "[REDACTED]");
                }
            }
            Box::<dyn std::error::Error>::from(detail)
        })?;
        if status != 200 {
            return Err(format!("KBO returned HTTP {status}").into());
        }
        Ok(result)
    }
}

fn request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("aanbestedingen-{}-{}", std::process::id(), nanos)
}

fn utc_timestamp(offset_secs: u64) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() + offset_secs)
        .unwrap_or(0);
    format_timestamp(secs)
}

fn format_timestamp(secs: u64) -> String {
    // Format manually to avoid a chrono dependency in this module's tests.
    let days = secs / 86_400;
    let time = secs % 86_400;
    let (mut y, mut days_left) = (1970_i64, days as i64);
    loop {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let year_days = if leap { 366 } else { 365 };
        if days_left < year_days {
            break;
        }
        days_left -= year_days;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let months = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    while days_left >= months[m] as i64 {
        days_left -= months[m] as i64;
        m += 1;
    }
    format!(
        "{y:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        m + 1,
        days_left + 1,
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// WS-Security PasswordDigest: Base64(SHA1(nonce + created + password)).
fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(nonce);
    hasher.update(created.as_bytes());
    hasher.update(password.as_bytes());
    BASE64.encode(hasher.finalize())
}

fn random_nonce() -> [u8; 16] {
    // Uniqueness per request is what matters, not cryptographic strength:
    // mix time, pid and address entropy without pulling a rand dependency.
    let mut nonce = [0u8; 16];
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0x9e3779b97f4a7c15);
    let pid = std::process::id() as u128;
    let stack = &nonce as *const u8 as usize as u128;
    let mixed = nanos ^ (pid << 64) ^ stack ^ 0x9e3779b97f4a7c159e3779b97f4a7c15u128;
    nonce.copy_from_slice(&mixed.to_le_bytes());
    nonce
}

fn build_request(user: &str, password: &str, digits: &str) -> String {
    let nonce = random_nonce();
    let created = utc_timestamp(0);
    let expires = utc_timestamp(300);
    let digest = password_digest(&nonce, &created, password);
    let nonce_b64 = BASE64.encode(nonce);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns:mes="{MES_NS}" xmlns:dat="http://economie.fgov.be/kbopub/webservices/v1/datamodel" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">
<soapenv:Header>
<wsse:Security>
<wsu:Timestamp><wsu:Created>{created}</wsu:Created><wsu:Expires>{expires}</wsu:Expires></wsu:Timestamp>
<wsse:UsernameToken><wsse:Username>{user}</wsse:Username><wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password><wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{nonce_b64}</wsse:Nonce><wsu:Created>{created}</wsu:Created></wsse:UsernameToken>
</wsse:Security>
<mes:RequestContext><mes:Id>{id}</mes:Id><mes:Language>nl</mes:Language><mes:Language>fr</mes:Language></mes:RequestContext>
</soapenv:Header>
<soapenv:Body>
<mes:ReadEnterpriseRequest><dat:EnterpriseNumber>{digits}</dat:EnterpriseNumber></mes:ReadEnterpriseRequest>
</soapenv:Body>
</soapenv:Envelope>"#,
        id = request_id(),
        user = xml_escape(user),
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn local_name(raw: &[u8], decoder: &quick_xml::encoding::Decoder) -> String {
    let decoded = decoder.decode(raw).unwrap_or_default().into_owned();
    decoded.rsplit(':').next().unwrap_or_default().to_owned()
}

#[derive(Default)]
struct Description {
    value: String,
    language: String,
}

struct PendingDescription {
    owner: String,
    description: Description,
}

#[derive(Default)]
struct PendingActivity {
    code: String,
    classification: String,
    descriptions: Vec<Description>,
}

#[derive(Default)]
struct PendingAddress {
    kind: String,
    zipcode: Option<String>,
    house_number: Option<String>,
    box_value: Option<String>,
    municipality: Vec<Description>,
    street: Vec<Description>,
    struck_off: bool,
}

fn description_value(descriptions: &[Description], language: &str) -> Option<String> {
    descriptions
        .iter()
        .find(|d| d.language.eq_ignore_ascii_case(language))
        .map(|d| d.value.clone())
        .filter(|v| !v.trim().is_empty())
}

/// Parse a `ReadEnterpriseReply`. The second argument is the requested digits,
/// checked against the normalized reply identity; the raw number is preserved.
#[cfg(test)]
fn parse_reply(
    xml: &str,
    requested: &str,
) -> Result<(Option<CompanyProfile>, Option<i64>), Box<dyn std::error::Error>> {
    let result = parse_reply_with_raw_number(xml, requested)?;
    Ok((result.profile, result.remaining_requests))
}

fn parse_reply_with_raw_number(
    xml: &str,
    requested: &str,
) -> Result<LiveEnterpriseLookup, Box<dyn std::error::Error>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let decoder = reader.decoder();
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();

    let mut status_code: Option<String> = None;
    let mut balance: Option<i64> = None;

    let mut number = String::new();
    let mut status = Option::<String>::None;
    let mut juridical_situation = Option::<String>::None;
    let mut start_date = Option::<String>::None;
    let mut juridical_form = Option::<String>::None;
    let mut names: Vec<String> = Vec::new();
    let mut activities: Vec<Activity> = Vec::new();
    let mut addresses: Vec<Address> = Vec::new();

    let mut pending_descriptions: Vec<PendingDescription> = Vec::new();
    let mut pending_activity: Option<PendingActivity> = None;
    let mut pending_address: Option<PendingAddress> = None;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(element)) => {
                let name = local_name(element.name().as_ref(), &decoder);
                if name == "Description" {
                    pending_descriptions.push(PendingDescription {
                        owner: stack.last().cloned().unwrap_or_default(),
                        description: Description::default(),
                    });
                }
                if name == "Activity" {
                    pending_activity = Some(PendingActivity::default());
                }
                if name == "Address" {
                    pending_address = Some(PendingAddress::default());
                }
                stack.push(name);
                text.clear();
            }
            Ok(Event::Empty(element)) => {
                let name = local_name(element.name().as_ref(), &decoder);
                if name == "StrikingOff" && pending_address.is_some() {
                    if let Some(address) = pending_address.as_mut() {
                        address.struck_off = true;
                    }
                }
            }
            Ok(Event::Text(event)) => {
                text = event.decode()?.into_owned();
            }
            Ok(Event::End(element)) => {
                let name = local_name(element.name().as_ref(), &decoder);
                let parent = if stack.len() >= 2 {
                    stack[stack.len() - 2].clone()
                } else {
                    String::new()
                };
                let grandparent = if stack.len() >= 3 {
                    stack[stack.len() - 3].clone()
                } else {
                    String::new()
                };
                match name.as_str() {
                    "Code" if parent == "ReplyStatus" => status_code = Some(text.trim().to_owned()),
                    "AccountBalance" => {
                        balance = text.trim().parse::<i64>().ok().or(balance);
                    }
                    "Number"
                        if parent == "Enterprise"
                            && stack.iter().any(|s| s == "ReadEnterpriseReply") =>
                    {
                        number = text.trim().to_owned();
                    }
                    "Code" if parent == "Status" && grandparent == "JuridicalSituation" => {
                        status = Some(text.trim().to_owned());
                    }
                    "Code"
                        if parent == "JuridicalSituation"
                            && stack.last().map(String::as_str) == Some("Code") =>
                    {
                        // Direct child Code of JuridicalSituation (not via Status).
                        if grandparent == "Enterprise" {
                            juridical_situation = Some(text.trim().to_owned());
                        }
                    }
                    "Begin" if parent == "Period" && grandparent == "Enterprise" => {
                        start_date = Some(text.trim().to_owned());
                    }
                    "Code" if parent == "JuridicalForm" && grandparent == "Enterprise" => {
                        juridical_form = Some(text.trim().to_owned());
                    }
                    "Code" if parent == "Activity" => {
                        if let Some(activity) = pending_activity.as_mut() {
                            activity.code = text.trim().to_owned();
                        }
                    }
                    "Code" if parent == "ActivityClassification" && grandparent == "Activity" => {
                        if let Some(activity) = pending_activity.as_mut() {
                            activity.classification = text.trim().to_owned();
                        }
                    }
                    "Code" if parent == "TypeOfAddress" => {
                        if let Some(address) = pending_address.as_mut() {
                            address.kind = text.trim().to_owned();
                        }
                    }
                    "Zipcode" => {
                        if let Some(address) = pending_address.as_mut() {
                            address.zipcode = Some(text.trim().to_owned());
                        }
                    }
                    "HouseNumber" => {
                        if let Some(address) = pending_address.as_mut() {
                            address.house_number = Some(text.trim().to_owned());
                        }
                    }
                    "Box" => {
                        if let Some(address) = pending_address.as_mut() {
                            address.box_value = Some(text.trim().to_owned());
                        }
                    }
                    "Value" => {
                        if let Some(pending) = pending_descriptions.last_mut() {
                            pending.description.value = text.trim().to_owned();
                        }
                    }
                    "Language" => {
                        if let Some(pending) = pending_descriptions.last_mut() {
                            pending.description.language = text.trim().to_owned();
                        }
                    }
                    "Description" => {
                        if let Some(pending) = pending_descriptions.pop() {
                            let owner_parent = parent.clone();
                            let finished = pending.description;
                            match (pending.owner.as_str(), owner_parent.as_str()) {
                                ("Denomination", _) => {
                                    if !finished.value.trim().is_empty() {
                                        names.push(finished.value);
                                    }
                                }
                                ("Activity", _) => {
                                    if let Some(activity) = pending_activity.as_mut() {
                                        activity.descriptions.push(finished);
                                    }
                                }
                                ("Municipality", _) => {
                                    if let Some(address) = pending_address.as_mut() {
                                        address.municipality.push(finished);
                                    }
                                }
                                ("Street", _) => {
                                    if let Some(address) = pending_address.as_mut() {
                                        address.street.push(finished);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    "Activity" => {
                        if let Some(activity) = pending_activity.take() {
                            // Snapshot parity: current MAIN activities only.
                            if activity.classification == "MAIN" && !activity.code.is_empty() {
                                activities.push(Activity {
                                    code: activity.code,
                                    classification: activity.classification,
                                    french: description_value(&activity.descriptions, "fr"),
                                    dutch: description_value(&activity.descriptions, "nl"),
                                });
                            }
                        }
                    }
                    "Address" => {
                        if let Some(address) = pending_address.take() {
                            if !address.struck_off {
                                addresses.push(Address {
                                    address_type: address.kind,
                                    zipcode: address.zipcode.filter(|v| !v.is_empty()),
                                    municipality_nl: description_value(&address.municipality, "nl"),
                                    municipality_fr: description_value(&address.municipality, "fr"),
                                    street_nl: description_value(&address.street, "nl"),
                                    street_fr: description_value(&address.street, "fr"),
                                    house_number: address.house_number.filter(|v| !v.is_empty()),
                                    box_value: address.box_value.filter(|v| !v.is_empty()),
                                });
                            }
                        }
                    }
                    _ => {}
                }
                stack.pop();
                text.clear();
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(format!("KBO reply is not valid XML: {error}").into()),
            _ => {}
        }
        buf.clear();
    }

    match status_code.as_deref() {
        Some("00001") => {}
        // No data / unknown / cancelled / not-yet-active: a clean absence.
        Some("00003") | Some("00010") | Some("00014") | Some("00016") | Some("00017") => {
            return Ok(LiveEnterpriseLookup {
                profile: None,
                reported_enterprise_number: None,
                remaining_requests: balance,
            })
        }
        Some(other) => {
            return Err(format!(
                "KBO lookup for {requested} refused (status {other}); \
                 00005 means the number failed validation"
            )
            .into())
        }
        None => {
            // A SOAP fault carries no ReplyStatus — surface its text, which
            // never contains our credentials (they only go out in requests).
            if let Some(fault) = soap_fault_text(xml) {
                return Err(format!("KBO webservice fault: {fault}").into());
            }
            return Err("KBO reply has no status and no fault".into());
        }
    }

    let enterprise_number = crate::kbo::normalize_enterprise_number(&number)
        .map_err(|_| "KBO successful reply has no valid enterprise identity")?;
    let expected = crate::kbo::normalize_enterprise_number(requested)
        .map_err(|_| "KBO requested identity is invalid")?;
    if enterprise_number != expected {
        return Err("KBO response enterprise identity mismatch".into());
    }
    let reported_enterprise_number = number;
    names.sort();
    names.dedup();
    // One row per activity group/version: collapse exact duplicates so the
    // profile matches the snapshot's one-row-per-code shape.
    activities.sort_by(|a, b| {
        (&a.code, &a.classification, &a.french, &a.dutch).cmp(&(
            &b.code,
            &b.classification,
            &b.french,
            &b.dutch,
        ))
    });
    activities.dedup_by(|a, b| {
        a.code == b.code
            && a.classification == b.classification
            && a.french == b.french
            && a.dutch == b.dutch
    });
    Ok(LiveEnterpriseLookup {
        profile: Some(CompanyProfile {
            enterprise_number,
            status,
            juridical_situation,
            juridical_form,
            start_date,
            names,
            activities,
            addresses,
            establishments: Vec::new(),
            ..Default::default()
        }),
        reported_enterprise_number: Some(reported_enterprise_number),
        remaining_requests: balance,
    })
}

/// Best-effort fault text for replies without a ReplyStatus.
fn soap_fault_text(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let decoder = reader.decoder();
    let mut buf = Vec::new();
    let mut in_faultstring = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(element)) => {
                in_faultstring = local_name(element.name().as_ref(), &decoder) == "faultstring";
            }
            Ok(Event::Text(event)) if in_faultstring => {
                let text: String = event.decode().unwrap_or_default().into_owned();
                if !text.trim().is_empty() {
                    return Some(text.trim().to_owned());
                }
            }
            Ok(Event::End(_)) => in_faultstring = false,
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_debug_is_redacted() {
        let credentials = LiveCredentials {
            user: "private-user".into(),
            password: "private-password".into(),
            endpoint: DEFAULT_ENDPOINT.into(),
        };
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("private-user"));
        assert!(!debug.contains("private-password"));
    }

    #[test]
    fn redirect_is_refused_without_following() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", socket.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = socket.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut b = [0u8; 1];
                stream.read_exact(&mut b).unwrap();
                request.push(b[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let headers = String::from_utf8(request).unwrap();
            let size = headers
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; size];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://invalid.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let error = LiveClient::new(LiveCredentials {
            user: "private-user".into(),
            password: "private-password".into(),
            endpoint,
        })
        .read_enterprise("0202239951")
        .unwrap_err();
        assert!(error.to_string().contains("redirect refused"));
        assert!(!error.to_string().contains("private-user"));
        server.join().unwrap();
    }

    const REPLY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns:mes="http://economie.fgov.be/kbopub/webservices/v1/messages" xmlns:dat="http://economie.fgov.be/kbopub/webservices/v1/datamodel">
<soapenv:Header><mes:ReplyContext><mes:RequestId>x</mes:RequestId><mes:ReplyId>y</mes:ReplyId><mes:ReplyStatus><mes:Code>00001</mes:Code><mes:Description>ok</mes:Description></mes:ReplyStatus><mes:AccountBalance>2999</mes:AccountBalance></mes:ReplyContext></soapenv:Header>
<soapenv:Body><mes:ReadEnterpriseReply>
<dat:Enterprise><dat:Number>0202239951</dat:Number>
<dat:JuridicalSituation><dat:Code>000</dat:Code><dat:Status><dat:Code>AC</dat:Code></dat:Status></dat:JuridicalSituation>
<dat:Period><dat:Begin>1930-07-19</dat:Begin></dat:Period>
<dat:JuridicalForm><dat:Code>114</dat:Code></dat:JuridicalForm>
<dat:Denomination><dat:Code>001</dat:Code><dat:Description><dat:Value>PROXIMUS</dat:Value><dat:Language>nl</dat:Language></dat:Description></dat:Denomination>
<dat:Activity><dat:Code>61100</dat:Code><dat:Version>2025</dat:Version><dat:Description><dat:Value>Draadgebonden en draadloze telecommunicatie</dat:Value><dat:Language>nl</dat:Language></dat:Description><dat:Description><dat:Value>Télécommunications filaires et sans fil</dat:Value><dat:Language>fr</dat:Language></dat:Description><dat:ActivityClassification><dat:Code>MAIN</dat:Code></dat:ActivityClassification></dat:Activity>
<dat:Activity><dat:Code>62010</dat:Code><dat:Version>2008</dat:Version><dat:Description><dat:Value>Oude activiteit</dat:Value><dat:Language>nl</dat:Language></dat:Description><dat:ActivityClassification><dat:Code>SECONDARY</dat:Code></dat:ActivityClassification></dat:Activity>
<dat:Address><dat:TypeOfAddress><dat:Code>REGO</dat:Code></dat:TypeOfAddress><dat:Zipcode>1030</dat:Zipcode><dat:Municipality><dat:Code>21003</dat:Code><dat:Description><dat:Value>Schaarbeek</dat:Value><dat:Language>nl</dat:Language></dat:Description><dat:Description><dat:Value>Schaerbeek</dat:Value><dat:Language>fr</dat:Language></dat:Description></dat:Municipality><dat:Street><dat:Description><dat:Value>Koning Albert II-laan</dat:Value><dat:Language>nl</dat:Language></dat:Description><dat:Description><dat:Value>Boulevard du Roi Albert II</dat:Value><dat:Language>fr</dat:Language></dat:Description></dat:Street><dat:HouseNumber>27</dat:HouseNumber></dat:Address>
</dat:Enterprise>
</mes:ReadEnterpriseReply></soapenv:Body>
</soapenv:Envelope>"#;

    /// Serves one KBO reply per entry, in order, and reports how many requests
    /// arrived so a retry test can prove the attempt count.
    fn serve_kbo(
        replies: Vec<(&'static str, &'static str, &'static [u8])>,
    ) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", socket.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut served = 0;
            for (status, headers, body) in replies {
                let (mut stream, _) = socket.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut b = [0u8; 1];
                    stream.read_exact(&mut b).unwrap();
                    request.push(b[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let headers_text = String::from_utf8(request).unwrap();
                if let Some(size) = headers_text.lines().find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                }) {
                    let mut body_bytes = vec![0; size.parse::<usize>().unwrap()];
                    stream.read_exact(&mut body_bytes).unwrap();
                }
                served += 1;
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
            served
        });
        (endpoint, handle)
    }

    fn live_client(endpoint: String) -> LiveClient {
        LiveClient::new(LiveCredentials {
            user: "private-user".into(),
            password: "private-password".into(),
            endpoint,
        })
    }

    /// Throttling is the one retryable status: the lookup recovers instead of
    /// losing the company from a nightly rebuild.
    #[test]
    fn retries_throttling_then_reads_the_reply() {
        let (endpoint, server) = serve_kbo(vec![
            ("429 Too Many Requests", "Retry-After: 1\r\n", b""),
            ("200 OK", "Content-Type: text/xml\r\n", REPLY.as_bytes()),
        ]);
        let lookup = live_client(endpoint).read_enterprise("0202239951").unwrap();
        assert_eq!(lookup.1, Some(2999));
        assert_eq!(server.join().unwrap(), 2, "the throttle is retried once");
    }
    /// A SOAP fault arrives as HTTP 500 and its body *is* the diagnosis, so it
    /// must be read once and never retried.
    #[test]
    fn reads_a_fault_body_once_without_retrying() {
        let fault = br#"<?xml version="1.0"?><soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><soap:Fault><faultcode>soap:Client</faultcode><faultstring>Quota Exceeded!</faultstring></soap:Fault></soap:Body></soap:Envelope>"#;
        let (endpoint, server) = serve_kbo(vec![(
            "500 Internal Server Error",
            "Content-Type: text/xml\r\n",
            fault,
        )]);
        let error = live_client(endpoint)
            .read_enterprise("0202239951")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Quota Exceeded!"),
            "the fault text is the diagnosis: {error}"
        );
        assert!(!error.contains("private-user") && !error.contains("private-password"));
        assert_eq!(
            server.join().unwrap(),
            1,
            "a SOAP fault is answered once; retrying would spend balance twice"
        );
    }

    #[test]
    fn reply_parses_to_snapshot_shaped_profile() {
        let lookup = parse_reply_with_raw_number(REPLY, "0202239951").expect("reply should parse");
        assert_eq!(
            lookup.reported_enterprise_number.as_deref(),
            Some("0202239951")
        );
        assert_eq!(lookup.remaining_requests, Some(2999));
        let profile = lookup.profile.expect("enterprise should be present");
        assert_eq!(profile.enterprise_number, "0202.239.951");
        assert_eq!(profile.status.as_deref(), Some("AC"));
        assert_eq!(profile.juridical_situation.as_deref(), Some("000"));
        assert_eq!(profile.juridical_form.as_deref(), Some("114"));
        assert_eq!(profile.start_date.as_deref(), Some("1930-07-19"));
        assert_eq!(profile.names, vec!["PROXIMUS".to_owned()]);
        // MAIN only: the SECONDARY row is dropped like the snapshot filter.
        assert_eq!(profile.activities.len(), 1);
        assert_eq!(profile.activities[0].code, "61100");
        assert_eq!(
            profile.activities[0].dutch.as_deref(),
            Some("Draadgebonden en draadloze telecommunicatie")
        );
        assert_eq!(
            profile.activities[0].french.as_deref(),
            Some("Télécommunications filaires et sans fil")
        );
        assert_eq!(profile.addresses.len(), 1);
        assert_eq!(profile.addresses[0].address_type, "REGO");
        assert_eq!(profile.addresses[0].zipcode.as_deref(), Some("1030"));
        assert_eq!(
            profile.addresses[0].municipality_nl.as_deref(),
            Some("Schaarbeek")
        );
        assert!(profile.establishments.is_empty());
    }

    #[test]
    fn nested_descriptions_keep_parent_owner_and_content_separate() {
        let xml = REPLY.replace(
            "<dat:Denomination><dat:Code>001</dat:Code><dat:Description><dat:Value>PROXIMUS</dat:Value><dat:Language>nl</dat:Language></dat:Description></dat:Denomination>",
            "<dat:Denomination><dat:Code>001</dat:Code><dat:Description><dat:Value>PROXIMUS</dat:Value><dat:Description><dat:Value>nested</dat:Value></dat:Description><dat:Language>nl</dat:Language></dat:Description></dat:Denomination>",
        );
        let profile = parse_reply(&xml, "0202239951")
            .expect("nested descriptions should parse")
            .0
            .expect("enterprise should be present");
        assert_eq!(profile.names, vec!["PROXIMUS"]);
    }

    #[test]
    fn raw_reply_number_is_not_padded_when_profile_is_normalized() {
        let xml = REPLY.replace(
            "<dat:Number>0202239951</dat:Number>",
            "<dat:Number>202239951</dat:Number>",
        );
        let lookup = parse_reply_with_raw_number(&xml, "0202239951").expect("reply should parse");
        assert_eq!(
            lookup.reported_enterprise_number.as_deref(),
            Some("202239951")
        );
        assert_eq!(lookup.profile.unwrap().enterprise_number, "0202.239.951");
    }

    #[test]
    fn rejects_success_with_mismatched_or_missing_identity() {
        assert!(parse_reply(REPLY, "0537875391").is_err());
        let xml = REPLY.replace(
            "<dat:Number>0202239951</dat:Number>",
            "<dat:Number></dat:Number>",
        );
        assert!(parse_reply(&xml, "0202239951").is_err());
    }

    #[test]
    fn unknown_number_is_absence_not_error() {
        let xml = REPLY.replace("<mes:Code>00001</mes:Code>", "<mes:Code>00010</mes:Code>");
        let (profile, _) = parse_reply(&xml, "0000000000").expect("absence should parse");
        assert!(profile.is_none());
    }

    #[test]
    fn fault_text_surfaces_without_status() {
        let xml = r#"<?xml version="1.0"?><soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"><soap:Body><soap:Fault><faultcode>soap:Client</faultcode><faultstring>Quota Exceeded!</faultstring></soap:Fault></soap:Body></soap:Envelope>"#;
        let error = parse_reply(xml, "0202239951").expect_err("fault should err");
        assert!(error.to_string().contains("Quota Exceeded!"));
    }

    #[test]
    fn timestamp_formats_known_epochs() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        // 2026-10-23T10:38:47Z per `date -u` during development.
        assert_eq!(format_timestamp(1792751927), "2026-10-23T10:38:47Z");
        assert_eq!(format_timestamp(1767225600), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn digest_matches_independent_python_vector() {
        // python3: base64(sha1(b"test" + b"2025-09-01T11:27:10Z" + b"secret"))
        let nonce = BASE64.decode("dGVzdA==").expect("fixture nonce");
        assert_eq!(
            password_digest(&nonce, "2025-09-01T11:27:10Z", "secret"),
            "VanV3KKdc1mo2c2fQRQulgO9qrk="
        );
    }

    #[test]
    fn digest_matches_ws_security_vector() {
        // nonce "dGVzdA==" decodes to "test"; recompute independently below.
        let nonce = BASE64.decode("dGVzdA==").expect("fixture nonce");
        let digest = password_digest(&nonce, "2025-09-01T11:27:10Z", "secret");
        let mut hasher = Sha1::new();
        hasher.update(b"test");
        hasher.update(b"2025-09-01T11:27:10Z");
        hasher.update(b"secret");
        assert_eq!(digest, BASE64.encode(hasher.finalize()));
    }

    #[test]
    fn request_carries_number_but_no_plaintext_secret() {
        let body = build_request("user", "s3cret", "0202239951");
        assert!(body.contains("<dat:EnterpriseNumber>0202239951</dat:EnterpriseNumber>"));
        assert!(!body.contains("s3cret"));
        assert!(body.contains("PasswordDigest"));
        assert!(body.contains("<mes:Language>nl</mes:Language>"));
    }

    #[test]
    fn credentials_fail_loudly_naming_the_variable() {
        // from_env must name the missing variable rather than defaulting to
        // an anonymous request. Save/restore so ambient developer envs pass.
        let saved_user = std::env::var("KBO_PUB_USER").ok();
        let saved_password = std::env::var("KBO_PUB_PASSWORD").ok();
        std::env::remove_var("KBO_PUB_USER");
        std::env::remove_var("KBO_PUB_PASSWORD");
        let error = LiveCredentials::from_env().expect_err("env should be absent");
        assert!(error.to_string().contains("KBO_PUB_USER"));
        if let Some(value) = saved_user {
            std::env::set_var("KBO_PUB_USER", value);
        }
        if let Some(value) = saved_password {
            std::env::set_var("KBO_PUB_PASSWORD", value);
        }
    }
}
