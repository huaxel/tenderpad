use aanbestedingen_matcher::bosa::archive::{
    inspect_archive_visiting, ArchiveReport, DEFAULT_LIMITS,
};
use aanbestedingen_matcher::bosa::classify::{
    manifest_entry, text_layer_from_pdftotext, FileKind, ManifestEntry, TextLayerStatus,
};
use aanbestedingen_matcher::bosa::documents::MAX_ATTACHMENT_BYTES;
use aanbestedingen_matcher::bosa::ooxml_text;
use aanbestedingen_matcher::bosa::pdf_text::sandboxed_pdf_text;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Directory entries examined per run; a larger folder needs an explicit decision.
const MAX_FILES: usize = 2_000;
/// Sandboxed extractions per run (up to 45 s each); more needs a deliberate rerun.
const MAX_TEXT_LAYER_PDFS: usize = 100;
/// DOCX/XLSX members extracted per archive (in-process, unsandboxed).
const MAX_OOXML_MEMBERS_PER_ARCHIVE: usize = 50;

struct Options {
    directory: PathBuf,
    text_layer: bool,
    ooxml: bool,
    archives: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, Box<dyn Error>> {
    let mut directory = None;
    let mut text_layer = false;
    let mut ooxml = false;
    let mut archives = false;
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--text-layer" => text_layer = true,
            "--ooxml" => ooxml = true,
            "--archives" => archives = true,
            _ if arg.starts_with('-') => return Err(format!("unknown argument: {arg}").into()),
            _ => {
                if directory.replace(PathBuf::from(arg)).is_some() {
                    return Err("exactly one DIRECTORY is accepted".into());
                }
            }
        }
    }
    Ok(Some(Options {
        directory: directory.ok_or("DIRECTORY is required")?,
        text_layer,
        ooxml,
        archives,
    }))
}

#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum TextLayerOutcome {
    Graded {
        pages: usize,
        pages_with_text: usize,
        status: TextLayerStatus,
    },
    ExtractionFailed {
        reason: String,
    },
}

impl TextLayerOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Graded {
                status: TextLayerStatus::Present,
                ..
            } => "present",
            Self::Graded {
                status: TextLayerStatus::Partial,
                ..
            } => "partial",
            Self::Graded {
                status: TextLayerStatus::Absent,
                ..
            } => "absent",
            Self::ExtractionFailed { .. } => "extraction_failed",
        }
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum OoxmlOutcome {
    Extracted {
        segments: usize,
        truncated: bool,
        not_read: Vec<&'static str>,
    },
    ExtractionFailed {
        reason: String,
    },
}

#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum ArchiveOutcome {
    Listed {
        members_by_kind: BTreeMap<String, usize>,
        /// Segment counts for DOCX/XLSX members, only with --ooxml; keyed by in-archive path.
        #[serde(skip_serializing_if = "BTreeMap::is_empty")]
        ooxml_text: BTreeMap<String, OoxmlOutcome>,
        /// PDF members are not text-layer graded: the sandboxed runner needs a file.
        pdf_members_not_graded: usize,
        /// DOCX/XLSX members beyond the per-archive cap, not extracted.
        ooxml_members_not_run: usize,
        #[serde(flatten)]
        report: ArchiveReport,
    },
    Failed {
        reason: String,
    },
}

fn archive_outcome(bytes: &[u8], ooxml: bool) -> ArchiveOutcome {
    let mut ooxml_text = BTreeMap::new();
    let (mut pdf_members_not_graded, mut ooxml_members_not_run) = (0usize, 0usize);
    let result = inspect_archive_visiting(bytes, DEFAULT_LIMITS, |entry, data| match entry.kind {
        FileKind::Pdf => pdf_members_not_graded += 1,
        FileKind::Docx | FileKind::Xlsx if ooxml => {
            if ooxml_text.len() < MAX_OOXML_MEMBERS_PER_ARCHIVE {
                ooxml_text.insert(entry.original_name.clone(), ooxml_outcome(data));
            } else {
                ooxml_members_not_run += 1;
            }
        }
        _ => {}
    });
    match result {
        Ok(report) => {
            let mut members_by_kind = BTreeMap::new();
            for member in &report.members {
                let kind = serde_json::to_string(&member.kind)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_owned();
                *members_by_kind.entry(kind).or_default() += 1;
            }
            ArchiveOutcome::Listed {
                members_by_kind,
                ooxml_text,
                pdf_members_not_graded,
                ooxml_members_not_run,
                report,
            }
        }
        Err(reason) => ArchiveOutcome::Failed { reason },
    }
}

