//! Portable PDF evidence bundles (v1), optionally paired with XBRL (v2). `manifest.json` is UTF-8 JSON:
//! `{ "version":1, "enterprise":"0123456789",
//! "restriction":"private_research_unreviewed_redistribution", "files":[{
//! "reference":"2025-123", "filename":"2025-123.pdf",
//! "period":{"start":"2024-01-01","end":"2024-12-31"},
//! "metadata":{ "ReferenceNumber":"2025-123", "EnterpriseNumber":"0123456789",
//! "Currency":"EUR", "ExerciseDates":{"startDate":"2024-01-01","endDate":"2024-12-31"}},
//! "receipt":{"url":"https://ws.cbso.nbb.be/authentic/deposit/2025-123/accountingData",
//! "request_id":"UUID", "retrieved_at":"RFC3339", "media_type":"application/pdf",
//! "sha256":"lowercase hex SHA-256", "bytes":123}}] }`.
//! In v2 each filing also has `xbrl: {filename, receipt}` with application/x.xbrl.
//! All v2 filings require this pair; v1 forbids it. Existing v1 bundles stay unchanged.
//! Metadata preserves provider fields but is unverified provider metadata, not a
//! retained raw metadata response. Receipts describe individual artifact captures.
//! No financial facts, group coverage or redistribution permission are asserted.
//! Verification checks internal consistency and artifact bytes, not financial correctness,
//! source authenticity or independent acquisition evidence. A modified manifest is
//! not authenticated. Consumers should retain their own trusted manifest hash.
//!
//! Creation reserves a new directory with create_dir and writes the manifest LAST.
//! There is no atomic whole-directory publication claim: interrupted bundles may
//! remain without a valid manifest and must be rejected. Ordinary errors clean up
//! only the directory reserved by this invocation. Do not mutate bundles while read.
#[cfg(feature = "network")]
use crate::nbb_api::{Capture, Format, NbbClient, NbbError};
use chrono::{DateTime, NaiveDate};
use quick_xml::{events::Event, name::ResolveResult, NsReader};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(feature = "network")]
use std::fs;
#[cfg(feature = "network")]
use std::io::Write;
use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fs::OpenOptions,
    io::Read,
    path::Path,
};
use uuid::Uuid;

