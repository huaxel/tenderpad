use aanbestedingen_matcher::kbo::{normalize_enterprise_number, KboRepository};
use serde_json::to_string_pretty;
use std::env;

#[derive(Debug, PartialEq)]
struct KboLookupOptions {
    enterprise_number: String,
    database: String,
    live: bool,
    live_only: bool,
}

fn parse_args<I>(args: I) -> Result<Option<KboLookupOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut raw_enterprise_number = None;
    let mut database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut live = false;
    let mut live_only = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--live" => live = true,
            "--live-only" => {
                live = true;
                live_only = true;
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => {
                if raw_enterprise_number.replace(argument.to_owned()).is_some() {
                    return Err("only one enterprise number is allowed".into());
                }
            }
        }
    }
    let enterprise_number = normalize_enterprise_number(
        &raw_enterprise_number.ok_or("enterprise number is required")?,
    )?;
    Ok(Some(KboLookupOptions {
        enterprise_number,
        database,
        live,
        live_only,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: kbo-lookup ENTERPRISE_NUMBER [--database PATH] [--live|--live-only]");
        println!("  --live queries the KBO Public Search webservice (needs");
        println!("  KBO_PUB_USER/KBO_PUB_PASSWORD) and falls back to the snapshot");
        println!("  database when the service cannot be reached.");
        println!("  --live-only fails on service errors; never reads the snapshot.");
        return Ok(());
    };
    let KboLookupOptions {
        enterprise_number,
        database,
        live,
        live_only,
    } = options;
    if live {
        match live_lookup(&enterprise_number) {
            Ok(()) => return Ok(()),
            Err(error) if live_only => return Err(error),
            Err(error) => eprintln!("live lookup failed ({error}); falling back to snapshot"),
        }
    }
    let repository = KboRepository::open(database)?;
    let profile = repository
        .lookup(&enterprise_number)?
        .ok_or("company not found")?;
    println!("{}", to_string_pretty(&profile)?);
    Ok(())
}

/// Query the webservice and print the profile with its remaining quota.
/// Any failure (missing env, auth, network, unknown number) is an `Err` so
/// the caller can fall back — except an unknown number, which is a clean
/// absence and prints nothing but succeeds.
fn live_lookup(enterprise_number: &str) -> Result<(), Box<dyn std::error::Error>> {
    use aanbestedingen_matcher::kbo_live::{LiveClient, LiveCredentials};
    let client = LiveClient::new(LiveCredentials::from_env()?);
    let (profile, balance) = client.read_enterprise(enterprise_number)?;
    match profile {
        Some(profile) => println!("{}", to_string_pretty(&profile)?),
        None => println!("company not found in live KBO"),
    }
    if let Some(left) = balance {
        eprintln!("{left} KBO request(s) left today");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_before_positional_enterprise_number() {
        let options = parse_args(
            ["--database", "test.duckdb", "0404484060"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.database, "test.duckdb");
    }

    #[test]
    fn parses_positional_enterprise_number_before_flags() {
        let options = parse_args(
            ["0404484060", "--database", "test.duckdb"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.database, "test.duckdb");
    }

    #[test]
    fn rejects_duplicate_enterprise_numbers() {
        assert!(parse_args(["0404484060", "0404484060"].into_iter().map(String::from),).is_err());
    }

    #[test]
    fn accepts_help_without_enterprise_number() {
        for flag in ["--help", "-h"] {
            assert!(parse_args([flag].into_iter().map(String::from))
                .expect("help should parse")
                .is_none());
        }
    }

    #[test]
    fn rejects_missing_values_and_unknown_arguments() {
        assert!(parse_args(["0404484060", "--database"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["0404484060", "--unknown"].into_iter().map(String::from),).is_err());
    }

    #[test]
    fn live_only_is_explicit_and_implies_live() {
        let options = parse_args(["0202239951", "--live-only"].into_iter().map(String::from))
            .unwrap()
            .unwrap();
        assert!(options.live);
        assert!(options.live_only);
        let options = parse_args(["0202239951", "--live"].into_iter().map(String::from))
            .unwrap()
            .unwrap();
        assert!(!options.live_only);
    }

    #[test]
    fn requires_enterprise_number() {
        assert!(parse_args(std::iter::empty::<String>()).is_err());
    }
}
