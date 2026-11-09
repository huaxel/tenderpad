use aanbestedingen_matcher::kbo::normalize_enterprise_number;
use aanbestedingen_matcher::recognition;
use aanbestedingen_matcher::store::{operator_label, TenderStore};
use aanbestedingen_matcher::{validate_value_limits, CompanyPreferences};
use rust_decimal::Decimal;
use std::env;

fn split_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[derive(Debug, PartialEq)]
struct PreferencesOptions {
    enterprise_number: String,
    database: String,
    confirmed_activities: Option<Vec<String>>,
    excluded_terms: Option<Vec<String>>,
    preferred_regions: Option<Vec<String>>,
    languages: Option<Vec<String>>,
    minimum_value: Option<Option<Decimal>>,
    maximum_value: Option<Option<Decimal>>,
    recognition_class: Option<Option<u8>>,
    recognition_categories: Option<Vec<String>>,
}

fn parse_args<I>(args: I) -> Result<Option<PreferencesOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut raw_enterprise_number = None;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut confirmed_activities = None;
    let mut excluded_terms = None;
    let mut preferred_regions = None;
    let mut languages = None;
    let mut minimum_value = None;
    let mut maximum_value = None;
    let mut recognition_class = None;
    let mut recognition_categories = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--confirmed-activities" => {
                confirmed_activities = Some(split_values(
                    &arguments
                        .next()
                        .ok_or("--confirmed-activities needs a value")?,
                ))
            }
            "--excluded-terms" => {
                excluded_terms = Some(split_values(
                    &arguments.next().ok_or("--excluded-terms needs a value")?,
                ))
            }
            "--preferred-regions" => {
                preferred_regions = Some(split_values(
                    &arguments
                        .next()
                        .ok_or("--preferred-regions needs a value")?,
                ))
            }
            "--languages" => {
                languages = Some(split_values(
                    &arguments.next().ok_or("--languages needs a value")?,
                ))
            }
            "--minimum-value" => {
                minimum_value = Some(Some(
                    arguments
                        .next()
                        .ok_or("--minimum-value needs a value")?
                        .parse()?,
                ))
            }
            "--maximum-value" => {
                maximum_value = Some(Some(
                    arguments
                        .next()
                        .ok_or("--maximum-value needs a value")?
                        .parse()?,
                ))
            }
            "--clear-minimum-value" => minimum_value = Some(None),
            "--clear-maximum-value" => maximum_value = Some(None),
            "--recognition-class" => {
                recognition_class = Some(Some(
                    arguments
                        .next()
                        .ok_or("--recognition-class needs a value")?
                        .parse()?,
                ))
            }
            "--recognition-categories" => {
                recognition_categories = Some(split_values(
                    &arguments
                        .next()
                        .ok_or("--recognition-categories needs a value")?,
                ))
            }
            "--clear-recognition" => {
                recognition_class = Some(None);
                recognition_categories = Some(Vec::new());
            }
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
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
    Ok(Some(PreferencesOptions {
        enterprise_number,
        database,
        confirmed_activities,
        excluded_terms,
        preferred_regions,
        languages,
        minimum_value,
        maximum_value,
        recognition_class,
        recognition_categories,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: preferences ENTERPRISE_NUMBER [OPTIONS]");
        println!("  --confirmed-activities a,b   --excluded-terms a,b");
        println!("  --preferred-regions a,b      --languages nl,fr");
        println!("  --minimum-value N            --maximum-value N");
        println!("  --clear-minimum-value        --clear-maximum-value");
        println!("  --recognition-class 1..8     --recognition-categories C,D17");
        println!("  --clear-recognition          --database PATH");
        return Ok(());
    };
    let PreferencesOptions {
        enterprise_number,
        database,
        confirmed_activities,
        excluded_terms,
        preferred_regions,
        languages,
        minimum_value,
        maximum_value,
        recognition_class,
        recognition_categories,
    } = options;
    let store = TenderStore::open(database)?;
    let mut preferences: CompanyPreferences = store.preferences_for(&enterprise_number)?;
    if let Some(value) = confirmed_activities {
        preferences.confirmed_activities = value;
    }
    if let Some(value) = excluded_terms {
        preferences.excluded_terms = value;
    }
    if let Some(value) = preferred_regions {
        preferences.preferred_regions = value;
    }
    if let Some(value) = languages {
        preferences.languages = value;
    }
    if let Some(value) = minimum_value {
        preferences.minimum_value = value;
    }
    if let Some(value) = maximum_value {
        preferences.maximum_value = value;
    }
    if let Some(value) = recognition_class {
        preferences.recognition_class = value;
    }
    if let Some(value) = recognition_categories {
        preferences.recognition_categories = value;
    }
    validate_value_limits(preferences.minimum_value, preferences.maximum_value)?;
    recognition::validate_recognition(
        preferences.recognition_class,
        &preferences.recognition_categories,
    )
    .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    store.save_preferences_audited(&operator_label(), &enterprise_number, &preferences)?;
    println!("Saved preferences for {enterprise_number}");
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
                "--languages",
                "nl,fr",
                "0404484060",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.database, "test.duckdb");
        assert_eq!(
            options.languages,
            Some(vec!["nl".to_owned(), "fr".to_owned()])
        );
    }

    #[test]
    fn parses_positional_enterprise_number_before_flags() {
        let options = parse_args(
            ["0404.484.060", "--minimum-value", "1000"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.minimum_value, Some(Some(Decimal::new(1000, 0))));
    }

    #[test]
    fn rejects_multiple_positional_arguments() {
        assert!(parse_args(["0404484060", "extra-arg"].into_iter().map(String::from),).is_err());
    }

    #[test]
    fn parses_clear_and_list_options() {
        let options = parse_args(
            [
                "0404484060",
                "--confirmed-activities",
                " 45, ,62 ",
                "--clear-minimum-value",
                "--clear-maximum-value",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(
            options.confirmed_activities,
            Some(vec!["45".to_owned(), "62".to_owned()])
        );
        assert_eq!(options.minimum_value, Some(None));
        assert_eq!(options.maximum_value, Some(None));
    }

    #[test]
    fn accepts_help_without_enterprise_number() {
        assert!(parse_args(["--help"].into_iter().map(String::from))
            .expect("help should parse")
            .is_none());
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
        for option in [
            "--confirmed-activities",
            "--excluded-terms",
            "--preferred-regions",
            "--languages",
            "--minimum-value",
            "--maximum-value",
            "--database",
        ] {
            assert!(parse_args(["0404484060", option].into_iter().map(String::from),).is_err());
        }
    }
}
