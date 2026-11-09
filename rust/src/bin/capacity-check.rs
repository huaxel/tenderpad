//! Flag awarded lots that are large relative to the winner's turnover.
//!
//! Reads `award_lots` joined to the newest `nbb_financials` row per winner
//! (see `TenderStore::capacity_outliers`). Populate the two sides with
//! `award-ingest` (winner-side history) and `nbb-import` (Consult CSV/XBRL)
//! first; this command only reports.

use aanbestedingen_matcher::store::{filing_age_days, TenderStore};
use chrono::Utc;
use std::env;

#[derive(Debug, PartialEq)]
struct CapacityCheckOptions {
    database: String,
    min_ratio: f64,
}

fn parse_args<I>(args: I) -> Result<Option<CapacityCheckOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut min_ratio = 1.0;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--min-ratio" => {
                let value: f64 = arguments
                    .next()
                    .ok_or("--min-ratio needs a value")?
                    .parse()?;
                if value.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                    return Err("--min-ratio must be positive".into());
                }
                min_ratio = value;
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => return Err(format!("unexpected argument: {argument}").into()),
        }
    }
    Ok(Some(CapacityCheckOptions {
        database,
        min_ratio,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = match parse_args(env::args().skip(1))? {
        Some(options) => options,
        None => {
            eprintln!(
                "usage: capacity-check [--database PATH] [--min-ratio RATIO]\n\nFlags lots worth >= RATIO of the winner's latest turnover (default 1.0)."
            );
            return Ok(());
        }
    };
    let store = TenderStore::open(&options.database)?;
    let coverage = store.capacity_coverage()?;
    let flags = store.capacity_outliers(options.min_ratio)?;
    if flags.is_empty() {
        println!(
            "no lots reach {:.2}x turnover in {}",
            options.min_ratio, options.database
        );
        // An empty list is not a clean bill of health: say how much of the
        // check was even possible.
        println!("capacity coverage: {}", coverage.describe());
        return Ok(());
    }
    // State the age of the filing behind each figure: the report exists to
    // question a company's capacity, so it should not read as though the
    // evidence were current when it may be years old.
    let today = Utc::now().date_naive();
    for flag in &flags {
        let age = match filing_age_days(flag.period_end.as_deref(), today) {
            Some(days) => format!("{days} days old"),
            None => "age unknown".to_owned(),
        };
        println!(
            "{:.2}x  {} {}  {} FY{} turnover={} staff={:?} [{}] filing {age}",
            flag.value_to_turnover,
            flag.publication_number,
            flag.lot_id,
            flag.winner_name.as_deref().unwrap_or("?"),
            flag.fiscal_year,
            flag.turnover,
            flag.staff_fte,
            flag.winner_kbo,
        );
    }
    println!(
        "{} lot(s) at or above {:.2}x turnover",
        flags.len(),
        options.min_ratio
    );
    println!("capacity coverage: {}", coverage.describe());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_flag_awards_above_full_turnover() {
        let options = parse_args(Vec::<String>::new())
            .expect("empty args parse")
            .expect("empty args are valid");
        assert_eq!(
            options,
            CapacityCheckOptions {
                database: "data/tenders-rust.duckdb".to_owned(),
                min_ratio: 1.0,
            }
        );
    }

    #[test]
    fn explicit_threshold_and_database_parse() {
        let options = parse_args(
            ["--database", "/tmp/x.duckdb", "--min-ratio", "0.25"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("args parse")
        .expect("args valid");
        assert_eq!(options.database, "/tmp/x.duckdb");
        assert!((options.min_ratio - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_non_positive_threshold() {
        assert!(parse_args(["--min-ratio", "0"].into_iter().map(str::to_owned)).is_err());
        assert!(parse_args(["--min-ratio", "-1"].into_iter().map(str::to_owned)).is_err());
        assert!(parse_args(["--bogus"].into_iter().map(str::to_owned)).is_err());
        assert!(parse_args(["stray"].into_iter().map(str::to_owned)).is_err());
    }
}
