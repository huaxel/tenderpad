use aanbestedingen_matcher::kbo::normalize_enterprise_number;
use aanbestedingen_matcher::store::{
    operator_label, validate_feedback_outcome, FeedbackUpdate, TenderStore,
};
use std::env;

fn validate_feedback_target(source: &str, external_id: &str) -> Result<(), String> {
    if source.trim().is_empty() {
        return Err("source must not be empty".to_owned());
    }
    if external_id.trim().is_empty() {
        return Err("external id must not be empty".to_owned());
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
struct FeedbackOptions {
    enterprise_number: String,
    source: String,
    external_id: String,
    relevant: bool,
    pursuable: Option<bool>,
    outcome: Option<String>,
    reason: Option<String>,
    database: String,
}

fn parse_args<I>(args: I) -> Result<Option<FeedbackOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut positionals = Vec::new();
    let mut relevant = None;
    let mut pursuable: Option<bool> = None;
    let mut outcome: Option<String> = None;
    let mut reason = None;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--relevant" => {
                if relevant == Some(false) {
                    return Err("choose exactly one of --relevant or --not-relevant".into());
                }
                relevant = Some(true);
            }
            "--not-relevant" => {
                if relevant == Some(true) {
                    return Err("choose exactly one of --relevant or --not-relevant".into());
                }
                relevant = Some(false);
            }
            "--reason" => reason = Some(arguments.next().ok_or("--reason needs a value")?),
            "--pursuable" => {
                if pursuable == Some(false) {
                    return Err("choose exactly one of --pursuable or --not-pursuable".into());
                }
                pursuable = Some(true);
            }
            "--not-pursuable" => {
                if pursuable == Some(true) {
                    return Err("choose exactly one of --pursuable or --not-pursuable".into());
                }
                pursuable = Some(false);
            }
            "--outcome" => {
                let value = arguments.next().ok_or("--outcome needs a value")?;
                validate_feedback_outcome(&value)?;
                outcome = Some(value);
            }
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => {
                if positionals.len() == 3 {
                    return Err("too many positional arguments".into());
                }
                positionals.push(argument.to_owned());
            }
        }
    }

    let mut positionals = positionals.into_iter();
    let enterprise_number =
        normalize_enterprise_number(&positionals.next().ok_or("enterprise number is required")?)?;
    let source = positionals
        .next()
        .ok_or("source is required")?
        .trim()
        .to_owned();
    let external_id = positionals
        .next()
        .ok_or("external id is required")?
        .trim()
        .to_owned();
    let relevant = relevant.ok_or("choose --relevant or --not-relevant")?;
    validate_feedback_target(&source, &external_id)?;
    Ok(Some(FeedbackOptions {
        enterprise_number,
        source,
        external_id,
        relevant,
        pursuable,
        outcome,
        reason,
        database,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: feedback ENTERPRISE_NUMBER SOURCE EXTERNAL_ID --relevant|--not-relevant [--pursuable|--not-pursuable] [--outcome bid|no-bid|won|lost] [--reason TEXT] [--database PATH]");
        return Ok(());
    };
    let FeedbackOptions {
        enterprise_number,
        source,
        external_id,
        relevant,
        pursuable,
        outcome,
        reason,
        database,
    } = options;
    let store = TenderStore::open(database)?;
    if store.tender(&source, &external_id)?.is_none() {
        return Err("tender not found".into());
    }
    store.update_feedback(
        &operator_label(),
        &enterprise_number,
        &source,
        &external_id,
        &FeedbackUpdate::replacement(relevant, pursuable, outcome, reason),
    )?;
    println!("Saved feedback for {source}/{external_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_before_positionals() {
        let options = parse_args(
            [
                "--database",
                "test.duckdb",
                "--relevant",
                "0404484060",
                "ted",
                "100-2026",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.source, "ted");
        assert_eq!(options.external_id, "100-2026");
        assert!(options.relevant);
        assert_eq!(options.database, "test.duckdb");
    }

    #[test]
    fn parses_positionals_before_flags() {
        let options = parse_args(
            [
                "0404.484.060",
                "ted",
                "100-2026",
                "--not-relevant",
                "--reason",
                "too high",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert!(!options.relevant);
        assert_eq!(options.reason.as_deref(), Some("too high"));
    }

    #[test]
    fn rejects_conflicting_relevance_flags() {
        assert!(parse_args(
            [
                "0404484060",
                "ted",
                "100-2026",
                "--relevant",
                "--not-relevant"
            ]
            .into_iter()
            .map(String::from),
        )
        .is_err());
    }

    #[test]
    fn rejects_missing_relevance_flag() {
        assert!(parse_args(
            ["0404484060", "ted", "100-2026"]
                .into_iter()
                .map(String::from),
        )
        .is_err());
    }

    #[test]
    fn accepts_help_without_positionals() {
        assert!(parse_args(["--help"].into_iter().map(String::from))
            .expect("help should parse")
            .is_none());
    }

    #[test]
    fn rejects_extra_positional_arguments() {
        assert!(parse_args(
            ["0404484060", "ted", "100-2026", "extra-arg", "--relevant"]
                .into_iter()
                .map(String::from),
        )
        .is_err());
    }

    #[test]
    fn rejects_missing_option_values_and_unknown_arguments() {
        for option in ["--reason", "--database", "--unknown"] {
            assert!(parse_args(
                ["0404484060", "ted", "100-2026", "--relevant", option]
                    .into_iter()
                    .map(String::from),
            )
            .is_err());
        }
    }

    #[test]
    fn rejects_empty_feedback_target() {
        assert!(validate_feedback_target(" ", "tender-1").is_err());
        assert!(validate_feedback_target("ted", "").is_err());
        assert!(validate_feedback_target("ted", "tender-1").is_ok());
    }
}
