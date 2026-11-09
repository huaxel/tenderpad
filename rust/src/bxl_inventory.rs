//! Brussels public contract inventories ("inventaires de marchés publics").
//!
//! Under the joint ordinance decree of 16 May 2019 and the circular of
//! 14 January 2021, Brussels administrations publish a per-year inventory of
//! their public contracts as XLSX on data.gov.be / datastore.brussels (CC0).
//! The file lists awarded **engagements and liquidations** — accounting lines,
//! not procedures — and carries the contracting authority's KBO number
//! (`id_bce_org`), the supplier's KBO number and name (`id_bce_recipient`,
//! `name_recipient`), amounts and dates (`amount_eng`, `date_eng`, `amount_liq`)
//! and the object of the expenditure (`name_eng`, `desc_ab_fr`/`_nl`).
//!
//! Measured on two 2024 files (2026-10-18): Evere 4,328 lines / €33,258,979 and
//! COCOM 490 lines / €17,863,100, while those two buyers appear in TED three
//! times that year. So this is the only open, contract-level view of the part of
//! Belgian procurement that never reaches TED — and, because it names the
//! supplier by KBO, it is also the only public source of outcomes that a
//! label-free evaluation can be built on. It is *not* a source of open tenders:
//! it says what was bought, not what can still be bid on.
//!
//! Format notes that matter for parsing: rows are sparse (a cell exists only
//! when it has a value, and `cell/@r` gives its column), strings live in
//! `xl/sharedStrings.xml` and are referenced by index, dates arrive as Excel
//! serial numbers (or occasionally as text), and amounts may use spaces or
//! commas as thousands separators. Reading the sheet positionally produces
//! nonsense; every test here exists because of that.

use chrono::{Duration, NaiveDate};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::io::Read;
use std::path::Path;
use zip::ZipArchive;

/// Decompressed-size bound per XLSX part. An inventory file is a few hundred
/// kilobytes, but a zip bomb is not, and the sheet and string table are read
/// into memory whole.
const MAX_PART_BYTES: u64 = 128 * 1024 * 1024;

/// One awarded engagement from an inventory file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryAward {
    /// KBO number of the contracting authority (`id_bce_org`).
    pub buyer_identifier: Option<String>,
    pub buyer_name: Option<String>,
    /// KBO number of the supplier (`id_bce_recipient`) when the file carries one.
    pub awardee_identifier: Option<String>,
    pub awardee_name: Option<String>,
    /// Engagement reference (`id_eng`).
    pub engagement_id: Option<String>,
    /// Engagement date as `YYYY-MM-DD`.
    pub engagement_date: Option<String>,
    pub engagement_amount: Option<Decimal>,
    /// Best available description of what was bought, from the first non-empty
    /// of `name_eng`, `desc_ab_fr`/`_nl`, `acc_desc_function_*`,
    /// `acc_desc_economic_*`.
    pub object: Option<String>,
    /// Stable identity of the source row, so re-importing a file is idempotent
    /// even though the file has no row identifier of its own.
    pub row_hash: String,
}

/// Parse one inventory XLSX into awards. Rows without any content are skipped,
/// and `row_hash` is filled in.
pub fn parse_inventory(path: impl AsRef<Path>) -> Result<Vec<InventoryAward>, Box<dyn Error>> {
    let file = std::fs::File::open(path.as_ref())?;
    let mut archive = ZipArchive::new(std::io::BufReader::new(file))?;
    let shared = read_shared_strings(&mut archive)?;
    let sheet = read_sheet_xml(&mut archive)?;
    let table = parse_sheet(&sheet, &shared)?;
    if table.is_empty() {
        return Err("inventory worksheet has no rows".into());
    }
    let header = &table[0];
    // The dataset lists several files per administration and not all of them
    // use the KBO-bearing layout: some older ones carry French column names and
    // no identifiers at all. Importing those "successfully" with zero rows is
    // worse than refusing them, so the layout is checked and named.
    let recognised = ["id_bce_org", "id_bce_recipient"].iter().any(|name| {
        header
            .iter()
            .any(|column| normalize_header(column) == *name)
    });
    if !recognised {
        let found = header
            .iter()
            .map(|column| column.trim())
            .filter(|column| !column.is_empty())
            .take(6)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "unrecognised inventory layout: expected a KBO-bearing inventory \
             (id_bce_org / id_bce_recipient), found first row: {found}"
        )
        .into());
    }
    Ok(table_to_awards(&table))
}

