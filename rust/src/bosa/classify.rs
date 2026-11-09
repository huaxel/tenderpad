//! Offline file-kind and text-layer classification for downloaded attachments.
//!
//! This is the first stage of the attachment strategy (`docs/attachment-strategy.md`):
//! it decides, from the bytes alone, what a file is and whether a PDF carries a
//! text layer. It never decodes document content, runs no external parser and
//! makes no network call. Extensions are not trusted; they are only compared
//! against the sniffed kind.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Cursor;

/// Zip directories are only read by name; this bounds the work on hostile files.
const MAX_ZIP_ENTRIES_INSPECTED: usize = 10_000;
/// A page needs at least this many alphanumeric characters to count as text.
/// Scanned pages usually yield only form feeds or a few stray glyphs.
const MIN_ALPHANUMERIC_PER_TEXT_PAGE: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    /// OLE2 container: legacy .doc/.xls/.ppt. Not distinguished further.
    LegacyOffice,
    /// A zip that is not recognisable OOXML; members are never expanded here.
    Zip,
    Jpeg,
    Png,
    Tiff,
    Unknown,
}

impl FileKind {
    /// Extensions a file of this kind legitimately carries (lowercase, no dot).
    fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Pdf => &["pdf"],
            Self::Docx => &["docx", "docm"],
            Self::Xlsx => &["xlsx", "xlsm"],
            Self::Pptx => &["pptx"],
            Self::LegacyOffice => &["doc", "xls", "ppt"],
            Self::Zip => &["zip"],
            Self::Jpeg => &["jpg", "jpeg"],
            Self::Png => &["png"],
            Self::Tiff => &["tif", "tiff"],
            Self::Unknown => &[],
        }
    }
}

/// Classify by magic bytes; zip containers are told apart by member *names* only.
pub fn sniff(bytes: &[u8]) -> FileKind {
    if bytes.starts_with(b"%PDF-") {
        return FileKind::Pdf;
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return FileKind::Jpeg;
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return FileKind::Png;
    }
    if bytes.starts_with(&[b'I', b'I', 0x2A, 0x00]) || bytes.starts_with(&[b'M', b'M', 0x00, 0x2A])
    {
        return FileKind::Tiff;
    }
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return FileKind::LegacyOffice;
    }
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return sniff_zip(bytes);
    }
    FileKind::Unknown
}

