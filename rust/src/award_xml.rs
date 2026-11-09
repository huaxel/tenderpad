use crate::{parse_money, AwardLot};
use quick_xml::events::Event;
use quick_xml::Reader;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::error::Error;

#[derive(Debug, Default)]
struct Node {
    name: String,
    attributes: HashMap<String, String>,
    text: String,
    children: Vec<Node>,
}

fn local_name(name: &[u8]) -> String {
    let value = String::from_utf8_lossy(name);
    value.rsplit(':').next().unwrap_or_default().to_lowercase()
}

fn normalized_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

const MAX_AWARD_XML_DEPTH: usize = 256;
const MAX_AWARD_XML_ELEMENTS: usize = 100_000;
// A single element can carry an unbounded number of attributes, and each
// distinct name is retained, so the element count alone does not bound memory.
const MAX_AWARD_XML_ATTRIBUTES_PER_ELEMENT: usize = 256;

fn read_attributes(
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> Result<HashMap<String, String>, Box<dyn Error>> {
    let mut attributes = HashMap::new();
    let mut seen = 0_usize;
    for attribute in element.attributes().with_checks(false) {
        seen += 1;
        if seen > MAX_AWARD_XML_ATTRIBUTES_PER_ELEMENT {
            return Err(format!(
                "award XML element exceeded the {MAX_AWARD_XML_ATTRIBUTES_PER_ELEMENT} attribute limit"
            )
            .into());
        }
        let attribute = attribute?;
        attributes.insert(
            local_name(attribute.key.as_ref()),
            attribute
                .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)?
                .into_owned(),
        );
    }
    Ok(attributes)
}

fn count_award_element(
    count: &mut usize,
    depth: usize,
    roots: &mut usize,
) -> Result<(), Box<dyn Error>> {
    if depth == 1 {
        *roots += 1;
        if *roots > 1 {
            return Err("award XML contained multiple root elements".into());
        }
    }
    if depth > MAX_AWARD_XML_DEPTH {
        return Err(
            format!("award XML exceeded the {MAX_AWARD_XML_DEPTH} nesting-depth limit").into(),
        );
    }
    *count += 1;
    if *count > MAX_AWARD_XML_ELEMENTS {
        return Err(
            format!("award XML exceeded the {MAX_AWARD_XML_ELEMENTS} element limit").into(),
        );
    }
    Ok(())
}

fn parse_document(data: &[u8]) -> Result<Node, Box<dyn Error>> {
    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut stack = Vec::new();
    let mut root = None;
    let mut element_count = 0;
    let mut root_count = 0;

    loop {
        match reader.read_event_into(&mut buffer)? {
            Event::Start(element) => {
                count_award_element(&mut element_count, stack.len() + 1, &mut root_count)?;
                let attributes = read_attributes(&element, reader.decoder())?;
                stack.push(Node {
                    name: local_name(element.name().as_ref()),
                    attributes,
                    ..Node::default()
                });
            }
            Event::Empty(element) => {
                count_award_element(&mut element_count, stack.len() + 1, &mut root_count)?;
                let attributes = read_attributes(&element, reader.decoder())?;
                let child = Node {
                    name: local_name(element.name().as_ref()),
                    attributes,
                    ..Node::default()
                };
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(child);
                } else {
                    root = Some(child);
                }
            }
            Event::Text(text) => {
                if let Some(node) = stack.last_mut() {
                    let decoded = text.decode()?;
                    node.text.push_str(&quick_xml::escape::unescape(&decoded)?);
                    node.text.push(' ');
                }
            }
            Event::CData(text) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&String::from_utf8_lossy(&text));
                    node.text.push(' ');
                }
            }
            Event::End(_) => {
                let node = stack.pop().ok_or("unexpected XML closing tag")?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    root = Some(node);
                }
            }
            Event::Eof => {
                if !stack.is_empty() {
                    return Err("award XML ended before all elements were closed".into());
                }
                break;
            }
            _ => {}
        }
        buffer.clear();
    }
    root.ok_or_else(|| "award XML contained no root element".into())
}

fn descendants<'a>(node: &'a Node, name: &str, result: &mut Vec<&'a Node>) {
    if node.name == name {
        result.push(node);
    }
    for child in &node.children {
        descendants(child, name, result);
    }
}

fn nodes<'a>(node: &'a Node, name: &str) -> Vec<&'a Node> {
    let mut result = Vec::new();
    descendants(node, name, &mut result);
    result
}

fn text_content(node: &Node) -> String {
    let mut text = node.text.clone();
    for child in &node.children {
        text.push(' ');
        text.push_str(&text_content(child));
    }
    normalized_text(&text)
}

fn first_text(node: &Node, name: &str) -> Option<String> {
    nodes(node, name)
        .into_iter()
        .map(text_content)
        .find(|text| !text.is_empty())
}

fn all_text(node: &Node, name: &str) -> Vec<String> {
    let mut values = Vec::new();
    for text in nodes(node, name).into_iter().map(text_content) {
        if !text.is_empty() && !values.contains(&text) {
            values.push(text);
        }
    }
    values
}

fn first_id(node: &Node, scheme: Option<&str>) -> Option<String> {
    nodes(node, "id").into_iter().find_map(|candidate| {
        let matches_scheme = scheme.is_none_or(|wanted| {
            candidate
                .attributes
                .get("schemename")
                .is_some_and(|value| value.eq_ignore_ascii_case(wanted))
        });
        matches_scheme
            .then(|| text_content(candidate))
            .filter(|text| !text.is_empty())
    })
}

fn id_references(node: &Node, scheme: &str) -> Vec<String> {
    let mut references = Vec::new();
    for candidate in nodes(node, "id") {
        if candidate
            .attributes
            .get("schemename")
            .is_some_and(|value| value.eq_ignore_ascii_case(scheme))
        {
            let reference = text_content(candidate);
            if !reference.is_empty() && !references.contains(&reference) {
                references.push(reference);
            }
        }
    }
    references
}

fn parse_value(value: Option<String>) -> Option<Decimal> {
    parse_money(&value?)
}

fn procurement_cpv_codes(root: &Node) -> Vec<String> {
    fn visit(node: &Node, values: &mut Vec<String>) {
        for child in &node.children {
            if ["lot", "procurementprojectlot", "tenderlot"].contains(&child.name.as_str()) {
                continue;
            }
            if child.name == "itemclassificationcode" {
                let text = text_content(child);
                if !text.is_empty() && !values.contains(&text) {
                    values.push(text);
                }
            }
            visit(child, values);
        }
    }

    let mut values = Vec::new();
    for project in nodes(root, "procurementproject") {
        visit(project, &mut values);
    }
    values
}

/// Index lot identifiers to their classification codes once.
///
/// Resolving this per lot result re-walked the whole document for every result,
/// so a notice with many lots cost a multiple of the document size each time:
/// quadratic work driven by the input. The element limit alone does not bound
/// that, because it caps the document rather than the traversal count.
fn lot_cpv_index(root: &Node) -> HashMap<String, Vec<String>> {
    let mut index = HashMap::new();
    for name in ["lot", "procurementprojectlot", "tenderlot"] {
        for lot in nodes(root, name) {
            let Some(candidate_id) = first_id(lot, Some("lot")).or_else(|| first_id(lot, None))
            else {
                continue;
            };
            if candidate_id.is_empty() || index.contains_key(&candidate_id) {
                continue;
            }
            let codes = all_text(lot, "itemclassificationcode");
            if !codes.is_empty() {
                index.insert(candidate_id, codes);
            }
        }
    }
    index
}

type OrganizationMap = HashMap<String, (Option<String>, Option<String>)>;
type PartyMembers = std::collections::BTreeMap<String, Option<bool>>;

