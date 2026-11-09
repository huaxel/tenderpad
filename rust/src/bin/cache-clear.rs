use aanbestedingen_matcher::store::TenderStore;
use std::env;

#[derive(Debug)]
struct Options {
    database: String,
    confirmed: bool,
}

fn parse_args<I>(mut arguments: I) -> Result<Option<Options>, Box<dyn std::error::Error>>
where
    I: Iterator<Item = String>,
{
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut confirmed = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--yes" => confirmed = true,
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }

    if !confirmed {
        return Err("refusing to clear analysis cache without --yes".into());
    }

    Ok(Some(Options {
        database,
        confirmed,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = match parse_args(env::args().skip(1))? {
        Some(options) => options,
        None => {
            println!("Usage: cache-clear [OPTIONS]");
            println!("  --database PATH   DuckDB database path");
            println!("  --yes             confirm deletion of all persisted analysis decisions");
            return Ok(());
        }
    };

    debug_assert!(options.confirmed);
    let store = TenderStore::open(options.database)?;
    let deleted = store.clear_analysis_cache()?;
    println!("Cleared {deleted} persisted analysis decisions");
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
    fn requires_explicit_confirmation() {
        let error = parse_args(arguments(&[])).expect_err("confirmation should be required");
        assert!(error.to_string().contains("--yes"));
    }

    #[test]
    fn accepts_database_and_confirmation() {
        let options = parse_args(arguments(&["--database", "tmp.duckdb", "--yes"]))
            .expect("valid options should parse")
            .expect("non-help options should return configuration");
        assert_eq!(options.database, "tmp.duckdb");
        assert!(options.confirmed);
    }

    #[test]
    fn help_does_not_require_confirmation() {
        assert!(parse_args(arguments(&["--help"]))
            .expect("help should parse")
            .is_none());
    }

    #[test]
    fn rejects_missing_database_value_and_unknown_arguments() {
        assert!(parse_args(arguments(&["--database", "--yes"])).is_err());
        assert!(parse_args(arguments(&["--unknown", "--yes"])).is_err());
    }
}
