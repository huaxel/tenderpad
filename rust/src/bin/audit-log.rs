//! Read the working-papers audit trail: judgments, preference changes and
//! matter-lifecycle events, newest first.
//!
//! ```text
//! audit-log --database data/qc-seed.duckdb --limit 20
//! ```
//!
//! Read-only: a wrong `--database` path fails instead of creating an empty
//! database and reporting no events.

use aanbestedingen_matcher::store::TenderStore;
use std::env;

#[derive(Debug, PartialEq)]
struct AuditLogOptions {
    database: String,
    limit: i64,
}

fn parse_args<I>(args: I) -> Result<Option<AuditLogOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut limit = 50_i64;
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--limit" => {
                limit = arguments
                    .next()
                    .ok_or("--limit needs a value")?
                    .parse()
                    .map_err(|_| "--limit must be a positive number")?;
                if limit <= 0 {
                    return Err("--limit must be a positive number".into());
                }
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => {
                return Err(format!(
                    "unexpected positional argument: {argument} (use --database PATH)"
                )
                .into())
            }
        }
    }
    Ok(Some(AuditLogOptions { database, limit }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: audit-log [--database PATH] [--limit N]");
        return Ok(());
    };
    let store = TenderStore::open_read_only(&options.database)?;
    let events = store.audit_log(options.limit)?;
    println!("{}", serde_json::to_string_pretty(&events)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults() {
        let options = parse_args(Vec::<String>::new())
            .expect("arguments should parse")
            .expect("help was not requested");
        assert_eq!(options.database, "data/tenders-rust.duckdb");
        assert_eq!(options.limit, 50);
    }

    #[test]
    fn rejects_bad_limits_and_positionals() {
        for args in [
            vec!["--limit", "0"],
            vec!["--limit", "many"],
            vec!["some.db"],
        ] {
            assert!(
                parse_args(args.into_iter().map(String::from)).is_err(),
                "--limit 0, non-numeric limits and positionals must be rejected"
            );
        }
    }
}
