#[cfg(feature = "network")]
use crate::parse_content_length;
use crate::parse_money;
#[cfg(feature = "network")]
use crate::ted::TedClient;
use crate::ted::{localized, strings, urls};
use crate::{AwardLot, AwardRecord};
use rust_decimal::Decimal;
use serde_json::Value;
use std::error::Error;
#[cfg(feature = "network")]
use std::io::Read;
#[cfg(feature = "network")]
use std::time::Duration;
#[cfg(feature = "network")]
use ureq::AgentBuilder;
use url::Url;

#[cfg(feature = "network")]
const MAX_AWARD_XML_BYTES: usize = 8 * 1024 * 1024;

pub fn award_xml_url(links: &[String]) -> Option<&str> {
    links.iter().find_map(|link| {
        let parsed = Url::parse(link).ok()?;
        let path = parsed.path().trim_end_matches('/');
        (parsed.scheme() == "https"
            && parsed.host_str() == Some("ted.europa.eu")
            && parsed.port_or_known_default() == Some(443)
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && path.ends_with("/xml"))
        .then_some(link.as_str())
    })
}

/// Fetches one award notice's XML.
///
/// The request goes through [`crate::ted::send_with_retry`], so a `429` or a
/// `5xx` — both observed on TED's XML endpoint — is retried instead of dropping
/// the award, which is what used to happen: the caller skipped the notice with a
/// warning, leaving that contract's lots and winners unrecorded. A failed body
/// read or a parse error is not retried, because neither is transient, and
/// redirects stay refused so the URL allow-list upstream keeps its meaning.
#[cfg(feature = "network")]
pub fn fetch_award_xml(url: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let response = crate::ted::send_with_retry(|| {
        AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .redirects(0)
            .build()
            .get(url)
            .set("Accept", "application/xml")
            .call()
    })?;
    if parse_content_length(response.header("Content-Length"))
        .is_some_and(|length| length > MAX_AWARD_XML_BYTES)
    {
        return Err(format!("award XML exceeded the {MAX_AWARD_XML_BYTES} byte limit").into());
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take((MAX_AWARD_XML_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_AWARD_XML_BYTES {
        return Err(format!("award XML exceeded the {MAX_AWARD_XML_BYTES} byte limit").into());
    }
    Ok(body)
}

fn values(value: Option<&Value>) -> Vec<Decimal> {
    match value {
        Some(Value::Array(items)) => items.iter().flat_map(|item| values(Some(item))).collect(),
        Some(Value::Object(object)) => {
            let amount = values(object.get("amount"));
            if amount.is_empty() {
                values(object.get("value"))
            } else {
                amount
            }
        }
        Some(Value::Number(number)) => parse_money(&number.to_string()).into_iter().collect(),
        Some(Value::String(text)) => parse_money(text).into_iter().collect(),
        _ => Vec::new(),
    }
}

pub fn try_notice_to_award(notice: &Value) -> Result<AwardRecord, Box<dyn Error>> {
    let object = notice
        .as_object()
        .ok_or("TED award notice must be a JSON object")?;
    let publication_number = localized(object.get("publication-number"));
    if publication_number.trim().is_empty() {
        return Err("TED award notice has no publication number".into());
    }
    let buyer = localized(object.get("buyer-name"));
    let lot_values = values(object.get("result-value-lot"));
    let award_values = if lot_values.is_empty() {
        values(object.get("result-value-notice"))
    } else {
        lot_values
    };
    Ok(AwardRecord {
        publication_number,
        title: localized(object.get("notice-title")),
        buyer: (!buyer.is_empty()).then_some(buyer),
        winner_identifiers: strings(object.get("winner-identifier")),
        winner_names: strings(object.get("winner-name")),
        cpv_codes: crate::clean_codes(strings(object.get("classification-cpv"))),
        values: award_values,
        links: urls(object.get("links")),
        raw: notice.clone(),
    })
}

pub fn notice_to_award(notice: &Value) -> AwardRecord {
    try_notice_to_award(notice).expect("TED award notice should be valid")
}

pub fn normalize_award_identifier(value: &str) -> String {
    let digits = value
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    if digits.len() == 9 {
        format!("0{digits}")
    } else {
        digits
    }
}

/// Keep only award data attributable to the requested enterprise.
/// Exact lot data is preferred; notice-level values are cleared when the
/// notice contains multiple winners and cannot be safely attributed.
pub fn award_xml_mapping_complete(award: &AwardRecord, lots: &[AwardLot]) -> bool {
    if lots.is_empty()
        || lots.iter().any(|lot| {
            lot.winner_identifier
                .as_deref()
                .is_none_or(|winner| normalize_award_identifier(winner).is_empty())
        })
    {
        return false;
    }
    award.winner_identifiers.iter().all(|winner| {
        lots.iter().any(|lot| {
            lot.winner_identifier.as_deref().is_some_and(|mapped| {
                normalize_award_identifier(mapped) == normalize_award_identifier(winner)
            })
        })
    })
}

pub fn scope_award_to_identifier(
    award: &AwardRecord,
    identifier: &str,
    lots: Option<&[AwardLot]>,
) -> Option<AwardRecord> {
    let target = normalize_award_identifier(identifier);
    if target.is_empty() {
        return None;
    }
    if let Some(lots) = lots {
        let matching = lots
            .iter()
            .filter(|lot| {
                lot.winner_identifier
                    .as_deref()
                    .is_some_and(|winner| normalize_award_identifier(winner) == target)
            })
            .collect::<Vec<_>>();
        if matching.is_empty() {
            return None;
        }
        let mut scoped = award.clone();
        scoped.winner_identifiers = vec![target.clone()];
        scoped.winner_names = matching
            .iter()
            .filter_map(|lot| lot.winner_name.clone())
            .fold(Vec::new(), |mut names, name| {
                if !names.contains(&name) {
                    names.push(name);
                }
                names
            });
        scoped.cpv_codes = matching
            .iter()
            .flat_map(|lot| lot.cpv_codes.iter().cloned())
            .fold(Vec::new(), |mut codes, code| {
                if !codes.contains(&code) {
                    codes.push(code);
                }
                codes
            });
        scoped.values = matching.iter().filter_map(|lot| lot.value).collect();
        return Some(scoped);
    }

    let matching_ids = award
        .winner_identifiers
        .iter()
        .filter(|winner| normalize_award_identifier(winner) == target)
        .count();
    if matching_ids == 0 {
        return None;
    }
    let ambiguous = award
        .winner_identifiers
        .iter()
        .any(|winner| normalize_award_identifier(winner) != target);
    let mut scoped = award.clone();
    scoped.winner_identifiers = vec![target];
    if ambiguous {
        scoped.winner_names.clear();
        scoped.cpv_codes.clear();
        scoped.values.clear();
    }
    Some(scoped)
}

#[cfg(feature = "network")]
fn convert_awards(notices: &[Value]) -> Vec<AwardRecord> {
    notices
        .iter()
        .filter_map(|notice| try_notice_to_award(notice).ok())
        .collect()
}

#[cfg(feature = "network")]
pub fn awards_for_identifier(
    identifier: &str,
    client: &TedClient,
    limit: usize,
) -> Result<Vec<AwardRecord>, Box<dyn Error>> {
    let digits = normalize_award_identifier(identifier);
    if digits.is_empty() {
        return Ok(Vec::new());
    }
    let fields = [
        "publication-number",
        "notice-title",
        "buyer-name",
        "winner-name",
        "winner-identifier",
        "classification-cpv",
        "result-value-lot",
        "result-value-notice",
        "links",
    ];
    // Award history is historical: this must not inherit the importer's
    // recent-publication window, or a company's wins silently disappear.
    Ok(convert_awards(&client.search_raw_window(
        &format!("notice-type=can-standard AND winner-identifier={digits}"),
        &fields,
        limit,
        &crate::ted::SearchWindow::history(),
    )?))
}

#[cfg(all(test, feature = "network"))]
mod retry_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    /// Serves one connection per entry, answering with that status and body.
    fn serve(statuses: Vec<(&'static str, &'static str)>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!(
            "http://{}/notice/1-2026/xml",
            listener.local_addr().unwrap()
        );
        let handle = std::thread::spawn(move || {
            for (status, payload) in statuses {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
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
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                )
                .unwrap();
            }
        });
        (endpoint, handle)
    }

    /// A throttled award XML used to be dropped with a warning, which lost that
    /// contract's lots and winners. It is retried instead.
    #[cfg(feature = "network")]
    #[test]
    fn retries_a_throttled_award_fetch() {
        let (endpoint, server) = serve(vec![
            ("429 Too Many Requests", ""),
            ("200 OK", "<award>ok</award>"),
        ]);
        let body = fetch_award_xml(&endpoint).expect("throttling should be retried");
        server.join().unwrap();
        assert_eq!(body, b"<award>ok</award>");
    }

    /// A rejected request is not worth retrying: the server is asked once.
    #[cfg(feature = "network")]
    #[test]
    fn does_not_retry_a_rejected_award_fetch() {
        let (endpoint, server) = serve(vec![("404 Not Found", "")]);
        let error = fetch_award_xml(&endpoint).expect_err("404 should fail");
        server.join().unwrap();
        assert!(
            error.to_string().contains("404"),
            "expected the status in the error, got: {error}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn audit_null_or_empty_amount_uses_value_fallback() {
        for amount in [Value::Null, json!(""), json!([])] {
            assert_eq!(
                values(Some(&json!({"amount": amount, "value": "125000"}))),
                vec![Decimal::new(125000, 0)]
            );
        }
    }

    #[test]
    fn finds_xml_links_with_query_parameters() {
        let links = vec![
            "https://ted.europa.eu/en/notice/100-2026/html".to_owned(),
            "https://ted.europa.eu/en/notice/100-2026/xml?lang=en".to_owned(),
        ];
        assert_eq!(
            award_xml_url(&links),
            Some("https://ted.europa.eu/en/notice/100-2026/xml?lang=en")
        );
    }

    #[test]
    fn rejects_untrusted_award_xml_links() {
        let links = [
            "http://ted.europa.eu/notice/1/xml".to_owned(),
            "https://example.test/notice/1/xml".to_owned(),
            "https://ted.europa.eu@example.test/notice/1/xml".to_owned(),
        ];
        assert_eq!(award_xml_url(&links), None);
    }

    #[test]
    fn falls_back_to_notice_value_when_lot_values_are_empty() {
        let award = notice_to_award(&json!({
            "publication-number": "100-2026",
            "winner-identifier": ["0123456789"],
            "result-value-lot": [],
            "result-value-notice": ["125000"]
        }));
        assert_eq!(award.values, vec![Decimal::new(125_000, 0)]);
    }

    #[cfg(feature = "network")]
    #[test]
    fn skips_malformed_awards_in_batches() {
        let notices = vec![
            json!({"publication-number": "valid-1"}),
            json!({"winner-identifier": ["0123456789"]}),
        ];
        let awards = convert_awards(&notices);
        assert_eq!(awards.len(), 1);
        assert_eq!(awards[0].publication_number, "valid-1");
    }

    #[test]
    fn normalizes_award_fields() {
        let award = notice_to_award(&json!({
            "publication-number": "100-2026",
            "notice-title": {"eng": "IT services"},
            "buyer-name": {"nld": ["City"]},
            "winner-name": {"nld": ["Example BV"]},
            "winner-identifier": ["0123456789"],
            "classification-cpv": ["72000000"],
            "result-value-lot": ["125000"]
        }));
        assert_eq!(award.publication_number, "100-2026");
        assert_eq!(award.winner_names, vec!["Example BV"]);
        assert_eq!(award.winner_identifiers, vec!["0123456789"]);
        assert_eq!(award.values, vec![Decimal::new(125_000, 0)]);
    }

    #[test]
    fn xml_mapping_requires_all_notice_winners() {
        let award = notice_to_award(&json!({
            "publication-number": "101-2026",
            "winner-identifier": ["0123456789", "0999999999"]
        }));
        let partial = vec![AwardLot {
            publication_number: "101-2026".to_owned(),
            lot_id: "LOT-1".to_owned(),
            winner_identifier: Some("0123456789".to_owned()),
            winner_name: Some("Example BV".to_owned()),
            value: None,
            lowest_value: None,
            highest_value: None,
            cpv_codes: vec![],
        }];
        let mut complete = partial.clone();
        complete.push(AwardLot {
            publication_number: "101-2026".to_owned(),
            lot_id: "LOT-2".to_owned(),
            winner_identifier: Some("0999999999".to_owned()),
            winner_name: Some("Other BV".to_owned()),
            value: None,
            lowest_value: None,
            highest_value: None,
            cpv_codes: vec![],
        });
        assert!(!award_xml_mapping_complete(&award, &partial));
        assert!(award_xml_mapping_complete(&award, &complete));
    }

    #[test]
    fn scopes_legacy_nine_digit_notice_identifier() {
        let award = notice_to_award(&json!({
            "publication-number": "100-2026",
            "winner-identifier": ["123456789"],
            "winner-name": ["Example BV"],
            "classification-cpv": ["72000000"],
            "result-value-notice": ["125000"]
        }));
        let scoped = scope_award_to_identifier(&award, "0123.456.789", None)
            .expect("legacy winner identifier should match");
        assert_eq!(scoped.winner_identifiers, vec!["0123456789"]);
        assert_eq!(scoped.values, vec![Decimal::new(125_000, 0)]);
    }

    #[test]
    fn scopes_notice_award_to_exact_lots() {
        let award = notice_to_award(&json!({
            "publication-number": "101-2026",
            "winner-identifier": ["0123456789", "0999999999"],
            "winner-name": ["Example BV", "Other BV"],
            "classification-cpv": ["72000000", "45000000"],
            "result-value-lot": ["50000", "5000000"]
        }));
        let lots = vec![
            AwardLot {
                publication_number: "101-2026".to_owned(),
                lot_id: "LOT-1".to_owned(),
                winner_identifier: Some("123456789".to_owned()),
                winner_name: Some("Example BV".to_owned()),
                value: Some(Decimal::new(50_000, 0)),
                lowest_value: None,
                highest_value: None,
                cpv_codes: vec!["72000000".to_owned()],
            },
            AwardLot {
                publication_number: "101-2026".to_owned(),
                lot_id: "LOT-2".to_owned(),
                winner_identifier: Some("0999999999".to_owned()),
                winner_name: Some("Other BV".to_owned()),
                value: Some(Decimal::new(5_000_000, 0)),
                lowest_value: None,
                highest_value: None,
                cpv_codes: vec!["45000000".to_owned()],
            },
        ];
        let scoped = scope_award_to_identifier(&award, "0123.456.789", Some(&lots))
            .expect("matching lot should produce an award");
        assert_eq!(scoped.values, vec![Decimal::new(50_000, 0)]);
        assert_eq!(scoped.cpv_codes, vec!["72000000"]);
    }

    #[test]
    fn rejects_malformed_award_notice() {
        assert!(try_notice_to_award(&json!({"winner-name": ["Example"]})).is_err());
        assert!(try_notice_to_award(&json!("not an object")).is_err());
    }
}
