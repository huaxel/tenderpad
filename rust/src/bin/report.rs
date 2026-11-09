use aanbestedingen_matcher::cpv::{cpv_label_nl, cpv_name_en};
use aanbestedingen_matcher::recommendations::corpus_evidence;
use aanbestedingen_matcher::store::TenderStore;
use aanbestedingen_matcher::{collapse_notice_versions, Tender};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::env;

#[derive(Debug, PartialEq)]
struct ReportOptions {
    database: String,
}

fn parse_args<I>(args: I) -> Result<Option<ReportOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }
    Ok(Some(ReportOptions { database }))
}

/// What the stored rows actually offer: award results, PINs and VEATs are not
/// biddable, and one procurement is republished whenever it changes, so the row
/// count overstates the corpus. Company-independent evidence mix comes from
/// [`corpus_evidence`], shared with `ted-ingest` (stored per run): live and
/// stored can only differ on as-of time. Measured 2026-10-19 on a 180-day store: 6,035
/// rows are 4,085 opportunities and 2,670 procurements. The collapse is the same
/// function the ranking uses, so the two can never disagree.
fn corpus_counts(tenders: &[Tender]) -> (usize, usize, usize) {
    let opportunities: Vec<Tender> = tenders
        .iter()
        .filter(|tender| tender.is_opportunity())
        .cloned()
        .collect();
    let award_notices = tenders.len() - opportunities.len();
    let procurements = collapse_notice_versions(opportunities.clone()).len();
    (opportunities.len(), award_notices, procurements)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: report [--database PATH]");
        return Ok(());
    };
    let ReportOptions { database } = options;
    // Reporting only reads: open read-only so a wrong --database fails loudly
    // instead of creating an empty database and reporting zero tenders.
    let store = TenderStore::open_read_only(database)?;
    const MAX_REPORTED_TENDERS: usize = 100_000;
    let tenders = store.tenders(MAX_REPORTED_TENDERS)?;
    let tenders_total = store.count()?;
    let (awards, awards_last_fetched) = store.award_stats()?;
    let (award_lots, award_lots_with_winner) = store.award_lot_stats()?;
    let mut sources = BTreeMap::<String, usize>::new();
    let mut cpv_codes = BTreeSet::new();
    let mut cpv_counts = BTreeMap::<String, usize>::new();
    for tender in &tenders {
        *sources.entry(tender.source.clone()).or_default() += 1;
        cpv_codes.extend(tender.cpv_codes.iter().cloned());
        for code in &tender.cpv_codes {
            *cpv_counts.entry(code.clone()).or_default() += 1;
        }
    }
    let mut top_cpv: Vec<(&String, &usize)> = cpv_counts.iter().collect();
    top_cpv.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let top_cpv: Vec<serde_json::Value> = top_cpv
        .into_iter()
        .take(10)
        .map(|(code, count)| {
            json!({
                "code": code,
                "notices": count,
                "label_nl": cpv_label_nl(code),
                "label_en": cpv_name_en(code),
            })
        })
        .collect();
    let cpv_unmapped = cpv_codes
        .iter()
        .filter(|code| cpv_label_nl(code).is_none())
        .count();
    let (opportunities, award_notices, procurements) = corpus_counts(&tenders);
    let evidence = corpus_evidence(&tenders);
    let (feasibility_stated, feasibility_hinted, feasibility_unknown) =
        (evidence.stated, evidence.hinted, evidence.unknown);
    let last_refresh = store
        .latest_refresh_run()
        .ok()
        .flatten()
        .map(|run| run.as_json());
    // The weekly award fetch is a separate run from the tender refresh, so it is
    // reported separately rather than as the store's "last run".
    let last_award_refresh = store
        .latest_award_run()
        .ok()
        .flatten()
        .map(|run| run.as_json());
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "tenders": tenders.len(),
            "tenders_total": tenders_total,
            "tenders_limited": tenders_total > tenders.len() as i64,
            "awards": awards,
            "awards_last_fetched": awards_last_fetched,
            "award_lots": award_lots,
            "award_lots_with_winner": award_lots_with_winner,
            "sources": sources,
            "opportunities": opportunities,
            "award_notices": award_notices,
            "procurements": procurements,
            "with_deadline": tenders.iter().filter(|tender| tender.deadline.is_some()).count(),
            "with_estimated_value": tenders.iter().filter(|tender| tender.estimated_value.is_some()).count(),
            "with_requirements": tenders.iter().filter(|tender| !tender.requirements.is_empty()).count(),
            "feasibility_mix": {"stated": feasibility_stated, "hinted": feasibility_hinted, "unknown": feasibility_unknown},
            "last_refresh": last_refresh,
            "last_award_refresh": last_award_refresh,
            "cpv_codes": cpv_codes.len(),
            "cpv_unmapped": cpv_unmapped,
            "top_cpv": top_cpv,
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

    /// The corpus counts must describe what can be bid on: an award notice is not
    /// an opportunity, and two publications of one procurement are one
    /// procurement — the same collapse the ranking uses.
    #[test]
    fn corpus_counts_separate_opportunities_procurements_and_award_notices() {
        let notice =
            |external_id: &str, title: &str, notice_type: &str, deadline: &str, cpv: &str| Tender {
                source: "ted".to_owned(),
                external_id: external_id.to_owned(),
                title: title.to_owned(),
                buyer: Some("Stad Gent".to_owned()),
                cpv_codes: vec![cpv.to_owned()],
                deadline: Some(deadline.to_owned()),
                notice_type: Some(notice_type.to_owned()),
                ..Tender::default()
            };
        let tenders = vec![
            notice(
                "1",
                "Elektrische installatie",
                "cn-standard",
                "2027-01-01",
                "45310000",
            ),
            notice(
                "2",
                "Elektrische installatie",
                "cn-standard",
                "2027-02-01",
                "45310000",
            ),
            notice(
                "3",
                "Schilderwerken",
                "cn-standard",
                "2027-01-01",
                "45440000",
            ),
            notice(
                "4",
                "Gegund: elektrische installatie",
                "can-standard",
                "2026-01-01",
                "45310000",
            ),
        ];
        assert_eq!(corpus_counts(&tenders), (3, 1, 2));
    }

    #[test]
    fn feasibility_mix_counts_stated_hinted_and_unknown() {
        use aanbestedingen_matcher::TenderRequirement;
        let mut stated = Tender {
            source: "ted".to_owned(),
            external_id: "s".to_owned(),
            title: "Works with criteria".to_owned(),
            deadline: Some("2027-01-01".to_owned()),
            notice_type: Some("cn-standard".to_owned()),
            requirements: vec![TenderRequirement {
                text: "turnover at least EUR 1 000 000".to_owned(),
                kind: "selection".to_owned(),
                mandatory: true,
                source_page: None,
            }],
            ..Tender::default()
        };
        stated.description = "turnover at least EUR 1 000 000".to_owned();
        let plain = Tender {
            source: "ted".to_owned(),
            external_id: "u".to_owned(),
            title: "Paper clips".to_owned(),
            deadline: Some("2027-01-01".to_owned()),
            notice_type: Some("cn-standard".to_owned()),
            ..Tender::default()
        };
        let mix = corpus_evidence(&[stated, plain]);
        assert_eq!(
            mix.stated, 1,
            "the notice with mandatory criteria is stated"
        );
        assert_eq!(
            mix.stated + mix.hinted + mix.unknown,
            2,
            "mix covers every opportunity"
        );
    }

    #[test]
    fn uses_default_database() {
        assert_eq!(
            parse_args(arguments(&[])).expect("arguments should parse"),
            Some(ReportOptions {
                database: "data/tenders-rust.duckdb".to_owned(),
            })
        );
    }

    #[test]
    fn accepts_explicit_database() {
        assert_eq!(
            parse_args(arguments(&["--database", "custom.duckdb"]))
                .expect("arguments should parse"),
            Some(ReportOptions {
                database: "custom.duckdb".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_missing_database_value() {
        let error = parse_args(arguments(&["--database"])).expect_err("value should be required");
        assert!(error.to_string().contains("--database needs a value"));
    }

    #[test]
    fn accepts_help_without_database() {
        for flag in ["--help", "-h"] {
            assert!(parse_args(arguments(&[flag]))
                .expect("help should parse")
                .is_none());
        }
    }

    #[test]
    fn rejects_unknown_and_positional_arguments() {
        assert!(parse_args(arguments(&["--unknown"])).is_err());
        assert!(parse_args(arguments(&["positional"])).is_err());
    }
}
