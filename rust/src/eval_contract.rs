use crate::recommendations::{
    apply_capacity_triage, is_eligible, rank_on, sort_by_score, CapacityLimits, Recommendation,
};
use crate::{Activity, Address, AwardRecord, CompanyPreferences, CompanyProfile, Tender};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Synthetic SME profile specified in FW-05.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationProfile {
    pub profile_id: String,
    pub name: String,
    pub sector_description: String,
    pub nace_codes: Vec<String>,
    pub primary_cpv_prefixes: Vec<String>,
    pub regions: Vec<String>,
    pub languages: Vec<String>,
    pub capacity: CapacityConfig,
    pub explicitly_unknown_qualifications: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapacityConfig {
    pub fte_count: usize,
    pub annual_turnover_eur: f64,
    pub max_bid_capacity_eur: f64,
}

impl EvaluationProfile {
    /// Convert synthetic profile into standard CompanyProfile for matcher compatibility.
    pub fn to_company_profile(&self) -> CompanyProfile {
        let activities = self
            .nace_codes
            .iter()
            .map(|code| Activity {
                code: code.clone(),
                classification: "NACE2008".to_owned(),
                french: None,
                dutch: Some(format!("NACE {code}")),
            })
            .collect();

        let addresses = self
            .regions
            .iter()
            .map(|region| Address {
                address_type: "REGO".to_owned(),
                zipcode: match region.as_str() {
                    "BE10" => Some("1000".to_owned()),
                    "BE2" => Some("9000".to_owned()),
                    "BE3" => Some("5000".to_owned()),
                    _ => Some("1000".to_owned()),
                },
                municipality_nl: match region.as_str() {
                    "BE10" => Some("Brussel".to_owned()),
                    "BE2" => Some("Gent".to_owned()),
                    "BE3" => Some("Namen".to_owned()),
                    _ => None,
                },
                municipality_fr: None,
                street_nl: None,
                street_fr: None,
                house_number: None,
                box_value: None,
            })
            .collect();

        CompanyProfile {
            enterprise_number: self.profile_id.clone(),
            status: Some("Active".to_owned()),
            juridical_situation: None,
            juridical_form: Some("BV".to_owned()),
            start_date: Some("2020-01-01".to_owned()),
            names: vec![self.name.clone()],
            activities,
            addresses,
            establishments: vec![],
            sector_description: Some(self.sector_description.clone()),
            primary_cpv_prefixes: self.primary_cpv_prefixes.clone(),
        }
    }

    /// Return shared non-capacity preferences for both benchmark variants.
    /// Capacity limits are applied separately by the capacity-aware runner.
    pub fn to_preferences(&self) -> CompanyPreferences {
        CompanyPreferences {
            confirmed_activities: self.nace_codes.clone(),
            excluded_terms: vec![],
            preferred_regions: self.regions.clone(),
            languages: self.languages.clone(),
            minimum_value: None,
            maximum_value: None,
            recognition_class: None,
            recognition_categories: vec![],
        }
    }
}

/// Frozen tender corpus manifest for reproducible evaluation runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationCorpusManifest {
    pub manifest_version: String,
    pub evaluation_reference_date: String,
    pub dataset_name: String,
    pub procedures: Vec<ManifestProcedure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestProcedure {
    pub procedure_key: String,
    pub source: String,
    pub external_id: String,
    #[serde(default)]
    pub dossier_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub buyer: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub cpv_codes: Vec<String>,
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default)]
    pub estimated_value: Option<rust_decimal::Decimal>,
    pub publication_date: String,
    #[serde(default)]
    pub deadline: Option<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub notice_type: Option<String>,
    #[serde(default)]
    pub requirements: Vec<String>,
    pub source_sha256: String,
    pub stratum: String,
    #[serde(default)]
    pub target_profile: Option<String>,
}

