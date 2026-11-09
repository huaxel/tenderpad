//! Read-only dossier attachments. Signed URLs never leave memory or receive OAuth headers.
#[cfg(feature = "network")]
use super::BosaClient;
use super::BosaError;
#[cfg(feature = "network")]
use super::Environment;
use serde::Serialize;
use serde_json::Value;
#[cfg(feature = "network")]
use sha2::{Digest, Sha256};
#[cfg(feature = "network")]
use std::fs::{self, OpenOptions};
#[cfg(feature = "network")]
use std::io::{Read, Write};
#[cfg(feature = "network")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "network")]
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const MAX_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumStatus {
    Verified,
    Unknown,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub version_id: Uuid,
    pub original_name: String,
    pub sha256: String,
    pub sha256_verified: bool,
    pub checksum_status: ChecksumStatus,
}

#[derive(Debug)]
pub struct XmlAttachmentBytes {
    pub version_id: Uuid,
    pub original_name: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub sha256_verified: bool,
}

#[derive(Debug, Serialize)]
pub struct DownloadedAttachment {
    pub version_id: Uuid,
    pub path: PathBuf,
    pub bytes: u64,
    /// Digest of the bytes written locally; source verification is separate.
    pub sha256: String,
    pub sha256_verified: bool,
    pub checksum_status: ChecksumStatus,
}

fn invalid(message: &str) -> BosaError {
    BosaError::InvalidResponse(message.into())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn parse_attachments(value: &Value) -> Result<Vec<Attachment>, BosaError> {
    let documents = value
        .as_array()
        .ok_or_else(|| invalid("documents response is not an array"))?;
    let mut result = Vec::new();
    for document in documents {
        let versions = document
            .get("versions")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("document has no versions"))?;
        // Ignore unactivated draft versions. Published list responses may include history.
        let latest = versions
            .iter()
            .filter(|v| v.get("activatedAt").and_then(Value::as_str).is_some())
            .max_by_key(|v| v.get("createdAt").and_then(Value::as_str).unwrap_or(""))
            .ok_or_else(|| invalid("document has no activated version"))?;
        let id = latest
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| invalid("invalid document version UUID"))?;
        let metadata = &latest["document"];
        let (sha256, sha256_verified, checksum_status) = match metadata.get("fileHash") {
            None | Some(Value::Null) => (String::new(), false, ChecksumStatus::Missing),
            Some(Value::String(hash)) if is_sha256(hash) => {
                (hash.to_ascii_lowercase(), true, ChecksumStatus::Verified)
            }
            Some(Value::String(hash)) if hash.eq_ignore_ascii_case("UNKNOWN HASH") => {
                (hash.to_owned(), false, ChecksumStatus::Unknown)
            }
            Some(_) => (String::new(), false, ChecksumStatus::Invalid),
        };
        let name = metadata["originalFileName"]
            .as_str()
            .ok_or_else(|| invalid("attachment has no filename"))?;
        result.push(Attachment {
            version_id: id,
            original_name: name.to_owned(),
            sha256,
            sha256_verified,
            checksum_status,
        });
    }
    Ok(result)
}

#[cfg(feature = "network")]
fn validate_download_url(value: &str, environment: Environment) -> Result<url::Url, BosaError> {
    let url = url::Url::parse(value).map_err(|_| invalid("invalid signed download URL"))?;
    // Only exact environment-specific object-store hosts observed in signed API
    // responses and verified with TLS are enabled. Never allow arbitrary
    // publicprocurement subdomains or user-supplied destinations.
    let expected_host = match environment {
        Environment::Int => "minio-int.publicprocurement.be",
        Environment::Prod => "minio-prd.publicprocurement.be",
    };
    if url.scheme() != "https"
        || url.host_str() != Some(expected_host)
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "download destination is not the verified production object store",
        ));
    }
    Ok(url)
}

#[cfg(feature = "network")]
struct PartialFile(PathBuf);
#[cfg(feature = "network")]
impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(feature = "network")]
fn unverified_download_allowed(attachment: &Attachment) -> bool {
    !attachment.sha256_verified
        && matches!(
            attachment.checksum_status,
            ChecksumStatus::Unknown | ChecksumStatus::Missing
        )
}

