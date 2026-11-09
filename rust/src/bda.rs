use crate::{parse_money, Tender, TenderRequirement};
use chrono::NaiveDate;
use quick_xml::events::Event;
use quick_xml::Reader;
use rust_decimal::Decimal;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use zip::ZipArchive;

type Document = (String, Vec<u8>);

/// Parsed import result: the tenders plus one (name, error) pair per skipped
/// unparseable document.
pub type ParsedBdaImport = (Vec<Tender>, Vec<(String, String)>, Vec<BdaAward>);

/// One awarded contract from a BDA award notice (migrated F03 eSenders XML
/// carries `AWARD_CONTRACT` blocks with winner + value; the new platform's
/// tender-file zips usually carry none, so absence is normal, not a gap).
/// Winner names are unresolved strings — no BCE number travels in the
/// notice; resolve via denomination join at query time (exact match works:
/// `RENEWI BELGIUM` → 0429.366.144, measured 2026-10-23).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BdaAward {
    pub source_name: String,
    pub notice_ref: Option<String>,
    pub winner_name: String,
    pub winner_town: Option<String>,
    pub winner_postcode: Option<String>,
    pub winner_country: Option<String>,
    pub value: Option<rust_decimal::Decimal>,
    /// Tag attribute; Belgian e-Notification is EUR-only in practice.
    pub currency: String,
    pub date_conclusion: Option<String>,
    pub tenders_received: Option<i64>,
    pub cpv: Option<String>,
    pub buyer: Option<String>,
}

/// Split raw XML into `<award_contract>…</award_contract>` spans,
/// case-insensitive (migrated notices use UPPER_SNAKE tags). Tags are ASCII
/// so indices from the lowered copy slice the original safely.
fn award_contract_spans(data: &str) -> Vec<&str> {
    let lowered = data.to_lowercase();
    let mut spans = Vec::new();
    let mut cursor = 0;
    while let Some(open) = lowered[cursor..].find("<award_contract") {
        let start = cursor + open;
        let body = match lowered[start..].find('>') {
            Some(end) => start + end + 1,
            None => break,
        };
        let end = lowered[body..]
            .find("</award_contract>")
            .map(|close| body + close + "</award_contract>".len());
        match end {
            Some(end) => {
                spans.push(&data[start..end]);
                cursor = end;
            }
            None => break,
        }
    }
    spans
}

fn span_text(span: &str, tag: &str) -> Option<String> {
    let lowered = span.to_lowercase();
    let open = format!("<{tag}");
    let start = lowered.find(open.as_str())?;
    let body = lowered[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = lowered[body..].find(close.as_str())? + body;
    let text = span[body..end].trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn span_attr(span: &str, tag: &str, attr: &str) -> Option<String> {
    let lowered = span.to_lowercase();
    let open = format!("<{tag}");
    let start = lowered.find(open.as_str())?;
    let tag_end = lowered[start..].find('>')? + start;
    let head = &span[start..tag_end];
    let needle = format!(r#"{attr}=""#);
    let head_lower = head.to_lowercase();
    let value_start = head_lower.find(needle.as_str())? + needle.len();
    let value_end = head[value_start..].find('"')? + value_start;
    let value = head[value_start..value_end].trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// `CODE="…"` attributes on `<CPV_CODE>` tags (eSenders shape, no text).
fn cpv_code_attributes(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let lowered = text.to_lowercase();
    let mut codes = Vec::new();
    let mut cursor = 0;
    while let Some(found) = lowered[cursor..].find("<cpv_code") {
        let start = cursor + found;
        let head_end = lowered[start..].find('>').map(|end| start + end);
        let Some(head_end) = head_end else { break };
        let head = &lowered[start..head_end];
        if let Some(value_start) = head.find("code=\"").map(|index| index + 6) {
            let rest = &text[start + value_start..];
            if let Some(value_end) = rest.find('"') {
                let code = rest[..value_end].trim().to_owned();
                if !code.is_empty() && !codes.contains(&code) {
                    codes.push(code);
                }
            }
        }
        cursor = head_end + 1;
    }
    codes
}

/// Awarded contracts in one BDA document. Empty for tender files and
/// competition notices — only award publications carry winners.
pub fn parse_bda_awards(data: &[u8], source_name: &str) -> Vec<BdaAward> {
    let text = String::from_utf8_lossy(data);
    let notice_ref = span_text(&text, "no_doc_ext");
    award_contract_spans(&text)
        .into_iter()
        .filter_map(|span| {
            let winner_name = span_text(span, "officialname")?;
            let value_text = span_text(span, "val_total");
            Some(BdaAward {
                source_name: source_name.to_owned(),
                notice_ref: notice_ref.clone(),
                winner_name,
                winner_town: span_text(span, "town"),
                winner_postcode: span_text(span, "postal_code"),
                winner_country: span_attr(span, "country", "value"),
                value: value_text
                    .as_deref()
                    .and_then(|value| value.replace(',', ".").parse().ok()),
                currency: span_attr(span, "val_total", "currency")
                    .unwrap_or_else(|| "EUR".to_owned()),
                date_conclusion: span_text(span, "date_conclusion_contract"),
                tenders_received: span_text(span, "nb_tenders_received")
                    .and_then(|value| value.parse().ok()),
                cpv: None,
                buyer: None,
            })
        })
        .collect()
}

const MAX_BDA_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_BDA_DOCUMENTS: usize = 10_000;
const MAX_BDA_TOTAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_BDA_ZIP_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BDA_XML_DEPTH: usize = 256;
const MAX_BDA_XML_ELEMENTS: usize = 100_000;
const MAX_BDA_XML_AGGREGATED_TEXT_BYTES: usize = 32 * 1024 * 1024;
/// Extracted PDF text is kept in `raw` and serialized back by the
/// tender-detail route; this cap bounds decompressed text independently of
/// the 8 MiB source-document limit. Sized for full besteks (measured
/// 2026-10-23: a 24-page bestek holds ~57k chars; the old 20k cap cut it
/// mid-sentence and lost the award criteria and technical lots).
const MAX_PDF_TEXT_CHARS: usize = 200_000;
/// Hard stop for emitted PDF text, enforced by the extraction writer.
/// This does not bound the PDF library's internal stream decoding allocations.
const MAX_PDF_EXTRACT_CHARS: usize = 250_000;

/// Counts bytes read while `ZipArchive::new` parses an archive's central
/// directory. The flag is disabled after construction so document contents
/// are governed by the decompressed-byte limits instead.
struct BoundedZipReader<R> {
    inner: R,
    metadata_phase: Arc<AtomicBool>,
    metadata_bytes_read: u64,
    metadata_limit: u64,
}

impl<R> BoundedZipReader<R> {
    fn new(inner: R, metadata_phase: Arc<AtomicBool>, metadata_limit: u64) -> Self {
        Self {
            inner,
            metadata_phase,
            metadata_bytes_read: 0,
            metadata_limit,
        }
    }
}

impl<R: Read> Read for BoundedZipReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes_read = self.inner.read(buffer)?;
        if self.metadata_phase.load(Ordering::Relaxed) {
            let total = self.metadata_bytes_read.saturating_add(bytes_read as u64);
            if total > self.metadata_limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "BDA ZIP metadata exceeded the {} byte limit",
                        self.metadata_limit
                    ),
                ));
            }
            self.metadata_bytes_read = total;
        }
        Ok(bytes_read)
    }
}

impl<R: Seek> Seek for BoundedZipReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

#[derive(Debug)]
struct XmlPath {
    name: String,
    currency_id: Option<String>,
    code_list_name: Option<String>,
    parent: Option<Arc<XmlPath>>,
    direct_text: OnceLock<String>,
}

#[derive(Debug)]
struct Node {
    path: Arc<XmlPath>,
    text: String,
    direct_text: String,
}

fn local_name(name: &[u8]) -> String {
    let value = String::from_utf8_lossy(name);
    value.rsplit(':').next().unwrap_or_default().to_lowercase()
}

fn normalized_text(value: &str) -> String {
    let mut words = value.split_whitespace();
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut normalized = String::with_capacity(value.len());
    normalized.push_str(first);
    for word in words {
        normalized.push(' ');
        normalized.push_str(word);
    }
    normalized
}

type PathElement = (Arc<XmlPath>, String);

fn append_xml_text(
    target: &mut String,
    value: &str,
    aggregate_bytes: &mut usize,
) -> Result<(), Box<dyn Error>> {
    let added = value.len().saturating_add(1);
    let total = aggregate_bytes.saturating_add(added);
    if total > MAX_BDA_XML_AGGREGATED_TEXT_BYTES {
        return Err(format!(
            "BDA XML exceeded the {MAX_BDA_XML_AGGREGATED_TEXT_BYTES} aggregated text limit"
        )
        .into());
    }
    target.push_str(value);
    target.push(' ');
    *aggregate_bytes = total;
    Ok(())
}

fn count_xml_element(
    count: &mut usize,
    depth: usize,
    roots: &mut usize,
) -> Result<(), Box<dyn Error>> {
    if depth == 1 {
        *roots += 1;
        if *roots > 1 {
            return Err("BDA XML contained multiple root elements".into());
        }
    }
    if depth > MAX_BDA_XML_DEPTH {
        return Err(format!("BDA XML exceeded the {MAX_BDA_XML_DEPTH} nesting-depth limit").into());
    }
    *count += 1;
    if *count > MAX_BDA_XML_ELEMENTS {
        return Err(format!("BDA XML exceeded the {MAX_BDA_XML_ELEMENTS} element limit").into());
    }
    Ok(())
}