fn sniff_zip(bytes: &[u8]) -> FileKind {
    let Ok(archive) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return FileKind::Unknown;
    };
    let mut content_types = false;
    let (mut word, mut xl, mut ppt) = (false, false, false);
    for name in archive
        .file_names()
        .take(MAX_ZIP_ENTRIES_INSPECTED)
        .filter_map(Result::ok)
    {
        content_types |= name == "[Content_Types].xml";
        word |= name.starts_with("word/");
        xl |= name.starts_with("xl/");
        ppt |= name.starts_with("ppt/");
    }
    match (content_types, word, xl, ppt) {
        (true, true, false, false) => FileKind::Docx,
        (true, false, true, false) => FileKind::Xlsx,
        (true, false, false, true) => FileKind::Pptx,
        _ => FileKind::Zip,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextLayerStatus {
    /// Every page has text.
    Present,
    /// Some pages have text, others look scanned: OCR only those pages.
    Partial,
    /// No page has text: a scan, or extraction failed. Not proof of emptiness.
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TextLayerSummary {
    pub pages: usize,
    pub pages_with_text: usize,
    pub status: TextLayerStatus,
}

/// Summarise `pdftotext` output, whose pages are separated by form feeds.
/// A final form feed does not start another page. An empty string is zero pages.
pub fn text_layer_from_pdftotext(text: &str) -> TextLayerSummary {
    let mut pages: Vec<&str> = text.split('\x0c').collect();
    if pages.last().is_some_and(|last| last.trim().is_empty()) {
        pages.pop();
    }
    let pages_with_text = pages
        .iter()
        .filter(|page| {
            page.chars().filter(|c| c.is_alphanumeric()).count() >= MIN_ALPHANUMERIC_PER_TEXT_PAGE
        })
        .count();
    let status = if pages_with_text == 0 {
        TextLayerStatus::Absent
    } else if pages_with_text == pages.len() {
        TextLayerStatus::Present
    } else {
        TextLayerStatus::Partial
    };
    TextLayerSummary {
        pages: pages.len(),
        pages_with_text,
        status,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestEntry {
    pub original_name: String,
    pub size_bytes: u64,
    /// Digest of the bytes classified, not BOSA's metadata hash.
    pub sha256: String,
    pub kind: FileKind,
    /// False when the name's extension is not one the sniffed kind carries.
    pub extension_matches_kind: bool,
}

/// Build the manifest row for one retained file. Text-layer detection needs the
/// sandboxed extractor's output and is attached separately.
pub fn manifest_entry(original_name: &str, bytes: &[u8]) -> ManifestEntry {
    let kind = sniff(bytes);
    let extension = original_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    ManifestEntry {
        original_name: original_name.to_owned(),
        size_bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        kind,
        extension_matches_kind: extension
            .as_deref()
            .is_some_and(|ext| kind.extensions().contains(&ext)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn zip_with(names: &[&str]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for name in names {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"x").unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn magic_bytes_decide_the_kind() {
        assert_eq!(sniff(b"%PDF-1.7\n..."), FileKind::Pdf);
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), FileKind::Jpeg);
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            FileKind::Png
        );
        assert_eq!(sniff(&[b'I', b'I', 0x2A, 0x00, 0]), FileKind::Tiff);
        assert_eq!(
            sniff(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]),
            FileKind::LegacyOffice
        );
        assert_eq!(sniff(b""), FileKind::Unknown);
        assert_eq!(sniff(b"hello"), FileKind::Unknown);
    }

    #[test]
    fn zip_members_separate_ooxml_from_plain_archives() {
        assert_eq!(
            sniff(&zip_with(&["[Content_Types].xml", "word/document.xml"])),
            FileKind::Docx
        );
        assert_eq!(
            sniff(&zip_with(&["[Content_Types].xml", "xl/workbook.xml"])),
            FileKind::Xlsx
        );
        assert_eq!(
            sniff(&zip_with(&["[Content_Types].xml", "ppt/presentation.xml"])),
            FileKind::Pptx
        );
        assert_eq!(sniff(&zip_with(&["a.pdf", "b.pdf"])), FileKind::Zip);
        // Ambiguous or incomplete OOXML is not trusted as a document.
        assert_eq!(sniff(&zip_with(&["word/document.xml"])), FileKind::Zip);
        assert_eq!(
            sniff(&zip_with(&[
                "[Content_Types].xml",
                "word/a.xml",
                "xl/b.xml"
            ])),
            FileKind::Zip
        );
    }

    #[test]
    fn a_truncated_zip_is_unknown_not_a_panic() {
        let mut bytes = zip_with(&["a.txt"]);
        bytes.truncate(10);
        assert_eq!(sniff(&bytes), FileKind::Unknown);
    }

    #[test]
    fn extension_is_checked_against_the_sniffed_kind() {
        let pdf = b"%PDF-1.4 body";
        assert!(manifest_entry("Bestek.PDF", pdf).extension_matches_kind);
        assert!(!manifest_entry("Bestek.docx", pdf).extension_matches_kind);
        assert!(!manifest_entry("no_extension", pdf).extension_matches_kind);
        let entry = manifest_entry("x.pdf", pdf);
        assert_eq!(entry.size_bytes, pdf.len() as u64);
        assert_eq!(entry.sha256.len(), 64);
    }

    #[test]
    fn unknown_kind_never_matches_an_extension() {
        assert!(!manifest_entry("notes.txt", b"plain text").extension_matches_kind);
    }

    #[test]
    fn text_layer_distinguishes_present_partial_and_absent() {
        let page = "Dit bestek beschrijft de gunningscriteria en de technische voorwaarden.";
        assert_eq!(
            text_layer_from_pdftotext(&format!("{page}\x0c{page}\x0c")),
            TextLayerSummary {
                pages: 2,
                pages_with_text: 2,
                status: TextLayerStatus::Present
            }
        );
        assert_eq!(
            text_layer_from_pdftotext(&format!("{page}\x0c \n \x0c{page}\x0c")),
            TextLayerSummary {
                pages: 3,
                pages_with_text: 2,
                status: TextLayerStatus::Partial
            }
        );
        // The documented scanned-PDF symptom: only form feeds.
        assert_eq!(
            text_layer_from_pdftotext("\x0c\x0c\x0c"),
            TextLayerSummary {
                pages: 3,
                pages_with_text: 0,
                status: TextLayerStatus::Absent
            }
        );
    }

    #[test]
    fn stray_glyphs_do_not_count_as_a_text_layer() {
        let summary = text_layer_from_pdftotext("p. 3 \x0c--\x0c");
        assert_eq!(summary.status, TextLayerStatus::Absent);
    }

    #[test]
    fn empty_extraction_is_absent_with_zero_pages() {
        assert_eq!(
            text_layer_from_pdftotext(""),
            TextLayerSummary {
                pages: 0,
                pages_with_text: 0,
                status: TextLayerStatus::Absent
            }
        );
    }
}