#[cfg(feature = "network")]
fn copy_attachment(
    mut input: impl Read,
    directory: &Path,
    attachment: &Attachment,
    limit: u64,
    allow_unverified: bool,
) -> Result<DownloadedAttachment, BosaError> {
    let checksum_verified = attachment.sha256_verified
        && attachment.checksum_status == ChecksumStatus::Verified
        && is_sha256(&attachment.sha256);
    if !(checksum_verified || allow_unverified && unverified_download_allowed(attachment)) {
        return Err(invalid("attachment checksum is not eligible for download"));
    }
    if limit == 0 || limit > MAX_ATTACHMENT_BYTES {
        return Err(invalid("attachment limit must be 1..32 MiB"));
    }
    fs::create_dir_all(directory).map_err(|_| invalid("cannot create attachment directory"))?;
    let metadata = fs::symlink_metadata(directory)
        .map_err(|_| invalid("cannot inspect attachment directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid(
            "attachment directory must be a real directory, not a symlink",
        ));
    }
    // Never use the remote filename in a filesystem path.
    let extension = Path::new(&attachment.original_name)
        .extension()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("bin")
        .to_ascii_lowercase();
    let destination = directory.join(format!("{}.{extension}", attachment.version_id));
    let partial_path = directory.join(format!(".{}.part", Uuid::new_v4()));
    // Declare the guard first so the file handle drops before cleanup on error.
    let partial;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial_path)
        .map_err(|_| invalid("cannot create temporary attachment file"))?;
    partial = PartialFile(partial_path);
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let started = Instant::now();
    let mut buffer = [0; 8192];
    loop {
        if started.elapsed() > Duration::from_secs(120) {
            return Err(invalid("attachment download exceeded time budget"));
        }
        let read = input
            .read(&mut buffer)
            .map_err(|_| invalid("attachment read failed"))?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        if bytes > limit {
            return Err(invalid("attachment exceeds byte limit"));
        }
        hash.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|_| invalid("attachment write failed"))?;
    }
    let actual = format!("{:x}", hash.finalize());
    if checksum_verified && actual != attachment.sha256 {
        return Err(invalid("attachment SHA256 does not match metadata"));
    }
    output
        .sync_all()
        .map_err(|_| invalid("cannot sync attachment file"))?;
    drop(output);
    // Atomic, no-overwrite publication: existing files and symlinks are never replaced.
    fs::hard_link(&partial.0, &destination)
        .map_err(|_| invalid("cannot publish attachment (destination may already exist)"))?;
    Ok(DownloadedAttachment {
        version_id: attachment.version_id,
        path: destination,
        bytes,
        sha256: actual,
        sha256_verified: checksum_verified,
        checksum_status: attachment.checksum_status,
    })
}

#[cfg(feature = "network")]
#[cfg(test)]
fn copy_verified(
    input: impl Read,
    directory: &Path,
    attachment: &Attachment,
    limit: u64,
) -> Result<DownloadedAttachment, BosaError> {
    copy_attachment(input, directory, attachment, limit, false)
}

#[cfg(feature = "network")]
impl BosaClient {
    pub fn attachments(&mut self, workspace_id: &str) -> Result<Vec<Attachment>, BosaError> {
        let id =
            Uuid::parse_str(workspace_id).map_err(|_| invalid("workspace ID must be a UUID"))?;
        let value = self.get(
            &format!("/api/eProcurementDos/v1/publication-workspaces/{id}/documents"),
            &[],
        )?;
        parse_attachments(&value)
    }

