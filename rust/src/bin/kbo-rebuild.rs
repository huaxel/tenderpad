use aanbestedingen_matcher::kbo_build::{build, DEFAULT_CSV_DIR, DEFAULT_OUTPUT};
use std::env;

#[derive(Debug, PartialEq)]
struct KboRebuildOptions {
    csv_dir: String,
    output: String,
    force: bool,
}

fn parse_args<I>(args: I) -> Result<Option<KboRebuildOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut csv_dir = DEFAULT_CSV_DIR.to_owned();
    let mut output = DEFAULT_OUTPUT.to_owned();
    let mut force = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--csv-dir" => {
                csv_dir = arguments.next().ok_or("--csv-dir needs a value")?;
            }
            "--output" => {
                output = arguments.next().ok_or("--output needs a value")?;
            }
            "--force" => {
                force = true;
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into());
            }
            argument => {
                return Err(format!("unexpected positional argument: {argument}").into());
            }
        }
    }

    Ok(Some(KboRebuildOptions {
        csv_dir,
        output,
        force,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: kbo-rebuild [--csv-dir PATH] [--output PATH] [--force]");
        return Ok(());
    };

    let counts = build(&options.csv_dir, &options.output, options.force)?;
    let order = [
        "kbo_enterprise",
        "kbo_establishment",
        "kbo_address",
        "kbo_activity",
        "kbo_denomination",
        "kbo_code",
    ];

    for table in order {
        if let Some(count) = counts.get(table) {
            println!("{table}: {count} rows");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_options() {
        let options = parse_args(std::iter::empty::<String>())
            .expect("default arguments should parse")
            .expect("help was not requested");
        assert_eq!(options.csv_dir, DEFAULT_CSV_DIR);
        assert_eq!(options.output, DEFAULT_OUTPUT);
        assert!(!options.force);
    }

    #[test]
    fn parses_explicit_options() {
        let options = parse_args(
            [
                "--csv-dir",
                "custom/csv",
                "--output",
                "custom/out.duckdb",
                "--force",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.csv_dir, "custom/csv");
        assert_eq!(options.output, "custom/out.duckdb");
        assert!(options.force);
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
    fn rejects_missing_values_and_unknown_arguments() {
        assert!(parse_args(["--csv-dir"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["--output"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["--unknown"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["extra-pos"].into_iter().map(String::from)).is_err());
    }
}
