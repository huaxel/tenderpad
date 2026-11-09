use aanbestedingen_matcher::pras_build::{
    build, DEFAULT_GEOJSON, DEFAULT_GPKG, DEFAULT_OUTPUT, EXPECTED_ZONES,
};
use std::env;

#[derive(Debug, PartialEq)]
struct PrasRebuildOptions {
    geojson: String,
    output: String,
    gpkg: Option<String>,
    force: bool,
}

fn parse_args<I>(args: I) -> Result<Option<PrasRebuildOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut geojson = DEFAULT_GEOJSON.to_owned();
    let mut output = DEFAULT_OUTPUT.to_owned();
    let mut gpkg = Some(DEFAULT_GPKG.to_owned());
    let mut no_gpkg = false;
    let mut force = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--geojson" => {
                geojson = arguments.next().ok_or("--geojson needs a value")?;
            }
            "--output" => {
                output = arguments.next().ok_or("--output needs a value")?;
            }
            "--gpkg" => {
                gpkg = Some(arguments.next().ok_or("--gpkg needs a value")?);
            }
            "--no-gpkg" => {
                no_gpkg = true;
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

    let effective_gpkg = if no_gpkg { None } else { gpkg };

    Ok(Some(PrasRebuildOptions {
        geojson,
        output,
        gpkg: effective_gpkg,
        force,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: pras-rebuild [--geojson PATH] [--output PATH] [--gpkg PATH] [--no-gpkg] [--force]"
        );
        return Ok(());
    };

    let counts = build(
        &options.geojson,
        &options.output,
        options.force,
        EXPECTED_ZONES,
        options.gpkg.as_deref(),
    )?;

    let order = ["pras_zones", "be_streets", "be_addrs"];

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
        assert_eq!(options.geojson, DEFAULT_GEOJSON);
        assert_eq!(options.output, DEFAULT_OUTPUT);
        assert_eq!(options.gpkg, Some(DEFAULT_GPKG.to_owned()));
        assert!(!options.force);
    }

    #[test]
    fn parses_explicit_options_with_no_gpkg() {
        let options = parse_args(
            [
                "--geojson",
                "custom.geojson",
                "--output",
                "custom.duckdb",
                "--no-gpkg",
                "--force",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.geojson, "custom.geojson");
        assert_eq!(options.output, "custom.duckdb");
        assert_eq!(options.gpkg, None);
        assert!(options.force);
    }

    #[test]
    fn parses_custom_gpkg() {
        let options = parse_args(["--gpkg", "custom.gpkg"].into_iter().map(String::from))
            .expect("arguments should parse")
            .expect("help was not requested");
        assert_eq!(options.gpkg, Some("custom.gpkg".to_owned()));
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
        assert!(parse_args(["--geojson"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["--output"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["--gpkg"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["--unknown"].into_iter().map(String::from)).is_err());
        assert!(parse_args(["extra-pos"].into_iter().map(String::from)).is_err());
    }
}
