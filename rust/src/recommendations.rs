use crate::analysis::AnalysisDecision;
use crate::requirements::{analyze_requirements, RequirementSignal};
use crate::{
    match_company_to_tender_on, AwardRecord, CompanyPreferences, CompanyProfile, Tender,
    TenderRequirement,
};
use chrono::{Duration, NaiveDate};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const MAX_RECOMMENDATION_CANDIDATES: usize = 100_000;

/// Candidate window a recommendation request uses when the caller does not ask
/// for one.
pub const DEFAULT_RECOMMENDATION_CANDIDATES: usize = 10_000;

// Score fusion policy for deep reranks: a strong analysis relevance adds
// this bonus (capped at 100), a poor one subtracts this penalty (floored at
// 0); any other relevance only records the reason.
pub const JEV_STRONG_BONUS: f64 = 15.0;
pub const JEV_POOR_PENALTY: f64 = 15.0;

/// Fuse one cached or fresh analysis decision into a recommendation item.
pub fn apply_jev_decision(item: &mut Recommendation, decision: &AnalysisDecision) {
    item.jev_relevance = Some(decision.relevance.clone());
    item.jev_qualification_risk = Some(decision.qualification_risk.clone());
    if decision.relevance.as_deref() == Some("strong") {
        item.score = (item.score + JEV_STRONG_BONUS).min(100.0);
    } else if decision.relevance.as_deref() == Some("poor") {
        item.score = (item.score - JEV_POOR_PENALTY).max(0.0);
    }
    item.reasons.push(format!(
        "Jev relevance: {}",
        decision.relevance.as_deref().unwrap_or("None")
    ));
}

/// Qualification evidence states carried by a notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FeasibilityStatus {
    Stated,
    Hinted,
    Unknown,
}

impl FeasibilityStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stated => "stated",
            Self::Hinted => "hinted",
            Self::Unknown => "unknown",
        }
    }
}

/// What the notice actually says about the company's qualification.
///
/// `Unknown` is the common case and is nobody's fault: most TED notices carry no
/// selection criteria, so nothing can be checked against the company. Measured on
/// the 6,035-row store: 7 of a company's top 10 rows had no evidence at all, yet
/// were presented with a score and "activity matches" as the only reason. Absence
/// of evidence must not read as absence of risk, so it is stated.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeasibilityEvidence {
    /// `Stated` — mandatory criteria were extracted; `Hinted` — no criterion was
    /// extracted but the notice text raises a qualification risk; `Unknown` —
    /// neither, so the ladder knows nothing about qualification.
    pub status: FeasibilityStatus,
    /// Mandatory criteria extracted from the notice, verbatim in `requirements`.
    pub mandatory_requirements: usize,
    /// The dominant risk from `qualification_risk`, from the same term tables as
    /// the warnings.
    pub qualification_risk: &'static str,
}

/// Classify the qualification evidence a notice carries for this company.
pub fn feasibility_evidence(tender: &Tender, history: &[AwardRecord]) -> FeasibilityEvidence {
    let mandatory_requirements = tender
        .requirements
        .iter()
        .filter(|requirement| requirement.mandatory)
        .count();
    let qualification_risk = crate::qualification_risk(tender, history);
    let status = if mandatory_requirements > 0 {
        FeasibilityStatus::Stated
    } else if qualification_risk != "none_obvious" {
        FeasibilityStatus::Hinted
    } else {
        FeasibilityStatus::Unknown
    };
    FeasibilityEvidence {
        status,
        mandatory_requirements,
        qualification_risk,
    }
}

/// Warning shown when the notice says nothing about qualification, so the advice
/// cannot be read as "you qualify".
pub const UNSTATED_FEASIBILITY_WARNING: &str =
    "The notice states no selection criteria: qualification requirements are unknown, check the tender documents";

/// Corpus evidence snapshot: time-invariant properties of the stored rows
/// (notice-type eligibility, field presence, qualification-evidence mix).
/// Deadline-relative openness is deliberately excluded — it goes stale, so
/// `/health` keeps computing it live per request while this snapshot is
/// recorded once per refresh and served as-of that run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CorpusEvidence {
    pub opportunities: usize,
    pub with_deadline: usize,
    pub with_value: usize,
    pub with_requirements: usize,
    pub stated: usize,
    pub hinted: usize,
    pub unknown: usize,
}

/// Evidence mix over rankable notices, with an empty history: corpus QC, not
/// a company judgement; per-company rows carry their own feasibility.
/// Shared by `report` (live, audit-grade) and `ted-ingest` (stored per run),
/// so the two can never disagree on the definition — only on the as-of time.
pub fn corpus_evidence(tenders: &[Tender]) -> CorpusEvidence {
    let mut evidence = CorpusEvidence::default();
    for tender in tenders.iter().filter(|tender| tender.is_opportunity()) {
        evidence.opportunities += 1;
        if tender.deadline.is_some() {
            evidence.with_deadline += 1;
        }
        if tender.estimated_value.is_some() {
            evidence.with_value += 1;
        }
        if !tender.requirements.is_empty() {
            evidence.with_requirements += 1;
        }
        match feasibility_evidence(tender, &[]).status {
            FeasibilityStatus::Stated => evidence.stated += 1,
            FeasibilityStatus::Hinted => evidence.hinted += 1,
            FeasibilityStatus::Unknown => evidence.unknown += 1,
        }
    }
    evidence
}

fn reported_framework_maximum_amounts(tender: &Tender) -> Vec<String> {
    tender
        .raw
        .get("framework_maximum_amounts")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let source_value = value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_number().map(ToString::to_string))?;
            crate::parse_money(&source_value)
                .filter(|amount| *amount > Decimal::ZERO)
                .map(|amount| amount.to_string())
        })
        .collect()
}

