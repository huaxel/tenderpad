//! Begeleidersdemonstratie voor Final Work (FW-11).
//!
//! Voert een volledige, reproduceerbare vergelijking uit tussen de baseline matcher
//! en de capaciteitsbewuste triage (FW-09) op basis van synthetische kmo-profielen
//! en gecontroleerde evaluatiecorpora.
//!
//! Geen externe credentials, betaalde AI-sleutels of cliëntdata vereist.

use aanbestedingen_matcher::eval_contract::{
    run_capacity_aware_benchmark, run_reproducible_benchmark, BenchmarkEvaluation, BenchmarkItem,
    BenchmarkMetrics, BenchmarkReport, EvaluationCorpusManifest, EvaluationProfile,
    FeasibilityLabel, ReviewRecordCollection, EVALUATION_PROTOCOL_VERSION,
};
use chrono::NaiveDate;
use std::env;
use std::path::Path;

#[derive(Debug, PartialEq)]
struct DemoOptions {
    profile_id: String,
    dataset: String,
    k: usize,
}

fn parse_args<I>(args: I) -> Result<Option<DemoOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut profile_id = "SYN-SME-001".to_owned();
    let mut dataset = "holdout".to_owned();
    let mut k = 5;

    while let Some(arg) = arguments.next() {
        match arg.as_str() {
            "--profile" => {
                profile_id = arguments.next().ok_or("--profile vereist een profiel-ID")?
            }
            "--dataset" => {
                dataset = arguments
                    .next()
                    .ok_or("--dataset vereist pilot of holdout")?
            }
            "--k" => {
                k = arguments
                    .next()
                    .ok_or("--k vereist een numerieke waarde")?
                    .parse()?
            }
            "--help" | "-h" => return Ok(None),
            arg if arg.starts_with('-') => return Err(format!("onbekend argument: {arg}").into()),
            arg => {
                // Positional profile shortcut
                if arg == "SYN-SME-001" || arg == "SYN-SME-002" || arg == "it" || arg == "fac" {
                    profile_id = match arg {
                        "it" => "SYN-SME-001".to_owned(),
                        "fac" => "SYN-SME-002".to_owned(),
                        other => other.to_owned(),
                    };
                } else {
                    return Err(format!("onverwacht argument: {arg}").into());
                }
            }
        }
    }

    if !matches!(dataset.as_str(), "pilot" | "holdout") {
        return Err("--dataset vereist pilot of holdout".into());
    }

    Ok(Some(DemoOptions {
        profile_id,
        dataset,
        k,
    }))
}

fn resolve_fixture_path(relative: &str) -> String {
    if Path::new(relative).exists() {
        relative.to_owned()
    } else {
        format!("../{relative}")
    }
}

fn rate(value: Option<f64>) -> String {
    value
        .map(|v| format!("{:.0}%", v * 100.0))
        .unwrap_or_else(|| "niet beoordeeld".to_owned())
}

fn performance_summary(metrics: &BenchmarkMetrics) -> String {
    format!(
        "P@{} (gevraagd {}): {} | marge [{}–{}] | Beoordeeld: {}/{} (onvoldoende informatie: {})\n  Brongetrouwheid: {} ({}/{} ondersteunde/getoetste claims; {}/{} dossiers met claimtoets) | Zelfverklaarde blindfase: {}; niet-geblindeerd: {}",
        metrics.evaluated_count, metrics.k, rate(metrics.precision_at_k),
        rate(metrics.precision_lower_bound), rate(metrics.precision_upper_bound),
        metrics.judged_count, metrics.evaluated_count, metrics.insufficient_info_count,
        rate(metrics.claim_fidelity_rate), metrics.supported_claim_count, metrics.assessed_claim_count,
        metrics.items_with_assessed_claims, metrics.evaluated_count,
        metrics.declared_blinded_review_count, metrics.unblinded_review_count,
    )
}