/// Missing fields are unknown, not contradictory replacements. Nonempty
/// disagreements must never turn declaration order into source authority.
fn merge_known<T: PartialEq>(
    target: &mut Option<T>,
    incoming: Option<T>,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    if let Some(value) = incoming {
        match target {
            Some(previous) if previous != &value => {
                return Err(format!("conflicting {context}").into());
            }
            Some(_) => {}
            None => *target = Some(value),
        }
    }
    Ok(())
}

fn organization_map(root: &Node) -> Result<OrganizationMap, Box<dyn Error>> {
    let mut organizations = OrganizationMap::new();
    for party in nodes(root, "party")
        .into_iter()
        .chain(nodes(root, "organization"))
    {
        let Some(organization_id) =
            first_id(party, Some("organization")).or_else(|| first_id(party, None))
        else {
            continue;
        };
        let name = first_text(party, "partyname").or_else(|| first_text(party, "name"));
        // Retain existing per-definition field precedence. This boundary
        // checks repeated identity definitions, not every XML identifier scheme.
        let identifier = first_text(party, "companyid");
        if identifier.is_none() && name.is_none() {
            continue; // A reference is not a replacement definition.
        }
        let previous = organizations.entry(organization_id).or_default();
        merge_known(&mut previous.0, identifier, "organization identifiers")?;
        merge_known(&mut previous.1, name, "organization names")?;
    }
    Ok(organizations)
}

#[derive(Default)]
struct AwardReferences {
    organizations: OrganizationMap,
    tendering_party_to_orgs: HashMap<String, PartyMembers>,
    tender_to_tendering_party: HashMap<String, String>,
    lot_tenders: HashMap<String, String>,
}

fn insert_reference(
    index: &mut HashMap<String, String>,
    key: String,
    value: String,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    match index.entry(key) {
        std::collections::hash_map::Entry::Occupied(entry) if entry.get() != &value => {
            Err(format!("conflicting {context}").into())
        }
        std::collections::hash_map::Entry::Occupied(_) => Ok(()),
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(value);
            Ok(())
        }
    }
}

/// Both public projections admit the same identity graph. Reference-only
/// nodes contribute no definitions; repeated coherent views may enrich unknowns.
fn award_references(root: &Node) -> Result<AwardReferences, Box<dyn Error>> {
    let mut index = AwardReferences {
        organizations: organization_map(root)?,
        ..AwardReferences::default()
    };
    for party in nodes(root, "tenderingparty") {
        let Some(party_id) = first_id(party, Some("tendering-party")) else {
            continue;
        };
        let mut members = PartyMembers::new();
        for tenderer in nodes(party, "tenderer") {
            if let Some(organization) = first_id(tenderer, Some("organization")) {
                let lead =
                    first_text(tenderer, "groupleadindicator").and_then(|s| s.parse::<bool>().ok());
                merge_known(
                    members.entry(organization).or_default(),
                    lead,
                    "group-lead indicators",
                )?;
            }
        }
        if members.is_empty() {
            continue;
        }
        if let Some(previous) = index.tendering_party_to_orgs.get_mut(&party_id) {
            if !previous.keys().eq(members.keys()) {
                return Err("conflicting tendering-party members".into());
            }
            for (organization, lead) in members {
                merge_known(
                    previous
                        .get_mut(&organization)
                        .expect("matching member keys"),
                    lead,
                    "group-lead indicators",
                )?;
            }
        } else {
            index.tendering_party_to_orgs.insert(party_id, members);
        }
    }
    for tender in nodes(root, "lottender") {
        let Some(id) = first_id(tender, Some("tender")) else {
            continue;
        };
        if let Some(party) = first_id(tender, Some("tendering-party")) {
            insert_reference(
                &mut index.tender_to_tendering_party,
                id.clone(),
                party,
                "tender-to-party references",
            )?;
        }
        if let Some(lot) = first_id(tender, Some("lot")) {
            insert_reference(&mut index.lot_tenders, id, lot, "tender-to-lot references")?;
        }
    }
    Ok(index)
}

/// Semantic states of a BOSA/eForms lot result. The source code remains stored
/// verbatim; this type centralizes only the known winner-selection meanings.
/// These per-lot states do not establish whether the overall procedure is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinnerSelectionState {
    WinnerSelected,
    CompetitionClosedWithoutWinner,
    CompetitionOngoingWithoutWinner,
    Unreported,
    Unrecognized,
}

impl WinnerSelectionState {
    pub fn from_source_code(code: Option<&str>) -> Self {
        match code {
            Some("selec-w") => Self::WinnerSelected,
            Some("clos-nw") => Self::CompetitionClosedWithoutWinner,
            Some("open-nw") => Self::CompetitionOngoingWithoutWinner,
            Some(code) if !code.trim().is_empty() => Self::Unrecognized,
            _ => Self::Unreported,
        }
    }

    pub const fn has_selected_winner(self) -> bool {
        matches!(self, Self::WinnerSelected)
    }
}

/// Published lot-result evidence, without interpreting whether a procurement
/// is fully closed. Missing lot IDs or status codes remain explicit unknowns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LotOutcomeEvidence {
    pub result_id: Option<String>,
    pub lot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lot_id_kind: Option<String>,
    pub winner_selection_status: Option<String>,
    pub non_award_reason: Option<String>,
}

fn is_legacy_f03_notice(root: &Node) -> bool {
    root.name == "ted_esenders" && !nodes(root, "f03_2014").is_empty()
}

pub fn parse_lot_outcomes(data: &[u8]) -> Result<Vec<LotOutcomeEvidence>, Box<dyn Error>> {
    let root = parse_document(data)?;
    if is_legacy_f03_notice(&root)
        && nodes(&root, "lotresult").is_empty()
        && !nodes(&root, "award_contract").is_empty()
    {
        return legacy_f03_awards(&root, "").map(|awards| {
            awards
                .into_iter()
                .map(|award| LotOutcomeEvidence {
                    result_id: award.settled_contract_references.first().cloned(),
                    lot_id: Some(award.lot.lot_id),
                    lot_id_kind: award.lot_id_kind,
                    winner_selection_status: award.winner_selection_status,
                    non_award_reason: None,
                })
                .collect()
        });
    }
    let references = award_references(&root)?;
    nodes(&root, "lotresult")
        .into_iter()
        .map(|result| result_outcome(result, &references))
        .collect()
}

fn result_outcome(
    result: &Node,
    references: &AwardReferences,
) -> Result<LotOutcomeEvidence, Box<dyn Error>> {
    let mut lot_ids = Vec::new();
    for id in nodes(result, "id") {
        if id
            .attributes
            .get("schemename")
            .is_some_and(|s| s.eq_ignore_ascii_case("lot"))
        {
            let value = text_content(id);
            if !value.is_empty() && !lot_ids.contains(&value) {
                lot_ids.push(value);
            }
        }
    }
    if lot_ids.len() > 1 {
        return Err("multiple lots in one result; refusing arbitrary attribution".into());
    }
    if lot_ids.is_empty() {
        for id in nodes(result, "id") {
            if id
                .attributes
                .get("schemename")
                .is_some_and(|s| s.eq_ignore_ascii_case("tender"))
            {
                if let Some(lot) = references.lot_tenders.get(&text_content(id)) {
                    if !lot_ids.contains(lot) {
                        lot_ids.push(lot.clone());
                    }
                }
            }
        }
        if lot_ids.len() > 1 {
            return Err(
                "referenced tenders map to multiple lots; refusing arbitrary attribution".into(),
            );
        }
    }
    let lot_id = lot_ids.into_iter().next();
    let statuses: Vec<_> = nodes(result, "tenderresultcode")
        .into_iter()
        .filter(|node| {
            node.attributes
                .get("listname")
                .is_some_and(|name| name.eq_ignore_ascii_case("winner-selection-status"))
        })
        .map(text_content)
        .filter(|s| !s.is_empty())
        .collect();
    if statuses.len() > 1 && statuses.iter().any(|s| s != &statuses[0]) {
        return Err("conflicting winner selection codes for one lot result".into());
    }
    Ok(LotOutcomeEvidence {
        result_id: first_id(result, Some("result")),
        lot_id,
        lot_id_kind: None,
        winner_selection_status: statuses.first().cloned(),
        non_award_reason: first_text(result, "decisionreasoncode"),
    })
}