pub type BundleResult<T> = Result<T, Box<dyn Error>>;
pub const RESTRICTION: &str = "private_research_unreviewed_redistribution";
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Preserve provider fields without inventing or flattening accounting semantics.
#[derive(Debug, Deserialize, Serialize)]
pub struct FilingReference {
    #[serde(rename = "ReferenceNumber")]
    pub reference_number: String,
    #[serde(rename = "EnterpriseNumber")]
    pub enterprise_number: String,
    #[serde(flatten)]
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub enterprise: String,
    pub restriction: String,
    pub files: Vec<Filing>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filing {
    pub reference: String,
    pub filename: String,
    pub period: Period,
    pub metadata: FilingReference,
    pub receipt: Receipt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xbrl: Option<Artifact>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub filename: String,
    pub receipt: Receipt,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub start: String,
    pub end: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub url: String,
    pub request_id: Uuid,
    pub retrieved_at: String,
    pub media_type: String,
    pub sha256: String,
    pub bytes: u64,
}

#[cfg(feature = "network")]
/// Inject retained metadata/capture bytes for offline assembly, or use NbbClient.
/// Implementations must return original artifact receipt values, not invent a new capture.
pub trait Source {
    fn reference(&self, reference: &str) -> Result<FilingReference, NbbError>;
    fn pdf(&self, reference: &str) -> Result<Capture, NbbError>;
    fn xbrl(&self, _reference: &str) -> Result<Capture, NbbError> {
        Err(NbbError::Response("XBRL capture unsupported by source"))
    }
}
#[cfg(feature = "network")]
impl Source for NbbClient {
    fn reference(&self, reference: &str) -> Result<FilingReference, NbbError> {
        self.reference(reference)
    }
    fn pdf(&self, reference: &str) -> Result<Capture, NbbError> {
        self.accounting_data(reference, Format::Pdf)
    }
    fn xbrl(&self, reference: &str) -> Result<Capture, NbbError> {
        self.accounting_data(reference, Format::Xbrl)
    }
}
fn enterprise_valid(enterprise: &str) -> bool {
    enterprise.len() == 10 && enterprise.bytes().all(|b| b.is_ascii_digit())
}
fn reference_valid(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 64
        && reference.bytes().all(|b| b.is_ascii_digit() || b == b'-')
}
fn requested(enterprise: &str, references: &[String]) -> BundleResult<()> {
    if !enterprise_valid(enterprise) || references.is_empty() {
        return Err("need a ten-digit enterprise and at least one reference".into());
    }
    let mut seen = HashSet::new();
    for reference in references {
        if !reference_valid(reference) || !seen.insert(reference) {
            return Err("invalid or duplicate reference".into());
        }
    }
    Ok(())
}
fn metadata_period(
    metadata: &FilingReference,
    enterprise: &str,
    reference: &str,
) -> BundleResult<Period> {
    if metadata.enterprise_number != enterprise || metadata.reference_number != reference {
        return Err("metadata enterprise/reference mismatch".into());
    }
    if metadata.fields.get("Currency").and_then(|v| v.as_str()) != Some("EUR") {
        return Err("metadata currency must be EUR".into());
    }
    let dates = metadata
        .fields
        .get("ExerciseDates")
        .ok_or("missing ExerciseDates")?;
    let start = dates
        .get("startDate")
        .and_then(|v| v.as_str())
        .ok_or("missing startDate")?;
    let end = dates
        .get("endDate")
        .and_then(|v| v.as_str())
        .ok_or("missing endDate")?;
    let parse = |s: &str| -> BundleResult<NaiveDate> {
        let date = NaiveDate::parse_from_str(s, "%Y-%m-%d")?;
        if date.format("%Y-%m-%d").to_string() != s {
            return Err("period dates must be YYYY-MM-DD".into());
        }
        Ok(date)
    };
    if parse(start)? > parse(end)? {
        return Err("period start is after end".into());
    }
    Ok(Period {
        start: start.into(),
        end: end.into(),
    })
}
fn check_receipt(
    reference: &str,
    receipt: &Receipt,
    bytes: &[u8],
    media: &str,
) -> BundleResult<()> {
    if receipt.url != format!("https://ws.cbso.nbb.be/authentic/deposit/{reference}/accountingData")
        || receipt.media_type != media
        || receipt.request_id.is_nil()
        || DateTime::parse_from_rfc3339(&receipt.retrieved_at).is_err()
    {
        return Err("invalid capture receipt".into());
    }
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES
        || receipt.bytes != bytes.len() as u64
        || receipt.sha256 != format!("{:x}", Sha256::digest(bytes))
    {
        return Err("artifact length or SHA-256 mismatch".into());
    }
    Ok(())
}
fn check_pdf(reference: &str, receipt: &Receipt, bytes: &[u8]) -> BundleResult<()> {
    check_receipt(reference, receipt, bytes, "application/pdf")?;
    if !bytes.starts_with(b"%PDF-") {
        return Err("invalid PDF signature".into());
    }
    Ok(())
}
/// Envelope validation only: no facts, contexts, entity or period matching.
fn check_xbrl(reference: &str, receipt: &Receipt, bytes: &[u8]) -> BundleResult<()> {
    check_receipt(reference, receipt, bytes, "application/x.xbrl")?;
    let mut reader = NsReader::from_reader(bytes);
    let mut depth = 0usize;
    let mut root_seen = false;
    loop {
        let (namespace, event) = reader.read_resolved_event()?;
        match event {
            Event::DocType(_) => return Err("XBRL DOCTYPE forbidden".into()),
            Event::Start(ref element) | Event::Empty(ref element) => {
                if depth == 0 {
                    if root_seen
                        || element.local_name().as_ref() != b"xbrl"
                        || !matches!(namespace, ResolveResult::Bound(ns) if ns.as_ref() == b"http://www.xbrl.org/2003/instance")
                    {
                        return Err("expected one XBRL instance root".into());
                    }
                    root_seen = true;
                }
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                    if depth > 256 {
                        return Err("XBRL nesting exceeds 256".into());
                    }
                }
            }
            Event::End(_) => depth = depth.checked_sub(1).ok_or("unmatched XML end")?,
            Event::Text(text) if depth == 0 && !text.iter().all(u8::is_ascii_whitespace) => {
                return Err("text outside XBRL root".into());
            }
            Event::CData(_) | Event::GeneralRef(_) if depth == 0 => {
                return Err("content outside XBRL root".into());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || depth != 0 {
        return Err("incomplete XBRL instance".into());
    }
    Ok(())
}
#[cfg(feature = "network")]
fn capture_receipt(capture: &Capture) -> Receipt {
    Receipt {
        url: capture.url.clone(),
        request_id: capture.request_id,
        retrieved_at: capture.retrieved_at.clone(),
        media_type: capture.media_type.clone(),
        sha256: capture.sha256.clone(),
        bytes: capture.bytes.len() as u64,
    }
}
#[cfg(feature = "network")]
fn save(path: &Path, bytes: &[u8]) -> BundleResult<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
#[cfg(feature = "network")]
/// Create-only, bounded PDF acquisition. Metadata is checked before each PDF call.
/// Existing paths (including dangling symlinks) fail before any source calls.
pub fn create_bundle(
    source: &impl Source,
    enterprise: &str,
    references: &[String],
    output: &Path,
) -> BundleResult<Manifest> {
    create(source, enterprise, references, output, false)
}
#[cfg(feature = "network")]
/// Create a new v2 bundle. Every filing must supply both PDF and XBRL; no fallback.
pub fn create_bundle_with_xbrl(
    source: &impl Source,
    enterprise: &str,
    references: &[String],
    output: &Path,
) -> BundleResult<Manifest> {
    create(source, enterprise, references, output, true)
}
#[cfg(feature = "network")]
fn create(
    source: &impl Source,
    enterprise: &str,
    references: &[String],
    output: &Path,
    xbrl: bool,
) -> BundleResult<Manifest> {
    requested(enterprise, references)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(output)?;
    let result = (|| {
        let mut manifest = Manifest {
            version: if xbrl { 2 } else { 1 },
            enterprise: enterprise.into(),
            restriction: RESTRICTION.into(),
            files: Vec::new(),
        };
        for reference in references {
            let metadata = source.reference(reference)?;
            let period = metadata_period(&metadata, enterprise, reference)?;
            let capture = source.pdf(reference)?;
            let receipt = capture_receipt(&capture);
            check_pdf(reference, &receipt, &capture.bytes)?;
            let filename = format!("{reference}.pdf");
            save(&output.join(&filename), &capture.bytes)?;
            let xbrl = if xbrl {
                let capture = source.xbrl(reference)?;
                let receipt = capture_receipt(&capture);
                check_xbrl(reference, &receipt, &capture.bytes)?;
                let filename = format!("{reference}.xbrl");
                save(&output.join(&filename), &capture.bytes)?;
                Some(Artifact { filename, receipt })
            } else {
                None
            };
            manifest.files.push(Filing {
                reference: reference.clone(),
                filename,
                period,
                metadata,
                receipt,
                xbrl,
            });
        }
        let json = serde_json::to_vec_pretty(&manifest)?;
        if json.len() as u64 > MAX_MANIFEST_BYTES {
            return Err("manifest exceeds 16 MiB limit".into());
        }
        save(&output.join("manifest.json"), &json)?;
        Ok(manifest)
    })();
    if result.is_err() {
        fs::remove_dir_all(output).map_err(|e| format!("bundle failed; cleanup failed: {e}"))?;
    }
    result
}
fn read_regular(path: &Path, limit: u64) -> BundleResult<Vec<u8>> {
    if !path.symlink_metadata()?.file_type().is_file() {
        return Err("bundle file must be regular, not a symlink".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
        return Err("invalid bundle file type or size".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("bundle file exceeds size limit".into());
    }
    Ok(bytes)
}
/// Offline consistency check only; requires no credentials or network access.
pub fn verify_bundle(path: &Path) -> BundleResult<Manifest> {
    if !path.symlink_metadata()?.file_type().is_dir() {
        return Err("bundle must be a directory, not a symlink".into());
    }
    let manifest: Manifest = serde_json::from_slice(&read_regular(
        &path.join("manifest.json"),
        MAX_MANIFEST_BYTES,
    )?)?;
    if !matches!(manifest.version, 1 | 2) || manifest.restriction != RESTRICTION {
        return Err("unsupported manifest version or restriction".into());
    }
    let references: Vec<_> = manifest.files.iter().map(|f| f.reference.clone()).collect();
    requested(&manifest.enterprise, &references)?;
    for filing in &manifest.files {
        if filing.filename != format!("{}.pdf", filing.reference)
            || filing.period
                != metadata_period(&filing.metadata, &manifest.enterprise, &filing.reference)?
        {
            return Err("unsafe filename or inconsistent period".into());
        }
        let bytes = read_regular(&path.join(&filing.filename), MAX_ARTIFACT_BYTES)?;
        check_pdf(&filing.reference, &filing.receipt, &bytes)?;
        match (manifest.version, &filing.xbrl) {
            (1, None) => {}
            (2, Some(artifact)) => {
                if artifact.filename != format!("{}.xbrl", filing.reference) {
                    return Err("unsafe XBRL filename".into());
                }
                let bytes = read_regular(&path.join(&artifact.filename), MAX_ARTIFACT_BYTES)?;
                check_xbrl(&filing.reference, &artifact.receipt, &bytes)?;
            }
            _ => return Err("artifact set inconsistent with manifest version".into()),
        }
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "network")]
    use super::*;
    #[cfg(feature = "network")]
    use std::cell::Cell;
    #[cfg(feature = "network")]
    struct Fixture {
        enterprise: &'static str,
        pdf_calls: Cell<usize>,
        corrupt: bool,
        fail_pdf: bool,
    }
    #[cfg(feature = "network")]
    impl Source for Fixture {
        fn reference(&self, reference: &str) -> Result<FilingReference, NbbError> {
            Ok(serde_json::from_value(serde_json::json!({
                "ReferenceNumber": reference, "EnterpriseNumber": self.enterprise,
                "Currency":"EUR", "ExerciseDates":{"startDate":"2024-01-01","endDate":"2024-12-31"}
            }))
            .unwrap())
        }
        fn pdf(&self, reference: &str) -> Result<Capture, NbbError> {
            self.pdf_calls.set(self.pdf_calls.get() + 1);
            if self.fail_pdf {
                return Err(NbbError::Response("synthetic failure"));
            }
            let bytes = b"%PDF-1.7\nsynthetic fixture\n%%EOF".to_vec();
            Ok(Capture {
                sha256: if self.corrupt {
                    "bad".into()
                } else {
                    format!("{:x}", Sha256::digest(&bytes))
                },
                bytes,
                url: format!("https://ws.cbso.nbb.be/authentic/deposit/{reference}/accountingData"),
                request_id: Uuid::new_v4(),
                media_type: "application/pdf".into(),
                retrieved_at: "2026-01-02T10:00:00Z".into(),
            })
        }
    }
    #[cfg(feature = "network")]
    fn fixture() -> Fixture {
        Fixture {
            enterprise: "0123456789",
            pdf_calls: Cell::new(0),
            corrupt: false,
            fail_pdf: false,
        }
    }
    #[cfg(feature = "network")]
    struct Temp(std::path::PathBuf);
    #[cfg(feature = "network")]
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("nbb-bundle-test-{}", Uuid::new_v4()));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
        fn output(&self) -> std::path::PathBuf {
            self.0.join("bundle")
        }
    }
    #[cfg(feature = "network")]
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(feature = "network")]
    fn refs() -> Vec<String> {
        vec!["2025-1".into(), "2024-1".into()]
    }
    #[cfg(feature = "network")]
    fn rewrite(path: &Path, change: impl FnOnce(&mut serde_json::Value)) {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(path.join("manifest.json")).unwrap()).unwrap();
        change(&mut value);
        fs::write(
            path.join("manifest.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }
    #[cfg(feature = "network")]
    struct Structured {
        xml: &'static [u8],
        missing_second: bool,
    }
    #[cfg(feature = "network")]
    impl Source for Structured {
        fn reference(&self, reference: &str) -> Result<FilingReference, NbbError> {
            fixture().reference(reference)
        }
        fn pdf(&self, reference: &str) -> Result<Capture, NbbError> {
            fixture().pdf(reference)
        }
        fn xbrl(&self, reference: &str) -> Result<Capture, NbbError> {
            if self.missing_second && reference == "2024-1" {
                return Err(NbbError::Response("synthetic unavailable XBRL"));
            }
            let mut capture = fixture().pdf(reference)?;
            capture.bytes = self.xml.to_vec();
            capture.sha256 = format!("{:x}", Sha256::digest(self.xml));
            capture.media_type = "application/x.xbrl".into();
            Ok(capture)
        }
    }
    #[cfg(feature = "network")]
    const XML: &[u8] = br#"<?xml version="1.0"?><xbrl xmlns="http://www.xbrl.org/2003/instance"><context id="synthetic"/></xbrl>"#;
    #[cfg(feature = "network")]
    #[test]
    fn structured_roundtrip_and_corruption() {
        let t = Temp::new();
        let source = Structured {
            xml: XML,
            missing_second: false,
        };
        let manifest =
            create_bundle_with_xbrl(&source, "0123456789", &refs(), &t.output()).unwrap();
        assert_eq!(manifest.version, 2);
        assert_eq!(manifest.files.len(), 2);
        assert!(manifest.files.iter().all(|f| f.xbrl.is_some()));
        assert_eq!(verify_bundle(&t.output()).unwrap().version, 2);
        fs::write(t.output().join("2024-1.xbrl"), b"corrupt").unwrap();
        assert!(verify_bundle(&t.output()).is_err());
    }
    #[cfg(feature = "network")]
    #[test]
    fn structured_requires_every_filing_and_supported_source() {
        let t = Temp::new();
        let source = Structured {
            xml: XML,
            missing_second: true,
        };
        assert!(create_bundle_with_xbrl(&source, "0123456789", &refs(), &t.output()).is_err());
        assert!(!t.output().exists());
        assert!(create_bundle_with_xbrl(&fixture(), "0123456789", &refs(), &t.output()).is_err());
        assert!(!t.output().exists());
    }
    #[cfg(feature = "network")]
    #[test]
    fn rejects_invalid_xbrl_envelopes_and_receipts() {
        let bad: &[&[u8]] = &[
            b"<html/>",
            b"<xbrl/>",
            br#"<xbrl xmlns="urn:wrong"/>"#,
            br#"<other xmlns="http://www.xbrl.org/2003/instance"/>"#,
            br#"<!DOCTYPE xbrl><xbrl xmlns="http://www.xbrl.org/2003/instance"/>"#,
            br#"<xbrl xmlns="http://www.xbrl.org/2003/instance"><a></xbrl>"#,
            br#"<xbrl xmlns="http://www.xbrl.org/2003/instance">"#,
            br#"<xbrl xmlns="http://www.xbrl.org/2003/instance"/><extra/>"#,
            br#"<xbrl xmlns="http://www.xbrl.org/2003/instance"/>&amp;"#,
        ];
        let source = Structured {
            xml: XML,
            missing_second: false,
        };
        let capture = source.xbrl("2025-1").unwrap();
        for bytes in bad {
            let mut receipt = capture_receipt(&capture);
            receipt.bytes = bytes.len() as u64;
            receipt.sha256 = format!("{:x}", Sha256::digest(bytes));
            assert!(check_xbrl("2025-1", &receipt, bytes).is_err());
        }
        let mut receipt = capture_receipt(&capture);
        receipt.media_type = "application/xml".into();
        assert!(check_xbrl("2025-1", &receipt, XML).is_err());
        let receipt = capture_receipt(&capture);
        assert!(check_xbrl("2024-1", &receipt, XML).is_err());
        let prefixed = br#"<i:xbrl xmlns:i="http://www.xbrl.org/2003/instance"/>"#;
        let mut receipt = receipt;
        receipt.bytes = prefixed.len() as u64;
        receipt.sha256 = format!("{:x}", Sha256::digest(prefixed));
        assert!(check_xbrl("2025-1", &receipt, prefixed).is_ok());
    }
    #[cfg(feature = "network")]
    #[test]
    fn structured_manifest_requires_all_pairs_and_safe_paths() {
        for change in [
            (|v: &mut serde_json::Value| {
                v["files"][1].as_object_mut().unwrap().remove("xbrl");
            }) as fn(&mut serde_json::Value),
            |v| v["files"][0]["xbrl"]["filename"] = "../outside.xbrl".into(),
            |v| v["version"] = 1.into(),
        ] {
            let t = Temp::new();
            let source = Structured {
                xml: XML,
                missing_second: false,
            };
            create_bundle_with_xbrl(&source, "0123456789", &refs(), &t.output()).unwrap();
            rewrite(&t.output(), change);
            assert!(verify_bundle(&t.output()).is_err());
        }
    }
    #[cfg(feature = "network")]
    #[test]
    fn roundtrip_and_corrupt_pdf() {
        let t = Temp::new();
        let source = fixture();
        let manifest = create_bundle(&source, "0123456789", &refs(), &t.output()).unwrap();
        assert_eq!(manifest.files.len(), 2);
        assert_eq!(verify_bundle(&t.output()).unwrap().files.len(), 2);
        assert_eq!(source.pdf_calls.get(), 2);
        fs::write(t.output().join("2025-1.pdf"), b"%PDF-tampered").unwrap();
        assert!(verify_bundle(&t.output()).is_err());
    }
    #[cfg(feature = "network")]
    #[test]
    fn failed_acquisition_cleans_reservation_and_checks_metadata_first() {
        for source in [
            Fixture {
                enterprise: "9999999999",
                ..fixture()
            },
            Fixture {
                corrupt: true,
                ..fixture()
            },
            Fixture {
                fail_pdf: true,
                ..fixture()
            },
        ] {
            let t = Temp::new();
            assert!(create_bundle(&source, "0123456789", &refs(), &t.output()).is_err());
            assert!(!t.output().exists());
            if source.enterprise != "0123456789" {
                assert_eq!(source.pdf_calls.get(), 0);
            }
        }
    }
    #[cfg(feature = "network")]
    #[test]
    fn duplicate_and_existing_outputs_never_acquire_or_overwrite() {
        let t = Temp::new();
        let source = fixture();
        assert!(create_bundle(
            &source,
            "0123456789",
            &["1".into(), "1".into()],
            &t.output()
        )
        .is_err());
        assert!(!t.output().exists());
        fs::write(t.output(), b"keep").unwrap();
        assert!(create_bundle(&source, "0123456789", &refs(), &t.output()).is_err());
        assert_eq!(fs::read(t.output()).unwrap(), b"keep");
        fs::remove_file(t.output()).unwrap();
        fs::create_dir(t.output()).unwrap();
        assert!(create_bundle(&source, "0123456789", &refs(), &t.output()).is_err());
        assert!(t.output().is_dir());
        assert_eq!(source.pdf_calls.get(), 0);
        assert!(verify_bundle(&t.output()).is_err());
    }
    #[cfg(feature = "network")]
    #[test]
    fn malformed_manifests_are_rejected() {
        let changes: Vec<fn(&mut serde_json::Value)> = vec![
            |v| v["version"] = 2.into(),
            |v| v["enterprise"] = "9999999999".into(),
            |v| v["files"][0]["filename"] = "../2025-1.pdf".into(),
            |v| v["files"][0]["period"]["start"] = "2023-01-01".into(),
            |v| v["files"][0]["metadata"]["Currency"] = "USD".into(),
            |v| v["files"][0]["metadata"]["ExerciseDates"]["endDate"] = "2023-01-01".into(),
            |v| v["files"][0]["receipt"]["bytes"] = 0.into(),
            |v| v["files"][1] = v["files"][0].clone(),
        ];
        for change in changes {
            let t = Temp::new();
            create_bundle(&fixture(), "0123456789", &refs(), &t.output()).unwrap();
            rewrite(&t.output(), change);
            assert!(verify_bundle(&t.output()).is_err());
        }
    }
    #[cfg(unix)]
    #[cfg(feature = "network")]
    #[test]
    fn symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let t = Temp::new();
        let source = fixture();
        symlink(t.0.join("missing"), t.output()).unwrap();
        assert!(create_bundle(&source, "0123456789", &refs(), &t.output()).is_err());
        assert_eq!(source.pdf_calls.get(), 0);
        fs::remove_file(t.output()).unwrap();
        create_bundle(&source, "0123456789", &refs(), &t.output()).unwrap();
        let pdf = t.output().join("2025-1.pdf");
        let outside = t.0.join("outside.pdf");
        fs::rename(&pdf, &outside).unwrap();
        symlink(&outside, &pdf).unwrap();
        assert!(verify_bundle(&t.output()).is_err());
        let manifest = t.output().join("manifest.json");
        let outside_manifest = t.0.join("outside.json");
        fs::rename(&manifest, &outside_manifest).unwrap();
        symlink(&outside_manifest, &manifest).unwrap();
        assert!(verify_bundle(&t.output()).is_err());
        let link = t.0.join("bundle-link");
        symlink(t.output(), &link).unwrap();
        assert!(verify_bundle(&link).is_err());
    }
}
