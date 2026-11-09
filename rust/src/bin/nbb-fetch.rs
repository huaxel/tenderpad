use aanbestedingen_matcher::nbb_api::{Capture, Format, KeySlot, NbbClient, Product};
use chrono::NaiveDate;
use std::{
    env,
    error::Error,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

struct Options {
    operation: String,
    target: String,
    product: Product,
    slot: KeySlot,
    format: Format,
    output: Option<PathBuf>,
}
fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, Box<dyn Error>> {
    let mut args = args.into_iter();
    let Some(operation) = args.next() else {
        return Ok(None);
    };
    if operation == "--help" || operation == "-h" {
        return Ok(None);
    }
    if ![
        "references",
        "reference",
        "filing",
        "batch-references",
        "batch-filings",
    ]
    .contains(&operation.as_str())
    {
        return Err("unknown operation".into());
    }
    let target = args
        .next()
        .ok_or("operation needs an enterprise number, reference or date")?;
    let mut options = Options {
        operation,
        target,
        product: Product::Authentic,
        slot: KeySlot::Primary,
        format: Format::Xbrl,
        output: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--product" => {
                options.product = match args.next().as_deref() {
                    Some("authentic") => Product::Authentic,
                    Some("extracts") => Product::Extracts,
                    Some("archive") => Product::Archive,
                    _ => return Err("--product needs authentic, extracts or archive".into()),
                }
            }
            "--secondary" => options.slot = KeySlot::Secondary,
            "--format" => {
                options.format = match args.next().as_deref() {
                    Some("xbrl") => Format::Xbrl,
                    Some("json") => Format::JsonXbrl,
                    Some("pdf") => Format::Pdf,
                    _ => return Err("--format needs xbrl, json or pdf".into()),
                }
            }
            "--output" => options.output = Some(args.next().ok_or("--output needs a path")?.into()),
            _ => return Err("unknown option".into()),
        }
    }
    let batch = options.operation.starts_with("batch-");
    if batch == (options.product == Product::Authentic) {
        return Err("batch operations require --product extracts|archive; individual operations require authentic".into());
    }
    let download = options.operation == "filing" || batch;
    if download != options.output.is_some() {
        return Err("downloads require --output; individual reference lookups print JSON and do not take --output".into());
    }
    if options
        .output
        .as_ref()
        .is_some_and(|p| p.symlink_metadata().is_ok())
    {
        return Err("output already exists; refusing to overwrite".into());
    }
    Ok(Some(options))
}

/// Create only, including refusal of symlinks. Remove our own partial file on write failure.
/// Not an atomic publication protocol; callers must not consume the path until this returns.
fn save(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let result = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}
fn download(options: &Options, capture: Capture) -> Result<(), Box<dyn Error>> {
    let path = options.output.as_ref().ok_or("missing output")?;
    save(path, &capture.bytes)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "path":path,"bytes":capture.bytes.len(),"sha256":capture.sha256,
            "url":capture.url,"request_id":capture.request_id,"media_type":capture.media_type,
            "retrieved_at":capture.retrieved_at,
        }))?
    );
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse(env::args().skip(1))? else {
        println!("Usage: nbb-fetch references ENTERPRISE | reference REFERENCE\n       nbb-fetch filing REFERENCE --format xbrl|json|pdf --output PATH\n       nbb-fetch batch-references|batch-filings YYYY-MM-DD --product extracts|archive --output PATH [--format xbrl|json|pdf]\n       Optional: --secondary (use secondary key instead of primary). Production only.\nLoad credentials from rust/.env into the process environment before running.");
        return Ok(());
    };
    let client = NbbClient::from_env(options.product, options.slot)?;
    match options.operation.as_str() {
        "references" => println!(
            "{}",
            serde_json::to_string_pretty(&client.references(&options.target)?)?
        ),
        "reference" => println!(
            "{}",
            serde_json::to_string_pretty(&client.reference(&options.target)?)?
        ),
        "filing" => download(
            &options,
            client.accounting_data(&options.target, options.format)?,
        )?,
        "batch-references" | "batch-filings" => {
            // Daily archives can exceed the in-memory cap: stream to disk.
            let date = NaiveDate::parse_from_str(&options.target, "%Y-%m-%d")?;
            let path = options.output.as_ref().ok_or("missing output")?;
            let capture = if options.operation == "batch-references" {
                client.batch_references_to_file(date, path)?
            } else {
                client.batch_accounting_data_to_file(date, options.format, path)?
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "path":path,"bytes":capture.bytes,"sha256":capture.sha256,
                    "url":capture.url,"request_id":capture.request_id,"media_type":capture.media_type,
                    "retrieved_at":capture.retrieved_at,
                }))?
            );
        }
        _ => unreachable!(),
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }
    #[test]
    fn explicit_operation_product_and_output_are_required() {
        assert!(parse(args(&["references", "0537875391"]))
            .unwrap()
            .is_some());
        assert!(parse(args(&["filing", "2025-123"])).is_err());
        assert!(parse(args(&[
            "batch-references",
            "2026-01-05",
            "--output",
            "/tmp/test"
        ]))
        .is_err());
        assert!(parse(args(&["references", "0537875391", "--product", "extracts"])).is_err());
        assert!(parse(args(&["references", "0537875391", "--output", "/tmp/test"])).is_err());
        assert!(parse(args(&[
            "filing",
            "2025-123",
            "--format",
            "bad",
            "--output",
            "/tmp/test"
        ]))
        .is_err());
    }
    #[test]
    fn private_create_only_download() {
        let path = std::env::temp_dir().join(format!("nbb-download-{}", uuid::Uuid::new_v4()));
        save(&path, b"first").unwrap();
        assert!(save(&path, b"second").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let link = path.with_extension("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(save(&link, b"bad").is_err());
            std::fs::remove_file(link).unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }
}