    /// Download XML in memory with a hard size/time bound. Unknown checksums are
    /// accepted only when the caller explicitly opts in; publication/reference
    /// validation remains the caller's responsibility before parsing or storing.
    /// Fetch a pre-signed object-store URL, retried under the shared policy.
    ///
    /// The GET carries no credential (the signature is in the URL) and has no
    /// side effect, so a transient object-store failure is worth another
    /// attempt. Any final failure still withholds the signed URL from the
    /// message, except for the shared ledger's own refusal, which names no URL
    /// and is what an operator needs to see.
    fn download_signed(&self, url: &url::Url) -> Result<ureq::Response, BosaError> {
        let outcome = crate::http_retry::send_with_retry(
            &crate::http_retry::RetryPolicy::bosa(),
            || {
                crate::service_calls::http(
                    self.audit_service(),
                    crate::service_calls::Operation::BosaDownload,
                    || self.agent.get(url.as_str()).call(),
                )
                .map_err(|e| crate::http_retry::Attempt::Guard(e.to_string()))
                .and_then(crate::http_retry::Attempt::classify)
            },
            crate::http_retry::Attempt::signal,
        );
        match outcome {
            Ok(response) => Ok(response),
            Err(stop) => Err(super::bosa_attempt_error(stop.into_error())),
        }
    }

