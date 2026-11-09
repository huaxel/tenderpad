use aanbestedingen_matcher::bda::load_bda_documents;
use aanbestedingen_matcher::store::TenderStore;
use std::env;

#[derive(Debug, PartialEq)]
struct BdaIngestOptions {
    path: String,
    database: String,
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

fn parse_args<I>(args: I) -> Result<Option<BdaIngestOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut path = None;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => {
                if path.replace(argument.to_owned()).is_some() {
                    return Err("only one BDA input path is allowed".into());
                }
            }
        }
    }
    Ok(Some(BdaIngestOptions {
        path: path.ok_or("BDA XML, ZIP or directory path is required")?,
        database,
    }))
}

fn validate_import(parsed: usize, skipped: usize) -> Result<(), String> {
    if parsed == 0 && skipped > 0 {
        return Err(format!(
            "no BDA notices parsed; skipped {skipped} unparseable document(s)"
        ));
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: bda-ingest PATH [--database PATH]");
        return Ok(());
    };
    let BdaIngestOptions { path, database } = options;
    let (tenders, skipped, awards) = load_bda_documents(path)?;
    for (name, error) in &skipped {
        eprintln!(
            "Warning: skipped unparseable document {}: {}",
            terminal_safe(name),
            terminal_safe(error)
        );
    }
    validate_import(tenders.len(), skipped.len())?;
    let store = TenderStore::open(database)?;
    let stored = store.upsert_many(tenders.iter())?;
    let mut award_files = std::collections::BTreeSet::new();
    for award in &awards {
        award_files.insert(award.source_name.clone());
    }
    let mut awards_stored = 0;
    for file in &award_files {
        let file_awards: Vec<_> = awards
            .iter()
            .filter(|award| &award.source_name == file)
            .cloned()
            .collect();
        awards_stored += store.replace_bda_awards(file, &file_awards)?;
    }
    println!(
        "Stored {stored} BDA notices and {awards_stored} BDA awards; Rust database contains {} total",
        store.count()?
    );
    if !skipped.is_empty() {
        println!("Skipped {} unparseable documents", skipped.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_skipped_import_fails_but_partial_import_is_allowed() {
        assert!(validate_import(0, 2).is_err());
        assert!(validate_import(1, 2).is_ok());
        assert!(validate_import(0, 0).is_ok());
    }

    #[test]
    fn terminal_safe_escapes_control_characters() {
        assert_eq!(
            terminal_safe("archive\u{1b}[31m\n"),
            "archive\\u{001B}[31m\\u{000A}"
        );
    }

    #[test]
    fn parses_flags_before_positional_path() {
        let options = parse_args(
            ["--database", "test.duckdb", "path/to/file.zip"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.path, "path/to/file.zip");
        assert_eq!(options.database, "test.duckdb");
    }

    #[test]
    fn parses_positional_path_before_flags() {
        let options = parse_args(
            ["path/to/file.zip", "--database", "test.duckdb"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.path, "path/to/file.zip");
        assert_eq!(options.database, "test.duckdb");
    }

    #[test]
    fn accepts_help_without_path() {
        assert!(parse_args(["--help"].into_iter().map(String::from))
            .expect("help should parse")
            .is_none());
    }

    #[test]
    fn rejects_duplicate_paths() {
        assert!(parse_args(["first.xml", "second.xml"].into_iter().map(String::from),).is_err());
    }

    #[test]
    fn requires_path() {
        assert!(parse_args(std::iter::empty::<String>()).is_err());
    }

    #[test]
    fn rejects_missing_database_value_and_unknown_arguments() {
        assert!(parse_args(["input.xml", "--database"].into_iter().map(String::from),).is_err());
        assert!(parse_args(["input.xml", "--unknown"].into_iter().map(String::from),).is_err());
    }
}