fn source_code_list_name(
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> Result<Option<String>, Box<dyn Error>> {
    for attribute in element.attributes() {
        let attribute = attribute?;
        if local_name(attribute.key.as_ref()) == "listname" {
            return Ok(Some(
                attribute
                    .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn framework_currency_id(
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> Result<Option<String>, Box<dyn Error>> {
    for attribute in element.attributes() {
        let attribute = attribute?;
        if local_name(attribute.key.as_ref()) == "currencyid" {
            return Ok(Some(
                attribute
                    .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

fn element_texts_with_paths(data: &[u8]) -> Result<Vec<PathElement>, Box<dyn Error>> {
    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut completed = Vec::new();
    let mut element_count = 0;
    let mut root_count = 0;
    let mut aggregate_text_bytes = 0;
    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(element) => {
                count_xml_element(&mut element_count, stack.len() + 1, &mut root_count)?;
                let name = local_name(element.name().as_ref());
                let currency_id = if name == "frameworkmaximumamount" {
                    framework_currency_id(&element, reader.decoder())?
                } else {
                    None
                };
                let code_list_name = if matches!(
                    name.as_str(),
                    "tendererrequirementtypecode"
                        | "companylegalformcode"
                        | "executionrequirementcode"
                        | "requiredcurriculacode"
                        | "guaranteetypecode"
                ) {
                    source_code_list_name(&element, reader.decoder())?
                } else {
                    None
                };
                let path = Arc::new(XmlPath {
                    name,
                    currency_id,
                    code_list_name,
                    parent: stack.last().map(|node| Arc::clone(&node.path)),
                    direct_text: OnceLock::new(),
                });
                stack.push(Node {
                    path,
                    text: String::new(),
                    direct_text: String::new(),
                });
            }
            Event::Empty(element) => {
                count_xml_element(&mut element_count, stack.len() + 1, &mut root_count)?;
                let name = local_name(element.name().as_ref());
                let currency_id = if name == "frameworkmaximumamount" {
                    framework_currency_id(&element, reader.decoder())?
                } else {
                    None
                };
                let code_list_name = if matches!(
                    name.as_str(),
                    "tendererrequirementtypecode"
                        | "companylegalformcode"
                        | "executionrequirementcode"
                        | "requiredcurriculacode"
                        | "guaranteetypecode"
                ) {
                    source_code_list_name(&element, reader.decoder())?
                } else {
                    None
                };
                let path = Arc::new(XmlPath {
                    name,
                    currency_id,
                    code_list_name,
                    parent: stack.last().map(|node| Arc::clone(&node.path)),
                    direct_text: OnceLock::new(),
                });
                completed.push((path, String::new()));
            }
            Event::Text(text) => {
                if let Some(node) = stack.last_mut() {
                    let decoded = text.decode()?;
                    let unescaped = quick_xml::escape::unescape(&decoded)?;
                    append_xml_text(&mut node.text, &unescaped, &mut aggregate_text_bytes)?;
                    node.direct_text.push_str(&unescaped);
                    node.direct_text.push(' ');
                }
            }
            Event::CData(text) => {
                if let Some(node) = stack.last_mut() {
                    let value = String::from_utf8_lossy(&text);
                    append_xml_text(&mut node.text, &value, &mut aggregate_text_bytes)?;
                    node.direct_text.push_str(&value);
                    node.direct_text.push(' ');
                }
            }
            Event::End(_) => {
                if let Some(node) = stack.pop() {
                    let text = normalized_text(&node.text);
                    let direct_text = normalized_text(&node.direct_text);
                    let _ = node.path.direct_text.set(direct_text);
                    if !text.is_empty() {
                        completed.push((Arc::clone(&node.path), text.clone()));
                    }
                    if let Some(parent) = stack.last_mut() {
                        append_xml_text(&mut parent.text, &text, &mut aggregate_text_bytes)?;
                    }
                }
            }
            Event::Eof => {
                // Reject truncated documents (unclosed elements) so malformed
                // input is skipped instead of producing a garbage tender.
                if !stack.is_empty() {
                    return Err("XML document ended before all elements were closed".into());
                }
                break;
            }
            _ => {}
        }
        buffer.clear();
    }
    if element_count == 0 {
        return Err("XML document contains no elements".into());
    }
    Ok(completed)
}

fn first_text(elements: &[(String, String)], names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        elements
            .iter()
            .find(|(element, text)| element == name && !text.is_empty())
            .map(|(_, text)| text.clone())
    })
}

fn has_ancestor(path: &XmlPath, ancestor: &str) -> bool {
    let mut current = path.parent.as_deref();
    while let Some(node) = current {
        if node.name == ancestor {
            return true;
        }
        current = node.parent.as_deref();
    }
    false
}

fn has_parent_chain(path: &XmlPath, expected: &[&str]) -> bool {
    let mut parent = path.parent.as_deref();
    for name in expected {
        let Some(node) = parent.filter(|node| node.name == *name) else {
            return false;
        };
        parent = node.parent.as_deref();
    }
    true
}

fn scoped_text(elements: &[PathElement], name: &str, ancestor: &str) -> Option<String> {
    elements
        .iter()
        .find(|(path, text)| path.name == name && has_ancestor(path, ancestor) && !text.is_empty())
        .map(|(_, text)| text.clone())
}

fn direct_child_text(elements: &[PathElement], name: &str) -> Option<String> {
    elements
        .iter()
        .find(|(path, text)| {
            path.name == name
                && path
                    .parent
                    .as_ref()
                    .is_some_and(|parent| parent.parent.is_none())
                && !text.is_empty()
        })
        .map(|(_, text)| text.clone())
}

/// Retain notice-level lot CPVs as evidence without flattening them into a
/// ranking feature. The global CPV list remains authoritative for current
/// matching; lot associations are useful for later evaluation and review.
fn lot_cpv_evidence(elements: &[PathElement]) -> Vec<(Option<String>, Vec<String>)> {
    let mut by_lot = HashMap::<*const XmlPath, usize>::new();
    let mut lots = Vec::<(Option<String>, Vec<String>)>::new();
    for (path, text) in elements {
        let mut ancestor = path.parent.as_deref();
        let mut lot = None;
        while let Some(node) = ancestor {
            if node.name == "procurementprojectlot" {
                lot = Some(node);
                break;
            }
            ancestor = node.parent.as_deref();
        }
        let Some(lot) = lot else { continue };
        let key = lot as *const XmlPath;
        let index = *by_lot.entry(key).or_insert_with(|| {
            lots.push((None, Vec::new()));
            lots.len() - 1
        });
        if path.name == "id"
            && path
                .parent
                .as_ref()
                .is_some_and(|parent| std::ptr::eq(parent.as_ref(), lot))
            && lots[index].0.is_none()
            && !text.is_empty()
        {
            lots[index].0 = Some(text.clone());
        }
        if matches!(path.name.as_str(), "itemclassificationcode" | "cpv") && !text.is_empty() {
            let code = text.split('-').next().unwrap_or(text).trim().to_owned();
            if !code.is_empty() && !lots[index].1.contains(&code) {
                lots[index].1.push(code);
            }
        }
    }
    lots.retain(|(_, codes)| !codes.is_empty());
    lots
}

const MAX_LOT_PLACE_EVIDENCE: usize = 100;
const MAX_LOT_PLACE_CODES_PER_LOT: usize = 10;

/// Keep only structured region codes under a lot's own realized location.
/// Notice-wide regions and buyer addresses are not assigned to lots.
fn lot_place_evidence(elements: &[PathElement]) -> (Vec<serde_json::Value>, bool) {
    let mut by_lot = HashMap::<*const XmlPath, usize>::new();
    let mut lots = Vec::<(Option<String>, Vec<String>)>::new();
    let mut truncated = false;

    for (path, text) in elements {
        let mut ancestor = Some(path.as_ref());
        let mut lot = None;
        while let Some(node) = ancestor {
            if node.name == "procurementprojectlot" {
                lot = Some(node);
                break;
            }
            ancestor = node.parent.as_deref();
        }
        let Some(lot) = lot else { continue };
        let key = lot as *const XmlPath;
        let index = if let Some(index) = by_lot.get(&key) {
            *index
        } else {
            if lots.len() == MAX_LOT_PLACE_EVIDENCE {
                truncated = true;
                continue;
            }
            let index = lots.len();
            lots.push((None, Vec::new()));
            by_lot.insert(key, index);
            index
        };

        if path.name == "id"
            && path
                .parent
                .as_ref()
                .is_some_and(|parent| std::ptr::eq(parent.as_ref(), lot))
            && lots[index].0.is_none()
            && !text.trim().is_empty()
        {
            lots[index].0 = Some(text.clone());
        }
        if path.name == "countrysubentitycode"
            && !text.trim().is_empty()
            && has_ancestor(path, "realizedlocation")
            && !lots[index].1.contains(text)
        {
            if lots[index].1.len() == MAX_LOT_PLACE_CODES_PER_LOT {
                truncated = true;
                continue;
            }
            lots[index].1.push(text.clone());
        }
    }

    let evidence = lots
        .into_iter()
        .map(|(lot_id, country_subentity_codes)| {
            json!({"lot_id": lot_id, "country_subentity_codes": country_subentity_codes})
        })
        .collect();
    (evidence, truncated)
}

const MAX_FRAMEWORK_MAXIMUM_EVIDENCE: usize = 100;

fn framework_maximum_evidence(elements: &[PathElement]) -> (Vec<serde_json::Value>, bool) {
    let mut evidence = Vec::new();
    let mut truncated = false;
    for (path, amount) in elements
        .iter()
        .filter(|(path, amount)| path.name == "frameworkmaximumamount" && !amount.is_empty())
    {
        if evidence.len() == MAX_FRAMEWORK_MAXIMUM_EVIDENCE {
            truncated = true;
            break;
        }
        let mut ancestor = path.parent.as_deref();
        let mut lot = None;
        while let Some(node) = ancestor {
            if node.name == "procurementprojectlot" {
                lot = Some(node);
                break;
            }
            ancestor = node.parent.as_deref();
        }
        let lot_id = lot.and_then(|lot| {
            elements
                .iter()
                .find(|(candidate, text)| {
                    candidate.name == "id"
                        && !text.is_empty()
                        && candidate
                            .parent
                            .as_ref()
                            .is_some_and(|parent| std::ptr::eq(parent.as_ref(), lot))
                })
                .map(|(_, text)| text.clone())
        });
        evidence.push(json!({
            "amount": amount,
            "currency_id": path.currency_id,
            "scope": if lot.is_some() { "lot" } else { "notice" },
            "lot_id": lot_id,
        }));
    }
    (evidence, truncated)
}

const MAX_LOT_AWARD_CRITERIA_EVIDENCE: usize = 100;
const MAX_LOT_AWARD_CRITERION_PARAMETERS: usize = 10;
const MAX_LOT_AWARD_CRITERION_TEXT_CHARS: usize = 2_000;

/// Preserve the source hierarchy of evaluation criteria declared inside each
/// lot. These judge offers; they are not bidder qualifications or requirements.
fn lot_award_criteria_evidence(elements: &[PathElement]) -> (Vec<serde_json::Value>, bool) {
    let criterion_name =
        |name: &str| matches!(name, "awardingcriterion" | "subordinateawardingcriterion");
    let mut direct_text = HashMap::<(*const XmlPath, String), String>::new();
    for (path, text) in elements.iter().filter(|(_, text)| !text.trim().is_empty()) {
        if let Some(parent) = path.parent.as_ref() {
            direct_text
                .entry((Arc::as_ptr(parent), path.name.clone()))
                .or_insert_with(|| text.clone());
        }
    }
    let mut lots = Vec::<(Arc<XmlPath>, Option<String>, Vec<serde_json::Value>)>::new();
    let mut lot_indexes = HashMap::<*const XmlPath, usize>::new();
    let mut truncated = false;

    for (path, _) in elements
        .iter()
        .filter(|(path, _)| path.name == "procurementprojectlot")
    {
        let key = Arc::as_ptr(path);
        if lot_indexes.contains_key(&key) {
            continue;
        }
        if lots.len() == MAX_LOT_PLACE_EVIDENCE {
            truncated = true;
            continue;
        }
        let lot_id = direct_text.get(&(key, "id".to_owned())).cloned();
        lot_indexes.insert(key, lots.len());
        lots.push((Arc::clone(path), lot_id, Vec::new()));
    }

    let mut nodes = Vec::new();
    for (order, (path, _)) in elements.iter().enumerate() {
        if !criterion_name(&path.name)
            || !has_ancestor(path, "tenderingterms")
            || !has_ancestor(path, "awardingterms")
        {
            continue;
        }
        let mut ancestor = path.parent.as_deref();
        let mut lot = None;
        let mut parent_criterion = None;
        while let Some(node) = ancestor {
            if lot.is_none() && node.name == "procurementprojectlot" {
                lot = Some(node);
            }
            if parent_criterion.is_none() && criterion_name(&node.name) {
                parent_criterion = Some(node as *const XmlPath);
            }
            ancestor = node.parent.as_deref();
        }
        let Some(lot) = lot else { continue };
        let lot_key = lot as *const XmlPath;
        if !lot_indexes.contains_key(&lot_key) {
            truncated = true;
            continue;
        }
        let depth = {
            let mut current = path.parent.as_deref();
            let mut depth = 0;
            while let Some(node) = current {
                if criterion_name(&node.name) {
                    depth += 1;
                }
                current = node.parent.as_deref();
            }
            depth
        };
        nodes.push((path, lot_key, parent_criterion, order, depth));
    }
    let criterion_keys = nodes
        .iter()
        .map(|(path, _, _, _, _)| Arc::as_ptr(path))
        .collect::<HashSet<_>>();
    let mut parameters_by_criterion = HashMap::<*const XmlPath, Vec<Arc<XmlPath>>>::new();
    let mut parameter_lists_truncated = HashSet::new();
    for (path, _) in elements
        .iter()
        .filter(|(path, _)| path.name == "awardcriterionparameter")
    {
        let mut current = path.parent.as_deref();
        let mut nearest_criterion = None;
        while let Some(node) = current {
            if criterion_name(&node.name) {
                nearest_criterion = Some(node as *const XmlPath);
                break;
            }
            current = node.parent.as_deref();
        }
        let Some(criterion_key) = nearest_criterion.filter(|key| criterion_keys.contains(key))
        else {
            continue;
        };
        let parameters = parameters_by_criterion.entry(criterion_key).or_default();
        if parameters.len() == MAX_LOT_AWARD_CRITERION_PARAMETERS {
            parameter_lists_truncated.insert(criterion_key);
        } else {
            parameters.push(Arc::clone(path));
        }
    }
    // Parent paths are assigned before children. Within each sibling group,
    // the XML completion order preserves source order.
    nodes.sort_by_key(|(_, _, _, order, depth)| (*depth, *order));

    let mut source_paths = HashMap::<*const XmlPath, Vec<usize>>::new();
    let mut sibling_counts = HashMap::<(*const XmlPath, Option<*const XmlPath>), usize>::new();
    for (emitted, (path, lot_key, parent_criterion, _, _)) in nodes.into_iter().enumerate() {
        if emitted == MAX_LOT_AWARD_CRITERIA_EVIDENCE {
            truncated = true;
            break;
        }
        let node_key = Arc::as_ptr(path);
        let mut ordinal_path = parent_criterion
            .and_then(|parent| source_paths.get(&parent).cloned())
            .unwrap_or_default();
        let sibling = sibling_counts
            .entry((lot_key, parent_criterion))
            .or_default();
        ordinal_path.push(*sibling);
        *sibling += 1;
        source_paths.insert(node_key, ordinal_path.clone());

        let direct_child_text = |name: &str| direct_text.get(&(node_key, name.to_owned())).cloned();
        let bounded_text = |value: Option<String>, truncated: &mut bool| {
            value.map(|value| {
                if value.chars().count() > MAX_LOT_AWARD_CRITERION_TEXT_CHARS {
                    *truncated = true;
                    value
                        .chars()
                        .take(MAX_LOT_AWARD_CRITERION_TEXT_CHARS)
                        .collect()
                } else {
                    value
                }
            })
        };

        let mut parameters = Vec::new();
        if parameter_lists_truncated.contains(&node_key) {
            truncated = true;
        }
        for parameter_path in parameters_by_criterion.get(&node_key).into_iter().flatten() {
            let parameter_key = Arc::as_ptr(parameter_path);
            let code = bounded_text(
                direct_text
                    .get(&(parameter_key, "parametercode".to_owned()))
                    .cloned(),
                &mut truncated,
            );
            let numeric = bounded_text(
                direct_text
                    .get(&(parameter_key, "parameternumeric".to_owned()))
                    .cloned(),
                &mut truncated,
            );
            if code.is_some() || numeric.is_some() {
                parameters.push(json!({"code": code, "numeric": numeric}));
            }
        }

        let index = lot_indexes[&lot_key];
        lots[index].2.push(json!({
            "source_element": if path.name == "awardingcriterion" {
                "AwardingCriterion"
            } else {
                "SubordinateAwardingCriterion"
            },
            "source_ordinal_path": ordinal_path,
            "type_code": bounded_text(direct_child_text("awardingcriteriontypecode"), &mut truncated),
            "description": bounded_text(direct_child_text("description"), &mut truncated),
            "calculation_expression": bounded_text(direct_child_text("calculationexpression"), &mut truncated),
            "parameters": parameters,
        }));
    }

    let evidence = lots
        .into_iter()
        .map(|(_, lot_id, criteria)| json!({"lot_id": lot_id, "criteria": criteria}))
        .collect();
    (evidence, truncated)
}

const MAX_LOT_SELECTION_EVIDENCE_RECORDS: usize = 100;
const MAX_LOT_SELECTION_TEXT_CHARS: usize = 2_000;
const MAX_LOT_SELECTION_DESCRIPTIONS_PER_RECORD: usize = 10;

/// Preserve source-defined lot selection evidence without converting codes or
/// descriptions into eligibility conclusions. Other lot requirement families
/// remain separate and are not inferred from these records.
fn lot_selection_evidence(elements: &[PathElement]) -> (Vec<serde_json::Value>, bool) {
    let mut direct_text = HashMap::<(*const XmlPath, String), String>::new();
    let mut direct_paths = HashMap::<(*const XmlPath, String), Arc<XmlPath>>::new();
    let mut text_by_path = HashMap::<*const XmlPath, String>::new();
    let mut code_list_name_by_path = HashMap::<*const XmlPath, Option<String>>::new();
    let mut parents_with_children = HashSet::<*const XmlPath>::new();
    for (path, text) in elements {
        let key = Arc::as_ptr(path);
        code_list_name_by_path.insert(key, path.code_list_name.clone());
        if !text.trim().is_empty() {
            text_by_path.insert(key, text.clone());
            if let Some(parent) = path.parent.as_ref() {
                direct_text
                    .entry((Arc::as_ptr(parent), path.name.clone()))
                    .or_insert_with(|| text.clone());
            }
        }
        if let Some(parent) = path.parent.as_ref() {
            let parent_key = Arc::as_ptr(parent);
            parents_with_children.insert(parent_key);
            direct_paths
                .entry((parent_key, path.name.clone()))
                .or_insert_with(|| Arc::clone(path));
        }
    }

    let mut lots = Vec::<(Option<String>, Vec<serde_json::Value>)>::new();
    let mut lot_indexes = HashMap::<*const XmlPath, usize>::new();
    let mut truncated = false;
    for (path, _) in elements
        .iter()
        .filter(|(path, _)| path.name == "procurementprojectlot")
    {
        let lot_key = Arc::as_ptr(path);
        if lot_indexes.contains_key(&lot_key) {
            continue;
        }
        if lots.len() == MAX_LOT_PLACE_EVIDENCE {
            truncated = true;
            continue;
        }
        let lot_id = direct_text.get(&(lot_key, "id".to_owned())).cloned();
        lot_indexes.insert(lot_key, lots.len());
        lots.push((lot_id, Vec::new()));
    }

    let lot_ancestor = |path: &XmlPath| {
        let mut current = path.parent.as_deref();
        while let Some(node) = current {
            if node.name == "procurementprojectlot" {
                return Some(node as *const XmlPath);
            }
            current = node.parent.as_deref();
        }
        None
    };
    let mut qualification_request_ordinals = HashMap::<*const XmlPath, usize>::new();
    let mut qualification_request_counts = HashMap::<*const XmlPath, usize>::new();
    let mut eforms_extension_ordinals = HashMap::<*const XmlPath, usize>::new();
    let mut eforms_extension_counts = HashMap::<*const XmlPath, usize>::new();
    for (path, _) in elements {
        let supported_parent_chain = match path.name.as_str() {
            "tendererqualificationrequest" => has_parent_chain(path, &["tenderingterms"]),
            "eformsextension" => has_parent_chain(
                path,
                &[
                    "extensioncontent",
                    "ublextension",
                    "ublextensions",
                    "tenderingterms",
                ],
            ),
            _ => false,
        };
        if !supported_parent_chain {
            continue;
        }
        let Some(lot_key) = lot_ancestor(path) else {
            continue;
        };
        if !lot_indexes.contains_key(&lot_key) {
            continue;
        }
        let counts = if path.name == "tendererqualificationrequest" {
            &mut qualification_request_counts
        } else {
            &mut eforms_extension_counts
        };
        let ordinals = if path.name == "tendererqualificationrequest" {
            &mut qualification_request_ordinals
        } else {
            &mut eforms_extension_ordinals
        };
        let ordinal = counts.entry(lot_key).or_default();
        ordinals.insert(Arc::as_ptr(path), *ordinal);
        *ordinal += 1;
    }

    let mut leaf_descriptions_by_selection = HashMap::<*const XmlPath, Vec<String>>::new();
    for (path, text) in elements
        .iter()
        .filter(|(path, text)| path.name == "description" && !text.trim().is_empty())
    {
        let description_key = Arc::as_ptr(path);
        if parents_with_children.contains(&description_key) {
            continue;
        }
        let mut current = path.parent.as_deref();
        let mut selection = None;
        let mut in_lot = false;
        while let Some(node) = current {
            if selection.is_none() && node.name == "selectioncriteria" {
                selection = Some(node as *const XmlPath);
            }
            if node.name == "procurementprojectlot" {
                in_lot = true;
                break;
            }
            current = node.parent.as_deref();
        }
        let Some(selection_key) = selection.filter(|_| in_lot) else {
            continue;
        };
        let descriptions = leaf_descriptions_by_selection
            .entry(selection_key)
            .or_default();
        if descriptions.len() == MAX_LOT_SELECTION_DESCRIPTIONS_PER_RECORD {
            truncated = true;
            continue;
        }
        descriptions.push(text.clone());
    }

    let mut child_ordinals = HashMap::<(*const XmlPath, String), usize>::new();
    let mut selection_ordinals = HashMap::<(*const XmlPath, *const XmlPath), usize>::new();
    let mut emitted = 0;
    for (path, _) in elements {
        let source_element = match path.name.as_str() {
            "specifictendererrequirement"
                if has_parent_chain(path, &["tendererqualificationrequest", "tenderingterms"]) =>
            {
                "SpecificTendererRequirement"
            }
            "companylegalformcode"
                if has_parent_chain(path, &["tendererqualificationrequest", "tenderingterms"]) =>
            {
                "CompanyLegalFormCode"
            }
            "selectioncriteria"
                if has_parent_chain(
                    path,
                    &[
                        "eformsextension",
                        "extensioncontent",
                        "ublextension",
                        "ublextensions",
                        "tenderingterms",
                    ],
                ) =>
            {
                "SelectionCriteria"
            }
            _ => continue,
        };
        if !has_ancestor(path, "tenderingterms") {
            continue;
        }
        let Some(lot_key) = lot_ancestor(path) else {
            continue;
        };
        let Some(lot_index) = lot_indexes.get(&lot_key).copied() else {
            truncated = true;
            continue;
        };
        if emitted == MAX_LOT_SELECTION_EVIDENCE_RECORDS {
            truncated = true;
            break;
        }

        let ordinal_path = match source_element {
            "SpecificTendererRequirement" | "CompanyLegalFormCode" => {
                let parent_key = path
                    .parent
                    .as_ref()
                    .map(Arc::as_ptr)
                    .expect("source path matched a direct qualification child");
                let Some(parent_ordinal) = qualification_request_ordinals.get(&parent_key).copied()
                else {
                    truncated = true;
                    continue;
                };
                let child_ordinal = child_ordinals
                    .entry((parent_key, path.name.clone()))
                    .or_default();
                let ordinal_path = vec![parent_ordinal, *child_ordinal];
                *child_ordinal += 1;
                ordinal_path
            }
            _ => {
                let parent_key = path
                    .parent
                    .as_ref()
                    .map(Arc::as_ptr)
                    .expect("source path matched a direct eForms selection child");
                let Some(parent_ordinal) = eforms_extension_ordinals.get(&parent_key).copied()
                else {
                    truncated = true;
                    continue;
                };
                let selection_ordinal =
                    selection_ordinals.entry((lot_key, parent_key)).or_default();
                let ordinal_path = vec![parent_ordinal, *selection_ordinal];
                *selection_ordinal += 1;
                ordinal_path
            }
        };

        let (code, code_list_name) = if source_element == "CompanyLegalFormCode" {
            (
                text_by_path.get(&Arc::as_ptr(path)).cloned(),
                path.code_list_name.clone(),
            )
        } else {
            let code_path_key = (Arc::as_ptr(path), "tendererrequirementtypecode".to_owned());
            let code = direct_text.get(&code_path_key).cloned();
            let list_name = direct_paths
                .get(&code_path_key)
                .and_then(|code_path| code_list_name_by_path.get(&Arc::as_ptr(code_path)))
                .cloned()
                .flatten();
            (code, list_name)
        };
        let descriptions = if source_element == "SelectionCriteria" {
            leaf_descriptions_by_selection
                .remove(&Arc::as_ptr(path))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let source_path = match source_element {
            "SpecificTendererRequirement" => {
                "TenderingTerms/TendererQualificationRequest/SpecificTendererRequirement"
            }
            "CompanyLegalFormCode" => {
                "TenderingTerms/TendererQualificationRequest/CompanyLegalFormCode"
            }
            _ => "TenderingTerms/UBLExtensions/UBLExtension/ExtensionContent/EformsExtension/SelectionCriteria",
        };
        let bounded_text = |value: Option<String>, truncated: &mut bool| {
            value.map(|value| {
                if value.chars().count() > MAX_LOT_SELECTION_TEXT_CHARS {
                    *truncated = true;
                    value.chars().take(MAX_LOT_SELECTION_TEXT_CHARS).collect()
                } else {
                    value
                }
            })
        };
        let code = bounded_text(code, &mut truncated);
        let code_list_name = bounded_text(code_list_name, &mut truncated);
        let descriptions = descriptions
            .into_iter()
            .map(|value| bounded_text(Some(value), &mut truncated).unwrap_or_default())
            .collect::<Vec<_>>();
        lots[lot_index].1.push(json!({
            "source_element": source_element,
            "source_path": source_path,
            "source_ordinal_path": ordinal_path,
            "code": code,
            "code_list_name": code_list_name,
            "descriptions": descriptions,
        }));
        emitted += 1;
    }

    let evidence = lots
        .into_iter()
        .map(|(lot_id, selection_evidence)| {
            json!({"lot_id": lot_id, "selection_evidence": selection_evidence})
        })
        .collect();
    (evidence, truncated)
}

const MAX_LOT_CONTRACT_EXECUTION_CONDITIONS: usize = 100;
const MAX_LOT_CONTRACT_EXECUTION_CODE_CHARS: usize = 256;
const MAX_LOT_CONTRACT_EXECUTION_LIST_NAME_CHARS: usize = 128;

/// Preserve source-coded performance conditions separately from bidder
/// selection criteria. Notice-wide conditions are not assigned to a lot.
fn lot_contract_execution_evidence(elements: &[PathElement]) -> (Vec<serde_json::Value>, bool) {
    let mut by_lot = HashMap::<*const XmlPath, usize>::new();
    let mut lots = Vec::<(Option<String>, Vec<serde_json::Value>)>::new();
    let mut truncated = false;

    for (path, _) in elements
        .iter()
        .filter(|(path, _)| path.name == "procurementprojectlot")
    {
        let key = Arc::as_ptr(path);
        if lots.len() == MAX_LOT_PLACE_EVIDENCE {
            truncated = true;
            continue;
        }
        by_lot.insert(key, lots.len());
        lots.push((None, Vec::new()));
    }

    let mut condition_ordinals = HashMap::<*const XmlPath, usize>::new();
    let mut emitted = 0;
    for (path, _) in elements.iter().filter(|(path, _)| {
        path.name == "contractexecutionrequirement" && has_parent_chain(path, &["tenderingterms"])
    }) {
        let mut ancestor = path.parent.as_deref();
        let mut lot_key = None;
        while let Some(node) = ancestor {
            if node.name == "procurementprojectlot" {
                lot_key = Some(node as *const XmlPath);
                break;
            }
            ancestor = node.parent.as_deref();
        }
        let Some(lot_key) = lot_key else { continue };
        let Some(lot_index) = by_lot.get(&lot_key).copied() else {
            truncated = true;
            continue;
        };
        if emitted == MAX_LOT_CONTRACT_EXECUTION_CONDITIONS {
            truncated = true;
            break;
        }
        let ordinal = condition_ordinals.entry(lot_key).or_default();
        let source_ordinal = *ordinal;
        *ordinal += 1;

        let code_element = elements.iter().find(|(candidate, _)| {
            candidate.name == "executionrequirementcode"
                && candidate
                    .parent
                    .as_ref()
                    .is_some_and(|parent| std::ptr::eq(parent.as_ref(), path.as_ref()))
        });
        let mut bounded = |value: Option<String>, limit: usize| {
            value.map(|value| {
                if value.chars().count() > limit {
                    truncated = true;
                    value.chars().take(limit).collect::<String>()
                } else {
                    value
                }
            })
        };
        let code = code_element.and_then(|(_, text)| {
            (!text.trim().is_empty())
                .then(|| bounded(Some(text.clone()), MAX_LOT_CONTRACT_EXECUTION_CODE_CHARS))
                .flatten()
        });
        let code_list_name = code_element
            .and_then(|(code_path, _)| code_path.code_list_name.clone())
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| bounded(Some(value), MAX_LOT_CONTRACT_EXECUTION_LIST_NAME_CHARS));
        lots[lot_index].1.push(json!({
            "source_element": "ContractExecutionRequirement",
            "source_path": "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode",
            "source_ordinal_path": [source_ordinal],
            "code": code,
            "code_list_name": code_list_name,
        }));
        emitted += 1;
    }

    for (path, text) in elements {
        if path.name != "id" || text.trim().is_empty() {
            continue;
        }
        let Some(parent) = path.parent.as_ref() else {
            continue;
        };
        if parent.name != "procurementprojectlot" {
            continue;
        }
        if let Some(index) = by_lot.get(&Arc::as_ptr(parent)) {
            if lots[*index].0.is_none() {
                lots[*index].0 = Some(text.clone());
            }
        }
    }

    let evidence = lots
        .into_iter()
        .map(|(lot_id, conditions)| json!({"lot_id": lot_id, "conditions": conditions}))
        .collect();
    (evidence, truncated)
}

const MAX_LOT_SUBSEQUENT_PROCESS_REQUIREMENTS: usize = 100;
const MAX_LOT_SUBSEQUENT_PROCESS_NAME_CHARS: usize = 2_000;

/// Preserve the source's later-stage framework agreement requirement names.
/// Keep these distinct from initial selection and contract-performance terms.
fn lot_subsequent_process_evidence(elements: &[PathElement]) -> (Vec<serde_json::Value>, bool) {
    let mut direct_text = HashMap::<(*const XmlPath, String), String>::new();
    let mut lots = Vec::<(Option<String>, Vec<serde_json::Value>)>::new();
    let mut lot_indexes = HashMap::<*const XmlPath, usize>::new();
    let mut truncated = false;
    for (path, _) in elements
        .iter()
        .filter(|(path, _)| path.name == "procurementprojectlot")
    {
        let key = Arc::as_ptr(path);
        if lots.len() == MAX_LOT_PLACE_EVIDENCE {
            truncated = true;
            continue;
        }
        lot_indexes.insert(key, lots.len());
        lots.push((None, Vec::new()));
    }
    for (path, text) in elements {
        if !text.trim().is_empty() {
            if let Some(parent) = path.parent.as_ref() {
                direct_text
                    .entry((Arc::as_ptr(parent), path.name.clone()))
                    .or_insert_with(|| text.clone());
            }
        }
    }

    let mut ordinals = HashMap::<*const XmlPath, usize>::new();
    let mut emitted = 0;
    for (path, _) in elements.iter().filter(|(path, _)| {
        path.name == "subsequentprocesstenderrequirement"
            && has_parent_chain(path, &["frameworkagreement", "tenderingprocess"])
    }) {
        let mut ancestor = path.parent.as_deref();
        let mut lot_key = None;
        while let Some(node) = ancestor {
            if node.name == "procurementprojectlot" {
                lot_key = Some(node as *const XmlPath);
                break;
            }
            ancestor = node.parent.as_deref();
        }
        let Some(lot_key) = lot_key else { continue };
        let Some(lot_index) = lot_indexes.get(&lot_key).copied() else {
            truncated = true;
            continue;
        };
        if emitted == MAX_LOT_SUBSEQUENT_PROCESS_REQUIREMENTS {
            truncated = true;
            break;
        }
        let ordinal = ordinals.entry(lot_key).or_default();
        let source_ordinal = *ordinal;
        *ordinal += 1;
        let name = direct_text
            .get(&(Arc::as_ptr(path), "name".to_owned()))
            .cloned();
        let name = name.map(|value| {
            if value.chars().count() > MAX_LOT_SUBSEQUENT_PROCESS_NAME_CHARS {
                truncated = true;
                value
                    .chars()
                    .take(MAX_LOT_SUBSEQUENT_PROCESS_NAME_CHARS)
                    .collect::<String>()
            } else {
                value
            }
        });
        lots[lot_index].1.push(json!({
            "source_element": "SubsequentProcessTenderRequirement",
            "source_path": "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name",
            "source_ordinal_path": [source_ordinal],
            "name": name,
        }));
        emitted += 1;
    }

    for (path, text) in elements {
        if path.name != "id" || text.trim().is_empty() {
            continue;
        }
        let Some(parent) = path.parent.as_ref() else {
            continue;
        };
        if parent.name == "procurementprojectlot" {
            if let Some(index) = lot_indexes.get(&Arc::as_ptr(parent)) {
                if lots[*index].0.is_none() {
                    lots[*index].0 = Some(text.clone());
                }
            }
        }
    }
    let evidence = lots
        .into_iter()
        .map(|(lot_id, requirements)| json!({"lot_id": lot_id, "requirements": requirements}))
        .collect();
    (evidence, truncated)
}

const MAX_LOT_GUARANTEE_CURRICULA_RECORDS: usize = 100;
const MAX_LOT_GUARANTEE_CURRICULA_TEXT_CHARS: usize = 2_000;

/// Preserve source-coded guarantees and required curricula as separate facts.
/// Neither is interpreted as bidder eligibility or evidence of compliance.
fn lot_guarantee_curricula_evidence(elements: &[PathElement]) -> (Vec<serde_json::Value>, bool) {
    let mut direct_text = HashMap::<(*const XmlPath, String), String>::new();
    for (path, text) in elements {
        if !text.trim().is_empty() {
            if let Some(parent) = path.parent.as_ref() {
                direct_text
                    .entry((Arc::as_ptr(parent), path.name.clone()))
                    .or_insert_with(|| text.clone());
            }
        }
    }
    let mut lots = Vec::<(
        Option<String>,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    )>::new();
    let mut lot_indexes = HashMap::<*const XmlPath, usize>::new();
    let mut truncated = false;
    for (path, _) in elements
        .iter()
        .filter(|(path, _)| path.name == "procurementprojectlot")
    {
        let key = Arc::as_ptr(path);
        if lots.len() == MAX_LOT_PLACE_EVIDENCE {
            truncated = true;
            continue;
        }
        lot_indexes.insert(key, lots.len());
        lots.push((None, Vec::new(), Vec::new()));
    }
    let lot_ancestor = |path: &XmlPath| {
        let mut current = path.parent.as_deref();
        while let Some(node) = current {
            if node.name == "procurementprojectlot" {
                return Some(node as *const XmlPath);
            }
            current = node.parent.as_deref();
        }
        None
    };
    let mut guarantee_ordinals = HashMap::<*const XmlPath, usize>::new();
    let mut curricula_ordinals = HashMap::<*const XmlPath, usize>::new();
    let mut emitted = 0;
    for (path, _) in elements.iter().filter(|(path, _)| {
        matches!(
            path.name.as_str(),
            "requiredfinancialguarantee" | "requiredcurriculacode"
        ) && has_parent_chain(path, &["tenderingterms"])
    }) {
        let Some(lot_key) = lot_ancestor(path) else {
            continue;
        };
        let Some(lot_index) = lot_indexes.get(&lot_key).copied() else {
            truncated = true;
            continue;
        };
        if emitted == MAX_LOT_GUARANTEE_CURRICULA_RECORDS {
            truncated = true;
            break;
        }
        let is_guarantee = path.name == "requiredfinancialguarantee";
        let (source_ordinal, code, code_list_name, description) = if is_guarantee {
            let ordinal = guarantee_ordinals.entry(lot_key).or_default();
            let source_ordinal = *ordinal;
            *ordinal += 1;
            let code_path = elements.iter().find(|(candidate, _)| {
                candidate.name == "guaranteetypecode"
                    && candidate
                        .parent
                        .as_ref()
                        .is_some_and(|parent| std::ptr::eq(parent.as_ref(), path.as_ref()))
            });
            let code = code_path
                .and_then(|(code_path, _)| code_path.direct_text.get().cloned())
                .filter(|text| !text.trim().is_empty());
            let code_list_name = code_path.and_then(|(code_path, _)| {
                code_path
                    .code_list_name
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned)
            });
            let description = code_path.and_then(|(code_path, _)| {
                direct_text
                    .get(&(Arc::as_ptr(code_path), "description".to_owned()))
                    .cloned()
            });
            (source_ordinal, code, code_list_name, description)
        } else {
            let ordinal = curricula_ordinals.entry(lot_key).or_default();
            let source_ordinal = *ordinal;
            *ordinal += 1;
            let code = path
                .direct_text
                .get()
                .cloned()
                .filter(|text| !text.trim().is_empty());
            let code_list_name = path
                .code_list_name
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned);
            (source_ordinal, code, code_list_name, None)
        };
        let bounded = |value: Option<String>, truncated: &mut bool| {
            value.map(|value| {
                if value.chars().count() > MAX_LOT_GUARANTEE_CURRICULA_TEXT_CHARS {
                    *truncated = true;
                    value
                        .chars()
                        .take(MAX_LOT_GUARANTEE_CURRICULA_TEXT_CHARS)
                        .collect::<String>()
                } else {
                    value
                }
            })
        };
        if is_guarantee {
            lots[lot_index].1.push(json!({
                "source_element": "RequiredFinancialGuarantee",
                "source_path": "TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode",
                "source_ordinal_path": [source_ordinal],
                "code": bounded(code, &mut truncated),
                "code_list_name": bounded(code_list_name, &mut truncated),
                "description": bounded(description, &mut truncated),
            }));
        } else {
            lots[lot_index].2.push(json!({
                "source_element": "RequiredCurriculaCode",
                "source_path": "TenderingTerms/RequiredCurriculaCode",
                "source_ordinal_path": [source_ordinal],
                "code": bounded(code, &mut truncated),
                "code_list_name": bounded(code_list_name, &mut truncated),
            }));
        }
        emitted += 1;
    }
    for (path, text) in elements {
        if path.name != "id" || text.trim().is_empty() {
            continue;
        }
        let Some(parent) = path.parent.as_ref() else {
            continue;
        };
        if parent.name == "procurementprojectlot" {
            if let Some(index) = lot_indexes.get(&Arc::as_ptr(parent)) {
                if lots[*index].0.is_none() {
                    lots[*index].0 = Some(text.clone());
                }
            }
        }
    }
    let evidence = lots
        .into_iter()
        .map(|(lot_id, guarantees, curricula)| {
            json!({"lot_id": lot_id, "guarantees": guarantees, "curricula": curricula})
        })
        .collect();
    (evidence, truncated)
}

fn notice_lot_ids_are_unique(elements: &[PathElement]) -> bool {
    let mut ids_by_lot = HashMap::<*const XmlPath, Vec<String>>::new();
    for (path, text) in elements {
        let mut ancestor = Some(path.as_ref());
        let mut lot = None;
        while let Some(node) = ancestor {
            if node.name == "procurementprojectlot" {
                lot = Some(node);
                break;
            }
            ancestor = node.parent.as_deref();
        }
        let Some(lot) = lot else { continue };
        let ids = ids_by_lot.entry(lot as *const XmlPath).or_default();
        if path.name == "id"
            && path
                .parent
                .as_ref()
                .is_some_and(|parent| std::ptr::eq(parent.as_ref(), lot))
        {
            ids.push(text.clone());
        }
    }
    let mut seen = HashSet::new();
    ids_by_lot.values().all(|ids| {
        if ids.len() != 1 || ids[0].trim().is_empty() {
            return false;
        }
        seen.insert(ids[0].trim().to_owned())
    })
}

fn all_text(elements: &[(String, String)], names: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    for name in names {
        for (_, text) in elements
            .iter()
            .filter(|(element, text)| element == name && !text.is_empty())
        {
            if !values.contains(text) {
                values.push(text.clone());
            }
        }
    }
    values
}

fn parse_date(value: Option<&str>) -> Option<String> {
    let value = value?;
    if let Some(candidate) = value
        .split(|character: char| !character.is_ascii_digit() && character != '-')
        .find(|part| part.len() == 10 && part.as_bytes().get(4) == Some(&b'-'))
    {
        if NaiveDate::parse_from_str(candidate, "%Y-%m-%d").is_ok() {
            return Some(candidate.to_owned());
        }
    }
    let parts = value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.len() >= 3 {
        let day = parts[0].parse::<u32>().ok()?;
        let month = parts[1].parse::<u32>().ok()?;
        let year = parts[2].parse::<i32>().ok()?;
        return NaiveDate::from_ymd_opt(year, month, day).map(|date| date.to_string());
    }
    None
}

fn parse_value(value: Option<&str>) -> Option<Decimal> {
    parse_money(value?)
}

fn extract_amount_from_token_slice(tokens: &[&str]) -> Option<Decimal> {
    for (index, token) in tokens.iter().enumerate() {
        let cleaned_token = token.trim_end_matches(['.', ',', ':', ';', '!', '?']);
        let lowered = cleaned_token.to_lowercase();
        if lowered == "eur" || cleaned_token == "€" || lowered == "euro" || lowered == "euros" {
            // Prefix pattern: "EUR 125.000" or "€ 125.000"
            if let Some(next_token) = tokens.get(index + 1) {
                let cleaned = next_token.trim_end_matches(['.', ',', ':', ';', '!', '?']);
                if let Some(value) = parse_value(Some(cleaned)) {
                    return Some(value);
                }
            }
            // Postfix pattern: "125.000 EUR" or "125.000 €"
            if index > 0 {
                if let Some(prev_token) = tokens.get(index - 1) {
                    let cleaned = prev_token.trim_end_matches(['.', ',', ':', ';', '!', '?']);
                    if let Some(value) = parse_value(Some(cleaned)) {
                        return Some(value);
                    }
                }
            }
        }
        if lowered.starts_with("eur") || cleaned_token.starts_with('€') {
            let value = cleaned_token
                .trim_start_matches('€')
                .trim_start_matches(|c: char| c.eq_ignore_ascii_case(&'e'))
                .trim_start_matches(|c: char| c.eq_ignore_ascii_case(&'u'))
                .trim_start_matches(|c: char| c.eq_ignore_ascii_case(&'r'));
            if let Some(value) = parse_value(Some(value)) {
                return Some(value);
            }
        }
        if cleaned_token.ends_with('€') {
            let value = &cleaned_token[..cleaned_token.len() - '€'.len_utf8()];
            if let Some(value) = parse_value(Some(value)) {
                return Some(value);
            }
        }
        if lowered.ends_with("eur") {
            let value = &cleaned_token[..cleaned_token.len() - 3];
            if let Some(value) = parse_value(Some(value)) {
                return Some(value);
            }
        }
        if lowered.ends_with("euro") {
            let value = &cleaned_token[..cleaned_token.len() - 4];
            if let Some(value) = parse_value(Some(value)) {
                return Some(value);
            }
        }
    }
    None
}

fn pdf_value(text: &str) -> Option<Decimal> {
    let lines = text.lines().collect::<Vec<_>>();
    // Priority 1: Check lines explicitly mentioning procurement value or budget keywords
    const VALUE_KEYWORDS: &[&str] = &[
        "raming",
        "geraamd",
        "waarde",
        "montant",
        "estimé",
        "estimation",
        "valeur",
        "value",
        "budget",
        "total",
        "totaal",
        "prix",
    ];
    for line in &lines {
        let lowered = line.to_lowercase();
        if VALUE_KEYWORDS.iter().any(|kw| lowered.contains(kw)) {
            let tokens = line.split_whitespace().collect::<Vec<_>>();
            if let Some(val) = extract_amount_from_token_slice(&tokens) {
                return Some(val);
            }
        }
    }
    // Priority 2: Fallback across all tokens in document
    let all_tokens = text.split_whitespace().collect::<Vec<_>>();
    extract_amount_from_token_slice(&all_tokens)
}

fn fallback_id(source_name: &str) -> String {
    format!(
        "file:{}",
        url::form_urlencoded::byte_serialize(source_name.as_bytes()).collect::<String>()
    )
}

fn legacy_fallback_id(source_name: &str) -> String {
    Path::new(source_name)
        .with_extension("")
        .to_string_lossy()
        .replace(['/', '\\'], "::")
}

fn language_codes(elements: &[(String, String)]) -> Vec<String> {
    let mut codes = Vec::new();
    for (_, text) in elements
        .iter()
        .filter(|(element, text)| element == "noticelanguagecode" && !text.is_empty())
    {
        let mapped = match text.to_uppercase().as_str() {
            "ENG" => "en".to_owned(),
            "FRA" => "fr".to_owned(),
            "NLD" => "nl".to_owned(),
            "DEU" => "de".to_owned(),
            _ => text.to_lowercase(),
        };
        if !mapped.is_empty() && !codes.contains(&mapped) {
            codes.push(mapped);
        }
    }
    codes
}

type VersionKey = (bool, Vec<(u8, usize, String)>);

fn normalize_version_chunk(chunk: &str, digit_run: bool) -> (u8, usize, String) {
    if digit_run {
        let trimmed = chunk.trim_start_matches('0');
        (0, trimmed.len(), trimmed.to_owned())
    } else {
        (1, 0, chunk.to_owned())
    }
}

fn version_key(version: Option<&str>) -> VersionKey {
    let Some(text) = version.filter(|text| !text.is_empty()) else {
        return (false, Vec::new());
    };
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_is_digit = false;
    let mut started = false;
    for character in text.chars() {
        let is_digit = character.is_ascii_digit();
        if started && is_digit != current_is_digit {
            chunks.push(normalize_version_chunk(&current, current_is_digit));
            current.clear();
        }
        started = true;
        current_is_digit = is_digit;
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(normalize_version_chunk(&current, current_is_digit));
    }
    (true, chunks)
}

pub(crate) fn compare_notice_versions(
    left: Option<&str>,
    right: Option<&str>,
) -> std::cmp::Ordering {
    version_key(left).cmp(&version_key(right))
}

fn tender_folder_id(tender: &Tender) -> Option<&str> {
    tender
        .raw
        .get("contract_folder_id")
        .and_then(serde_json::Value::as_str)
}

fn tender_version(tender: &Tender) -> Option<&str> {
    tender
        .raw
        .get("notice_version")
        .and_then(serde_json::Value::as_str)
}

/// One dossier surfaces as one rich row. Versioned notices compete per
/// folder (highest VersionID wins, ties keep input order — the Python
/// reference behavior); then unversioned folder members (PDF renderings,
/// besteks) FUSE into the winner — their text, requirements and CPVs join
/// it — instead of surfacing as separate ladder slots with no buyer. Fused
/// members are recorded in `raw.dossier_members` for audit. Superseded
/// versions are still dropped silently: a correction replaces its notice.
fn prefer_latest_versions(tenders: Vec<Tender>) -> Vec<Tender> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Tender>> = HashMap::new();
    for tender in tenders {
        let key = crate::dossier_group_key(&tender);
        groups.entry(key.clone()).or_default().push(tender);
        if !order.contains(&key) {
            order.push(key);
        }
    }
    let mut fused = Vec::new();
    for key in order {
        let members = groups.remove(&key).expect("grouped key should exist");
        fused.push(fuse_dossier_group(members));
    }
    fused
}

fn fuse_dossier_group(members: Vec<Tender>) -> Tender {
    // Winner by index: same-folder XML rows share one external_id, so
    // identity comparison cannot tell members apart. Versioned groups keep
    // the highest version (strictly greater wins, so ties keep input
    // order); unversioned groups keep the structured/longest row. Either
    // way only unversioned rows fuse — a correction replaces its notice,
    // it does not extend it, so superseded versions stay dropped.
    let (mut versioned, mut unversioned): (Vec<Tender>, Vec<Tender>) = members
        .into_iter()
        .partition(|tender| tender_version(tender).is_some());
    let mut winner;
    if !versioned.is_empty() {
        let mut best = 0;
        let mut best_key = version_key(None);
        for (index, tender) in versioned.iter().enumerate() {
            let key = version_key(tender_version(tender));
            if index == 0 || key > best_key {
                best_key = key;
                best = index;
            }
        }
        winner = versioned.remove(best);
    } else {
        let survivor = crate::dossier_survivor_index(&unversioned);
        winner = unversioned.remove(survivor);
    }
    let mut fused_ids = Vec::new();
    for member in unversioned {
        // Dossier-level facts the winner lacks: a rendering never carries
        // the buyer, so the first member that states one lends it. Deadline
        // and value stay the winner's — those are version-specific.
        if winner.buyer.as_ref().is_none_or(|buyer| buyer.is_empty()) {
            winner.buyer = member.buyer.clone();
        }
        fused_ids.push(member.external_id.clone());
        if !member.description.is_empty()
            && !winner.description.contains(member.description.as_str())
        {
            winner.description.push_str("\n\n");
            winner.description.push_str(member.description.as_str());
            if winner.description.len() > MAX_PDF_TEXT_CHARS {
                let mut end = MAX_PDF_TEXT_CHARS;
                while !winner.description.is_char_boundary(end) {
                    end -= 1;
                }
                winner.description.truncate(end);
                break;
            }
        }
        for requirement in member.requirements {
            if !winner.requirements.iter().any(|existing| {
                existing.kind == requirement.kind && existing.text == requirement.text
            }) {
                winner.requirements.push(requirement);
            }
        }
        for cpv in member.cpv_codes {
            if !winner.cpv_codes.contains(&cpv) {
                winner.cpv_codes.push(cpv);
            }
        }
    }
    if !fused_ids.is_empty() {
        winner.raw["dossier_members"] = serde_json::Value::Array(
            fused_ids
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
    }
    winner
}

/// Maximum captured award-criteria section: the substance (criteria plus
/// weighting), not the whole bestek, which already lives in `description`.
const MAX_CRITERIA_CHARS: usize = 2_000;

/// Award-criteria section of a bestek or cahier des charges (NL
/// `Gunningscriteria`, FR `Critères d'attribution`), from its header line to
/// the next roman-numbered section header (`I.11`, `II.9`, …). The caller
/// stores it as a `requirement` of kind `award_criterion` with
/// `mandatory = false`: criteria judge offers, they are not bidder
/// qualifications.
fn extract_award_criteria(text: &str) -> Option<String> {
    let start_pattern =
        regex::Regex::new(r"(?i)gunningscriteri|crit[èe]res?\s+d['’]attribution|award\s+criteri")
            .expect("valid criteria pattern");
    let next_section =
        regex::Regex::new(r"^\s*[IVXLCDM]+\.\d+[\s.]").expect("valid section pattern");
    // The table of contents names the section before the real one begins,
    // and body sentences can mention the criteria in passing (measured
    // 2026-10-23: "...opnemen in de gunningscriteria..." followed by a long
    // numbered subsection outscored the real two-sentence section). So
    // section-shaped headers (`I.10 Gunningscriteria`) are tried first and
    // the longest of those wins; only if none has a body do bare mentions
    // fall back to the same longest-wins rule.
    let header_pattern = regex::Regex::new(
        r"(?i)^\s*[IVXLCDM]+\.\d+\s+.*(gunningscriteri|crit[èe]res?\s+d['’]attribution|award\s+criteri)",
    )
    .expect("valid header pattern");
    let lines: Vec<&str> = text.lines().collect();
    let capture = |header: &regex::Regex| -> Option<String> {
        let mut best: Option<String> = None;
        for (index, line) in lines.iter().enumerate() {
            if !header.is_match(line) {
                continue;
            }
            let mut body: Vec<&str> = Vec::new();
            for following in lines.iter().skip(index + 1) {
                if next_section.is_match(following) {
                    break;
                }
                body.push(following);
            }
            let section = body
                .join("\n")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if section.len() > best.as_deref().unwrap_or("").len() {
                best = Some(section);
            }
        }
        best.filter(|section| !section.is_empty())
    };
    let section = capture(&header_pattern).or_else(|| capture(&start_pattern))?;
    Some(section.chars().take(MAX_CRITERIA_CHARS).collect())
}

/// Explicit lot declaration (`Aantal percelen: N`, `nombre de lots: N`) for
/// a `requirement` of kind `lot`. Only explicit counts; fuzzy lot mentions
/// stay out rather than guess.
fn extract_lot_count(text: &str) -> Option<String> {
    let pattern =
        regex::Regex::new(r"(?i)(?:aantal\s+percelen?|nombre\s+de\s+lots?)\s*[:\-]?\s*\d+")
            .expect("valid lot pattern");
    pattern
        .find(text)
        .map(|declaration| declaration.as_str().trim().to_owned())
}

fn parse_pdf_text(text: &str, source_name: &str) -> Tender {
    let lines = text
        .lines()
        .map(normalized_text)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let title = lines
        .first()
        .cloned()
        .unwrap_or_else(|| source_name.to_owned());
    let cpv_pattern = regex::Regex::new(r"\b\d{8}\b").expect("valid CPV pattern");
    let mut dropped_cpv_candidates: Vec<String> = Vec::new();
    let cpv_codes = cpv_pattern
        .find_iter(text)
        .map(|value| value.as_str().to_owned())
        .fold(Vec::new(), |mut values, value| {
            if values.contains(&value) || dropped_cpv_candidates.contains(&value) {
                return values;
            }
            // Any 8-digit run matches the pattern: amounts, phone fragments,
            // reference numbers. Only real CPV 2008 codes are kept (measured
            // 2026-10-23: an Etterbeek notice carried 26272702/26272329/
            // 22349611 next to the real 72415000, and the phantom 22-prefix
            // code matched retail NACE 47120 as an IT supplier). Rejects stay
            // in the raw payload so a newer-than-SDK code can be audited
            // instead of vanishing.
            if crate::cpv::cpv_is_known(&value) {
                values.push(value);
            } else {
                dropped_cpv_candidates.push(value);
            }
            values
        });
    let deadline = lines
        .iter()
        .find(|line| {
            let lowered = line.to_lowercase();
            ["deadline", "date limite", "indiening", "offre"]
                .iter()
                .any(|term| lowered.contains(term))
        })
        .and_then(|line| parse_date(Some(line)));
    let estimated_value = pdf_value(text);
    let mut requirements = lines
        .iter()
        .filter(|line| {
            let lowered = line.to_lowercase();
            [
                "certificat",
                "certification",
                "attestation",
                "chiffre d'affaires",
                "omzet",
                "turnover",
                "reference",
                "référence",
                "erkenning",
            ]
            .iter()
            .any(|term| lowered.contains(term))
        })
        .map(|text| TenderRequirement {
            text: text.clone(),
            ..TenderRequirement::default()
        })
        .collect::<Vec<_>>();
    // Structured bestek substance on top of the keyword lines: the award
    // criteria section and an explicit lot count, if the document states
    // them. Both are informational (mandatory = false); criteria judge
    // offers and lots describe structure, neither qualifies the bidder.
    if let Some(criteria) = extract_award_criteria(text) {
        requirements.push(TenderRequirement {
            text: criteria,
            kind: "award_criterion".to_owned(),
            mandatory: false,
            source_page: None,
        });
    }
    if let Some(lots) = extract_lot_count(text) {
        requirements.push(TenderRequirement {
            text: lots,
            kind: "lot".to_owned(),
            mandatory: false,
            source_page: None,
        });
    }
    Tender {
        source: "bda".to_owned(),
        external_id: fallback_id(source_name),
        title,
        description: text.chars().take(MAX_PDF_TEXT_CHARS).collect(),
        cpv_codes,
        estimated_value,
        deadline,
        requirements,
        raw: json!({
            "source_name": source_name,
            "legacy_external_id": legacy_fallback_id(source_name),
            "pdf_text": text.chars().take(MAX_PDF_TEXT_CHARS).collect::<String>(),
            "dropped_cpv_candidates": dropped_cpv_candidates,
        }),
        ..Tender::default()
    }
}

/// The extractor writes through this sink, so the limit applies during
/// extraction, not after an eager Vec<String> has already been allocated.
#[derive(Default)]
struct PdfTextSink {
    text: String,
    chars: usize,
    exceeded: bool,
}

impl std::fmt::Write for PdfTextSink {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let chars = value.chars().count();
        if chars > MAX_PDF_EXTRACT_CHARS.saturating_sub(self.chars) {
            self.exceeded = true;
            return Err(std::fmt::Error);
        }
        self.text.push_str(value);
        self.chars += chars;
        Ok(())
    }
}

impl<'a> pdf_extract::ConvertToFmt for &'a mut PdfTextSink {
    type Writer = &'a mut PdfTextSink;

    fn convert(self) -> Self::Writer {
        self
    }
}

fn parse_pdf(data: &[u8], source_name: &str) -> Result<Tender, Box<dyn Error>> {
    let mut document = pdf_extract::Document::load_mem(data)?;
    if document.is_encrypted() {
        document.decrypt("")?;
    }
    let mut sink = PdfTextSink::default();
    let result = {
        let mut output = pdf_extract::PlainTextOutput::new(&mut sink);
        pdf_extract::output_doc(&document, &mut output)
    };
    if sink.exceeded {
        return Err(format!(
            "PDF text exceeded the {MAX_PDF_EXTRACT_CHARS} character extraction limit"
        )
        .into());
    }
    result?;
    Ok(parse_pdf_text(&sink.text, source_name))
}

pub fn parse_xml(data: &[u8], source_name: &str) -> Result<Tender, Box<dyn Error>> {
    let paths = element_texts_with_paths(data)?;
    let elements = paths
        .iter()
        .map(|(path, text)| (path.name.clone(), text.clone()))
        .collect::<Vec<_>>();
    let title = scoped_text(&paths, "name", "procurementproject")
        .or_else(|| scoped_text(&paths, "title", "procurementproject"))
        .or_else(|| first_text(&elements, &["name", "title"]))
        .unwrap_or_else(|| source_name.to_owned());
    let buyer = scoped_text(&paths, "partyname", "contractingparty")
        .or_else(|| scoped_text(&paths, "officialname", "address_contracting_body"))
        .or_else(|| first_text(&elements, &["partyname"]));
    let description = scoped_text(&paths, "description", "procurementproject")
        .or_else(|| first_text(&elements, &["description"]))
        .unwrap_or_default();
    let mut cpv_codes = all_text(&elements, &["itemclassificationcode", "cpv"]);
    // Migrated eSenders XML carries the code as an attribute
    // (`<CPV_CODE CODE="90500000"/>`) with no element text, which the
    // path model never sees — scan the raw document as a fallback.
    if cpv_codes.is_empty() {
        cpv_codes = cpv_code_attributes(data);
    }
    let regions = all_text(
        &elements,
        &["countrysubentitycode", "countrycode", "cityname"],
    );
    let deadline = parse_date(
        first_text(&elements, &["enddate", "deadline", "submissiondeadline"]).as_deref(),
    );
    let languages = language_codes(&elements);
    // ContractFolderID is the mandatory UBL dossier identifier (stable across
    // corrected notices, unlike the per-document ID); TotalAmount covers both
    // RequestedTenderTotal and ProcurementProject/BudgetAmount totals.
    let folder_id = first_text(&elements, &["contractfolderid"]);
    let notice_identifier = first_text(&elements, &["publicationnumber", "noticeid"])
        .or_else(|| direct_child_text(&paths, "id"));
    let embedded_id = folder_id.clone().or_else(|| notice_identifier.clone());
    let legacy_external_id = embedded_id
        .is_none()
        .then(|| legacy_fallback_id(source_name));
    let external_id = embedded_id.unwrap_or_else(|| fallback_id(source_name));
    // Belgian e-Notification states when the notice was published as IssueDate.
    // Without it a BDA-only store can never report coverage or a `fresh` verdict:
    // every row would be an "unknown date" row. Measured 2026-10-19, the fixture
    // store reported 1 unknown row out of 1.
    let publication_date = first_text(&elements, &["issuedate"])
        .and_then(|value| value.get(..10).map(ToOwned::to_owned))
        .filter(|value| chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok());
    let estimated_value = parse_value(
        first_text(
            &elements,
            &[
                "estimatedoverallcontractamount",
                "totalamount",
                "estimatedvalue",
                "contractvalue",
            ],
        )
        .as_deref(),
    );
    let (framework_maximum_evidence, framework_maximum_evidence_truncated) =
        framework_maximum_evidence(&paths);
    let (notice_lot_place_evidence, notice_lot_place_evidence_truncated) =
        lot_place_evidence(&paths);
    let (notice_lot_award_criteria_evidence, notice_lot_award_criteria_evidence_truncated) =
        lot_award_criteria_evidence(&paths);
    let (notice_lot_selection_evidence, notice_lot_selection_evidence_truncated) =
        lot_selection_evidence(&paths);
    let (notice_lot_contract_execution_evidence, notice_lot_contract_execution_evidence_truncated) =
        lot_contract_execution_evidence(&paths);
    let (notice_lot_subsequent_process_evidence, notice_lot_subsequent_process_evidence_truncated) =
        lot_subsequent_process_evidence(&paths);
    let (
        notice_lot_guarantee_curricula_evidence,
        notice_lot_guarantee_curricula_evidence_truncated,
    ) = lot_guarantee_curricula_evidence(&paths);
    // AwardingCriteria/LowTendersDescription are notice-level TenderingTerms
    // text. Per-criterion Description elements stay out of requirements and are
    // retained only in the separate, lot-scoped evidence above.
    let requirements = all_text(
        &elements,
        &[
            "minimumqualificationrequirement",
            "selectioncriterion",
            "requirement",
            "awardingcriteria",
            "lowtendersdescription",
        ],
    )
    .into_iter()
    .map(|text| TenderRequirement {
        text,
        ..TenderRequirement::default()
    })
    .collect();
    Ok(Tender {
        source: "bda".to_owned(),
        external_id,
        title,
        buyer,
        description,
        cpv_codes,
        regions,
        estimated_value,
        deadline,
        publication_date,
        languages,
        requirements,
        raw: json!({
            // The full source XML is intentionally not retained: documents may
            // be up to 8 MiB each, and persisting them per tender inflates the
            // database and the tender-detail response. Folder/version metadata
            // preserves correction tracking without the payload.
            "source_name": source_name,
            "source_bytes": data.len(),
            "legacy_external_id": legacy_external_id,
            "notice_identifier": notice_identifier,
            "contract_folder_id": folder_id,
            "notice_version": first_text(&elements, &["versionid"]),
            // Framework ceilings are not estimated spend. Preserve them separately
            // rather than silently treating a maximum as an estimated value.
            "framework_maximum_amounts": all_text(&elements, &["frameworkmaximumamount"]),
            "framework_maximum_evidence": framework_maximum_evidence,
            "framework_maximum_evidence_truncated": framework_maximum_evidence_truncated,
            "notice_lot_ids_unique": notice_lot_ids_are_unique(&paths),
            "notice_lot_place_evidence": notice_lot_place_evidence,
            "notice_lot_place_evidence_truncated": notice_lot_place_evidence_truncated,
            "notice_lot_award_criteria_evidence": notice_lot_award_criteria_evidence,
            "notice_lot_award_criteria_evidence_truncated": notice_lot_award_criteria_evidence_truncated,
            "notice_lot_selection_evidence": notice_lot_selection_evidence,
            "notice_lot_selection_evidence_truncated": notice_lot_selection_evidence_truncated,
            "notice_lot_contract_execution_evidence": notice_lot_contract_execution_evidence,
            "notice_lot_contract_execution_evidence_truncated": notice_lot_contract_execution_evidence_truncated,
            "notice_lot_subsequent_process_evidence": notice_lot_subsequent_process_evidence,
            "notice_lot_subsequent_process_evidence_truncated": notice_lot_subsequent_process_evidence_truncated,
            "notice_lot_guarantee_curricula_evidence": notice_lot_guarantee_curricula_evidence,
            "notice_lot_guarantee_curricula_evidence_truncated": notice_lot_guarantee_curricula_evidence_truncated,
            "notice_lot_cpv_evidence": lot_cpv_evidence(&paths)
                .into_iter()
                .map(|(lot_id, cpv_codes)| json!({"lot_id": lot_id, "cpv_codes": cpv_codes}))
                .collect::<Vec<_>>(),
        }),
        ..Tender::default()
    })
}

fn xml_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
}

fn supported_file(path: &Path) -> bool {
    xml_file(path)
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

fn read_bounded<R: Read>(reader: R, name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut data = Vec::new();
    reader
        .take((MAX_BDA_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut data)?;
    if data.len() > MAX_BDA_DOCUMENT_BYTES {
        return Err(format!(
            "BDA document {name} exceeded the {MAX_BDA_DOCUMENT_BYTES} byte limit"
        )
        .into());
    }
    Ok(data)
}

fn add_total_bytes(total: &mut usize, size: usize, _name: &str) -> Result<(), Box<dyn Error>> {
    *total = total
        .checked_add(size)
        .ok_or("BDA decompressed byte count overflow")?;
    if *total > MAX_BDA_TOTAL_BYTES {
        return Err(format!(
            "BDA input exceeded the {MAX_BDA_TOTAL_BYTES} decompressed byte limit"
        )
        .into());
    }
    Ok(())
}

fn collect_directory(
    path: &Path,
    root: &Path,
    result: &mut Vec<Document>,
    skipped: &mut Vec<(String, String)>,
    total_bytes: &mut usize,
) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_directory(&child, root, result, skipped, total_bytes)?;
        } else if file_type.is_symlink() && supported_file(&child) {
            return Err(format!("BDA directory contains a symlink: {}", child.display()).into());
        } else if file_type.is_file() && supported_file(&child) {
            if result.len() >= MAX_BDA_DOCUMENTS {
                return Err(
                    format!("BDA input exceeded the {MAX_BDA_DOCUMENTS} document limit").into(),
                );
            }
            let name = child
                .strip_prefix(root)
                .unwrap_or(&child)
                .to_string_lossy()
                .replace('\\', "/");
            // One oversized file must not sink the dossier: stat first and
            // skip by name instead of aborting the collection.
            let too_big = fs::metadata(&child)
                .map(|metadata| metadata.len() > MAX_BDA_DOCUMENT_BYTES as u64)
                .unwrap_or(false);
            if too_big {
                skipped.push((
                    name,
                    format!("BDA document exceeded the {MAX_BDA_DOCUMENT_BYTES} byte limit"),
                ));
                continue;
            }
            let data = read_bounded(fs::File::open(&child)?, &name)?;
            add_total_bytes(total_bytes, data.len(), &name)?;
            result.push((name.clone(), data));
        }
    }
    Ok(())
}

/// Collected input plus per-file collection skips (oversized files skip by
/// name; aggregate and archive-metadata failures still abort the run).
type CollectedDocuments = (Vec<Document>, Vec<(String, String)>);

fn documents(path: &Path) -> Result<CollectedDocuments, Box<dyn Error>> {
    if path.is_dir() {
        let mut result = Vec::new();
        let mut skipped = Vec::new();
        let mut total_bytes = 0;
        collect_directory(path, path, &mut result, &mut skipped, &mut total_bytes)?;
        result.sort_by(|left, right| left.0.cmp(&right.0));
        return Ok((result, skipped));
    }
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        let file = fs::File::open(path)?;
        let metadata_phase = Arc::new(AtomicBool::new(true));
        let reader = BoundedZipReader::new(
            file,
            Arc::clone(&metadata_phase),
            MAX_BDA_ZIP_METADATA_BYTES,
        );
        let archive = ZipArchive::new(reader);
        metadata_phase.store(false, Ordering::Relaxed);
        let mut archive = archive.map_err(|error| {
            format!(
                "{error} (platform dossier zips with large PDFs can trip the \
                 archive scan; extract the zip and report the directory instead)"
            )
        })?;
        let mut result = Vec::new();
        let mut skipped = Vec::new();
        let mut total_bytes = 0;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let entry_name = entry.name()?;
            if entry.is_dir() || !supported_file(Path::new(entry_name.as_ref())) {
                continue;
            }
            if result.len() >= MAX_BDA_DOCUMENTS {
                return Err(
                    format!("BDA input exceeded the {MAX_BDA_DOCUMENTS} document limit").into(),
                );
            }
            let name = entry_name.replace('\\', "/");
            // The central directory carries each entry's size, so oversized
            // files skip without ever being decompressed.
            if entry.size() > MAX_BDA_DOCUMENT_BYTES as u64 {
                skipped.push((
                    name,
                    format!("BDA document exceeded the {MAX_BDA_DOCUMENT_BYTES} byte limit"),
                ));
                continue;
            }
            let data = read_bounded(&mut entry, &name)?;
            add_total_bytes(&mut total_bytes, data.len(), &name)?;
            result.push((name, data));
        }
        result.sort_by(|left, right| left.0.cmp(&right.0));
        return Ok((result, skipped));
    }
    if supported_file(path) {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_owned();
        let data = read_bounded(fs::File::open(path)?, &name)?;
        let mut total_bytes = 0;
        add_total_bytes(&mut total_bytes, data.len(), &name)?;
        return Ok((vec![(name, data)], Vec::new()));
    }
    Err(format!(
        "Expected an XML file, ZIP, or directory: {}",
        path.display()
    )
    .into())
}

/// Parse a BDA export, skipping malformed documents instead of failing.
///
/// Bulk downloads can contain one broken file among thousands; like the TED
/// adapter, the importer records the failure per document and continues.
/// Oversized single documents join the skip list too (the aggregate and
/// archive-metadata bounds still abort the run: those signal a runaway
/// input, not one big file). Platform dossier zips whose central-directory
/// scan trips the metadata bound should be extracted first and reported as
/// a directory, which lists entries without that scan.
/// File-level problems (oversized input, symlinks, archive limits) still
/// abort the run.
pub fn load_bda_documents(path: impl AsRef<Path>) -> Result<ParsedBdaImport, Box<dyn Error>> {
    let mut tenders = Vec::new();
    let mut awards = Vec::new();
    let (documents, mut skipped) = documents(path.as_ref())?;
    // Dossier container: archive stem for zips, walked root name for
    // directories. Every file row carries it so the ladder can collapse
    // one dossier's files to one candidate (XML rows additionally carry
    // contract_folder_id, which wins at grouping time).
    let container_base = path
        .as_ref()
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (name, data) in &documents {
        let parsed = if xml_file(Path::new(name)) {
            parse_xml(data, name)
        } else {
            parse_pdf(data, name)
        };
        match parsed {
            Ok(mut tender) => {
                // Container plus the file's parent inside the import: one
                // download may bundle several dossiers in subfolders, and
                // those must stay split.
                if !container_base.is_empty() {
                    let parent = Path::new(name)
                        .parent()
                        .map(|parent| parent.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let container = if parent.is_empty() {
                        container_base.clone()
                    } else {
                        format!("{container_base}/{parent}")
                    };
                    tender.raw["dossier_container"] = serde_json::Value::String(container);
                }
                // Same-file context the award XML does not repeat: first
                // CPV and buyer travel with each extracted award.
                for mut award in parse_bda_awards(data, name) {
                    award.cpv = tender.cpv_codes.first().cloned();
                    award.buyer = tender.buyer.clone();
                    awards.push(award);
                }
                tenders.push(tender);
            }
            Err(error) => skipped.push((name.clone(), error.to_string())),
        }
    }
    // File renderings join their notice's folder: within one import, a
    // container holding exactly one ContractFolderID lends it to the rows
    // that parsed without one (PDFs carry no folder element). Containers
    // spanning several folders stay unstamped — those dossiers stay split.
    let mut folders_by_container: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for tender in &tenders {
        if let (Some(folder), Some(container)) = (
            tender_folder_id(tender),
            tender
                .raw
                .get("dossier_container")
                .and_then(|value| value.as_str()),
        ) {
            let folders = folders_by_container
                .entry(container.to_owned())
                .or_default();
            if !folders.contains(&folder.to_owned()) {
                folders.push(folder.to_owned());
            }
        }
    }
    for tender in &mut tenders {
        if !matches!(tender_folder_id(tender), None | Some("")) {
            continue;
        }
        let container = tender
            .raw
            .get("dossier_container")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned();
        if let Some(folders) = folders_by_container.get(&container) {
            if let [folder] = folders.as_slice() {
                tender.raw["contract_folder_id"] = serde_json::Value::String(folder.clone());
            }
        }
    }
    // FR/NL doubles describe the same award: one row per notice, winner
    // and value, keeping the first file's tender context.
    let mut seen = std::collections::HashSet::new();
    awards.retain(|award| {
        seen.insert((
            award.notice_ref.clone(),
            award.winner_name.clone(),
            award.value,
        ))
    });
    Ok((prefer_latest_versions(tenders), skipped, awards))
}

pub fn load_bda(path: impl AsRef<Path>) -> Result<Vec<Tender>, Box<dyn Error>> {
    Ok(load_bda_documents(path)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::{write::SimpleFileOptions, ZipWriter};

    #[test]
    fn rejects_zip_metadata_before_archive_construction_completes() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("notice.xml", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"<notice/>").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let metadata_phase = Arc::new(AtomicBool::new(true));
        let reader = BoundedZipReader::new(Cursor::new(bytes), metadata_phase, 1);

        assert!(ZipArchive::new(reader).is_err());
    }

    #[test]
    fn rejects_aggregate_size_limit() {
        let mut total = MAX_BDA_TOTAL_BYTES;
        let error = add_total_bytes(&mut total, 1, "second.xml")
            .expect_err("aggregate BDA size should be bounded");
        assert!(error.to_string().contains("decompressed byte limit"));
    }

    #[test]
    fn rejects_oversized_documents() {
        let error = read_bounded(
            Cursor::new(vec![0_u8; MAX_BDA_DOCUMENT_BYTES + 1]),
            "large.xml",
        )
        .expect_err("oversized BDA documents should be rejected");
        assert!(error.to_string().contains("exceeded"));
    }

    /// One huge brand-guidelines PDF must not sink its dossier (measured
    /// 2026-10-23: a 16 MB PDF aborted a 13-file platform export). The
    /// oversized file joins the skip list by name; the rest still loads.
    #[test]
    fn oversized_file_skips_by_name_in_directory() {
        use std::fs;
        let root = std::env::temp_dir().join(format!(
            "aanbestedingen-bda-oversize-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("directory should create");
        fs::write(root.join("ok.xml"), b"<Notice><ID>OK</ID></Notice>")
            .expect("small notice should write");
        fs::write(
            root.join("huge.pdf"),
            vec![0_u8; MAX_BDA_DOCUMENT_BYTES + 1],
        )
        .expect("oversized file should write");
        let (documents, skipped) = documents(&root).expect("directory should load");
        assert_eq!(documents.len(), 1);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, "huge.pdf");
        assert!(skipped[0].1.contains("exceeded"));
        fs::remove_dir_all(&root).expect("directory should remove");
    }

    /// Migrated award XML in TED eSenders shape (measured 2026-10-23 on BDA
    /// 2023-509623: winner RENEWI BELGIUM, €171,459, CPV 90500000).
    const F03_AWARD: &str = r#"<?xml version="1.0" encoding="UTF-8"?><TED_ESENDERS><SENDER><IDENTIFICATION><ESENDER_LOGIN>BE001</ESENDER_LOGIN><NO_DOC_EXT>2023-509623</NO_DOC_EXT></IDENTIFICATION></SENDER><FORM_SECTION><F03_2014 CATEGORY="ORIGINAL" FORM="F03" LG="FR"><CONTRACTING_BODY><ADDRESS_CONTRACTING_BODY><OFFICIALNAME>Agence Fédérale pour la Sécurité de la Chaine Alimentaire</OFFICIALNAME></ADDRESS_CONTRACTING_BODY></CONTRACTING_BODY><OBJECT_CONTRACT><TITLE><P>Enlèvement déchets</P></TITLE><CPV_MAIN><CPV_CODE CODE="90500000"/></CPV_MAIN></OBJECT_CONTRACT><AWARD_CONTRACT ITEM="1"><AWARDED_CONTRACT><DATE_CONCLUSION_CONTRACT>2023-02-17</DATE_CONCLUSION_CONTRACT><TENDERS><NB_TENDERS_RECEIVED>2</NB_TENDERS_RECEIVED></TENDERS><CONTRACTORS><CONTRACTOR><ADDRESS_CONTRACTOR><OFFICIALNAME>RENEWI BELGIUM</OFFICIALNAME><TOWN>LOMMEL</TOWN><POSTAL_CODE>3920</POSTAL_CODE><COUNTRY VALUE="BE"/></ADDRESS_CONTRACTOR></CONTRACTOR></CONTRACTORS><VALUES><VAL_TOTAL CURRENCY="EUR">171459.00</VAL_TOTAL></VALUES></AWARDED_CONTRACT></AWARD_CONTRACT></F03_2014></FORM_SECTION></TED_ESENDERS>"#;

    #[test]
    fn award_xml_yields_winner_value_and_notice_ref() {
        let awards = parse_bda_awards(F03_AWARD.as_bytes(), "475712_0_0_fr-FR.xml");
        assert_eq!(awards.len(), 1);
        let award = &awards[0];
        assert_eq!(award.winner_name, "RENEWI BELGIUM");
        assert_eq!(award.winner_town.as_deref(), Some("LOMMEL"));
        assert_eq!(award.winner_postcode.as_deref(), Some("3920"));
        assert_eq!(award.winner_country.as_deref(), Some("BE"));
        assert_eq!(
            award.value.map(|value| value.to_string()).as_deref(),
            Some("171459.00")
        );
        assert_eq!(award.currency, "EUR");
        assert_eq!(award.date_conclusion.as_deref(), Some("2023-02-17"));
        assert_eq!(award.tenders_received, Some(2));
        assert_eq!(award.notice_ref.as_deref(), Some("2023-509623"));
    }

    #[test]
    fn esenders_attributes_yield_buyer_and_cpv() {
        let tender =
            parse_xml(F03_AWARD.as_bytes(), "award.xml").expect("award notice should parse");
        assert!(tender
            .buyer
            .as_deref()
            .unwrap_or_default()
            .contains("Sécurité de la Chaine"));
        assert_eq!(tender.cpv_codes, vec!["90500000".to_owned()]);
    }

    #[test]
    fn tender_files_carry_no_awards() {
        let awards = parse_bda_awards(
            br"<ContractNotice><ContractFolderID>F-1</ContractFolderID><ProcurementProject><Name>Open</Name></ProcurementProject></ContractNotice>",
            "open.xml",
        );
        assert!(awards.is_empty());
    }

    #[test]
    fn fr_nl_doubles_deduplicate_to_one_award() {
        use std::fs;
        let root = std::env::temp_dir().join(format!(
            "aanbestedingen-bda-awardlang-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("directory should create");
        fs::write(root.join("475712_0_0_fr-FR.xml"), F03_AWARD)
            .expect("french notice should write");
        fs::write(
            root.join("475712_0_0_nl-NL.xml"),
            F03_AWARD.replace("Enlèvement déchets", "Ophaling afval"),
        )
        .expect("dutch notice should write");
        let (_, _, awards) = load_bda_documents(&root).expect("dossier should load");
        assert_eq!(awards.len(), 1, "FR/NL doubles are one award");
        assert_eq!(awards[0].winner_name, "RENEWI BELGIUM");
        fs::remove_dir_all(&root).expect("directory should remove");
    }

    #[test]
    fn oversized_entry_skips_without_decompressing() {
        use std::fs;
        let path = std::env::temp_dir().join(format!(
            "aanbestedingen-bda-bigzip-{}.zip",
            std::process::id()
        ));
        let file = fs::File::create(&path).expect("zip should create");
        let mut archive = ZipWriter::new(file);
        archive
            .start_file("ok.xml", SimpleFileOptions::default())
            .expect("entry should start");
        archive
            .write_all(b"<Notice><ID>OK</ID></Notice>")
            .expect("entry should write");
        // Zeros compress to nearly nothing on disk, but the central
        // directory still reports the full uncompressed size — which is
        // what the skip decision reads, so nothing is decompressed.
        archive
            .start_file("huge.pdf", SimpleFileOptions::default())
            .expect("entry should start");
        archive
            .write_all(&vec![0_u8; MAX_BDA_DOCUMENT_BYTES + 1])
            .expect("entry should write");
        archive.finish().expect("zip should finish");
        let (tenders, skipped, _) = load_bda_documents(&path).expect("dossier should load");
        assert_eq!(tenders.len(), 1);
        assert!(skipped.iter().any(|(name, _)| name == "huge.pdf"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_excessive_xml_depth() {
        let xml = "<n>".repeat(MAX_BDA_XML_DEPTH + 1);
        let error = element_texts_with_paths(xml.as_bytes())
            .expect_err("excessively deep XML should be rejected");
        assert!(error.to_string().contains("nesting-depth limit"));
    }

    #[test]
    fn rejects_excessive_aggregated_xml_text() {
        let mut xml = "<n>".repeat(40);
        xml.push_str(&"x".repeat(1024 * 1024));
        xml.push_str(&"</n>".repeat(40));
        let error = element_texts_with_paths(xml.as_bytes())
            .expect_err("repeated ancestor text aggregation should be bounded");
        assert!(error.to_string().contains("aggregated text limit"));
    }

    fn versioned_xml(folder: &str, version: Option<&str>, title: &str) -> Vec<u8> {
        let version_tag = version
            .map(|version| format!("<VersionID>{version}</VersionID>"))
            .unwrap_or_default();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <ContractNotice xmlns="urn:test">
              <ID>DOC-{title}</ID>
              <ContractFolderID>{folder}</ContractFolderID>
              {version_tag}
              <ProcurementProject><Name>{title}</Name></ProcurementProject>
            </ContractNotice>"#
        )
        .into_bytes()
    }

    /// Folder-less rows in a single-folder import join that folder, so file
    /// renderings collapse with their notice at ranking time. A second
    /// folder in the same import keeps its rows split.
    #[test]
    fn folder_less_rows_join_the_imports_single_folder() {
        let bare = br#"<?xml version="1.0" encoding="UTF-8"?>
            <ContractNotice xmlns="urn:test">
              <ID>DOC-bare</ID>
              <ProcurementProject><Name>Bare rendering</Name></ProcurementProject>
            </ContractNotice>"#
            .to_vec();
        let root = versioned_directory(&[
            ("notice.xml", versioned_xml("FOLDER-1", Some("01"), "Works")),
            ("rendering.xml", bare),
        ]);
        let (tenders, _, _) = load_bda_documents(&root).expect("directory should load");
        fs::remove_dir_all(&root).ok();
        assert_eq!(tenders.len(), 1, "one dossier, one row");
        let winner = &tenders[0];
        assert_eq!(winner.external_id, "FOLDER-1", "the notice wins");
        let container = root
            .file_stem()
            .expect("temp dir should have a name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            winner.raw["dossier_container"].as_str(),
            Some(container.as_str())
        );
        assert_eq!(
            winner.raw["dossier_members"],
            serde_json::json!(["DOC-bare"]),
            "fused members stay auditable"
        );
    }

    fn versioned_directory(files: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "aanbestedingen-bda-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should advance")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("directory should create");
        for (name, data) in files {
            fs::write(root.join(name), data).expect("file should write");
        }
        root
    }

    #[test]
    fn keeps_highest_version_per_dossier() {
        let root = versioned_directory(&[
            ("v01.xml", versioned_xml("FOLDER-1", Some("01"), "Works v1")),
            ("v02.xml", versioned_xml("FOLDER-1", Some("02"), "Works v2")),
            ("other.xml", versioned_xml("FOLDER-2", Some("01"), "Other")),
        ]);
        let (tenders, skipped, _) = load_bda_documents(&root).expect("directory should load");
        fs::remove_dir_all(&root).ok();
        assert!(skipped.is_empty());
        // Winners keep first-seen file order (other.xml sorts first).
        let titles: Vec<(&str, &str)> = tenders
            .iter()
            .map(|tender| (tender.title.as_str(), tender.external_id.as_str()))
            .collect();
        assert_eq!(titles, [("Other", "FOLDER-2"), ("Works v2", "FOLDER-1")]);
        assert_eq!(
            tenders[1]
                .raw
                .get("notice_version")
                .and_then(|value| value.as_str()),
            Some("02")
        );
    }

    #[test]
    fn version_tie_and_missing_versions_pass_through() {
        let root = versioned_directory(&[
            ("a.xml", versioned_xml("FOLDER-1", Some("01"), "First")),
            ("b.xml", versioned_xml("FOLDER-1", Some("01"), "Second")),
            ("c.xml", versioned_xml("FOLDER-2", None, "Third")),
            ("d.xml", versioned_xml("FOLDER-2", None, "Fourth")),
        ]);
        let (tenders, _, _) = load_bda_documents(&root).expect("directory should load");
        fs::remove_dir_all(&root).ok();
        let titles: Vec<&str> = tenders.iter().map(|tender| tender.title.as_str()).collect();
        // Tie keeps the first file; the two versionless rows share FOLDER-2
        // so they fuse to one candidate (Fourth's id stays auditable).
        assert_eq!(titles, ["First", "Third"]);
        assert_eq!(
            tenders[1].raw["dossier_members"],
            serde_json::json!(["FOLDER-2"]),
        );
    }

    #[test]
    fn extracts_notice_languages() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
        <ContractNotice xmlns="urn:test">
          <ID>LANG-1</ID>
          <NoticeLanguageCode>NLD</NoticeLanguageCode>
          <ProcurementProject><Name>Taal</Name></ProcurementProject>
        </ContractNotice>"#;
        let tender = parse_xml(xml, "lang.xml").expect("notice should parse");
        assert_eq!(tender.languages, ["nl".to_owned()]);
    }

    #[test]
    fn missing_party_leaves_buyer_empty() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
        <ContractNotice xmlns="urn:test">
          <ID>NOPARTY-1</ID>
          <ProcurementProject><Name>Works</Name>
          <SelectionCriterion><Description>ISO cert</Description></SelectionCriterion></ProcurementProject>
        </ContractNotice>"#;
        let tender = parse_xml(xml, "notice.xml").expect("notice should parse");
        assert_eq!(tender.title, "Works");
        assert_eq!(tender.buyer, None);
        let [requirement] = tender.requirements.as_slice() else {
            panic!("expected one requirement");
        };
        assert_eq!(requirement.text, "ISO cert");
        assert_eq!(requirement.kind, "other");
        assert!(requirement.mandatory);
    }

    #[test]
    fn loads_xml_from_zip() {
        let path =
            std::env::temp_dir().join(format!("aanbestedingen-bda-{}.zip", std::process::id()));
        let file = fs::File::create(&path).expect("zip should create");
        let mut archive = ZipWriter::new(file);
        archive
            .start_file("nested/notice.xml", SimpleFileOptions::default())
            .expect("entry should start");
        archive
            .write_all(b"<Notice><ID>BDA-ZIP</ID><Name>Works</Name></Notice>")
            .expect("entry should write");
        archive.finish().expect("zip should finish");
        let tenders = load_bda(&path).expect("zip should load");
        assert_eq!(tenders.len(), 1);
        assert_eq!(tenders[0].external_id, "BDA-ZIP");
        let _ = fs::remove_file(path);
    }

    /// A real dossier is a folder tree in a zip: several notices, corrigenda of
    /// the same folder at different versions, and the occasional unparseable file.
    /// The import must walk the tree, keep only the highest version per folder and
    /// report the broken entries instead of aborting.
    #[test]
    fn imports_a_dossier_tree_with_versions_and_broken_entries() {
        let path = std::env::temp_dir().join(format!(
            "aanbestedingen-bda-dossier-{}.zip",
            std::process::id()
        ));
        let file = fs::File::create(&path).expect("zip should create");
        let mut archive = ZipWriter::new(file);
        let entry = |archive: &mut ZipWriter<fs::File>, name: &str, body: &str| {
            archive
                .start_file(name, SimpleFileOptions::default())
                .expect("entry should start");
            archive
                .write_all(body.as_bytes())
                .expect("entry should write");
        };
        entry(
            &mut archive,
            "2024/v1.xml",
            "<ContractNotice><ContractFolderID>F-1</ContractFolderID><VersionID>01</VersionID><ProcurementProject><Name>Origin</Name></ProcurementProject></ContractNotice>",
        );
        entry(
            &mut archive,
            "2024/corrections/v2.xml",
            "<ContractNotice><ContractFolderID>F-1</ContractFolderID><VersionID>02</VersionID><ProcurementProject><Name>Corrected</Name></ProcurementProject></ContractNotice>",
        );
        entry(
            &mut archive,
            "2024/other.xml",
            "<ContractNotice><ContractFolderID>F-2</ContractFolderID><ProcurementProject><Name>Second</Name></ProcurementProject></ContractNotice>",
        );
        entry(
            &mut archive,
            "2024/broken.xml",
            "<ContractNotice><unclosed>",
        );
        archive.finish().expect("zip should finish");

        let (tenders, skipped, _) = load_bda_documents(&path).expect("dossier should load");
        assert_eq!(tenders.len(), 2, "one per folder, not one per version");
        let corrected = tenders
            .iter()
            .find(|tender| tender.external_id == "F-1")
            .expect("the corrected folder is present");
        assert_eq!(corrected.title, "Corrected", "the highest version wins");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, "2024/broken.xml");
        let _ = fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn skips_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("aanbestedingen-bda-symlink-{}", std::process::id()));
        fs::create_dir_all(&root).expect("directory should create");
        let source = root.join("notice.xml");
        fs::write(&source, b"<Notice><ID>SYMLINK</ID></Notice>")
            .expect("source notice should write");
        let file_link = root.join("linked.xml");
        symlink(&source, &file_link).expect("file symlink should create");
        let directory_link = root.join("cycle");
        symlink(&root, &directory_link).expect("directory symlink should create");
        let error = documents(&root).expect_err("file symlinks should be rejected");
        assert!(error.to_string().contains("symlink"));
        fs::remove_file(&file_link).expect("file symlink should remove");
        fs::remove_file(&directory_link).expect("directory symlink should remove");
        fs::remove_file(&source).expect("source notice should remove");
        fs::remove_dir(&root).expect("directory should remove");
    }

    #[test]
    fn dossier_identity_survives_publication_id_changes() {
        let old = parse_xml(br"<Notice><PublicationNumber>PUB-1</PublicationNumber><ContractFolderID>FOLDER-1</ContractFolderID><VersionID>1</VersionID></Notice>", "v1.xml").unwrap();
        let new = parse_xml(br"<Notice><PublicationNumber>PUB-2</PublicationNumber><ContractFolderID>FOLDER-1</ContractFolderID><VersionID>2</VersionID></Notice>", "v2.xml").unwrap();
        assert_eq!(old.external_id, "FOLDER-1");
        assert_eq!(old.external_id, new.external_id);
    }

    #[test]
    fn filename_id_distinguishes_extensions_and_separator_text() {
        let xml = parse_xml(b"<Notice><Name>XML</Name></Notice>", "dir/notice.xml").unwrap();
        let pdf = parse_pdf_text("PDF notice", "dir/notice.pdf");
        assert_ne!(xml.external_id, pdf.external_id);
        assert_ne!(
            fallback_id("one/notice.xml"),
            fallback_id("one::notice.xml")
        );
        assert_eq!(xml.raw["source_name"], "dir/notice.xml");
    }

    #[test]
    fn preserves_relative_paths_for_fallback_ids() {
        let root =
            std::env::temp_dir().join(format!("aanbestedingen-bda-dir-{}", std::process::id()));
        let one = root.join("one");
        let two = root.join("two");
        fs::create_dir_all(&one).expect("first directory should create");
        fs::create_dir_all(&two).expect("second directory should create");
        fs::write(
            one.join("notice.xml"),
            b"<Notice><Name>Fallback</Name></Notice>",
        )
        .expect("first notice should write");
        fs::write(
            two.join("notice.xml"),
            b"<Notice><Name>Fallback</Name></Notice>",
        )
        .expect("second notice should write");
        let tenders = load_bda(&root).expect("directory should load");
        let identifiers = tenders
            .iter()
            .map(|tender| tender.external_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            identifiers,
            ["file:one%2Fnotice.xml", "file:two%2Fnotice.xml"]
                .into_iter()
                .collect()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lot_selection_evidence_is_bounded_and_reports_truncation() {
        let requirements = (0..=MAX_LOT_SELECTION_EVIDENCE_RECORDS)
            .map(|index| {
                format!(
                    "<SpecificTendererRequirement><TendererRequirementTypeCode listName=\"types\">TYPE-{index}</TendererRequirementTypeCode></SpecificTendererRequirement>"
                )
            })
            .collect::<String>();
        let xml = format!(
            "<Notice><ProcurementProject><ProcurementProjectLot><ID>LOT-1</ID><TenderingTerms><TendererQualificationRequest>{requirements}</TendererQualificationRequest></TenderingTerms></ProcurementProjectLot></ProcurementProject></Notice>"
        );
        let tender = parse_xml(xml.as_bytes(), "selection-bound.xml").unwrap();
        let evidence = tender.raw["notice_lot_selection_evidence"][0]["selection_evidence"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), MAX_LOT_SELECTION_EVIDENCE_RECORDS);
        assert_eq!(evidence[0]["source_ordinal_path"], json!([0, 0]));
        assert_eq!(
            evidence[MAX_LOT_SELECTION_EVIDENCE_RECORDS - 1]["source_ordinal_path"],
            json!([0, 99])
        );
        assert_eq!(tender.raw["notice_lot_selection_evidence_truncated"], true);
    }

    #[test]
    fn skips_unparseable_documents_instead_of_failing_the_import() {
        let dir =
            std::env::temp_dir().join(format!("aanbestedingen-bda-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("directory should create");
        fs::write(
            dir.join("good.xml"),
            b"<ContractNotice><ProcurementProject><Name>Good notice</Name></ProcurementProject></ContractNotice>",
        )
        .expect("good notice should write");
        fs::write(
            dir.join("broken.xml"),
            b"<ContractNotice><ProcurementProject>",
        )
        .expect("broken notice should write");
        fs::write(dir.join("empty.pdf"), b"not-a-pdf").expect("invalid pdf should write");
        fs::write(dir.join("empty.xml"), b"").expect("empty XML should write");
        fs::write(dir.join("whitespace.xml"), b" \n\t").expect("whitespace XML should write");
        let (tenders, skipped, _) =
            load_bda_documents(&dir).expect("import should tolerate bad documents");
        assert_eq!(tenders.len(), 1);
        assert_eq!(tenders[0].title, "Good notice");
        let skipped_names: std::collections::HashSet<_> =
            skipped.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            skipped_names,
            ["broken.xml", "empty.pdf", "empty.xml", "whitespace.xml"]
                .into_iter()
                .collect()
        );
        assert!(skipped.iter().all(|(_, error)| !error.is_empty()));
        let _ = fs::remove_dir_all(dir);
    }

    fn pdf_with_text(text: &str) -> Vec<u8> {
        use pdf_extract::{Dictionary, Document, Object, Stream};
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut font = Dictionary::new();
        font.set("Type", "Font");
        font.set("Subtype", "Type1");
        font.set("BaseFont", "Helvetica");
        let font_id = doc.add_object(font);
        let content = pdf_extract::content::Content {
            operations: vec![
                pdf_extract::content::Operation::new("BT", vec![]),
                pdf_extract::content::Operation::new(
                    "Tf",
                    vec![Object::Name(b"F1".to_vec()), 12.into()],
                ),
                pdf_extract::content::Operation::new("Tj", vec![Object::string_literal(text)]),
                pdf_extract::content::Operation::new("ET", vec![]),
            ],
        }
        .encode()
        .unwrap();
        let stream_id = doc.add_object(Stream::new(Dictionary::new(), content));
        let mut fonts = Dictionary::new();
        fonts.set("F1", font_id);
        let mut resources = Dictionary::new();
        resources.set("Font", fonts);
        let mut page = Dictionary::new();
        page.set("Type", "Page");
        page.set("Parent", pages_id);
        page.set("MediaBox", vec![0.into(), 0.into(), 595.into(), 842.into()]);
        page.set("Resources", resources);
        page.set("Contents", stream_id);
        let page_id = doc.add_object(page);
        let mut pages = Dictionary::new();
        pages.set("Type", "Pages");
        pages.set("Kids", vec![Object::Reference(page_id)]);
        pages.set("Count", 1);
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let mut catalog = Dictionary::new();
        catalog.set("Type", "Catalog");
        catalog.set("Pages", pages_id);
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn pdf_extraction_rejects_excess_text_while_writing() {
        let pdf = pdf_with_text(&"a".repeat(MAX_PDF_EXTRACT_CHARS + 1));
        assert!(pdf.len() < MAX_BDA_DOCUMENT_BYTES);
        let error = parse_pdf(&pdf, "large.pdf").unwrap_err();
        assert!(error.to_string().contains("character extraction limit"));
        let small = parse_pdf(&pdf_with_text("Electrical works"), "small.pdf").unwrap();
        assert!(small.description.contains("Electrical works"));
    }

    #[test]
    fn pdf_sink_enforces_unicode_character_budget() {
        use std::fmt::Write;
        let mut sink = PdfTextSink::default();
        sink.write_str(&"é".repeat(MAX_PDF_EXTRACT_CHARS)).unwrap();
        assert!(sink.write_str("x").is_err());
        assert_eq!(sink.chars, MAX_PDF_EXTRACT_CHARS);
        assert_eq!(sink.text.len(), MAX_PDF_EXTRACT_CHARS * 2);
    }

    #[test]
    fn xml_retains_provenance_without_source_payload() {
        let xml = b"<Notice><ContractFolderID>folder</ContractFolderID><VersionID>2</VersionID><Name>Works</Name></Notice>";
        let tender = parse_xml(xml, "notice.xml").unwrap();
        assert!(tender.raw.get("xml").is_none());
        assert_eq!(tender.raw["source_bytes"], xml.len());
        assert_eq!(tender.raw["contract_folder_id"], "folder");
        assert_eq!(tender.raw["notice_version"], "2");
    }

    #[test]
    fn parses_pdf_text_fields() {
        let tender = parse_pdf_text(
            "Tender title\nCPV 45221100\nDeadline: 31.12.2026\nValue EUR 125.000\nCertificate required",
            "notice.pdf",
        );
        assert_eq!(tender.external_id, "file:notice.pdf");
        assert_eq!(tender.title, "Tender title");
        assert_eq!(tender.cpv_codes, vec!["45221100"]);
        assert_eq!(tender.deadline.as_deref(), Some("2026-12-31"));
        assert_eq!(tender.estimated_value, Some(Decimal::new(125_000, 0)));
        assert_eq!(tender.requirements.len(), 1);
    }

    #[test]
    fn pdf_value_extracts_postfixed_eur_and_euro_symbol() {
        let t1 = parse_pdf_text(
            "Onderhoudscontract\nCPV 90911200\nDe totale geraamde waarde bedraagt 350.000,00 EUR excl. btw.",
            "postfixed_eur.pdf",
        );
        assert_eq!(t1.estimated_value, Some(Decimal::new(350_000, 0)));

        let t2 = parse_pdf_text(
            "Schoonmaakopdracht\nCPV 90911200\nMontant estimé du marché: 180.000 € hors TVA.",
            "postfixed_symbol.pdf",
        );
        assert_eq!(t2.estimated_value, Some(Decimal::new(180_000, 0)));

        let t3 = parse_pdf_text(
            "Schilderwerken\nCPV 45442100\nTotale raming: 75000 euro.",
            "postfixed_euro_word.pdf",
        );
        assert_eq!(t3.estimated_value, Some(Decimal::new(75_000, 0)));
    }

    #[test]
    fn pdf_value_prioritizes_budget_line_over_incidental_fee() {
        let text = "Inschrijvingsoverzicht\nCPV 72262000\nAdministratiekosten afschrift: 25 EUR per kopie.\nTotale raming van de overheidsopdracht: 420.000,00 EUR excl. btw.\nWaarborgsom: 5.000 EUR.";
        let tender = parse_pdf_text(text, "prioritized_value.pdf");
        assert_eq!(tender.estimated_value, Some(Decimal::new(420_000, 0)));
    }

    /// Measured 2026-10-23 on the Etterbeek website-hosting notice: the PDF
    /// carried 26272702/26272329/22349611 next to the real 72415000, and the
    /// phantom 22-prefix code went on to match retail NACE 47120 as an IT
    /// supplier. Only real CPV 2008 codes survive extraction.
    #[test]
    fn pdf_fallback_drops_unknown_8_digit_runs_as_cpv() {
        let tender = parse_pdf_text(
            "Aankondiging\nCPV 72415000\nReferentie 26272702\nTelefoon 26272329\nDossier 22349611",
            "notice.pdf",
        );
        assert_eq!(tender.cpv_codes, vec!["72415000"]);
        let dropped = tender.raw["dropped_cpv_candidates"]
            .as_array()
            .expect("rejects should be an array");
        let dropped: Vec<&str> = dropped
            .iter()
            .map(|value| value.as_str().expect("rejects should be strings"))
            .collect();
        assert_eq!(dropped, vec!["26272702", "26272329", "22349611"]);
    }

    fn requirement_kinds(tender: &Tender) -> Vec<(&str, bool)> {
        tender
            .requirements
            .iter()
            .map(|requirement| (requirement.kind.as_str(), requirement.mandatory))
            .collect()
    }

    /// Measured 2026-10-23 on the Etterbeek bestek: "Prijs is het enige
    /// gunningscriterium" sat two-thirds into a 24-page PDF the old 20k cap
    /// never kept. Criteria are informational, never bidder qualifications.
    #[test]
    fn pdf_fallback_extracts_award_criteria_section() {
        let tender = parse_pdf_text(
            "BESTEK\nI.10 Gunningscriteria\nPrijs is het enige gunningscriterium. De overheid kiest de economisch meest voordelige offerte.\n\nI.11 Varianten\nVrije varianten worden niet toegelaten.",
            "bestek.pdf",
        );
        let criteria: Vec<&TenderRequirement> = tender
            .requirements
            .iter()
            .filter(|requirement| requirement.kind == "award_criterion")
            .collect();
        assert_eq!(criteria.len(), 1);
        assert!(criteria[0]
            .text
            .contains("Prijs is het enige gunningscriterium"));
        assert!(!criteria[0].text.contains("Varianten"));
        assert!(!criteria[0].mandatory);
        // A passing mention of the criteria inside a later subsection must
        // not outscore the real section with its long tail.
        let tender = parse_pdf_text(
            "BESTEK\nI.10 Gunningscriteria\nPrijs is het enige gunningscriterium.\n\nI.11 Varianten\nGeen varianten.\n\nIII.5 Borgstelling\nDe overheid kan het niveau opnemen in de gunningscriteria en behoudt zich het recht voor om bewijsstukken op te vragen.\n\n2. Beheer van incidenten\n\nDe dienstverlener meldt elk incident binnen vier uur en rapporteert dagelijks over de voortgang van de afhandeling en de genomen maatregelen.",
            "mention.pdf",
        );
        let criteria: Vec<&TenderRequirement> = tender
            .requirements
            .iter()
            .filter(|requirement| requirement.kind == "award_criterion")
            .collect();
        assert_eq!(criteria.len(), 1);
        assert!(criteria[0]
            .text
            .contains("Prijs is het enige gunningscriterium"));
        let tender = parse_pdf_text(
            "CAHIER\nI.10 Critères d'attribution\nLe prix est le seul critère d'attribution.\n\nII.1 Variantes\nAucune variante admise.",
            "cahier.pdf",
        );
        assert!(tender
            .requirements
            .iter()
            .any(|requirement| requirement.kind == "award_criterion"
                && requirement.text.contains("seul critère")));
        let plain = parse_pdf_text("BESTEK\nI.10 Gunningscriteria", "empty.pdf");
        assert!(requirement_kinds(&plain)
            .iter()
            .all(|(kind, _)| *kind != "award_criterion"));
        // A contents page naming the section must not shadow the real one.
        let tender = parse_pdf_text(
            "INHOUD\nI.10 Gunningscriteria ........ 6\nI.11 Varianten ........ 6\nI.10 Gunningscriteria\nPrijs is het enige gunningscriterium.\n\nI.11 Varianten\nGeen varianten.",
            "toc.pdf",
        );
        let criteria: Vec<&TenderRequirement> = tender
            .requirements
            .iter()
            .filter(|requirement| requirement.kind == "award_criterion")
            .collect();
        assert_eq!(criteria.len(), 1);
        assert!(criteria[0]
            .text
            .contains("Prijs is het enige gunningscriterium"));
    }

    #[test]
    fn pdf_fallback_extracts_explicit_lot_counts_only() {
        let tender = parse_pdf_text("BESTEK\nAantal percelen: 3\nWerkzaamheden", "lots.pdf");
        let lots: Vec<&TenderRequirement> = tender
            .requirements
            .iter()
            .filter(|requirement| requirement.kind == "lot")
            .collect();
        assert_eq!(lots.len(), 1);
        assert!(lots[0].text.contains('3'));
        assert!(!lots[0].mandatory);
        let tender = parse_pdf_text("CAHIER\nLe marché comporte un lot unique.", "onelot.pdf");
        assert!(requirement_kinds(&tender)
            .iter()
            .all(|(kind, _)| *kind != "lot"));
    }

    /// S0 probe (`docs/final-work/vendor-probe`): run the existing
    /// deterministic PDF-text extractor on the synthetic bestek and print what
    /// it proposes as JSONL in the annotation schema of
    /// `docs/final-work/benchmark-eisenextractie.md`. Fields the extractor
    /// cannot fill stay `null`; it produces no lot scope at all, so every row
    /// is `unclear`. Run with `-- --nocapture` to see the JSONL. Measured
    /// 2026-11-06: 4 proposals against 11 gold requirements, recorded in
    /// `docs/final-work/vendor-probe/s0-baseline.md`. This test only records
    /// the baseline; it changes no extraction behaviour.
    #[test]
    fn s0_probe_prints_synthetic_bestek_requirements_as_jsonl() {
        use sha2::{Digest, Sha256};

        const TEXT: &str = include_str!("../tests/fixtures/synthetisch-bestek.txt");
        // The gold annotations of this fixture are tied to these exact bytes.
        const GOLD_SHA256: &str =
            "f54bbde32dea156e58950aad9df7de0328e1e38bbaec3f320e5cb45e04ed8041";

        let sha = format!("{:x}", Sha256::digest(TEXT.as_bytes()));
        assert_eq!(
            sha, GOLD_SHA256,
            "synthetic bestek differs from the gold source"
        );

        // `parse_pdf_text` normalises each line and the criteria section, so a
        // proposal is not always a verbatim substring. Map its words back onto
        // the source, requiring only whitespace between them, to recover the
        // extent it was read from.
        fn source_extent(text: &str, extracted: &str) -> Option<(usize, usize)> {
            let words: Vec<&str> = extracted.split_whitespace().collect();
            let first = *words.first()?;
            let mut from = 0;
            while let Some(offset) = text[from..].find(first) {
                let start = from + offset;
                let mut cursor = start;
                let mut complete = true;
                for word in &words {
                    match text[cursor..].find(word) {
                        Some(step) if text[cursor..cursor + step].trim().is_empty() => {
                            cursor += step + word.len();
                        }
                        _ => {
                            complete = false;
                            break;
                        }
                    }
                }
                if complete {
                    return Some((start, cursor));
                }
                from = start + first.len();
            }
            None
        }

        let tender = parse_pdf_text(TEXT, "synthetisch-bestek.txt");
        assert_eq!(
            tender.requirements.len(),
            4,
            "measured baseline proposal count"
        );
        // Measured: the Dutch deadline ("15 januari 2027 om 11.00 uur") is not
        // parsed; `parse_date` expects digits in day-month-year order, so the
        // formal deadline survives only as text inside the document.
        assert!(
            tender.deadline.is_none(),
            "measured baseline: Dutch month names are not parsed"
        );
        for (index, requirement) in tender.requirements.iter().enumerate() {
            let (start, end) = source_extent(TEXT, &requirement.text)
                .expect("every extracted requirement maps back into the source text");
            // The schema has no `other` type: only the award-criteria section
            // maps onto a schema type. Keyword lines stay null, not guessed.
            let requirement_type = match requirement.kind.as_str() {
                "award_criterion" => Some("award"),
                _ => None,
            };
            let verbatim = TEXT.get(start..end) == Some(requirement.text.as_str());
            let row = serde_json::json!({
                "annotation_id": format!("S0-{:02}", index + 1),
                "procedure_key": "SYN-PROBE-001",
                "notice_version": "1",
                "language": "nl",
                "requirement_type": requirement_type,
                "text_quote": requirement.text.clone(),
                "source": {
                    "file_sha256": sha.clone(),
                    "page_or_sheet": 1,
                    "char_span": [start, end],
                },
                "lot_scope": {"kind": "unclear", "lots": []},
                "threshold": null,
                "annotator_id": "s0-bda-parse-pdf-text",
                "notes": format!(
                    "extractor kind={} mandatory={} source_page={:?}; S0 produces no lot \
                     scope, no threshold and no requirement_type; procedure_key, \
                     notice_version and language are fixture constants supplied by this probe, \
                     not extracted; text_quote_verbatim={}",
                    requirement.kind, requirement.mandatory, requirement.source_page, verbatim
                ),
            });
            println!("{}", serde_json::to_string(&row).expect("row serialises"));
        }

        // Second existing path: the attachment pipeline
        // (`bosa::evidence::extract_evidence`) reads already-extracted text and
        // keeps a page number. Same keyword family with more terms, but no
        // award section and no lot structure.
        let evidence = crate::bosa::evidence::extract_evidence(
            uuid::Uuid::nil(),
            "synthetisch-bestek.txt",
            &sha,
            TEXT,
        )
        .expect("attachment evidence extraction");
        assert_eq!(evidence.signals.len(), 3, "measured baseline signal count");
        for (index, signal) in evidence.signals.iter().enumerate() {
            let (start, end) = source_extent(TEXT, &signal.text)
                .expect("every extracted signal maps back into the source text");
            let verbatim = TEXT.get(start..end) == Some(signal.text.as_str());
            let row = serde_json::json!({
                "annotation_id": format!("S0B-{:02}", index + 1),
                "procedure_key": "SYN-PROBE-001",
                "notice_version": "1",
                "language": "nl",
                "requirement_type": null,
                "text_quote": signal.text.clone(),
                "source": {
                    "file_sha256": sha.clone(),
                    "page_or_sheet": signal.source_page.unwrap_or(1),
                    "char_span": [start, end],
                },
                "lot_scope": {"kind": "unclear", "lots": []},
                "threshold": null,
                "annotator_id": "s0-bosa-evidence",
                "notes": format!(
                    "attachment pipeline kind={} mandatory={}; S0 produces no lot scope, no \
                     threshold and no requirement_type; procedure_key, notice_version and \
                     language are fixture constants supplied by this probe, not extracted; \
                     text_quote_verbatim={}",
                    signal.kind, signal.mandatory, verbatim
                ),
            });
            println!("{}", serde_json::to_string(&row).expect("row serialises"));
        }
    }

    #[test]
    fn caps_extracted_pdf_text_in_description_and_raw() {
        // Past the 200k stored-text cap (a 24-page bestek holds ~57k).
        let long = "word ".repeat(50_000);
        let tender = parse_pdf_text(&long, "notice.pdf");
        assert_eq!(tender.description.chars().count(), MAX_PDF_TEXT_CHARS);
        assert_eq!(
            tender.raw["pdf_text"]
                .as_str()
                .expect("pdf text should be a string")
                .chars()
                .count(),
            MAX_PDF_TEXT_CHARS
        );
    }

    #[test]
    fn lot_contract_execution_evidence_is_scoped_and_preserves_code_provenance() {
        let xml = br#"
            <ContractNotice>
              <ProcurementProject>
                <ProcurementProjectLot><ID>LOT-1</ID><TenderingTerms>
                  <ContractExecutionRequirement><ExecutionRequirementCode listName="Execution conditions">GREEN_DELIVERY</ExecutionRequirementCode></ContractExecutionRequirement>
                  <ContractExecutionRequirement><ExecutionRequirementCode listName="Execution conditions">SAFE_HANDLING</ExecutionRequirementCode></ContractExecutionRequirement>
                </TenderingTerms></ProcurementProjectLot>
                <ProcurementProjectLot><ID>LOT-2</ID></ProcurementProjectLot>
              </ProcurementProject>
              <TenderingTerms><ContractExecutionRequirement><ExecutionRequirementCode listName="Notice-wide">DO_NOT_ATTACH</ExecutionRequirementCode></ContractExecutionRequirement></TenderingTerms>
              <ID>EXECUTION-1</ID>
            </ContractNotice>
        "#;
        let tender = parse_xml(xml, "execution-evidence.xml").unwrap();
        assert_eq!(tender.raw["notice_lot_ids_unique"], true);
        assert_eq!(
            tender.raw["notice_lot_contract_execution_evidence"],
            json!([
                {
                    "lot_id": "LOT-1",
                    "conditions": [
                        {"source_element": "ContractExecutionRequirement", "source_path": "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode", "source_ordinal_path": [0], "code": "GREEN_DELIVERY", "code_list_name": "Execution conditions"},
                        {"source_element": "ContractExecutionRequirement", "source_path": "TenderingTerms/ContractExecutionRequirement/ExecutionRequirementCode", "source_ordinal_path": [1], "code": "SAFE_HANDLING", "code_list_name": "Execution conditions"}
                    ]
                },
                {"lot_id": "LOT-2", "conditions": []}
            ])
        );
        assert_eq!(
            tender.raw["notice_lot_contract_execution_evidence_truncated"],
            false
        );
    }

    #[test]
    fn lot_contract_execution_evidence_is_bounded_and_reports_truncation() {
        let conditions = (0..=MAX_LOT_CONTRACT_EXECUTION_CONDITIONS)
            .map(|index| {
                format!(
                    "<ContractExecutionRequirement><ExecutionRequirementCode listName=\"types\">TYPE-{index}</ExecutionRequirementCode></ContractExecutionRequirement>"
                )
            })
            .collect::<String>();
        let xml = format!(
            "<ContractNotice><ProcurementProject><ProcurementProjectLot><ID>LOT-1</ID><TenderingTerms>{conditions}</TenderingTerms></ProcurementProjectLot></ProcurementProject><ID>EXECUTION-CAP</ID></ContractNotice>"
        );
        let tender = parse_xml(xml.as_bytes(), "execution-cap.xml").unwrap();
        let evidence = tender.raw["notice_lot_contract_execution_evidence"][0]["conditions"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), MAX_LOT_CONTRACT_EXECUTION_CONDITIONS);
        assert_eq!(evidence[0]["source_ordinal_path"], json!([0]));
        assert_eq!(
            evidence[MAX_LOT_CONTRACT_EXECUTION_CONDITIONS - 1]["source_ordinal_path"],
            json!([99])
        );
        assert_eq!(
            tender.raw["notice_lot_contract_execution_evidence_truncated"],
            true
        );
    }

    #[test]
    fn lot_subsequent_process_evidence_is_scoped_and_preserves_source_order() {
        let xml = br#"
            <ContractNotice>
              <ProcurementProject>
                <ProcurementProjectLot><ID>LOT-1</ID><TenderingProcess><FrameworkAgreement>
                  <SubsequentProcessTenderRequirement><Name>First later-stage criterion</Name></SubsequentProcessTenderRequirement>
                  <SubsequentProcessTenderRequirement><Name>Second later-stage criterion</Name></SubsequentProcessTenderRequirement>
                </FrameworkAgreement></TenderingProcess></ProcurementProjectLot>
                <ProcurementProjectLot><ID>LOT-2</ID></ProcurementProjectLot>
              </ProcurementProject>
              <TenderingProcess><FrameworkAgreement><SubsequentProcessTenderRequirement><Name>Notice-wide must not attach</Name></SubsequentProcessTenderRequirement></FrameworkAgreement></TenderingProcess>
              <ID>SUBSEQUENT-1</ID>
            </ContractNotice>
        "#;
        let tender = parse_xml(xml, "subsequent-process.xml").unwrap();
        assert_eq!(
            tender.raw["notice_lot_subsequent_process_evidence"],
            json!([
                {"lot_id": "LOT-1", "requirements": [
                    {"source_element": "SubsequentProcessTenderRequirement", "source_path": "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name", "source_ordinal_path": [0], "name": "First later-stage criterion"},
                    {"source_element": "SubsequentProcessTenderRequirement", "source_path": "TenderingProcess/FrameworkAgreement/SubsequentProcessTenderRequirement/Name", "source_ordinal_path": [1], "name": "Second later-stage criterion"}
                ]},
                {"lot_id": "LOT-2", "requirements": []}
            ])
        );
        assert_eq!(
            tender.raw["notice_lot_subsequent_process_evidence_truncated"],
            false
        );
    }

    #[test]
    fn lot_subsequent_process_evidence_is_bounded_and_reports_truncation() {
        let requirements = (0..=MAX_LOT_SUBSEQUENT_PROCESS_REQUIREMENTS)
            .map(|index| format!("<SubsequentProcessTenderRequirement><Name>REQ-{index}</Name></SubsequentProcessTenderRequirement>"))
            .collect::<String>();
        let xml = format!("<ContractNotice><ProcurementProject><ProcurementProjectLot><ID>LOT-1</ID><TenderingProcess><FrameworkAgreement>{requirements}</FrameworkAgreement></TenderingProcess></ProcurementProjectLot></ProcurementProject><ID>SUBSEQUENT-CAP</ID></ContractNotice>");
        let tender = parse_xml(xml.as_bytes(), "subsequent-process-cap.xml").unwrap();
        let evidence = tender.raw["notice_lot_subsequent_process_evidence"][0]["requirements"]
            .as_array()
            .unwrap();
        assert_eq!(evidence.len(), MAX_LOT_SUBSEQUENT_PROCESS_REQUIREMENTS);
        assert_eq!(evidence[99]["source_ordinal_path"], json!([99]));
        assert_eq!(
            tender.raw["notice_lot_subsequent_process_evidence_truncated"],
            true
        );
    }

    #[test]
    fn lot_guarantees_and_curricula_preserve_separate_source_evidence() {
        let xml = br#"
            <ContractNotice>
              <ProcurementProject>
                <ProcurementProjectLot><ID>LOT-1</ID><TenderingTerms>
                  <RequiredFinancialGuarantee><GuaranteeTypeCode listName="guarantee types">PERFORMANCE_BOND<Description>Performance bond description</Description></GuaranteeTypeCode></RequiredFinancialGuarantee>
                  <RequiredFinancialGuarantee><GuaranteeTypeCode>ADVANCE_PAYMENT</GuaranteeTypeCode></RequiredFinancialGuarantee>
                  <RequiredCurriculaCode listName="curriculum codes">CV_TEAM</RequiredCurriculaCode>
                  <RequiredCurriculaCode>CV_MANAGER</RequiredCurriculaCode>
                </TenderingTerms></ProcurementProjectLot>
                <ProcurementProjectLot><ID>LOT-2</ID></ProcurementProjectLot>
              </ProcurementProject>
              <TenderingTerms><RequiredFinancialGuarantee><GuaranteeTypeCode>NOTICE_WIDE</GuaranteeTypeCode></RequiredFinancialGuarantee><RequiredCurriculaCode>NOTICE_WIDE_CV</RequiredCurriculaCode></TenderingTerms>
              <ID>GUARANTEE-CURRICULA-1</ID>
            </ContractNotice>
        "#;
        let tender = parse_xml(xml, "lot-guarantees-curricula.xml").unwrap();
        assert_eq!(
            tender.raw["notice_lot_guarantee_curricula_evidence"],
            json!([
                {"lot_id": "LOT-1", "guarantees": [
                    {"source_element": "RequiredFinancialGuarantee", "source_path": "TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode", "source_ordinal_path": [0], "code": "PERFORMANCE_BOND", "code_list_name": "guarantee types", "description": "Performance bond description"},
                    {"source_element": "RequiredFinancialGuarantee", "source_path": "TenderingTerms/RequiredFinancialGuarantee/GuaranteeTypeCode", "source_ordinal_path": [1], "code": "ADVANCE_PAYMENT", "code_list_name": null, "description": null}
                ], "curricula": [
                    {"source_element": "RequiredCurriculaCode", "source_path": "TenderingTerms/RequiredCurriculaCode", "source_ordinal_path": [0], "code": "CV_TEAM", "code_list_name": "curriculum codes"},
                    {"source_element": "RequiredCurriculaCode", "source_path": "TenderingTerms/RequiredCurriculaCode", "source_ordinal_path": [1], "code": "CV_MANAGER", "code_list_name": null}
                ]},
                {"lot_id": "LOT-2", "guarantees": [], "curricula": []}
            ])
        );
        assert_eq!(
            tender.raw["notice_lot_guarantee_curricula_evidence_truncated"],
            false
        );
    }

    #[test]
    fn lot_guarantees_and_curricula_are_bounded_and_report_truncation() {
        let records = (0..=MAX_LOT_GUARANTEE_CURRICULA_RECORDS)
            .map(|index| format!("<RequiredCurriculaCode>CV-{index}</RequiredCurriculaCode>"))
            .collect::<String>();
        let xml = format!("<ContractNotice><ProcurementProject><ProcurementProjectLot><ID>LOT-1</ID><TenderingTerms>{records}</TenderingTerms></ProcurementProjectLot></ProcurementProject><ID>GUARANTEE-CURRICULA-CAP</ID></ContractNotice>");
        let tender = parse_xml(xml.as_bytes(), "lot-guarantees-curricula-cap.xml").unwrap();
        let evidence = &tender.raw["notice_lot_guarantee_curricula_evidence"][0]["curricula"];
        assert_eq!(
            evidence.as_array().unwrap().len(),
            MAX_LOT_GUARANTEE_CURRICULA_RECORDS
        );
        assert_eq!(evidence[99]["source_ordinal_path"], json!([99]));
        assert_eq!(
            tender.raw["notice_lot_guarantee_curricula_evidence_truncated"],
            true
        );
    }

    #[test]
    fn parses_namespaced_xml() {
        let xml = br#"
            <Notice xmlns="urn:test">
              <RealizedLocation><Address><CountrySubentityCode>BE999</CountrySubentityCode></Address></RealizedLocation>
              <FrameworkMaximumAmount currencyID="EUR">900000.00</FrameworkMaximumAmount>
              <ProcurementProject><Name>Bridge repair</Name><Description>Repair works</Description>
                <ProcurementProjectLot><ID schemeName="lot">LOT-1</ID><ItemClassificationCode>45221100-4</ItemClassificationCode><FrameworkMaximumAmount currencyID="EUR">350000.00</FrameworkMaximumAmount><ProcurementProject><RealizedLocation><Address><CountrySubentityCode>BE231</CountrySubentityCode><CityName>not projected</CityName></Address></RealizedLocation></ProcurementProject><TenderingTerms><TendererQualificationRequest><SpecificTendererRequirement><TendererRequirementTypeCode listName="Synthetic requirement codes">ECONOMIC_FINANCIAL_STANDING</TendererRequirementTypeCode></SpecificTendererRequirement><CompanyLegalFormCode listName="Synthetic legal-form codes">LEGAL_FORM</CompanyLegalFormCode></TendererQualificationRequest><UBLExtensions><UBLExtension><ExtensionContent><EformsExtension><SelectionCriteria><TendererRequirementTypeCode listName="Synthetic selection codes">TECHNICAL_PROFESSIONAL_ABILITY</TendererRequirementTypeCode><Description><Description>Technical experience required</Description></Description></SelectionCriteria></EformsExtension></ExtensionContent></UBLExtension></UBLExtensions><AwardingTerms><AwardingCriterion><SubordinateAwardingCriterion><UBLExtensions><UBLExtension><ExtensionContent><EformsExtension><AwardCriterionParameter><ParameterCode>WEIGHT</ParameterCode><ParameterNumeric>35</ParameterNumeric></AwardCriterionParameter></EformsExtension></ExtensionContent></UBLExtension></UBLExtensions><AwardingCriterionTypeCode>QUALITY</AwardingCriterionTypeCode><Description>Quality of the service</Description><CalculationExpression>source formula</CalculationExpression></SubordinateAwardingCriterion><SubordinateAwardingCriterion><AwardingCriterionTypeCode>PRICE</AwardingCriterionTypeCode><Description>Price</Description></SubordinateAwardingCriterion></AwardingCriterion></AwardingTerms></TenderingTerms></ProcurementProjectLot>
                <ProcurementProjectLot><ID schemeName="lot">LOT-2</ID><ItemClassificationCode>71300000-1</ItemClassificationCode></ProcurementProjectLot>
              </ProcurementProject>
              <TenderingTerms><TendererQualificationRequest><SpecificTendererRequirement><TendererRequirementTypeCode listName="Synthetic notice-wide codes">NOTICE_WIDE</TendererRequirementTypeCode></SpecificTendererRequirement></TendererQualificationRequest><UBLExtensions><UBLExtension><ExtensionContent><EformsExtension><SelectionCriteria><TendererRequirementTypeCode>NOTICE_WIDE_SELECTION</TendererRequirementTypeCode><Description>notice-wide selection must not attach</Description></SelectionCriteria></EformsExtension></ExtensionContent></UBLExtension></UBLExtensions><AwardingTerms><AwardingCriterion><SubordinateAwardingCriterion><AwardingCriterionTypeCode>NOTICE</AwardingCriterionTypeCode><Description>notice-wide criterion must not be attached to a lot</Description></SubordinateAwardingCriterion></AwardingCriterion></AwardingTerms></TenderingTerms>
              <ContractingParty><PartyName>City</PartyName></ContractingParty>
              <ItemClassificationCode>45221100</ItemClassificationCode>
              <CountryCode>BE</CountryCode>
              <SubmissionDeadline>31.12.2026</SubmissionDeadline>
              <EstimatedValue>125000,50</EstimatedValue>
              <Requirement>Certificate required</Requirement>
              <ID>BDA-42</ID>
            </Notice>
        "#;
        let tender = parse_xml(xml, "notice.xml").expect("XML should parse");
        assert_eq!(tender.external_id, "BDA-42");
        assert_eq!(tender.title, "Bridge repair");
        assert_eq!(tender.buyer.as_deref(), Some("City"));
        assert_eq!(tender.estimated_value, Some(Decimal::new(1_250_005, 1)));
        assert_eq!(tender.deadline.as_deref(), Some("2026-12-31"));
        assert_eq!(tender.requirements.len(), 1);
        assert_eq!(
            tender.raw["framework_maximum_amounts"],
            json!(["900000.00", "350000.00"])
        );
        assert_eq!(
            tender.raw["framework_maximum_evidence"],
            json!([
                {"amount": "900000.00", "currency_id": "EUR", "scope": "notice", "lot_id": null},
                {"amount": "350000.00", "currency_id": "EUR", "scope": "lot", "lot_id": "LOT-1"}
            ])
        );
        assert_eq!(tender.raw["notice_lot_ids_unique"], true);
        assert_eq!(
            tender.raw["notice_lot_place_evidence"],
            json!([
                {"lot_id": "LOT-1", "country_subentity_codes": ["BE231"]},
                {"lot_id": "LOT-2", "country_subentity_codes": []}
            ])
        );
        assert_eq!(tender.raw["notice_lot_place_evidence_truncated"], false);
        assert_eq!(
            tender.raw["notice_lot_award_criteria_evidence"],
            json!([
                {
                    "lot_id": "LOT-1",
                    "criteria": [
                        {"source_element": "AwardingCriterion", "source_ordinal_path": [0], "type_code": null, "description": null, "calculation_expression": null, "parameters": []},
                        {"source_element": "SubordinateAwardingCriterion", "source_ordinal_path": [0, 0], "type_code": "QUALITY", "description": "Quality of the service", "calculation_expression": "source formula", "parameters": [{"code": "WEIGHT", "numeric": "35"}]},
                        {"source_element": "SubordinateAwardingCriterion", "source_ordinal_path": [0, 1], "type_code": "PRICE", "description": "Price", "calculation_expression": null, "parameters": []}
                    ]
                },
                {"lot_id": "LOT-2", "criteria": []}
            ])
        );
        assert_eq!(
            tender.raw["notice_lot_award_criteria_evidence_truncated"],
            false
        );
        assert_eq!(
            tender.raw["notice_lot_selection_evidence"],
            json!([
                {
                    "lot_id": "LOT-1",
                    "selection_evidence": [
                        {"source_element": "SpecificTendererRequirement", "source_path": "TenderingTerms/TendererQualificationRequest/SpecificTendererRequirement", "source_ordinal_path": [0, 0], "code": "ECONOMIC_FINANCIAL_STANDING", "code_list_name": "Synthetic requirement codes", "descriptions": []},
                        {"source_element": "CompanyLegalFormCode", "source_path": "TenderingTerms/TendererQualificationRequest/CompanyLegalFormCode", "source_ordinal_path": [0, 0], "code": "LEGAL_FORM", "code_list_name": "Synthetic legal-form codes", "descriptions": []},
                        {"source_element": "SelectionCriteria", "source_path": "TenderingTerms/UBLExtensions/UBLExtension/ExtensionContent/EformsExtension/SelectionCriteria", "source_ordinal_path": [0, 0], "code": "TECHNICAL_PROFESSIONAL_ABILITY", "code_list_name": "Synthetic selection codes", "descriptions": ["Technical experience required"]}
                    ]
                },
                {"lot_id": "LOT-2", "selection_evidence": []}
            ])
        );
        assert_eq!(tender.raw["notice_lot_selection_evidence_truncated"], false);
        assert_eq!(
            tender.raw["notice_lot_cpv_evidence"],
            json!([
                {"lot_id": "LOT-1", "cpv_codes": ["45221100"]},
                {"lot_id": "LOT-2", "cpv_codes": ["71300000"]}
            ])
        );
    }

    #[test]
    fn lot_award_criteria_parser_caps_hierarchy_and_marks_truncation() {
        let children = "<SubordinateAwardingCriterion><Description>criterion</Description></SubordinateAwardingCriterion>"
            .repeat(MAX_LOT_AWARD_CRITERIA_EVIDENCE + 1);
        let xml = format!(
            "<ContractNotice><ID>CRITERIA-CAP</ID><ProcurementProject><ProcurementProjectLot><ID>LOT-1</ID><TenderingTerms><AwardingTerms><AwardingCriterion>{children}</AwardingCriterion></AwardingTerms></TenderingTerms></ProcurementProjectLot></ProcurementProject></ContractNotice>"
        );
        let tender =
            parse_xml(xml.as_bytes(), "criteria-cap.xml").expect("bounded XML should parse");
        let criteria = tender.raw["notice_lot_award_criteria_evidence"][0]["criteria"]
            .as_array()
            .expect("criteria should be retained as an array");
        assert_eq!(criteria.len(), MAX_LOT_AWARD_CRITERIA_EVIDENCE);
        assert_eq!(criteria[0]["source_element"], "AwardingCriterion");
        assert_eq!(criteria[1]["source_ordinal_path"], json!([0, 0]));
        assert_eq!(
            tender.raw["notice_lot_award_criteria_evidence_truncated"],
            true
        );
    }

    #[test]
    fn duplicate_or_missing_procurement_project_lot_ids_are_not_unique() {
        let duplicate = br#"
            <Notice><ID>NOTICE-1</ID><ProcurementProject>
              <ProcurementProjectLot><ID>LOT-1</ID><ItemClassificationCode>45221100</ItemClassificationCode></ProcurementProjectLot>
              <ProcurementProjectLot><ID>LOT-1</ID><ItemClassificationCode>71300000</ItemClassificationCode></ProcurementProjectLot>
            </ProcurementProject></Notice>
        "#;
        let tender = parse_xml(duplicate, "duplicate-lots.xml").expect("duplicate IDs parse");
        assert_eq!(tender.raw["notice_lot_ids_unique"], false);

        let missing = br#"
            <Notice><ID>NOTICE-2</ID><ProcurementProject>
              <ProcurementProjectLot><ItemClassificationCode>45221100</ItemClassificationCode></ProcurementProjectLot>
            </ProcurementProject></Notice>
        "#;
        let tender = parse_xml(missing, "missing-lot-id.xml").expect("missing ID parses");
        assert_eq!(tender.raw["notice_lot_ids_unique"], false);
    }

    #[test]
    fn falls_back_to_filename_when_id_is_only_nested() {
        let xml = br#"
            <Notice xmlns="urn:test">
              <ContractingParty>
                <PartyIdentification><ID>PARTY-ID</ID></PartyIdentification>
              </ContractingParty>
              <ProcurementProject><Name>Project name</Name></ProcurementProject>
            </Notice>
        "#;
        let tender = parse_xml(xml, "dir/notice-01.xml").expect("XML should parse");
        assert_eq!(tender.external_id, "file:dir%2Fnotice-01.xml");
    }

    #[test]
    fn prefers_notice_and_procurement_project_fields_over_nested_generic_names() {
        let xml = br#"
            <Notice xmlns="urn:test">
              <ContractingParty>
                <PartyIdentification><ID>PARTY-ID</ID></PartyIdentification>
                <PartyName>Buyer name</PartyName>
              </ContractingParty>
              <ProcurementProject>
                <ID>PROJECT-ID</ID>
                <Name>Actual procurement title</Name>
                <Description>Actual procurement description</Description>
              </ProcurementProject>
              <Lot><ID>LOT-ID</ID><Name>Lot title</Name></Lot>
              <ID>NOTICE-ID</ID>
            </Notice>
        "#;
        let tender = parse_xml(xml, "notice.xml").expect("XML should parse");
        assert_eq!(tender.external_id, "NOTICE-ID");
        assert_eq!(tender.title, "Actual procurement title");
        assert_eq!(tender.description, "Actual procurement description");
        assert_eq!(tender.buyer.as_deref(), Some("Buyer name"));
    }

    #[test]
    fn parses_ubl_canonical_notice_fields() {
        // UBL ContractNotice vocabulary (OASIS UBL 2.x): the mandatory
        // ContractFolderID dossier identifier, TenderSubmissionDeadlinePeriod,
        // BudgetAmount totals, and TenderingTerms awarding criteria.
        let xml = br#"
            <ContractNotice xmlns:cbc="urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2"
                            xmlns:cac="urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2">
              <cbc:ID>DOC-2026-001</cbc:ID>
              <cbc:ContractFolderID>BDA-2026-042</cbc:ContractFolderID>
              <cac:ContractingParty><cac:Party><cac:PartyName><cbc:Name>Stad Gent</cbc:Name></cac:PartyName></cac:Party></cac:ContractingParty>
              <cac:ProcurementProject>
                <cbc:Name>Schoolrenovatie</cbc:Name>
                <cbc:Description>Renovatie van een schoolgebouw</cbc:Description>
                <cac:BudgetAmount><cbc:TotalAmount currencyID="EUR">250000</cbc:TotalAmount></cac:BudgetAmount>
                <cac:MainCommodityClassification><cbc:ItemClassificationCode listID="CPV">45214200</cbc:ItemClassificationCode></cac:MainCommodityClassification>
                <cac:RealizedLocation><cbc:CountrySubentityCode>BE-VOV</cbc:CountrySubentityCode><cbc:CityName>Gent</cbc:CityName></cac:RealizedLocation>
              </cac:ProcurementProject>
              <cac:TenderingProcess><cac:TenderSubmissionDeadlinePeriod><cbc:EndDate>2026-11-30</cbc:EndDate></cac:TenderSubmissionDeadlinePeriod></cac:TenderingProcess>
              <cac:TenderingTerms><cac:AwardingTerms><cbc:AwardingCriteria>Laagste prijs</cbc:AwardingCriteria><cbc:LowTendersDescription>Abnormaal lage offertes worden geweerd</cbc:LowTendersDescription></cac:AwardingTerms></cac:TenderingTerms>
            </ContractNotice>
        "#;
        let tender = parse_xml(xml, "notice.xml").expect("XML should parse");
        assert_eq!(tender.external_id, "BDA-2026-042");
        assert_eq!(tender.title, "Schoolrenovatie");
        assert_eq!(tender.buyer.as_deref(), Some("Stad Gent"));
        assert_eq!(tender.description, "Renovatie van een schoolgebouw");
        assert_eq!(
            tender.estimated_value,
            Some(rust_decimal::Decimal::new(250_000, 0))
        );
        assert_eq!(tender.deadline.as_deref(), Some("2026-11-30"));
        assert!(tender.cpv_codes.contains(&"45214200".to_owned()));
        assert!(tender.regions.contains(&"Gent".to_owned()));
        let requirements = tender
            .requirements
            .iter()
            .map(|requirement| requirement.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            requirements,
            ["Laagste prijs", "Abnormaal lage offertes worden geweerd"]
        );
    }
}
