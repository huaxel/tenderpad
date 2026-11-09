//! Score one stored tender for one company, **including a tender whose deadline
//! has passed**.
//!
//! `recommend` answers "what can I bid on now", so it filters by deadline on
//! purpose and can never settle the label-free known-item question: *would the
//! product have surfaced the notice for a contract this company actually won?*
//! Measured 2026-10-19: Evere's 2024 waste-collection call (`484632-2024`) is in
//! a backfilled store, but a passed deadline keeps it out of the ranking, so its
//! position was unanswerable. This command answers it.
//!
//! ```text
//! score-tender 0429366144 ted 484632-2024 --database /tmp/backfill.duckdb
//! ```
//!
//! It prints the score, confidence, reasons and warnings exactly as the ranking
//! would compute them, then says where that score would have sat: its position
//! among the still-open candidates and among every stored notice, and whether
//! the ranking would have excluded it. It ranks the whole store for that
//! company, so the position is a real one rather than a guess. Read-only.

use aanbestedingen_matcher::kbo::{normalize_enterprise_number, KboRepository};
use aanbestedingen_matcher::recommendations::{rank, sort_by_score, Recommendation};
use aanbestedingen_matcher::store::TenderStore;
use aanbestedingen_matcher::{award_title_history_similarity, score_breakdown};
use chrono::Utc;
use serde_json::json;
use std::env;

/// Cap on how many stored notices are ranked to place the target. Higher than
/// any realistic store of Belgian notices in one window; a truncated ladder
/// says so rather than pretending.
const LADDER_LIMIT: usize = 100_000;

#[derive(Debug, PartialEq)]
struct Options {
    enterprise_number: String,
    source: String,
    external_id: String,
    database: String,
    kbo_database: String,
    /// Print the weighted terms behind the score instead of only the score.
    explain: bool,
    /// Print the top of the ladder with each candidate's weighted terms, so the
    /// ranking's actual discrimination can be measured instead of assumed.
    ladder_limit: usize,
}

fn parse_args<I>(args: I) -> Result<Option<Options>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut explain = false;
    let mut ladder_limit = 0_usize;
    let mut positional: Vec<String> = Vec::new();
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?
            }
            "--explain" => explain = true,
            "--ladder" => {
                ladder_limit = arguments.next().ok_or("--ladder needs a value")?.parse()?;
            }
            "--help" | "-h" => return Ok(None),
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}").into())
            }
            value => positional.push(value.to_owned()),
        }
    }
    if positional.len() != 3 {
        return Err(
            "usage: score-tender ENTERPRISE_NUMBER SOURCE EXTERNAL_ID [--database PATH]".into(),
        );
    }
    Ok(Some(Options {
        enterprise_number: normalize_enterprise_number(&positional[0])?,
        source: positional[1].clone(),
        external_id: positional[2].clone(),
        database,
        kbo_database,
        explain,
        ladder_limit,
    }))
}

/// Where a score sits in a descending ladder: how many notices are strictly
/// ahead of it. Ties are reported separately so an equal score is not sold as a
/// better rank than it is.
fn position_in(scores: &[f64], target: f64) -> (usize, usize) {
    let ahead = scores.iter().filter(|score| **score > target).count();
    let tied = scores.iter().filter(|score| **score == target).count();
    (ahead + 1, tied)
}

