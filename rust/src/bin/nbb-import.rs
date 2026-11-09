//! Import NBB Consult CSV exports into `nbb_financials`.
//!
//! Manual bridge until API access lands: download per-company CSVs from the
//! free Consult app, then:
//!
//! ```text
//! nbb-import --database data/tenders-rust.duckdb export-2024-00489680.csv
//! ```
//!
//! The filename stem becomes the filing `source_ref`. Nothing is inferred:
//! unknown codes are ignored, malformed rows abort the file (drift alarm).

use aanbestedingen_matcher::nbb::{parse_consult_csv, to_financials};
use aanbestedingen_matcher::nbb_xbrl::parse_consult_xbrl;
use aanbestedingen_matcher::store::TenderStore;
use std::env;
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
struct NbbImportOptions {
    database: String,
    method: String,
    files: Vec<PathBuf>,
    /// Bulk archives: report a filing that fails validation and continue.
    /// Off by default so a single-company import still aborts on drift.
    skip_invalid: bool,
}

fn parse_args<I>(args: I) -> Result<Option<NbbImportOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut method = "manual-consult".to_owned();
    let mut files = Vec::new();
    let mut skip_invalid = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--method" => method = arguments.next().ok_or("--method needs a value")?,
            "--skip-invalid" => skip_invalid = true,
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => files.push(PathBuf::from(argument)),
        }
    }
    if files.is_empty() {
        return Err("at least one Consult CSV file is required".into());
    }
    Ok(Some(NbbImportOptions {
        database,
        method,
        files,
        skip_invalid,
    }))
}

fn decode_bytes(bytes: &[u8], origin: &str) -> Result<String, Box<dyn std::error::Error>> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_owned()),
        Err(_) => {
            // Consult exports are usually UTF-8, but accented labels sometimes
            // arrive as Windows-1252. Decode lossily rather than abort the file.
            let (text, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
            if text.is_empty() {
                return Err(format!("{origin}: empty after decoding").into());
            }
            Ok(text.into_owned())
        }
    }
}

enum UnitText {
    Csv(String),
    Xbrl(String),
}

/// One importable unit: (source_ref, file text). A `.zip` Consult export can
/// hold several companies' filings, so it expands to one unit per entry;
/// each entry is parsed according to its own extension.
fn read_units(
    path: &std::path::Path,
) -> Result<Vec<(String, UnitText)>, Box<dyn std::error::Error>> {
    let is_zip = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if !is_zip {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("unusable filename: {}", path.display()))?;
        // Consult zips name files `<ref>.csv`; a bare export keeps its name.
        let source_ref = stem.strip_prefix("export-").unwrap_or(stem).to_owned();
        let bytes = std::fs::read(path)?;
        let origin = path.display().to_string();
        let text = decode_bytes(&bytes, &origin)?;
        return Ok(vec![(source_ref, unit_for(path, text)?)]);
    }
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut units = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name()?.into_owned();
        let lower = name.to_ascii_lowercase();
        if !lower.ends_with(".csv") && !lower.ends_with(".xbrl") {
            continue;
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes)?;
        let base = name.rsplit('/').next().unwrap_or(&name);
        let stem = std::path::Path::new(base)
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("unusable entry name: {name}"))?
            .to_owned();
        let origin = format!("{}:{name}", path.display());
        let text = decode_bytes(&bytes, &origin)?;
        let entry_path = std::path::Path::new(&name);
        units.push((stem, unit_for(entry_path, text)?));
    }
    if units.is_empty() {
        return Err(format!("{}: no CSV/XBRL entries found", path.display()).into());
    }
    Ok(units)
}

fn unit_for(path: &std::path::Path, text: String) -> Result<UnitText, Box<dyn std::error::Error>> {
    let is_xbrl = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("xbrl"));
    Ok(if is_xbrl {
        UnitText::Xbrl(text)
    } else {
        UnitText::Csv(text)
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = match parse_args(env::args().skip(1))? {
        Some(options) => options,
        None => {
            eprintln!(
                "usage: nbb-import [--database PATH] [--method NAME] [--skip-invalid] file.csv|file.xbrl|export.zip [...]"
            );
            return Ok(());
        }
    };
    let store = TenderStore::open(&options.database)?;
    let mut total = 0;
    let mut skipped = 0;
    for file in &options.files {
        for (source_ref, unit) in read_units(file)? {
            let parsed = match &unit {
                UnitText::Csv(text) => {
                    parse_consult_csv(text, &source_ref).map(|filing| vec![filing])
                }
                UnitText::Xbrl(text) => parse_consult_xbrl(text, &source_ref),
            };
            let filings = match parsed {
                Ok(filings) => filings,
                Err(error) if options.skip_invalid => {
                    skipped += 1;
                    eprintln!("skipped [{source_ref}] in {}: {error}", file.display());
                    continue;
                }
                Err(error) => return Err(format!("{}: {error}", file.display()).into()),
            };
            let source_format = match &unit {
                UnitText::Csv(_) => "csv",
                UnitText::Xbrl(_) => "xbrl",
            };
            for filing in &filings {
                let fin = to_financials(filing, &options.method, source_format);
                store.upsert_nbb_financials(&fin)?;
                total += 1;
                println!(
                    "stored {} FY{} turnover={:?} staff={:?} equity={:?} result={:?} [{}]",
                    fin.enterprise_kbo,
                    fin.fiscal_year,
                    fin.turnover,
                    fin.staff_fte,
                    fin.equity,
                    fin.result,
                    source_ref
                );
            }
        }
    }
    println!(
        "imported {total} filing(s) into {}; skipped {skipped} invalid unit(s)",
        options.database
    );
    // An import that succeeds can still leave filings undatable: a store built
    // by an older importer keeps the period boundaries empty, and every later
    // reader then sees "period unknown" instead of a stale store.
    let coverage = store.nbb_period_coverage()?;
    println!("{}", coverage.describe());
    if coverage.undatable() > 0 {
        eprintln!(
            "warning: {} stored filing(s) cannot be dated; re-import them with this importer",
            coverage.undatable()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn skip_invalid_is_opt_in() {
        let strict = parse_args(args(&["a.xbrl"])).unwrap().unwrap();
        assert!(!strict.skip_invalid);
        let bulk = parse_args(args(&["--skip-invalid", "day.zip"]))
            .unwrap()
            .unwrap();
        assert!(bulk.skip_invalid);
        assert_eq!(bulk.files, vec![PathBuf::from("day.zip")]);
    }
}
