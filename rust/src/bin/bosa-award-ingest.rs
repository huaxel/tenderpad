use aanbestedingen_matcher::bosa_award_history::collect_bosa_award_history;
use aanbestedingen_matcher::store::TenderStore;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
struct Options {
    database: PathBuf,
    source_databases: Vec<PathBuf>,
}

fn parse_args<I>(args: I) -> Result<Option<Options>, Box<dyn Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut database = None;
    let mut source_databases = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--database" => {
                if database.is_some() {
                    return Err("--database may be supplied only once".into());
                }
                database = Some(PathBuf::from(args.next().ok_or("--database needs a path")?))
            }
            "--source-db" => source_databases.push(PathBuf::from(
                args.next().ok_or("--source-db needs a path")?,
            )),
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }
    let database = database.ok_or("--database is required")?;
    if source_databases.is_empty() {
        return Err("at least one --source-db is required".into());
    }
    Ok(Some(Options {
        database,
        source_databases,
    }))
}

fn resolved_path(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Ok(canonical);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file = path.file_name().ok_or("database path has no file name")?;
    Ok(fs::canonicalize(parent)?.join(file))
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: bosa-award-ingest --database PATH --source-db BOSA.duckdb [--source-db MORE.duckdb ...]");
        println!("Reads Production BOSA snapshots read-only and syncs company CPV history.");
        println!("Only selected, contract-linked Belgian enterprises are included; BOSA amounts are excluded from ranking.");
        return Ok(());
    };

    let destination = resolved_path(&options.database)?;
    for source in &options.source_databases {
        if resolved_path(source)? == destination {
            return Err("destination database must differ from every BOSA source database".into());
        }
    }

    // Parse and validate all source snapshots before opening the destination for
    // write. Duplicate BOSA snapshots must agree on their award evidence.
    let batch = collect_bosa_award_history(&options.source_databases)?;
    if batch.report.source_notices == 0 {
        return Err("no BOSA award evidence found in the supplied source databases".into());
    }
    let store = TenderStore::open(&options.database)?;
    let written = store.replace_bosa_award_history(&batch.publications, &batch.records)?;
    let bidder_rows = store.replace_bosa_lot_bidders(&batch.publications, &batch.bidders)?;
    let not_awarded = batch
        .bidders
        .iter()
        .filter(|bidder| bidder.outcome == "not-awarded")
        .count();
    println!(
        "BOSA lot tenderers synced: {bidder_rows} rows ({not_awarded} not awarded) on lots with a settled contract."
    );

    println!(
        "BOSA award history synced: {} notices, {} selected lots, {} company-notice records across {} companies.",
        batch.report.source_notices,
        batch.report.selected_lots,
        written,
        batch.records.iter().map(|(enterprise, _)| enterprise).collect::<std::collections::BTreeSet<_>>().len()
    );
    println!(
        "Skipped: {} lots without CPV, {} without contract-linked suppliers, {} with contract evidence but no usable supplier identity, {} supplier participations with non-Belgian/invalid IDs; {} joint-participation records.",
        batch.report.skipped_lots_without_cpv,
        batch.report.skipped_lots_without_contract_link,
        batch.report.skipped_lots_without_usable_supplier_identity,
        batch.report.skipped_non_belgian_or_invalid_ids,
        batch.report.joint_company_lot_participations
    );
    println!(
        "Compatible duplicate snapshots reconciled: {}.",
        batch.report.merged_compatible_snapshots
    );
    println!("BOSA monetary amounts were not imported; TED/manual award history was preserved.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn requires_destination_and_at_least_one_source() {
        assert!(parse_args(args(&[])).is_err());
        assert!(parse_args(args(&["--database", "out.duckdb"])).is_err());
        assert!(parse_args(args(&[
            "--database",
            "first.duckdb",
            "--database",
            "second.duckdb",
            "--source-db",
            "source.duckdb",
        ]))
        .is_err());
        let options = parse_args(args(&[
            "--database",
            "out.duckdb",
            "--source-db",
            "september.duckdb",
            "--source-db",
            "october.duckdb",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(options.database, PathBuf::from("out.duckdb"));
        assert_eq!(
            options.source_databases,
            [
                PathBuf::from("september.duckdb"),
                PathBuf::from("october.duckdb")
            ]
        );
        assert!(parse_args(args(&["--help"])).unwrap().is_none());
    }
}