fn tender_deadline_date(tender: &aanbestedingen_matcher::Tender) -> Option<chrono::NaiveDate> {
    tender
        .deadline
        .as_deref()
        .and_then(|value| chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
}

fn summarize(item: &Recommendation) -> serde_json::Value {
    json!({
        "source": item.source,
        "external_id": item.external_id,
        "title": item.title,
        "buyer": item.buyer,
        "deadline": item.deadline,
        "estimated_value": item.estimated_value,
        "score": item.score,
        "confidence": item.confidence,
        "reasons": item.reasons,
        "warnings": item.warnings,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: score-tender ENTERPRISE_NUMBER SOURCE EXTERNAL_ID [--database PATH]");
        println!("       [--kbo-database PATH]");
        println!();
        println!("Scores one stored tender for one company even when its deadline has passed,");
        println!("and reports where that score would have sat for the company.");
        println!();
        println!("  --explain   also print the weighted terms behind the score");
        println!("  --ladder N  print the top N candidates with their weighted terms");
        return Ok(());
    };

    let explain = options.explain;
    let ladder_limit = options.ladder_limit;
    let repository = KboRepository::open(&options.kbo_database)?;
    let company = repository
        .lookup(&options.enterprise_number)?
        .ok_or("company not found")?;
    let store = TenderStore::open_read_only(&options.database)?;
    let target = store
        .tender(&options.source, &options.external_id)?
        .ok_or("tender not found in this database")?;
    let preferences = store.preferences_for(&company.enterprise_number)?;
    let history = store.awards_for(&company.enterprise_number)?;
    let feedback = store.feedback_for(&company.enterprise_number)?;

    let today = Utc::now().date_naive();
    // The ladder is the set the ranking would actually have offered, taken from
    // the store's own opportunity filter (award results, PINs and VEATs are not
    // biddable). Ranking `store.tenders()` instead measured the score against
    // notices no candidate list ever contains: 851 of the 2,889 rows in the test
    // store were award results, revisions or qualification notices.
    //
    // For a closed notice the reference date is its own deadline, so the ladder
    // holds what was open *then*, not today's candidates.
    let ladder_date = tender_deadline_date(&target)
        .filter(|deadline| *deadline < today)
        .unwrap_or(today);
    let ladder_tenders = store.open_tenders_as_of(LADDER_LIMIT, ladder_date)?;
    let mut ladder = rank(
        &company,
        &ladder_tenders,
        &history,
        &preferences,
        &feedback,
        ladder_tenders.len(),
    );
    sort_by_score(&mut ladder);
    let scores: Vec<f64> = ladder.iter().map(|item| item.score).collect();

    let mut target_ladder = rank(
        &company,
        std::slice::from_ref(&target),
        &history,
        &preferences,
        &feedback,
        1,
    );
    let item = target_ladder
        .pop()
        .ok_or("the tender could not be scored")?;

    let (rank_ladder, tied) = position_in(&scores, item.score);
    // The breakdown is computed from the same function the ranking uses, so the
    // explanation cannot disagree with the score it explains.
    let breakdown = score_breakdown(&company, &target, &history, &preferences);
    let eligible = target
        .deadline
        .as_deref()
        .is_none_or(|deadline| deadline >= today.to_string().as_str());
    // Only the cap makes a ladder incomplete; a small ladder is a small ladder.
    let truncated = ladder_tenders.len() >= LADDER_LIMIT;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "company": company.enterprise_number,
            "ladder": {
                "date": ladder_date.to_string(),
                "candidates": scores.len(),
                "truncated": truncated,
            },
            "tender": summarize(&item),
            "position": {
                "in_ladder": rank_ladder,
                "ties_at_that_score": tied,
            },
            "explain": explain.then(|| {
                let mut details = serde_json::to_value(&breakdown).unwrap_or_default();
                if let Some(fields) = details.as_object_mut() {
                    let signal = award_title_history_similarity(&history, &target);
                    fields.insert(
                        "award_title_similarity".to_owned(),
                        json!({"signal": signal, "weight": 0.0, "points": 0.0}),
                    );
                }
                details
            }),
            "ladder_top": ladder
                .iter()
                .take(ladder_limit)
                .map(|entry| {
                    let tender = ladder_tenders
                        .iter()
                        .find(|tender| {
                            tender.external_id == entry.external_id && tender.source == entry.source
                        });
                    serde_json::json!({
                        "external_id": entry.external_id,
                        "title": entry.title,
                        "score": entry.score,
                        "terms": tender.map(|tender| {
                            let breakdown =
                                score_breakdown(&company, tender, &history, &preferences);
                            serde_json::json!({
                                "activity": breakdown.activity.points,
                                "region": breakdown.region.points,
                                "value": breakdown.value.points,
                                "history": breakdown.history.points,
                                "deadline": breakdown.deadline.points,
                                "recognition": breakdown.recognition.points,
                                "requirements": breakdown.requirements.points,
                                "award_title_similarity_signal": award_title_history_similarity(&history, tender),
                                "language_points": breakdown.language_points,
                            })
                        }),
                    })
                })
                .collect::<Vec<_>>(),
            "ranking_would_consider_it": eligible,
            "note": if eligible {
                "the deadline is still open, so the ranking already considers this notice"
            } else {
                "the deadline has passed: the ranking excludes it by design, which is why this command scores it directly"
            },
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn takes_a_company_a_tender_and_optional_paths() {
        let options = parse_args(arguments(&[
            "0429366144",
            "ted",
            "484632-2024",
            "--database",
            "test.duckdb",
        ]))
        .expect("arguments should parse")
        .expect("options");
        assert_eq!(options.enterprise_number, "0429.366.144");
        assert_eq!(options.source, "ted");
        assert_eq!(options.external_id, "484632-2024");
        assert_eq!(options.database, "test.duckdb");
        assert_eq!(options.kbo_database, "data/kbo/kbo_enrich.duckdb");
    }

    #[test]
    fn rejects_a_wrong_number_of_positional_arguments() {
        assert!(parse_args(arguments(&["0429366144", "ted"])).is_err());
        assert!(parse_args(arguments(&["0429366144", "ted", "id", "extra"])).is_err());
        assert!(parse_args(arguments(&[])).is_err());
        assert!(parse_args(arguments(&["--unknown"])).is_err());
        assert!(parse_args(arguments(&["--help"]))
            .expect("help should parse")
            .is_none());
    }

    /// The position counts what is strictly ahead, and reports ties separately
    /// so an equal score is not sold as a better rank.
    #[test]
    fn position_counts_ahead_and_ties_separately() {
        let ladder = vec![88.8, 76.2, 70.6, 70.6, 60.6, 60.6, 60.6, 42.5];
        assert_eq!(position_in(&ladder, 60.6), (5, 3));
        assert_eq!(position_in(&ladder, 88.8), (1, 1));
        assert_eq!(position_in(&ladder, 10.0), (9, 0));
        assert_eq!(position_in(&[], 50.0), (1, 0));
    }
}