fn comparison_summary(baseline: &BenchmarkMetrics, improved: &BenchmarkMetrics) -> String {
    let count = |m: &BenchmarkMetrics| m.feasibility_counts.get("plausibel").copied().unwrap_or(0);
    format!(
        "  * Thematische precisie: {} baseline → {} capaciteitsbewust\n  * Geannoteerd plausibel: {}/{} baseline → {}/{} capaciteitsbewust\n  * Dekking: {}/{} baseline → {}/{} capaciteitsbewust\n  * Brongetrouwheid: {} baseline → {} capaciteitsbewust; ontwikkelaarsevaluatie\n  * Conclusie: Deze kleine, synthetische evaluatie bevestigt geen algemene kwaliteitsverbetering.",
        rate(baseline.precision_at_k), rate(improved.precision_at_k),
        count(baseline), baseline.evaluated_count, count(improved), improved.evaluated_count,
        baseline.judged_count, baseline.evaluated_count, improved.judged_count, improved.evaluated_count,
        rate(baseline.claim_fidelity_rate), rate(improved.claim_fidelity_rate),
    )
}

fn review_annotation<'a>(
    evaluation: &BenchmarkEvaluation<'a>,
    item: &BenchmarkItem,
) -> &'static str {
    match evaluation
        .review_for(item)
        .map(|r| &r.participation_feasibility.label)
    {
        Some(FeasibilityLabel::Plausibel) => "[OK] Plausibel volgens profielreview",
        Some(FeasibilityLabel::ConcreteBelemmering) => {
            "[!!] Concrete belemmering volgens profielreview"
        }
        Some(FeasibilityLabel::Onbekend) => "[??] Onbekend volgens profielreview",
        None => "[--] Niet beoordeeld voor geselecteerd profiel",
    }
}