pub fn parse_award_xml(
    data: &[u8],
    publication_number: &str,
) -> Result<Vec<AwardLot>, Box<dyn Error>> {
    parse_award_xml_with_tenderers(data, publication_number).map(|lots| {
        lots.into_iter()
            .map(|evidence| {
                let mut lot = evidence.lot;
                // Preserve the pre-existing BDA extraction behavior. BOSA uses
                // parse_award_xml_with_tenderers and requires contract links.
                let mut legacy_linked_tenderers = Vec::new();
                for tenderer in &evidence.linked_tenderers {
                    if !legacy_linked_tenderers
                        .iter()
                        .any(|linked: &&LinkedTendererEvidence| {
                            linked.tendering_party_reference == tenderer.tendering_party_reference
                                && linked.organization_reference == tenderer.organization_reference
                        })
                    {
                        legacy_linked_tenderers.push(tenderer);
                    }
                }
                let (winner_identifier, winner_name) = match legacy_linked_tenderers.as_slice() {
                    [only] => (only.identifier.clone(), only.name.clone()),
                    _ => (None, None),
                };
                lot.winner_identifier = winner_identifier;
                lot.winner_name = winner_name;
                lot
            })
            .collect()
    })
}

/// Organization evidence linked through a result's tender references. The
/// tendering-party reference preserves grouping, including consortium members;
/// contract references identify tenders selected for an award.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LinkedTendererEvidence {
    pub tender_reference: String,
    pub tendering_party_reference: String,
    pub organization_reference: String,
    pub identifier: Option<String>,
    pub name: Option<String>,
    pub group_lead: Option<bool>,
    pub contract_references: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AwardLotWithTenderers {
    pub lot: AwardLot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lot_id_kind: Option<String>,
    pub linked_tenderers: Vec<LinkedTendererEvidence>,
    pub contract_linked_tenderers: Vec<LinkedTendererEvidence>,
    pub settled_contract_references: Vec<String>,
    pub winner_selection_status: Option<String>,
}

