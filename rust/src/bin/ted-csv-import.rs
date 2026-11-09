//! Import one TED CAN archive year into `ted_awards_history`.
//!
//! ```bash
//! just ted-csv-import data/ted-csv/export_CAN_2021.csv 2021
//! ```
//!
//! Belgian-buyer rows resolve against the KBO snapshot; the summary reports
//! the join mix so a thin year is visible before training ever reads it.

use aanbestedingen_matcher::store::TenderStore;
use aanbestedingen_matcher::ted_csv::{denomination_maps, read_belgian_rows, resolve_rows};
use std::env;

#[derive(Debug, PartialEq)]
struct ImportOptions {
    csv: String,
    year: i64,
    database: String,
    kbo_database: String,
}

fn parse_args<I>(args: I) -> Result<Option<ImportOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut csv = None;
    let mut year = None;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => {
                if csv.is_none() {
                    csv = Some(argument.to_owned());
                } else if year.is_none() {
                    year = Some(
                        argument
                            .parse::<i64>()
                            .map_err(|_| format!("year must be an integer: {argument}"))?,
                    );
                } else {
                    return Err("only CSV PATH and YEAR positionals are allowed".into());
                }
            }
        }
    }
    Ok(Some(ImportOptions {
        csv: csv.ok_or("CSV PATH is required")?,
        year: year.ok_or("YEAR is required")?,
        database,
        kbo_database,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: ted-csv-import CSV YEAR [--database PATH] [--kbo-database PATH]");
        return Ok(());
    };
    let ImportOptions {
        csv,
        year,
        database,
        kbo_database,
    } = options;
    let scratch = duckdb::Connection::open_in_memory()?;
    let rows = read_belgian_rows(&scratch, &csv)?;
    println!("Belgian-buyer rows in {csv}: {}", rows.len());
    let kbo = duckdb::Connection::open_with_flags(
        kbo_database.as_str(),
        duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
    )?;
    let (exact, normalized) = denomination_maps(&kbo)?;
    println!("KBO map: {} normalized keys", normalized.len());
    let resolved = resolve_rows(year, rows, &exact, &normalized);
    let mut tiers = std::collections::BTreeMap::<&str, usize>::new();
    let mut valued = 0;
    let mut ambiguous = 0;
    for award in &resolved {
        *tiers.entry(award.join_confidence).or_default() += 1;
        valued += award.award_value.is_some() as usize;
        ambiguous += award.ambiguous as usize;
    }
    let store = TenderStore::open(database)?;
    let stored = store.replace_ted_awards_history(year, &resolved)?;
    println!("Stored {stored} history rows for {year}: {tiers:?}; {valued} valued, {ambiguous} ambiguous");
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
    fn accepts_csv_and_year_positionals() {
        let options = parse_args(arguments(&["a.csv", "2021"]))
            .expect("args should parse")
            .expect("options should exist");
        assert_eq!(options.csv, "a.csv");
        assert_eq!(options.year, 2021);
    }

    #[test]
    fn rejects_missing_year() {
        assert!(parse_args(arguments(&["a.csv"])).is_err());
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(arguments(&["a.csv", "2021", "--bogus"])).is_err());
    }
}
