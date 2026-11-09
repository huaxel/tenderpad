use aanbestedingen_matcher::kbo::{normalize_enterprise_number, KboRepository};
use aanbestedingen_matcher::recommendations::rank;
use aanbestedingen_matcher::store::TenderStore;
use serde_json::json;
use std::env;
use std::hint::black_box;
use std::time::Instant;

const MAX_BENCHMARK_ITERATIONS: usize = 100_000;

#[derive(Debug, PartialEq)]
struct BenchmarkOptions {
    enterprise_number: String,
    database: String,
    kbo_database: String,
    iterations: usize,
}

fn parse_args<I>(args: I) -> Result<Option<BenchmarkOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut raw_enterprise_number = None;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut iterations = 100_usize;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?
            }
            "--iterations" => {
                let value: usize = arguments
                    .next()
                    .ok_or("--iterations needs a value")?
                    .parse()?;
                if value == 0 || value > MAX_BENCHMARK_ITERATIONS {
                    return Err(format!(
                        "--iterations must be between 1 and {MAX_BENCHMARK_ITERATIONS}"
                    )
                    .into());
                }
                iterations = value;
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
    Ok(Some(BenchmarkOptions {
        enterprise_number,
        database,
        kbo_database,
        iterations,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: benchmark ENTERPRISE_NUMBER [--database PATH] [--kbo-database PATH] [--iterations N]");
        return Ok(());
    };
    let BenchmarkOptions {
        enterprise_number,
        database,
        kbo_database,
        iterations,
    } = options;

    let repository = KboRepository::open(kbo_database)?;
    let company = repository
        .lookup(&enterprise_number)?
        .ok_or("company not found")?;
    let store = TenderStore::open(&database)?;
    let tenders = store.tenders(10_000)?;
    if tenders.is_empty() {
        // An empty store made this print 5.9 million iterations per second, which
        // measures nothing and reads like a very fast product.
        return Err(format!(
            "'{database}' holds no tenders; pass a seeded store, e.g. \
             just rust-benchmark {enterprise_number} <populated.duckdb>"
        )
        .into());
    }
    let preferences = store.preferences_for(&company.enterprise_number)?;
    let history = store.awards_for(&company.enterprise_number)?;
    let feedback = store.feedback_for(&company.enterprise_number)?;

    let started = Instant::now();
    let mut recommendation_count = 0;
    for _ in 0..iterations {
        let results = rank(
            black_box(&company),
            black_box(&tenders),
            black_box(&history),
            black_box(&preferences),
            black_box(&feedback),
            tenders.len(),
        );
        recommendation_count += black_box(results.len());
    }
    let elapsed = started.elapsed();
    let seconds = elapsed.as_secs_f64();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "tenders": tenders.len(),
            "iterations": iterations,
            "recommendations_evaluated": recommendation_count,
            "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
            "iterations_per_second": if seconds > 0.0 { iterations as f64 / seconds } else { 0.0 },
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_before_positional_enterprise_number() {
        let options = parse_args(
            [
                "--database",
                "test.duckdb",
                "--kbo-database",
                "kbo.duckdb",
                "--iterations",
                "7",
                "0404484060",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.database, "test.duckdb");
        assert_eq!(options.kbo_database, "kbo.duckdb");
        assert_eq!(options.iterations, 7);
    }

    #[test]
    fn parses_positional_enterprise_number_before_flags() {
        let options = parse_args(
            ["0404.484.060", "--iterations", "3"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.iterations, 3);
    }

    #[test]
    fn accepts_help_without_enterprise_number() {
        assert!(parse_args(["--help"].into_iter().map(String::from))
            .expect("help should parse")
            .is_none());
    }

    #[test]
    fn rejects_duplicate_enterprise_numbers() {
        assert!(parse_args(["0404484060", "extra-arg"].into_iter().map(String::from),).is_err());
    }

    #[test]
    fn rejects_unknown_argument() {
        assert!(parse_args(
            ["0404484060", "--unknown-flag"]
                .into_iter()
                .map(String::from),
        )
        .is_err());
    }

    #[test]
    fn rejects_missing_option_values() {
        for option in ["--database", "--kbo-database", "--iterations"] {
            assert!(parse_args(["0404484060", option].into_iter().map(String::from),).is_err());
        }
    }

    #[test]
    fn requires_enterprise_number() {
        assert!(parse_args(std::iter::empty::<String>()).is_err());
    }

    #[test]
    fn rejects_invalid_or_out_of_range_iterations() {
        assert!(parse_args(
            ["0404484060", "--iterations", "abc"]
                .into_iter()
                .map(String::from),
        )
        .is_err());
        for value in [0, MAX_BENCHMARK_ITERATIONS + 1] {
            assert!(parse_args(
                ["0404484060", "--iterations", &value.to_string()]
                    .into_iter()
                    .map(String::from),
            )
            .is_err());
        }
    }
}
