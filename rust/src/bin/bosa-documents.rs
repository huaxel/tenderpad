use aanbestedingen_matcher::bosa::documents::{Attachment, ChecksumStatus, MAX_ATTACHMENT_BYTES};
use aanbestedingen_matcher::bosa::{BosaClient, BosaConfig, Environment};
use std::error::Error;
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
struct Options {
    environment: Environment,
    workspace: String,
    output: Option<PathBuf>,
    limit: usize,
    version_id: Option<uuid::Uuid>,
    allow_unverified: bool,
}
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, Box<dyn Error>> {
    let mut environment = Environment::Int;
    let mut workspace = None;
    let mut output = None;
    let mut limit = 1;
    let mut version_id = None;
    let mut allow_unverified = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        if arg == "--allow-unverified" {
            allow_unverified = true;
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--env" => environment = Environment::parse(&value)?,
            "--workspace" => workspace = Some(uuid::Uuid::parse_str(&value)?.to_string()),
            "--output" => {
                if value.trim().is_empty() {
                    return Err("--output cannot be empty".into());
                }
                output = Some(PathBuf::from(value));
            }
            "--limit" => limit = value.parse()?,
            "--version-id" => version_id = Some(uuid::Uuid::parse_str(&value)?),
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if !(1..=5).contains(&limit) {
        return Err("--limit must be 1..5".into());
    }
    if allow_unverified && output.is_none() {
        return Err("--allow-unverified requires --output".into());
    }
    Ok(Some(Options {
        environment,
        workspace: workspace.ok_or("--workspace UUID is required")?,
        output,
        limit,
        version_id,
        allow_unverified,
    }))
}
fn attachment_is_downloadable(attachment: &Attachment, allow_unverified: bool) -> bool {
    (attachment.sha256_verified && attachment.checksum_status == ChecksumStatus::Verified)
        || (allow_unverified
            && !attachment.sha256_verified
            && matches!(
                attachment.checksum_status,
                ChecksumStatus::Unknown | ChecksumStatus::Missing
            ))
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse_args(std::env::args().skip(1))? else {
        println!("Usage: bosa-documents --workspace UUID [--env int|prod] [--output DIRECTORY] [--limit 1..5] [--version-id UUID] [--allow-unverified]");
        println!("Lists metadata only unless --output is given. --limit examines at most 1..5 attachments. Unverified downloads require the explicit --allow-unverified flag; their local SHA-256 is recorded but marked unverified. Invalid checksum metadata is always skipped. Downloads are <=32 MiB each. Existing files are never overwritten. No extraction, scoring, or import.");
        return Ok(());
    };
    let mut client = BosaClient::new(BosaConfig::for_environment(options.environment)?);
    let attachments = client.attachments(&options.workspace)?;
    let mut downloads = Vec::new();
    let mut skipped = Vec::new();
    if let Some(directory) = options.output {
        // A chosen version ID replaces the first-N window; verification is unchanged.
        let selected: Vec<&Attachment> = match options.version_id {
            Some(id) => attachments.iter().filter(|a| a.version_id == id).collect(),
            None => attachments.iter().take(options.limit).collect(),
        };
        if options.version_id.is_some() && selected.is_empty() {
            return Err("--version-id matches no attachment in this workspace".into());
        }
        for attachment in selected {
            if !attachment_is_downloadable(attachment, options.allow_unverified) {
                skipped.push(serde_json::json!({
                    "version_id": attachment.version_id,
                    "checksum_status": attachment.checksum_status,
                }));
                continue;
            }
            let downloaded = if attachment.sha256_verified {
                client.download_attachment(
                    attachment,
                    options.environment,
                    &directory,
                    MAX_ATTACHMENT_BYTES,
                )?
            } else {
                client.download_attachment_unverified(
                    attachment,
                    options.environment,
                    &directory,
                    MAX_ATTACHMENT_BYTES,
                )?
            };
            downloads.push(downloaded);
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "workspace": options.workspace,
            "attachments": attachments,
            "downloads": downloads,
            "skipped": skipped,
        }))?
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicitly_verified_checksums_are_downloadable() {
        let verified = Attachment {
            version_id: uuid::Uuid::new_v4(),
            original_name: "verified.pdf".into(),
            sha256: "a".repeat(64),
            sha256_verified: true,
            checksum_status: ChecksumStatus::Verified,
        };
        let missing = Attachment {
            checksum_status: ChecksumStatus::Missing,
            sha256_verified: false,
            sha256: String::new(),
            ..verified.clone()
        };
        let unknown = Attachment {
            checksum_status: ChecksumStatus::Unknown,
            sha256: "UNKNOWN HASH".into(),
            ..missing.clone()
        };
        let invalid = Attachment {
            checksum_status: ChecksumStatus::Invalid,
            ..missing.clone()
        };
        assert!(attachment_is_downloadable(&verified, false));
        assert!(!attachment_is_downloadable(&missing, false));
        assert!(attachment_is_downloadable(&missing, true));
        assert!(attachment_is_downloadable(&unknown, true));
        assert!(!attachment_is_downloadable(&invalid, true));
    }

    #[test]
    fn version_id_is_parsed_and_must_be_a_uuid() {
        let id = "fdf0d911-f12f-4d8f-8552-01fbbdd9dc1c";
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        let ok = parse(&["--workspace", id, "--output", "/x", "--version-id", id])
            .unwrap()
            .unwrap();
        assert_eq!(ok.version_id.unwrap().to_string(), id);
        assert!(parse(&["--workspace", id, "--version-id", "nope"]).is_err());
    }

    #[test]
    fn listing_is_default_and_downloads_allow_verified_environments_and_bounds() {
        let id = "fdf0d911-f12f-4d8f-8552-01fbbdd9dc1c";
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        assert!(parse(&[]).is_err());
        assert!(parse(&["--workspace", "not-a-uuid"]).is_err());
        let options = parse(&["--workspace", id]).unwrap().unwrap();
        assert_eq!(options.environment, Environment::Int);
        assert_eq!(options.output, None);
        assert!(!options.allow_unverified);
        assert!(parse(&["--workspace", id, "--allow-unverified"]).is_err());
        assert!(parse(&["--workspace", id, "--output", "files"]).is_ok());
        let unverified = parse(&["--workspace", id, "--output", "files", "--allow-unverified"])
            .unwrap()
            .unwrap();
        assert!(unverified.allow_unverified);
        assert!(parse(&["--workspace", id, "--limit", "6"]).is_err());
        assert!(parse(&["--workspace", id, "--env", "prod", "--output", "files"]).is_ok());
    }
}
