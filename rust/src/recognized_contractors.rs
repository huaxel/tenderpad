//! Import FPS Economy recognized-contractor workbook exports as provenance-rich snapshots.
//!
//! Rows remain approval-level records: one enterprise may hold multiple approvals.
//! This module reports exact KBO joins against a named local snapshot; it does not
//! decide whether a contractor is currently eligible for a particular tender.

use crate::kbo::KboRepository;
use crate::recognition::RecognitionStatement;
use crate::recognition_catalog::{self, ClassAmountGuidance};
use calamine::{open_workbook_auto, Data, DataType, Reader};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

const REQUIRED_HEADERS: &[&str] = &[
    "N° enterprise",
    "VAT number",
    "Enterprise name",
    "Approval number",
    "Street",
    "Postcode",
    "Municipality",
    "Decision date",
    "Expiration date",
    "Category/Class",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecognizedApproval {
    pub source_row: usize,
    pub validity_as_of: ApprovalValidity,
    pub enterprise_number_raw: Option<String>,
    pub enterprise_number_normalized: Option<String>,
    pub vat_number: Option<String>,
    pub enterprise_name: Option<String>,
    pub approval_number: Option<String>,
    pub street: Option<String>,
    pub postcode: Option<String>,
    pub municipality: Option<String>,
    pub decision_date_raw: Option<String>,
    pub expiration_date_raw: Option<String>,
    pub category_class_raw: Option<String>,
    pub category_class_parse_status: CategoryClassParseStatus,
    pub recognition_credentials: Vec<RecognitionCredential>,
    pub kbo_match: KboMatch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalValidity {
    CurrentAsOf,
    ExpiredAsOf,
    NotYetEffective,
    UnknownDecisionDate,
    UnknownExpiry,
    InvalidDate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecognitionCredential {
    pub category: String,
    pub category_description_nl: Option<String>,
    pub class: u8,
    pub class_amount_guidance: Option<ClassAmountGuidance>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecognitionEvidenceStatus {
    NoRequirement,
    RequirementNotStructuredEnough,
    CurrentDatedApprovalFound,
    ApprovalFoundButExpiryUnknown,
    ExpiredApprovalFound,
    FutureDatedApprovalFound,
    ApprovalFoundWithUncertainDates,
    NoMatchingApprovalInSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecognitionEvidence {
    pub source_row: usize,
    pub approval_number: Option<String>,
    pub kbo_match: KboMatch,
    pub category: String,
    pub category_description_nl: Option<String>,
    pub class: u8,
    pub class_amount_guidance: Option<ClassAmountGuidance>,
    pub validity_as_of: ApprovalValidity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecognitionComparison {
    pub required_category: Option<String>,
    pub required_class: Option<u8>,
    pub status: RecognitionEvidenceStatus,
    pub evidence: Vec<RecognitionEvidence>,
}

/// Compare only requirements that reduce to one category and one class.
/// Multi-category/class requirements need lot-specific pairing before comparison.
/// This reports register evidence, never legal eligibility. `approvals` must
/// contain records for one enterprise, selected by the caller using exact IDs.
pub fn compare_requirement_to_approvals(
    statement: &RecognitionStatement,
    approvals: &[RecognizedApproval],
) -> RecognitionComparison {
    if !statement.mentioned {
        return RecognitionComparison {
            required_category: None,
            required_class: None,
            status: RecognitionEvidenceStatus::NoRequirement,
            evidence: Vec::new(),
        };
    }
    if statement.categories.len() != 1 || statement.classes.len() != 1 {
        return RecognitionComparison {
            required_category: None,
            required_class: None,
            status: RecognitionEvidenceStatus::RequirementNotStructuredEnough,
            evidence: Vec::new(),
        };
    }
    let category = statement.categories[0].to_ascii_uppercase();
    let class = statement.classes[0];
    let required_category = category.as_str();
    let evidence: Vec<_> = approvals
        .iter()
        .filter(|approval| approval.kbo_match.is_exact())
        .flat_map(|approval| {
            approval
                .recognition_credentials
                .iter()
                .filter(move |credential| {
                    related_recognition_categories(required_category, &credential.category)
                        && credential.class >= class
                })
                .map(move |credential| RecognitionEvidence {
                    source_row: approval.source_row,
                    approval_number: approval.approval_number.clone(),
                    kbo_match: approval.kbo_match.clone(),
                    category: credential.category.clone(),
                    category_description_nl: credential.category_description_nl.clone(),
                    class: credential.class,
                    class_amount_guidance: credential.class_amount_guidance,
                    validity_as_of: approval.validity_as_of.clone(),
                })
        })
        .collect();
    let status = if evidence
        .iter()
        .any(|item| item.validity_as_of == ApprovalValidity::CurrentAsOf)
    {
        RecognitionEvidenceStatus::CurrentDatedApprovalFound
    } else if evidence
        .iter()
        .any(|item| item.validity_as_of == ApprovalValidity::UnknownExpiry)
    {
        RecognitionEvidenceStatus::ApprovalFoundButExpiryUnknown
    } else if evidence
        .iter()
        .any(|item| item.validity_as_of == ApprovalValidity::ExpiredAsOf)
    {
        RecognitionEvidenceStatus::ExpiredApprovalFound
    } else if evidence
        .iter()
        .any(|item| item.validity_as_of == ApprovalValidity::NotYetEffective)
    {
        RecognitionEvidenceStatus::FutureDatedApprovalFound
    } else if evidence.iter().any(|item| {
        matches!(
            item.validity_as_of,
            ApprovalValidity::UnknownDecisionDate | ApprovalValidity::InvalidDate
        )
    }) {
        RecognitionEvidenceStatus::ApprovalFoundWithUncertainDates
    } else {
        RecognitionEvidenceStatus::NoMatchingApprovalInSnapshot
    };
    RecognitionComparison {
        required_category: Some(category),
        required_class: Some(class),
        status,
        evidence,
    }
}

fn related_recognition_categories(required: &str, approved: &str) -> bool {
    required.starts_with(approved) || approved.starts_with(required)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CategoryClassParseStatus {
    Complete,
    Missing,
    Partial,
    Unrecognized,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KboMatch {
    ExactLocalSnapshot,
    ExactLiveApi {
        verified_at_utc: String,
        current_status: Option<String>,
    },
    LiveNotFound {
        verified_at_utc: String,
    },
    LiveDifferentId {
        verified_at_utc: String,
        returned_enterprise_number: String,
    },
    LiveLookupFailed {
        verified_at_utc: String,
    },
    UnmatchedLocalSnapshot,
    MissingIdentifier,
    InvalidIdentifier {
        reason: String,
    },
}

impl KboMatch {
    pub fn is_exact(&self) -> bool {
        matches!(self, Self::ExactLocalSnapshot | Self::ExactLiveApi { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecognitionSnapshot {
    pub schema_version: u32,
    pub generated_at_utc: String,
    pub source_file: String,
    pub source_sha256: String,
    pub source_worksheet: String,
    pub kbo_database: String,
    pub kbo_snapshot_as_of: String,
    pub register_as_of: String,
    pub approval_rows: Vec<RecognizedApproval>,
    pub report: JoinReport,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JoinReport {
    pub approval_rows: usize,
    pub distinct_ids_joinable: usize,
    pub distinct_ids_invalid_format: usize,
    pub rows_missing_identifier: usize,
    pub rows_with_invalid_identifier: usize,
    pub distinct_ids_exactly_matched: usize,
    pub distinct_ids_unmatched: usize,
    pub rows_exactly_matched: usize,
    pub repeated_enterprise_rows: usize,
    pub category_class_rows_complete: usize,
    pub category_class_rows_unparsed: usize,
    pub parsed_credential_count: usize,
    pub credentials_with_category_description: usize,
    pub credentials_without_category_description: usize,
    pub approvals_current_as_of: usize,
    pub approvals_expired_as_of: usize,
    pub approvals_not_yet_effective: usize,
    pub approvals_unknown_expiry: usize,
    pub approvals_unknown_decision_date: usize,
    pub approvals_with_invalid_date: usize,
}

fn parse_register_date(raw: Option<&str>) -> Result<Option<chrono::NaiveDate>, ()> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    ["%d/%m/%Y", "%Y-%m-%d"]
        .iter()
        .find_map(|format| chrono::NaiveDate::parse_from_str(raw, format).ok())
        .map(Some)
        .ok_or(())
}

fn approval_validity(
    decision_date: Option<&str>,
    expiration_date: Option<&str>,
    as_of: chrono::NaiveDate,
) -> ApprovalValidity {
    let decision = match parse_register_date(decision_date) {
        Ok(date) => date,
        Err(()) => return ApprovalValidity::InvalidDate,
    };
    let expiration = match parse_register_date(expiration_date) {
        Ok(date) => date,
        Err(()) => return ApprovalValidity::InvalidDate,
    };
    if decision.is_some_and(|date| date > as_of) {
        return ApprovalValidity::NotYetEffective;
    }
    if expiration.is_some() && decision.is_none() {
        return ApprovalValidity::UnknownDecisionDate;
    }
    match expiration {
        Some(date) if date < as_of => ApprovalValidity::ExpiredAsOf,
        Some(_) => ApprovalValidity::CurrentAsOf,
        None => ApprovalValidity::UnknownExpiry,
    }
}

fn category_class_regex() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)([A-HKLMNPSTUV][0-9]{0,2})\s*\(\s*([1-8])\s*\)")
            .expect("static category/class expression is valid")
    })
}

fn parse_category_class(
    raw: Option<&str>,
) -> (CategoryClassParseStatus, Vec<RecognitionCredential>) {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return (CategoryClassParseStatus::Missing, Vec::new());
    };
    let pattern = category_class_regex();
    let credentials: Vec<_> = pattern
        .captures_iter(raw)
        .filter_map(|capture| {
            let category = capture.get(1)?.as_str().to_ascii_uppercase();
            let class = capture.get(2)?.as_str().parse().ok()?;
            Some(RecognitionCredential {
                category_description_nl: recognition_catalog::category(&category)
                    .map(|item| item.description_nl.to_owned()),
                class_amount_guidance: recognition_catalog::class_amount_guidance(class).copied(),
                category,
                class,
            })
        })
        .collect();
    if credentials.is_empty() {
        return (CategoryClassParseStatus::Unrecognized, credentials);
    }
    let remainder = pattern.replace_all(raw, "");
    if remainder
        .trim_matches(|character: char| character.is_whitespace() || ",;/|".contains(character))
        .is_empty()
    {
        (CategoryClassParseStatus::Complete, credentials)
    } else {
        (CategoryClassParseStatus::Partial, credentials)
    }
}

fn cell_text(cell: &Data) -> Option<String> {
    let value = cell.to_string();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Normalize punctuation only. Unlike KBO lookup normalization, never pad a
/// short identifier: that would turn an uncertain source value into a match.
fn exact_enterprise_id(raw: &str) -> Result<String, String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 10 {
        return Err(format!("expected 10 digits, found {}", digits.len()));
    }
    Ok(format!(
        "{}.{}.{}",
        &digits[..4],
        &digits[4..7],
        &digits[7..]
    ))
}

pub fn build_snapshot(
    workbook_path: &Path,
    kbo_database: &Path,
    kbo_snapshot_as_of: &str,
    register_as_of: chrono::NaiveDate,
) -> Result<RecognitionSnapshot, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(workbook_path)?;
    let source_sha256 = format!("{:x}", Sha256::digest(&bytes));
    let mut workbook = open_workbook_auto(workbook_path)?;
    let worksheet = workbook
        .sheet_names()
        .first()
        .cloned()
        .ok_or("workbook has no worksheets")?;
    let range = workbook.worksheet_range(&worksheet)?;
    let mut rows = range.rows();
    let header = rows.next().ok_or("worksheet is empty")?;
    let positions: BTreeMap<String, usize> = header
        .iter()
        .enumerate()
        .filter_map(|(index, cell)| cell_text(cell).map(|name| (name, index)))
        .collect();
    let missing: Vec<_> = REQUIRED_HEADERS
        .iter()
        .filter(|name| !positions.contains_key(**name))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "worksheet {worksheet:?} missing required columns: {}",
            missing.join(", ")
        )
        .into());
    }
    let field = |row: &[Data], name: &str| -> Option<String> {
        positions
            .get(name)
            .and_then(|index| row.get(*index))
            .and_then(cell_text)
    };

    let mut source_rows = Vec::new();
    let mut ids = Vec::new();
    for (offset, row) in rows.enumerate() {
        if row.iter().all(|cell| cell.is_empty()) {
            continue;
        }
        let raw_id = field(row, "N° enterprise");
        if let Some(value) = raw_id
            .as_deref()
            .and_then(|raw| exact_enterprise_id(raw).ok())
        {
            ids.push(value);
        }
        source_rows.push((offset + 2, row.to_vec(), raw_id));
    }

    let repository = KboRepository::open(kbo_database)?;
    let profiles = repository.lookup_batch(&ids)?;
    let matched: HashSet<String> = profiles
        .into_iter()
        .map(|profile| profile.enterprise_number)
        .collect();
    let mut approvals = Vec::with_capacity(source_rows.len());
    let mut all_ids = HashSet::new();
    let mut matched_ids = HashSet::new();
    let mut unmatched_ids = HashSet::new();
    let mut invalid_ids = HashSet::new();
    let mut missing_rows = 0;
    let mut invalid_rows = 0;
    let mut matched_rows = 0;
    let mut id_row_counts = BTreeMap::<String, usize>::new();

    for (source_row, row, raw_id) in source_rows {
        let normalized = raw_id.as_deref().map(exact_enterprise_id);
        let kbo_match = match normalized {
            None => {
                missing_rows += 1;
                KboMatch::MissingIdentifier
            }
            Some(Err(reason)) => {
                invalid_rows += 1;
                invalid_ids.insert(
                    raw_id
                        .as_deref()
                        .unwrap_or_default()
                        .chars()
                        .filter(char::is_ascii_digit)
                        .collect::<String>(),
                );
                KboMatch::InvalidIdentifier { reason }
            }
            Some(Ok(ref id)) => {
                all_ids.insert(id.clone());
                *id_row_counts.entry(id.clone()).or_default() += 1;
                if matched.contains(id) {
                    matched_rows += 1;
                    matched_ids.insert(id.clone());
                    KboMatch::ExactLocalSnapshot
                } else {
                    unmatched_ids.insert(id.clone());
                    KboMatch::UnmatchedLocalSnapshot
                }
            }
        };
        let normalized_id = raw_id
            .as_deref()
            .and_then(|raw| exact_enterprise_id(raw).ok());
        let category_class_raw = field(&row, "Category/Class");
        let (category_class_parse_status, recognition_credentials) =
            parse_category_class(category_class_raw.as_deref());
        let decision_date_raw = field(&row, "Decision date");
        let expiration_date_raw = field(&row, "Expiration date");
        approvals.push(RecognizedApproval {
            source_row,
            validity_as_of: approval_validity(
                decision_date_raw.as_deref(),
                expiration_date_raw.as_deref(),
                register_as_of,
            ),
            enterprise_number_raw: raw_id,
            enterprise_number_normalized: normalized_id,
            vat_number: field(&row, "VAT number"),
            enterprise_name: field(&row, "Enterprise name"),
            approval_number: field(&row, "Approval number"),
            street: field(&row, "Street"),
            postcode: field(&row, "Postcode"),
            municipality: field(&row, "Municipality"),
            decision_date_raw,
            expiration_date_raw,
            category_class_raw,
            category_class_parse_status,
            recognition_credentials,
            kbo_match,
        });
    }

    let report = JoinReport {
        approval_rows: approvals.len(),
        distinct_ids_joinable: all_ids.len(),
        distinct_ids_invalid_format: invalid_ids.len(),
        rows_missing_identifier: missing_rows,
        rows_with_invalid_identifier: invalid_rows,
        distinct_ids_exactly_matched: matched_ids.len(),
        distinct_ids_unmatched: unmatched_ids.len(),
        rows_exactly_matched: matched_rows,
        repeated_enterprise_rows: id_row_counts
            .values()
            .map(|count| count.saturating_sub(1))
            .sum(),
        category_class_rows_complete: approvals
            .iter()
            .filter(|row| row.category_class_parse_status == CategoryClassParseStatus::Complete)
            .count(),
        category_class_rows_unparsed: approvals
            .iter()
            .filter(|row| {
                matches!(
                    row.category_class_parse_status,
                    CategoryClassParseStatus::Partial | CategoryClassParseStatus::Unrecognized
                )
            })
            .count(),
        parsed_credential_count: approvals
            .iter()
            .map(|row| row.recognition_credentials.len())
            .sum(),
        credentials_with_category_description: approvals
            .iter()
            .flat_map(|row| &row.recognition_credentials)
            .filter(|credential| credential.category_description_nl.is_some())
            .count(),
        credentials_without_category_description: approvals
            .iter()
            .flat_map(|row| &row.recognition_credentials)
            .filter(|credential| credential.category_description_nl.is_none())
            .count(),
        approvals_current_as_of: approvals
            .iter()
            .filter(|row| row.validity_as_of == ApprovalValidity::CurrentAsOf)
            .count(),
        approvals_expired_as_of: approvals
            .iter()
            .filter(|row| row.validity_as_of == ApprovalValidity::ExpiredAsOf)
            .count(),
        approvals_not_yet_effective: approvals
            .iter()
            .filter(|row| row.validity_as_of == ApprovalValidity::NotYetEffective)
            .count(),
        approvals_unknown_expiry: approvals
            .iter()
            .filter(|row| row.validity_as_of == ApprovalValidity::UnknownExpiry)
            .count(),
        approvals_unknown_decision_date: approvals
            .iter()
            .filter(|row| row.validity_as_of == ApprovalValidity::UnknownDecisionDate)
            .count(),
        approvals_with_invalid_date: approvals
            .iter()
            .filter(|row| row.validity_as_of == ApprovalValidity::InvalidDate)
            .count(),
    };
    Ok(RecognitionSnapshot {
        schema_version: 3,
        generated_at_utc: chrono::Utc::now().to_rfc3339(),
        source_file: workbook_path.display().to_string(),
        source_sha256,
        source_worksheet: worksheet,
        kbo_database: kbo_database.display().to_string(),
        kbo_snapshot_as_of: kbo_snapshot_as_of.to_owned(),
        register_as_of: register_as_of.to_string(),
        approval_rows: approvals,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_id_normalization_strips_punctuation_but_never_pads() {
        assert_eq!(
            exact_enterprise_id("0.402.123.456").unwrap(),
            "0402.123.456"
        );
        assert_eq!(exact_enterprise_id("0402123456").unwrap(), "0402.123.456");
        assert!(exact_enterprise_id("402123456").is_err());
        assert!(exact_enterprise_id("four zero two").is_err());
    }

    fn credential(category: &str, class: u8) -> RecognitionCredential {
        RecognitionCredential {
            category: category.into(),
            category_description_nl: recognition_catalog::category(category)
                .map(|item| item.description_nl.to_owned()),
            class,
            class_amount_guidance: recognition_catalog::class_amount_guidance(class).copied(),
        }
    }

    #[test]
    fn parses_multiple_category_class_pairs_without_collapsing_them() {
        let (status, credentials) = parse_category_class(Some("C (1) C1 (1) D (8) D4 (1)"));
        assert_eq!(status, CategoryClassParseStatus::Complete);
        assert_eq!(
            credentials,
            vec![
                credential("C", 1),
                credential("C1", 1),
                credential("D", 8),
                credential("D4", 1)
            ]
        );
    }

    #[test]
    fn partial_or_missing_category_class_text_is_not_called_complete() {
        assert_eq!(
            parse_category_class(Some("C (7) unknown")).0,
            CategoryClassParseStatus::Partial
        );
        assert_eq!(
            parse_category_class(Some("unrecognized")).0,
            CategoryClassParseStatus::Unrecognized
        );
        assert_eq!(
            parse_category_class(None).0,
            CategoryClassParseStatus::Missing
        );
    }

    fn requirement(mentioned: bool, categories: &[&str], classes: &[u8]) -> RecognitionStatement {
        RecognitionStatement {
            source: None,
            lot_aggregation_needs_review: false,
            mentioned,
            categories: categories.iter().map(|value| (*value).to_owned()).collect(),
            classes: classes.to_vec(),
        }
    }

    fn approval_with(category: &str, class: u8, validity: ApprovalValidity) -> RecognizedApproval {
        RecognizedApproval {
            source_row: 12,
            validity_as_of: validity,
            enterprise_number_raw: Some("0402123456".into()),
            enterprise_number_normalized: Some("0402.123.456".into()),
            vat_number: None,
            enterprise_name: Some("Example".into()),
            approval_number: Some("A-12".into()),
            street: None,
            postcode: None,
            municipality: None,
            decision_date_raw: Some("01/01/2025".into()),
            expiration_date_raw: None,
            category_class_raw: Some(format!("{category} ({class})")),
            category_class_parse_status: CategoryClassParseStatus::Complete,
            recognition_credentials: vec![credential(category, class)],
            kbo_match: KboMatch::ExactLocalSnapshot,
        }
    }

    #[test]
    fn compares_only_one_category_and_class_and_returns_source_evidence() {
        let statement = requirement(true, &["C"], &[7]);
        let approval = approval_with("C1", 8, ApprovalValidity::CurrentAsOf);
        let result = compare_requirement_to_approvals(&statement, &[approval]);
        assert_eq!(
            result.status,
            RecognitionEvidenceStatus::CurrentDatedApprovalFound
        );
        assert_eq!(result.required_category.as_deref(), Some("C"));
        assert_eq!(result.required_class, Some(7));
        assert_eq!(result.evidence[0].source_row, 12);
        assert_eq!(result.evidence[0].approval_number.as_deref(), Some("A-12"));
        assert_eq!(
            result.evidence[0].category_description_nl.as_deref(),
            Some("Gewone rioleringswerken")
        );
        assert_eq!(
            result.evidence[0]
                .class_amount_guidance
                .unwrap()
                .above_amount_eur,
            Some(5_330_000)
        );
    }

    #[test]
    fn live_exact_identity_is_accepted_and_its_provenance_is_preserved() {
        let statement = requirement(true, &["C"], &[7]);
        let mut approval = approval_with("C", 8, ApprovalValidity::CurrentAsOf);
        approval.kbo_match = KboMatch::ExactLiveApi {
            verified_at_utc: "2026-10-31T10:00:00Z".into(),
            current_status: Some("AC".into()),
        };
        let result = compare_requirement_to_approvals(&statement, &[approval]);
        assert_eq!(
            result.status,
            RecognitionEvidenceStatus::CurrentDatedApprovalFound
        );
        assert!(matches!(
            &result.evidence[0].kbo_match,
            KboMatch::ExactLiveApi { .. }
        ));
    }

    #[test]
    fn comparisons_keep_missing_and_uncertain_evidence_nonconclusive() {
        let statement = requirement(true, &["C"], &[7]);
        let unrelated = approval_with("D", 8, ApprovalValidity::CurrentAsOf);
        assert_eq!(
            compare_requirement_to_approvals(&statement, &[unrelated]).status,
            RecognitionEvidenceStatus::NoMatchingApprovalInSnapshot
        );
        let mut unmatched = approval_with("C", 8, ApprovalValidity::CurrentAsOf);
        unmatched.kbo_match = KboMatch::UnmatchedLocalSnapshot;
        assert_eq!(
            compare_requirement_to_approvals(&statement, &[unmatched]).status,
            RecognitionEvidenceStatus::NoMatchingApprovalInSnapshot
        );
        let mut different_live_id = approval_with("C", 8, ApprovalValidity::CurrentAsOf);
        different_live_id.kbo_match = KboMatch::LiveDifferentId {
            verified_at_utc: "2026-10-31T10:00:00Z".into(),
            returned_enterprise_number: "0402.999.999".into(),
        };
        assert_eq!(
            compare_requirement_to_approvals(&statement, &[different_live_id]).status,
            RecognitionEvidenceStatus::NoMatchingApprovalInSnapshot
        );
        let unknown = approval_with("C", 8, ApprovalValidity::UnknownExpiry);
        assert_eq!(
            compare_requirement_to_approvals(&statement, &[unknown]).status,
            RecognitionEvidenceStatus::ApprovalFoundButExpiryUnknown
        );
    }

    #[test]
    fn multi_category_requirements_are_not_guessed_into_pairs() {
        assert_eq!(
            compare_requirement_to_approvals(&requirement(true, &["C", "D"], &[3, 8]), &[]).status,
            RecognitionEvidenceStatus::RequirementNotStructuredEnough
        );
        assert_eq!(
            compare_requirement_to_approvals(&requirement(false, &[], &[]), &[]).status,
            RecognitionEvidenceStatus::NoRequirement
        );
    }

    #[test]
    fn approval_validity_keeps_unknown_expiry_separate_from_current() {
        let as_of = chrono::NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        assert_eq!(
            approval_validity(Some("01/01/2025"), Some("30/10/2026"), as_of),
            ApprovalValidity::CurrentAsOf
        );
        assert_eq!(
            approval_validity(Some("01/01/2025"), Some("29/10/2026"), as_of),
            ApprovalValidity::ExpiredAsOf
        );
        assert_eq!(
            approval_validity(Some("31/10/2026"), Some("31/12/2027"), as_of),
            ApprovalValidity::NotYetEffective
        );
        assert_eq!(
            approval_validity(Some("01/01/2025"), None, as_of),
            ApprovalValidity::UnknownExpiry
        );
        assert_eq!(
            approval_validity(None, Some("31/12/2027"), as_of),
            ApprovalValidity::UnknownDecisionDate
        );
        assert_eq!(
            approval_validity(Some("not a date"), Some("31/12/2027"), as_of),
            ApprovalValidity::InvalidDate
        );
    }

    #[test]
    fn absent_expiry_stays_absent_and_not_a_validity_claim() {
        let approval = RecognizedApproval {
            source_row: 2,
            validity_as_of: ApprovalValidity::UnknownExpiry,
            enterprise_number_raw: Some("0402123456".into()),
            enterprise_number_normalized: Some("0402.123.456".into()),
            vat_number: None,
            enterprise_name: Some("Example".into()),
            approval_number: Some("A-1".into()),
            street: None,
            postcode: None,
            municipality: None,
            decision_date_raw: None,
            expiration_date_raw: None,
            category_class_raw: Some("V (8)".into()),
            category_class_parse_status: CategoryClassParseStatus::Complete,
            recognition_credentials: vec![credential("V", 8)],
            kbo_match: KboMatch::ExactLocalSnapshot,
        };
        assert!(approval.expiration_date_raw.is_none());
        let json = serde_json::to_value(approval).unwrap();
        assert!(json["expiration_date_raw"].is_null());
    }
}
