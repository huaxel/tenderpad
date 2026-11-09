use aanbestedingen_matcher::bosa::{BosaClient, BosaConfig, Environment, PublicationSearch};
use chrono::NaiveDate;
use std::env;
use std::error::Error;

#[derive(Debug, PartialEq)]
struct Options {
    environment: Environment,
    from: Option<String>,
    to: Option<String>,
    terms: Option<String>,
    page: u32,
    limit: u32,
}

fn parse_args<I>(args: I) -> Result<Option<Options>, Box<dyn Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut options = Options {
        environment: Environment::Int,
        from: None,
        to: None,
        terms: None,
        page: 1,
        limit: 10,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--env" => {
                options.environment =
                    Environment::parse(&args.next().ok_or("--env needs int or prod")?)?
            }
            "--from" => options.from = Some(date_arg("--from", args.next())?),
            "--to" => options.to = Some(date_arg("--to", args.next())?),
            "--terms" => options.terms = Some(args.next().ok_or("--terms needs a value")?),
            "--page" => {
                options.page = args.next().ok_or("--page needs a value")?.parse()?;
                if options.page == 0 {
                    return Err("--page must be >= 1".into());
                }
            }
            "--limit" => {
                options.limit = args.next().ok_or("--limit needs a value")?.parse()?;
                if !(1..=100).contains(&options.limit) {
                    return Err("--limit must be between 1 and 100".into());
                }
            }
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }
    if let (Some(from), Some(to)) = (&options.from, &options.to) {
        if from > to {
            return Err("--from must not be after --to".into());
        }
    }
    Ok(Some(options))
}

fn date_arg(name: &str, value: Option<String>) -> Result<String, Box<dyn Error>> {
    let value = value.ok_or_else(|| format!("{name} needs a value"))?;
    NaiveDate::parse_from_str(&value, "%Y-%m-%d")
        .map_err(|_| format!("{name} must use YYYY-MM-DD"))?;
    Ok(value)
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: bosa-search [--env int|prod] [--from YYYY-MM-DD] [--to YYYY-MM-DD] [--terms TEXT] [--page N] [--limit 1..100]");
        println!("Requires BOSA_INT_* credentials (default) or BOSA_PR_* with --env prod.");
        return Ok(());
    };
    let search = PublicationSearch {
        terms: options.terms,
        publication_date_from: options.from,
        publication_date_to: options.to,
        with_active_submission_deadline: Some(true),
        page: Some(options.page),
        page_size: Some(options.limit),
        ..PublicationSearch::default()
    };
    let result = BosaClient::new(BosaConfig::for_environment(options.environment)?)
        .search_publications(&search)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
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
    fn defaults_to_first_small_page() {
        assert_eq!(
            parse_args(args(&[])).unwrap(),
            Some(Options {
                environment: Environment::Int,
                from: None,
                to: None,
                terms: None,
                page: 1,
                limit: 10,
            })
        );
    }

    #[test]
    fn accepts_date_window_terms_and_page_size() {
        assert_eq!(
            parse_args(args(&[
                "--from",
                "2026-10-01",
                "--to",
                "2026-10-30",
                "--terms",
                "roads",
                "--page",
                "2",
                "--limit",
                "25"
            ]))
            .unwrap(),
            Some(Options {
                environment: Environment::Int,
                from: Some("2026-10-01".into()),
                to: Some("2026-10-30".into()),
                terms: Some("roads".into()),
                page: 2,
                limit: 25,
            })
        );
    }

    #[test]
    fn rejects_bad_dates_ranges_and_limits() {
        assert!(parse_args(args(&["--from", "not-a-date"])).is_err());
        assert!(parse_args(args(&["--from", "2026-10-30", "--to", "2026-10-01"])).is_err());
        assert!(parse_args(args(&["--limit", "101"])).is_err());
        assert!(parse_args(args(&["--page", "0"])).is_err());
        assert!(parse_args(args(&["--env", "other"])).is_err());
        assert_eq!(
            parse_args(args(&["--env", "prod"]))
                .unwrap()
                .unwrap()
                .environment,
            Environment::Prod
        );
    }
}