    pub fn download_xml_attachment_bytes(
        &mut self,
        attachment: &Attachment,
        environment: Environment,
        allow_unverified: bool,
    ) -> Result<XmlAttachmentBytes, BosaError> {
        if Path::new(&attachment.original_name)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("xml"))
        {
            return Err(invalid("unverified attachment fallback is XML-only"));
        }
        match attachment.checksum_status {
            ChecksumStatus::Verified
                if attachment.sha256_verified && is_sha256(&attachment.sha256) => {}
            ChecksumStatus::Unknown
                if !attachment.sha256_verified
                    && allow_unverified
                    && attachment.sha256.eq_ignore_ascii_case("UNKNOWN HASH") => {}
            _ => return Err(invalid("unverified XML fallback was not enabled")),
        }
        let expected_origin = match environment {
            Environment::Int => "https://public.int.fedservices.be",
            Environment::Prod => "https://public.pr.fedservices.be",
        };
        if self.config.api_base_url.trim_end_matches('/') != expected_origin {
            return Err(invalid(
                "attachment downloads require the selected verified gateway",
            ));
        }
        let value = self.get(
            &format!(
                "/api/eProcurementDos/v1/publication-workspace-document-versions/{}/download-url",
                attachment.version_id
            ),
            &[("unpublished".into(), "false".into())],
        )?;
        let signed_url = value
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing signed download URL"))?;
        let url = validate_download_url(signed_url, environment)?;
        // Deliberately NO Authorization header; the exact-host URL is fetched
        // directly and the shared agent has redirects disabled.
        let response = match self.download_signed(&url) {
            Ok(response) => response,
            Err(BosaError::CallGuard(message)) => return Err(BosaError::CallGuard(message)),
            Err(_) => return Err(invalid("object-store download failed; signed URL withheld")),
        };
        if response.status() != 200 {
            return Err(invalid("object store did not return 200"));
        }
        if response
            .header("Content-Length")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|bytes| bytes > MAX_ATTACHMENT_BYTES)
        {
            return Err(invalid("attachment exceeds byte limit"));
        }
        let mut reader = response.into_reader();
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        let started = Instant::now();
        loop {
            if started.elapsed() > Duration::from_secs(120) {
                return Err(invalid("attachment download exceeded time budget"));
            }
            let read = reader
                .read(&mut buffer)
                .map_err(|_| invalid("attachment read failed"))?;
            if read == 0 {
                break;
            }
            if bytes.len() as u64 + read as u64 > MAX_ATTACHMENT_BYTES {
                return Err(invalid("attachment exceeds byte limit"));
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if attachment.checksum_status == ChecksumStatus::Verified && sha256 != attachment.sha256 {
            return Err(invalid("attachment SHA256 does not match metadata"));
        }
        Ok(XmlAttachmentBytes {
            version_id: attachment.version_id,
            original_name: attachment.original_name.clone(),
            bytes,
            sha256,
            sha256_verified: attachment.sha256_verified,
        })
    }

    pub fn download_attachment(
        &mut self,
        attachment: &Attachment,
        environment: Environment,
        directory: &Path,
        limit: u64,
    ) -> Result<DownloadedAttachment, BosaError> {
        self.download_attachment_with_policy(attachment, environment, directory, limit, false)
    }

    /// Download an attachment whose source checksum is missing or explicitly unknown.
    /// The local digest is recorded, but it is not represented as source-verified.
    pub fn download_attachment_unverified(
        &mut self,
        attachment: &Attachment,
        environment: Environment,
        directory: &Path,
        limit: u64,
    ) -> Result<DownloadedAttachment, BosaError> {
        if !unverified_download_allowed(attachment) {
            return Err(invalid(
                "unverified download is limited to missing or unknown checksums",
            ));
        }
        self.download_attachment_with_policy(attachment, environment, directory, limit, true)
    }

    fn download_attachment_with_policy(
        &mut self,
        attachment: &Attachment,
        environment: Environment,
        directory: &Path,
        limit: u64,
        allow_unverified: bool,
    ) -> Result<DownloadedAttachment, BosaError> {
        let checksum_verified = attachment.sha256_verified
            && attachment.checksum_status == ChecksumStatus::Verified
            && is_sha256(&attachment.sha256);
        if !(checksum_verified || allow_unverified && unverified_download_allowed(attachment)) {
            return Err(invalid("attachment checksum is not eligible for download"));
        }
        if limit == 0 || limit > MAX_ATTACHMENT_BYTES {
            return Err(invalid("attachment limit must be 1..32 MiB"));
        }
        // Verify the selected gateway before requesting a signed URL.
        let expected_origin = match environment {
            Environment::Int => "https://public.int.fedservices.be",
            Environment::Prod => "https://public.pr.fedservices.be",
        };
        if self.config.api_base_url.trim_end_matches('/') != expected_origin {
            return Err(invalid(
                "attachment downloads require the selected verified gateway",
            ));
        }
        let value = self.get(
            &format!(
                "/api/eProcurementDos/v1/publication-workspace-document-versions/{}/download-url",
                attachment.version_id
            ),
            &[("unpublished".into(), "false".into())],
        )?;
        let url = value
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing signed download URL"))?;
        let url = validate_download_url(url, environment)?;
        // Deliberately NO Authorization header here. Redirects are disabled on the agent.
        let response = match self.download_signed(&url) {
            Ok(response) => response,
            Err(BosaError::CallGuard(message)) => return Err(BosaError::CallGuard(message)),
            Err(_) => return Err(invalid("object-store download failed; signed URL withheld")),
        };
        if response.status() != 200 {
            return Err(invalid("object store did not return 200"));
        }
        if response
            .header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok())
            .is_some_and(|n| n > limit)
        {
            return Err(invalid("attachment exceeds byte limit"));
        }
        copy_attachment(
            response.into_reader(),
            directory,
            attachment,
            limit,
            allow_unverified,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "network")]
    use sha2::{Digest, Sha256};
    #[cfg(feature = "network")]
    fn attachment(data: &[u8]) -> Attachment {
        Attachment {
            version_id: Uuid::new_v4(),
            original_name: "../../unsafe.pdf".into(),
            sha256: format!("{:x}", Sha256::digest(data)),
            sha256_verified: true,
            checksum_status: ChecksumStatus::Verified,
        }
    }
    #[cfg(feature = "network")]
    #[test]
    fn signed_url_policy_rejects_ssrf_and_cross_environment_targets() {
        let good_prod = "https://minio-prd.publicprocurement.be/bucket/file?signature=secret";
        let good_int = "https://minio-int.publicprocurement.be/bucket/file?signature=secret";
        assert!(validate_download_url(good_prod, Environment::Prod).is_ok());
        assert!(validate_download_url(good_int, Environment::Int).is_ok());
        assert!(validate_download_url(good_prod, Environment::Int).is_err());
        assert!(validate_download_url(good_int, Environment::Prod).is_err());
        for bad in [
            "http://minio-prd.publicprocurement.be/file",
            "https://127.0.0.1/file",
            "https://minio-prd.publicprocurement.be.evil.test/file",
            "https://evil.test/file",
            "https://user@minio-prd.publicprocurement.be/file",
            "https://minio-prd.publicprocurement.be:444/file",
            "https://minio-prd.publicprocurement.be/file#fragment",
            "https://minio-int.publicprocurement.be.evil.test/file",
        ] {
            assert!(
                validate_download_url(bad, Environment::Prod).is_err(),
                "{bad}"
            );
        }
    }
    #[cfg(feature = "network")]
    #[test]
    fn downloads_are_bounded_verified_and_never_overwrite() {
        let directory = std::env::temp_dir().join(format!("bosa-doc-test-{}", Uuid::new_v4()));
        let data = b"%PDF-test";
        let a = attachment(data);
        let file = copy_verified(data.as_slice(), &directory, &a, 100).unwrap();
        assert_eq!(file.path.parent(), Some(directory.as_path()));
        assert_eq!(fs::read(&file.path).unwrap(), data);
        assert!(copy_verified(data.as_slice(), &directory, &a, 100).is_err());
        assert!(copy_verified(data.as_slice(), &directory, &attachment(data), 2).is_err());
        assert!(copy_verified(b"wrong".as_slice(), &directory, &attachment(data), 100).is_err());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
    #[cfg(feature = "network")]
    #[test]
    fn opt_in_unverified_copy_records_local_hash_without_claiming_source_verification() {
        let data = b"unverified but bounded";
        let directory =
            std::env::temp_dir().join(format!("bosa-unverified-opt-in-{}", Uuid::new_v4()));
        let missing = Attachment {
            version_id: Uuid::new_v4(),
            original_name: "specification.pdf".into(),
            sha256: String::new(),
            sha256_verified: false,
            checksum_status: ChecksumStatus::Missing,
        };
        assert!(copy_attachment(data.as_slice(), &directory, &missing, 100, false).is_err());
        assert!(!directory.exists());
        let downloaded = copy_attachment(data.as_slice(), &directory, &missing, 100, true).unwrap();
        assert_eq!(fs::read(&downloaded.path).unwrap(), data);
        assert_eq!(downloaded.sha256, format!("{:x}", Sha256::digest(data)));
        assert!(!downloaded.sha256_verified);
        assert_eq!(downloaded.checksum_status, ChecksumStatus::Missing);

        let unknown = Attachment {
            version_id: Uuid::new_v4(),
            original_name: "unknown.pdf".into(),
            sha256: "UNKNOWN HASH".into(),
            sha256_verified: false,
            checksum_status: ChecksumStatus::Unknown,
        };
        let downloaded_unknown =
            copy_attachment(data.as_slice(), &directory, &unknown, 100, true).unwrap();
        assert!(!downloaded_unknown.sha256_verified);
        assert_eq!(downloaded_unknown.checksum_status, ChecksumStatus::Unknown);

        let invalid = Attachment {
            checksum_status: ChecksumStatus::Invalid,
            sha256: String::new(),
            ..missing.clone()
        };
        assert!(copy_attachment(data.as_slice(), &directory, &invalid, 100, true).is_err());
        let mut client = BosaClient::new(super::super::BosaConfig {
            token_url: "https://invalid.example/token".into(),
            api_base_url: "https://public.pr.fedservices.be".into(),
            client_id: "test".into(),
            client_secret: "test".into(),
        });
        assert!(client
            .download_attachment(&missing, Environment::Prod, &directory, 100)
            .is_err());
        assert!(client
            .download_attachment_unverified(&invalid, Environment::Prod, &directory, 100)
            .is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(feature = "network")]
    #[test]
    fn unknown_hash_is_unverified_and_never_enters_the_verified_file_path() {
        let id = Uuid::new_v4();
        let value = serde_json::json!([{"versions":[{
            "id":id,
            "createdAt":"2017-07-04",
            "activatedAt":"2017-07-04",
            "document":{"fileHash":"UNKNOWN HASH","originalFileName":"notice.xml"}
        }]}]);
        let parsed = parse_attachments(&value).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(!parsed[0].sha256_verified);
        assert_eq!(parsed[0].sha256, "UNKNOWN HASH");
        assert_eq!(parsed[0].checksum_status, ChecksumStatus::Unknown);

        let directory = std::env::temp_dir().join(format!("bosa-unverified-{}", Uuid::new_v4()));
        assert!(copy_verified(b"<xml/>".as_slice(), &directory, &parsed[0], 100).is_err());
        assert!(!directory.exists());

        let mut client = BosaClient::new(super::super::BosaConfig {
            token_url: "https://invalid.example/token".into(),
            api_base_url: "https://public.pr.fedservices.be".into(),
            client_id: "test".into(),
            client_secret: "test".into(),
        });
        assert!(client
            .download_xml_attachment_bytes(&parsed[0], Environment::Prod, false)
            .is_err());
        let pdf = Attachment {
            original_name: "notice.pdf".into(),
            ..parsed[0].clone()
        };
        assert!(client
            .download_xml_attachment_bytes(&pdf, Environment::Prod, true)
            .is_err());
    }

    #[cfg(feature = "network")]
    #[test]
    fn mixed_checksum_metadata_keeps_verified_siblings_and_blocks_unverified_downloads() {
        let valid_id = Uuid::new_v4();
        let missing_id = Uuid::new_v4();
        let invalid_id = Uuid::new_v4();
        let value = serde_json::json!([
            {"versions":[{"id":valid_id,"createdAt":"2026-10-01","activatedAt":"2026-10-02","document":{"fileHash":"A".repeat(64),"originalFileName":"verified.pdf"}}]},
            {"versions":[{"id":missing_id,"createdAt":"2026-10-01","activatedAt":"2026-10-02","document":{"originalFileName":"missing.xml"}}]},
            {"versions":[{"id":invalid_id,"createdAt":"2026-10-01","activatedAt":"2026-10-02","document":{"fileHash":"not-a-hash","originalFileName":"invalid.xml"}}]}
        ]);
        let parsed = parse_attachments(&value).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].version_id, valid_id);
        assert_eq!(parsed[0].checksum_status, ChecksumStatus::Verified);
        assert!(parsed[0].sha256_verified);
        assert_eq!(parsed[0].sha256, "a".repeat(64));
        assert_eq!(parsed[1].checksum_status, ChecksumStatus::Missing);
        assert_eq!(parsed[2].checksum_status, ChecksumStatus::Invalid);
        assert!(parsed[1..]
            .iter()
            .all(|attachment| !attachment.sha256_verified));

        let mut client = BosaClient::new(super::super::BosaConfig {
            token_url: "https://invalid.example/token".into(),
            api_base_url: "https://public.pr.fedservices.be".into(),
            client_id: "test".into(),
            client_secret: "test".into(),
        });
        let directory = std::env::temp_dir().join(format!("bosa-unverified-{}", Uuid::new_v4()));
        for attachment in &parsed[1..] {
            assert!(copy_verified(b"untrusted".as_slice(), &directory, attachment, 100).is_err());
            assert!(client
                .download_attachment(attachment, Environment::Prod, &directory, 100)
                .is_err());
            assert!(client
                .download_xml_attachment_bytes(attachment, Environment::Prod, true)
                .is_err());
        }
        assert!(!directory.exists());
    }

    #[test]
    fn metadata_chooses_latest_activated_version_and_validates_hash() {
        let id = Uuid::new_v4();
        let value = serde_json::json!([{"versions":[
            {"id":id,"createdAt":"2026-10-01","activatedAt":"2026-10-02","document":{"fileHash":"a".repeat(64),"originalFileName":"safe.pdf"}},
            {"id":Uuid::new_v4(),"createdAt":"2026-11-01","document":{"originalFileName":"draft.pdf"}}
        ]}]);
        let parsed = parse_attachments(&value).unwrap();
        assert_eq!(parsed[0].version_id, id);
        assert_eq!(parsed[0].checksum_status, ChecksumStatus::Verified);
        let mut invalid = value;
        invalid[0]["versions"][0]["document"]["fileHash"] = "bad".into();
        let parsed = parse_attachments(&invalid).unwrap();
        assert_eq!(parsed[0].checksum_status, ChecksumStatus::Invalid);
        assert!(!parsed[0].sha256_verified);
    }
}