impl ManifestProcedure {
    pub fn to_tender(&self) -> Tender {
        Tender {
            source: self.source.clone(),
            external_id: self.external_id.clone(),
            title: self.title.clone(),
            buyer: self.buyer.clone(),
            description: self.description.clone().unwrap_or_default(),
            cpv_codes: self.cpv_codes.clone(),
            regions: self.regions.clone(),
            estimated_value: self.estimated_value,
            lot_values: vec![],
            deadline: self.deadline.clone(),
            publication_date: Some(self.publication_date.clone()),
            languages: if self.languages.is_empty() {
                vec!["nl".to_owned()]
            } else {
                self.languages.clone()
            },
            notice_type: self
                .notice_type
                .clone()
                .or_else(|| Some("cn-standard".to_owned())),
            requirements: self
                .requirements
                .iter()
                .map(|r| crate::TenderRequirement {
                    text: r.clone(),
                    kind: "other".to_owned(),
                    mandatory: true,
                    source_page: None,
                })
                .collect(),
            document_urls: vec![],
            raw: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewRecordCollection {
    #[serde(default)]
    pub records: Vec<ReviewRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThematicRelevanceLabel {
    Ja,
    Nee,
    OnvoldoendeInformatie,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeasibilityLabel {
    Plausibel,
    ConcreteBelemmering,
    Onbekend,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaimVerdict {
    Ondersteund,
    Tegengesproken,
    NietVerifieerbaar,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClaimVerification {
    pub claim_text: String,
    pub system_variant: String,
    pub verdict: ClaimVerdict,
    pub source_passage_quote: Option<String>,
    pub source_location: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UncertaintyAssessment {
    pub notice_states_selection_criteria: bool,
    pub expected_warning_triggered: bool,
    pub unfounded_certainty_detected: bool,
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewRecord {
    pub review_id: String,
    pub protocol_version: String,
    pub evaluation_reference_date: String,
    pub profile_id: String,
    pub procedure_key: String,
    pub reviewer_id: String,
    pub review_timestamp: String,
    pub blinded_stage_completed: bool,
    pub thematic_relevance: ThematicRelevance,
    pub participation_feasibility: ParticipationFeasibility,
    pub uncertainty_assessment: UncertaintyAssessment,
    pub claim_verifications: Vec<ClaimVerification>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThematicRelevance {
    pub label: ThematicRelevanceLabel,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParticipationFeasibility {
    pub label: FeasibilityLabel,
    #[serde(default)]
    pub identified_barrier: Option<String>,
    pub notes: String,
}

/// Recorded score tie during benchmark ranking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkScoreTie {
    pub score: f64,
    pub procedure_keys: Vec<String>,
    pub tiebreak_rule: String,
}

/// Single item in benchmark ranking report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkItem {
    pub rank: usize,
    pub source: String,
    pub external_id: String,
    pub title: String,
    pub score: f64,
    pub confidence: f64,
    pub feasibility_status: String,
    #[serde(default)]
    pub feasibility_barrier: Option<String>,
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
    pub had_score_tie: bool,
}

/// Full reproducible benchmark execution report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkReport {
    pub profile_id: String,
    pub system_variant: String,
    pub evaluation_reference_date: String,
    pub k: usize,
    pub candidates_evaluated: usize,
    pub ranked_items: Vec<BenchmarkItem>,
    pub ties_detected: Vec<BenchmarkScoreTie>,
}

/// Protocol required by the current evaluation route; not inferred from review input.
pub const EVALUATION_PROTOCOL_VERSION: &str = "1.0.0";

/// Evidence completeness is separate from whether ranking completed successfully.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkMetrics {
    /// Requested cutoff, retained alongside the actual denominator.
    pub k: usize,
    pub evaluated_count: usize,
    pub judged_count: usize,
    pub unjudged_count: usize,
    pub insufficient_info_count: usize,
    /// No point estimate until all selected relevance labels are decisive.
    pub precision_at_k: Option<f64>,
    pub precision_lower_bound: Option<f64>,
    pub precision_upper_bound: Option<f64>,
    pub judged_coverage: Option<f64>,
    pub feasibility_counts: HashMap<String, usize>,
    pub supported_claim_count: usize,
    pub assessed_claim_count: usize,
    pub items_with_assessed_claims: usize,
    pub claim_fidelity_rate: Option<f64>,
    /// Self-declared metadata, not authentication or independent/blinded-review proof.
    pub declared_blinded_review_count: usize,
    pub unblinded_review_count: usize,
    /// Shared review-level assessment, not a variant-specific safety certificate.
    pub unfounded_certainty_detected: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationError(pub String);

impl std::fmt::Display for EvaluationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for EvaluationError {}

/// Only an admitted set can supply metrics or displayed review/claim annotations.
#[derive(Debug)]
pub struct BenchmarkEvaluation<'a> {
    pub metrics: BenchmarkMetrics,
    reviews_by_key: HashMap<String, &'a ReviewRecord>,
    system_variant: &'a str,
}

impl<'a> BenchmarkEvaluation<'a> {
    pub fn review_for(&self, item: &BenchmarkItem) -> Option<&'a ReviewRecord> {
        self.reviews_by_key
            .get(&format!("{}:{}", item.source, item.external_id))
            .copied()
    }

    pub fn claims_for<'b>(
        &'b self,
        item: &BenchmarkItem,
    ) -> impl Iterator<Item = &'a ClaimVerification> + 'b {
        self.review_for(item)
            .into_iter()
            .flat_map(|record| &record.claim_verifications)
            .filter(move |claim| claim.system_variant == self.system_variant)
    }
}

impl BenchmarkReport {
    /// Fail closed on ambiguous selected-profile reviews or incompatible context.
    /// There is deliberately no last-reviewer-wins or automatic adjudication policy.
    pub fn evaluate<'a>(
        &'a self,
        records: &'a [ReviewRecord],
        protocol_version: &str,
    ) -> Result<BenchmarkEvaluation<'a>, EvaluationError> {
        let fail = |message: String| EvaluationError(message);
        if protocol_version.trim().is_empty()
            || self.profile_id.trim().is_empty()
            || NaiveDate::parse_from_str(&self.evaluation_reference_date, "%Y-%m-%d").is_err()
            || !matches!(self.system_variant.as_str(), "baseline" | "improved")
        {
            return Err(fail("invalid evaluation context".to_owned()));
        }
        let mut reviews_by_key = HashMap::new();
        for record in records.iter().filter(|r| r.profile_id == self.profile_id) {
            if record.protocol_version != protocol_version
                || record.evaluation_reference_date != self.evaluation_reference_date
            {
                return Err(fail(format!(
                    "review context mismatch: {}",
                    record.procedure_key
                )));
            }
            if record.review_id.trim().is_empty()
                || record.reviewer_id.trim().is_empty()
                || record.procedure_key.trim().is_empty()
            {
                return Err(fail("review identity must not be empty".to_owned()));
            }
            // Even agreeing labels do not establish how to merge reviewers' claims.
            if reviews_by_key
                .insert(record.procedure_key.clone(), record)
                .is_some()
            {
                return Err(fail(format!("ambiguous reviews: {}", record.procedure_key)));
            }
            let mut claim_keys = std::collections::HashSet::new();
            for claim in &record.claim_verifications {
                if !claim_keys.insert((&claim.system_variant, claim.claim_text.trim())) {
                    return Err(fail(format!(
                        "ambiguous claim annotations: {}",
                        record.procedure_key
                    )));
                }
                let located = claim
                    .source_passage_quote
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
                    && claim
                        .source_location
                        .as_deref()
                        .is_some_and(|s| !s.trim().is_empty());
                if !matches!(claim.system_variant.as_str(), "baseline" | "improved")
                    || claim.claim_text.trim().is_empty()
                    || (claim.verdict != ClaimVerdict::NietVerifieerbaar && !located)
                {
                    return Err(fail(format!(
                        "invalid claim evidence: {}",
                        record.procedure_key
                    )));
                }
            }
        }
        let mut selected_keys = std::collections::HashSet::new();
        let mut relevant_hits = 0;
        let mut insufficient_info_count = 0;
        let mut judged_count = 0;
        let mut feasibility_counts = HashMap::new();
        let mut supported_claim_count = 0;
        let mut assessed_claim_count = 0;
        let mut items_with_assessed_claims = 0;
        let mut declared_blinded_review_count = 0;
        let mut unblinded_review_count = 0;
        let mut unfounded_certainty = false;
        let evaluated_count = self.ranked_items.len().min(self.k);
        for item in self.ranked_items.iter().take(self.k) {
            let key = format!("{}:{}", item.source, item.external_id);
            if !selected_keys.insert(key.clone()) {
                return Err(fail(format!("duplicate ranked procedure: {key}")));
            }
            if let Some(record) = reviews_by_key.get(&key) {
                judged_count += 1;
                match record.thematic_relevance.label {
                    ThematicRelevanceLabel::Ja => relevant_hits += 1,
                    ThematicRelevanceLabel::OnvoldoendeInformatie => insufficient_info_count += 1,
                    ThematicRelevanceLabel::Nee => {}
                }
                let label = match record.participation_feasibility.label {
                    FeasibilityLabel::Plausibel => "plausibel",
                    FeasibilityLabel::ConcreteBelemmering => "concrete_belemmering",
                    FeasibilityLabel::Onbekend => "onbekend",
                };
                *feasibility_counts.entry(label.to_owned()).or_default() += 1;
                if record.blinded_stage_completed {
                    declared_blinded_review_count += 1;
                } else {
                    unblinded_review_count += 1;
                }
                unfounded_certainty |= record.uncertainty_assessment.unfounded_certainty_detected;
                if record
                    .claim_verifications
                    .iter()
                    .any(|c| c.system_variant == self.system_variant)
                {
                    items_with_assessed_claims += 1;
                }
                for claim in record
                    .claim_verifications
                    .iter()
                    .filter(|c| c.system_variant == self.system_variant)
                {
                    assessed_claim_count += 1;
                    supported_claim_count +=
                        usize::from(claim.verdict == ClaimVerdict::Ondersteund);
                }
            }
        }
        let unjudged_count = evaluated_count - judged_count;
        let ratio = |numerator: usize| {
            (evaluated_count > 0).then(|| numerator as f64 / evaluated_count as f64)
        };
        let precision_lower_bound = ratio(relevant_hits);
        let metrics = BenchmarkMetrics {
            k: self.k,
            evaluated_count,
            judged_count,
            unjudged_count,
            insufficient_info_count,
            precision_at_k: if unjudged_count == 0 && insufficient_info_count == 0 {
                precision_lower_bound
            } else {
                None
            },
            precision_lower_bound,
            precision_upper_bound: ratio(relevant_hits + insufficient_info_count + unjudged_count),
            judged_coverage: ratio(judged_count),
            feasibility_counts,
            supported_claim_count,
            assessed_claim_count,
            items_with_assessed_claims,
            claim_fidelity_rate: (assessed_claim_count > 0)
                .then(|| supported_claim_count as f64 / assessed_claim_count as f64),
            declared_blinded_review_count,
            unblinded_review_count,
            unfounded_certainty_detected: if unfounded_certainty {
                Some(true)
            } else if evaluated_count > 0 && unjudged_count == 0 {
                Some(false)
            } else {
                None
            },
        };
        Ok(BenchmarkEvaluation {
            metrics,
            reviews_by_key,
            system_variant: &self.system_variant,
        })
    }
}

/// Execute a reproducible benchmark ranking for an evaluation profile.
///
/// Ensures:
/// 1. Reference date is used for deadline filtering, eliminating calendar drift.
/// 2. Empty feedback map is passed to prevent feedback score contamination (+10/-20).
/// 3. Score ties are identified and recorded with their resolution order.
/// 4. Profile preferences are used unless the caller supplies an override.
pub fn run_reproducible_benchmark(
    profile: &EvaluationProfile,
    tenders: &[Tender],
    history: &[AwardRecord],
    preferences: Option<&CompanyPreferences>,
    reference_date: NaiveDate,
    k: usize,
) -> BenchmarkReport {
    let company = profile.to_company_profile();
    let default_prefs = profile.to_preferences();
    let prefs = preferences.unwrap_or(&default_prefs);

    // Eligible candidates as of the fixed reference date.
    let eligible_tenders: Vec<Tender> = tenders
        .iter()
        .filter(|t| is_eligible(t, reference_date, None))
        .cloned()
        .collect();

    // Strict feedback isolation: no human labels modulate ranking scores.
    let isolated_feedback: HashMap<(String, String), bool> = HashMap::new();

    let full_recommendations: Vec<Recommendation> = rank_on(
        &company,
        &eligible_tenders,
        history,
        prefs,
        &isolated_feedback,
        eligible_tenders.len(),
        reference_date,
    );

    // Group score ties.
    let mut ties_detected = Vec::new();
    let mut i = 0;
    while i < full_recommendations.len() {
        let mut j = i + 1;
        while j < full_recommendations.len()
            && (full_recommendations[i].score - full_recommendations[j].score).abs() < 1e-6
        {
            j += 1;
        }
        if j - i > 1 {
            let tied_keys: Vec<String> = full_recommendations[i..j]
                .iter()
                .map(|r| format!("{}:{}", r.source, r.external_id))
                .collect();
            ties_detected.push(BenchmarkScoreTie {
                score: full_recommendations[i].score,
                procedure_keys: tied_keys.clone(),
                tiebreak_rule: "evidence_cmp (stated > hinted > unknown) then deadline_cmp"
                    .to_owned(),
            });
        }
        i = j;
    }

    let tied_keys_set: std::collections::HashSet<String> = ties_detected
        .iter()
        .flat_map(|t| t.procedure_keys.iter().cloned())
        .collect();

    let ranked_items: Vec<BenchmarkItem> = full_recommendations
        .into_iter()
        .take(k)
        .enumerate()
        .map(|(idx, rec)| {
            let key = format!("{}:{}", rec.source, rec.external_id);
            BenchmarkItem {
                rank: idx + 1,
                source: rec.source,
                external_id: rec.external_id,
                title: rec.title,
                score: rec.score,
                confidence: rec.confidence,
                feasibility_status: rec.feasibility.status.as_str().to_owned(),
                feasibility_barrier: None,
                reasons: rec.reasons,
                warnings: rec.warnings,
                had_score_tie: tied_keys_set.contains(&key),
            }
        })
        .collect();

    BenchmarkReport {
        profile_id: profile.profile_id.clone(),
        system_variant: "baseline".to_owned(),
        evaluation_reference_date: reference_date.format("%Y-%m-%d").to_string(),
        k,
        candidates_evaluated: eligible_tenders.len(),
        ranked_items,
        ties_detected,
    }
}

/// Execute a capacity-aware benchmark ranking (Improved Variant for FW-09).
///
/// Builds upon the baseline ranker by:
/// 1. Verifying tender assessed values against the SME's `max_bid_capacity_eur`.
/// 2. Verifying turnover requirement warnings against the SME's `annual_turnover_eur`.
/// 3. Flagging concrete feasibility barriers (`CapacityExceeded`, `TurnoverTooHigh`).
/// 4. Penalizing inaccessible contracts by 30 points to prevent disqualifying contracts from polluting top ranks.
/// 5. Re-sorting the ladder with canonical `sort_by_score`.
pub fn run_capacity_aware_benchmark(
    profile: &EvaluationProfile,
    tenders: &[Tender],
    history: &[AwardRecord],
    preferences: Option<&CompanyPreferences>,
    reference_date: NaiveDate,
    k: usize,
) -> BenchmarkReport {
    let company = profile.to_company_profile();
    let default_prefs = profile.to_preferences();
    let prefs = preferences.unwrap_or(&default_prefs);

    let eligible_tenders: Vec<Tender> = tenders
        .iter()
        .filter(|t| is_eligible(t, reference_date, None))
        .cloned()
        .collect();

    let tenders_by_key: HashMap<(String, String), &Tender> = eligible_tenders
        .iter()
        .map(|t| ((t.source.clone(), t.external_id.clone()), t))
        .collect();

    let isolated_feedback: HashMap<(String, String), bool> = HashMap::new();

    let mut full_recommendations: Vec<Recommendation> = rank_on(
        &company,
        &eligible_tenders,
        history,
        prefs,
        &isolated_feedback,
        eligible_tenders.len(),
        reference_date,
    );

    // Apply capacity-aware feasibility auditing and triage penalties
    let capacity = CapacityLimits {
        annual_turnover_eur: Some(profile.capacity.annual_turnover_eur),
        max_bid_capacity_eur: Some(profile.capacity.max_bid_capacity_eur),
    };
    // Synthetic profile: no filing behind the figure, so no provenance to cite.
    let barriers = apply_capacity_triage(
        &mut full_recommendations,
        &tenders_by_key,
        capacity,
        None,
        false,
    );

    // Re-sort with canonical tie-breakers
    sort_by_score(&mut full_recommendations);

    // Group score ties
    let mut ties_detected = Vec::new();
    let mut i = 0;
    while i < full_recommendations.len() {
        let mut j = i + 1;
        while j < full_recommendations.len()
            && (full_recommendations[i].score - full_recommendations[j].score).abs() < 1e-6
        {
            j += 1;
        }
        if j - i > 1 {
            let tied_keys: Vec<String> = full_recommendations[i..j]
                .iter()
                .map(|r| format!("{}:{}", r.source, r.external_id))
                .collect();
            ties_detected.push(BenchmarkScoreTie {
                score: full_recommendations[i].score,
                procedure_keys: tied_keys.clone(),
                tiebreak_rule: "evidence_cmp (stated > hinted > unknown) then deadline_cmp"
                    .to_owned(),
            });
        }
        i = j;
    }

    let tied_keys_set: std::collections::HashSet<String> = ties_detected
        .iter()
        .flat_map(|t| t.procedure_keys.iter().cloned())
        .collect();

    let ranked_items: Vec<BenchmarkItem> = full_recommendations
        .into_iter()
        .take(k)
        .enumerate()
        .map(|(idx, rec)| {
            let key = format!("{}:{}", rec.source, rec.external_id);
            let barrier = barriers
                .get(&(rec.source.clone(), rec.external_id.clone()))
                .cloned();
            let feasibility_status = if barrier.is_some() {
                "concrete_belemmering".to_owned()
            } else {
                rec.feasibility.status.as_str().to_owned()
            };

            BenchmarkItem {
                rank: idx + 1,
                source: rec.source,
                external_id: rec.external_id,
                title: rec.title,
                score: rec.score,
                confidence: rec.confidence,
                feasibility_status,
                feasibility_barrier: barrier,
                reasons: rec.reasons,
                warnings: rec.warnings,
                had_score_tie: tied_keys_set.contains(&key),
            }
        })
        .collect();

    BenchmarkReport {
        profile_id: profile.profile_id.clone(),
        system_variant: "improved".to_owned(),
        evaluation_reference_date: reference_date.format("%Y-%m-%d").to_string(),
        k,
        candidates_evaluated: eligible_tenders.len(),
        ranked_items,
        ties_detected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> EvaluationProfile {
        EvaluationProfile {
            profile_id: "SYN-SME-001".to_owned(),
            name: "Synthetisch IT BV".to_owned(),
            sector_description: "Software".to_owned(),
            nace_codes: vec!["62.010".to_owned(), "62.020".to_owned()],
            primary_cpv_prefixes: vec!["72000000".to_owned()],
            regions: vec!["BE10".to_owned()],
            languages: vec!["nl".to_owned()],
            capacity: CapacityConfig {
                fte_count: 8,
                annual_turnover_eur: 850_000.0,
                max_bid_capacity_eur: 350_000.0,
            },
            explicitly_unknown_qualifications: vec!["Geen ISO 27001".to_owned()],
        }
    }

    fn sample_tender(id: &str, deadline: Option<&str>, cpv: &str) -> Tender {
        Tender {
            source: "ted".to_owned(),
            external_id: id.to_owned(),
            title: format!("Tender {id}"),
            buyer: Some("Gemeente Test".to_owned()),
            description: String::new(),
            cpv_codes: vec![cpv.to_owned()],
            regions: vec!["BE10".to_owned()],
            estimated_value: None,
            lot_values: vec![],
            deadline: deadline.map(|s| s.to_owned()),
            publication_date: Some("2026-10-01".to_owned()),
            languages: vec!["nl".to_owned()],
            notice_type: Some("cn-standard".to_owned()),
            requirements: vec![],
            document_urls: vec!["https://ted.europa.eu/test".to_owned()],
            raw: serde_json::Value::Null,
        }
    }

    #[test]
    fn test_profile_to_company_conversion() {
        let profile = sample_profile();
        let company = profile.to_company_profile();

        assert_eq!(company.enterprise_number, "SYN-SME-001");
        assert_eq!(company.names, vec!["Synthetisch IT BV"]);
        assert_eq!(company.activities.len(), 2);
        assert_eq!(company.activities[0].code, "62.010");
        assert_eq!(company.addresses[0].zipcode.as_deref(), Some("1000"));
        assert_eq!(company.sector_description.as_deref(), Some("Software"));
        assert_eq!(company.primary_cpv_prefixes, vec!["72000000"]);
    }

    #[test]
    fn benchmark_variants_share_non_capacity_profile_preferences_by_default() {
        let profile = sample_profile();
        let ref_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        let preferred = sample_tender("preferred", Some("2026-11-15"), "72000000");
        let mut mismatch = sample_tender("mismatch", Some("2026-11-15"), "45000000");
        mismatch.regions = vec!["BE20".to_owned()];
        mismatch.languages = vec!["fr".to_owned()];
        let tenders = vec![mismatch, preferred];
        let profile_preferences = profile.to_preferences();

        assert_eq!(profile_preferences.confirmed_activities, profile.nace_codes);
        assert_eq!(profile_preferences.preferred_regions, profile.regions);
        assert_eq!(profile_preferences.languages, profile.languages);
        assert_eq!(profile_preferences.maximum_value, None);

        let implicit =
            run_reproducible_benchmark(&profile, &tenders, &[], None, ref_date, tenders.len());
        let explicit = run_reproducible_benchmark(
            &profile,
            &tenders,
            &[],
            Some(&profile_preferences),
            ref_date,
            tenders.len(),
        );
        let capacity_aware =
            run_capacity_aware_benchmark(&profile, &tenders, &[], None, ref_date, tenders.len());

        let ranking = |report: &BenchmarkReport| {
            report
                .ranked_items
                .iter()
                .map(|item| (item.external_id.clone(), item.score))
                .collect::<Vec<_>>()
        };
        assert_eq!(ranking(&implicit), ranking(&explicit));
        assert_eq!(ranking(&implicit), ranking(&capacity_aware));
    }

    #[test]
    fn test_run_reproducible_benchmark_calendar_invariance() {
        let profile = sample_profile();
        let ref_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();

        // One expired notice, two active notices.
        let tenders = vec![
            sample_tender("expired", Some("2026-10-20"), "72000000"),
            sample_tender("active-1", Some("2026-11-15"), "72000000"),
            sample_tender("active-2", Some("2026-12-01"), "72000000"),
        ];

        let report = run_reproducible_benchmark(&profile, &tenders, &[], None, ref_date, 10);

        assert_eq!(report.candidates_evaluated, 2);
        assert_eq!(report.ranked_items.len(), 2);
        assert!(report
            .ranked_items
            .iter()
            .all(|item| item.external_id != "expired"));
    }

    #[test]
    fn test_feedback_isolation() {
        let profile = sample_profile();
        let ref_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        let tenders = vec![
            sample_tender("active-1", Some("2026-11-15"), "72000000"),
            sample_tender("active-2", Some("2026-11-15"), "72000000"),
        ];

        let report_without_feedback =
            run_reproducible_benchmark(&profile, &tenders, &[], None, ref_date, 10);

        // Feedback in system should never affect the reproducible benchmark
        assert_eq!(report_without_feedback.ranked_items.len(), 2);
        assert!(report_without_feedback.ranked_items[0]
            .reasons
            .iter()
            .all(|r| !r.contains("Previously marked")));
    }

    #[test]
    fn test_ties_are_detected_and_reported() {
        let profile = sample_profile();
        let ref_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();

        // Both tenders have identical fit, same cpv, same deadline.
        let tenders = vec![
            sample_tender("tie-a", Some("2026-11-15"), "72000000"),
            sample_tender("tie-b", Some("2026-11-15"), "72000000"),
        ];

        let report = run_reproducible_benchmark(&profile, &tenders, &[], None, ref_date, 10);

        assert_eq!(report.ties_detected.len(), 1);
        assert_eq!(report.ties_detected[0].procedure_keys.len(), 2);
        assert!(report.ranked_items[0].had_score_tie);
        assert!(report.ranked_items[1].had_score_tie);
    }

    #[test]
    fn test_metrics_computation() {
        let profile = sample_profile();
        let ref_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        let tenders = vec![
            sample_tender("doc-1", Some("2026-11-15"), "72000000"),
            sample_tender("doc-2", Some("2026-11-15"), "72000000"),
        ];

        let report = run_reproducible_benchmark(&profile, &tenders, &[], None, ref_date, 2);

        let records = vec![
            ReviewRecord {
                review_id: "rev-1".to_owned(),
                protocol_version: "1.0.0".to_owned(),
                evaluation_reference_date: "2026-10-30".to_owned(),
                profile_id: "SYN-SME-001".to_owned(),
                procedure_key: "ted:doc-1".to_owned(),
                reviewer_id: "rev-a".to_owned(),
                review_timestamp: "2026-10-30T14:00:00Z".to_owned(),
                blinded_stage_completed: true,
                thematic_relevance: ThematicRelevance {
                    label: ThematicRelevanceLabel::Ja,
                    rationale: "Relevant".to_owned(),
                },
                participation_feasibility: ParticipationFeasibility {
                    label: FeasibilityLabel::Plausibel,
                    identified_barrier: None,
                    notes: "".to_owned(),
                },
                uncertainty_assessment: UncertaintyAssessment {
                    notice_states_selection_criteria: false,
                    expected_warning_triggered: true,
                    unfounded_certainty_detected: false,
                    notes: "".to_owned(),
                },
                claim_verifications: vec![ClaimVerification {
                    claim_text: "Text matches".to_owned(),
                    system_variant: "baseline".to_owned(),
                    verdict: ClaimVerdict::Ondersteund,
                    source_passage_quote: Some("Quote".to_owned()),
                    source_location: Some("p1".to_owned()),
                }],
            },
            ReviewRecord {
                review_id: "rev-2".to_owned(),
                protocol_version: "1.0.0".to_owned(),
                evaluation_reference_date: "2026-10-30".to_owned(),
                profile_id: "SYN-SME-001".to_owned(),
                procedure_key: "ted:doc-2".to_owned(),
                reviewer_id: "rev-a".to_owned(),
                review_timestamp: "2026-10-30T14:00:00Z".to_owned(),
                blinded_stage_completed: true,
                thematic_relevance: ThematicRelevance {
                    label: ThematicRelevanceLabel::OnvoldoendeInformatie,
                    rationale: "Unsure".to_owned(),
                },
                participation_feasibility: ParticipationFeasibility {
                    label: FeasibilityLabel::Onbekend,
                    identified_barrier: None,
                    notes: "".to_owned(),
                },
                uncertainty_assessment: UncertaintyAssessment {
                    notice_states_selection_criteria: false,
                    expected_warning_triggered: true,
                    unfounded_certainty_detected: false,
                    notes: "".to_owned(),
                },
                claim_verifications: vec![],
            },
        ];

        let metrics = report
            .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;

        assert_eq!(metrics.precision_at_k, None); // An insufficient label prevents a point estimate.
        assert_eq!(metrics.precision_lower_bound, Some(0.5));
        assert_eq!(metrics.precision_upper_bound, Some(1.0)); // (1 + 1) / 2
        assert_eq!(metrics.judged_coverage, Some(1.0)); // 2 / 2
        assert_eq!(metrics.claim_fidelity_rate, Some(1.0));
        assert_eq!(metrics.unfounded_certainty_detected, Some(false));
    }

    #[test]
    fn metrics_missing_reviews_widen_uncertainty() {
        let report = run_reproducible_benchmark(
            &sample_profile(),
            &[sample_tender("unreviewed", Some("2026-11-15"), "72000000")],
            &[],
            None,
            NaiveDate::from_ymd_opt(2026, 10, 30).unwrap(),
            5,
        );
        let metrics = report
            .evaluate(&[], EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;
        assert_eq!(metrics.precision_upper_bound, Some(1.0));
        assert_eq!(metrics.precision_at_k, None);
        assert_eq!(metrics.unfounded_certainty_detected, None);
    }

    #[test]
    fn metrics_empty_claims_are_not_perfect_fidelity() {
        let report = run_reproducible_benchmark(
            &sample_profile(),
            &[],
            &[],
            None,
            NaiveDate::from_ymd_opt(2026, 10, 30).unwrap(),
            5,
        );
        let metrics = report
            .evaluate(&[], EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;
        assert_eq!(metrics.claim_fidelity_rate, None);
    }

    fn evidence_report(count: usize, k: usize) -> BenchmarkReport {
        let report = run_reproducible_benchmark(
            &sample_profile(),
            &(0..count)
                .map(|n| sample_tender(&format!("evidence-{n}"), Some("2026-11-15"), "72000000"))
                .collect::<Vec<_>>(),
            &[],
            None,
            NaiveDate::from_ymd_opt(2026, 10, 30).unwrap(),
            k,
        );
        assert_eq!(report.ranked_items.len(), count.min(k));
        report
    }

    fn evidence_review(item: &BenchmarkItem) -> ReviewRecord {
        serde_json::from_value(serde_json::json!({
            "review_id": format!("review-{}", item.external_id),
            "protocol_version": EVALUATION_PROTOCOL_VERSION,
            "evaluation_reference_date": "2026-10-30", "profile_id": "SYN-SME-001",
            "procedure_key": format!("{}:{}", item.source, item.external_id),
            "reviewer_id": "synthetic-reviewer", "review_timestamp": "2026-10-30T14:00:00Z",
            "blinded_stage_completed": true,
            "thematic_relevance": {"label":"ja", "rationale":"synthetic"},
            "participation_feasibility": {"label":"plausibel", "identified_barrier":null, "notes":"synthetic"},
            "uncertainty_assessment": {"notice_states_selection_criteria":false, "expected_warning_triggered":true, "unfounded_certainty_detected":false, "notes":"synthetic"},
            "claim_verifications":[]
        })).unwrap()
    }

    #[test]
    fn evidence_rejects_wrong_context_and_ambiguous_reviewers_in_both_orders() {
        let report = evidence_report(1, 5);
        let record = evidence_review(&report.ranked_items[0]);
        for field in ["date", "protocol", "reviewer", "review_id"] {
            let mut wrong = record.clone();
            match field {
                "date" => wrong.evaluation_reference_date = "2026-10-29".to_owned(),
                "protocol" => wrong.protocol_version = "different".to_owned(),
                "reviewer" => wrong.reviewer_id.clear(),
                _ => wrong.review_id.clear(),
            }
            assert!(
                report
                    .evaluate(&[wrong], EVALUATION_PROTOCOL_VERSION)
                    .is_err(),
                "{field}"
            );
        }
        let mut conflicting = record.clone();
        conflicting.reviewer_id = "another-reviewer".to_owned();
        conflicting.thematic_relevance.label = ThematicRelevanceLabel::Nee;
        for records in [
            vec![record.clone(), conflicting.clone()],
            vec![conflicting, record.clone()],
            vec![record.clone(), record.clone()],
        ] {
            assert!(report
                .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
                .unwrap_err()
                .to_string()
                .contains("ambiguous reviews"));
        }
        let mut nonblind = record.clone();
        nonblind.blinded_stage_completed = false;
        let records = [nonblind];
        let metrics = report
            .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;
        assert_eq!(metrics.precision_at_k, Some(1.0));
        assert_eq!(metrics.judged_coverage, Some(1.0));
        assert_eq!(metrics.declared_blinded_review_count, 0);
        assert_eq!(metrics.unblinded_review_count, 1);
        let partial_report = evidence_report(2, 5);
        let mut incident = evidence_review(&partial_report.ranked_items[0]);
        incident.uncertainty_assessment.unfounded_certainty_detected = true;
        let records = [incident];
        let metrics = partial_report
            .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;
        assert_eq!(metrics.unjudged_count, 1);
        assert_eq!(metrics.unfounded_certainty_detected, Some(true));
        let mut foreign = record;
        foreign.profile_id = "another-profile".to_owned();
        let records = [foreign];
        let evaluation = report
            .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
            .unwrap();
        assert!(evaluation.review_for(&report.ranked_items[0]).is_none());
        assert_eq!(evaluation.metrics.judged_count, 0);
    }

    #[test]
    fn evidence_claim_scope_and_annotation_scope_share_admission() {
        let baseline = evidence_report(1, 5);
        let mut improved = baseline.clone();
        improved.system_variant = "improved".to_owned();
        let mut record = evidence_review(&baseline.ranked_items[0]);
        record.claim_verifications = vec![ClaimVerification {
            claim_text: "synthetic baseline assertion".to_owned(),
            system_variant: "baseline".to_owned(),
            verdict: ClaimVerdict::Ondersteund,
            source_passage_quote: Some("synthetic quote".to_owned()),
            source_location: Some("p1".to_owned()),
        }];
        let records = [record.clone()];
        let absent = improved
            .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
            .unwrap();
        assert_eq!(absent.metrics.claim_fidelity_rate, None);
        assert_eq!(absent.metrics.assessed_claim_count, 0);
        assert_eq!(absent.claims_for(&improved.ranked_items[0]).count(), 0);
        let mut claim = record.claim_verifications[0].clone();
        claim.system_variant = "improved".to_owned();
        claim.verdict = ClaimVerdict::Tegengesproken;
        record.claim_verifications.push(claim);
        let records = [record.clone()];
        let base = baseline
            .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
            .unwrap();
        let imp = improved
            .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
            .unwrap();
        assert_eq!(base.metrics.claim_fidelity_rate, Some(1.0));
        assert_eq!(imp.metrics.claim_fidelity_rate, Some(0.0));
        assert_eq!(base.metrics.assessed_claim_count, 1);
        assert_eq!(imp.metrics.assessed_claim_count, 1);
        assert_eq!(
            imp.claims_for(&improved.ranked_items[0])
                .next()
                .unwrap()
                .verdict,
            ClaimVerdict::Tegengesproken
        );
        let mut duplicate = record.clone();
        duplicate
            .claim_verifications
            .push(duplicate.claim_verifications[0].clone());
        assert!(baseline
            .evaluate(&[duplicate], EVALUATION_PROTOCOL_VERSION)
            .unwrap_err()
            .to_string()
            .contains("ambiguous claim annotations"));
        assert_eq!(base.metrics.items_with_assessed_claims, 1);
        assert_eq!(imp.metrics.items_with_assessed_claims, 1);
        for bad in ["variant", "quote", "location", "text"] {
            let mut invalid = record.clone();
            match bad {
                "variant" => invalid.claim_verifications[0].system_variant = "typo".to_owned(),
                "quote" => invalid.claim_verifications[0].source_passage_quote = None,
                "location" => invalid.claim_verifications[0].source_location = Some(" ".to_owned()),
                _ => invalid.claim_verifications[0].claim_text.clear(),
            }
            assert!(
                baseline
                    .evaluate(&[invalid], EVALUATION_PROTOCOL_VERSION)
                    .is_err(),
                "{bad}"
            );
        }
        record.claim_verifications[0].verdict = ClaimVerdict::NietVerifieerbaar;
        record.claim_verifications[0].source_passage_quote = None;
        record.claim_verifications[0].source_location = None;
        assert_eq!(
            baseline
                .evaluate(&[record], EVALUATION_PROTOCOL_VERSION)
                .unwrap()
                .metrics
                .claim_fidelity_rate,
            Some(0.0)
        );
    }

    #[test]
    fn evidence_bounds_cover_every_completion_in_small_domains() {
        // Exhaustive domain: 0–3 returned items, requested k 0–5, every combination
        // of missing/yes/no/insufficient labels, and every compatible true completion.
        for count in 0..=3 {
            for k in 0..=5 {
                let report = evidence_report(count, k);
                let n = report.ranked_items.len().min(k);
                for state in 0..4_usize.pow(n as u32) {
                    let mut digits = state;
                    let mut records = Vec::new();
                    let mut possible = vec![0_usize];
                    for item in report.ranked_items.iter().take(k) {
                        let label = digits % 4;
                        digits /= 4;
                        if label == 0 || label == 3 {
                            possible = possible.iter().flat_map(|hits| [*hits, hits + 1]).collect();
                        } else if label == 1 {
                            possible.iter_mut().for_each(|hits| *hits += 1);
                        }
                        if label != 0 {
                            let mut record = evidence_review(item);
                            record.thematic_relevance.label = match label {
                                1 => ThematicRelevanceLabel::Ja,
                                2 => ThematicRelevanceLabel::Nee,
                                _ => ThematicRelevanceLabel::OnvoldoendeInformatie,
                            };
                            records.push(record);
                        }
                    }
                    let metrics = report
                        .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
                        .unwrap()
                        .metrics;
                    assert_eq!(metrics.evaluated_count, n);
                    assert_eq!(metrics.judged_count + metrics.unjudged_count, n);
                    assert_eq!(
                        metrics.feasibility_counts.values().sum::<usize>(),
                        metrics.judged_count
                    );
                    assert_eq!(metrics.claim_fidelity_rate, None);
                    if n == 0 {
                        assert_eq!(metrics.precision_at_k, None);
                        assert_eq!(metrics.precision_lower_bound, None);
                        assert_eq!(metrics.precision_upper_bound, None);
                        assert_eq!(metrics.judged_coverage, None);
                    } else {
                        let min = *possible.iter().min().unwrap() as f64 / n as f64;
                        let max = *possible.iter().max().unwrap() as f64 / n as f64;
                        assert_eq!(metrics.precision_lower_bound, Some(min));
                        assert_eq!(metrics.precision_upper_bound, Some(max));
                        assert_eq!(metrics.precision_at_k, (min == max).then_some(min));
                    }
                    records.reverse();
                    assert_eq!(
                        metrics,
                        report
                            .evaluate(&records, EVALUATION_PROTOCOL_VERSION)
                            .unwrap()
                            .metrics
                    );
                    let value = serde_json::to_value(&metrics).unwrap();
                    if n == 0 {
                        assert!(value["precision_at_k"].is_null());
                    }
                    assert!(value["claim_fidelity_rate"].is_null());
                }
            }
        }
    }

    #[test]
    fn evidence_rejects_duplicate_ranked_procedures() {
        let mut report = evidence_report(1, 5);
        report.ranked_items.push(report.ranked_items[0].clone());
        assert!(report
            .evaluate(&[], EVALUATION_PROTOCOL_VERSION)
            .unwrap_err()
            .to_string()
            .contains("duplicate ranked procedure"));
    }

    #[test]
    fn test_fixture_deserialization_roundtrip() {
        let s1 = std::fs::read_to_string("../tests/fixtures/eval/profiles/SYN-SME-001.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/profiles/SYN-SME-001.json"))
            .expect("should read SYN-SME-001.json");
        let p1: EvaluationProfile = serde_json::from_str(&s1).expect("valid profile json");
        assert_eq!(p1.profile_id, "SYN-SME-001");

        let s2 = std::fs::read_to_string("../tests/fixtures/eval/manifest.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/manifest.json"))
            .expect("should read manifest.json");
        let manifest: EvaluationCorpusManifest =
            serde_json::from_str(&s2).expect("valid manifest json");
        assert_eq!(manifest.procedures.len(), 10);
    }

    #[test]
    fn test_pilot_dataset_execution() {
        use rust_decimal::Decimal;

        let s1 = std::fs::read_to_string("../tests/fixtures/eval/profiles/SYN-SME-001.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/profiles/SYN-SME-001.json"))
            .expect("should read SYN-SME-001.json");
        let it_profile: EvaluationProfile = serde_json::from_str(&s1).unwrap();

        let s2 = std::fs::read_to_string("../tests/fixtures/eval/profiles/SYN-SME-002.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/profiles/SYN-SME-002.json"))
            .expect("should read SYN-SME-002.json");
        let facility_profile: EvaluationProfile = serde_json::from_str(&s2).unwrap();

        let ref_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();

        let tenders = vec![
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-001-000101".to_owned(),
                title: "Ontwikkeling en onderhoud van een portaal voor open data".to_owned(),
                buyer: Some("Stad Antwerpen".to_owned()),
                description: "Webportaal".to_owned(),
                cpv_codes: vec!["72200000".to_owned()],
                regions: vec!["BE2".to_owned()],
                estimated_value: Some(Decimal::new(180_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-20".to_owned()),
                publication_date: Some("2026-10-01".to_owned()),
                languages: vec!["nl".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/101".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-002-000102".to_owned(),
                title: "Migratie en modernisering cloudinfrastructuur".to_owned(),
                buyer: Some("Vlaamse Overheid".to_owned()),
                description: "Kubernetes en cloud".to_owned(),
                cpv_codes: vec!["72000000".to_owned()],
                regions: vec!["BE2".to_owned()],
                estimated_value: Some(Decimal::new(220_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-25".to_owned()),
                publication_date: Some("2026-10-05".to_owned()),
                languages: vec!["nl".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/102".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-003-000103".to_owned(),
                title: "Schoonmaak en periodiek onderhoud gemeentelijke gebouwen".to_owned(),
                buyer: Some("Gemeente Schaarbeek".to_owned()),
                description: "Schoonmaak".to_owned(),
                cpv_codes: vec!["90910000".to_owned()],
                regions: vec!["BE10".to_owned()],
                estimated_value: Some(Decimal::new(150_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-15".to_owned()),
                publication_date: Some("2026-10-02".to_owned()),
                languages: vec!["fr".to_owned(), "nl".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/103".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-004-000104".to_owned(),
                title: "Levering van gespecialiseerde medische diagnostische apparatuur".to_owned(),
                buyer: Some("UZ Brussel".to_owned()),
                description: "Medische toestellen".to_owned(),
                cpv_codes: vec!["33600000".to_owned()],
                regions: vec!["BE10".to_owned()],
                estimated_value: Some(Decimal::new(500_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-18".to_owned()),
                publication_date: Some("2026-10-03".to_owned()),
                languages: vec!["nl".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/104".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-005-000105".to_owned(),
                title: "Asfalteringswerken gewestwegen Henegouwen".to_owned(),
                buyer: Some("SPW Mobilité".to_owned()),
                description: "Wegenbouw".to_owned(),
                cpv_codes: vec!["45233140".to_owned()],
                regions: vec!["BE3".to_owned()],
                estimated_value: Some(Decimal::new(1_200_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-22".to_owned()),
                publication_date: Some("2026-10-04".to_owned()),
                languages: vec!["fr".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/105".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-006-000106".to_owned(),
                title: "Aanleg en fysieke bekabeling glasvezelinfrastructuur datacenter".to_owned(),
                buyer: Some("Belnet".to_owned()),
                description: "Bekabeling".to_owned(),
                cpv_codes: vec!["32562000".to_owned()],
                regions: vec!["BE10".to_owned()],
                estimated_value: Some(Decimal::new(300_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-30".to_owned()),
                publication_date: Some("2026-10-06".to_owned()),
                languages: vec!["nl".to_owned(), "en".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/106".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-007-000107".to_owned(),
                title: "Reiniging en industriële sanering na brandschade".to_owned(),
                buyer: Some("Haven van Luik".to_owned()),
                description: "Sanering".to_owned(),
                cpv_codes: vec!["90911200".to_owned()],
                regions: vec!["BE3".to_owned()],
                estimated_value: Some(Decimal::new(250_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-12-05".to_owned()),
                publication_date: Some("2026-10-07".to_owned()),
                languages: vec!["fr".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/107".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "bda".to_owned(),
                external_id: "2026-000108".to_owned(),
                title: "Raamovereenkomst IT-ondersteuning en helpdesk ter plaatse (omzetvereiste € 2.500.000)".to_owned(),
                buyer: Some("FOD BOSA".to_owned()),
                description: "Helpdesk support".to_owned(),
                cpv_codes: vec!["72253000".to_owned()],
                regions: vec!["BE10".to_owned()],
                estimated_value: Some(Decimal::new(2_500_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-28".to_owned()),
                publication_date: Some("2026-10-08".to_owned()),
                languages: vec!["nl".to_owned(), "fr".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://bda.be/108".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "bda".to_owned(),
                external_id: "2026-000109".to_owned(),
                title: "Catering en levering van schoolmaaltijden".to_owned(),
                buyer: Some("Stad Gent".to_owned()),
                description: "Schoolmaaltijden".to_owned(),
                cpv_codes: vec!["55523100".to_owned()],
                regions: vec!["BE2".to_owned()],
                estimated_value: Some(Decimal::new(400_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-19".to_owned()),
                publication_date: Some("2026-10-09".to_owned()),
                languages: vec!["nl".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://bda.be/109".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-010-000110".to_owned(),
                title: "Periodieke ruitenwasserij en gevelreiniging op hoogte".to_owned(),
                buyer: Some("Regie der Gebouwen".to_owned()),
                description: "Ruitenwasserij".to_owned(),
                cpv_codes: vec!["90911300".to_owned()],
                regions: vec!["BE10".to_owned()],
                estimated_value: Some(Decimal::new(120_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-12-01".to_owned()),
                publication_date: Some("2026-10-10".to_owned()),
                languages: vec!["nl".to_owned(), "fr".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/110".to_owned()],
                raw: serde_json::Value::Null,
            },
        ];

        let it_report = run_reproducible_benchmark(&it_profile, &tenders, &[], None, ref_date, 10);
        let fac_report =
            run_reproducible_benchmark(&facility_profile, &tenders, &[], None, ref_date, 10);

        println!("=== IT PROFILE PILOT RANKING ===");
        for item in &it_report.ranked_items {
            println!(
                "Rank {}: [{}] {} (score: {}, status: {}, reasons: {:?}, warnings: {:?})",
                item.rank,
                item.external_id,
                item.title,
                item.score,
                item.feasibility_status,
                item.reasons,
                item.warnings
            );
        }

        println!("=== FACILITY PROFILE PILOT RANKING ===");
        for item in &fac_report.ranked_items {
            println!(
                "Rank {}: [{}] {} (score: {}, status: {}, reasons: {:?}, warnings: {:?})",
                item.rank,
                item.external_id,
                item.title,
                item.score,
                item.feasibility_status,
                item.reasons,
                item.warnings
            );
        }

        // IT Profile asserts
        assert_eq!(it_report.candidates_evaluated, 10);
        let top_it_keys: Vec<_> = it_report
            .ranked_items
            .iter()
            .take(3)
            .map(|i| i.external_id.as_str())
            .collect();
        // Notice that 2026-000108 (the 2.5M contract) ranks in the top for IT!
        assert!(
            top_it_keys.contains(&"2026-000108")
                || top_it_keys.contains(&"2026-S-001-000101")
                || top_it_keys.contains(&"2026-S-002-000102")
        );

        // Facility Profile asserts
        assert_eq!(fac_report.candidates_evaluated, 10);
        let top_fac_keys: Vec<_> = fac_report
            .ranked_items
            .iter()
            .take(3)
            .map(|i| i.external_id.as_str())
            .collect();
        assert!(
            top_fac_keys.contains(&"2026-S-003-000103")
                || top_fac_keys.contains(&"2026-S-007-000107")
                || top_fac_keys.contains(&"2026-S-010-000110")
        );
    }

    #[test]
    fn test_capacity_aware_triage_demotes_the_impossible_contract() {
        use rust_decimal::Decimal;

        let s1 = std::fs::read_to_string("../tests/fixtures/eval/profiles/SYN-SME-001.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/profiles/SYN-SME-001.json"))
            .expect("should read SYN-SME-001.json");
        let it_profile: EvaluationProfile = serde_json::from_str(&s1).unwrap();

        let ref_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();

        let tenders = vec![
            Tender {
                source: "bda".to_owned(),
                external_id: "2026-000108".to_owned(),
                title: "Raamovereenkomst IT-ondersteuning en helpdesk ter plaatse (omzetvereiste € 2.500.000)".to_owned(),
                buyer: Some("FOD BOSA".to_owned()),
                description: "Helpdesk support met omzetvereiste".to_owned(),
                cpv_codes: vec!["72253000".to_owned()],
                regions: vec!["BE10".to_owned()],
                estimated_value: Some(Decimal::new(2_500_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-28".to_owned()),
                publication_date: Some("2026-10-08".to_owned()),
                languages: vec!["nl".to_owned(), "fr".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://bda.be/108".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-001-000101".to_owned(),
                title: "Ontwikkeling en onderhoud van een portaal voor open data".to_owned(),
                buyer: Some("Stad Antwerpen".to_owned()),
                description: "Webportaal".to_owned(),
                cpv_codes: vec!["72200000".to_owned()],
                regions: vec!["BE2".to_owned()],
                estimated_value: Some(Decimal::new(180_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-20".to_owned()),
                publication_date: Some("2026-10-01".to_owned()),
                languages: vec!["nl".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/101".to_owned()],
                raw: serde_json::Value::Null,
            },
            Tender {
                source: "ted".to_owned(),
                external_id: "2026-S-002-000102".to_owned(),
                title: "Migratie en modernisering cloudinfrastructuur".to_owned(),
                buyer: Some("Vlaamse Overheid".to_owned()),
                description: "Kubernetes en cloud".to_owned(),
                cpv_codes: vec!["72000000".to_owned()],
                regions: vec!["BE2".to_owned()],
                estimated_value: Some(Decimal::new(220_000, 0)),
                lot_values: vec![],
                deadline: Some("2026-11-25".to_owned()),
                publication_date: Some("2026-10-05".to_owned()),
                languages: vec!["nl".to_owned()],
                notice_type: Some("cn-standard".to_owned()),
                requirements: vec![],
                document_urls: vec!["https://ted.europa.eu/102".to_owned()],
                raw: serde_json::Value::Null,
            },
        ];

        // 1. Baseline: The impossible € 2.5M contract ranks #1
        let baseline = run_reproducible_benchmark(&it_profile, &tenders, &[], None, ref_date, 3);
        assert_eq!(baseline.system_variant, "baseline");
        assert_eq!(baseline.ranked_items[0].external_id, "2026-000108");
        assert_eq!(baseline.ranked_items[0].score, 80.6);
        assert_eq!(baseline.ranked_items[0].feasibility_status, "hinted");
        assert_eq!(baseline.ranked_items[0].feasibility_barrier, None);

        // 2. Improved Variant (FW-09): Capacity-Aware Triage downgrades the impossible contract
        let improved = run_capacity_aware_benchmark(&it_profile, &tenders, &[], None, ref_date, 3);
        assert_eq!(improved.system_variant, "improved");

        // Accessible contracts are promoted to Rank 1 and Rank 2!
        assert_eq!(improved.ranked_items[0].external_id, "2026-S-001-000101");
        assert_eq!(improved.ranked_items[0].score, 80.6);
        assert_eq!(improved.ranked_items[1].external_id, "2026-S-002-000102");
        assert_eq!(improved.ranked_items[1].score, 80.6);

        // The impossible contract is demoted with explicit barrier and warning
        assert_eq!(improved.ranked_items[2].external_id, "2026-000108");
        assert_eq!(
            improved.ranked_items[2].score,
            baseline.ranked_items[0].score - 30.0
        ); // capacity barrier applies the fixed penalty
        assert_eq!(
            improved.ranked_items[2].feasibility_status,
            "concrete_belemmering"
        );
        assert_eq!(
            improved.ranked_items[2].feasibility_barrier.as_deref(),
            Some("turnover_too_high")
        );
        assert!(improved.ranked_items[2]
            .warnings
            .iter()
            .any(|w| w.contains("Capaciteitsbelemmering")));
    }

    #[test]
    fn test_holdout_eindtest_full_comparison() {
        use std::collections::HashSet;

        // 1. Load profiles
        let s_it = std::fs::read_to_string("../tests/fixtures/eval/profiles/SYN-SME-001.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/profiles/SYN-SME-001.json"))
            .expect("should read SYN-SME-001.json");
        let it_profile: EvaluationProfile = serde_json::from_str(&s_it).unwrap();

        let s_fac = std::fs::read_to_string("../tests/fixtures/eval/profiles/SYN-SME-002.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/profiles/SYN-SME-002.json"))
            .expect("should read SYN-SME-002.json");
        let fac_profile: EvaluationProfile = serde_json::from_str(&s_fac).unwrap();

        // 2. Load pilot manifest to verify strict isolation
        let s_pilot = std::fs::read_to_string("../tests/fixtures/eval/manifest.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/manifest.json"))
            .expect("should read manifest.json");
        let pilot_manifest: EvaluationCorpusManifest = serde_json::from_str(&s_pilot).unwrap();
        let pilot_ids: HashSet<String> = pilot_manifest
            .procedures
            .iter()
            .map(|p| p.external_id.clone())
            .collect();
        assert_eq!(pilot_ids.len(), 10);

        // 3. Load holdout manifest (FW-10)
        let s_holdout = std::fs::read_to_string("../tests/fixtures/eval/holdout_manifest.json")
            .or_else(|_| std::fs::read_to_string("tests/fixtures/eval/holdout_manifest.json"))
            .expect("should read holdout_manifest.json");
        let holdout_manifest: EvaluationCorpusManifest =
            serde_json::from_str(&s_holdout).expect("valid holdout manifest json");
        assert_eq!(holdout_manifest.procedures.len(), 30);

        // Strict isolation guarantee: zero intersection with pilot
        for p in &holdout_manifest.procedures {
            assert!(
                !pilot_ids.contains(&p.external_id),
                "Holdout procedure {} collides with pilot dataset! Strict isolation violated.",
                p.external_id
            );
        }

        // 4. Load ground truth review records
        let s_reviews =
            std::fs::read_to_string("../tests/fixtures/eval/holdout_review_records.json")
                .or_else(|_| {
                    std::fs::read_to_string("tests/fixtures/eval/holdout_review_records.json")
                })
                .expect("should read holdout_review_records.json");
        let review_collection: ReviewRecordCollection =
            serde_json::from_str(&s_reviews).expect("valid holdout reviews json");
        assert_eq!(review_collection.records.len(), 30);

        // Convert manifest procedures to Tender domain objects
        let tenders: Vec<Tender> = holdout_manifest
            .procedures
            .iter()
            .map(|p| p.to_tender())
            .collect();
        assert_eq!(tenders.len(), 30);

        let ref_date = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        let k = 5;

        // ==========================================
        // 5. IT Profile (SYN-SME-001) Evaluation
        // ==========================================
        let it_baseline = run_reproducible_benchmark(&it_profile, &tenders, &[], None, ref_date, k);
        let it_improved =
            run_capacity_aware_benchmark(&it_profile, &tenders, &[], None, ref_date, k);

        let it_metrics_base = it_baseline
            .evaluate(&review_collection.records, EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;
        let it_metrics_imp = it_improved
            .evaluate(&review_collection.records, EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;

        println!("=== IT PROFILE (SYN-SME-001) BASELINE vs IMPROVED ===");
        println!(
            "Baseline P@{}: {:?}, Feasibility counts: {:?}",
            k, it_metrics_base.precision_at_k, it_metrics_base.feasibility_counts
        );
        for item in &it_baseline.ranked_items {
            println!(
                "  Base Rank {}: [{}] score: {:.1}, status: {}, barrier: {:?}",
                item.rank,
                item.external_id,
                item.score,
                item.feasibility_status,
                item.feasibility_barrier
            );
        }
        println!(
            "Improved P@{}: {:?}, Feasibility counts: {:?}",
            k, it_metrics_imp.precision_at_k, it_metrics_imp.feasibility_counts
        );
        for item in &it_improved.ranked_items {
            println!(
                "  Imp  Rank {}: [{}] score: {:.1}, status: {}, barrier: {:?}",
                item.rank,
                item.external_id,
                item.score,
                item.feasibility_status,
                item.feasibility_barrier
            );
        }

        // Thematic Precision at 5 remains high for IT in both
        assert!(it_metrics_base.precision_lower_bound.unwrap() >= 0.8);
        assert!(it_metrics_imp.precision_lower_bound.unwrap() >= 0.8);

        let base_top_ids: Vec<_> = it_baseline
            .ranked_items
            .iter()
            .map(|i| i.external_id.as_str())
            .collect();
        let imp_top_ids: Vec<_> = it_improved
            .ranked_items
            .iter()
            .map(|i| i.external_id.as_str())
            .collect();

        // Baseline includes turnover barrier contract in top-5
        assert!(base_top_ids.contains(&"2026-S-030-000211"));
        // Improved variant demotes it out of top-5
        assert!(!imp_top_ids.contains(&"2026-S-030-000211"));

        // In improved, accessible tenders are favored and impossible ones are demoted
        let imp_plausible = it_metrics_imp
            .feasibility_counts
            .get("plausibel")
            .copied()
            .unwrap_or(0);
        let base_plausible = it_metrics_base
            .feasibility_counts
            .get("plausibel")
            .copied()
            .unwrap_or(0);
        assert!(
            imp_plausible >= base_plausible,
            "Improved variant should have at least as many plausible items in top-5 (imp: {imp_plausible}, base: {base_plausible})"
        );

        // ==========================================
        // 6. Facility Profile (SYN-SME-002) Evaluation
        // ==========================================
        let fac_baseline =
            run_reproducible_benchmark(&fac_profile, &tenders, &[], None, ref_date, k);
        let fac_improved =
            run_capacity_aware_benchmark(&fac_profile, &tenders, &[], None, ref_date, k);

        let fac_metrics_base = fac_baseline
            .evaluate(&review_collection.records, EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;
        let fac_metrics_imp = fac_improved
            .evaluate(&review_collection.records, EVALUATION_PROTOCOL_VERSION)
            .unwrap()
            .metrics;

        println!("=== FACILITY PROFILE (SYN-SME-002) BASELINE vs IMPROVED ===");
        println!(
            "Baseline P@{}: {:?}, Feasibility counts: {:?}",
            k, fac_metrics_base.precision_at_k, fac_metrics_base.feasibility_counts
        );
        for item in &fac_baseline.ranked_items {
            println!(
                "  Base Rank {}: [{}] score: {:.1}, status: {}, barrier: {:?}",
                item.rank,
                item.external_id,
                item.score,
                item.feasibility_status,
                item.feasibility_barrier
            );
        }
        println!(
            "Improved P@{}: {:?}, Feasibility counts: {:?}",
            k, fac_metrics_imp.precision_at_k, fac_metrics_imp.feasibility_counts
        );
        for item in &fac_improved.ranked_items {
            println!(
                "  Imp  Rank {}: [{}] score: {:.1}, status: {}, barrier: {:?}",
                item.rank,
                item.external_id,
                item.score,
                item.feasibility_status,
                item.feasibility_barrier
            );
        }

        // Thematic precision at 5 remains high for Facility
        assert!(fac_metrics_base.precision_lower_bound.unwrap() >= 0.8);
        assert!(fac_metrics_imp.precision_lower_bound.unwrap() >= 0.8);

        // Baseline has assessed claims; improved has none in the frozen review set.
        assert_eq!(it_metrics_base.claim_fidelity_rate, Some(1.0));
        assert_eq!(it_metrics_imp.claim_fidelity_rate, None);
        assert_eq!(fac_metrics_base.claim_fidelity_rate, Some(1.0));
        assert_eq!(fac_metrics_imp.claim_fidelity_rate, None);
    }
}