fn framework_maximum_warning(amounts: &[String]) -> Option<String> {
    match amounts.len() {
        0 => None,
        1 => Some(
            "A parseable framework maximum amount is available; it is a ceiling, not estimated spend. Currency and lot scope may be available in source details; no currency conversion or ranking effect is applied"
                .to_owned(),
        ),
        count => Some(format!(
            "{count} parseable positive framework maximum amounts are available; they are ceiling declarations, not estimated spend. Currency and lot scope are shown in source details when retained; do not sum amounts without checking the notice"
        )),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Recommendation {
    pub source: String,
    pub external_id: String,
    pub title: String,
    pub buyer: Option<String>,
    pub deadline: Option<String>,
    pub estimated_value: Option<Decimal>,
    /// Compact source-reported ceiling summary, kept separate from estimated
    /// spend. Per-declaration currency and scope are available in raw detail
    /// evidence when retained; this flat list does not encode those associations.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub framework_maximum_amounts: Vec<String>,
    pub regions: Vec<String>,
    pub cpv_codes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lot_cpv_alignment: Option<LotCpvAlignment>,
    pub languages: Vec<String>,
    pub source_url: Option<String>,
    pub requirements: Vec<TenderRequirement>,
    pub requirement_signals: Vec<RequirementSignal>,
    pub score: f64,
    pub confidence: f64,
    /// Opt-in diagnostic only; omitted from normal responses and never affects ranking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_activity_text_similarity: Option<f64>,
    /// Coverage explanation for the opt-in text diagnostic; never affects ranking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_activity_text_similarity_coverage: Option<&'static str>,
    /// Opt-in explanation of existing geographic evidence; never a score input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_geographic_evidence: Option<crate::GeographicMatchEvidence>,
    /// Files collapsed into this candidate from one dossier (1 when the
    /// tender stands alone). The ladder shows one slot per procurement;
    /// the members stay in the store for audit.
    pub dossier_members: usize,
    /// Whether the notice states qualification criteria at all. See
    /// `FeasibilityEvidence`.
    pub feasibility: FeasibilityEvidence,
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
    /// Deep-rerank fields are omitted when no analysis ran; answered values
    /// may be `null` when TypeSafe did not return that question's answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jev_relevance: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jev_qualification_risk: Option<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CpvNaceMatchLevel {
    Class,
    Division,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CpvNaceOverlap {
    pub cpv_code: String,
    pub match_level: CpvNaceMatchLevel,
    pub nace_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LotCpvAlignmentEntry {
    pub lot_id: Option<String>,
    pub cpv_codes: Vec<String>,
    pub overlaps: Vec<CpvNaceOverlap>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LotCpvAlignment {
    pub lots: Vec<LotCpvAlignmentEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LotFrameworkMaximum {
    pub scope: &'static str,
    /// Original source amount text; not parsed, converted, or aggregated.
    pub amount: String,
    pub currency_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowLotAwardCriterionParameter {
    pub code: Option<String>,
    /// Source text only; it is not interpreted as a weight or percentage.
    pub numeric: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowLotAwardCriterion {
    pub source_element: &'static str,
    /// Zero-based source-order indexes preserving the nested XML hierarchy.
    pub source_ordinal_path: Vec<u64>,
    pub type_code: Option<String>,
    pub description: Option<String>,
    /// Source expression retained verbatim; never executed or interpreted.
    pub calculation_expression: Option<String>,
    pub parameters: Vec<ShadowLotAwardCriterionParameter>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowLotSelectionEvidence {
    pub source_element: &'static str,
    pub source_path: &'static str,
    /// Zero-based source-order indexes under the enclosing source structures.
    pub source_ordinal_path: Vec<u64>,
    pub code: Option<String>,
    pub code_list_name: Option<String>,
    pub descriptions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowLotExecutionCondition {
    pub source_element: &'static str,
    pub source_path: &'static str,
    /// Source-order index under the lot's TenderingTerms.
    pub source_ordinal_path: Vec<u64>,
    /// Raw source code, not an eligibility or bidder-qualification judgment.
    pub code: Option<String>,
    pub code_list_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowLotSubsequentProcessRequirement {
    pub source_element: &'static str,
    pub source_path: &'static str,
    /// Source-order index under the lot's framework agreement.
    pub source_ordinal_path: Vec<u64>,
    /// Raw name only; it is not interpreted as bidder eligibility.
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowLotRequiredFinancialGuarantee {
    pub source_element: &'static str,
    pub source_path: &'static str,
    pub source_ordinal_path: Vec<u64>,
    /// Raw source code; no threshold or compliance decision is inferred.
    pub code: Option<String>,
    pub code_list_name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowLotRequiredCurriculum {
    pub source_element: &'static str,
    pub source_path: &'static str,
    pub source_ordinal_path: Vec<u64>,
    /// Raw source code; no qualification decision is inferred.
    pub code: Option<String>,
    pub code_list_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowTenderLotFeature {
    pub lot_id: String,
    pub cpv_codes: Vec<String>,
    pub cpv_nace_overlaps: Vec<CpvNaceOverlap>,
    pub nace_coverage: &'static str,
    pub country_subentity_codes: Vec<String>,
    pub place_coverage: &'static str,
    pub framework_maximums: Vec<LotFrameworkMaximum>,
    pub award_criteria: Vec<ShadowLotAwardCriterion>,
    pub award_criteria_coverage: &'static str,
    pub selection_evidence: Vec<ShadowLotSelectionEvidence>,
    pub selection_evidence_coverage: &'static str,
    pub contract_execution_conditions: Vec<ShadowLotExecutionCondition>,
    pub contract_execution_conditions_coverage: &'static str,
    pub subsequent_process_requirements: Vec<ShadowLotSubsequentProcessRequirement>,
    pub subsequent_process_requirements_coverage: &'static str,
    pub required_financial_guarantees: Vec<ShadowLotRequiredFinancialGuarantee>,
    pub financial_guarantees_coverage: &'static str,
    pub required_curricula: Vec<ShadowLotRequiredCurriculum>,
    pub curricula_coverage: &'static str,
    pub coverage: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowTedLotValue {
    /// Exact singleton source token; also validated by the existing Decimal parser.
    pub raw_amount: String,
    pub currency: Option<String>,
    pub currency_coverage: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowTedLotEvidence {
    pub status: &'static str,
    pub association_rule: &'static str,
    pub lot_id: Option<String>,
    pub lot_id_coverage: &'static str,
    pub main_classification_cpv: Option<String>,
    pub main_classification_coverage: &'static str,
    pub estimated_value: Option<ShadowTedLotValue>,
    pub estimated_value_coverage: &'static str,
    pub framework_maximum: Option<ShadowTedLotValue>,
    pub framework_maximum_coverage: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShadowLotFeatureProjection {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ted_lot_evidence: Option<ShadowTedLotEvidence>,
    pub association_rule: &'static str,
    pub status: &'static str,
    pub lots: Vec<ShadowTenderLotFeature>,
    pub unkeyed_cpv_records: usize,
    pub cpv_records_without_valid_codes: usize,
    pub unkeyed_lot_framework_maximum_records: usize,
    pub invalid_lot_framework_maximum_records: usize,
    pub notice_level_framework_maximum_records: usize,
    pub withheld_ambiguous_framework_maximum_records: usize,
    pub withheld_framework_maximum_records_due_to_truncation: usize,
    pub ambiguous_lot_id_count: usize,
    pub unkeyed_place_records: usize,
    pub invalid_place_records: usize,
    pub unkeyed_award_criteria_records: usize,
    pub invalid_award_criteria_records: usize,
    pub unkeyed_selection_records: usize,
    pub invalid_selection_records: usize,
    pub unkeyed_contract_execution_records: usize,
    pub invalid_contract_execution_records: usize,
    pub unkeyed_subsequent_process_records: usize,
    pub invalid_subsequent_process_records: usize,
    pub unkeyed_guarantee_records: usize,
    pub invalid_guarantee_records: usize,
    pub unkeyed_curricula_records: usize,
    pub invalid_curricula_records: usize,
    pub flat_framework_amounts_present: bool,
    pub unprojected_dimensions: Vec<&'static str>,
    pub truncated: bool,
}

const MAX_LOT_ALIGNMENT_LOTS: usize = 20;
const MAX_LOT_ALIGNMENT_CPVS_PER_LOT: usize = 10;

fn cpv_nace_overlaps(company: &CompanyProfile, cpv_codes: &[String]) -> Vec<CpvNaceOverlap> {
    let activity_classes = company
        .activities
        .iter()
        .filter_map(|activity| activity.code.get(..4).map(str::to_owned))
        .collect::<BTreeSet<_>>();
    let activity_divisions = company
        .activities
        .iter()
        .filter_map(|activity| activity.code.get(..2).map(str::to_owned))
        .collect::<BTreeSet<_>>();
    cpv_codes
        .iter()
        .filter_map(|cpv_code| {
            let mapped = crate::nace_prefixes_for_cpv(cpv_code);
            let class_matches = mapped
                .iter()
                .filter(|prefix| prefix.len() == 4 && activity_classes.contains(*prefix))
                .cloned()
                .collect::<Vec<_>>();
            if !class_matches.is_empty() {
                return Some(CpvNaceOverlap {
                    cpv_code: cpv_code.clone(),
                    match_level: CpvNaceMatchLevel::Class,
                    nace_codes: class_matches,
                });
            }
            let division_matches = mapped
                .iter()
                .filter(|prefix| prefix.len() == 2 && activity_divisions.contains(*prefix))
                .cloned()
                .collect::<Vec<_>>();
            (!division_matches.is_empty()).then(|| CpvNaceOverlap {
                cpv_code: cpv_code.clone(),
                match_level: CpvNaceMatchLevel::Division,
                nace_codes: division_matches,
            })
        })
        .collect()
}

fn derive_lot_cpv_alignment(company: &CompanyProfile, tender: &Tender) -> Option<LotCpvAlignment> {
    let records = tender
        .raw
        .get("notice_lot_cpv_evidence")
        .and_then(serde_json::Value::as_array)?;
    if records.is_empty() || company.activities.is_empty() {
        return None;
    }

    // This explains the existing crosswalk; it is not a score.
    let mut truncated = tender
        .raw
        .get("notice_lot_cpv_evidence_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || records.len() > MAX_LOT_ALIGNMENT_LOTS;
    let mut lots = Vec::with_capacity(records.len().min(MAX_LOT_ALIGNMENT_LOTS));
    for record in records.iter().take(MAX_LOT_ALIGNMENT_LOTS) {
        let all_codes = record
            .get("cpv_codes")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|code| code.len() == 8 && code.bytes().all(|byte| byte.is_ascii_digit()))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if all_codes.len() > MAX_LOT_ALIGNMENT_CPVS_PER_LOT {
            truncated = true;
        }
        let cpv_codes = all_codes
            .into_iter()
            .take(MAX_LOT_ALIGNMENT_CPVS_PER_LOT)
            .collect::<Vec<_>>();

        let overlaps = cpv_nace_overlaps(company, &cpv_codes);
        let lot_id = record
            .get("lot_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.chars().take(128).collect());
        lots.push(LotCpvAlignmentEntry {
            lot_id,
            cpv_codes,
            overlaps,
        });
    }
    (!lots.is_empty()).then_some(LotCpvAlignment { lots, truncated })
}

const MAX_SHADOW_LOT_FEATURES: usize = 20;
const MAX_SHADOW_LOT_CPVS: usize = 10;
const MAX_SHADOW_LOT_PLACE_CODES: usize = 10;
const MAX_SHADOW_LOT_FRAMEWORK_MAXIMUMS: usize = 20;
const MAX_SHADOW_FRAMEWORK_EVIDENCE: usize = 100;
const MAX_SHADOW_LOT_AWARD_CRITERIA: usize = 20;
const MAX_SHADOW_LOT_AWARD_CRITERIA_PER_LOT: usize = 10;
const MAX_SHADOW_LOT_AWARD_CRITERION_PARAMETERS: usize = 10;
const MAX_SHADOW_LOT_SELECTION_EVIDENCE: usize = 100;
const MAX_SHADOW_LOT_SELECTION_PER_LOT: usize = 20;
const MAX_SHADOW_LOT_SELECTION_DESCRIPTIONS: usize = 10;
const MAX_SHADOW_LOT_CONTRACT_EXECUTION_PER_LOT: usize = 20;
const MAX_SHADOW_LOT_CONTRACT_EXECUTION_CODE_CHARS: usize = 256;
const MAX_SHADOW_LOT_CONTRACT_EXECUTION_LIST_NAME_CHARS: usize = 128;
const MAX_SHADOW_LOT_SUBSEQUENT_REQUIREMENTS: usize = 100;
const MAX_SHADOW_LOT_SUBSEQUENT_PER_LOT: usize = 20;
const MAX_SHADOW_LOT_SUBSEQUENT_NAME_CHARS: usize = 2_000;
const MAX_SHADOW_LOT_GUARANTEES_PER_LOT: usize = 20;
const MAX_SHADOW_LOT_CURRICULA_PER_LOT: usize = 20;
const MAX_SHADOW_LOT_GUARANTEE_CURRICULA_RECORDS: usize = 100;
const MAX_SHADOW_LOT_GUARANTEE_CURRICULA_TEXT_CHARS: usize = 2_000;

fn ted_source_field<'a>(
    fields: &'a serde_json::Value,
    name: &str,
) -> (usize, Option<&'a str>, &'static str) {
    let Some(summary) = fields.get(name) else {
        return (0, None, "not_recorded");
    };
    let count = summary
        .get("source_cardinality")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX);
    let value = summary
        .get("singleton_value")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let status = match summary.get("status").and_then(serde_json::Value::as_str) {
        Some("not_recorded") => "not_recorded",
        Some("singleton") => "singleton",
        Some("invalid_singleton") => "invalid_singleton",
        Some("multiple_values") => "multiple_values",
        _ => "invalid_summary",
    };
    (count, value, status)
}

fn ted_singleton_value(
    fields: &serde_json::Value,
    amount_field: &str,
    currency_field: &str,
) -> (Option<ShadowTedLotValue>, &'static str) {
    let (count, amount, amount_status) = ted_source_field(fields, amount_field);
    if count == 0 {
        return (None, "not_recorded");
    }
    if count != 1 || amount_status == "multiple_values" {
        return (None, "multiple_values_withheld");
    }
    if amount_status != "singleton" {
        return (None, "invalid_singleton_withheld");
    }
    let Some(amount) = amount.filter(|value| crate::parse_money(value).is_some()) else {
        return (None, "invalid_singleton_withheld");
    };
    let (currency_count, currency_value, currency_status) =
        ted_source_field(fields, currency_field);
    let (currency, currency_coverage) = if currency_count == 0 {
        (None, "not_recorded")
    } else if currency_count != 1 || currency_status == "multiple_values" {
        (None, "multiple_values_withheld")
    } else if currency_status != "singleton" {
        (None, "invalid_singleton_withheld")
    } else if let Some(currency) = currency_value
        .filter(|value| value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_alphabetic()))
    {
        (Some(currency.to_ascii_uppercase()), "singleton")
    } else {
        (None, "invalid_singleton_withheld")
    };
    (
        Some(ShadowTedLotValue {
            raw_amount: amount.to_owned(),
            currency,
            currency_coverage,
        }),
        "singleton",
    )
}

fn shadow_ted_lot_evidence(tender: &Tender) -> ShadowTedLotEvidence {
    let fields = tender
        .raw
        .get("notice_lot_ted_evidence")
        .and_then(|evidence| evidence.get("fields"));
    let empty_fields = serde_json::Value::Null;
    let fields = fields.unwrap_or(&empty_fields);
    let (lot_count, lot_id, lot_status) = ted_source_field(fields, "lot_id");
    let (status, lot_id, lot_id_coverage) = match (lot_count, lot_id, lot_status) {
        (1, Some(id), "singleton") => ("partial", Some(id.to_owned()), "singleton"),
        (0, _, _) => ("no_lot_evidence", None, "not_recorded"),
        (count, _, "multiple_values") if count > 1 => {
            ("multiple_lots_withheld", None, "multiple_values_withheld")
        }
        _ => ("invalid_lot_identity", None, "invalid_singleton_withheld"),
    };
    let (main_count, main_value, main_status) = ted_source_field(fields, "main_classification");
    let main_classification_cpv =
        if lot_id.is_some() && main_count == 1 && main_status == "singleton" {
            main_value.and_then(|value| {
                let normalized = crate::clean_codes(vec![value.to_owned()]);
                normalized
                    .into_iter()
                    .find(|code| code.len() == 8 && code.bytes().all(|byte| byte.is_ascii_digit()))
            })
        } else {
            None
        };
    let main_classification_coverage = if lot_id.is_none() {
        "withheld_without_singleton_lot_id"
    } else if main_count == 0 {
        "not_recorded"
    } else if main_count != 1 || main_status == "multiple_values" {
        "multiple_values_withheld"
    } else if main_classification_cpv.is_some() {
        "singleton"
    } else {
        "invalid_singleton_withheld"
    };
    let (estimated_value, estimated_value_coverage) = if lot_id.is_some() {
        ted_singleton_value(fields, "estimated_value", "estimated_currency")
    } else {
        (None, "withheld_without_singleton_lot_id")
    };
    let (framework_maximum, framework_maximum_coverage) = if lot_id.is_some() {
        ted_singleton_value(fields, "framework_maximum", "framework_currency")
    } else {
        (None, "withheld_without_singleton_lot_id")
    };
    ShadowTedLotEvidence {
        status,
        association_rule: "singleton_field_cardinality_only_no_positional_join",
        lot_id,
        lot_id_coverage,
        main_classification_cpv,
        main_classification_coverage,
        estimated_value,
        estimated_value_coverage,
        framework_maximum,
        framework_maximum_coverage,
    }
}

/// Build an explain-only lot projection from the shared BDA/BOSA XML schema.
/// Values and CPVs join only on their explicit, exact source lot ID; notice-level
/// ceilings and other unkeyed facts are never attached to an arbitrary lot.
pub fn shadow_lot_feature_projection(
    company: &CompanyProfile,
    tender: &Tender,
) -> ShadowLotFeatureProjection {
    let mut projection = ShadowLotFeatureProjection {
        source: tender.source.clone(),
        ted_lot_evidence: None,
        association_rule: "exact_source_lot_id",
        status: "source_not_supported",
        lots: Vec::new(),
        unkeyed_cpv_records: 0,
        cpv_records_without_valid_codes: 0,
        unkeyed_lot_framework_maximum_records: 0,
        invalid_lot_framework_maximum_records: 0,
        notice_level_framework_maximum_records: 0,
        withheld_ambiguous_framework_maximum_records: 0,
        withheld_framework_maximum_records_due_to_truncation: 0,
        ambiguous_lot_id_count: 0,
        unkeyed_place_records: 0,
        invalid_place_records: 0,
        unkeyed_award_criteria_records: 0,
        invalid_award_criteria_records: 0,
        unkeyed_selection_records: 0,
        invalid_selection_records: 0,
        unkeyed_contract_execution_records: 0,
        invalid_contract_execution_records: 0,
        unkeyed_subsequent_process_records: 0,
        invalid_subsequent_process_records: 0,
        unkeyed_guarantee_records: 0,
        invalid_guarantee_records: 0,
        unkeyed_curricula_records: 0,
        invalid_curricula_records: 0,
        flat_framework_amounts_present: tender
            .raw
            .get("framework_maximum_amounts")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|values| !values.is_empty()),
        unprojected_dimensions: vec![
            "lot_contract_execution_requirements",
            "lot_guarantees_and_curricula",
            "lot_subsequent_process_requirements",
        ],
        truncated: false,
    };
    if tender.source == "ted" {
        let evidence = shadow_ted_lot_evidence(tender);
        projection.status = evidence.status;
        projection.association_rule = "singleton_source_field_cardinality_no_positional_join";
        projection.ted_lot_evidence = Some(evidence);
        projection.unprojected_dimensions = vec![
            "multi_lot_value_and_classification_association",
            "lot_place",
            "lot_requirements",
            "additional_lot_classifications",
        ];
        return projection;
    }
    if !matches!(tender.source.as_str(), "bda" | "bda-int") {
        return projection;
    }

    let cpv_records = tender
        .raw
        .get("notice_lot_cpv_evidence")
        .and_then(serde_json::Value::as_array);
    let framework_records = tender
        .raw
        .get("framework_maximum_evidence")
        .and_then(serde_json::Value::as_array);
    let place_records = tender
        .raw
        .get("notice_lot_place_evidence")
        .and_then(serde_json::Value::as_array);
    let no_cpv_records = cpv_records.is_none_or(|records| records.is_empty());
    let no_framework_records = framework_records.is_none_or(|records| records.is_empty());
    let no_place_records = place_records.is_none_or(|records| records.is_empty());
    let criteria_records = tender
        .raw
        .get("notice_lot_award_criteria_evidence")
        .and_then(serde_json::Value::as_array);
    let no_criteria_records = criteria_records.is_none_or(|records| records.is_empty());
    let selection_records = tender
        .raw
        .get("notice_lot_selection_evidence")
        .and_then(serde_json::Value::as_array);
    let no_selection_records = selection_records.is_none_or(|records| records.is_empty());
    let execution_records = tender
        .raw
        .get("notice_lot_contract_execution_evidence")
        .and_then(serde_json::Value::as_array);
    let no_execution_conditions = execution_records.is_none_or(|records| {
        records.iter().all(|record| {
            record
                .get("conditions")
                .and_then(serde_json::Value::as_array)
                .is_none_or(|conditions| conditions.is_empty())
        })
    });
    if execution_records.is_some() {
        projection
            .unprojected_dimensions
            .retain(|dimension| *dimension != "lot_contract_execution_requirements");
    }
    let subsequent_records = tender
        .raw
        .get("notice_lot_subsequent_process_evidence")
        .and_then(serde_json::Value::as_array);
    let no_subsequent_records = subsequent_records.is_none_or(|records| {
        records.iter().all(|record| {
            record
                .get("requirements")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|requirements| requirements.is_empty())
        })
    });
    if subsequent_records.is_some() {
        projection
            .unprojected_dimensions
            .retain(|dimension| *dimension != "lot_subsequent_process_requirements");
    }
    let guarantee_curricula_records = tender
        .raw
        .get("notice_lot_guarantee_curricula_evidence")
        .and_then(serde_json::Value::as_array);
    let guarantee_curricula_truncated = tender
        .raw
        .get("notice_lot_guarantee_curricula_evidence_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let no_guarantee_curricula_records = if guarantee_curricula_truncated {
        false
    } else if !tender
        .raw
        .as_object()
        .is_some_and(|raw| raw.contains_key("notice_lot_guarantee_curricula_evidence"))
    {
        true
    } else {
        guarantee_curricula_records.is_some_and(|records| {
            records.iter().all(|record| {
                record.as_object().is_some_and(|object| {
                    object
                        .get("guarantees")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|items| items.is_empty())
                        && object
                            .get("curricula")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|items| items.is_empty())
                })
            })
        })
    };
    if guarantee_curricula_records.is_some() {
        projection
            .unprojected_dimensions
            .retain(|dimension| *dimension != "lot_guarantees_and_curricula");
    }
    if no_cpv_records
        && no_framework_records
        && no_place_records
        && no_criteria_records
        && no_selection_records
        && no_execution_conditions
        && no_subsequent_records
        && no_guarantee_curricula_records
    {
        projection.status = "no_lot_evidence";
        return projection;
    }
    match tender
        .raw
        .get("notice_lot_ids_unique")
        .and_then(serde_json::Value::as_bool)
    {
        Some(true) => {}
        Some(false) => {
            projection.status = "lot_ids_not_unique";
            return projection;
        }
        None => {
            projection.status = "lot_id_uniqueness_unknown";
            return projection;
        }
    }

    let cpv_evidence_truncated = tender
        .raw
        .get("notice_lot_cpv_evidence_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || cpv_records.is_some_and(|records| records.len() > MAX_SHADOW_LOT_FEATURES);
    projection.truncated = cpv_evidence_truncated
        || tender
            .raw
            .get("notice_lot_place_evidence_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || place_records.is_some_and(|records| records.len() > MAX_SHADOW_LOT_FEATURES)
        || tender
            .raw
            .get("notice_lot_award_criteria_evidence_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || criteria_records.is_some_and(|records| records.len() > MAX_SHADOW_LOT_FEATURES)
        || tender
            .raw
            .get("notice_lot_selection_evidence_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || selection_records.is_some_and(|records| records.len() > MAX_SHADOW_LOT_FEATURES)
        || tender
            .raw
            .get("notice_lot_contract_execution_evidence_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || execution_records.is_some_and(|records| records.len() > MAX_SHADOW_LOT_FEATURES)
        || tender
            .raw
            .get("notice_lot_subsequent_process_evidence_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || subsequent_records.is_some_and(|records| records.len() > MAX_SHADOW_LOT_FEATURES)
        || tender
            .raw
            .get("notice_lot_guarantee_curricula_evidence_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || guarantee_curricula_records
            .is_some_and(|records| records.len() > MAX_SHADOW_LOT_FEATURES)
        || tender
            .raw
            .get("framework_maximum_evidence_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

    let mut cpvs_by_lot = BTreeMap::<String, Vec<String>>::new();
    let mut seen_cpv_lot_ids = BTreeSet::new();
    let mut ambiguous_lot_ids = BTreeSet::new();
    if let Some(records) = cpv_records {
        projection.truncated |= records.len() > MAX_SHADOW_LOT_FEATURES;
        for record in records.iter().take(MAX_SHADOW_LOT_FEATURES) {
            let Some(object) = record.as_object() else {
                projection.truncated = true;
                continue;
            };
            let lot_id = object
                .get("lot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.chars().count() <= 128)
                .map(str::to_owned);
            let Some(lot_id) = lot_id else {
                projection.unkeyed_cpv_records += 1;
                continue;
            };
            if !seen_cpv_lot_ids.insert(lot_id.clone()) {
                ambiguous_lot_ids.insert(lot_id.clone());
                cpvs_by_lot.remove(&lot_id);
                continue;
            }
            let mut cpv_codes = record
                .get("cpv_codes")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|code| code.len() == 8 && code.bytes().all(|byte| byte.is_ascii_digit()))
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            if cpv_codes.is_empty() {
                projection.cpv_records_without_valid_codes += 1;
                continue;
            }
            if cpv_codes.len() > MAX_SHADOW_LOT_CPVS {
                projection.truncated = true;
                cpv_codes.truncate(MAX_SHADOW_LOT_CPVS);
            }
            cpvs_by_lot.insert(lot_id, cpv_codes);
        }
    }
    let mut places_by_lot = BTreeMap::<String, Vec<String>>::new();
    let mut seen_place_lot_ids = BTreeSet::new();
    if let Some(records) = place_records {
        projection.truncated |= records.len() > MAX_SHADOW_LOT_FEATURES;
        for record in records.iter().take(MAX_SHADOW_LOT_FEATURES) {
            let Some(object) = record.as_object() else {
                projection.invalid_place_records += 1;
                projection.truncated = true;
                continue;
            };
            let lot_id = object
                .get("lot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.chars().count() <= 128)
                .map(str::to_owned);
            let Some(lot_id) = lot_id else {
                projection.unkeyed_place_records += 1;
                continue;
            };
            if !seen_place_lot_ids.insert(lot_id.clone()) {
                ambiguous_lot_ids.insert(lot_id.clone());
                cpvs_by_lot.remove(&lot_id);
                places_by_lot.remove(&lot_id);
                continue;
            }
            let Some(codes) = object
                .get("country_subentity_codes")
                .and_then(serde_json::Value::as_array)
            else {
                projection.invalid_place_records += 1;
                projection.truncated = true;
                continue;
            };
            let mut seen_codes = BTreeSet::new();
            let mut country_subentity_codes = Vec::new();
            for code in codes {
                let Some(code) = code.as_str() else {
                    projection.invalid_place_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let code = code.trim();
                if code.is_empty() || code.chars().count() > 128 {
                    projection.invalid_place_records += 1;
                    projection.truncated = true;
                    continue;
                }
                if seen_codes.insert(code.to_owned()) {
                    if country_subentity_codes.len() == MAX_SHADOW_LOT_PLACE_CODES {
                        projection.truncated = true;
                        continue;
                    }
                    country_subentity_codes.push(code.to_owned());
                }
            }
            places_by_lot.insert(lot_id, country_subentity_codes);
        }
    }

    let mut award_criteria_by_lot = BTreeMap::<String, Vec<ShadowLotAwardCriterion>>::new();
    let mut seen_criteria_lot_ids = BTreeSet::new();
    let mut projected_criteria_count = 0;
    if let Some(records) = criteria_records {
        projection.truncated |= records.len() > MAX_SHADOW_LOT_FEATURES;
        for record in records.iter().take(MAX_SHADOW_LOT_FEATURES) {
            let Some(object) = record.as_object() else {
                projection.invalid_award_criteria_records += 1;
                projection.truncated = true;
                continue;
            };
            let lot_id = object
                .get("lot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.chars().count() <= 128)
                .map(str::to_owned);
            let Some(lot_id) = lot_id else {
                projection.unkeyed_award_criteria_records += 1;
                continue;
            };
            if !seen_criteria_lot_ids.insert(lot_id.clone()) {
                ambiguous_lot_ids.insert(lot_id.clone());
                cpvs_by_lot.remove(&lot_id);
                places_by_lot.remove(&lot_id);
                award_criteria_by_lot.remove(&lot_id);
                continue;
            }
            let Some(criteria) = object.get("criteria").and_then(serde_json::Value::as_array)
            else {
                projection.invalid_award_criteria_records += 1;
                projection.truncated = true;
                continue;
            };
            let mut projected = Vec::new();
            for criterion in criteria {
                if projected_criteria_count == MAX_SHADOW_LOT_AWARD_CRITERIA
                    || projected.len() == MAX_SHADOW_LOT_AWARD_CRITERIA_PER_LOT
                {
                    projection.truncated = true;
                    break;
                }
                let Some(criterion) = criterion.as_object() else {
                    projection.invalid_award_criteria_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let source_element = match criterion
                    .get("source_element")
                    .and_then(serde_json::Value::as_str)
                {
                    Some("AwardingCriterion") => "AwardingCriterion",
                    Some("SubordinateAwardingCriterion") => "SubordinateAwardingCriterion",
                    _ => {
                        projection.invalid_award_criteria_records += 1;
                        projection.truncated = true;
                        continue;
                    }
                };
                let Some(path) = criterion
                    .get("source_ordinal_path")
                    .and_then(serde_json::Value::as_array)
                else {
                    projection.invalid_award_criteria_records += 1;
                    projection.truncated = true;
                    continue;
                };
                if path.is_empty() || path.len() > 20 {
                    projection.invalid_award_criteria_records += 1;
                    projection.truncated = true;
                    continue;
                }
                let Some(source_ordinal_path) = path
                    .iter()
                    .map(serde_json::Value::as_u64)
                    .collect::<Option<Vec<_>>>()
                else {
                    projection.invalid_award_criteria_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let text_field =
                    |key: &str, limit: usize, truncated: &mut bool| match criterion.get(key) {
                        None | Some(serde_json::Value::Null) => None,
                        Some(serde_json::Value::String(value)) => {
                            if value.chars().count() > limit {
                                *truncated = true;
                                Some(value.chars().take(limit).collect())
                            } else {
                                Some(value.clone())
                            }
                        }
                        Some(_) => {
                            *truncated = true;
                            None
                        }
                    };
                let type_code = text_field("type_code", 128, &mut projection.truncated);
                let description = text_field("description", 1_000, &mut projection.truncated);
                let calculation_expression =
                    text_field("calculation_expression", 1_000, &mut projection.truncated);
                let Some(parameters) = criterion
                    .get("parameters")
                    .and_then(serde_json::Value::as_array)
                else {
                    projection.invalid_award_criteria_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let mut projected_parameters = Vec::new();
                for parameter in parameters {
                    if projected_parameters.len() == MAX_SHADOW_LOT_AWARD_CRITERION_PARAMETERS {
                        projection.truncated = true;
                        break;
                    }
                    let Some(parameter) = parameter.as_object() else {
                        projection.invalid_award_criteria_records += 1;
                        projection.truncated = true;
                        continue;
                    };
                    let parameter_text = |key: &str, truncated: &mut bool| match parameter.get(key)
                    {
                        None | Some(serde_json::Value::Null) => None,
                        Some(serde_json::Value::String(value)) if value.chars().count() <= 128 => {
                            Some(value.clone())
                        }
                        Some(serde_json::Value::String(value)) => {
                            *truncated = true;
                            Some(value.chars().take(128).collect())
                        }
                        Some(_) => {
                            *truncated = true;
                            None
                        }
                    };
                    let code = parameter_text("code", &mut projection.truncated);
                    let numeric = parameter_text("numeric", &mut projection.truncated);
                    if code.is_some() || numeric.is_some() {
                        projected_parameters
                            .push(ShadowLotAwardCriterionParameter { code, numeric });
                    }
                }
                projected.push(ShadowLotAwardCriterion {
                    source_element,
                    source_ordinal_path,
                    type_code,
                    description,
                    calculation_expression,
                    parameters: projected_parameters,
                });
                projected_criteria_count += 1;
            }
            award_criteria_by_lot.insert(lot_id, projected);
        }
    }
    let mut selection_evidence_by_lot = BTreeMap::<String, Vec<ShadowLotSelectionEvidence>>::new();
    let mut seen_selection_lot_ids = BTreeSet::new();
    let mut projected_selection_count = 0;
    if let Some(records) = selection_records {
        projection.truncated |= records.len() > MAX_SHADOW_LOT_FEATURES;
        for record in records.iter().take(MAX_SHADOW_LOT_FEATURES) {
            let Some(object) = record.as_object() else {
                projection.invalid_selection_records += 1;
                projection.truncated = true;
                continue;
            };
            let lot_id = object
                .get("lot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.chars().count() <= 128)
                .map(str::to_owned);
            let Some(lot_id) = lot_id else {
                projection.unkeyed_selection_records += 1;
                continue;
            };
            if !seen_selection_lot_ids.insert(lot_id.clone()) {
                ambiguous_lot_ids.insert(lot_id.clone());
                cpvs_by_lot.remove(&lot_id);
                places_by_lot.remove(&lot_id);
                award_criteria_by_lot.remove(&lot_id);
                selection_evidence_by_lot.remove(&lot_id);
                continue;
            }
            let Some(source_records) = object
                .get("selection_evidence")
                .and_then(serde_json::Value::as_array)
            else {
                projection.invalid_selection_records += 1;
                projection.truncated = true;
                continue;
            };
            let mut projected = Vec::new();
            for source_record in source_records {
                if projected_selection_count == MAX_SHADOW_LOT_SELECTION_EVIDENCE
                    || projected.len() == MAX_SHADOW_LOT_SELECTION_PER_LOT
                {
                    projection.truncated = true;
                    break;
                }
                projected_selection_count += 1;
                let Some(source_record) = source_record.as_object() else {
                    projection.invalid_selection_records += 1;
                    projection.truncated = true;
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
                        projection.invalid_selection_records += 1;
                        projection.truncated = true;
                        continue;
                    }
                };
                if source_record
                    .get("source_path")
                    .and_then(serde_json::Value::as_str)
                    != Some(source_path)
                {
                    projection.invalid_selection_records += 1;
                    projection.truncated = true;
                    continue;
                }
                let Some(path) = source_record
                    .get("source_ordinal_path")
                    .and_then(serde_json::Value::as_array)
                else {
                    projection.invalid_selection_records += 1;
                    projection.truncated = true;
                    continue;
                };
                if path.len() != 2 {
                    projection.invalid_selection_records += 1;
                    projection.truncated = true;
                    continue;
                }
                let Some(source_ordinal_path) = path
                    .iter()
                    .map(|index| index.as_u64().filter(|value| *value <= 10_000))
                    .collect::<Option<Vec<_>>>()
                else {
                    projection.invalid_selection_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let text_field =
                    |key: &str, limit: usize, truncated: &mut bool| match source_record.get(key) {
                        None | Some(serde_json::Value::Null) => None,
                        Some(serde_json::Value::String(value)) => {
                            if value.chars().count() > limit {
                                *truncated = true;
                                Some(value.chars().take(limit).collect())
                            } else {
                                Some(value.clone())
                            }
                        }
                        Some(_) => {
                            *truncated = true;
                            None
                        }
                    };
                let code = text_field("code", 128, &mut projection.truncated);
                let code_list_name = text_field("code_list_name", 128, &mut projection.truncated);
                let Some(descriptions) = source_record
                    .get("descriptions")
                    .and_then(serde_json::Value::as_array)
                else {
                    projection.invalid_selection_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let mut projected_descriptions = Vec::new();
                if descriptions.len() > MAX_SHADOW_LOT_SELECTION_DESCRIPTIONS {
                    projection.truncated = true;
                }
                for description in descriptions
                    .iter()
                    .take(MAX_SHADOW_LOT_SELECTION_DESCRIPTIONS)
                {
                    let Some(description) = description.as_str() else {
                        projection.invalid_selection_records += 1;
                        projection.truncated = true;
                        continue;
                    };
                    if description.chars().count() > 1_000 {
                        projection.truncated = true;
                        projected_descriptions.push(description.chars().take(1_000).collect());
                    } else {
                        projected_descriptions.push(description.to_owned());
                    }
                }
                projected.push(ShadowLotSelectionEvidence {
                    source_element,
                    source_path,
                    source_ordinal_path,
                    code,
                    code_list_name,
                    descriptions: projected_descriptions,
                });
            }
            selection_evidence_by_lot.insert(lot_id, projected);
        }
    }
    let mut execution_conditions_by_lot =
        BTreeMap::<String, Vec<ShadowLotExecutionCondition>>::new();
    let mut seen_execution_lot_ids = BTreeSet::new();
    if let Some(records) = execution_records {
        projection.truncated |= records.len() > MAX_SHADOW_LOT_FEATURES;
        for record in records.iter().take(MAX_SHADOW_LOT_FEATURES) {
            let Some(object) = record.as_object() else {
                projection.invalid_contract_execution_records += 1;
                projection.truncated = true;
                continue;
            };
            let lot_id = object
                .get("lot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.chars().count() <= 128)
                .map(str::to_owned);
            let Some(lot_id) = lot_id else {
                projection.unkeyed_contract_execution_records += 1;
                continue;
            };
            if !seen_execution_lot_ids.insert(lot_id.clone()) {
                ambiguous_lot_ids.insert(lot_id.clone());
                execution_conditions_by_lot.remove(&lot_id);
                continue;
            }
            let Some(source_conditions) = object
                .get("conditions")
                .and_then(serde_json::Value::as_array)
            else {
                projection.invalid_contract_execution_records += 1;
                projection.truncated = true;
                continue;
            };
            let mut projected = Vec::new();
            if source_conditions.len() > MAX_SHADOW_LOT_CONTRACT_EXECUTION_PER_LOT {
                projection.truncated = true;
            }
            for source_condition in source_conditions
                .iter()
                .take(MAX_SHADOW_LOT_CONTRACT_EXECUTION_PER_LOT)
            {
                let Some(source_condition) = source_condition.as_object() else {
                    projection.invalid_contract_execution_records += 1;
                    projection.truncated = true;
                    continue;
                };
                if source_condition
                    .get("source_element")
                    .and_then(serde_json::Value::as_str)
                    != Some("ContractExecutionRequirement")
                    || source_condition
                        .get("source_path")
                        .and_then(serde_json::Value::as_str)
                        != Some(
                            "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode",
                        )
                {
                    projection.invalid_contract_execution_records += 1;
                    projection.truncated = true;
                    continue;
                }
                let Some(source_ordinal_path) = source_condition
                    .get("source_ordinal_path")
                    .and_then(serde_json::Value::as_array)
                    .filter(|path| path.len() == 1)
                    .and_then(|path| {
                        path.iter()
                            .map(|index| index.as_u64().filter(|value| *value <= 10_000))
                            .collect::<Option<Vec<_>>>()
                    })
                else {
                    projection.invalid_contract_execution_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let bounded_text =
                    |key: &str, limit: usize, truncated: &mut bool| match source_condition.get(key)
                    {
                        None | Some(serde_json::Value::Null) => None,
                        Some(serde_json::Value::String(value)) => {
                            if value.chars().count() > limit {
                                *truncated = true;
                                Some(value.chars().take(limit).collect())
                            } else {
                                Some(value.clone())
                            }
                        }
                        Some(_) => {
                            *truncated = true;
                            None
                        }
                    };
                let code = bounded_text(
                    "code",
                    MAX_SHADOW_LOT_CONTRACT_EXECUTION_CODE_CHARS,
                    &mut projection.truncated,
                );
                let code_list_name = bounded_text(
                    "code_list_name",
                    MAX_SHADOW_LOT_CONTRACT_EXECUTION_LIST_NAME_CHARS,
                    &mut projection.truncated,
                );
                projected.push(ShadowLotExecutionCondition {
                    source_element: "ContractExecutionRequirement",
                    source_path:
                        "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode",
                    source_ordinal_path,
                    code,
                    code_list_name,
                });
            }
            execution_conditions_by_lot.insert(lot_id, projected);
        }
    }
    let mut subsequent_requirements_by_lot =
        BTreeMap::<String, Vec<ShadowLotSubsequentProcessRequirement>>::new();
    let mut seen_subsequent_lot_ids = BTreeSet::new();
    let mut projected_subsequent_count = 0;
    if let Some(records) = subsequent_records {
        projection.truncated |= records.len() > MAX_SHADOW_LOT_FEATURES;
        for record in records.iter().take(MAX_SHADOW_LOT_FEATURES) {
            let Some(object) = record.as_object() else {
                projection.invalid_subsequent_process_records += 1;
                projection.truncated = true;
                continue;
            };
            let lot_id = object
                .get("lot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.chars().count() <= 128)
                .map(str::to_owned);
            let Some(lot_id) = lot_id else {
                projection.unkeyed_subsequent_process_records += 1;
                continue;
            };
            if !seen_subsequent_lot_ids.insert(lot_id.clone()) {
                ambiguous_lot_ids.insert(lot_id.clone());
                subsequent_requirements_by_lot.remove(&lot_id);
                continue;
            }
            let Some(source_requirements) = object
                .get("requirements")
                .and_then(serde_json::Value::as_array)
            else {
                projection.invalid_subsequent_process_records += 1;
                projection.truncated = true;
                continue;
            };
            if source_requirements.len() > MAX_SHADOW_LOT_SUBSEQUENT_PER_LOT {
                projection.truncated = true;
            }
            let mut projected = Vec::new();
            for source_requirement in source_requirements
                .iter()
                .take(MAX_SHADOW_LOT_SUBSEQUENT_PER_LOT)
            {
                if projected_subsequent_count == MAX_SHADOW_LOT_SUBSEQUENT_REQUIREMENTS {
                    projection.truncated = true;
                    break;
                }
                let Some(source_requirement) = source_requirement.as_object() else {
                    projection.invalid_subsequent_process_records += 1;
                    projection.truncated = true;
                    continue;
                };
                if source_requirement
                    .get("source_element")
                    .and_then(serde_json::Value::as_str)
                    != Some("SubsequentProcessTenderRequirement")
                    || source_requirement
                        .get("source_path")
                        .and_then(serde_json::Value::as_str)
                        != Some("TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name")
                {
                    projection.invalid_subsequent_process_records += 1;
                    projection.truncated = true;
                    continue;
                }
                let Some(source_ordinal_path) = source_requirement
                    .get("source_ordinal_path")
                    .and_then(serde_json::Value::as_array)
                    .filter(|path| path.len() == 1)
                    .and_then(|path| {
                        path.iter()
                            .map(|index| index.as_u64().filter(|value| *value <= 10_000))
                            .collect::<Option<Vec<_>>>()
                    })
                else {
                    projection.invalid_subsequent_process_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let name = match source_requirement.get("name") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(value)) => {
                        if value.chars().count() > MAX_SHADOW_LOT_SUBSEQUENT_NAME_CHARS {
                            projection.truncated = true;
                        }
                        Some(
                            value
                                .chars()
                                .take(MAX_SHADOW_LOT_SUBSEQUENT_NAME_CHARS)
                                .collect(),
                        )
                    }
                    Some(_) => {
                        projection.invalid_subsequent_process_records += 1;
                        projection.truncated = true;
                        continue;
                    }
                };
                projected.push(ShadowLotSubsequentProcessRequirement {
                    source_element: "SubsequentProcessTenderRequirement",
                    source_path: "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name",
                    source_ordinal_path,
                    name,
                });
                projected_subsequent_count += 1;
            }
            subsequent_requirements_by_lot.insert(lot_id, projected);
        }
    }
    let mut guarantees_by_lot = BTreeMap::<String, Vec<ShadowLotRequiredFinancialGuarantee>>::new();
    let mut curricula_by_lot = BTreeMap::<String, Vec<ShadowLotRequiredCurriculum>>::new();
    let mut seen_guarantee_curricula_lot_ids = BTreeSet::new();
    let mut projected_guarantee_curricula_count = 0;
    if let Some(records) = guarantee_curricula_records {
        projection.truncated |= records.len() > MAX_SHADOW_LOT_FEATURES;
        for record in records.iter().take(MAX_SHADOW_LOT_FEATURES) {
            let Some(object) = record.as_object() else {
                projection.invalid_guarantee_records += 1;
                projection.invalid_curricula_records += 1;
                projection.truncated = true;
                continue;
            };
            let lot_id = object
                .get("lot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.chars().count() <= 128)
                .map(str::to_owned);
            let Some(lot_id) = lot_id else {
                projection.unkeyed_guarantee_records += object
                    .get("guarantees")
                    .and_then(serde_json::Value::as_array)
                    .map_or(1, Vec::len);
                projection.unkeyed_curricula_records += object
                    .get("curricula")
                    .and_then(serde_json::Value::as_array)
                    .map_or(1, Vec::len);
                continue;
            };
            if !seen_guarantee_curricula_lot_ids.insert(lot_id.clone()) {
                ambiguous_lot_ids.insert(lot_id.clone());
                guarantees_by_lot.remove(&lot_id);
                curricula_by_lot.remove(&lot_id);
                continue;
            }
            let Some(source_guarantees) = object
                .get("guarantees")
                .and_then(serde_json::Value::as_array)
            else {
                projection.invalid_guarantee_records += 1;
                projection.truncated = true;
                continue;
            };
            let Some(source_curricula) = object
                .get("curricula")
                .and_then(serde_json::Value::as_array)
            else {
                projection.invalid_curricula_records += 1;
                projection.truncated = true;
                continue;
            };
            if source_guarantees.len() > MAX_SHADOW_LOT_GUARANTEES_PER_LOT {
                projection.truncated = true;
            }
            if source_curricula.len() > MAX_SHADOW_LOT_CURRICULA_PER_LOT {
                projection.truncated = true;
            }
            let mut projected_guarantees = Vec::new();
            for source_record in source_guarantees
                .iter()
                .take(MAX_SHADOW_LOT_GUARANTEES_PER_LOT)
            {
                if projected_guarantee_curricula_count == MAX_SHADOW_LOT_GUARANTEE_CURRICULA_RECORDS
                {
                    projection.truncated = true;
                    break;
                }
                let Some(source_record) = source_record.as_object() else {
                    projection.invalid_guarantee_records += 1;
                    projection.truncated = true;
                    continue;
                };
                if source_record
                    .get("source_element")
                    .and_then(serde_json::Value::as_str)
                    != Some("RequiredFinancialGuarantee")
                    || source_record
                        .get("source_path")
                        .and_then(serde_json::Value::as_str)
                        != Some("TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode")
                {
                    projection.invalid_guarantee_records += 1;
                    projection.truncated = true;
                    continue;
                }
                let Some(source_ordinal_path) = source_record
                    .get("source_ordinal_path")
                    .and_then(serde_json::Value::as_array)
                    .filter(|path| path.len() == 1)
                    .and_then(|path| {
                        path.iter()
                            .map(|index| index.as_u64().filter(|value| *value <= 10_000))
                            .collect::<Option<Vec<_>>>()
                    })
                else {
                    projection.invalid_guarantee_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let optional_text =
                    |key: &str, limit: usize, truncated: &mut bool| match source_record.get(key) {
                        None | Some(serde_json::Value::Null) => None,
                        Some(serde_json::Value::String(value)) => {
                            if value.chars().count() > limit {
                                *truncated = true;
                            }
                            Some(value.chars().take(limit).collect())
                        }
                        Some(_) => {
                            *truncated = true;
                            None
                        }
                    };
                projected_guarantees.push(ShadowLotRequiredFinancialGuarantee {
                    source_element: "RequiredFinancialGuarantee",
                    source_path: "TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode",
                    source_ordinal_path,
                    code: optional_text("code", 256, &mut projection.truncated),
                    code_list_name: optional_text("code_list_name", 128, &mut projection.truncated),
                    description: optional_text(
                        "description",
                        MAX_SHADOW_LOT_GUARANTEE_CURRICULA_TEXT_CHARS,
                        &mut projection.truncated,
                    ),
                });
                projected_guarantee_curricula_count += 1;
            }
            let mut projected_curricula = Vec::new();
            for source_record in source_curricula
                .iter()
                .take(MAX_SHADOW_LOT_CURRICULA_PER_LOT)
            {
                if projected_guarantee_curricula_count == MAX_SHADOW_LOT_GUARANTEE_CURRICULA_RECORDS
                {
                    projection.truncated = true;
                    break;
                }
                let Some(source_record) = source_record.as_object() else {
                    projection.invalid_curricula_records += 1;
                    projection.truncated = true;
                    continue;
                };
                if source_record
                    .get("source_element")
                    .and_then(serde_json::Value::as_str)
                    != Some("RequiredCurriculaCode")
                    || source_record
                        .get("source_path")
                        .and_then(serde_json::Value::as_str)
                        != Some("TenderingTerms/RequiredCurriculaCode")
                {
                    projection.invalid_curricula_records += 1;
                    projection.truncated = true;
                    continue;
                }
                let Some(source_ordinal_path) = source_record
                    .get("source_ordinal_path")
                    .and_then(serde_json::Value::as_array)
                    .filter(|path| path.len() == 1)
                    .and_then(|path| {
                        path.iter()
                            .map(|index| index.as_u64().filter(|value| *value <= 10_000))
                            .collect::<Option<Vec<_>>>()
                    })
                else {
                    projection.invalid_curricula_records += 1;
                    projection.truncated = true;
                    continue;
                };
                let optional_text =
                    |key: &str, limit: usize, truncated: &mut bool| match source_record.get(key) {
                        None | Some(serde_json::Value::Null) => None,
                        Some(serde_json::Value::String(value)) => {
                            if value.chars().count() > limit {
                                *truncated = true;
                            }
                            Some(value.chars().take(limit).collect())
                        }
                        Some(_) => {
                            *truncated = true;
                            None
                        }
                    };
                projected_curricula.push(ShadowLotRequiredCurriculum {
                    source_element: "RequiredCurriculaCode",
                    source_path: "TenderingTerms/RequiredCurriculaCode",
                    source_ordinal_path,
                    code: optional_text(
                        "code",
                        MAX_SHADOW_LOT_GUARANTEE_CURRICULA_TEXT_CHARS,
                        &mut projection.truncated,
                    ),
                    code_list_name: optional_text("code_list_name", 128, &mut projection.truncated),
                });
                projected_guarantee_curricula_count += 1;
            }
            guarantees_by_lot.insert(lot_id.clone(), projected_guarantees);
            curricula_by_lot.insert(lot_id, projected_curricula);
        }
    }
    projection.ambiguous_lot_id_count = ambiguous_lot_ids.len();

    let mut ceilings_by_lot = BTreeMap::<String, Vec<LotFrameworkMaximum>>::new();
    if let Some(records) = framework_records {
        projection.truncated |= records.len() > MAX_SHADOW_FRAMEWORK_EVIDENCE;
        for record in records.iter().take(MAX_SHADOW_FRAMEWORK_EVIDENCE) {
            let Some(object) = record.as_object() else {
                projection.truncated = true;
                continue;
            };
            let scope = object.get("scope").and_then(serde_json::Value::as_str);
            if scope == Some("notice") {
                projection.notice_level_framework_maximum_records += 1;
                continue;
            }
            if scope != Some("lot") {
                projection.truncated = true;
                continue;
            }
            let lot_id = object
                .get("lot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.chars().count() <= 128)
                .map(str::to_owned);
            let Some(lot_id) = lot_id else {
                projection.unkeyed_lot_framework_maximum_records += 1;
                continue;
            };
            let amount = object
                .get("amount")
                .and_then(serde_json::Value::as_str)
                .filter(|amount| !amount.trim().is_empty());
            let currency_id = object
                .get("currency_id")
                .and_then(serde_json::Value::as_str);
            if amount.is_none()
                || amount.is_some_and(|value| value.chars().count() > 128)
                || currency_id.is_some_and(|value| value.chars().count() > 16)
            {
                projection.invalid_lot_framework_maximum_records += 1;
                continue;
            }
            if cpv_evidence_truncated {
                projection.withheld_framework_maximum_records_due_to_truncation += 1;
                continue;
            }
            if ambiguous_lot_ids.contains(&lot_id) {
                projection.withheld_ambiguous_framework_maximum_records += 1;
                continue;
            }
            let values = ceilings_by_lot.entry(lot_id).or_default();
            if values.len() == MAX_SHADOW_LOT_FRAMEWORK_MAXIMUMS {
                projection.truncated = true;
                continue;
            }
            values.push(LotFrameworkMaximum {
                scope: "lot",
                amount: amount.expect("validated above").to_owned(),
                currency_id: currency_id.map(str::to_owned),
            });
        }
    }

    let mut lot_ids = cpvs_by_lot
        .keys()
        .chain(ceilings_by_lot.keys())
        .chain(places_by_lot.keys())
        .chain(award_criteria_by_lot.keys())
        .chain(selection_evidence_by_lot.keys())
        .chain(execution_conditions_by_lot.keys())
        .chain(subsequent_requirements_by_lot.keys())
        .chain(guarantees_by_lot.keys())
        .chain(curricula_by_lot.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    lot_ids.retain(|lot_id| !ambiguous_lot_ids.contains(lot_id));
    projection.truncated |= lot_ids.len() > MAX_SHADOW_LOT_FEATURES;
    for lot_id in lot_ids.into_iter().take(MAX_SHADOW_LOT_FEATURES) {
        let cpv_codes = cpvs_by_lot.remove(&lot_id).unwrap_or_default();
        let framework_maximums = ceilings_by_lot.remove(&lot_id).unwrap_or_default();
        let country_subentity_codes = places_by_lot.remove(&lot_id).unwrap_or_default();
        let award_criteria = award_criteria_by_lot.remove(&lot_id).unwrap_or_default();
        let selection_evidence = selection_evidence_by_lot
            .remove(&lot_id)
            .unwrap_or_default();
        let selection_evidence_coverage = if !selection_evidence.is_empty() {
            "selection_evidence_retained"
        } else if seen_selection_lot_ids.contains(&lot_id) {
            "no_selection_element_retained"
        } else {
            "not_recorded"
        };
        let contract_execution_conditions = execution_conditions_by_lot
            .remove(&lot_id)
            .unwrap_or_default();
        let contract_execution_conditions_coverage = if !contract_execution_conditions.is_empty() {
            "execution_conditions_retained"
        } else if seen_execution_lot_ids.contains(&lot_id) {
            "no_execution_condition_retained"
        } else {
            "not_recorded"
        };
        let subsequent_process_requirements = subsequent_requirements_by_lot
            .remove(&lot_id)
            .unwrap_or_default();
        let subsequent_process_requirements_coverage =
            if !subsequent_process_requirements.is_empty() {
                "subsequent_process_requirements_retained"
            } else if seen_subsequent_lot_ids.contains(&lot_id) {
                "no_subsequent_process_requirement_retained"
            } else {
                "not_recorded"
            };
        let required_financial_guarantees = guarantees_by_lot.remove(&lot_id).unwrap_or_default();
        let financial_guarantees_coverage = if !required_financial_guarantees.is_empty() {
            "financial_guarantees_retained"
        } else if seen_guarantee_curricula_lot_ids.contains(&lot_id) {
            "no_financial_guarantee_retained"
        } else {
            "not_recorded"
        };
        let required_curricula = curricula_by_lot.remove(&lot_id).unwrap_or_default();
        let curricula_coverage = if !required_curricula.is_empty() {
            "curricula_retained"
        } else if seen_guarantee_curricula_lot_ids.contains(&lot_id) {
            "no_curriculum_code_retained"
        } else {
            "not_recorded"
        };
        let award_criteria_coverage = if !award_criteria.is_empty() {
            "criteria_retained"
        } else if seen_criteria_lot_ids.contains(&lot_id) {
            "no_criterion_record_retained"
        } else {
            "not_recorded"
        };
        let place_coverage = if !country_subentity_codes.is_empty() {
            "region_code_present"
        } else if seen_place_lot_ids.contains(&lot_id) {
            "no_region_code"
        } else {
            "not_recorded"
        };
        let coverage = match (
            cpv_codes.is_empty(),
            framework_maximums.is_empty(),
            country_subentity_codes.is_empty(),
        ) {
            (false, false, false) => "cpv_framework_maximum_and_place",
            (false, false, true) => "cpv_and_framework_maximum",
            (false, true, false) => "cpv_and_place",
            (false, true, true) => "cpv_only",
            (true, false, false) => "framework_maximum_and_place",
            (true, false, true) => "framework_maximum_only",
            (true, true, false) => "place_only",
            (true, true, true) => "lot_identity_only",
        };
        projection.lots.push(ShadowTenderLotFeature {
            lot_id,
            cpv_nace_overlaps: cpv_nace_overlaps(company, &cpv_codes),
            nace_coverage: if company.activities.is_empty() {
                "company_activities_unavailable"
            } else {
                "evaluated"
            },
            cpv_codes,
            country_subentity_codes,
            place_coverage,
            framework_maximums,
            award_criteria,
            award_criteria_coverage,
            selection_evidence,
            selection_evidence_coverage,
            contract_execution_conditions,
            contract_execution_conditions_coverage,
            subsequent_process_requirements,
            subsequent_process_requirements_coverage,
            required_financial_guarantees,
            financial_guarantees_coverage,
            required_curricula,
            curricula_coverage,
            coverage,
        });
    }

    let has_gaps = projection.truncated
        || projection.unkeyed_cpv_records > 0
        || projection.cpv_records_without_valid_codes > 0
        || projection.unkeyed_lot_framework_maximum_records > 0
        || projection.invalid_lot_framework_maximum_records > 0
        || projection.withheld_ambiguous_framework_maximum_records > 0
        || projection.withheld_framework_maximum_records_due_to_truncation > 0
        || projection.ambiguous_lot_id_count > 0
        || projection.unkeyed_place_records > 0
        || projection.invalid_place_records > 0
        || projection.unkeyed_award_criteria_records > 0
        || projection.invalid_award_criteria_records > 0
        || projection.unkeyed_selection_records > 0
        || projection.invalid_selection_records > 0
        || projection.unkeyed_contract_execution_records > 0
        || projection.invalid_contract_execution_records > 0
        || projection.unkeyed_subsequent_process_records > 0
        || projection.invalid_subsequent_process_records > 0
        || projection.unkeyed_guarantee_records > 0
        || projection.invalid_guarantee_records > 0
        || projection.unkeyed_curricula_records > 0
        || projection.invalid_curricula_records > 0
        || projection.lots.iter().any(|lot| {
            lot.award_criteria_coverage == "not_recorded"
                || lot.contract_execution_conditions_coverage == "not_recorded"
                || lot.subsequent_process_requirements_coverage == "not_recorded"
                || lot.financial_guarantees_coverage == "not_recorded"
                || lot.curricula_coverage == "not_recorded"
        });
    projection.status = if projection.lots.is_empty() {
        if projection.ambiguous_lot_id_count > 0 {
            "no_unambiguous_lot_ids"
        } else {
            "no_keyed_lot_ids"
        }
    } else if has_gaps {
        "partial"
    } else {
        "available"
    };
    projection
}

pub fn is_eligible(tender: &Tender, today: NaiveDate, due_within: Option<i64>) -> bool {
    let deadline = tender
        .deadline
        .as_deref()
        .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok());
    if deadline.is_some_and(|date| date < today) {
        return false;
    }
    match (due_within, deadline) {
        (Some(days), Some(date)) => date <= today + Duration::days(days),
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn source_url(urls: &[String]) -> Option<String> {
    let linkable: Vec<&String> = urls
        .iter()
        .filter(|url| crate::is_web_link(url.as_str()))
        .collect();
    for language in ["/en/", "/nl/", "/fr/", "/de/"] {
        for suffix in ["/detail/", "/html", "/pdf", "/xml"] {
            if let Some(url) = linkable
                .iter()
                .find(|url| url.contains(language) && url.contains(suffix))
            {
                return Some((*url).clone());
            }
        }
    }
    linkable.first().map(|url| (*url).clone())
}

/// Rank order prefers more qualification evidence between otherwise equal scores.
///
/// No penalty is invented for `unknown`: measured on the 6,035-row store, 71% of
/// notices state no selection criteria, and a mere 5-point penalty reordered all
/// ten top positions — the ladder would then be sorting on a publication format
/// rather than on company fit. Between exactly equal scores, though, the notice
/// that states its criteria is the more actionable one, so it goes first.
fn evidence_cmp(left: &Recommendation, right: &Recommendation) -> Ordering {
    let rank = |item: &Recommendation| match item.feasibility.status {
        FeasibilityStatus::Stated => 0,
        FeasibilityStatus::Hinted => 1,
        FeasibilityStatus::Unknown => 2,
    };
    rank(left).cmp(&rank(right))
}

pub fn sort_by_score(recommendations: &mut [Recommendation]) {
    recommendations.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| evidence_cmp(left, right))
            .then_with(|| deadline_cmp(left.deadline.as_deref(), right.deadline.as_deref()))
    });
}

pub fn sort_by_deadline(recommendations: &mut [Recommendation]) {
    recommendations.sort_by(|left, right| {
        deadline_cmp(left.deadline.as_deref(), right.deadline.as_deref()).then_with(|| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(Ordering::Equal)
        })
    });
}

fn deadline_cmp(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

pub fn rank(
    company: &CompanyProfile,
    tenders: &[Tender],
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
    feedback: &HashMap<(String, String), bool>,
    limit: usize,
) -> Vec<Recommendation> {
    rank_on(
        company,
        tenders,
        history,
        preferences,
        feedback,
        limit,
        chrono::Local::now().date_naive(),
    )
}

/// As `rank`, scoring deadline runway against an explicit "today".
pub fn rank_on(
    company: &CompanyProfile,
    tenders: &[Tender],
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
    feedback: &HashMap<(String, String), bool>,
    limit: usize,
    today: NaiveDate,
) -> Vec<Recommendation> {
    // One slot per procurement: dossier members collapse before scoring,
    // so file renderings never outnumber real notices in the ladder.
    let group_sizes: std::collections::HashMap<String, usize> = {
        let mut sizes = std::collections::HashMap::new();
        for tender in tenders.iter() {
            *sizes.entry(crate::dossier_group_key(tender)).or_default() += 1;
        }
        sizes
    };
    let tenders = crate::collapse_dossier_members(tenders.to_vec());
    let mut recommendations = tenders
        .iter()
        .map(|tender| {
            let result = match_company_to_tender_on(company, tender, history, preferences, today);
            let feasibility = feasibility_evidence(tender, history);
            let framework_maximum_amounts = reported_framework_maximum_amounts(tender);
            let lot_cpv_alignment = derive_lot_cpv_alignment(company, tender);
            let mut warnings = result.warnings;
            if let Some(warning) = framework_maximum_warning(&framework_maximum_amounts) {
                warnings.push(warning);
            }
            if feasibility.status == FeasibilityStatus::Unknown {
                warnings.push(UNSTATED_FEASIBILITY_WARNING.to_owned());
            }
            let mut item = Recommendation {
                source: tender.source.clone(),
                external_id: tender.external_id.clone(),
                title: tender.title.clone(),
                buyer: tender.buyer.clone(),
                deadline: tender.deadline.clone(),
                estimated_value: tender.estimated_value,
                framework_maximum_amounts,
                regions: tender.regions.clone(),
                cpv_codes: tender.cpv_codes.clone(),
                lot_cpv_alignment,
                languages: tender.languages.clone(),
                source_url: source_url(&tender.document_urls),
                requirements: tender.requirements.clone(),
                requirement_signals: analyze_requirements(&tender.requirements),
                score: result.score,
                confidence: result.confidence,
                shadow_activity_text_similarity: None,
                shadow_activity_text_similarity_coverage: None,
                shadow_geographic_evidence: None,
                dossier_members: group_sizes
                    .get(&crate::dossier_group_key(tender))
                    .copied()
                    .unwrap_or(1),
                feasibility,
                reasons: result.reasons,
                warnings,
                jev_relevance: None,
                jev_qualification_risk: None,
            };
            if let Some(relevant) =
                feedback.get(&(tender.source.clone(), tender.external_id.clone()))
            {
                item.score = (item.score + if *relevant { 10.0 } else { -20.0 }).clamp(0.0, 100.0);
                item.reasons.push(if *relevant {
                    "Previously marked relevant".to_owned()
                } else {
                    "Previously marked not relevant".to_owned()
                });
            }
            item
        })
        .collect::<Vec<_>>();
    sort_by_score(&mut recommendations);
    recommendations.truncate(limit);
    recommendations
}

/// Score penalty deducted from contracts exceeding company turnover or bid capacity (FW-09).
pub const CAPACITY_TRIAGE_PENALTY: f64 = 30.0;

/// Economic and bid capacity limits for capacity-aware triage (FW-09).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CapacityLimits {
    pub annual_turnover_eur: Option<f64>,
    pub max_bid_capacity_eur: Option<f64>,
}

impl CapacityLimits {
    pub fn new(annual_turnover_eur: Option<f64>, max_bid_capacity_eur: Option<f64>) -> Self {
        Self {
            annual_turnover_eur,
            max_bid_capacity_eur,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.annual_turnover_eur.is_none() && self.max_bid_capacity_eur.is_none()
    }
}

/// Applies capacity-aware feasibility auditing and triage penalties (FW-09).
///
/// 1. Verifies minimum turnover requirement warnings against `annual_turnover_eur`.
/// 2. Verifies contract estimated/assessed values against `max_bid_capacity_eur`.
/// 3. Applies a 30-point score penalty and appends a `Capaciteitsbelemmering` warning
///    to inaccessible notices.
/// 4. Returns a map of `(source, external_id) -> barrier_type` (`turnover_too_high`, `capacity_exceeded`).
pub fn apply_capacity_triage(
    recommendations: &mut [Recommendation],
    tenders_by_key: &HashMap<(String, String), &Tender>,
    capacity: CapacityLimits,
    turnover_provenance: Option<&crate::store::NbbTurnoverProvenance>,
    triage_requested: bool,
) -> HashMap<(String, String), String> {
    let mut barriers = HashMap::new();
    if capacity.is_empty() && !triage_requested {
        return barriers;
    }

    let max_bid_cap = capacity.max_bid_capacity_eur;
    let annual_turnover = capacity.annual_turnover_eur;

    for rec in recommendations.iter_mut() {
        if let Some(tender) = tenders_by_key
            .get(&(rec.source.clone(), rec.external_id.clone()))
            .copied()
        {
            // The turnover signal is the warning, because the warning is
            // produced from the matcher tables' Dutch, French and English
            // terms. Re-scanning the text here with a Dutch literal would be a
            // second, narrower detector that silently misses "chiffre
            // d'affaires" and "turnover".
            let has_turnover_warning = rec
                .warnings
                .iter()
                .any(|w| w.contains("Minimum turnover requirement"));
            let val = tender
                .assessed_value()
                .or(tender.estimated_value)
                .and_then(|d| d.to_f64());

            // A stated turnover requirement is checked only when the company's
            // turnover is known. Silence would read as "it fits", which is the
            // mistake the unstated-criteria warning already guards against on
            // the tender side, so the unknown case says so instead.
            if has_turnover_warning && annual_turnover.is_none() && triage_requested {
                // A provenance with no figure means a filing existed and was
                // withheld, which is a different fact from having no filing at
                // all; the warning says which.
                let note = match turnover_provenance {
                    Some(provenance) => format!(
                        "Capaciteit onbekend: de jaaromzet uit boekjaar FY{} is te oud voor de ingestelde drempel, dus de omzetvereiste is niet getoetst",
                        provenance.fiscal_year
                    ),
                    None => "Capaciteit onbekend: geen bruikbare jaaromzet voor deze kmo, dus de omzetvereiste is niet getoetst".to_owned(),
                };
                rec.warnings.push(note);
            }

            if let Some(tender_val) = val {
                if let Some(annual_turnover) = annual_turnover {
                    if has_turnover_warning && tender_val > annual_turnover {
                        barriers.insert(
                            (rec.source.clone(), rec.external_id.clone()),
                            "turnover_too_high".to_owned(),
                        );
                        let provenance = turnover_provenance
                            .map(|provenance| format!(", {}", provenance.short_clause()))
                            .unwrap_or_default();
                        rec.warnings.push(format!(
                            "Capaciteitsbelemmering: opdrachtwaarde (€ {tender_val:.0}) overschrijdt jaaromzet van de kmo (€ {annual_turnover:.0}{provenance}), terwijl het bericht een omzetvereiste vermeldt"
                        ));
                        rec.score = (rec.score - CAPACITY_TRIAGE_PENALTY).max(0.0);
                        continue;
                    }
                }

                if let Some(max_bid_cap) = max_bid_cap {
                    if tender_val > max_bid_cap {
                        barriers.insert(
                            (rec.source.clone(), rec.external_id.clone()),
                            "capacity_exceeded".to_owned(),
                        );
                        rec.warnings.push(format!(
                            "Capaciteitsbelemmering: geraamde waarde (€ {tender_val:.0}) overschrijdt maximale kmo-inschrijvingscapaciteit (€ {max_bid_cap:.0})"
                        ));
                        rec.score = (rec.score - CAPACITY_TRIAGE_PENALTY).max(0.0);
                    }
                }
            }
        }
    }

    barriers
}

/// Variant of `rank` that enforces capacity-aware triage limits (FW-09).
pub fn rank_with_capacity(
    company: &CompanyProfile,
    tenders: &[Tender],
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
    feedback: &HashMap<(String, String), bool>,
    capacity: CapacityLimits,
    limit: usize,
) -> Vec<Recommendation> {
    let mut recommendations = rank(
        company,
        tenders,
        history,
        preferences,
        feedback,
        tenders.len(),
    );
    if !capacity.is_empty() {
        let tenders_by_key: HashMap<(String, String), &Tender> = tenders
            .iter()
            .map(|tender| ((tender.source.clone(), tender.external_id.clone()), tender))
            .collect();
        // Convenience wrapper: it has no filing behind the limits it is given, so
        // its warnings cannot cite one. Callers that hold provenance (the CLI and
        // the API) call `rank` then `apply_capacity_triage` with it instead.
        apply_capacity_triage(&mut recommendations, &tenders_by_key, capacity, None, false);
        sort_by_score(&mut recommendations);
    }
    recommendations.truncate(limit);
    recommendations
}

/// Reverse search: one company's standing for one tender, for buyer-side
/// prospecting ("who could bid on this?"). The score, confidence, reasons
/// and warnings are exactly what the forward ranking would compute for this
/// pair — `rank` on the single tender — plus the tie-break keys the
/// prospect order documents: topical text overlap (which separates companies
/// inside a coarse division-match tier) and the region signal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompanyProspect {
    pub enterprise_number: String,
    pub name: Option<String>,
    pub nace_codes: Vec<String>,
    pub municipality: Option<String>,
    pub zip: Option<String>,
    pub score: f64,
    pub confidence: f64,
    pub text_score: f64,
    pub region_signal: f64,
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
}

/// Score one company against one tender, mirroring `rank` on a single
/// tender. Infallible by construction (`rank` maps every tender to exactly
/// one recommendation); the direct return says so instead of suggesting an
/// unscorable-pair case that does not exist.
///
/// The tie-break keys use the same inputs as the headline score — notably
/// the real preferences, not defaults — so a judged company's confirmed
/// activities order it the way the score itself saw it.
pub fn score_prospect(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
    feedback: &HashMap<(String, String), bool>,
) -> CompanyProspect {
    let mut ladder = rank(
        company,
        std::slice::from_ref(tender),
        history,
        preferences,
        feedback,
        1,
    );
    let item = ladder.pop().expect("rank maps every tender to one item");
    let breakdown = crate::score_breakdown(company, tender, history, preferences);
    CompanyProspect {
        enterprise_number: company.enterprise_number.clone(),
        name: company.names.first().cloned(),
        nace_codes: company
            .activities
            .iter()
            .map(|activity| activity.code.clone())
            .collect(),
        municipality: company
            .addresses
            .first()
            .and_then(|address| address.municipality_nl.clone()),
        zip: company
            .addresses
            .first()
            .and_then(|address| address.zipcode.clone()),
        score: item.score,
        confidence: item.confidence,
        text_score: crate::activity_text_score_with(company, tender, preferences),
        region_signal: breakdown.region.signal,
        reasons: item.reasons,
        warnings: item.warnings,
    }
}

/// Fuse one cached or fresh analysis decision into a prospect, mirroring
/// [`apply_jev_decision`]: a strong relevance adds the bonus (capped at 100),
/// a poor one subtracts the penalty (floored at 0); any other relevance only
/// records the reason. Shared by the CLI (which persists fresh decisions)
/// and the API (which only reuses cache hits).
pub fn apply_prospect_decision(prospect: &mut CompanyProspect, decision: &AnalysisDecision) {
    if decision.relevance.as_deref() == Some("strong") {
        prospect.score = (prospect.score + JEV_STRONG_BONUS).min(100.0);
    } else if decision.relevance.as_deref() == Some("poor") {
        prospect.score = (prospect.score - JEV_POOR_PENALTY).max(0.0);
    }
    prospect.reasons.push(format!(
        "Jev relevance: {}",
        decision.relevance.as_deref().unwrap_or("None")
    ));
}

/// Deterministic prospect order: score, then topical similarity, then region
/// signal, then enterprise number. The tail keys exist because the headline
/// score ties widely on metadata-poor tenders (measured 2026-10-23: 9,752 of
/// 19,702 Brussels IT companies tied at 66.8 on a PDF-imported notice);
/// without them the top-N is HashMap order. The similarity key assumes only
/// monotonicity (higher = more topical), never a particular measure.
pub fn sort_prospects(prospects: &mut [CompanyProspect]) {
    prospects.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then(
                right
                    .text_score
                    .partial_cmp(&left.text_score)
                    .unwrap_or(Ordering::Equal),
            )
            .then(
                right
                    .region_signal
                    .partial_cmp(&left.region_signal)
                    .unwrap_or(Ordering::Equal),
            )
            .then(left.enterprise_number.cmp(&right.enterprise_number))
    });
}

/// Maximum reverse-search pool: scoring is in-memory but each prospect still
/// costs profile loading, so an unbounded pool is a slow lorry, not a query.
pub const MAX_PROSPECT_CANDIDATES: usize = 100_000;

/// Default reverse-search pool cap: nationwide NACE pools reach six figures,
/// which is minutes per query. Callers opt into more explicitly.
pub const DEFAULT_PROSPECT_CANDIDATES: usize = 20_000;

/// Stored ranking context per company for [`rank_prospects`]: preferences,
/// award history and feedback. Companies absent from the map score with
/// defaults, which is exactly what per-company loading would read for them.
pub type ProspectContext = (
    CompanyPreferences,
    Vec<AwardRecord>,
    HashMap<(String, String), bool>,
);

/// Score every company against one tender with the forward ranking function,
/// in deterministic prospect order. Contexts for judged companies come from
/// the map; everyone else scores with defaults.
pub fn rank_prospects(
    companies: &[CompanyProfile],
    tender: &Tender,
    contexts: &HashMap<String, ProspectContext>,
) -> Vec<CompanyProspect> {
    let default_context: ProspectContext =
        (CompanyPreferences::default(), Vec::new(), HashMap::new());
    let mut prospects = companies
        .iter()
        .map(|company| {
            let (preferences, history, feedback) = contexts
                .get(&company.enterprise_number)
                .unwrap_or(&default_context);
            score_prospect(company, tender, history, preferences, feedback)
        })
        .collect::<Vec<_>>();
    sort_prospects(&mut prospects);
    prospects
}

/// Optional hard region filter for reverse search: a Belgian zip prefix
/// (`1` matches all of `1xxx`, i.e. Brussels) or a municipality-name
/// substring, case-insensitive. A commune name only matches itself —
/// `etterbeek` does not imply `schaarbeek` — so prefer the zip form for
/// regional scoping and read the prospect's municipality in the output.
pub fn company_matches_region_filter(company: &CompanyProfile, filter: &str) -> bool {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return true;
    }
    company.addresses.iter().any(|address| {
        address
            .zipcode
            .as_deref()
            .is_some_and(|zip| zip.replace(' ', "").to_lowercase().starts_with(&filter))
            || address
                .municipality_nl
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(&filter))
            || address
                .municipality_fr
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(&filter))
    })
}

/// NACE prefixes scoping a reverse-search pool from the tender's CPV codes:
/// exact-class mappings where the class table knows them, else the broader
/// division prefixes (the same evidence ladder the forward activity match
/// climbs). Sorted and deduplicated. Empty when the tender carries no CPV
/// codes: without sector evidence the pool would be the whole country, so
/// callers must fail or take an explicit `--nace` instead of silently
/// scoring millions of companies.
pub fn nace_prefixes_for_tender(tender: &Tender) -> Vec<String> {
    let mut prefixes: Vec<String> = Vec::new();
    for cpv in &tender.cpv_codes {
        prefixes.extend(crate::nace_prefixes_for_cpv(cpv));
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Activity, Address};

    fn company() -> CompanyProfile {
        CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![Activity {
                code: "43210".to_owned(),
                classification: "MAIN".to_owned(),
                french: Some("Electrical installation".to_owned()),
                dutch: Some("Elektrische installatie".to_owned()),
            }],
            addresses: vec![Address::default()],
            establishments: vec![],
            ..Default::default()
        }
    }

    fn prospect_tender() -> Tender {
        Tender {
            source: "bda".to_owned(),
            external_id: "notice-1".to_owned(),
            title: "Hosting en onderhoud website".to_owned(),
            description: "hosting onderhoud website ondersteuning".to_owned(),
            cpv_codes: vec!["72415000".to_owned()],
            ..Tender::default()
        }
    }

    /// The pool follows the same evidence ladder as the forward activity
    /// match: class mapping where known, else division prefixes. No CPV
    /// means no pool rather than the whole country.
    #[test]
    fn tender_cpv_derives_nace_pool_prefixes() {
        let prefixes = nace_prefixes_for_tender(&prospect_tender());
        for expected in ["58", "61", "62", "63", "78"] {
            assert!(
                prefixes.contains(&expected.to_owned()),
                "missing {expected}: {prefixes:?}"
            );
        }
        let bare = Tender {
            cpv_codes: vec![],
            ..Tender::default()
        };
        assert!(nace_prefixes_for_tender(&bare).is_empty());
    }

    /// Prospect scores must equal what the forward ranking computes for the
    /// same pair, or the two directions tell different stories — including
    /// for judged companies, whose confirmed activities feed both the score
    /// and the tie-break (review I2: defaults there once ordered by a signal
    /// the score never saw).
    #[test]
    fn prospect_score_matches_forward_rank() {
        let tender = prospect_tender();
        let company = company();
        let history = vec![];
        let preferences = CompanyPreferences::default();
        let feedback = HashMap::new();
        let ladder = rank(
            &company,
            std::slice::from_ref(&tender),
            &history,
            &preferences,
            &feedback,
            1,
        );
        let prospect = score_prospect(&company, &tender, &history, &preferences, &feedback);
        assert_eq!(prospect.score, ladder[0].score);
        assert_eq!(prospect.enterprise_number, "0123.456.789");
        assert_eq!(prospect.nace_codes, vec!["43210".to_owned()]);
        let judged_preferences = CompanyPreferences {
            confirmed_activities: vec!["hosting website onderhoud".to_owned()],
            ..CompanyPreferences::default()
        };
        let judged_ladder = rank(
            &company,
            std::slice::from_ref(&tender),
            &history,
            &judged_preferences,
            &feedback,
            1,
        );
        let judged = score_prospect(&company, &tender, &history, &judged_preferences, &feedback);
        assert_eq!(judged.score, judged_ladder[0].score);
        // Epsilon, not exact equality: the similarity sums in HashMap
        // iteration order, so two evaluations can differ by 1 ulp.
        assert!(
            (judged.text_score
                - crate::activity_text_score_with(&company, &tender, &judged_preferences))
            .abs()
                < 1e-12,
            "tie-break must use the same preferences as the score"
        );
    }

    /// The headline score ties widely on metadata-poor tenders, so the order
    /// falls through text overlap, region signal and enterprise number — and
    /// is deterministic, never HashMap order.
    #[test]
    fn prospect_order_is_score_then_text_then_region_then_id() {
        let base = CompanyProspect {
            enterprise_number: "0000.000.002".to_owned(),
            name: None,
            nace_codes: vec![],
            municipality: None,
            zip: None,
            score: 50.0,
            confidence: 0.5,
            text_score: 0.2,
            region_signal: 0.5,
            reasons: vec![],
            warnings: vec![],
        };
        let higher = CompanyProspect {
            enterprise_number: "0000.000.004".to_owned(),
            score: 60.0,
            ..base.clone()
        };
        let better_text = CompanyProspect {
            enterprise_number: "0000.000.001".to_owned(),
            text_score: 0.9,
            ..base.clone()
        };
        let better_region = CompanyProspect {
            enterprise_number: "0000.000.003".to_owned(),
            text_score: 0.9,
            region_signal: 1.0,
            ..base.clone()
        };
        let mut prospects = vec![
            base.clone(),
            better_text.clone(),
            better_region.clone(),
            higher.clone(),
        ];
        sort_prospects(&mut prospects);
        let order: Vec<&str> = prospects
            .iter()
            .map(|prospect| prospect.enterprise_number.as_str())
            .collect();
        assert_eq!(
            order,
            vec![
                "0000.000.004",
                "0000.000.003",
                "0000.000.001",
                "0000.000.002"
            ]
        );
    }

    /// Zip prefixes scope regions (`1` is all of Brussels); names match one
    /// commune only, never its neighbours.
    #[test]
    fn region_filter_matches_zip_prefix_or_commune() {
        let mut brussels = company();
        brussels.addresses = vec![Address {
            address_type: "REGO".to_owned(),
            zipcode: Some("1040".to_owned()),
            municipality_nl: Some("Etterbeek".to_owned()),
            municipality_fr: Some("Etterbeek".to_owned()),
            ..Address::default()
        }];
        assert!(company_matches_region_filter(&brussels, "1"));
        assert!(company_matches_region_filter(&brussels, "104"));
        assert!(company_matches_region_filter(&brussels, "etterbeek"));
        assert!(!company_matches_region_filter(&brussels, "2"));
        assert!(!company_matches_region_filter(&brussels, "schaarbeek"));
        assert!(company_matches_region_filter(&brussels, ""));
    }

    fn prospect_decision(relevance: Option<&str>) -> AnalysisDecision {
        AnalysisDecision {
            activity_match: None,
            geography_match: None,
            previous_work_match: None,
            qualification_risk: None,
            relevance: relevance.map(str::to_owned),
            raw: std::collections::BTreeMap::new(),
            prompt_version: None,
            provider: "test".to_owned(),
            cached: false,
        }
    }

    /// Deep rerank bands must mirror the forward fusion: strong +15 (capped),
    /// poor −15 (floored), anything else reason-only.
    #[test]
    fn prospect_decision_fusion_applies_bonus_penalty_and_reasons() {
        let mut base = CompanyProspect {
            enterprise_number: "0000.000.001".to_owned(),
            name: None,
            nace_codes: vec![],
            municipality: None,
            zip: None,
            score: 95.0,
            confidence: 0.5,
            text_score: 0.0,
            region_signal: 0.0,
            reasons: vec![],
            warnings: vec![],
        };
        let mut strong = base.clone();
        apply_prospect_decision(&mut strong, &prospect_decision(Some("strong")));
        assert_eq!(strong.score, 100.0);
        assert!(strong
            .reasons
            .iter()
            .any(|reason| reason.contains("strong")));
        let mut poor = base.clone();
        apply_prospect_decision(&mut poor, &prospect_decision(Some("poor")));
        assert_eq!(poor.score, 80.0);
        let mut middling = base.clone();
        apply_prospect_decision(&mut middling, &prospect_decision(Some("possible")));
        assert_eq!(middling.score, 95.0);
        assert_eq!(middling.reasons.len(), 1);
        base.score = 5.0;
        let mut floored = base.clone();
        apply_prospect_decision(&mut floored, &prospect_decision(Some("poor")));
        assert_eq!(floored.score, 0.0);
    }

    /// One dossier's files collapse to the structured notice before scoring:
    /// six file rows and one notice enter, one candidate leaves, and it says
    /// how many files it stands for.
    #[test]
    fn rank_collapses_dossier_members_to_one_slot() {
        let member = |id: &str| Tender {
            source: "bda".to_owned(),
            external_id: id.to_owned(),
            title: "Bestek".to_owned(),
            description: "dienstverlening".to_owned(),
            raw: serde_json::json!({
                "contract_folder_id": "FOLDER-9",
                "dossier_container": "dossier-1",
            }),
            ..Tender::default()
        };
        let solo = Tender {
            source: "bda".to_owned(),
            external_id: "solo".to_owned(),
            title: "Overeenkomst".to_owned(),
            ..Tender::default()
        };
        let ranked = rank(
            &company(),
            &[member("a.pdf"), member("b.pdf"), solo],
            &[],
            &CompanyPreferences::default(),
            &HashMap::new(),
            10,
        );
        assert_eq!(ranked.len(), 2, "two procurements, two slots");
        let group = ranked
            .iter()
            .find(|item| item.external_id == "a.pdf")
            .expect("first file stands for the group");
        assert_eq!(group.dossier_members, 2);
        let solo = ranked
            .iter()
            .find(|item| item.external_id == "solo")
            .expect("standalone tender keeps its slot");
        assert_eq!(solo.dossier_members, 1);
    }

    #[test]
    fn lot_cpv_alignment_uses_class_then_division_crosswalk_without_changing_score() {
        let tender = Tender {
            source: "bda".to_owned(),
            external_id: "lot-crosswalk".to_owned(),
            cpv_codes: vec!["45221100".to_owned()],
            raw: serde_json::json!({
                "notice_lot_cpv_evidence": [{
                    "lot_id": "LOT-1",
                    "cpv_codes": ["45221100", "45221100", "invalid"]
                }]
            }),
            ..Tender::default()
        };
        let class_company = CompanyProfile {
            activities: vec![Activity {
                code: "42110".to_owned(),
                classification: "MAIN".to_owned(),
                french: None,
                dutch: None,
            }],
            ..company()
        };
        let alignment = derive_lot_cpv_alignment(&class_company, &tender).unwrap();
        assert_eq!(alignment.lots.len(), 1);
        assert_eq!(alignment.lots[0].lot_id.as_deref(), Some("LOT-1"));
        assert_eq!(alignment.lots[0].cpv_codes, ["45221100"]);
        assert_eq!(alignment.lots[0].overlaps.len(), 1);
        assert_eq!(
            alignment.lots[0].overlaps[0].match_level,
            CpvNaceMatchLevel::Class
        );
        assert_eq!(alignment.lots[0].overlaps[0].nace_codes, ["4211"]);
        let mut projected_truncated = tender.clone();
        projected_truncated.raw["notice_lot_cpv_evidence_truncated"] = serde_json::json!(true);
        assert!(
            derive_lot_cpv_alignment(&class_company, &projected_truncated)
                .unwrap()
                .truncated
        );

        let division_company = CompanyProfile {
            activities: vec![Activity {
                code: "43110".to_owned(),
                classification: "MAIN".to_owned(),
                french: None,
                dutch: None,
            }],
            ..company()
        };
        let division = derive_lot_cpv_alignment(&division_company, &tender).unwrap();
        assert_eq!(
            division.lots[0].overlaps[0].match_level,
            CpvNaceMatchLevel::Division
        );
        assert_eq!(division.lots[0].overlaps[0].nace_codes, ["43"]);

        let no_overlap_company = CompanyProfile {
            activities: vec![Activity {
                code: "62010".to_owned(),
                classification: "MAIN".to_owned(),
                french: None,
                dutch: None,
            }],
            ..company()
        };
        let no_overlap = derive_lot_cpv_alignment(&no_overlap_company, &tender).unwrap();
        assert!(no_overlap.lots[0].overlaps.is_empty());

        let base = Tender {
            raw: serde_json::json!({}),
            ..tender.clone()
        };
        let preferences = CompanyPreferences::default();
        let feedback = HashMap::new();
        let without = rank(&class_company, &[base], &[], &preferences, &feedback, 1);
        let with = rank(&class_company, &[tender], &[], &preferences, &feedback, 1);
        assert_eq!(with[0].score, without[0].score);
        assert!(with[0].lot_cpv_alignment.is_some());
        assert!(without[0].lot_cpv_alignment.is_none());
    }

    #[test]
    fn shadow_lot_features_join_only_unambiguous_exact_source_ids() {
        let tender = Tender {
            source: "bda".to_owned(),
            raw: serde_json::json!({
                "notice_lot_ids_unique": true,
                "framework_maximum_amounts": ["900000.00", "350000.00"],
                "notice_lot_cpv_evidence": [
                    {"lot_id": "LOT-2", "cpv_codes": ["71300000"]},
                    {"lot_id": "LOT-1", "cpv_codes": ["45221100", "invalid"]},
                    {"lot_id": null, "cpv_codes": ["45221100"]}
                ],
                "notice_lot_place_evidence": [
                    {"lot_id": "LOT-1", "country_subentity_codes": ["BE231"]},
                    {"lot_id": "LOT-2", "country_subentity_codes": []},
                    {"lot_id": "LOT-P", "country_subentity_codes": ["BE100"]}
                ],
                "notice_lot_place_evidence_truncated": false,
                "notice_lot_award_criteria_evidence": [
                    {"lot_id": "LOT-1", "criteria": [
                        {"source_element": "AwardingCriterion", "source_ordinal_path": [0], "type_code": null, "description": null, "calculation_expression": null, "parameters": []},
                        {"source_element": "SubordinateAwardingCriterion", "source_ordinal_path": [0, 0], "type_code": "QUALITY", "description": "Service quality", "calculation_expression": "raw expression", "parameters": [{"code": "WEIGHT", "numeric": "35"}]}
                    ]},
                    {"lot_id": "LOT-2", "criteria": []},
                    {"lot_id": null, "criteria": []}
                ],
                "notice_lot_award_criteria_evidence_truncated": false,
                "notice_lot_selection_evidence": [
                    {"lot_id": "LOT-1", "selection_evidence": [
                        {"source_element": "SpecificTendererRequirement", "source_path": "TenderingTerms/TendererQualificationRequest/SpecificTendererRequirement", "source_ordinal_path": [0, 1], "code": "TECHNICAL_ABILITY", "code_list_name": "UBL requirement types", "descriptions": []}
                    ]},
                    {"lot_id": "LOT-2", "selection_evidence": []}
                ],
                "notice_lot_selection_evidence_truncated": false,
                "framework_maximum_evidence": [
                    {"amount": "900000.00", "currency_id": "EUR", "scope": "notice", "lot_id": null},
                    {"amount": "350000.00", "currency_id": "EUR", "scope": "lot", "lot_id": "LOT-1"},
                    {"amount": "100000.00", "currency_id": null, "scope": "lot", "lot_id": "LOT-X"},
                    {"amount": "200000.00", "currency_id": "EUR", "scope": "lot", "lot_id": null}
                ]
            }),
            ..prospect_tender()
        };
        let class_company = CompanyProfile {
            activities: vec![Activity {
                code: "42110".to_owned(),
                classification: "MAIN".to_owned(),
                french: None,
                dutch: None,
            }],
            ..company()
        };
        let projection = shadow_lot_feature_projection(&class_company, &tender);
        assert_eq!(projection.status, "partial");
        assert_eq!(projection.association_rule, "exact_source_lot_id");
        assert!(projection.flat_framework_amounts_present);
        assert_eq!(projection.notice_level_framework_maximum_records, 1);
        assert_eq!(projection.unkeyed_cpv_records, 1);
        assert_eq!(projection.unkeyed_lot_framework_maximum_records, 1);
        assert_eq!(projection.unkeyed_award_criteria_records, 1);
        assert_eq!(
            projection.unprojected_dimensions,
            [
                "lot_contract_execution_requirements",
                "lot_guarantees_and_curricula",
                "lot_subsequent_process_requirements"
            ]
        );
        assert_eq!(
            projection
                .lots
                .iter()
                .map(|lot| lot.lot_id.as_str())
                .collect::<Vec<_>>(),
            ["LOT-1", "LOT-2", "LOT-P", "LOT-X"]
        );
        let lot_one = &projection.lots[0];
        assert_eq!(lot_one.coverage, "cpv_framework_maximum_and_place");
        assert_eq!(lot_one.country_subentity_codes, ["BE231"]);
        assert_eq!(lot_one.place_coverage, "region_code_present");
        assert_eq!(lot_one.cpv_codes, ["45221100"]);
        assert_eq!(lot_one.framework_maximums.len(), 1);
        assert_eq!(lot_one.framework_maximums[0].scope, "lot");
        assert_eq!(lot_one.framework_maximums[0].amount, "350000.00");
        assert_eq!(
            lot_one.framework_maximums[0].currency_id.as_deref(),
            Some("EUR")
        );
        assert_eq!(lot_one.cpv_nace_overlaps.len(), 1);
        assert_eq!(lot_one.cpv_nace_overlaps[0].nace_codes, ["4211"]);
        assert_eq!(lot_one.award_criteria.len(), 2);
        assert_eq!(lot_one.award_criteria[0].source_ordinal_path, [0]);
        assert_eq!(lot_one.award_criteria[1].source_ordinal_path, [0, 0]);
        assert_eq!(
            lot_one.award_criteria[1].source_element,
            "SubordinateAwardingCriterion"
        );
        assert_eq!(
            lot_one.award_criteria[1].type_code.as_deref(),
            Some("QUALITY")
        );
        assert_eq!(
            lot_one.award_criteria[1].description.as_deref(),
            Some("Service quality")
        );
        assert_eq!(
            lot_one.award_criteria[1].calculation_expression.as_deref(),
            Some("raw expression")
        );
        assert_eq!(
            lot_one.award_criteria[1].parameters[0].numeric.as_deref(),
            Some("35")
        );
        assert_eq!(lot_one.award_criteria_coverage, "criteria_retained");
        assert_eq!(lot_one.selection_evidence.len(), 1);
        assert_eq!(
            lot_one.selection_evidence[0].source_element,
            "SpecificTendererRequirement"
        );
        assert_eq!(
            lot_one.selection_evidence[0].code.as_deref(),
            Some("TECHNICAL_ABILITY")
        );
        assert_eq!(
            lot_one.selection_evidence[0].code_list_name.as_deref(),
            Some("UBL requirement types")
        );
        assert_eq!(
            lot_one.selection_evidence_coverage,
            "selection_evidence_retained"
        );
        assert!(lot_one.contract_execution_conditions.is_empty());
        assert_eq!(
            lot_one.contract_execution_conditions_coverage,
            "not_recorded"
        );
        assert_eq!(projection.lots[1].coverage, "cpv_only");
        assert_eq!(
            projection.lots[1].award_criteria_coverage,
            "no_criterion_record_retained"
        );
        assert_eq!(
            projection.lots[1].selection_evidence_coverage,
            "no_selection_element_retained"
        );
        assert_eq!(
            projection.lots[1].contract_execution_conditions_coverage,
            "not_recorded"
        );
        assert_eq!(projection.lots[1].place_coverage, "no_region_code");
        assert!(projection.lots[1].framework_maximums.is_empty());
        assert_eq!(projection.lots[2].coverage, "place_only");
        assert_eq!(projection.lots[2].country_subentity_codes, ["BE100"]);
        assert_eq!(projection.lots[2].award_criteria_coverage, "not_recorded");
        assert_eq!(projection.lots[3].coverage, "framework_maximum_only");
        assert_eq!(projection.lots[3].place_coverage, "not_recorded");
        assert!(projection.lots[3].cpv_codes.is_empty());

        let mut ambiguous = tender.clone();
        ambiguous.raw["notice_lot_cpv_evidence"] = serde_json::json!([
            {"lot_id": "DUP", "cpv_codes": ["45221100"]},
            {"lot_id": "DUP", "cpv_codes": ["71300000"]}
        ]);
        ambiguous.raw["framework_maximum_evidence"] = serde_json::json!([
            {"amount": "250.00", "currency_id": "EUR", "scope": "lot", "lot_id": "DUP"}
        ]);
        ambiguous.raw["notice_lot_place_evidence"] = serde_json::json!([
            {"lot_id": "DUP", "country_subentity_codes": ["BE231"]},
            {"lot_id": "DUP", "country_subentity_codes": ["BE232"]}
        ]);
        ambiguous.raw["notice_lot_award_criteria_evidence"] = serde_json::json!([
            {"lot_id": "DUP", "criteria": []},
            {"lot_id": "DUP", "criteria": []}
        ]);
        ambiguous.raw["notice_lot_selection_evidence"] = serde_json::json!([
            {"lot_id": "DUP", "selection_evidence": []},
            {"lot_id": "DUP", "selection_evidence": []}
        ]);
        let projection = shadow_lot_feature_projection(&class_company, &ambiguous);
        assert_eq!(projection.status, "no_unambiguous_lot_ids");
        assert_eq!(projection.ambiguous_lot_id_count, 1);
        assert_eq!(projection.withheld_ambiguous_framework_maximum_records, 1);
        assert!(projection.lots.is_empty());

        let mut truncated = tender.clone();
        truncated.raw["notice_lot_cpv_evidence_truncated"] = serde_json::json!(true);
        let projection = shadow_lot_feature_projection(&class_company, &truncated);
        assert_eq!(projection.status, "partial");
        assert_eq!(
            projection.withheld_framework_maximum_records_due_to_truncation,
            2
        );
        assert!(projection
            .lots
            .iter()
            .all(|lot| lot.framework_maximums.is_empty()));
    }

    #[test]
    fn shadow_lot_features_report_missing_evidence_and_company_activities() {
        let tender = Tender {
            source: "ted".to_owned(),
            ..prospect_tender()
        };
        assert_eq!(
            shadow_lot_feature_projection(&company(), &tender).status,
            "no_lot_evidence"
        );
        assert_eq!(
            shadow_lot_feature_projection(&company(), &prospect_tender()).status,
            "no_lot_evidence"
        );
        let legacy = Tender {
            raw: serde_json::json!({
                "notice_lot_cpv_evidence": [{"lot_id": "LOT-1", "cpv_codes": ["45221100"]}]
            }),
            ..prospect_tender()
        };
        let legacy_projection = shadow_lot_feature_projection(&company(), &legacy);
        assert_eq!(legacy_projection.status, "lot_id_uniqueness_unknown");
        assert!(legacy_projection.lots.is_empty());

        let ted_single = crate::ted::notice_to_tender(&serde_json::json!({
            "publication-number": "single",
            "identifier-lot": ["LOT-1"],
            "main-classification-lot": ["45233100"],
            "estimated-value-lot": ["123.45"],
            "estimated-value-cur-lot": ["EUR"],
            "framework-maximum-value-lot": ["250.00"],
            "framework-maximum-value-cur-lot": ["EUR"],
        }));
        let ted_projection = shadow_lot_feature_projection(&company(), &ted_single);
        assert_eq!(ted_projection.status, "partial");
        assert!(ted_projection.lots.is_empty());
        let ted_evidence = ted_projection.ted_lot_evidence.unwrap();
        assert_eq!(ted_evidence.lot_id.as_deref(), Some("LOT-1"));
        assert_eq!(
            ted_evidence.main_classification_cpv.as_deref(),
            Some("45233100")
        );
        assert_eq!(
            ted_evidence.estimated_value.as_ref().unwrap().raw_amount,
            "123.45"
        );
        assert_eq!(
            ted_evidence.estimated_value.unwrap().currency.as_deref(),
            Some("EUR")
        );
        assert_eq!(
            ted_evidence.framework_maximum.as_ref().unwrap().raw_amount,
            "250.00"
        );

        let ted_ambiguous = crate::ted::notice_to_tender(&serde_json::json!({
            "publication-number": "single-id-ambiguous-fields",
            "identifier-lot": ["LOT-1"],
            "main-classification-lot": ["45233100", "45233200"],
            "estimated-value-lot": ["100.00", "200.00"],
            "estimated-value-cur-lot": ["EUR"],
        }));
        let ambiguous_projection = shadow_lot_feature_projection(&company(), &ted_ambiguous);
        let ambiguous_evidence = ambiguous_projection.ted_lot_evidence.unwrap();
        assert_eq!(ambiguous_evidence.lot_id.as_deref(), Some("LOT-1"));
        assert!(ambiguous_evidence.main_classification_cpv.is_none());
        assert_eq!(
            ambiguous_evidence.main_classification_coverage,
            "multiple_values_withheld"
        );
        assert!(ambiguous_evidence.estimated_value.is_none());
        assert_eq!(
            ambiguous_evidence.estimated_value_coverage,
            "multiple_values_withheld"
        );

        let ted_multi = crate::ted::notice_to_tender(&serde_json::json!({
            "publication-number": "multi",
            "identifier-lot": ["LOT-1", "LOT-2"],
            "main-classification-lot": ["45233100", "45233200"],
            "estimated-value-lot": ["100.00", "200.00"],
            "estimated-value-cur-lot": ["EUR"],
        }));
        let multi_projection = shadow_lot_feature_projection(&company(), &ted_multi);
        assert_eq!(multi_projection.status, "multiple_lots_withheld");
        let multi_evidence = multi_projection.ted_lot_evidence.unwrap();
        assert!(multi_evidence.lot_id.is_none());
        assert!(multi_evidence.main_classification_cpv.is_none());
        assert!(multi_evidence.estimated_value.is_none());

        let not_unique = Tender {
            raw: serde_json::json!({
                "notice_lot_ids_unique": false,
                "notice_lot_cpv_evidence": [{"lot_id": "LOT-1", "cpv_codes": ["45221100"]}]
            }),
            ..prospect_tender()
        };
        assert_eq!(
            shadow_lot_feature_projection(&company(), &not_unique).status,
            "lot_ids_not_unique"
        );
        let tender = Tender {
            raw: serde_json::json!({
                "notice_lot_ids_unique": true,
                "notice_lot_cpv_evidence": [{"lot_id": "LOT-1", "cpv_codes": ["45221100"]}]
            }),
            ..prospect_tender()
        };
        let no_activities = CompanyProfile {
            activities: Vec::new(),
            ..company()
        };
        let projection = shadow_lot_feature_projection(&no_activities, &tender);
        assert_eq!(projection.status, "partial");
        assert_eq!(projection.lots[0].place_coverage, "not_recorded");
        assert_eq!(projection.lots[0].award_criteria_coverage, "not_recorded");
        assert_eq!(
            projection.lots[0].nace_coverage,
            "company_activities_unavailable"
        );
        assert!(projection.lots[0].cpv_nace_overlaps.is_empty());
    }

    #[test]
    fn framework_ceilings_are_exposed_separately_without_affecting_rank_scores() {
        let base = prospect_tender();
        let with_ceiling = Tender {
            raw: serde_json::json!({"framework_maximum_amounts": ["2600000.00"]}),
            ..base.clone()
        };
        let feedback = HashMap::new();
        let preferences = CompanyPreferences::default();
        let without = rank(&company(), &[base], &[], &preferences, &feedback, 1);
        let with = rank(&company(), &[with_ceiling], &[], &preferences, &feedback, 1);
        assert_eq!(with[0].score, without[0].score);
        assert_eq!(with[0].estimated_value, None);
        assert_eq!(with[0].framework_maximum_amounts, ["2600000.00"]);
        assert!(with[0]
            .warnings
            .iter()
            .any(|warning| warning.contains("ceiling, not estimated spend")
                && warning.contains("Currency and lot scope may be available")));
        let serialized = serde_json::to_value(&with[0]).unwrap();
        assert_eq!(serialized["framework_maximum_amounts"][0], "2600000.00");
        assert!(serde_json::to_value(&without[0]).unwrap()["framework_maximum_amounts"].is_null());
    }

    #[test]
    fn multiple_framework_ceilings_are_not_aggregated_or_mapped_to_lots() {
        let tender = Tender {
            raw: serde_json::json!({
                "framework_maximum_amounts": ["350000.00", "300000.00", "not-a-number"]
            }),
            ..prospect_tender()
        };
        let ranked = rank(
            &company(),
            &[tender],
            &[],
            &CompanyPreferences::default(),
            &HashMap::new(),
            1,
        );
        assert_eq!(
            ranked[0].framework_maximum_amounts,
            ["350000.00", "300000.00"]
        );
        assert!(ranked[0].warnings.iter().any(|warning| {
            warning.contains("Currency and lot scope are shown in source details when retained")
                && warning.contains("do not sum amounts without checking the notice")
        }));
    }

    fn recommendation(deadline: Option<&str>, score: f64) -> Recommendation {
        Recommendation {
            source: "test".to_owned(),
            external_id: deadline.unwrap_or("undated").to_owned(),
            title: "Tender".to_owned(),
            buyer: None,
            deadline: deadline.map(str::to_owned),
            estimated_value: None,
            framework_maximum_amounts: vec![],
            regions: vec![],
            cpv_codes: vec![],
            lot_cpv_alignment: None,
            languages: vec![],
            source_url: None,
            requirements: vec![],
            requirement_signals: vec![],
            score,
            confidence: 0.5,
            shadow_activity_text_similarity: None,
            shadow_activity_text_similarity_coverage: None,
            shadow_geographic_evidence: None,
            dossier_members: 1,
            feasibility: FeasibilityEvidence {
                status: FeasibilityStatus::Unknown,
                mandatory_requirements: 0,
                qualification_risk: "none_obvious",
            },
            reasons: vec![],
            warnings: vec![],
            jev_relevance: None,
            jev_qualification_risk: None,
        }
    }

    /// Three notices, three answers to "does this say anything about whether the
    /// company qualifies?". The unknown case must be labelled and warned about,
    /// never silently presented as unproblematic.
    #[test]
    fn feasibility_status_preserves_lowercase_wire_values() {
        assert_eq!(
            serde_json::to_value(FeasibilityStatus::Stated).unwrap(),
            "stated"
        );
        assert_eq!(
            serde_json::to_value(FeasibilityStatus::Hinted).unwrap(),
            "hinted"
        );
        assert_eq!(
            serde_json::to_value(FeasibilityStatus::Unknown).unwrap(),
            "unknown"
        );
    }

    #[test]
    fn feasibility_evidence_separates_stated_hinted_and_unknown() {
        let mut stated = Tender {
            source: "test".to_owned(),
            external_id: "1-2026".to_owned(),
            title: "Electrical installation".to_owned(),
            deadline: Some("2026-12-01".to_owned()),
            ..Tender::default()
        };
        stated.requirements = vec![TenderRequirement {
            text: "Turnover of at least EUR 500,000".to_owned(),
            source_page: None,
            ..TenderRequirement::default()
        }];
        let mut hinted = Tender {
            source: "test".to_owned(),
            external_id: "2-2026".to_owned(),
            title: "Electrical installation".to_owned(),
            deadline: Some("2026-12-01".to_owned()),
            ..Tender::default()
        };
        hinted.description = "Minimum turnover requirement applies.".to_owned();
        let unknown = Tender {
            source: "test".to_owned(),
            external_id: "3-2026".to_owned(),
            title: "Electrical installation".to_owned(),
            deadline: Some("2026-12-01".to_owned()),
            ..Tender::default()
        };
        let history: Vec<AwardRecord> = Vec::new();

        assert_eq!(
            feasibility_evidence(&stated, &history).status,
            FeasibilityStatus::Stated
        );
        assert_eq!(
            feasibility_evidence(&hinted, &history).status,
            FeasibilityStatus::Hinted
        );
        let evidence = feasibility_evidence(&unknown, &history);
        assert_eq!(evidence.status, FeasibilityStatus::Unknown);
        assert_eq!(evidence.mandatory_requirements, 0);

        let ranked = rank(
            &company(),
            &[stated, hinted, unknown],
            &history,
            &CompanyPreferences::default(),
            &HashMap::new(),
            10,
        );
        let status = |id: &str| {
            ranked
                .iter()
                .find(|item| item.external_id == id)
                .expect("item should be ranked")
                .feasibility
                .status
        };
        assert_eq!(status("1-2026"), FeasibilityStatus::Stated);
        assert_eq!(status("2-2026"), FeasibilityStatus::Hinted);
        assert_eq!(status("3-2026"), FeasibilityStatus::Unknown);
        let unknown_item = ranked
            .iter()
            .find(|item| item.external_id == "3-2026")
            .expect("item should be ranked");
        assert!(
            unknown_item
                .warnings
                .iter()
                .any(|warning| warning == UNSTATED_FEASIBILITY_WARNING),
            "an unstated qualification requirement must be warned about"
        );
        for id in ["1-2026", "2-2026"] {
            let item = ranked
                .iter()
                .find(|item| item.external_id == id)
                .expect("item should be ranked");
            assert!(
                !item
                    .warnings
                    .iter()
                    .any(|warning| warning == UNSTATED_FEASIBILITY_WARNING),
                "{id} does carry evidence, so it must not warn about its absence"
            );
        }
    }

    #[test]
    fn source_url_keeps_only_web_links() {
        let urls = [
            "http-evil:boom".to_owned(),
            "javascript:alert(1)".to_owned(),
            "https://ted.europa.eu/en/notice/-/detail/1".to_owned(),
        ];
        assert_eq!(
            source_url(&urls).as_deref(),
            Some("https://ted.europa.eu/en/notice/-/detail/1")
        );
        assert_eq!(source_url(&["mailto:buyer@example.com".to_owned()]), None);
        assert_eq!(source_url(&[]), None);
    }

    #[test]
    fn source_url_still_prefers_a_localised_detail_view() {
        let urls = [
            "https://ted.europa.eu/notice/1".to_owned(),
            "https://ted.europa.eu/nl/detail/2".to_owned(),
        ];
        assert_eq!(
            source_url(&urls).as_deref(),
            Some("https://ted.europa.eu/nl/detail/2")
        );
    }

    #[test]
    fn filters_expired_and_due_window_tenders() {
        // Arbitrary anchor: production passes `Utc::now().date_naive()` as
        // `today` (see recommend.rs), so this date never needs updating.
        let today = NaiveDate::from_ymd_opt(2026, 9, 20).expect("test date should be valid");
        let expired = Tender {
            deadline: Some("2026-09-19".to_owned()),
            ..Tender::default()
        };
        let soon = Tender {
            deadline: Some("2026-09-25".to_owned()),
            ..Tender::default()
        };
        let later = Tender {
            deadline: Some("2026-10-01".to_owned()),
            ..Tender::default()
        };
        let undated = Tender::default();
        assert!(!is_eligible(&expired, today, None));
        assert!(is_eligible(&soon, today, Some(7)));
        assert!(!is_eligible(&later, today, Some(7)));
        assert!(!is_eligible(&undated, today, Some(7)));
        assert!(is_eligible(&undated, today, None));
    }

    /// Equal scores are ordered by how much the notice says about qualification,
    /// not by an invented penalty: more evidence first, then the deadline.
    #[test]
    fn equal_scores_place_stated_evidence_first() {
        let mut stated = recommendation(Some("2026-10-20"), 70.0);
        stated.external_id = "stated".to_owned();
        stated.feasibility = FeasibilityEvidence {
            status: FeasibilityStatus::Stated,
            mandatory_requirements: 2,
            qualification_risk: "none_obvious",
        };
        let mut unknown = recommendation(Some("2026-10-10"), 70.0);
        unknown.external_id = "unknown".to_owned();
        let mut hinted = recommendation(Some("2026-10-05"), 70.0);
        hinted.external_id = "hinted".to_owned();
        hinted.feasibility = FeasibilityEvidence {
            status: FeasibilityStatus::Hinted,
            mandatory_requirements: 0,
            qualification_risk: "capacity",
        };
        let mut recommendations = vec![unknown, stated, hinted];
        sort_by_score(&mut recommendations);
        let order: Vec<&str> = recommendations
            .iter()
            .map(|item| item.external_id.as_str())
            .collect();
        assert_eq!(order, ["stated", "hinted", "unknown"]);
    }

    #[test]
    fn score_sort_places_undated_ties_last() {
        let mut recommendations = vec![
            recommendation(None, 50.0),
            recommendation(Some("2026-10-01"), 50.0),
        ];
        sort_by_score(&mut recommendations);
        assert_eq!(recommendations[0].deadline.as_deref(), Some("2026-10-01"));
        assert_eq!(recommendations[1].deadline, None);
    }

    #[test]
    fn deadline_sort_uses_score_for_equal_dates() {
        let mut recommendations = vec![
            recommendation(Some("2026-10-01"), 40.0),
            recommendation(Some("2026-10-01"), 80.0),
            recommendation(None, 100.0),
        ];
        sort_by_deadline(&mut recommendations);
        assert_eq!(recommendations[0].score, 80.0);
        assert_eq!(recommendations[1].score, 40.0);
        assert_eq!(recommendations[2].deadline, None);
    }

    #[test]
    fn rank_returns_score_order_with_undated_ties_last() {
        let tenders = vec![
            Tender {
                source: "test".to_owned(),
                external_id: "undated".to_owned(),
                title: "Electrical installation".to_owned(),
                ..Tender::default()
            },
            Tender {
                source: "test".to_owned(),
                external_id: "dated".to_owned(),
                title: "Electrical installation".to_owned(),
                // Relative to today: the runway signal is graded, so a fixed
                // date would silently become a past deadline and reverse this
                // ordering. 45 days is the comfortable band, an imminent
                // deadline deliberately scores below an undated notice.
                deadline: Some(
                    (chrono::Local::now().date_naive() + chrono::Duration::days(45)).to_string(),
                ),
                ..Tender::default()
            },
        ];
        let recommendations = rank(
            &company(),
            &tenders,
            &[],
            &CompanyPreferences::default(),
            &HashMap::new(),
            10,
        );
        assert_eq!(recommendations[0].external_id, "dated");
        assert_eq!(recommendations[1].external_id, "undated");
    }

    fn analysis_decision(relevance: &str, qualification_risk: &str) -> AnalysisDecision {
        AnalysisDecision {
            provider: "deterministic-rust".to_owned(),
            activity_match: Some(0.0),
            geography_match: Some(0.0),
            previous_work_match: Some(0.0),
            qualification_risk: Some(qualification_risk.to_owned()),
            relevance: Some(relevance.to_owned()),
            raw: std::collections::BTreeMap::new(),
            prompt_version: None,
            cached: false,
        }
    }

    #[test]
    fn jev_decision_fusion_applies_bonus_penalty_and_reasons() {
        // strong adds the capped bonus ...
        let mut item = recommendation(Some("2026-10-01"), 95.0);
        apply_jev_decision(&mut item, &analysis_decision("strong", "none"));
        assert_eq!(item.score, 100.0);
        assert_eq!(
            item.jev_relevance.as_ref().and_then(Option::as_deref),
            Some("strong")
        );
        assert_eq!(
            item.jev_qualification_risk
                .as_ref()
                .and_then(Option::as_deref),
            Some("none")
        );
        assert!(item
            .reasons
            .iter()
            .any(|reason| reason == "Jev relevance: strong"));
        // ... poor subtracts the floored penalty ...
        let mut item = recommendation(Some("2026-10-02"), 5.0);
        apply_jev_decision(&mut item, &analysis_decision("poor", "capacity"));
        assert_eq!(item.score, 0.0);
        assert_eq!(
            item.jev_qualification_risk
                .as_ref()
                .and_then(Option::as_deref),
            Some("capacity")
        );
        assert!(item
            .reasons
            .iter()
            .any(|reason| reason == "Jev relevance: poor"));
        // ... and anything else only records the reason.
        let mut item = recommendation(Some("2026-10-03"), 50.0);
        apply_jev_decision(&mut item, &analysis_decision("possible", "none"));
        assert_eq!(item.score, 50.0);
        assert_eq!(
            item.jev_relevance.as_ref().and_then(Option::as_deref),
            Some("possible")
        );
    }

    #[test]
    fn null_jev_answers_remain_null_on_deep_recommendations() {
        let mut item = recommendation(None, 50.0);
        let decision = AnalysisDecision {
            provider: "typesafe:jev-latest".to_owned(),
            activity_match: None,
            geography_match: None,
            previous_work_match: None,
            qualification_risk: None,
            relevance: None,
            raw: std::collections::BTreeMap::new(),
            prompt_version: None,
            cached: false,
        };
        apply_jev_decision(&mut item, &decision);
        assert_eq!(item.score, 50.0);
        assert_eq!(item.jev_relevance, Some(None));
        assert_eq!(item.jev_qualification_risk, Some(None));
        assert!(item
            .reasons
            .iter()
            .any(|reason| reason == "Jev relevance: None"));
        let serialized = serde_json::to_value(&item).expect("serialization should succeed");
        assert!(serialized["jev_relevance"].is_null());
        assert!(serialized["jev_qualification_risk"].is_null());
    }

    #[test]
    fn plain_recommendations_omit_jev_fields() {
        let item = recommendation(None, 50.0);
        assert_eq!(item.jev_relevance, None);
        assert_eq!(item.jev_qualification_risk, None);
        let serialized = serde_json::to_string(&item).expect("serialization should succeed");
        assert!(!serialized.contains("jev_relevance"));
        assert!(!serialized.contains("jev_qualification_risk"));
    }

    #[test]
    fn capacity_triage_demotes_contract_exceeding_max_bid_capacity() {
        let rec = recommendation(Some("t1"), 80.0);
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "t1".to_owned(),
            title: "Large IT Contract".to_owned(),
            buyer: None,
            description: "Test".to_owned(),
            cpv_codes: vec!["72000000".to_owned()],
            regions: vec![],
            estimated_value: Some(Decimal::new(500_000, 0)),
            lot_values: vec![],
            deadline: None,
            publication_date: None,
            languages: vec!["nl".to_owned()],
            notice_type: None,
            requirements: vec![],
            document_urls: vec![],
            raw: serde_json::Value::Null,
        };
        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "t1".to_owned()), &tender);

        let mut items = vec![rec];
        let limits = CapacityLimits::new(None, Some(350_000.0));
        let barriers = apply_capacity_triage(&mut items, &tenders_by_key, limits, None, false);

        assert_eq!(items[0].score, 50.0); // 80 - 30
        assert_eq!(
            barriers
                .get(&("test".to_owned(), "t1".to_owned()))
                .map(String::as_str),
            Some("capacity_exceeded")
        );
        assert!(items[0]
            .warnings
            .iter()
            .any(|w| w.contains("Capaciteitsbelemmering: geraamde waarde")));
    }

    #[test]
    fn capacity_triage_reads_a_french_turnover_requirement() {
        // The requirement signal lives in the matcher tables, which carry the
        // Dutch, French and English terms. A hand-rolled needle in the triage
        // path would silently miss two of the three, so this test drives the
        // French wording end-to-end and lets `rank` produce the warning.
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "fr-1".to_owned(),
            title: "Marche de services informatiques".to_owned(),
            description: "Le chiffre d'affaires annuel minimum est de 2.000.000 EUR.".to_owned(),
            estimated_value: Some(Decimal::new(2_500_000, 0)),
            deadline: Some("2026-12-01".to_owned()),
            ..Tender::default()
        };
        let mut items = rank(
            &company(),
            std::slice::from_ref(&tender),
            &[],
            &CompanyPreferences::default(),
            &HashMap::new(),
            10,
        );
        assert!(
            items[0]
                .warnings
                .iter()
                .any(|w| w.contains("Minimum turnover requirement")),
            "the shared table should flag the French term, got {:?}",
            items[0].warnings
        );

        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "fr-1".to_owned()), &tender);
        let barriers = apply_capacity_triage(
            &mut items,
            &tenders_by_key,
            CapacityLimits::new(Some(1_000_000.0), None),
            None,
            false,
        );

        assert_eq!(
            barriers
                .get(&("test".to_owned(), "fr-1".to_owned()))
                .map(String::as_str),
            Some("turnover_too_high")
        );
    }

    fn turnover_requirement_tender(external_id: &str) -> Tender {
        Tender {
            source: "test".to_owned(),
            external_id: external_id.to_owned(),
            title: "Framework with a turnover requirement".to_owned(),
            estimated_value: Some(Decimal::new(2_500_000, 0)),
            deadline: Some("2026-12-01".to_owned()),
            ..Tender::default()
        }
    }

    #[test]
    fn withheld_turnover_says_it_was_too_old() {
        // A provenance with no figure means a filing was withheld by the
        // opt-in age limit, which is not the same as having no filing.
        let mut rec = recommendation(Some("t9"), 70.0);
        rec.warnings
            .push("Minimum turnover requirement: 2500000 EUR".to_owned());
        let tender = turnover_requirement_tender("t9");
        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "t9".to_owned()), &tender);
        let provenance = crate::store::NbbTurnoverProvenance {
            fiscal_year: 2015,
            period_end: "2015-12-31".to_owned(),
            age_days: Some(3961),
            as_of: "2026-11-04".to_owned(),
            source_ref: "filing".to_owned(),
        };

        let mut items = vec![rec];
        let barriers = apply_capacity_triage(
            &mut items,
            &tenders_by_key,
            CapacityLimits::default(),
            Some(&provenance),
            true,
        );

        assert!(barriers.is_empty());
        assert_eq!(items[0].score, 70.0);
        assert!(items[0]
            .warnings
            .iter()
            .any(|w| w.contains("te oud voor de ingestelde drempel") && w.contains("FY2015")));
    }

    #[test]
    fn unknown_turnover_says_the_requirement_was_not_checked() {
        // Silence would read as "it fits". The tender states a turnover
        // requirement, so the honest report is that it could not be checked.
        let mut rec = recommendation(Some("t9"), 70.0);
        rec.warnings
            .push("Minimum turnover requirement: 2500000 EUR".to_owned());
        let tender = turnover_requirement_tender("t9");
        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "t9".to_owned()), &tender);

        let mut items = vec![rec];
        let barriers = apply_capacity_triage(
            &mut items,
            &tenders_by_key,
            CapacityLimits::default(),
            None,
            true,
        );

        assert!(barriers.is_empty(), "unknown is not a barrier");
        assert_eq!(items[0].score, 70.0, "unknown must not be penalised");
        assert!(items[0]
            .warnings
            .iter()
            .any(|w| w.contains("Capaciteit onbekend")));
    }

    #[test]
    fn unknown_turnover_is_silent_when_triage_was_not_requested() {
        let mut rec = recommendation(Some("t9"), 70.0);
        rec.warnings
            .push("Minimum turnover requirement: 2500000 EUR".to_owned());
        let tender = turnover_requirement_tender("t9");
        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "t9".to_owned()), &tender);

        let mut items = vec![rec];
        apply_capacity_triage(
            &mut items,
            &tenders_by_key,
            CapacityLimits::default(),
            None,
            false,
        );

        assert!(!items[0]
            .warnings
            .iter()
            .any(|w| w.contains("Capaciteit onbekend")));
    }

    #[test]
    fn a_known_turnover_that_fits_gets_no_unknown_note() {
        let mut rec = recommendation(Some("t9"), 70.0);
        rec.warnings
            .push("Minimum turnover requirement: 2500000 EUR".to_owned());
        let tender = turnover_requirement_tender("t9");
        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "t9".to_owned()), &tender);

        let mut items = vec![rec];
        let barriers = apply_capacity_triage(
            &mut items,
            &tenders_by_key,
            CapacityLimits::new(Some(5_000_000.0), None),
            None,
            true,
        );

        assert!(barriers.is_empty());
        assert!(!items[0]
            .warnings
            .iter()
            .any(|w| w.contains("Capaciteit onbekend")));
    }

    #[test]
    fn capacity_warning_cites_the_filing_it_rests_on() {
        // A 30-point demotion that names an amount but not its filing cannot be
        // audited by the person reading the recommendation.
        let mut rec = recommendation(Some("t2"), 85.0);
        rec.warnings
            .push("Minimum turnover requirement: 2500000 EUR".to_owned());
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "t2".to_owned(),
            title: "Mega Framework".to_owned(),
            estimated_value: Some(Decimal::new(2_500_000, 0)),
            deadline: Some("2026-12-01".to_owned()),
            ..Tender::default()
        };
        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "t2".to_owned()), &tender);
        let provenance = crate::store::NbbTurnoverProvenance {
            fiscal_year: 2021,
            period_end: "2021-12-31".to_owned(),
            age_days: Some(1400),
            as_of: "2026-11-07".to_owned(),
            source_ref: "2022-000123".to_owned(),
        };

        let mut items = vec![rec];
        apply_capacity_triage(
            &mut items,
            &tenders_by_key,
            CapacityLimits::new(Some(1_000_000.0), None),
            Some(&provenance),
            false,
        );

        let warning = items[0]
            .warnings
            .iter()
            .find(|w| w.contains("omzetvereiste vermeldt"))
            .expect("turnover barrier should warn");
        assert!(
            warning.contains("boekjaar FY2021, 1400 dagen oud"),
            "{warning}"
        );
        assert!(
            !warning.contains("source_ref"),
            "the reference stays out of the warning"
        );
    }

    #[test]
    fn capacity_triage_demotes_contract_exceeding_annual_turnover() {
        let mut rec = recommendation(Some("t2"), 85.0);
        rec.warnings
            .push("Minimum turnover requirement: 2500000 EUR".to_owned());
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "t2".to_owned(),
            title: "Mega Framework".to_owned(),
            buyer: None,
            description: "Test".to_owned(),
            cpv_codes: vec!["72000000".to_owned()],
            regions: vec![],
            estimated_value: Some(Decimal::new(2_500_000, 0)),
            lot_values: vec![],
            deadline: None,
            publication_date: None,
            languages: vec!["nl".to_owned()],
            notice_type: None,
            requirements: vec![],
            document_urls: vec![],
            raw: serde_json::Value::Null,
        };
        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "t2".to_owned()), &tender);

        let mut items = vec![rec];
        let limits = CapacityLimits::new(Some(850_000.0), Some(10_000_000.0));
        let barriers = apply_capacity_triage(&mut items, &tenders_by_key, limits, None, false);

        assert_eq!(items[0].score, 55.0); // 85 - 30
        assert_eq!(
            barriers
                .get(&("test".to_owned(), "t2".to_owned()))
                .map(String::as_str),
            Some("turnover_too_high")
        );
        assert!(items[0]
            .warnings
            .iter()
            .any(|w| w.contains("opdrachtwaarde (€ 2500000) overschrijdt jaaromzet")));
        assert!(items[0]
            .warnings
            .iter()
            .any(|w| w.contains("terwijl het bericht een omzetvereiste vermeldt")));
    }

    #[test]
    fn capacity_triage_is_noop_when_within_limits_or_empty() {
        let rec = recommendation(Some("t3"), 80.0);
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "t3".to_owned(),
            title: "Small Tender".to_owned(),
            buyer: None,
            description: "Test".to_owned(),
            cpv_codes: vec!["72000000".to_owned()],
            regions: vec![],
            estimated_value: Some(Decimal::new(100_000, 0)),
            lot_values: vec![],
            deadline: None,
            publication_date: None,
            languages: vec!["nl".to_owned()],
            notice_type: None,
            requirements: vec![],
            document_urls: vec![],
            raw: serde_json::Value::Null,
        };
        let mut tenders_by_key = HashMap::new();
        tenders_by_key.insert(("test".to_owned(), "t3".to_owned()), &tender);

        let mut items = vec![rec];
        let barriers = apply_capacity_triage(
            &mut items,
            &tenders_by_key,
            CapacityLimits::default(),
            None,
            false,
        );
        assert!(barriers.is_empty());
        assert_eq!(items[0].score, 80.0);
        assert!(items[0].warnings.is_empty());
    }
}
