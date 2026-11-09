//! Import a Brussels public contract inventory (XLSX) into the store, or report
//! what the store holds for one company.
//!
//! The inventories are published per administration and per year on data.gov.be
//! / datastore.brussels under CC0; see `rust/src/bxl_inventory.rs` for what they
//! contain and what they are not (they record awarded contracts, not open
//! tenders). Import:

//! ```text
//! inventory-ingest ./downloads/0207365709_PP_2024.xlsx --database data/tenders-rust.duckdb
//! ```
//!
//! Report the contracts recorded for one supplier KBO:
//!
//! ```text
//! inventory-ingest --awardee 0556965585 --database data/tenders-rust.duckdb
//! ```
//!
//! Imports are idempotent: rows are keyed by a hash of their content, so
//! re-importing the same file updates rows instead of duplicating them.

use aanbestedingen_matcher::bxl_inventory::parse_inventory;
use aanbestedingen_matcher::store::TenderStore;
use std::env;
use std::path::Path;

#[derive(Debug, PartialEq, Default)]
struct Options {
    path: Option<String>,
    awardee: Option<String>,
    database: String,
}

fn parse_args<I>(args: I) -> Result<Option<Options>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut options = Options {
        database: "data/tenders-rust.duckdb".to_owned(),
        ..Options::default()
    };
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => {
                options.database = arguments.next().ok_or("--database needs a value")?
            }
            "--awardee" => {
                options.awardee = Some(arguments.next().ok_or("--awardee needs a value")?)
            }
            "--help" | "-h" => return Ok(None),
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}").into())
            }
            path => {
                if options.path.replace(path.to_owned()).is_some() {
                    return Err("only one inventory file is allowed".into());
                }
            }
        }
    }
    if options.path.is_some() && options.awardee.is_some() {
        return Err("import one file or report one awardee, not both".into());
    }
    if options.path.is_none() && options.awardee.is_none() {
        return Err("an inventory file or --awardee KBO is required".into());
    }
    Ok(Some(options))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: inventory-ingest FILE [--database PATH]");
        println!("       inventory-ingest --awardee KBO [--database PATH]");
        println!();
        println!("FILE is a Brussels contract inventory XLSX (CC0, data.gov.be).");
        return Ok(());
    };

    if let Some(kbo) = options.awardee {
        let store = TenderStore::open_read_only(&options.database)?;
        // One line per engagement, not per accounting row: an engagement is
        // repeated on every liquidation, so listing rows shows one contract
        // many times (measured: a single €58,871.98 engagement appears on eight
        // rows). Summing the accounting rows would count it eight times too.
        // Ask for one more than we print, so a truncated list says so instead of
        // looking like the whole history.
        const PRINT_LIMIT: usize = 200;
        let mut engagements = store.inventory_engagements_for_awardee(&kbo, PRINT_LIMIT + 1)?;
        let truncated = engagements.len() > PRINT_LIMIT;
        engagements.truncate(PRINT_LIMIT);
        let accounting_rows: i64 = engagements
            .iter()
            .map(|engagement| engagement.accounting_rows)
            .sum();
        println!(
            "{} engagement(s) recorded for {kbo} ({} accounting rows{})",
            engagements.len(),
            accounting_rows,
            if truncated {
                "; showing the most recent 200, more were read back"
            } else {
                ""
            }
        );
        for engagement in engagements {
            let amount = engagement
                .engagement_amount
                .map(|value| format!("EUR {value}"))
                .unwrap_or_else(|| "amount unknown".to_owned());
            let repeated = if engagement.accounting_rows > 1 {
                format!(" [{} accounting rows]", engagement.accounting_rows)
            } else {
                String::new()
            };
            println!(
                "  {} | {amount} | {} | {}{repeated}",
                engagement.engagement_date.as_deref().unwrap_or("undated"),
                engagement.buyer_name.as_deref().unwrap_or("unknown buyer"),
                engagement.object.as_deref().unwrap_or("no object recorded"),
            );
        }
        return Ok(());
    }

    let path = options.path.expect("checked above");
    println!("Reading {}", Path::new(&path).display());
    let awards = parse_inventory(&path)?;
    let source_file = Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&path)
        .to_owned();
    let store = TenderStore::open(&options.database)?;
    let stored = store.upsert_inventory_awards(&source_file, &awards)?;
    let awardees = awards
        .iter()
        .filter_map(|award| award.awardee_identifier.clone())
        .collect::<std::collections::HashSet<_>>()
        .len();
    let with_amount = awards
        .iter()
        .filter(|award| award.engagement_amount.is_some())
        .count();
    println!(
        "Parsed {} row(s): {} with an amount, {} distinct awardee KBO numbers; stored {stored} \
         into {}",
        awards.len(),
        with_amount,
        awardees,
        options.database
    );
    println!(
        "The database now holds {} inventory row(s)",
        store.inventory_award_count(None)?
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
    fn accepts_a_file_with_a_database() {
        let options = parse_args(arguments(&["file.xlsx", "--database", "test.duckdb"]))
            .expect("arguments should parse")
            .expect("file mode");
        assert_eq!(options.path.as_deref(), Some("file.xlsx"));
        assert_eq!(options.database, "test.duckdb");
        assert_eq!(options.awardee, None);
    }

    #[test]
    fn accepts_an_awardee_lookup() {
        let options = parse_args(arguments(&["--awardee", "0556965585"]))
            .expect("arguments should parse")
            .expect("lookup mode");
        assert_eq!(options.awardee.as_deref(), Some("0556965585"));
        assert_eq!(options.database, "data/tenders-rust.duckdb");
    }

    #[test]
    fn rejects_missing_or_conflicting_targets() {
        assert!(parse_args(arguments(&[])).is_err());
        assert!(parse_args(arguments(&["file.xlsx", "--awardee", "0556965585"])).is_err());
        assert!(parse_args(arguments(&["one.xlsx", "two.xlsx"])).is_err());
        assert!(parse_args(arguments(&["--unknown"])).is_err());
        assert!(parse_args(arguments(&["--help"]))
            .expect("help should parse")
            .is_none());
    }
}