fn print_ranking(report: &BenchmarkReport, evaluation: &BenchmarkEvaluation<'_>) {
    for item in report.ranked_items.iter().take(report.k) {
        println!(
            "  Rang {}: [{}] (Score: {:.1}) {}",
            item.rank,
            item.external_id,
            item.score,
            review_annotation(evaluation, item)
        );
        println!("          Titel: {}", item.title);
        if let Some(claim) = evaluation.claims_for(item).next() {
            println!(
                "          Bewering ({:?}): {}",
                claim.verdict, claim.claim_text
            );
            if let Some(quote) = &claim.source_passage_quote {
                println!(
                    "          Bestekcitaat: \"{quote}\" ({})",
                    claim
                        .source_location
                        .as_deref()
                        .unwrap_or("locatie niet beoordeeld")
                );
            }
        }
        if !item.reasons.is_empty() {
            println!("          Redenen: {}", item.reasons.join(" | "));
        }
        if !item.warnings.is_empty() {
            println!("          Signalen: {}", item.warnings.join(" | "));
        }
        println!();
    }
    println!("  >> {}\n", performance_summary(&evaluation.metrics));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = match parse_args(env::args().skip(1))? {
        Some(opts) => opts,
        None => {
            println!(
                "Gebruik: supervisor-demo [--profile SYN-SME-001|SYN-SME-002] [--dataset holdout|pilot] [--k 5]\n\n\
                 Demonstreert de capaciteitsbewuste matching en triage voor de bachelorproefbegeleider."
            );
            return Ok(());
        }
    };

    println!("================================================================================");
    println!("ACADEMISCHE BEGELEIDERSDEMONSTRATIE: OVERHEIDSOPDRACHTEN MATCHING (FW-11)");
    println!("Juan Benjumea - Erasmushogeschool Brussel (EhB)");
    println!("Geen externe credentials of cliëntdata vereist | 100% reproduceerbare evaluatie");
    println!("================================================================================\n");

    // 1. Laad synthetisch profiel
    let profile_file = format!("tests/fixtures/eval/profiles/{}.json", options.profile_id);
    let resolved_profile = resolve_fixture_path(&profile_file);
    let profile_str = std::fs::read_to_string(&resolved_profile)
        .map_err(|e| format!("Kan profielbestand {resolved_profile} niet lezen: {e}"))?;
    let profile: EvaluationProfile = serde_json::from_str(&profile_str)?;

    println!("[1] GESELECTEERD KMO-BEDRIJFSPROFIEL:");
    println!("  Bedrijfsnaam:     {}", profile.name);
    println!("  Profiel-ID:       {}", profile.profile_id);
    println!("  Sector / Focus:   {}", profile.sector_description);
    println!("  NACE-activiteiten: {:?}", profile.nace_codes);
    println!("  Doelregio's:      {:?}", profile.regions);
    println!(
        "  Jaaromzet:        € {:.0} (excl. btw)",
        profile.capacity.annual_turnover_eur
    );
    println!(
        "  Biedcapaciteit:   € {:.0} (max per inschrijving)",
        profile.capacity.max_bid_capacity_eur
    );
    println!();

    // 2. Laad manifest en review records
    let (manifest_rel, reviews_rel) = if options.dataset == "pilot" {
        (
            "tests/fixtures/eval/manifest.json",
            "tests/fixtures/eval/pilot_review_records.json",
        )
    } else {
        (
            "tests/fixtures/eval/holdout_manifest.json",
            "tests/fixtures/eval/holdout_review_records.json",
        )
    };
    let manifest_path = resolve_fixture_path(manifest_rel);
    let manifest_str = std::fs::read_to_string(&manifest_path)?;
    let manifest: EvaluationCorpusManifest = serde_json::from_str(&manifest_str)?;

    let reviews_path = resolve_fixture_path(reviews_rel);
    let reviews_str = std::fs::read_to_string(&reviews_path)?;
    let review_collection: ReviewRecordCollection = serde_json::from_str(&reviews_str)?;

    let tenders: Vec<_> = manifest.procedures.iter().map(|p| p.to_tender()).collect();
    let ref_date = NaiveDate::parse_from_str(&manifest.evaluation_reference_date, "%Y-%m-%d")?;

    println!("[2] TESTDATASET:");
    println!(
        "  Naam:             {} ({})",
        manifest.dataset_name, options.dataset
    );
    println!(
        "  Dossiers:         {} procedures geëvalueerd",
        tenders.len()
    );
    println!(
        "  Referentiedatum:  {} (geen kalenderdrift)",
        manifest.evaluation_reference_date
    );
    println!();

    // 3. Uitvoeren Baseline Matching
    let baseline_report =
        run_reproducible_benchmark(&profile, &tenders, &[], None, ref_date, options.k);
    let baseline_evaluation =
        baseline_report.evaluate(&review_collection.records, EVALUATION_PROTOCOL_VERSION)?;

    println!("--------------------------------------------------------------------------------");
    println!(
        "[3] BASELINE RESULTATEN (Traditionele relevantie-matching, Top-{}):",
        options.k
    );
    println!("--------------------------------------------------------------------------------");
    print_ranking(&baseline_report, &baseline_evaluation);

    // 4. Uitvoeren Verbeterde Capaciteitsbewuste Triage (FW-09)
    let improved_report =
        run_capacity_aware_benchmark(&profile, &tenders, &[], None, ref_date, options.k);
    let improved_evaluation =
        improved_report.evaluate(&review_collection.records, EVALUATION_PROTOCOL_VERSION)?;

    println!("--------------------------------------------------------------------------------");
    println!(
        "[4] CAPACITEITSBEWUSTE TRIAGE RESULTATEN (FW-09 Verbetering, Top-{}):",
        options.k
    );
    println!("--------------------------------------------------------------------------------");
    print_ranking(&improved_report, &improved_evaluation);

    // 5. Vergelijkende Synthese
    println!("================================================================================");
    println!("[5] VERGELIJKING EN HYPOTHESETOETS:");
    println!("================================================================================");
    println!(
        "{}",
        comparison_summary(&baseline_evaluation.metrics, &improved_evaluation.metrics)
    );
    println!("================================================================================\n");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_demo_parses_options() {
        let opts = parse_args(vec![
            "--profile".to_owned(),
            "SYN-SME-002".to_owned(),
            "--dataset".to_owned(),
            "pilot".to_owned(),
            "--k".to_owned(),
            "3".to_owned(),
        ])
        .unwrap()
        .unwrap();

        assert_eq!(opts.profile_id, "SYN-SME-002");
        assert_eq!(opts.dataset, "pilot");
        assert_eq!(opts.k, 3);
    }

    #[test]
    fn comparison_summary_reports_both_precisions_and_avoids_quality_claims() {
        let metrics = |file: &str| {
            let manifest: EvaluationCorpusManifest =
                serde_json::from_str(&std::fs::read_to_string(resolve_fixture_path(file)).unwrap())
                    .unwrap();
            let profile: EvaluationProfile = serde_json::from_str(
                &std::fs::read_to_string(resolve_fixture_path(
                    "tests/fixtures/eval/profiles/SYN-SME-001.json",
                ))
                .unwrap(),
            )
            .unwrap();
            let report = run_reproducible_benchmark(
                &profile,
                &manifest
                    .procedures
                    .iter()
                    .map(|p| p.to_tender())
                    .collect::<Vec<_>>(),
                &[],
                None,
                NaiveDate::parse_from_str(&manifest.evaluation_reference_date, "%Y-%m-%d").unwrap(),
                5,
            );
            report
                .evaluate(&[], EVALUATION_PROTOCOL_VERSION)
                .unwrap()
                .metrics
        };
        let baseline = metrics("tests/fixtures/eval/holdout_manifest.json");
        let summary = comparison_summary(&baseline, &baseline);
        assert!(summary.contains("niet beoordeeld baseline → niet beoordeeld capaciteitsbewust"));
        assert!(summary.contains("Dekking: 0/5 baseline → 0/5"));
        assert!(summary.contains("bevestigt geen algemene kwaliteitsverbetering"));
        assert!(!summary.contains("empirisch bevestigd"));
        let performance = performance_summary(&baseline);
        assert!(performance.contains("P@5 (gevraagd 5): niet beoordeeld | marge [0%–100%]"));
        assert!(performance.contains("Brongetrouwheid: niet beoordeeld (0/0"));
    }

    #[test]
    fn annotations_never_borrow_other_profile_or_variant() {
        let load = |path| std::fs::read_to_string(resolve_fixture_path(path)).unwrap();
        let manifest: EvaluationCorpusManifest =
            serde_json::from_str(&load("tests/fixtures/eval/holdout_manifest.json")).unwrap();
        let profile: EvaluationProfile =
            serde_json::from_str(&load("tests/fixtures/eval/profiles/SYN-SME-001.json")).unwrap();
        let reviews: ReviewRecordCollection =
            serde_json::from_str(&load("tests/fixtures/eval/holdout_review_records.json")).unwrap();
        let mut report = run_reproducible_benchmark(
            &profile,
            &manifest
                .procedures
                .iter()
                .map(|p| p.to_tender())
                .collect::<Vec<_>>(),
            &[],
            None,
            NaiveDate::parse_from_str(&manifest.evaluation_reference_date, "%Y-%m-%d").unwrap(),
            30,
        );
        let evaluation = report
            .evaluate(&reviews.records, EVALUATION_PROTOCOL_VERSION)
            .unwrap();
        let mut missing = 0;
        for item in &report.ranked_items {
            let has_profile_review = reviews.records.iter().any(|r| {
                r.profile_id == profile.profile_id
                    && r.procedure_key == format!("{}:{}", item.source, item.external_id)
            });
            if !has_profile_review {
                missing += 1;
                assert_eq!(
                    review_annotation(&evaluation, item),
                    "[--] Niet beoordeeld voor geselecteerd profiel"
                );
                assert_eq!(evaluation.claims_for(item).count(), 0);
            }
        }
        assert!(
            missing > 0,
            "fixture must exercise missing-profile annotations"
        );
        report.system_variant = "improved".to_owned();
        let evaluation = report
            .evaluate(&reviews.records, EVALUATION_PROTOCOL_VERSION)
            .unwrap();
        assert_eq!(evaluation.metrics.claim_fidelity_rate, None);
        assert!(report
            .ranked_items
            .iter()
            .all(|item| evaluation.claims_for(item).next().is_none()));
    }

    #[test]
    fn demo_rejects_unknown_dataset() {
        assert!(parse_args(vec!["--dataset".to_owned(), "typo".to_owned()]).is_err());
    }

    #[test]
    fn test_demo_shortcut_parsing() {
        let opts = parse_args(vec!["it".to_owned()]).unwrap().unwrap();
        assert_eq!(opts.profile_id, "SYN-SME-001");

        let opts_fac = parse_args(vec!["fac".to_owned()]).unwrap().unwrap();
        assert_eq!(opts_fac.profile_id, "SYN-SME-002");
    }
}
