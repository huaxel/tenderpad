use aanbestedingen_matcher::bosa::evidence::extract_evidence;
use aanbestedingen_matcher::bosa::pdf_text::sandboxed_pdf_text;
use aanbestedingen_matcher::bosa::{BosaClient, BosaConfig, Environment};
use aanbestedingen_matcher::store::TenderStore;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use uuid::Uuid;

const MAX_PDF_BYTES: u64 = 32 * 1024 * 1024;

struct Options {
    database: PathBuf,
    workspace: Uuid,
    pdf: PathBuf,
    environment: Environment,
}
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, Box<dyn Error>> {
    let mut database = None;
    let mut workspace = None;
    let mut pdf = None;
    let mut environment = Environment::Int;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--database" => database = Some(PathBuf::from(value)),
            "--workspace" => workspace = Some(Uuid::parse_str(&value)?),
            "--pdf" => pdf = Some(PathBuf::from(value)),
            "--env" => environment = Environment::parse(&value)?,
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    Ok(Some(Options {
        database: database.ok_or("--database required")?,
        workspace: workspace.ok_or("--workspace required")?,
        pdf: pdf.ok_or("--pdf required")?,
        environment,
    }))
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse_args(std::env::args().skip(1))? else {
        println!("Usage: bosa-evidence --database EXISTING --workspace UUID --pdf DOWNLOADED_PDF [--env int|prod]");
        println!("Matches the downloaded PDF to current BOSA document metadata and SHA256, extracts review hints in a macOS sandbox, and stores page-level provenance. No OCR. Backup the database first.");
        return Ok(());
    };
    if !options.database.is_file() {
        return Err("database does not exist; refusing to create one".into());
    }
    let metadata = fs::symlink_metadata(&options.pdf)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_PDF_BYTES {
        return Err("PDF must be a non-symlink regular file <=32 MiB".into());
    }
    let version = options
        .pdf
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or("PDF filename must be the downloaded document-version UUID")?;
    if options
        .pdf
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
        != Some("pdf")
    {
        return Err("only PDF documents are supported".into());
    }
    let mut client = BosaClient::new(BosaConfig::for_environment(options.environment)?);
    let docs = client.attachments(&options.workspace.to_string())?;
    let doc = docs
        .iter()
        .find(|d| d.version_id == version)
        .ok_or("document version is not a current activated attachment of this workspace")?;
    let mut file = fs::File::open(&options.pdf)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > MAX_PDF_BYTES {
            return Err("PDF exceeds 32 MiB".into());
        }
        hash.update(&buffer[..n]);
    }
    if format!("{:x}", hash.finalize()) != doc.sha256 {
        return Err("PDF checksum does not match current BOSA metadata".into());
    }
    let text = sandboxed_pdf_text(&options.pdf)?;
    // Re-hash after the subprocess to detect replacement while extraction ran.
    let mut after = Vec::new();
    fs::File::open(&options.pdf)?
        .take(MAX_PDF_BYTES + 1)
        .read_to_end(&mut after)?;
    if after.len() as u64 > MAX_PDF_BYTES || format!("{:x}", Sha256::digest(&after)) != doc.sha256 {
        return Err("PDF changed during extraction".into());
    }
    let evidence = extract_evidence(version, &doc.original_name, &doc.sha256, &text)?;
    let store = TenderStore::open_read_only(&options.database)?;
    let tender = store
        .bosa_workspace_tender(options.environment.source(), &options.workspace.to_string())?
        .ok_or("no stored tender for workspace; no writes performed")?;
    let notice = tender
        .raw
        .get("bosa_notice_id")
        .and_then(serde_json::Value::as_str)
        .ok_or("stored tender lacks notice ID")?
        .to_owned();
    let id = tender.external_id.clone();
    drop(store);
    let store = TenderStore::open(&options.database)?;
    let count = store.replace_attachment_evidence(
        options.environment.source(),
        &id,
        &options.workspace.to_string(),
        &notice,
        std::slice::from_ref(&evidence),
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"workspace":options.workspace,"tender":id,"version_id":version,"sha256":doc.sha256,"pages":evidence.page_count,"new_document_signals":evidence.signals.len(),"total_attachment_signals":count,"mandatory":false})
        )?
    );
    Ok(())
}
