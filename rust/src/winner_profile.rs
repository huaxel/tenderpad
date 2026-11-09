//! Winner-profile fit: what do past winners of such tenders look like?
//!
//! Exact history match only fires for companies that already won the same
//! CPV. This signal generalizes it: NACE-division × CPV-division affinity
//! learned from `ted_awards_history` (linked, high-confidence joins), as
//! smoothed log-odds. A works contractor with no award history still scores
//! above a bakery on a works tender, because works-shaped companies win
//! works tenders.
//!
//! Measured 2026-10-23 (train ≤2019, test ≥2020, exact+normalized
//! non-ambiguous joins): true winners outscore random enterprises with
//! AUC-like **0.878** over 38k pairs. Caveat: random negatives are easy —
//! production retrieval ranks plausible companies, so treat this as an
//! upper bound, not a deployment number.
//!
//! The signal ships at **zero weight** (`matcher_tables.json`:
//! `"winner_profile": 0.0`): visible in score breakdowns, contributing
//! nothing until a calibration run on held-out pairs sets a real weight.
//! Tables live in `rust/data/winner_profile.json`, built by the
//! `train-winner-profile` binary; scoring formulas stay here.
//!
//! Calibration status (2026-10-23, `calibrate-winner-weight`): attempted on
//! 400 held-out awards, ranking each winner among 20 same-sector lookalikes
//! with the real scorer. Result: base totals tie on synthetic tenders (no
//! titles/regions/deadlines survive the archive), so the stable sort parks
//! the winner first and weight-0 MRR reads a perfect 1.0000 — a sort-order
//! artifact, caught by the binary's tie diagnostic, not discrimination.
//! Nonzero weights only perturb ties. Verdict recorded by the tool: KEEP
//! 0.0; reopen with real (company, live-tender) outcome pairs, where base
//! signals actually vary.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use crate::{CompanyProfile, Tender};

#[derive(Debug, serde::Deserialize)]
struct WinnerProfileTables {
    #[allow(dead_code)]
    provenance: serde_json::Value,
    #[allow(dead_code)]
    cpv_divisions: usize,
    logodds: HashMap<String, HashMap<String, f64>>,
}

fn tables() -> &'static WinnerProfileTables {
    static TABLES: OnceLock<WinnerProfileTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        serde_json::from_str(include_str!("../data/winner_profile.json"))
            .expect("winner_profile.json should parse")
    })
}

fn division(code: &str) -> Option<&str> {
    let trimmed = code.trim();
    (!trimmed.is_empty()).then(|| trimmed.get(..2).unwrap_or(trimmed))
}

/// Mean NACE×CPV affinity for the company's activities against the tender's
/// CPVs, mapped to [0,1]. `0.5` = no information (no NACE, no CPV, or no
/// learned pair) — not a penalty.
pub fn winner_profile_fit(company: &CompanyProfile, tender: &Tender) -> f64 {
    let tables = tables();
    let mut total = 0.0;
    let mut count = 0_usize;
    for activity in &company.activities {
        let Some(nace) = division(&activity.code) else {
            continue;
        };
        let Some(row) = tables.logodds.get(nace) else {
            continue;
        };
        for cpv in &tender.cpv_codes {
            let Some(cpv_div) = division(cpv) else {
                continue;
            };
            if let Some(affinity) = row.get(cpv_div) {
                total += affinity;
                count += 1;
            }
        }
    }
    if count == 0 {
        return 0.5;
    }
    let mean = total / count as f64;
    1.0 / (1.0 + (-mean).exp())
}

/// Count table builder shared by the trainer binary and unit tests:
/// `(nace divisions, cpv division)` pairs accumulate into smoothed log-odds
/// against the CPV-division prior. Laplace +1 over the CPV-division support
/// keeps unseen pairs finite and negative (rare, not impossible).
pub fn build_logodds(
    pairs: &[(Vec<String>, String)],
    cpv_support: usize,
) -> BTreeMap<String, BTreeMap<String, f64>> {
    let mut joint: HashMap<(String, String), usize> = HashMap::new();
    let mut nace_totals: HashMap<String, usize> = HashMap::new();
    let mut cpv_totals: HashMap<String, usize> = HashMap::new();
    let mut total = 0_usize;
    for (divisions, cpv) in pairs {
        for nace in divisions {
            *joint.entry((nace.clone(), cpv.clone())).or_default() += 1;
            *nace_totals.entry(nace.clone()).or_default() += 1;
            *cpv_totals.entry(cpv.clone()).or_default() += 1;
            total += 1;
        }
    }
    let support = cpv_support.max(1) as f64;
    let mut tables = BTreeMap::new();
    for ((nace, cpv), count) in joint {
        let posterior = (count as f64 + 1.0) / (nace_totals[&nace] as f64 + support);
        let prior = (cpv_totals[&cpv] as f64 + 1.0) / (total as f64 + support);
        tables
            .entry(nace)
            .or_insert_with(BTreeMap::new)
            .insert(cpv, (posterior / prior).ln());
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Activity;

    fn company_with(nace: &[&str]) -> CompanyProfile {
        CompanyProfile {
            enterprise_number: "test".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: Vec::new(),
            activities: nace
                .iter()
                .map(|code| Activity {
                    code: code.to_string(),
                    classification: "MAIN".to_owned(),
                    french: None,
                    dutch: None,
                })
                .collect(),
            addresses: Vec::new(),
            establishments: Vec::new(),
            ..Default::default()
        }
    }

    fn tender_with(cpv: &[&str]) -> Tender {
        Tender {
            source: "test".to_owned(),
            external_id: "t".to_owned(),
            title: String::new(),
            cpv_codes: cpv.iter().map(ToString::to_string).collect(),
            ..Tender::default()
        }
    }

    #[test]
    fn learned_affinity_orders_matching_pairs_first() {
        // Construction NACE wins construction CPVs far more than IT NACE.
        let pairs = vec![
            (vec!["45".to_owned()], "45".to_owned()),
            (vec!["45".to_owned()], "45".to_owned()),
            (vec!["45".to_owned()], "45".to_owned()),
            (vec!["72".to_owned()], "72".to_owned()),
        ];
        let tables = build_logodds(&pairs, 2);
        let same = tables["45"]["45"];
        assert!(same > 0.0, "matching pair should attract: {same}");
        // Unseen pairs stay absent and read as no info at score time.
        assert!(!tables["45"].contains_key("72"));
        assert!(!tables["72"].contains_key("45"));
    }

    #[test]
    fn no_information_returns_neutral() {
        // Unknown NACE division and empty CPVs both read as 0.5, not 0.0.
        let known = company_with(&["45"]);
        assert_eq!(winner_profile_fit(&known, &tender_with(&[])), 0.5);
        assert_eq!(
            winner_profile_fit(&company_with(&[]), &tender_with(&["45"])),
            0.5
        );
    }

    #[test]
    fn division_takes_two_chars() {
        assert_eq!(division("61100"), Some("61"));
        assert_eq!(division("45"), Some("45"));
        assert_eq!(division(""), None);
        assert_eq!(division("   "), None);
    }

    #[test]
    fn shipped_tables_resolve_construction_and_it() {
        // Smoke on the committed artifact: works-shaped affinity exists.
        let works = winner_profile_fit(&company_with(&["45100"]), &tender_with(&["45000000"]));
        let mismatch = winner_profile_fit(&company_with(&["61100"]), &tender_with(&["45000000"]));
        assert!(
            works > mismatch,
            "works company should outscore IT company on works tender: {works} vs {mismatch}"
        );
    }
}
