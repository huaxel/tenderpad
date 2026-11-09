//! Conservative, provenance-bearing signals from previously extracted PDF text.
//! This module never decodes documents or runs an external parser.
use crate::{Tender, TenderRequirement};
use serde::{Deserialize, Serialize};
use std::error::Error;
use uuid::Uuid;

const MAX_PAGES: usize = 500;
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SIGNALS_PER_DOCUMENT: usize = 250;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentEvidence {
    pub version_id: Uuid,
    pub original_name: String,
    pub sha256: String,
    pub page_count: usize,
    pub signals: Vec<TenderRequirement>,
}

/// Keyword hits are prompts for review, not verified eligibility requirements.
pub fn extract_evidence(
    version_id: Uuid,
    name: &str,
    sha256: &str,
    text: &str,
) -> Result<AttachmentEvidence, Box<dyn Error>> {
    if text.len() > MAX_TEXT_BYTES {
        return Err("extracted text exceeds 2 MiB".into());
    }
    if sha256.len() != 64 || !sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid attachment SHA256".into());
    }
    if name.len() > 1000 || name.chars().any(char::is_control) {
        return Err("invalid attachment name".into());
    }
    let pages: Vec<_> = text.split('\x0c').collect();
    let pages = if pages.last().is_some_and(|p| p.trim().is_empty()) {
        &pages[..pages.len() - 1]
    } else {
        &pages[..]
    };
    if pages.len() > MAX_PAGES {
        return Err("extracted text exceeds 500 pages".into());
    }
    if text
        .trim_matches(|c: char| c.is_whitespace() || c == '\x0c')
        .is_empty()
    {
        return Err("document has no extractable text; OCR not performed".into());
    }
    let terms = [
        "certificat",
        "attestation",
        "chiffre d'affaires",
        "chiffre d’affaires",
        "omzet",
        "turnover",
        "référence",
        "reference",
        "erkenning",
        "agréation",
        "agreation",
        "assurance professionnelle",
        "beroepsverzekering",
        "garantie bancaire",
        "bankgarantie",
    ];
    let mut signals = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        for line in page.lines() {
            let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
            let lower = line.to_lowercase();
            if !terms.iter().any(|term| lower.contains(term)) {
                continue;
            }
            if line.len() > 1500 {
                return Err("requirement line exceeds 1500 bytes; review document manually".into());
            }
            if signals.iter().any(|s: &TenderRequirement| {
                s.text == line && s.source_page == Some(index as u32 + 1)
            }) {
                continue;
            }
            if signals.len() == MAX_SIGNALS_PER_DOCUMENT {
                return Err("more than 250 requirement signals; review document manually".into());
            }
            signals.push(TenderRequirement {
                text: line,
                kind: "attachment_hint".into(),
                mandatory: false,
                source_page: Some(index as u32 + 1),
            });
        }
    }
    Ok(AttachmentEvidence {
        version_id,
        original_name: name.into(),
        sha256: sha256.to_ascii_lowercase(),
        page_count: pages.len(),
        signals,
    })
}

/// Replace evidence for selected version IDs, preserving notice requirements and
/// other attachment evidence. Raw per-document records preserve provenance even
/// when identical text appears in several files.
pub fn replace_evidence(
    tender: &mut Tender,
    evidence: &[AttachmentEvidence],
) -> Result<usize, Box<dyn Error>> {
    let existing = tender
        .raw
        .get("bosa_attachment_evidence")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let mut records: Vec<AttachmentEvidence> = serde_json::from_value(existing)?;
    for incoming in evidence {
        records.retain(|record| record.version_id != incoming.version_id);
        records.push(incoming.clone());
    }
    // Keyword hits are unreviewed evidence, not eligibility requirements;
    // keep them out of ranking and warnings until a person evaluates them.
    let count = records.iter().map(|r| r.signals.len()).sum();
    tender.raw["bosa_attachment_evidence"] = serde_json::to_value(records)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn page_and_provenance_are_preserved_and_hits_are_not_mandatory() {
        let id = Uuid::new_v4();
        let evidence = extract_evidence(
            id,
            "bestek.pdf",
            &"a".repeat(64),
            "heading\nCertificat exigé\n\x0cOmzet minimum EUR 50 000\n\x0c",
        )
        .unwrap();
        assert_eq!(evidence.page_count, 2);
        assert_eq!(evidence.signals.len(), 2);
        assert_eq!(evidence.signals[1].source_page, Some(2));
        assert!(evidence.signals.iter().all(|s| !s.mandatory));
        let mut t = Tender {
            requirements: vec![TenderRequirement {
                text: "Notice requirement".into(),
                ..Default::default()
            }],
            raw: serde_json::json!({}),
            ..Default::default()
        };
        assert_eq!(
            replace_evidence(&mut t, std::slice::from_ref(&evidence)).unwrap(),
            2
        );
        assert_eq!(
            replace_evidence(&mut t, std::slice::from_ref(&evidence)).unwrap(),
            2
        );
        assert_eq!(t.requirements.len(), 1);
        assert_eq!(
            t.raw["bosa_attachment_evidence"][0]["version_id"],
            id.to_string()
        );
    }
    #[test]
    fn rejects_empty_excess_and_bad_identity() {
        let id = Uuid::new_v4();
        assert!(extract_evidence(id, "file.pdf", &"b".repeat(64), "\x0c\n").is_err());
        assert!(extract_evidence(id, "file.pdf", "bad", "Certificat").is_err());
        assert!(extract_evidence(
            id,
            "file.pdf",
            &"b".repeat(64),
            &"x".repeat(MAX_TEXT_BYTES + 1)
        )
        .is_err());
    }
}
