//! Text extraction for DOCX and XLSX attachments, with source locations.
//!
//! Deterministic and offline: no model, no OCR, no network. Content is only
//! read as text; macros, embedded objects, images, headers/footers, comments
//! and tracked-change history are not read or executed. Formulas are not
//! evaluated: spreadsheet cells carry the value cached in the file.
//!
//! Safety bounds apply to hostile input (declared archive size, part size,
//! entry count, total text). Unlike the PDF path this runs **in-process**, not
//! in the OS sandbox, so run it on untrusted files only inside a stronger
//! boundary for unattended work.
use super::classify::{sniff, FileKind};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::Serialize;
use std::io::{Cursor, Read};

const MAX_DECLARED_UNCOMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 10_000;
const MAX_XML_PART_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SPREADSHEET_CELLS: usize = 200_000;
const MAX_SEGMENTS: usize = 50_000;
/// calamine allocates a dense grid for a sheet's bounding box, so two far-apart
/// cells in a tiny file could demand gigabytes. Bound the box before calamine runs.
const MAX_SHEET_BOUNDING_BOX_CELLS: u64 = 4_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TextSegment {
    /// Human-checkable source position, e.g. `paragraph 12`, `table 2 row 3 cell 1`,
    /// or `Sheet1!B7`. Paragraph numbers count `w:p` elements and cell numbers count
    /// `w:tc` elements in the row, so merged cells (gridSpan/vMerge) make a cell
    /// number differ from the visual column.
    pub location: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtractedText {
    pub kind: FileKind,
    pub segments: Vec<TextSegment>,
    /// True when a bound stopped extraction or a sheet could not be read; the
    /// segments are then incomplete and a missing quote proves nothing.
    pub truncated: bool,
    /// Sheets that could not be read; empty for DOCX.
    pub failed_sheets: Vec<String>,
    /// Plain statements of what was not read, so absence is never read as emptiness.
    pub not_read: Vec<&'static str>,
}

/// Dispatch on the sniffed kind, never on the file name.
pub fn extract(bytes: &[u8]) -> Result<ExtractedText, String> {
    match sniff(bytes) {
        FileKind::Docx => extract_docx(bytes),
        FileKind::Xlsx => extract_xlsx(bytes),
        other => Err(format!("unsupported kind for OOXML extraction: {other:?}")),
    }
}

fn check_archive(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("invalid zip: {e}"))?;
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err("archive has too many entries".into());
    }
    let mut declared: u64 = 0;
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|e| format!("invalid zip entry: {e}"))?;
        declared = declared.saturating_add(entry.size());
    }
    if declared > MAX_DECLARED_UNCOMPRESSED_BYTES {
        return Err("archive declares too much uncompressed data".into());
    }
    Ok(archive)
}

