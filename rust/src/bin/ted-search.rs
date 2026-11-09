use aanbestedingen_matcher::ted::{
    SearchScope, SearchWindow, TedClient, DEFAULT_LOOKBACK_DAYS, DEFAULT_QUERY,
    MAX_TED_SEARCH_RESULTS,
};
use serde_json::Value;
use std::env;

#[derive(Debug, PartialEq)]
struct TedSearchOptions {
    query: String,
    limit: usize,
    scope: SearchScope,
    days: i64,
}

fn parse_args<I>(args: I) -> Result<Option<TedSearchOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut query = DEFAULT_QUERY.to_owned();
    let mut limit = 10_usize;
    let mut scope = SearchScope::default();
    let mut days = DEFAULT_LOOKBACK_DAYS;
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--query" => query = arguments.next().ok_or("--query needs a value")?,
            "--limit" => {
                let value: usize = arguments.next().ok_or("--limit needs a value")?.parse()?;
                if value == 0 || value > MAX_TED_SEARCH_RESULTS {
                    return Err(format!(
                        "TED search limit must be between 1 and {MAX_TED_SEARCH_RESULTS}"
                    )
                    .into());
                }
                limit = value;
            }
            "--scope" => {
                scope = SearchScope::parse(&arguments.next().ok_or("--scope needs a value")?)?
            }
            "--days" => {
                let value: i64 = arguments.next().ok_or("--days needs a value")?.parse()?;
                if value < 0 {
                    return Err("--days must not be negative; use 0 for no date bound".into());
                }
                days = value;
            }
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }
    Ok(Some(TedSearchOptions {
        query,
        limit,
        scope,
        days,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: ted-search [--query QUERY] [--limit N] [--scope latest|active|all] [--days N]"
        );
        return Ok(());
    };
    let TedSearchOptions {
        query,
        limit,
        scope,
        days,
    } = options;
    let window = SearchWindow::recent(days, chrono::Local::now().date_naive()).with_scope(scope);
    let tenders = TedClient::default().search_window(&query, limit, &window)?;
    let mut output = Vec::with_capacity(tenders.len());
    for tender in tenders {
        let mut value = serde_json::to_value(tender)?;
        if let Value::Object(object) = &mut value {
            object.remove("raw");
        }
        output.push(value);
    }
    println!("{}", serde_json::to_string_pretty(&output)?);
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
    fn uses_default_options() {
        assert_eq!(
            parse_args(arguments(&[])).expect("arguments should parse"),
            Some(TedSearchOptions {
                query: DEFAULT_QUERY.to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: DEFAULT_LOOKBACK_DAYS,
            })
        );
    }

    #[test]
    fn accepts_explicit_query_and_limit() {
        assert_eq!(
            parse_args(arguments(&[
                "--query",
                "buyer-country=BEL",
                "--limit",
                "25",
                "--scope",
                "latest",
                "--days",
                "7",
            ]))
            .expect("arguments should parse"),
            Some(TedSearchOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 25,
                scope: SearchScope::Latest,
                days: 7,
            })
        );
    }

    #[test]
    fn accepts_help_without_options() {
        for flag in ["--help", "-h"] {
            assert!(parse_args(arguments(&[flag]))
                .expect("help should parse")
                .is_none());
        }
    }

    #[test]
    fn rejects_missing_values() {
        assert!(parse_args(arguments(&["--query"])).is_err());
        assert!(parse_args(arguments(&["--limit"])).is_err());
    }

    #[test]
    fn rejects_invalid_or_out_of_range_limits() {
        assert!(parse_args(arguments(&["--limit", "abc"])).is_err());
        assert!(parse_args(arguments(&["--limit", "0"])).is_err());
        assert!(parse_args(arguments(&[
            "--limit",
            &(MAX_TED_SEARCH_RESULTS + 1).to_string(),
        ]))
        .is_err());
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(arguments(&["--unknown"])).is_err());
        assert!(parse_args(arguments(&["positional"])).is_err());
    }
}
