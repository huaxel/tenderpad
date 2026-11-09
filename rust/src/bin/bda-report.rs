use aanbestedingen_matcher::bda::load_bda_documents;
use aanbestedingen_matcher::cross_source::find_probable_duplicates;
use aanbestedingen_matcher::store::TenderStore;
use serde_json::json;
use std::env;

#[derive(Debug, PartialEq)]
struct ReportOptions {
    path: String,
    /// When given, the parsed notices are compared with what the store already
    /// holds from other sources: the same procurement can arrive from TED and from
    /// the Belgian e-Notification import.
    database: Option<String>,
}

fn parse_args<I>(args: I) -> Result<Option<ReportOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut path = None;
    let mut database = None;
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => return Ok(None),
            "--database" => database = Some(arguments.next().ok_or("--database needs a value")?),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => {
                if path.replace(argument.to_owned()).is_some() {
                    return Err("only one BDA input path is allowed".into());
                }
            }
        }
    }
    Ok(Some(ReportOptions {
        path: path.ok_or("BDA XML, PDF, ZIP or directory path is required")?,
        database,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: bda-report PATH [--database PATH]");
        println!();
        println!("With --database, reports which parsed notices look like a procurement");
        println!("the store already holds from another source (TED), with the evidence.");
        return Ok(());
    };
    let (tenders, skipped, awards) = load_bda_documents(&options.path)?;
    let cross_source = match options.database.as_deref() {
        Some(database) => {
            let store = TenderStore::open_read_only(database)?;
            let stored = store.tenders(100_000)?;
            let mut rows = Vec::new();
            for tender in &tenders {
                for candidate in find_probable_duplicates(tender, &stored) {
                    rows.push(json!({
                        "incoming": tender.external_id,
                        "incoming_title": tender.title,
                        "candidate_source": candidate.source,
                        "candidate_external_id": candidate.external_id,
                        "title_similarity": candidate.title_similarity,
                        "same_cpvs": candidate.same_cpvs,
                        "same_deadline": candidate.same_deadline,
                        "deadline_delta_days": candidate.deadline_delta_days,
                    }));
                }
            }
            Some(rows)
        }
        None => None,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "parsed_tenders": tenders.len(),
            "skipped_documents": skipped
                .iter()
                .map(|(name, error)| json!({"name": name, "error": error}))
                .collect::<Vec<_>>(),
            "with_deadline": tenders.iter().filter(|tender| tender.deadline.is_some()).count(),
            "with_cpv": tenders.iter().filter(|tender| !tender.cpv_codes.is_empty()).count(),
            "with_estimated_value": tenders.iter().filter(|tender| tender.estimated_value.is_some()).count(),
            "with_requirements": tenders.iter().filter(|tender| !tender.requirements.is_empty()).count(),
            "titles": tenders.iter().take(5).map(|tender| tender.title.clone()).collect::<Vec<_>>(),
            "bda_awards": awards,
            "cross_source_candidates": cross_source,
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
    fn parses_input_path() {
        assert_eq!(
            parse_args(arguments(&["fixtures/notices.zip"])).expect("path should parse"),
            Some(ReportOptions {
                path: "fixtures/notices.zip".to_owned(),
                database: None,
            })
        );
    }

    #[test]
    fn accepts_a_database_to_compare_against() {
        assert_eq!(
            parse_args(arguments(&[
                "fixtures/notices.zip",
                "--database",
                "store.duckdb"
            ]))
            .expect("arguments should parse"),
            Some(ReportOptions {
                path: "fixtures/notices.zip".to_owned(),
                database: Some("store.duckdb".to_owned()),
            })
        );
        assert!(parse_args(arguments(&["fixtures.zip", "--database"])).is_err());
    }

    #[test]
    fn accepts_help_without_path() {
        assert!(parse_args(arguments(&["--help"]))
            .expect("help should parse")
            .is_none());
    }

    #[test]
    fn requires_input_path() {
        assert!(parse_args(arguments(&[])).is_err());
    }

    #[test]
    fn rejects_extra_positional_arguments() {
        assert!(parse_args(arguments(&["one.xml", "two.xml"])).is_err());
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(arguments(&["input.xml", "--verbose"])).is_err());
    }
}