fn ooxml_outcome(bytes: &[u8]) -> OoxmlOutcome {
    match ooxml_text::extract(bytes) {
        Ok(text) => OoxmlOutcome::Extracted {
            segments: text.segments.len(),
            truncated: text.truncated,
            not_read: text.not_read,
        },
        Err(reason) => OoxmlOutcome::ExtractionFailed { reason },
    }
}

/// The sandboxed extractor reopens the path, so re-hash afterwards and refuse to
/// grade a file that no longer matches the bytes that were classified.
fn text_layer_outcome(path: &Path, expected_sha256: &str) -> TextLayerOutcome {
    let extracted = sandboxed_pdf_text(path);
    let unchanged = matches!(
        read_bounded(path, MAX_ATTACHMENT_BYTES),
        Ok(Some(bytes)) if format!("{:x}", Sha256::digest(&bytes)) == expected_sha256
    );
    if !unchanged {
        return TextLayerOutcome::ExtractionFailed {
            reason: "file changed or became unreadable during extraction".into(),
        };
    }
    match extracted {
        Ok(text) => {
            let summary = text_layer_from_pdftotext(&text);
            TextLayerOutcome::Graded {
                pages: summary.pages,
                pages_with_text: summary.pages_with_text,
                status: summary.status,
            }
        }
        Err(error) => TextLayerOutcome::ExtractionFailed {
            reason: error.to_string(),
        },
    }
}