fn read_shared_strings<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut xml = String::new();
    match archive.by_name("xl/sharedStrings.xml") {
        Ok(mut entry) => {
            if entry.size() > MAX_PART_BYTES {
                return Err(format!(
                    "inventory sharedStrings is {} bytes, above the {MAX_PART_BYTES} byte bound",
                    entry.size()
                )
                .into());
            }
            entry.read_to_string(&mut xml)?;
        }
        Err(_) => return Ok(Vec::new()),
    }
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    let mut strings = Vec::new();
    let mut current = String::new();
    let mut in_item = false;
    loop {
        match reader.read_event()? {
            quick_xml::events::Event::Start(element) => {
                if local_name(element.name().as_ref()) == "si" {
                    in_item = true;
                    current.clear();
                }
            }
            quick_xml::events::Event::End(element) => {
                if local_name(element.name().as_ref()) == "si" {
                    strings.push(std::mem::take(&mut current));
                    in_item = false;
                }
            }
            quick_xml::events::Event::Text(text) => {
                if in_item {
                    current.push_str(&text.decode()?);
                }
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    Ok(strings)
}

fn read_sheet_xml<R: Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<String, Box<dyn Error>> {
    // The workbook's first sheet is not always `sheet1.xml`, so follow the
    // workbook relationships rather than guessing.
    let mut workbook = String::new();
    let mut entry = archive
        .by_name("xl/workbook.xml")
        .map_err(|_| -> Box<dyn Error> {
            "inventory workbook is missing xl/workbook.xml".into()
        })?;
    if entry.size() > MAX_PART_BYTES {
        return Err(format!(
            "inventory workbook is {} bytes, above the {MAX_PART_BYTES} byte bound",
            entry.size()
        )
        .into());
    }
    entry.read_to_string(&mut workbook)?;
    drop(entry);
    let mut reader = quick_xml::Reader::from_str(&workbook);
    let mut first_sheet_id = None;
    loop {
        match reader.read_event()? {
            quick_xml::events::Event::Start(element) | quick_xml::events::Event::Empty(element) => {
                if local_name(element.name().as_ref()) == "sheet" {
                    for attribute in element.attributes().flatten() {
                        let key = local_name(attribute.key.as_ref());
                        if key == "id" || key.ends_with(":id") {
                            first_sheet_id =
                                Some(String::from_utf8_lossy(&attribute.value).into_owned());
                        }
                        if (key == "sheetId" || key.ends_with(":sheetId"))
                            && first_sheet_id.is_none()
                        {
                            first_sheet_id =
                                Some(String::from_utf8_lossy(&attribute.value).into_owned());
                        }
                    }
                    break;
                }
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    let mut names = vec!["xl/worksheets/sheet1.xml".to_owned()];
    if let Some(identifier) = first_sheet_id {
        let hint: String = identifier
            .chars()
            .filter(|character| character.is_ascii_digit())
            .collect();
        if !hint.is_empty() {
            names.insert(0, format!("xl/worksheets/sheet{hint}.xml"));
        }
    }
    for name in names {
        if let Ok(mut entry) = archive.by_name(&name) {
            if entry.size() > MAX_PART_BYTES {
                return Err(format!(
                    "inventory sheet {name} is {} bytes, above the {MAX_PART_BYTES} byte bound",
                    entry.size()
                )
                .into());
            }
            let mut xml = String::new();
            entry.read_to_string(&mut xml)?;
            return Ok(xml);
        }
    }
    Err("inventory workbook has no worksheet".into())
}

fn local_name(name: &[u8]) -> String {
    let text = String::from_utf8_lossy(name).into_owned();
    text.rsplit(':').next().unwrap_or(&text).to_owned()
}

/// A dense view of the sheet: every row is a vector indexed by column.
fn parse_sheet(xml: &str, shared: &[String]) -> Result<Vec<Vec<String>>, Box<dyn Error>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    let mut cell_column: Option<usize> = None;
    let mut cell_kind: Option<String> = None;
    let mut cell_text = String::new();
    let mut in_value = false;
    loop {
        match reader.read_event()? {
            quick_xml::events::Event::Start(element) => {
                let name = local_name(element.name().as_ref());
                match name.as_str() {
                    "row" => current = Some(vec![String::new(); 40]),
                    "c" => {
                        cell_column = None;
                        cell_kind = None;
                        cell_text.clear();
                        for attribute in element.attributes().flatten() {
                            let key = local_name(attribute.key.as_ref());
                            let value = String::from_utf8_lossy(&attribute.value).into_owned();
                            match key.as_str() {
                                "r" => cell_column = column_index(&value),
                                "t" => cell_kind = Some(value),
                                _ => {}
                            }
                        }
                    }
                    "v" | "t" => in_value = true,
                    _ => {}
                }
            }
            quick_xml::events::Event::Empty(element) => {
                if local_name(element.name().as_ref()) == "c" {
                    // A cell with no value contributes nothing.
                }
            }
            quick_xml::events::Event::Text(text) => {
                if in_value {
                    cell_text.push_str(&text.decode()?);
                }
            }
            quick_xml::events::Event::End(element) => {
                let name = local_name(element.name().as_ref());
                match name.as_str() {
                    "v" | "t" => in_value = false,
                    "c" => {
                        if let (Some(row), Some(column)) = (current.as_mut(), cell_column) {
                            if column >= row.len() {
                                row.resize(column + 8, String::new());
                            }
                            row[column] = match cell_kind.as_deref() {
                                Some("s") => cell_text
                                    .trim()
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|index| shared.get(index).cloned())
                                    .unwrap_or_default(),
                                _ => cell_text.clone(),
                            };
                        }
                        cell_column = None;
                        cell_kind = None;
                        cell_text.clear();
                    }
                    "row" => {
                        if let Some(row) = current.take() {
                            rows.push(row);
                        }
                    }
                    _ => {}
                }
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    Ok(rows)
}

/// `C12` -> 2 (zero-based column), ignoring the row number.
pub fn column_index(reference: &str) -> Option<usize> {
    let mut value: usize = 0;
    let mut seen_letter = false;
    for character in reference.chars() {
        if character.is_ascii_alphabetic() {
            seen_letter = true;
            value = value * 26 + (character.to_ascii_uppercase() as usize - 'A' as usize + 1);
        } else {
            break;
        }
    }
    seen_letter.then(|| value - 1)
}

/// Excel serial date (1900 system, with its 29 February 1900 quirk) to
/// `YYYY-MM-DD`. Values outside a plausible range are rejected rather than
/// guessed at.
pub fn excel_serial_to_date(serial: f64) -> Option<String> {
    if !(1.0..=80_000.0).contains(&serial) {
        return None;
    }
    let days = serial.trunc() as i64;
    Some((NaiveDate::from_ymd_opt(1899, 12, 30)? + Duration::days(days)).to_string())
}

/// Dates arrive as Excel serials in the published files, but text dates appear
/// in hand-edited ones: `YYYY-MM-DD` and Belgian `DD/MM/YYYY` are both accepted.
pub fn parse_date(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(serial) = trimmed.parse::<f64>() {
        return excel_serial_to_date(serial);
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        return Some(date.to_string());
    }
    for format in ["%d/%m/%Y", "%d-%m-%Y", "%d.%m.%Y"] {
        if let Ok(date) = NaiveDate::parse_from_str(trimmed, format) {
            return Some(date.to_string());
        }
    }
    None
}

/// Amounts are plain numbers in the published files, but copies of them travel
/// through spreadsheets that add thousands separators.
pub fn parse_amount(value: &str) -> Option<Decimal> {
    let cleaned: String = value
        .trim()
        .chars()
        .filter(|character| !character.is_whitespace() && *character != '\u{a0}')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    let normalized = match (cleaned.rfind('.'), cleaned.rfind(',')) {
        (Some(dot), Some(comma)) if dot > comma => cleaned.replace(',', ""),
        (Some(dot), Some(comma)) if comma > dot => cleaned.replace('.', "").replace(',', "."),
        (None, Some(comma)) => {
            // `1,5` is a decimal comma; `1,500` in a Belgian file is more often
            // a thousands separator, so only a single comma followed by one or
            // two digits is read as decimal.
            let digits_after = cleaned.len() - comma - 1;
            if cleaned.matches(',').count() == 1 && (1..=2).contains(&digits_after) {
                cleaned.replace(',', ".")
            } else {
                cleaned.replace(',', "")
            }
        }
        _ => cleaned,
    };
    normalized.parse::<Decimal>().ok()
}

/// Header lookup that survives case, spaces and a UTF-8 BOM.
fn normalize_header(name: &str) -> String {
    name.trim_start_matches('\u{feff}')
        .trim()
        .to_ascii_lowercase()
}

fn table_to_awards(table: &[Vec<String>]) -> Vec<InventoryAward> {
    let header = &table[0];
    let index: std::collections::HashMap<String, usize> = header
        .iter()
        .enumerate()
        .map(|(position, name)| (normalize_header(name), position))
        .filter(|(name, _)| !name.is_empty())
        .collect();
    let cell = |row: &Vec<String>, name: &str| -> Option<String> {
        index
            .get(name)
            .and_then(|position| row.get(*position))
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let mut awards = Vec::new();
    for row in &table[1..] {
        if row.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        let object = [
            "name_eng",
            "desc_ab_fr",
            "desc_ab_nl",
            "acc_desc_function_fr",
            "acc_desc_function_nl",
            "acc_desc_economic_fr",
            "acc_desc_economic_nl",
            "name_org2_nl",
            "name_org2_fr",
        ]
        .iter()
        .find_map(|name| cell(row, name));
        let engagement_amount = cell(row, "amount_eng").as_deref().and_then(parse_amount);
        let mut award = InventoryAward {
            buyer_identifier: cell(row, "id_bce_org"),
            buyer_name: cell(row, "name_org1_fr").or_else(|| cell(row, "name_org1_nl")),
            awardee_identifier: cell(row, "id_bce_recipient"),
            awardee_name: cell(row, "name_recipient"),
            engagement_id: cell(row, "id_eng"),
            engagement_date: cell(row, "date_eng").as_deref().and_then(parse_date),
            engagement_amount,
            object,
            row_hash: String::new(),
        };
        // A row has to identify a party. An amount alone cannot be attributed
        // to anyone, and a spreadsheet total is exactly that shape, so it is
        // dropped rather than counted as a purchase. Measured on the two
        // published 2024 files: 0 of 4,818 rows are in that shape.
        if award.buyer_identifier.is_none()
            && award.awardee_identifier.is_none()
            && award.awardee_name.is_none()
        {
            continue;
        }
        award.row_hash = row_hash(row);
        awards.push(award);
    }
    awards
}

/// SHA-256 over the *whole* row's cells, so re-importing a file does not
/// duplicate it while distinct lines stay distinct.
///
/// Hashing only the columns this module maps was a bug worth recording: real
/// files differ in columns such as `amount_liq`, `id_ab` or `acc_id_function`,
/// and a narrower hash silently collapsed 2,155 of Evere's 4,328 rows (and 375
/// of COCOM's 490). Measured: both published files contain **no** verbatim
/// duplicate rows, so every line must survive.
pub fn row_hash(cells: &[String]) -> String {
    let mut hasher = Sha256::new();
    let mut start = 0;
    let mut end = cells.len();
    while end > start && cells[end - 1].trim().is_empty() {
        end -= 1;
    }
    while start < end && cells[start].trim().is_empty() {
        start += 1;
    }
    for cell in &cells[start..end] {
        hasher.update(cell.trim().as_bytes());
        hasher.update([0x1f]);
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Minimal XLSX with the parts the parser reads. Cells carry explicit
    /// references so a fixture can leave gaps between columns, exactly as the
    /// published files do; a `true` flag marks a shared-string index.
    fn write_xlsx(
        path: &Path,
        header: &[(char, &str)],
        rows: &[Vec<(char, &str, bool)>],
        shared: &[&str],
    ) {
        let file = std::fs::File::create(path).expect("fixture should be created");
        let mut archive = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        let mut sheet = String::from(r#"<?xml version="1.0"?><worksheet><sheetData>"#);
        sheet.push_str("<row r=\"1\">");
        for (column, name) in header {
            let index = shared.iter().position(|value| value == name).unwrap_or(0);
            sheet.push_str(&format!("<c r=\"{column}1\" t=\"s\"><v>{index}</v></c>"));
        }
        sheet.push_str("</row>");
        for (offset, cells) in rows.iter().enumerate() {
            let number = offset + 2;
            sheet.push_str(&format!("<row r=\"{number}\">"));
            for (column, value, is_shared) in cells {
                if *is_shared {
                    sheet.push_str(&format!(
                        "<c r=\"{column}{number}\" t=\"s\"><v>{value}</v></c>"
                    ));
                } else {
                    sheet.push_str(&format!("<c r=\"{column}{number}\"><v>{value}</v></c>"));
                }
            }
            sheet.push_str("</row>");
        }
        sheet.push_str("</sheetData></worksheet>");
        let mut strings = String::from(r#"<?xml version="1.0"?><sst>"#);
        for value in shared {
            strings.push_str(&format!("<si><t>{value}</t></si>"));
        }
        strings.push_str("</sst>");
        let workbook = r#"<?xml version="1.0"?><workbook><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        for (name, contents) in [
            ("xl/workbook.xml", workbook),
            ("xl/worksheets/sheet1.xml", sheet.as_str()),
            ("xl/sharedStrings.xml", strings.as_str()),
        ] {
            archive.start_file(name, options).unwrap();
            archive.write_all(contents.as_bytes()).unwrap();
        }
        archive.finish().unwrap();
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("aanbestedingen-inventory-{name}-{suffix}.xlsx"))
    }

    /// A row may skip columns entirely; reading positionally shifts every value
    /// left, which is the mistake this fixture exists to prevent.
    #[test]
    fn sparse_rows_are_read_by_cell_reference() {
        let path = temp_path("sparse");
        let shared = [
            "id_bce_org",
            "id_eng",
            "amount_eng",
            "0207365709",
            "2024021141",
        ];
        write_xlsx(
            &path,
            &[('A', "id_bce_org"), ('B', "id_eng"), ('C', "amount_eng")],
            &[
                vec![('A', "3", true), ('C', "4553.42", false)],
                vec![('A', "3", true), ('B', "4", true), ('C', "1200", false)],
            ],
            &shared,
        );
        let awards = parse_inventory(&path).expect("fixture should parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(awards.len(), 2);
        assert_eq!(awards[0].buyer_identifier.as_deref(), Some("0207365709"));
        assert_eq!(awards[0].engagement_id, None);
        assert_eq!(awards[0].engagement_amount, parse_amount("4553.42"));
        assert_eq!(awards[1].engagement_id.as_deref(), Some("2024021141"));
    }

    #[test]
    fn reads_strings_dates_amounts_and_object() {
        let path = temp_path("fields");
        let shared = [
            "id_bce_org",
            "name_org1_fr",
            "id_bce_recipient",
            "name_recipient",
            "id_eng",
            "date_eng",
            "amount_eng",
            "name_eng",
            "0207365709",
            "Administration communale d'Evere",
            "0556965585",
            "DESK PARTNERS",
            "2024021141",
            "RMPLCT CHAUDIERE COIN VERT",
        ];
        write_xlsx(
            &path,
            &[
                ('A', "id_bce_org"),
                ('B', "name_org1_fr"),
                ('C', "id_bce_recipient"),
                ('D', "name_recipient"),
                ('E', "id_eng"),
                ('F', "date_eng"),
                ('G', "amount_eng"),
                ('H', "name_eng"),
            ],
            &[vec![
                ('A', "8", true),
                ('B', "9", true),
                ('C', "10", true),
                ('D', "11", true),
                ('E', "12", true),
                ('F', "45657", false),
                ('G', "4 553,42", false),
                ('H', "13", true),
            ]],
            &shared,
        );
        let awards = parse_inventory(&path).expect("fixture should parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(awards.len(), 1);
        let award = &awards[0];
        assert_eq!(award.buyer_identifier.as_deref(), Some("0207365709"));
        assert_eq!(
            award.buyer_name.as_deref(),
            Some("Administration communale d'Evere")
        );
        assert_eq!(award.awardee_identifier.as_deref(), Some("0556965585"));
        assert_eq!(award.awardee_name.as_deref(), Some("DESK PARTNERS"));
        assert_eq!(award.engagement_id.as_deref(), Some("2024021141"));
        assert_eq!(award.engagement_date.as_deref(), Some("2024-12-31"));
        assert_eq!(award.engagement_amount, parse_amount("4553.42"));
        assert_eq!(award.object.as_deref(), Some("RMPLCT CHAUDIERE COIN VERT"));
        assert_eq!(award.row_hash.len(), 64);
    }

    #[test]
    fn dates_and_amounts_survive_common_spellings() {
        assert_eq!(excel_serial_to_date(45657.0).as_deref(), Some("2024-12-31"));
        assert_eq!(parse_date("2024-12-31").as_deref(), Some("2024-12-31"));
        assert_eq!(parse_date("31/12/2024").as_deref(), Some("2024-12-31"));
        assert_eq!(parse_date("31-12-2024").as_deref(), Some("2024-12-31"));
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("niet van toepassing"), None);
        assert_eq!(excel_serial_to_date(0.0), None);
        assert_eq!(excel_serial_to_date(999_999.0), None);

        assert_eq!(parse_amount("4553.42").unwrap().to_string(), "4553.42");
        assert_eq!(parse_amount("4 553,42").unwrap().to_string(), "4553.42");
        assert_eq!(parse_amount("4.553,42").unwrap().to_string(), "4553.42");
        assert_eq!(parse_amount("1,5").unwrap().to_string(), "1.5");
        assert_eq!(parse_amount("1,500").unwrap().to_string(), "1500");
        assert_eq!(parse_amount("1 500").unwrap().to_string(), "1500");
        assert_eq!(parse_amount(""), None);
        assert_eq!(parse_amount("n/a"), None);
    }

    /// A row that names no party is dropped even when it carries a number:
    /// that shape is a spreadsheet total, not a purchase.
    #[test]
    fn rows_without_an_identity_are_skipped() {
        let path = temp_path("empty");
        let shared = ["id_bce_org", "amount_eng", "0207365709"];
        write_xlsx(
            &path,
            &[('A', "id_bce_org"), ('B', "amount_eng")],
            &[
                vec![],
                vec![('B', "0", false)],
                vec![('A', "2", true), ('B', "10", false)],
            ],
            &shared,
        );
        let awards = parse_inventory(&path).expect("fixture should parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(awards.len(), 1);
        assert_eq!(awards[0].buyer_identifier.as_deref(), Some("0207365709"));
    }

    #[test]
    fn row_identity_covers_every_column_and_ignores_padding() {
        let row = vec![
            "0207365709".to_owned(),
            "2024021141".to_owned(),
            "4553.42".to_owned(),
            "EUR".to_owned(),
        ];
        let first = row_hash(&row);
        assert_eq!(first, row_hash(&row));
        // Trailing empty cells from the sparse sheet are padding, not content.
        let mut padded = row.clone();
        padded.extend(vec![String::new(); 8]);
        assert_eq!(first, row_hash(&padded));
        // A column this module does not map must still separate two rows: real
        // files differ in `amount_liq` and the accounting columns, and a hash
        // over the mapped fields alone collapsed half of them.
        let mut other = row.clone();
        other[3] = "EUR adjusted".to_owned();
        assert_ne!(first, row_hash(&other));
    }

    /// Two rows that differ only in an unmapped column stay two awards.
    #[test]
    fn rows_differing_in_an_unmapped_column_are_both_kept() {
        let path = temp_path("unmapped");
        let shared = ["id_bce_org", "amount_eng", "amount_liq", "0207365709"];
        write_xlsx(
            &path,
            &[
                ('A', "id_bce_org"),
                ('B', "amount_eng"),
                ('C', "amount_liq"),
            ],
            &[
                vec![('A', "3", true), ('B', "10", false), ('C', "10", false)],
                vec![('A', "3", true), ('B', "10", false), ('C', "20", false)],
            ],
            &shared,
        );
        let awards = parse_inventory(&path).expect("fixture should parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(awards.len(), 2);
        assert_ne!(awards[0].row_hash, awards[1].row_hash);
    }

    /// A file from the same dataset with a different layout must be refused
    /// loudly, not imported as zero rows: the older French inventories carry no
    /// KBO numbers, so nothing in them can be joined to a company.
    #[test]
    fn refuses_a_layout_without_identifiers() {
        let path = temp_path("other-layout");
        let shared = [
            "Objet du marché",
            "Nom du fournisseur",
            "Montant du marché (TVA C.)",
            "Destinataire",
        ];
        write_xlsx(
            &path,
            &[
                ('A', "Objet du marché"),
                ('B', "Nom du fournisseur"),
                ('C', "Montant du marché (TVA C.)"),
                ('D', "Destinataire"),
            ],
            &[vec![
                ('A', "0", true),
                ('B', "1", true),
                ('C', "123329.25", false),
                ('D', "3", true),
            ]],
            &shared,
        );
        let error = parse_inventory(&path).expect_err("layout should be refused");
        let _ = std::fs::remove_file(&path);
        let message = error.to_string();
        assert!(
            message.contains("unrecognised inventory layout"),
            "{message}"
        );
        assert!(message.contains("Objet du marché"), "{message}");
        assert!(message.contains("first row"), "{message}");
    }

    #[test]
    fn column_references_map_to_indexes() {
        assert_eq!(column_index("A1"), Some(0));
        assert_eq!(column_index("C12"), Some(2));
        assert_eq!(column_index("AA3"), Some(26));
        assert_eq!(column_index("ab100"), Some(27));
        assert_eq!(column_index("12"), None);
    }
}