fn reference_parts(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

fn legacy_reference_matches(source: &str, expected: &str) -> bool {
    let source = reference_parts(source);
    let expected = reference_parts(expected);
    // Exact complete references may be embedded in a BOSA publication reference.
    // Keep token boundaries: 1234 is not a match for 912345.
    if source.len() >= 2
        && source.iter().map(String::len).sum::<usize>() >= 8
        && expected.windows(source.len()).any(|parts| parts == source)
    {
        return true;
    }
    // Legacy BOSA references encode a dossier as PPP...-<id>/<sub-id>/<series>/<year>.
    // F03 may instead say <series>/<year>/<sequence> - ID<id>. Require both
    // the explicit dossier ID and the series/year, not an arbitrary shared number.
    if source.len() < 3 {
        return false;
    }
    let Some(id) = source.last().and_then(|part| part.strip_prefix("ID")) else {
        return false;
    };
    if id.len() < 4 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let has_dossier_id = expected
        .windows(2)
        .any(|parts| parts[0].starts_with("PPP") && parts[1] == id);
    let has_series_year = expected.windows(2).any(|parts| parts == &source[..2]);
    has_dossier_id && has_series_year
}

/// Validate an XML-only F03 attachment against stable IDs in its BOSA reference.
/// This is for the explicit checksum-bypass path only; ordinary downloads still
/// require the server-provided SHA256 checksum.
pub fn validate_legacy_f03_award_xml(
    data: &[u8],
    expected_references: &[&str],
) -> Result<(), Box<dyn Error>> {
    let root = parse_document(data)?;
    if !is_legacy_f03_notice(&root) {
        return Err("XML attachment is not a TED F03 notice".into());
    }
    let source_references = nodes(&root, "reference_number")
        .into_iter()
        .chain(nodes(&root, "contract_no"))
        .map(text_content)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if !source_references.iter().any(|source| {
        expected_references
            .iter()
            .any(|expected| legacy_reference_matches(source, expected))
    }) {
        return Err("F03 XML reference does not match the BOSA publication".into());
    }
    if legacy_f03_awards(&root, "")?.is_empty() {
        return Err("F03 XML contains no awarded contract".into());
    }
    Ok(())
}

fn legacy_f03_awards(
    root: &Node,
    publication_number: &str,
) -> Result<Vec<AwardLotWithTenderers>, Box<dyn Error>> {
    let mut results = Vec::new();
    let mut global_cpv = Vec::new();
    // Notice-level classifications only: another lot's CPVs must never
    // become fallback history for an award whose lot cannot be identified.
    for code in nodes(root, "object_contract")
        .into_iter()
        .flat_map(|object| &object.children)
        .filter(|child| matches!(child.name.as_str(), "cpv_main" | "cpv_additional"))
        .flat_map(|classification| nodes(classification, "cpv_code"))
        .filter_map(|node| node.attributes.get("code").cloned())
        .filter(|code| !code.trim().is_empty())
    {
        if !global_cpv.contains(&code) {
            global_cpv.push(code);
        }
    }

    for (index, award_contract) in nodes(root, "award_contract").into_iter().enumerate() {
        let item = award_contract
            .attributes
            .get("item")
            .cloned()
            .filter(|item| !item.trim().is_empty())
            .unwrap_or_else(|| (index + 1).to_string());
        let explicit_lot = first_text(award_contract, "lot_no")
            .or_else(|| first_text(award_contract, "lot_number"));
        let (lot_id, lot_id_kind) = match explicit_lot.clone() {
            Some(lot) => (
                if lot.to_ascii_uppercase().starts_with("LOT-") {
                    lot
                } else {
                    format!("LOT-{lot}")
                },
                None,
            ),
            None => (format!("AWARD-ITEM-{item}"), Some("award-item".to_owned())),
        };
        let contract_references = all_text(award_contract, "contract_no");
        let awarded_contracts = nodes(award_contract, "awarded_contract");
        if awarded_contracts.is_empty() {
            continue;
        }

        let mut linked_tenderers = Vec::new();
        let mut reported_values = Vec::new();
        for awarded in awarded_contracts {
            if let Some(value) = parse_value(first_text(awarded, "val_total")) {
                if !reported_values.contains(&value) {
                    reported_values.push(value);
                }
            }
            for contractor in nodes(awarded, "contractor") {
                let name = first_text(contractor, "officialname");
                let identifier = first_text(contractor, "nationalid")
                    .or_else(|| first_text(contractor, "companyid"));
                if name.is_none() && identifier.is_none() {
                    continue;
                }
                let evidence = LinkedTendererEvidence {
                    tender_reference: contract_references.first().cloned().unwrap_or_default(),
                    // The legacy form has no stable tender/party/org references.
                    // Empty strings are intentional; never invent identifiers.
                    tendering_party_reference: String::new(),
                    organization_reference: identifier.clone().unwrap_or_default(),
                    identifier,
                    name,
                    group_lead: None,
                    contract_references: contract_references.clone(),
                };
                if !linked_tenderers
                    .iter()
                    .any(|existing: &LinkedTendererEvidence| {
                        existing.identifier == evidence.identifier && existing.name == evidence.name
                    })
                {
                    linked_tenderers.push(evidence);
                }
            }
        }

        let (winner_identifier, winner_name) = match linked_tenderers.as_slice() {
            [only] => (
                (!contract_references.is_empty())
                    .then(|| only.identifier.clone())
                    .flatten(),
                only.name.clone(),
            ),
            _ => (None, None),
        };
        let mut cpv_codes = Vec::new();
        let descriptors = nodes(root, "object_descr");
        let unpartitioned = descriptors.len() == 1 && !nodes(root, "no_lot_division").is_empty();
        for descriptor in descriptors
            .into_iter()
            .filter(|node| match explicit_lot.as_deref() {
                Some(lot) => first_text(node, "lot_no")
                    .or_else(|| first_text(node, "lot_number"))
                    .is_some_and(|number| number == lot),
                None => unpartitioned,
            })
        {
            for code in nodes(descriptor, "cpv_code")
                .into_iter()
                .filter_map(|node| node.attributes.get("code").cloned())
            {
                if !cpv_codes.contains(&code) {
                    cpv_codes.push(code);
                }
            }
        }
        if cpv_codes.is_empty() {
            cpv_codes = global_cpv.clone();
        }
        let value = match reported_values.as_slice() {
            [only] => Some(*only),
            _ => None,
        };
        results.push(AwardLotWithTenderers {
            lot: AwardLot {
                publication_number: publication_number.to_owned(),
                lot_id,
                winner_identifier,
                winner_name,
                value,
                lowest_value: None,
                highest_value: None,
                cpv_codes,
            },
            lot_id_kind,
            contract_linked_tenderers: linked_tenderers.clone(),
            linked_tenderers,
            settled_contract_references: contract_references,
            winner_selection_status: Some("selec-w".to_owned()),
        });
    }
    Ok(results)
}

pub fn parse_award_xml_with_tenderers(
    data: &[u8],
    publication_number: &str,
) -> Result<Vec<AwardLotWithTenderers>, Box<dyn Error>> {
    let root = parse_document(data)?;
    if is_legacy_f03_notice(&root)
        && nodes(&root, "lotresult").is_empty()
        && !nodes(&root, "award_contract").is_empty()
    {
        return legacy_f03_awards(&root, publication_number);
    }
    let references = award_references(&root)?;
    let organizations = &references.organizations;
    let tendering_party_to_orgs = &references.tendering_party_to_orgs;
    let tender_to_tendering_party = &references.tender_to_tendering_party;
    let mut tender_to_contracts: HashMap<String, Vec<String>> = HashMap::new();
    let mut contract_to_tenders: HashMap<String, Vec<String>> = HashMap::new();
    for settled_contract in nodes(&root, "settledcontract") {
        let Some(contract_id) = first_id(settled_contract, Some("contract")) else {
            continue;
        };
        for tender_id in id_references(settled_contract, "tender") {
            let contracts = tender_to_contracts.entry(tender_id.clone()).or_default();
            if !contracts.contains(&contract_id) {
                contracts.push(contract_id.clone());
            }
            let tenders = contract_to_tenders.entry(contract_id.clone()).or_default();
            if !tenders.contains(&tender_id) {
                tenders.push(tender_id);
            }
        }
    }

    let global_cpv = procurement_cpv_codes(&root);
    let lot_cpvs = lot_cpv_index(&root);
    let mut results = Vec::new();
    for result in nodes(&root, "lotresult") {
        let mut tender_ids = Vec::new();
        for id in nodes(result, "id") {
            if id
                .attributes
                .get("schemename")
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("tender"))
            {
                let tender_id = text_content(id);
                if !tender_id.is_empty() && !tender_ids.contains(&tender_id) {
                    tender_ids.push(tender_id);
                }
            }
        }
        let settled_contract_references = id_references(result, "contract");
        let outcome = result_outcome(result, &references)?;
        let Some(lot_id) = outcome.lot_id else {
            continue;
        };
        let mut linked_tenderers = Vec::new();
        for tender_id in &tender_ids {
            let Some(tendering_party_id) = tender_to_tendering_party.get(tender_id) else {
                continue;
            };
            for (organization_id, group_lead) in tendering_party_to_orgs
                .get(tendering_party_id)
                .into_iter()
                .flatten()
            {
                if linked_tenderers
                    .iter()
                    .any(|linked: &LinkedTendererEvidence| {
                        linked.tender_reference == *tender_id
                            && linked.tendering_party_reference == *tendering_party_id
                            && linked.organization_reference == *organization_id
                    })
                {
                    continue;
                }
                let (identifier, name) = organizations
                    .get(organization_id)
                    .cloned()
                    .unwrap_or((None, None));
                let contract_references = tender_to_contracts
                    .get(tender_id)
                    .into_iter()
                    .flatten()
                    .filter(|contract| settled_contract_references.contains(contract))
                    .cloned()
                    .collect();
                linked_tenderers.push(LinkedTendererEvidence {
                    tender_reference: tender_id.clone(),
                    tendering_party_reference: tendering_party_id.clone(),
                    organization_reference: organization_id.clone(),
                    identifier,
                    name,
                    group_lead: *group_lead,
                    contract_references,
                });
            }
        }
        let winner_selection_status = outcome.winner_selection_status;
        let mut contract_linked_tenderers = linked_tenderers
            .iter()
            .filter(|tenderer| !tenderer.contract_references.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        // A lot result may cite a settled contract without repeating its tender
        // reference. Resolve that contract through its declared LotTender links.
        for contract_id in &settled_contract_references {
            for tender_id in contract_to_tenders.get(contract_id).into_iter().flatten() {
                let Some(tendering_party_id) = tender_to_tendering_party.get(tender_id) else {
                    continue;
                };
                for (organization_id, group_lead) in tendering_party_to_orgs
                    .get(tendering_party_id)
                    .into_iter()
                    .flatten()
                {
                    let evidence = contract_linked_tenderers.iter_mut().find(|evidence| {
                        evidence.tender_reference == *tender_id
                            && evidence.tendering_party_reference == *tendering_party_id
                            && evidence.organization_reference == *organization_id
                    });
                    if let Some(evidence) = evidence {
                        if !evidence.contract_references.contains(contract_id) {
                            evidence.contract_references.push(contract_id.clone());
                        }
                        continue;
                    }
                    let (identifier, name) = organizations
                        .get(organization_id)
                        .cloned()
                        .unwrap_or((None, None));
                    contract_linked_tenderers.push(LinkedTendererEvidence {
                        tender_reference: tender_id.clone(),
                        tendering_party_reference: tendering_party_id.clone(),
                        organization_reference: organization_id.clone(),
                        identifier,
                        name,
                        group_lead: *group_lead,
                        contract_references: vec![contract_id.clone()],
                    });
                }
            }
        }
        let mut unique_winner_organizations = Vec::new();
        for tenderer in &contract_linked_tenderers {
            if !unique_winner_organizations
                .iter()
                .any(|winner: &&LinkedTendererEvidence| {
                    winner.organization_reference == tenderer.organization_reference
                })
            {
                unique_winner_organizations.push(tenderer);
            }
        }
        let is_winner_selected =
            WinnerSelectionState::from_source_code(winner_selection_status.as_deref())
                .has_selected_winner();
        let (winner_identifier, winner_name) =
            match (is_winner_selected, unique_winner_organizations.as_slice()) {
                (true, [only]) => (only.identifier.clone(), only.name.clone()),
                _ => (None, None),
            };
        let value = parse_value(
            first_text(result, "reestimatedvalueamount")
                .or_else(|| first_text(result, "maximumvalueamount"))
                .or_else(|| first_text(result, "payableamount")),
        );
        // BT-710/711: lowest/highest admissible tender values. Stored as-is;
        // the audit test below pins that a highest bid is never mistaken for
        // the awarded value. CSV extracts (ted_csv.rs) do not carry these
        // columns, so range coverage is eForms-XML-only until stated otherwise.
        let lowest_value = parse_value(first_text(result, "lowertenderamount"));
        let highest_value = parse_value(first_text(result, "highertenderamount"));
        let cpv_codes = lot_cpvs
            .get(&lot_id)
            .cloned()
            .unwrap_or_else(|| global_cpv.clone());
        results.push(AwardLotWithTenderers {
            lot: AwardLot {
                publication_number: publication_number.to_owned(),
                lot_id,
                winner_identifier,
                winner_name,
                value,
                lowest_value,
                highest_value,
                cpv_codes,
            },
            linked_tenderers,
            contract_linked_tenderers,
            settled_contract_references,
            winner_selection_status,
            lot_id_kind: None,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission_company(base: u32) -> String {
        format!("{base:08}{:02}", 97 - base % 97)
    }
    fn admission_organization(id: &str, name: &str) -> String {
        format!(
            r#"<Party><PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification><PartyName><Name>{name}</Name></PartyName><PartyLegalEntity><CompanyID>{id}</CompanyID></PartyLegalEntity></Party>"#
        )
    }
    fn admission_notice(
        orgs: &str,
        early_definition: bool,
        statuses: &str,
        folder: &str,
    ) -> String {
        let party = r#"<TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer></TenderingParty>"#;
        let early = if early_definition { party } else { "" };
        let late = if early_definition { "" } else { party };
        format!(
            r#"<ContractAwardNotice><ContractFolderID>{folder}</ContractFolderID><Name>Synthetic audit notice</Name><ProcurementProject><ProcurementProjectLot><ID schemeName="lot">LOT-1</ID><ItemClassificationCode>72000000</ItemClassificationCode></ProcurementProjectLot></ProcurementProject>{early}<LotResult><ID schemeName="result">RES-1</ID><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot>{statuses}<LotTender><ID schemeName="tender">TEN-1</ID></LotTender><SettledContract><ID schemeName="contract">CON-1</ID></SettledContract></LotResult><SettledContract><ID schemeName="contract">CON-1</ID><LotTender><ID schemeName="tender">TEN-1</ID></LotTender></SettledContract><LotTender><ID schemeName="tender">TEN-1</ID><TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotTender>{late}{orgs}</ContractAwardNotice>"#
        )
    }

    const ADMISSION_SELECTED: &str =
        r#"<TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode>"#;
    fn admitted_tender(xml: &str) -> Result<crate::Tender, crate::bosa::BosaError> {
        let publication = serde_json::json!({"publicationWorkspaceId":"audit-workspace","publicationType":"ACTIVE","noticeIds":["audit-notice"],"dossier":{"titles":[{"text":"Synthetic parser audit"}]}});
        let workspace = serde_json::json!({"id":"audit-workspace","noticeType":"can-general","versions":[{"status":"PUBLISHED","activationDate":"2026-10-30","notice":{"id":"audit-notice","xmlContent":xml}}]});
        crate::bosa::publication_to_tender(&publication, &workspace, crate::bosa::Environment::Prod)
    }
    fn assert_award_admission_rejected(xml: &str) {
        assert!(
            parse_lot_outcomes(xml.as_bytes()).is_err(),
            "outcome projection admitted ambiguity"
        );
        assert!(
            parse_award_xml_with_tenderers(xml.as_bytes(), "audit").is_err(),
            "strict award projection admitted ambiguity"
        );
        assert!(
            parse_award_xml(xml.as_bytes(), "audit").is_err(),
            "legacy projection admitted ambiguity"
        );
        assert!(
            admitted_tender(xml).is_err(),
            "source normalization admitted ambiguity"
        );
    }
    struct AdmissionScratch(std::path::PathBuf);
    impl AdmissionScratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("award-admission-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for AdmissionScratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn award_reference_definition_order_survives_admission_and_history() {
        let scratch = AdmissionScratch::new();
        let alpha = admission_company(1234567);
        let definition = admission_organization(&alpha, "Synthetic Alpha");
        let reference = r#"<Party><PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification></Party>"#;
        for early in [false, true] {
            for reference_last in [false, true] {
                let orgs = if reference_last {
                    format!("{definition}{reference}")
                } else {
                    format!("{reference}{definition}")
                };
                let xml = admission_notice(&orgs, early, ADMISSION_SELECTED, "FOLDER-A");
                let tender = admitted_tender(&xml).unwrap();
                assert_eq!(tender.raw["bosa_award_lots"][0]["winner_identifier"], alpha);
                assert_eq!(tender.raw["bosa_lot_outcomes"][0]["lot_id"], "LOT-1");
                let database = scratch.0.join(format!("{early}-{reference_last}.duckdb"));
                {
                    let store = crate::store::TenderStore::open(&database).unwrap();
                    store.upsert_many([&tender]).unwrap();
                }
                let batch =
                    crate::bosa_award_history::collect_bosa_award_history(&[&database]).unwrap();
                assert_eq!(batch.records.len(), 1);
                assert_eq!(
                    batch.records[0].0,
                    crate::kbo::normalize_enterprise_number(&alpha).unwrap()
                );
            }
        }
    }

    #[test]
    fn award_identity_conflicts_are_rejected_by_all_projections() {
        let a = admission_organization(&admission_company(1234567), "Synthetic Alpha");
        let b = admission_organization(&admission_company(2345678), "Synthetic Beta");
        let same_id_different_name =
            admission_organization(&admission_company(1234567), "Conflicting name");
        for orgs in [
            format!("{a}{b}"),
            format!("{b}{a}"),
            format!("{a}{same_id_different_name}"),
            format!("{same_id_different_name}{a}"),
        ] {
            assert_award_admission_rejected(&admission_notice(
                &orgs,
                false,
                ADMISSION_SELECTED,
                "FOLDER-A",
            ));
        }
        let base = admission_notice(&a, false, ADMISSION_SELECTED, "FOLDER-A");
        let conflicting_party = r#"<TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-2</ID></Tenderer></TenderingParty>"#;
        let conflicting_tender = r#"<LotTender><ID schemeName="tender">TEN-1</ID><TenderingParty><ID schemeName="tendering-party">TPA-2</ID></TenderingParty></LotTender>"#;
        let conflicting_lot = r#"<LotTender><ID schemeName="tender">TEN-1</ID><TenderLot><ID schemeName="lot">LOT-2</ID></TenderLot></LotTender>"#;
        for extra in [conflicting_party, conflicting_tender, conflicting_lot] {
            assert_award_admission_rejected(&base.replace(
                "</ContractAwardNotice>",
                &format!("{extra}</ContractAwardNotice>"),
            ));
        }
    }

    #[test]
    fn award_xml_requires_single_root_in_every_projection() {
        let a = admission_organization(&admission_company(1234567), "Synthetic Alpha");
        let base = admission_notice(&a, false, ADMISSION_SELECTED, "FOLDER-A");
        for xml in [
            format!("{base}{base}"),
            format!("{base}<ignored/>"),
            format!("<ignored/>{base}"),
            "<r/><s/>".to_owned(),
        ] {
            assert!(
                crate::bda::parse_xml(xml.as_bytes(), "audit.xml").is_err(),
                "BDA admitted multiple roots"
            );
            assert_award_admission_rejected(&xml);
        }
    }

    #[test]
    fn award_reference_aliases_enrich_without_selecting_conflicts() {
        let id = admission_company(1234567);
        let named = r#"<Party><PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification><PartyName><Name>Synthetic Alpha</Name></PartyName></Party>"#;
        let identified = format!(
            r#"<Organization><ID schemeName="organization">ORG-1</ID><CompanyID>{id}</CompanyID></Organization>"#
        );
        let full = admission_organization(&id, "Synthetic Alpha");
        for orgs in [
            format!("{named}{identified}"),
            format!("{identified}{named}"),
            format!("{full}{full}"),
        ] {
            let xml = admission_notice(&orgs, false, ADMISSION_SELECTED, "FOLDER-A");
            let rows = parse_award_xml_with_tenderers(xml.as_bytes(), "audit").unwrap();
            assert_eq!(rows[0].lot.winner_identifier.as_deref(), Some(id.as_str()));
            assert_eq!(rows[0].lot.winner_name.as_deref(), Some("Synthetic Alpha"));
        }
    }

    #[test]
    fn award_party_aliases_preserve_members_and_lead_facts() {
        let alpha = admission_organization(&admission_company(1234567), "Synthetic Alpha");
        let beta = admission_organization(&admission_company(2345678), "Synthetic Beta")
            .replace("ORG-1", "ORG-2");
        let plain = r#"<TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer></TenderingParty>"#;
        let lead = plain.replace(
            "</Tenderer>",
            "<GroupLeadIndicator>true</GroupLeadIndicator></Tenderer>",
        );
        let not_lead = plain.replace(
            "</Tenderer>",
            "<GroupLeadIndicator>false</GroupLeadIndicator></Tenderer>",
        );
        let base = admission_notice(&alpha, false, ADMISSION_SELECTED, "FOLDER-A");
        for aliases in [format!("{plain}{lead}"), format!("{lead}{plain}")] {
            let xml = base.replace(plain, &aliases);
            let awards = parse_award_xml_with_tenderers(xml.as_bytes(), "audit").unwrap();
            assert_eq!(awards[0].contract_linked_tenderers.len(), 1);
            assert_eq!(
                awards[0].contract_linked_tenderers[0].group_lead,
                Some(true)
            );
        }
        for aliases in [format!("{lead}{not_lead}"), format!("{not_lead}{lead}")] {
            assert_award_admission_rejected(&base.replace(plain, &aliases));
        }
        let members_a = r#"<TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-1</ID><GroupLeadIndicator>true</GroupLeadIndicator></Tenderer><Tenderer><ID schemeName="organization">ORG-2</ID><GroupLeadIndicator>false</GroupLeadIndicator></Tenderer></TenderingParty>"#;
        let members_b = r#"<TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-2</ID><GroupLeadIndicator>false</GroupLeadIndicator></Tenderer><Tenderer><ID schemeName="organization">ORG-1</ID><GroupLeadIndicator>true</GroupLeadIndicator></Tenderer></TenderingParty>"#;
        let xml = admission_notice(
            &format!("{alpha}{beta}"),
            false,
            ADMISSION_SELECTED,
            "FOLDER-A",
        )
        .replace(plain, &format!("{members_a}{members_b}"));
        let awards = parse_award_xml_with_tenderers(xml.as_bytes(), "audit").unwrap();
        assert_eq!(awards[0].lot.winner_identifier, None);
        assert_eq!(awards[0].contract_linked_tenderers.len(), 2);
        assert_eq!(
            awards[0].contract_linked_tenderers[0].organization_reference,
            "ORG-1"
        );
        assert_eq!(
            awards[0].contract_linked_tenderers[0].group_lead,
            Some(true)
        );
        assert_eq!(
            awards[0].contract_linked_tenderers[1].organization_reference,
            "ORG-2"
        );
        assert_eq!(
            awards[0].contract_linked_tenderers[1].group_lead,
            Some(false)
        );
        assert!(awards[0]
            .contract_linked_tenderers
            .iter()
            .all(|p| p.contract_references == ["CON-1"]));
    }

    #[test]
    fn award_result_guards_are_shared_but_legacy_projection_is_preserved() {
        let a = admission_organization(&admission_company(1234567), "Synthetic Alpha");
        let closed =
            r#"<TenderResultCode listName="winner-selection-status">clos-nw</TenderResultCode>"#;
        for status in [
            format!("{ADMISSION_SELECTED}{closed}"),
            format!("{closed}{ADMISSION_SELECTED}"),
        ] {
            assert_award_admission_rejected(&admission_notice(&a, false, &status, "FOLDER-A"));
        }
        let base = admission_notice(&a, false, ADMISSION_SELECTED, "FOLDER-A");
        let ambiguous = base.replace(r#"<TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot>"#, r#"<TenderLot><ID schemeName="lot">LOT-1</ID><ID schemeName="lot">LOT-2</ID></TenderLot>"#);
        assert_award_admission_rejected(&ambiguous);
        let no_contract = base
            .replace(r#"<SettledContract><ID schemeName="contract">CON-1</ID></SettledContract>"#, "")
            .replace(r#"<SettledContract><ID schemeName="contract">CON-1</ID><LotTender><ID schemeName="tender">TEN-1</ID></LotTender></SettledContract>"#, "");
        assert_eq!(
            parse_award_xml_with_tenderers(no_contract.as_bytes(), "audit").unwrap()[0]
                .lot
                .winner_identifier,
            None
        );
        assert_eq!(
            parse_award_xml(no_contract.as_bytes(), "audit").unwrap()[0]
                .winner_identifier
                .as_deref(),
            Some(admission_company(1234567).as_str())
        );
    }

    #[test]
    fn winner_selection_codes_map_to_per_lot_states_and_preserve_unknowns() {
        assert_eq!(
            WinnerSelectionState::from_source_code(Some("selec-w")),
            WinnerSelectionState::WinnerSelected
        );
        assert_eq!(
            WinnerSelectionState::from_source_code(Some("clos-nw")),
            WinnerSelectionState::CompetitionClosedWithoutWinner
        );
        assert_eq!(
            WinnerSelectionState::from_source_code(Some("open-nw")),
            WinnerSelectionState::CompetitionOngoingWithoutWinner
        );
        assert_eq!(
            WinnerSelectionState::from_source_code(None),
            WinnerSelectionState::Unreported
        );
        assert_eq!(
            WinnerSelectionState::from_source_code(Some("future-code")),
            WinnerSelectionState::Unrecognized
        );
        assert!(WinnerSelectionState::WinnerSelected.has_selected_winner());
        assert!(!WinnerSelectionState::CompetitionClosedWithoutWinner.has_selected_winner());
        assert!(!WinnerSelectionState::CompetitionOngoingWithoutWinner.has_selected_winner());
        assert!(!WinnerSelectionState::Unreported.has_selected_winner());
        assert!(!WinnerSelectionState::Unrecognized.has_selected_winner());
    }

    #[test]
    fn parses_legacy_f03_award_contract_as_marked_award_item() {
        let xml = include_bytes!("../tests/fixtures/bosa-2017-stad-gent-f03.xml");
        let lots = parse_award_xml_with_tenderers(xml, "STAD-GENT-2017-F03")
            .expect("legacy F03 award XML should parse");

        assert_eq!(lots.len(), 1);
        let evidence = &lots[0];
        assert_eq!(evidence.lot.lot_id, "AWARD-ITEM-1");
        assert_eq!(
            evidence.lot.winner_name.as_deref(),
            Some("nv Voorbeeld Leverancier")
        );
        assert_eq!(evidence.lot.winner_identifier, None);
        assert_eq!(evidence.winner_selection_status.as_deref(), Some("selec-w"));
        assert_eq!(
            evidence.settled_contract_references,
            ["ECO/2017/019 - ID4165"]
        );
        assert_eq!(evidence.lot.cpv_codes, ["31000000"]);
        assert_eq!(evidence.lot.value, Some(Decimal::new(54144251, 2)));
        assert_eq!(evidence.contract_linked_tenderers.len(), 1);
        assert_eq!(
            evidence.contract_linked_tenderers[0].name.as_deref(),
            Some("nv Voorbeeld Leverancier")
        );
        assert_eq!(evidence.contract_linked_tenderers[0].identifier, None);
    }

    #[test]
    fn legacy_f03_cpvs_follow_explicit_lot_numbers_not_award_item_order() {
        let xml = br#"<TED_ESENDERS><F03_2014>
            <OBJECT_CONTRACT>
                <CPV_MAIN><CPV_CODE CODE="45000000"/></CPV_MAIN>
                <OBJECT_DESCR ITEM="1"><LOT_NO>1</LOT_NO><CPV_ADDITIONAL><CPV_CODE CODE="45221100"/></CPV_ADDITIONAL></OBJECT_DESCR>
                <OBJECT_DESCR ITEM="2"><LOT_NO>2</LOT_NO><CPV_ADDITIONAL><CPV_CODE CODE="71300000"/></CPV_ADDITIONAL></OBJECT_DESCR>
            </OBJECT_CONTRACT>
            <AWARD_CONTRACT ITEM="1"><LOT_NO>2</LOT_NO><CONTRACT_NO>C2</CONTRACT_NO><AWARDED_CONTRACT><CONTRACTOR><OFFICIALNAME>Second lot supplier</OFFICIALNAME></CONTRACTOR></AWARDED_CONTRACT></AWARD_CONTRACT>
            <AWARD_CONTRACT ITEM="2"><CONTRACT_NO>Unknown lot</CONTRACT_NO><AWARDED_CONTRACT><CONTRACTOR><OFFICIALNAME>Unscoped supplier</OFFICIALNAME></CONTRACTOR></AWARDED_CONTRACT></AWARD_CONTRACT>
        </F03_2014></TED_ESENDERS>"#;
        let awards = parse_award_xml_with_tenderers(xml, "").unwrap();
        assert_eq!(awards[0].lot.lot_id, "LOT-2");
        assert_eq!(awards[0].lot.cpv_codes, ["71300000"]);
        // Unscoped awards may retain notice-wide CPVs, never another lot's CPVs.
        assert_eq!(awards[1].lot.cpv_codes, ["45000000"]);
    }

    #[test]
    fn unrelated_award_contract_tags_are_not_classified_as_ted_f03() {
        let xml = br#"<Other><AWARD_CONTRACT ITEM="1"><AWARDED_CONTRACT><CONTRACTOR><OFFICIALNAME>Not a TED notice</OFFICIALNAME></CONTRACTOR></AWARDED_CONTRACT></AWARD_CONTRACT></Other>"#;
        assert!(parse_award_xml_with_tenderers(xml, "").unwrap().is_empty());
        assert!(parse_lot_outcomes(xml).unwrap().is_empty());
    }

    #[test]
    fn legacy_f03_attachment_requires_a_matching_publication_reference() {
        let xml = include_bytes!("../tests/fixtures/bosa-2017-stad-gent-f03.xml");
        assert!(validate_legacy_f03_award_xml(
            xml,
            &["STAD GENT-PPP0HZ-4165/7087/ECO/2017-F03__3"]
        )
        .is_ok());
        assert!(validate_legacy_f03_award_xml(xml, &["UNRELATED-99999"]).is_err());
        // A shared short number, even the actual dossier number outside its
        // expected structure, does not establish publication identity.
        for reference in [
            "UNRELATED-4165",
            "OTHER-PPP0HZ-4165/7087/OTHER/2017-F03__3",
            "STAD GENT-PPP0HZ-9999/4165/ECO/2017-F03__3",
        ] {
            assert!(validate_legacy_f03_award_xml(xml, &[reference]).is_err());
        }
        assert!(validate_legacy_f03_award_xml(xml, &["ECO/2017/019 - ID4165"]).is_ok());
    }

    #[test]
    fn rejects_excessive_attributes_on_one_element() {
        let attributes: String = (0..=MAX_AWARD_XML_ATTRIBUTES_PER_ELEMENT)
            .map(|index| format!(" a{index}=\"v\""))
            .collect();
        let xml = format!("<r><lot{attributes}/></r>");
        let error = parse_award_xml(xml.as_bytes(), "100-2026")
            .expect_err("an element with too many attributes should be rejected");
        assert!(
            error.to_string().contains("attribute limit"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn many_lots_do_not_multiply_document_traversals() {
        // With the previous per-result lookup this took minutes: every lot
        // result re-walked the whole document three times.
        let lots = 5_000;
        let mut xml = String::from("<r>");
        for index in 0..lots {
            xml.push_str(&format!(
                "<lot><id schemeName=\"lot\">lot-{index}</id><itemclassificationcode>CPV{index}</itemclassificationcode></lot>"
            ));
        }
        for index in 0..lots {
            xml.push_str(&format!(
                "<lotresult><lot><id schemeName=\"lot\">lot-{index}</id></lot></lotresult>"
            ));
        }
        xml.push_str("</r>");

        let started = std::time::Instant::now();
        let results = parse_award_xml(xml.as_bytes(), "100-2026").expect("many lots parse");
        assert_eq!(results.len(), lots);
        assert_eq!(results[7].cpv_codes, vec!["CPV7".to_string()]);
        assert_eq!(
            results[lots - 1].cpv_codes,
            vec![format!("CPV{}", lots - 1)]
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "parsing {lots} lots took {:?}, which suggests quadratic traversal",
            started.elapsed()
        );
    }

    #[test]
    fn rejects_excessive_nesting_depth() {
        let xml = format!(
            "<r>{}x{}</r>",
            "<n>".repeat(MAX_AWARD_XML_DEPTH + 1),
            "</n>".repeat(MAX_AWARD_XML_DEPTH + 1)
        );
        let error = parse_award_xml(xml.as_bytes(), "100-2026")
            .expect_err("deeply nested award XML should be rejected");
        assert!(error.to_string().contains("nesting-depth"));
    }

    #[test]
    fn rejects_excessive_element_count() {
        let xml = format!("<r>{}</r>", "<e/>".repeat(MAX_AWARD_XML_ELEMENTS + 1));
        let error = parse_award_xml(xml.as_bytes(), "100-2026")
            .expect_err("broad award XML should be rejected");
        assert!(error.to_string().contains("element limit"));
    }

    #[test]
    fn audit_highest_bid_is_not_an_awarded_value() {
        let xml = br#"<ContractAwardNotice><LotResult><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot><HigherTenderAmount>999999</HigherTenderAmount></LotResult></ContractAwardNotice>"#;
        let lots = parse_award_xml(xml, "100-2026").unwrap();
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].value, None);
        assert_eq!(
            lots[0].highest_value.map(|d| d.to_string()),
            Some("999999".to_string()),
            "BT-711 lands on highest_value, never on value"
        );
        assert_eq!(lots[0].lowest_value, None);
    }

    #[test]
    fn parses_lowest_and_highest_offer_range() {
        let xml = br#"<ContractAwardNotice><LotResult><TenderLot><ID schemeName="lot">LOT-9</ID></TenderLot><PayableAmount>500000</PayableAmount><LowerTenderAmount>480000</LowerTenderAmount><HigherTenderAmount>750000</HigherTenderAmount></LotResult></ContractAwardNotice>"#;
        let lots = parse_award_xml(xml, "100-2026").unwrap();
        assert_eq!(lots.len(), 1);
        assert_eq!(
            lots[0].value.map(|d| d.to_string()),
            Some("500000".to_string())
        );
        assert_eq!(
            lots[0].lowest_value.map(|d| d.to_string()),
            Some("480000".to_string())
        );
        assert_eq!(
            lots[0].highest_value.map(|d| d.to_string()),
            Some("750000".to_string())
        );
    }

    #[test]
    fn skips_lot_without_identifier() {
        let xml = br"<ContractAwardNotice><LotResult><HigherTenderAmount>10</HigherTenderAmount></LotResult></ContractAwardNotice>";
        assert!(parse_award_xml(xml, "100-2026")
            .expect("award XML should parse")
            .is_empty());
    }

    #[test]
    fn resolves_winner_to_lot_without_party_scheme_name() {
        let xml = br#"
            <ContractAwardNotice xmlns="urn:test">
              <ProcurementProject><ItemClassificationCode>72000000</ItemClassificationCode><ProcurementProjectLot><ID schemeName="lot">LOT-2</ID><ItemClassificationCode>45000000</ItemClassificationCode></ProcurementProjectLot></ProcurementProject>
              <LotResult>
                <ID schemeName="result">RES-1</ID>
                <PayableAmount>368039</PayableAmount>
                <TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode>
                <LotTender><ID schemeName="tender">TEN-1</ID></LotTender>
                <SettledContract><ID schemeName="contract">CON-1</ID></SettledContract>
                <TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot>
              </LotResult>
              <SettledContract>
                <ID schemeName="contract">CON-1</ID>
                <LotTender><ID schemeName="tender">TEN-1</ID></LotTender>
              </SettledContract>
              <LotTender>
                <ID schemeName="tender">TEN-1</ID>
                <TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty>
                <TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot>
              </LotTender>
              <TenderingParty>
                <ID schemeName="tendering-party">TPA-1</ID>
                <Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer>
              </TenderingParty>
              <Party>
                <PartyIdentification><ID>ORG-1</ID></PartyIdentification>
                <PartyName><Name>Example BV</Name></PartyName>
                <PartyLegalEntity><CompanyID>0123456789</CompanyID></PartyLegalEntity>
              </Party>
            </ContractAwardNotice>
        "#;
        let lots = parse_award_xml(xml, "100-2026").expect("award XML should parse");
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].lot_id, "LOT-1");
        assert_eq!(lots[0].winner_identifier.as_deref(), Some("0123456789"));
        assert_eq!(lots[0].winner_name.as_deref(), Some("Example BV"));
        assert_eq!(lots[0].value, Some(Decimal::new(368039, 0)));
        assert_eq!(lots[0].cpv_codes, vec!["72000000"]);
    }
    #[test]
    fn resolves_winner_through_lot_contract_when_result_omits_tender_reference() {
        let xml = br#"
            <ContractAwardNotice>
              <LotTender>
                <ID schemeName="tender">TEN-1</ID>
                <TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty>
                <TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot>
              </LotTender>
              <TenderingParty>
                <ID schemeName="tendering-party">TPA-1</ID>
                <Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer>
              </TenderingParty>
              <SettledContract>
                <ID schemeName="contract">CON-1</ID>
                <LotTender><ID schemeName="tender">TEN-1</ID></LotTender>
              </SettledContract>
              <LotResult>
                <TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode>
                <SettledContract><ID schemeName="contract">CON-1</ID></SettledContract>
                <TenderLot><ID schemeName="lot">LOT-2</ID></TenderLot>
              </LotResult>
              <Party>
                <PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification>
                <PartyName><Name>Winner Example</Name></PartyName>
                <PartyLegalEntity><CompanyID>0123456789</CompanyID></PartyLegalEntity>
              </Party>
            </ContractAwardNotice>
        "#;
        let lots = parse_award_xml_with_tenderers(xml, "100-2026").unwrap();
        assert_eq!(lots.len(), 1);
        assert!(lots[0].linked_tenderers.is_empty());
        assert_eq!(lots[0].contract_linked_tenderers.len(), 1, "{:#?}", lots[0]);
        assert_eq!(lots[0].settled_contract_references, ["CON-1"]);
        assert_eq!(lots[0].winner_selection_status.as_deref(), Some("selec-w"));
        assert_eq!(lots[0].lot.winner_identifier.as_deref(), Some("0123456789"));
        assert_eq!(lots[0].lot.winner_name.as_deref(), Some("Winner Example"));
    }

    #[test]
    fn contract_with_multiple_tenders_keeps_winner_identity_ambiguous() {
        let xml = br#"
            <ContractAwardNotice>
              <LotTender><ID schemeName="tender">TEN-1</ID><TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty></LotTender>
              <LotTender><ID schemeName="tender">TEN-2</ID><TenderingParty><ID schemeName="tendering-party">TPA-2</ID></TenderingParty></LotTender>
              <TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer></TenderingParty>
              <TenderingParty><ID schemeName="tendering-party">TPA-2</ID><Tenderer><ID schemeName="organization">ORG-2</ID></Tenderer></TenderingParty>
              <SettledContract>
                <ID schemeName="contract">CON-1</ID>
                <LotTender><ID schemeName="tender">TEN-1</ID></LotTender>
                <LotTender><ID schemeName="tender">TEN-2</ID></LotTender>
              </SettledContract>
              <LotResult>
                <TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode>
                <SettledContract><ID schemeName="contract">CON-1</ID></SettledContract>
                <TenderLot><ID schemeName="lot">LOT-3</ID></TenderLot>
              </LotResult>
              <Party><PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification><PartyLegalEntity><CompanyID>0123456789</CompanyID></PartyLegalEntity></Party>
              <Party><PartyIdentification><ID schemeName="organization">ORG-2</ID></PartyIdentification><PartyLegalEntity><CompanyID>0987654321</CompanyID></PartyLegalEntity></Party>
            </ContractAwardNotice>
        "#;
        let lots = parse_award_xml_with_tenderers(xml, "100-2026").unwrap();
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].contract_linked_tenderers.len(), 2);
        assert_eq!(lots[0].lot.winner_identifier, None);
        assert_eq!(lots[0].lot.winner_name, None);
    }

    #[test]
    fn selected_result_without_a_contract_link_does_not_infer_a_winner() {
        let xml = br#"<ContractAwardNotice><LotResult><TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode><LotTender><ID schemeName="tender">TEN-1</ID></LotTender><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotResult><LotTender><ID schemeName="tender">TEN-1</ID><TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotTender><TenderingParty><ID schemeName="tendering-party">TPA-1</ID><Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer></TenderingParty><Party><PartyIdentification><ID schemeName="organization">ORG-1</ID></PartyIdentification><PartyLegalEntity><CompanyID>0123</CompanyID></PartyLegalEntity></Party></ContractAwardNotice>"#;
        let strict_lots =
            parse_award_xml_with_tenderers(xml, "100-2026").expect("award XML should parse");
        assert_eq!(strict_lots.len(), 1);
        assert_eq!(strict_lots[0].lot.winner_identifier, None);
        assert_eq!(strict_lots[0].lot.winner_name, None);

        // The legacy BDA wrapper retains its pre-existing extraction behavior;
        // BOSA uses the strict parser above and never promotes this candidate.
        let legacy_lots = parse_award_xml(xml, "100-2026").expect("award XML should parse");
        assert_eq!(legacy_lots[0].winner_identifier.as_deref(), Some("0123"));
    }

    #[test]
    fn lot_outcomes_preserve_codes_and_unresolved_results_without_implying_closure() {
        let xml=br#"<ContractAwardNotice><LotResult><ID schemeName="result">RES-1</ID><TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode><TenderLot><ID schemeName="Lot">LOT-1</ID></TenderLot></LotResult><LotResult><ID schemeName="result">RES-2</ID><TenderResultCode listName="winner-selection-status">clos-nw</TenderResultCode><DecisionReasonCode>other</DecisionReasonCode><TenderLot><ID schemeName="Lot">LOT-2</ID></TenderLot></LotResult><LotResult><ID schemeName="result">RES-3</ID><TenderResultCode listName="unrelated">other</TenderResultCode></LotResult></ContractAwardNotice>"#;
        let rows = parse_lot_outcomes(xml).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].lot_id.as_deref(), Some("LOT-1"));
        assert_eq!(rows[0].winner_selection_status.as_deref(), Some("selec-w"));
        assert_eq!(rows[1].winner_selection_status.as_deref(), Some("clos-nw"));
        assert_eq!(rows[1].non_award_reason.as_deref(), Some("other"));
        assert_eq!(rows[2].lot_id, None);
        assert_eq!(rows[2].winner_selection_status, None);
    }
    #[test]
    fn lot_outcomes_resolve_tender_references_and_reject_conflicts() {
        let xml=br#"<r><LotTender><ID schemeName="tender">T-1</ID><TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot></LotTender><LotResult><ID schemeName="tender">T-1</ID><TenderResultCode listName="winner-selection-status">selec-w</TenderResultCode></LotResult></r>"#;
        assert_eq!(
            parse_lot_outcomes(xml).unwrap()[0].lot_id.as_deref(),
            Some("LOT-1")
        );
        let conflicting=br#"<r><LotTender><ID schemeName="tender">T-1</ID><ID schemeName="lot">LOT-1</ID></LotTender><LotTender><ID schemeName="tender">T-1</ID><ID schemeName="lot">LOT-2</ID></LotTender></r>"#;
        assert!(parse_lot_outcomes(conflicting).is_err());
        let ambiguous=br#"<r><LotResult><ID schemeName="lot">LOT-1</ID><ID schemeName="lot">LOT-2</ID></LotResult></r>"#;
        assert!(parse_lot_outcomes(ambiguous).is_err());
    }
}