/// Read at most `limit` bytes; a longer file is reported, never truncated into a
/// misleading classification.
fn read_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NOFOLLOW refuses a symlink swapped in after the directory listing, and
    // the regular-file check runs on the open handle, not the path. O_NONBLOCK
    // keeps a swapped-in FIFO from hanging the open.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse_args(std::env::args().skip(1))? else {
        println!("Usage: attachment-classify DIRECTORY [--text-layer] [--ooxml] [--archives]");
        println!("Classifies local files by their bytes (PDF, DOCX, XLSX, ZIP, images, ...) and prints a manifest plus coverage counts as JSON. Offline and read-only: no network, no extraction, no writes. Symbolic links and subdirectories are skipped and counted; files over 32 MiB are listed as oversized; at most 2000 entries are examined. With --text-layer, each PDF (at most 100) is also run through the macOS-sandboxed pdftotext and graded present/partial/absent; failures are reported as extraction_failed, never as absent. No OCR. With --ooxml, each DOCX/XLSX (at most 100) is read in-process (not sandboxed) and summarised as segment counts, never full text; failures are reported, not hidden. With --archives, each plain ZIP (at most 100) is listed member by member in memory (nothing written; nested archives, encrypted members, symlinks, unsafe or duplicate names and oversized members are skipped with a reason; truncation is flagged).");
        return Ok(());
    };
    let mut entries: Vec<ManifestEntry> = Vec::new();
    let (mut oversized, mut skipped_non_files, mut unreadable) = (Vec::new(), 0usize, Vec::new());
    let mut text_layers: BTreeMap<String, TextLayerOutcome> = BTreeMap::new();
    let mut text_layer_not_run = 0usize;
    let mut ooxml_summaries: BTreeMap<String, OoxmlOutcome> = BTreeMap::new();
    let mut ooxml_not_run = 0usize;
    let mut archive_reports: BTreeMap<String, ArchiveOutcome> = BTreeMap::new();
    let mut archives_not_run = 0usize;
    let mut names = fs::read_dir(&options.directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    let truncated = names.len() > MAX_FILES;
    for path in names.into_iter().take(MAX_FILES) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() {
            skipped_non_files += 1;
            continue;
        }
        match read_bounded(&path, MAX_ATTACHMENT_BYTES) {
            Ok(Some(bytes)) => {
                let entry = manifest_entry(&name, &bytes);
                if options.text_layer && entry.kind == FileKind::Pdf {
                    if text_layers.len() < MAX_TEXT_LAYER_PDFS {
                        text_layers.insert(name.clone(), text_layer_outcome(&path, &entry.sha256));
                    } else {
                        text_layer_not_run += 1;
                    }
                }
                if options.ooxml && matches!(entry.kind, FileKind::Docx | FileKind::Xlsx) {
                    if ooxml_summaries.len() < MAX_TEXT_LAYER_PDFS {
                        ooxml_summaries.insert(name.clone(), ooxml_outcome(&bytes));
                    } else {
                        ooxml_not_run += 1;
                    }
                }
                if options.archives && entry.kind == FileKind::Zip {
                    if archive_reports.len() < MAX_TEXT_LAYER_PDFS {
                        archive_reports
                            .insert(name.clone(), archive_outcome(&bytes, options.ooxml));
                    } else {
                        archives_not_run += 1;
                    }
                }
                entries.push(entry);
            }
            Ok(None) => oversized.push(name),
            Err(_) => unreadable.push(name),
        }
    }
    let mut text_layer_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for outcome in text_layers.values() {
        *text_layer_counts.entry(outcome.label()).or_default() += 1;
    }
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &entries {
        *by_kind
            .entry(
                serde_json::to_string(&entry.kind)?
                    .trim_matches('"')
                    .to_owned(),
            )
            .or_default() += 1;
    }
    let extension_mismatches: Vec<&str> = entries
        .iter()
        .filter(|entry| !entry.extension_matches_kind)
        .map(|entry| entry.original_name.as_str())
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "files_classified": entries.len(),
            "by_kind": by_kind,
            "unknown_kind": entries.iter().filter(|entry| entry.kind == FileKind::Unknown).count(),
            "extension_mismatches": extension_mismatches,
            "oversized": oversized,
            "unreadable": unreadable,
            "skipped_non_files": skipped_non_files,
            "directory_truncated": truncated,
            "text_layer_requested": options.text_layer,
            "pdf_text_layer_counts": text_layer_counts,
            "pdf_text_layer_not_run_over_cap": text_layer_not_run,
            "pdf_text_layers": text_layers,
            "ooxml_requested": options.ooxml,
            "ooxml_not_run_over_cap": ooxml_not_run,
            "ooxml_text": ooxml_summaries,
            "archives_requested": options.archives,
            "archives_not_run_over_cap": archives_not_run,
            "archives": archive_reports,
            "entries": entries,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_exactly_one_directory() {
        assert!(parse_args(Vec::<String>::new()).is_err());
        assert!(parse_args(["a".to_owned(), "b".to_owned()]).is_err());
        assert!(parse_args(["--bogus".to_owned()]).is_err());
        let parsed = parse_args(["dir".to_owned()]).unwrap().unwrap();
        assert_eq!(parsed.directory, PathBuf::from("dir"));
        assert!(!parsed.text_layer);
        let parsed = parse_args(["--text-layer".to_owned(), "dir".to_owned()])
            .unwrap()
            .unwrap();
        assert!(parsed.text_layer);
        assert!(parse_args(["--help".to_owned()]).unwrap().is_none());
    }

    #[test]
    fn a_file_that_no_longer_matches_its_hash_is_not_graded() {
        let path = std::env::temp_dir().join(format!("attachment-hash-{}", std::process::id()));
        fs::write(&path, b"%PDF-1.7 changed after classification").unwrap();
        let stale = format!("{:x}", Sha256::digest(b"%PDF-1.7 original"));
        match text_layer_outcome(&path, &stale) {
            TextLayerOutcome::ExtractionFailed { reason } => assert!(reason.contains("changed")),
            TextLayerOutcome::Graded { .. } => panic!("a changed file must not be graded"),
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn oversized_files_are_not_classified_from_a_truncated_read() {
        let path = std::env::temp_dir().join(format!("attachment-classify-{}", std::process::id()));
        fs::write(&path, b"%PDF-1.7 0123456789").unwrap();
        assert!(read_bounded(&path, 8).unwrap().is_none());
        assert!(read_bounded(&path, 1024).unwrap().is_some());
        fs::remove_file(path).unwrap();
    }
}