fn read_part(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<Vec<u8>>, String> {
    let Ok(entry) = archive.by_name(name) else {
        return Ok(None);
    };
    if entry.size() > MAX_XML_PART_BYTES {
        return Err(format!("{name} exceeds the part size limit"));
    }
    let mut data = Vec::new();
    entry
        .take(MAX_XML_PART_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("cannot read {name}: {e}"))?;
    if data.len() as u64 > MAX_XML_PART_BYTES {
        return Err(format!("{name} exceeds the part size limit"));
    }
    Ok(Some(data))
}

fn local(name: &[u8]) -> &[u8] {
    name.rsplit(|b| *b == b':').next().unwrap_or(name)
}

/// One open table cell while walking `word/document.xml`; a stack handles nesting.
struct Cell {
    table: usize,
    row: usize,
    cell: usize,
    parts: Vec<String>,
}

pub fn extract_docx(bytes: &[u8]) -> Result<ExtractedText, String> {
    if sniff(bytes) != FileKind::Docx {
        return Err("not a DOCX file".into());
    }
    let mut archive = check_archive(bytes)?;
    let xml = read_part(&mut archive, "word/document.xml")?
        .ok_or_else(|| "word/document.xml is missing".to_owned())?;
    let mut reader = Reader::from_reader(xml.as_slice());
    reader.config_mut().trim_text(false);

    let mut segments = Vec::new();
    let (mut truncated, mut total) = (false, 0usize);
    let (mut paragraph_no, mut table_no) = (0usize, 0usize);
    let mut paragraph = String::new();
    let mut in_text = false;
    let mut skip_depth = 0usize; // inside w:txbxContent: not read
    let mut dropped_entity = false;
    let mut cells: Vec<Cell> = Vec::new();
    let mut table_stack: Vec<(usize, usize, usize)> = Vec::new(); // (table, row, cells so far in row)

    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("invalid document.xml: {e}"))?;
        // Text boxes nest paragraphs inside a paragraph and may appear twice
        // (AlternateContent); they are skipped whole so the outer text stays intact.
        if skip_depth > 0 {
            match &event {
                Event::Start(e) if local(e.name().as_ref()) == b"txbxContent" => skip_depth += 1,
                Event::End(e) if local(e.name().as_ref()) == b"txbxContent" => skip_depth -= 1,
                Event::Eof => break,
                _ => {}
            }
            continue;
        }
        if let Event::Start(e) = &event {
            if local(e.name().as_ref()) == b"txbxContent" {
                skip_depth = 1;
                continue;
            }
        }
        match event {
            Event::Start(e) => match local(e.name().as_ref()) {
                b"tbl" => {
                    table_no += 1;
                    table_stack.push((table_no, 0, 0));
                }
                b"tr" => {
                    if let Some(top) = table_stack.last_mut() {
                        top.1 += 1;
                        top.2 = 0;
                    }
                }
                b"tc" => {
                    if let Some(top) = table_stack.last_mut() {
                        top.2 += 1;
                        cells.push(Cell {
                            table: top.0,
                            row: top.1,
                            cell: top.2,
                            parts: Vec::new(),
                        });
                    }
                }
                b"p" => {
                    paragraph.clear();
                    paragraph_no += 1;
                }
                b"t" => in_text = true,
                _ => {}
            },
            Event::Empty(e) => match local(e.name().as_ref()) {
                b"p" => paragraph_no += 1,
                b"tab" => paragraph.push('\t'),
                b"br" | b"cr" => paragraph.push('\n'),
                _ => {}
            },
            Event::Text(t) if in_text => {
                paragraph.push_str(&t.decode().map_err(|e| e.to_string())?);
            }
            Event::CData(t) if in_text => {
                paragraph.push_str(&t.decode().map_err(|e| e.to_string())?);
            }
            Event::GeneralRef(r) if in_text => {
                if let Some(c) = r.resolve_char_ref().map_err(|e| e.to_string())? {
                    paragraph.push(c);
                } else if let Some(s) = quick_xml::escape::resolve_predefined_entity(
                    &r.decode().map_err(|e| e.to_string())?,
                ) {
                    paragraph.push_str(s);
                } else {
                    dropped_entity = true;
                }
            }
            Event::End(e) => match local(e.name().as_ref()) {
                b"t" => in_text = false,
                b"p" => {
                    let text = paragraph.trim().to_owned();
                    paragraph.clear();
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(cell) = cells.last_mut() {
                        cell.parts.push(text);
                    } else {
                        push_segment(
                            &mut segments,
                            &mut total,
                            &mut truncated,
                            format!("paragraph {paragraph_no}"),
                            text,
                        );
                    }
                }
                b"tc" => {
                    if let Some(cell) = cells.pop() {
                        let text = cell.parts.join(" ");
                        if !text.is_empty() {
                            push_segment(
                                &mut segments,
                                &mut total,
                                &mut truncated,
                                format!("table {} row {} cell {}", cell.table, cell.row, cell.cell),
                                text,
                            );
                        }
                    }
                }
                b"tbl" => {
                    table_stack.pop();
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        if truncated {
            break;
        }
    }
    let mut not_read = vec![
        "headers_and_footers",
        "comments_and_tracked_changes",
        "text_boxes_and_embedded_objects",
        "images",
    ];
    if dropped_entity {
        not_read.push("unresolved_entity_references_dropped");
    }
    Ok(ExtractedText {
        kind: FileKind::Docx,
        segments,
        truncated,
        failed_sheets: Vec::new(),
        not_read,
    })
}

fn push_segment(
    segments: &mut Vec<TextSegment>,
    total: &mut usize,
    truncated: &mut bool,
    location: String,
    text: String,
) {
    if segments.len() >= MAX_SEGMENTS || *total + text.len() > MAX_TOTAL_TEXT_BYTES {
        *truncated = true;
        return;
    }
    *total += text.len();
    segments.push(TextSegment { location, text });
}

fn column_letters(mut index: u32) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.reverse();
    String::from_utf8(letters).unwrap_or_default()
}

/// Parse an A1 reference into zero-based (row, column); saturating, never panics.
fn parse_a1(reference: &str) -> Option<(u64, u64)> {
    let letters = reference
        .bytes()
        .take_while(u8::is_ascii_alphabetic)
        .count();
    let (col, row) = reference.split_at(letters);
    if col.is_empty() || row.is_empty() || !row.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let column = col.bytes().fold(0u64, |acc, b| {
        acc.saturating_mul(26)
            .saturating_add(u64::from(b.to_ascii_uppercase() - b'A') + 1)
    });
    let row: u64 = row.parse().ok()?;
    Some((row.checked_sub(1)?, column.checked_sub(1)?))
}

/// Reject any worksheet whose cells span a bounding box larger than the limit.
/// Uses the cells actually present, not the file's own `<dimension>` claim.
fn check_sheet_areas(archive: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Result<(), String> {
    let names: Vec<String> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|n| n.starts_with("xl/") && n.ends_with(".xml"))
        .map(|n| n.into_owned())
        .collect();
    for name in names {
        let Some(xml) = read_part(archive, &name)? else {
            continue;
        };
        let mut reader = Reader::from_reader(xml.as_slice());
        let (mut row_min, mut row_max, mut col_min, mut col_max) = (u64::MAX, 0u64, u64::MAX, 0u64);
        let (mut current_row, mut next_col) = (0u64, 0u64);
        loop {
            let event = reader
                .read_event()
                .map_err(|e| format!("invalid {name}: {e}"))?;
            let element = match &event {
                Event::Start(e) | Event::Empty(e) => e,
                Event::Eof => break,
                _ => continue,
            };
            let kind = local(element.name().as_ref()).to_vec();
            if kind != b"row" && kind != b"c" {
                continue;
            }
            let reference = element
                .attributes()
                .flatten()
                .find(|a| local(a.key.as_ref()) == b"r")
                .and_then(|a| String::from_utf8(a.value.into_owned()).ok());
            if kind == b"row" {
                if let Some(row) = reference.as_deref().and_then(|r| r.parse::<u64>().ok()) {
                    current_row = row.saturating_sub(1);
                } else {
                    current_row = current_row.saturating_add(1);
                }
                next_col = 0;
                continue;
            }
            let (row, col) = match reference.as_deref() {
                Some(r) => {
                    parse_a1(r).ok_or_else(|| format!("invalid cell reference in {name}"))?
                }
                None => (current_row, next_col),
            };
            next_col = col.saturating_add(1);
            row_min = row_min.min(row);
            row_max = row_max.max(row);
            col_min = col_min.min(col);
            col_max = col_max.max(col);
        }
        if row_max >= row_min {
            let area = (row_max - row_min + 1).saturating_mul(col_max - col_min + 1);
            if area > MAX_SHEET_BOUNDING_BOX_CELLS {
                return Err(format!(
                    "{name} spans a bounding box too large to read safely"
                ));
            }
        }
    }
    Ok(())
}

pub fn extract_xlsx(bytes: &[u8]) -> Result<ExtractedText, String> {
    use calamine::{Data, DataType, Reader as _, Xlsx};
    if sniff(bytes) != FileKind::Xlsx {
        return Err("not an XLSX file".into());
    }
    let mut archive = check_archive(bytes)?;
    check_sheet_areas(&mut archive)?;
    let mut workbook = calamine::open_workbook_from_rs::<Xlsx<_>, _>(Cursor::new(bytes))
        .map_err(|e| e.to_string())?;
    let mut segments = Vec::new();
    let mut failed_sheets = Vec::new();
    let (mut truncated, mut total, mut cells_seen) = (false, 0usize, 0usize);
    'sheets: for name in workbook.sheet_names() {
        let Ok(range) = workbook.worksheet_range(&name) else {
            failed_sheets.push(name.clone());
            truncated = true;
            continue;
        };
        let (row0, col0) = range.start().unwrap_or((0, 0));
        for (row, col, value) in range.cells() {
            if matches!(value, Data::Empty) {
                continue;
            }
            cells_seen += 1;
            if cells_seen > MAX_SPREADSHEET_CELLS {
                truncated = true;
                break 'sheets;
            }
            let text = value
                .as_string()
                .unwrap_or_else(|| value.to_string())
                .trim()
                .to_owned();
            if text.is_empty() {
                continue;
            }
            // calamine ranges are relative to their start cell; restore A1 addresses.
            let address = format!(
                "{name}!{}{}",
                column_letters(col0 + col as u32),
                row0 + row as u32 + 1
            );
            push_segment(&mut segments, &mut total, &mut truncated, address, text);
            if truncated {
                break 'sheets;
            }
        }
    }
    Ok(ExtractedText {
        kind: FileKind::Xlsx,
        segments,
        truncated,
        failed_sheets,
        not_read: vec![
            "formulas_not_evaluated_cached_values_only",
            "comments",
            "charts_images_and_embedded_objects",
            "hidden_state_not_reported",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn docx(document_xml: &str) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in [
            ("[Content_Types].xml", "<Types/>"),
            ("word/document.xml", document_xml),
        ] {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    #[test]
    fn paragraphs_and_table_cells_keep_their_locations() {
        let xml = format!(
            r#"<w:document {W}><w:body>
            <w:p><w:r><w:t>Gunningscriteria</w:t></w:r></w:p>
            <w:p><w:r><w:t>Prijs </w:t></w:r><w:r><w:t>60 &amp; kwaliteit 40</w:t></w:r></w:p>
            <w:tbl>
              <w:tr><w:tc><w:p><w:r><w:t>Criterium</w:t></w:r></w:p></w:tc>
                    <w:tc><w:p><w:r><w:t>Gewicht</w:t></w:r></w:p></w:tc></w:tr>
              <w:tr><w:tc><w:p><w:r><w:t>Prijs</w:t></w:r></w:p></w:tc>
                    <w:tc><w:p><w:r><w:t>60</w:t></w:r></w:p></w:tc></w:tr>
            </w:tbl>
            </w:body></w:document>"#
        );
        let out = extract(&docx(&xml)).unwrap();
        assert!(!out.truncated);
        let got: Vec<(&str, &str)> = out
            .segments
            .iter()
            .map(|s| (s.location.as_str(), s.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("paragraph 1", "Gunningscriteria"),
                ("paragraph 2", "Prijs 60 & kwaliteit 40"),
                ("table 1 row 1 cell 1", "Criterium"),
                ("table 1 row 1 cell 2", "Gewicht"),
                ("table 1 row 2 cell 1", "Prijs"),
                ("table 1 row 2 cell 2", "60"),
            ]
        );
        assert!(out.not_read.contains(&"headers_and_footers"));
    }

    #[test]
    fn nested_tables_attribute_text_to_the_innermost_cell() {
        let xml = format!(
            r#"<w:document {W}><w:body><w:tbl><w:tr><w:tc>
              <w:p><w:r><w:t>outer</w:t></w:r></w:p>
              <w:tbl><w:tr><w:tc><w:p><w:r><w:t>inner</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
            </w:tc></w:tr></w:tbl></w:body></w:document>"#
        );
        let out = extract_docx(&docx(&xml)).unwrap();
        let got: Vec<_> = out
            .segments
            .iter()
            .map(|s| (s.location.as_str(), s.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("table 2 row 1 cell 1", "inner"),
                ("table 1 row 1 cell 1", "outer")
            ]
        );
    }

    #[test]
    fn text_outside_runs_is_not_invented() {
        let xml = format!(
            r#"<w:document {W}><w:body><w:p/><w:p><w:r><w:tab/></w:r></w:p></w:body></w:document>"#
        );
        assert!(extract_docx(&docx(&xml)).unwrap().segments.is_empty());
    }

    #[test]
    fn a_missing_document_part_or_wrong_kind_is_an_error_not_empty_text() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for name in ["[Content_Types].xml", "word/styles.xml"] {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"<x/>").unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        assert!(extract_docx(&bytes).unwrap_err().contains("missing"));
        assert!(extract(b"%PDF-1.7").is_err());
        assert!(extract_xlsx(&docx("<a/>")).is_err());
    }

    #[test]
    fn broken_xml_fails_closed() {
        let bytes = docx("<w:document><w:body><w:p><w:t>x</w:body>");
        assert!(extract_docx(&bytes).is_err());
    }

    #[test]
    fn oversized_text_truncates_and_says_so() {
        let paragraph = format!("<w:p><w:r><w:t>{}</w:t></w:r></w:p>", "a".repeat(100_000));
        let xml = format!(
            "<w:document {W}><w:body>{}</w:body></w:document>",
            paragraph.repeat(30)
        );
        let out = extract_docx(&docx(&xml)).unwrap();
        assert!(out.truncated);
        assert!(out.segments.len() < 30);
    }

    fn xlsx() -> Vec<u8> {
        xlsx_with(
            r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="3"><c r="B3" t="inlineStr"><is><t>Post</t></is></c><c r="C3" t="inlineStr"><is><t>Prijs</t></is></c></row><row r="4"><c r="B4" t="inlineStr"><is><t>Dallen</t></is></c><c r="C4"><v>1250.5</v></c></row></sheetData></worksheet>"#,
        )
    }

    fn xlsx_with(sheet: &str) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let parts = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Lijst" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            ("xl/worksheets/sheet1.xml", sheet),
        ];
        for (name, body) in parts {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn xlsx_cells_keep_sheet_and_absolute_addresses() {
        let out = extract(&xlsx()).unwrap();
        assert_eq!(out.kind, FileKind::Xlsx);
        let got: Vec<_> = out
            .segments
            .iter()
            .map(|s| (s.location.as_str(), s.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Lijst!B3", "Post"),
                ("Lijst!C3", "Prijs"),
                ("Lijst!B4", "Dallen"),
                ("Lijst!C4", "1250.5"),
            ]
        );
        assert!(out
            .not_read
            .contains(&"formulas_not_evaluated_cached_values_only"));
    }

    #[test]
    fn a_tiny_xlsx_with_far_apart_cells_is_refused_before_allocation() {
        let sheet = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row><row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>b</t></is></c></row></sheetData></worksheet>"#;
        let error = extract_xlsx(&xlsx_with(sheet)).unwrap_err();
        assert!(error.contains("bounding box"), "{error}");
    }

    #[test]
    fn a1_references_parse_without_panicking() {
        assert_eq!(parse_a1("A1"), Some((0, 0)));
        assert_eq!(parse_a1("XFD1048576"), Some((1_048_575, 16_383)));
        for bad in [
            "",
            "A",
            "1",
            "A0",
            "1A",
            "A-1",
            "AAAAAAAAAAAAAAAAAAAAAAAAAA1",
        ] {
            // Must return without panicking; oversized columns saturate.
            let _ = parse_a1(bad);
        }
        assert_eq!(parse_a1("A0"), None);
        assert_eq!(parse_a1(""), None);
    }

    #[test]
    fn text_boxes_are_skipped_whole_and_do_not_swallow_surrounding_text() {
        let xml = format!(
            r#"<w:document {W}><w:body><w:p><w:r><w:t>Before</w:t></w:r><w:r><w:txbxContent><w:p><w:r><w:t>box</w:t></w:r></w:p></w:txbxContent></w:r><w:r><w:t> after</w:t></w:r></w:p></w:body></w:document>"#
        );
        let out = extract_docx(&docx(&xml)).unwrap();
        let got: Vec<_> = out.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(got, ["Before after"]);
        assert!(out.not_read.contains(&"text_boxes_and_embedded_objects"));
    }

    #[test]
    fn self_closed_empty_paragraphs_keep_word_numbering() {
        let xml = format!(
            r#"<w:document {W}><w:body><w:p/><w:p></w:p><w:p><w:r><w:t>derde</w:t></w:r></w:p></w:body></w:document>"#
        );
        let out = extract_docx(&docx(&xml)).unwrap();
        assert_eq!(out.segments[0].location, "paragraph 3");
    }

    #[test]
    fn cdata_is_read_and_unknown_entities_are_reported_not_silent() {
        let xml = format!(
            r#"<w:document {W}><w:body><w:p><w:r><w:t><![CDATA[a < b]]> &nbsp; x</w:t></w:r></w:p></w:body></w:document>"#
        );
        let out = extract_docx(&docx(&xml)).unwrap();
        assert!(out.segments[0].text.contains("a < b"));
        assert!(out
            .not_read
            .contains(&"unresolved_entity_references_dropped"));
    }

    #[test]
    fn spreadsheet_addresses_are_absolute_a1_references() {
        assert_eq!(column_letters(0), "A");
        assert_eq!(column_letters(25), "Z");
        assert_eq!(column_letters(26), "AA");
        assert_eq!(column_letters(701), "ZZ");
        assert_eq!(column_letters(702), "AAA");
    }
}
