//! Close a matter: purge the confidential per-company tables (judgments,
//! preferences, cached analysis) while keeping public tender/award data, run
//! provenance, and the audit log itself — the log's `matter.close` row is the
//! record that the purge happened.
//!
//! ```text
//! matter-close --database data/qc-seed.duckdb --yes
//! ```
//!
//! The command refuses to run without `--yes` and refuses a missing
//! `--database`: there is no default to destroy.

use aanbestedingen_matcher::store::{operator_label, TenderStore};
use std::env;

#[derive(Debug, PartialEq)]
struct MatterCloseOptions {
    database: String,
    confirmed: bool,
}

fn parse_args<I>(args: I) -> Result<Option<MatterCloseOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut database: Option<String> = None;
    let mut confirmed = false;
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => {
                database = Some(arguments.next().ok_or("--database needs a value")?);
            }
            "--yes" => confirmed = true,
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => {
                if database.replace(argument.to_owned()).is_some() {
                    return Err("only one database path is allowed".into());
                }
            }
        }
    }
    Ok(Some(MatterCloseOptions {
        database: database.ok_or("a matter database is required: --database PATH")?,
        confirmed,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: matter-close --database PATH --yes");
        println!();
        println!("Purges feedback, preferences and cached analysis decisions,");
        println!("keeping public tender/award data, run provenance and the audit log.");
        return Ok(());
    };
    if !options.confirmed {
        return Err(
            "refusing to purge without --yes: re-run with --yes to confirm matter close".into(),
        );
    }
    let store = TenderStore::open(&options.database)?;
    let counts = store.close_matter(&operator_label())?;
    println!(
        "Matter closed: purged {} feedback row(s), {} preference row(s), {} cached decision(s); \
         audit log retains the matter.close event",
        counts.feedback, counts.preferences, counts.cached_decisions
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_without_confirmation() {
        let options = parse_args(["--database", "test.duckdb"].into_iter().map(String::from))
            .expect("arguments should parse")
            .expect("help was not requested");
        assert!(!options.confirmed);
    }

    #[test]
    fn refuses_without_a_database() {
        assert!(parse_args(["--yes"].into_iter().map(String::from)).is_err());
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(
            ["--database", "x", "--yes", "--force"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
    }
}
