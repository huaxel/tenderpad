use aanbestedingen_matcher::eval_ranking::{evaluate, feedback_case, rank_baseline};
use aanbestedingen_matcher::recommendations::DEFAULT_RECOMMENDATION_CANDIDATES;
use serde_json::to_string_pretty;
use std::env;

#[derive(Debug, PartialEq)]
struct EvalRankingOptions {
    enterprise_number: String,
    kbo_database: String,
    tender_database: String,
    candidates: usize,
    k: usize,
}

fn terminal_safe(value: &str) -> String {
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            safe.push_str(&format!("\\u{{{:04X}}}", character as u32));
        } else {
            safe.push(character);
        }
    }
    safe
}

fn parse_args<I>(args: I) -> Result<Option<EvalRankingOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut enterprise_number = None;
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut tender_database = "data/tenders-rust.duckdb".to_owned();
    let mut candidates = DEFAULT_RECOMMENDATION_CANDIDATES;
    let mut k = 5;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?;
            }
            "--tender-database" => {
                tender_database = arguments.next().ok_or("--tender-database needs a value")?;
            }
            "--candidates" => {
                let val = arguments.next().ok_or("--candidates needs a value")?;
                candidates = val.parse().map_err(|_| "invalid --candidates value")?;
                if candidates == 0 {
                    return Err("--candidates must be at least 1".into());
                }
            }
            "--k" => {
                let val = arguments.next().ok_or("--k needs a value")?;
                k = val.parse().map_err(|_| "invalid --k value")?;
                if k == 0 {
                    return Err("--k must be at least 1".into());
                }
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into());
            }
            argument => {
                if enterprise_number.replace(argument.to_owned()).is_some() {
                    return Err("only one enterprise number is allowed".into());
                }
            }
        }
    }

    let enterprise_number = enterprise_number.ok_or("enterprise number is required")?;

    Ok(Some(EvalRankingOptions {
        enterprise_number,
        kbo_database,
        tender_database,
        candidates,
        k,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: eval-ranking ENTERPRISE_NUMBER [--kbo-database PATH] [--tender-database PATH] [--candidates N] [--k N]"
        );
        return Ok(());
    };

    let case = feedback_case(
        &options.enterprise_number,
        &options.kbo_database,
        &options.tender_database,
        options.candidates,
    )?;

    let ranked = rank_baseline(
        &case.company,
        &case.candidates,
        &case.history,
        case.preferences.as_ref(),
    );

    let result = evaluate(std::slice::from_ref(&case), |_| ranked.clone(), options.k)?;

    println!(
        "{}: {} candidates, {} judged ({:.1}% coverage, {} with outcome)",
        case.name,
        case.candidates.len(),
        case.relevance.len(),
        case.judged_coverage() * 100.0,
        case.outcome_labels,
    );
    let (pursuable, topic, not) = case.grade_counts();
    println!("  grades: {pursuable} pursuable, {topic} topic-only, {not} not");
    if case.relevance.len() < 10 {
        eprintln!(
            "warning: {} judged label(s): the metric is arithmetic, not evidence — \
             seed more labels before quoting precision (see docs/qc-framework.md)",
            case.relevance.len()
        );
    }
    if case.unmatched_labels > 0 {
        // Say it out loud: a label that cannot be placed is a judgement silently
        // missing from the only quality measurement the product has.
        eprintln!(
            "warning: {} relevance label(s) matched no current candidate or superseded \
             version; they are not part of this score",
            case.unmatched_labels
        );
    }

    let mut judged: Vec<(&String, &f64)> = case.relevance.iter().collect();
    judged.sort_by(|a, b| {
        b.1.partial_cmp(a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });

    // Relevance and ranking both use collision-safe composite source/id keys, so
    // same-id tenders from different sources are judged independently.
    for (key, grade) in judged {
        let position = match ranked.iter().position(|id| id == key) {
            Some(pos) => (pos + 1).to_string(),
            None => "None".to_string(),
        };
        let label = if *grade >= 2.0 {
            "pursuable"
        } else if *grade >= 1.0 {
            "relevant (topic only)"
        } else {
            "not relevant"
        };
        println!(
            "  {}: {label}, baseline rank {position}",
            terminal_safe(key)
        );
    }

    println!("{}", to_string_pretty(&result)?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_safe_escapes_control_characters() {
        assert_eq!(
            terminal_safe("abc\u{1b}[31m\n"),
            "abc\\u{001B}[31m\\u{000A}"
        );
    }

    #[test]
    fn parses_positional_enterprise_number_and_defaults() {
        let options = parse_args(["0404.484.060"].into_iter().map(String::from))
            .expect("arguments should parse")
            .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.kbo_database, "data/kbo/kbo_enrich.duckdb");
        assert_eq!(options.tender_database, "data/tenders-rust.duckdb");
        assert_eq!(options.candidates, DEFAULT_RECOMMENDATION_CANDIDATES);
        assert_eq!(options.k, 5);
    }

    #[test]
    fn parses_flags_before_positional_enterprise_number() {
        let options = parse_args(
            [
                "--kbo-database",
                "custom-kbo.duckdb",
                "--tender-database",
                "custom-tenders.duckdb",
                "--candidates",
                "50",
                "--k",
                "10",
                "0404.484.060",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.kbo_database, "custom-kbo.duckdb");
        assert_eq!(options.tender_database, "custom-tenders.duckdb");
        assert_eq!(options.candidates, 50);
        assert_eq!(options.k, 10);
    }

    #[test]
    fn accepts_help() {
        for flag in ["--help", "-h"] {
            assert!(parse_args([flag].into_iter().map(String::from))
                .expect("help should parse")
                .is_none());
        }
    }

    #[test]
    fn rejects_missing_enterprise_number() {
        assert!(parse_args(std::iter::empty::<String>()).is_err());
    }

    #[test]
    fn rejects_duplicate_enterprise_numbers() {
        assert!(parse_args(
            ["0404.484.060", "0200.065.765"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
    }

    #[test]
    fn rejects_invalid_numeric_values() {
        assert!(parse_args(
            ["0404.484.060", "--candidates", "not-a-number"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
        assert!(parse_args(
            ["0404.484.060", "--k", "not-a-number"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
    }

    #[test]
    fn rejects_zero_k_and_candidates() {
        assert!(parse_args(["0404.484.060", "--k", "0"].into_iter().map(String::from)).is_err());
        assert!(parse_args(
            ["0404.484.060", "--candidates", "0"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
    }

    #[test]
    fn rejects_missing_values_and_unknown_arguments() {
        assert!(parse_args(
            ["0404.484.060", "--kbo-database"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
        assert!(parse_args(
            ["0404.484.060", "--tender-database"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
        assert!(parse_args(["0404.484.060", "--unknown"].into_iter().map(String::from)).is_err());
    }
}
