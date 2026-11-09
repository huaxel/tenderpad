use crate::kbo::normalize_enterprise_number;
use crate::AwardRecord;
use duckdb::{AccessMode, Config, Connection};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BosaAwardHistoryReport {
    pub source_notices: usize,
    pub selected_lots: usize,
    pub company_notice_records: usize,
    pub skipped_lots_without_cpv: usize,
    pub skipped_lots_without_contract_link: usize,
    pub skipped_lots_without_usable_supplier_identity: usize,
    pub skipped_non_belgian_or_invalid_ids: usize,
    pub joint_company_lot_participations: usize,
    /// Compatible differing snapshots reconciled without dropping bidder evidence.
    pub merged_compatible_snapshots: usize,
}

/// One named tenderer on a BOSA award lot. `outcome` is `won` when its tender
/// is linked to a settled contract, `not-awarded` when the lot settled a
/// contract with another tenderer but not this one. Lots without any settled
/// contract are not emitted: absent contract links there are not evidence of
/// losing (a snapshot can predate contract publication).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BosaLotBidder {
    pub publication_number: String,
    pub dossier_id: Option<String>,
    pub lot_id: String,
    pub organization_reference: String,
    /// Canonical Belgian enterprise number, when the identifier validates.
    pub enterprise_number: Option<String>,
    pub raw_identifier: Option<String>,
    pub name: Option<String>,
    pub tendering_party_reference: Option<String>,
    pub group_lead: Option<bool>,
    pub outcome: &'static str,
    /// Distinct tendering parties named on this lot.
    pub lot_tenderers: usize,
    pub lowest_tender_value: Option<String>,
    pub highest_tender_value: Option<String>,
    pub cpv_codes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BosaAwardHistoryBatch {
    /// Every source notice with parsed award-lot evidence, including notices
    /// with no winners, so reimports can remove stale rows for that notice.
    pub publications: Vec<String>,
    /// `(enterprise number, company-specific award record)` pairs.
    pub records: Vec<(String, AwardRecord)>,
    /// Named tenderers on lots with a settled contract (winners and others).
    pub bidders: Vec<BosaLotBidder>,
    pub report: BosaAwardHistoryReport,
}

#[derive(Debug, Clone, PartialEq)]
struct SourceNotice {
    workspace_id: String,
    notice_id: String,
    title: String,
    buyer: Option<String>,
    dossier_id: Option<Value>,
    bda_references: Option<Value>,
    award_lots: Vec<Value>,
}

#[derive(Debug, Clone)]
struct ContractParticipant {
    enterprise_number: String,
    name: Option<String>,
    organization_reference: Option<String>,
    tendering_party_reference: Option<String>,
    group_lead: Option<bool>,
    contract_references: Vec<String>,
    joint_participation: bool,
}

#[derive(Default)]
struct CompanyNoticeEvidence {
    names: BTreeSet<String>,
    cpvs: BTreeSet<String>,
    lots: Vec<Value>,
    joint: bool,
}

pub fn publication_key(workspace_id: &str, notice_id: &str) -> String {
    format!("bosa:{workspace_id}:{notice_id}")
}

/// Read the explicitly supplied Production BOSA databases read-only and build
/// CPV-only company history records. BOSA monetary fields are intentionally
/// excluded: they are not established as per-supplier awarded spend.
pub fn collect_bosa_award_history<P: AsRef<Path>>(
    source_databases: &[P],
) -> Result<BosaAwardHistoryBatch, Box<dyn Error>> {
    if source_databases.is_empty() {
        return Err("at least one BOSA source database is required".into());
    }

    let mut notices = BTreeMap::<String, SourceNotice>::new();
    let mut merged = 0;
    for path in source_databases {
        read_source_database(path.as_ref(), &mut notices, &mut merged)?;
    }

    let mut batch = BosaAwardHistoryBatch {
        publications: Vec::with_capacity(notices.len()),
        records: Vec::new(),
        bidders: Vec::new(),
        report: BosaAwardHistoryReport {
            source_notices: notices.len(),
            merged_compatible_snapshots: merged,
            ..BosaAwardHistoryReport::default()
        },
    };

    for (publication, notice) in notices {
        batch.publications.push(publication.clone());
        let records = records_for_notice(&publication, &notice, &mut batch.report)?;
        batch.records.extend(records);
        batch
            .bidders
            .extend(bidders_for_notice(&publication, &notice));
    }
    batch.report.company_notice_records = batch.records.len();
    batch.records.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.publication_number.cmp(&right.1.publication_number))
    });
    Ok(batch)
}

fn read_source_database(
    path: &Path,
    notices: &mut BTreeMap<String, SourceNotice>,
    merged: &mut usize,
) -> Result<(), Box<dyn Error>> {
    let config = Config::default().access_mode(AccessMode::ReadOnly)?;
    let connection = Connection::open_with_flags(path, config)?;
    let mut statement = connection.prepare(
        "SELECT title, buyer, raw FROM tenders WHERE source = 'bda' AND notice_type LIKE 'can%'",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;

    for row in rows {
        let (title, buyer, raw) = row?;
        let Some(raw) = raw else { continue };
        let raw: Value = serde_json::from_str(&raw)?;
        let Some(award_lots) = raw.get("bosa_award_lots").and_then(Value::as_array) else {
            // Old/manual BDA rows without parsed BOSA award evidence are not
            // authoritative replacements for a previously imported notice.
            continue;
        };
        match raw.get("bosa_environment").and_then(Value::as_str) {
            Some("prod") => {}
            Some(environment) => {
                return Err(format!(
                    "BOSA award evidence in {} is tagged {environment:?}, expected production",
                    path.display()
                )
                .into());
            }
            None => {
                return Err(format!(
                    "BOSA award evidence in {} has no environment tag",
                    path.display()
                )
                .into());
            }
        }
        let workspace_id = required_string(&raw, "bosa_publication_workspace_id")?;
        let notice_id = required_string(&raw, "bosa_notice_id")?;
        let publication = publication_key(&workspace_id, &notice_id);
        let notice = SourceNotice {
            workspace_id,
            notice_id,
            title: canonical_buyer(&title),
            buyer: buyer.as_deref().map(canonical_buyer),
            dossier_id: raw.get("bosa_dossier_id").cloned(),
            bda_references: raw.get("bosa_publication_reference_numbers_bda").cloned(),
            award_lots: award_lots.clone(),
        };
        match notices.get(&publication) {
            Some(previous) if !same_notice_text(previous, &notice) => {
                return Err(
                    format!("conflicting BOSA snapshots for publication {publication}").into(),
                );
            }
            Some(previous) if previous.award_lots != notice.award_lots => {
                let kept = contract_links(previous);
                let incoming = contract_links(&notice);
                if !kept.is_superset(&incoming) && !incoming.is_superset(&kept) {
                    return Err(format!(
                        "conflicting BOSA award evidence for publication {publication}"
                    )
                    .into());
                }
                let mut combined = previous.clone();
                combined.award_lots = merge_lots(&previous.award_lots, &notice.award_lots)
                    .map_err(|error| {
                        format!("conflicting BOSA snapshots for publication {publication}: {error}")
                    })?;
                notices.insert(publication, combined);
                *merged += 1;
            }
            Some(_) => {}
            None => {
                notices.insert(publication, notice);
            }
        }
    }
    Ok(())
}

/// Reconcile additive evidence, never pick a snapshot based only on contract links.
/// Multiple result rows for the same lot have no stable result ID here: if those
/// differ, fail closed rather than invent a correspondence between contracts.
fn merge_lots(left: &[Value], right: &[Value]) -> Result<Vec<Value>, String> {
    let group = |lots: &[Value]| -> Result<BTreeMap<String, Vec<Value>>, String> {
        let mut grouped = BTreeMap::<String, Vec<Value>>::new();
        for lot in lots {
            let id = lot
                .get("lot_id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or("missing lot ID")?;
            grouped.entry(id.to_owned()).or_default().push(lot.clone());
        }
        for rows in grouped.values_mut() {
            rows.sort_by_key(Value::to_string);
        }
        Ok(grouped)
    };
    let mut merged = group(left)?;
    for (id, rows) in group(right)? {
        match merged.get_mut(&id) {
            Some(previous) if *previous == rows => {}
            Some(previous) if previous.len() == 1 && rows.len() == 1 => {
                previous[0] = merge_evidence(&previous[0], &rows[0], "lot")?;
            }
            Some(_) => return Err(format!("ambiguous result rows for lot {id}")),
            None => {
                merged.insert(id, rows);
            }
        }
    }
    Ok(merged.into_values().flatten().collect())
}

/// Scalar facts must agree. Only absent/null facts may be enriched, and only
/// explicitly set-valued fields may grow. Participant identity includes the
/// tender and party, not just the organization (one firm can submit many bids).
fn merge_evidence(left: &Value, right: &Value, field: &str) -> Result<Value, String> {
    if left == right || right.is_null() {
        return Ok(left.clone());
    }
    if left.is_null() {
        return Ok(right.clone());
    }
    if let (Some(a), Some(b)) = (left.as_object(), right.as_object()) {
        let mut result = a.clone();
        for (key, value) in b {
            let merged = match result.get(key) {
                Some(previous) => merge_evidence(previous, value, key)?,
                None => value.clone(),
            };
            result.insert(key.clone(), merged);
        }
        return Ok(Value::Object(result));
    }
    if matches!(field, "contract_references" | "settled_contract_references") {
        let mut references = BTreeSet::new();
        for value in [left, right] {
            for reference in value
                .as_array()
                .ok_or("contract references must be arrays")?
            {
                references.insert(
                    reference
                        .as_str()
                        .ok_or("contract reference must be text")?
                        .to_owned(),
                );
            }
        }
        return Ok(json!(references));
    }
    if matches!(field, "linked_tenderers" | "contract_linked_tenderers") {
        let mut participants = BTreeMap::new();
        for value in [left, right] {
            for entry in value.as_array().ok_or("tenderer evidence must be arrays")? {
                let organization = entry
                    .get("organization_reference")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .ok_or("tenderer has no organization reference")?;
                let key = (
                    entry
                        .get("tender_reference")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    entry
                        .get("tendering_party_reference")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    organization.to_owned(),
                );
                let combined = match participants.get(&key) {
                    Some(previous) => merge_evidence(previous, entry, "tenderer")?,
                    None => entry.clone(),
                };
                participants.insert(key, combined);
            }
        }
        return Ok(Value::Array(participants.into_values().collect()));
    }
    Err(format!("disagreeing {field}"))
}

/// Bilingual titles and multi-buyer names are joined with " / " in an order
/// that varies between snapshots of the same notice version.
fn same_notice_text(left: &SourceNotice, right: &SourceNotice) -> bool {
    canonical_buyer(&left.title) == canonical_buyer(&right.title)
        && left.buyer == right.buyer
        && left.dossier_id == right.dossier_id
        && left.bda_references == right.bda_references
}

/// `(lot, organization, contract)` links; a later snapshot can omit them when
/// contract publication was not yet captured, never the reverse.
fn contract_links(notice: &SourceNotice) -> BTreeSet<(String, String, String)> {
    let mut links = BTreeSet::new();
    for lot in &notice.award_lots {
        let lot_id = lot
            .get("lot_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for key in ["linked_tenderers", "contract_linked_tenderers"] {
            for entry in lot.get(key).and_then(Value::as_array).into_iter().flatten() {
                let organization = entry
                    .get("organization_reference")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                for contract in entry
                    .get("contract_references")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    links.insert((
                        lot_id.to_owned(),
                        organization.to_owned(),
                        contract.to_owned(),
                    ));
                }
            }
        }
    }
    links
}

/// Multi-buyer notices join names with " / " in an order that differs between
/// snapshots of the same version; sort the parts so duplicates compare equal.
fn canonical_buyer(buyer: &str) -> String {
    let mut parts = buyer.split(" / ").map(str::trim).collect::<Vec<_>>();
    parts.sort_unstable();
    parts.join(" / ")
}

fn required_string(raw: &Value, key: &str) -> Result<String, Box<dyn Error>> {
    raw.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("BOSA award row has no {key}").into())
}

fn records_for_notice(
    publication: &str,
    notice: &SourceNotice,
    report: &mut BosaAwardHistoryReport,
) -> Result<Vec<(String, AwardRecord)>, Box<dyn Error>> {
    let mut companies = BTreeMap::<String, CompanyNoticeEvidence>::new();
    let mut selected_lots = BTreeMap::<String, Vec<&Value>>::new();

    for lot in &notice.award_lots {
        let status = crate::award_xml::WinnerSelectionState::from_source_code(
            lot.get("winner_selection_status").and_then(Value::as_str),
        );
        if !status.has_selected_winner() {
            continue;
        }
        let lot_id = lot
            .get("lot_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("BOSA selected award in {publication} has no lot ID"))?
            .to_owned();
        selected_lots.entry(lot_id).or_default().push(lot);
    }

    for (lot_id, lot_evidence) in selected_lots {
        // BOSA can report several selected contracts as separate result rows
        // for one lot. Treat them as one lot for counts and CPVs while retaining
        // each contract-linked supplier as distinct award evidence below.
        report.selected_lots += 1;
        let cpvs = lot_evidence
            .iter()
            .flat_map(|lot| {
                lot.get("cpv_codes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if cpvs.is_empty() {
            // An award record without CPV would turn the existing neutral
            // history signal into a negative mismatch for unrelated tenders.
            report.skipped_lots_without_cpv += 1;
            continue;
        }

        let invalid_ids_before = report.skipped_non_belgian_or_invalid_ids;
        let mut all_orgs = BTreeSet::new();
        let mut party_members = HashMap::<String, BTreeSet<String>>::new();
        for lot in &lot_evidence {
            for entry in lot
                .get("contract_linked_tenderers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|entry| has_contract_reference(entry))
            {
                let Some(organization) = entry
                    .get("organization_reference")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                else {
                    continue;
                };
                all_orgs.insert(organization.to_owned());
                if let Some(party) = entry
                    .get("tendering_party_reference")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                {
                    party_members
                        .entry(party.to_owned())
                        .or_default()
                        .insert(organization.to_owned());
                }
            }
        }
        let multi_supplier_lot = all_orgs.len() > 1;
        let lot_id_kind = lot_evidence
            .iter()
            .find_map(|lot| lot.get("lot_id_kind").and_then(Value::as_str));

        let mut participants_by_identity = BTreeMap::<
            (
                String,
                Option<String>,
                Option<String>,
                Option<bool>,
                bool,
                Option<String>,
            ),
            ContractParticipant,
        >::new();
        let mut has_contract_evidence = false;
        for lot in &lot_evidence {
            let (participants, has_evidence) = contract_participants(lot, report)?;
            has_contract_evidence |= has_evidence;
            for mut participant in participants {
                if participant
                    .tendering_party_reference
                    .as_ref()
                    .and_then(|party| party_members.get(party))
                    .is_some_and(|members| members.len() > 1)
                {
                    participant.joint_participation = true;
                }
                let identity = (
                    participant.enterprise_number.clone(),
                    participant.organization_reference.clone(),
                    participant.tendering_party_reference.clone(),
                    participant.group_lead,
                    participant.joint_participation,
                    participant.name.clone(),
                );
                if let Some(existing) = participants_by_identity.get_mut(&identity) {
                    existing
                        .contract_references
                        .extend(participant.contract_references);
                    existing.contract_references.sort();
                    existing.contract_references.dedup();
                } else {
                    participants_by_identity.insert(identity, participant);
                }
            }
        }
        if participants_by_identity.is_empty() {
            if !has_contract_evidence {
                report.skipped_lots_without_contract_link += 1;
            } else if report.skipped_non_belgian_or_invalid_ids == invalid_ids_before {
                // A contract can be explicit while its legacy contractor has
                // no usable organization identifier; preserve the raw evidence
                // but do not silently imply a company-history match.
                report.skipped_lots_without_usable_supplier_identity += 1;
            }
            continue;
        }

        let mut joint_suppliers = BTreeSet::new();
        for participant in participants_by_identity.into_values() {
            let Some(evidence_organization) = participant.organization_reference.as_deref() else {
                // A legacy unique winner identifier can be trusted from the
                // parser's contract-link-gated winner field even where the raw
                // evidence predates the expanded reference list.
                if participant.enterprise_number.is_empty() {
                    continue;
                }
                let entry = companies
                    .entry(participant.enterprise_number.clone())
                    .or_default();
                if let Some(name) = participant.name.as_deref().filter(|name| !name.is_empty()) {
                    entry.names.insert(name.to_owned());
                }
                entry.cpvs.extend(cpvs.iter().cloned());
                let mut lot_record = json!({
                    "lot_id": lot_id,
                    "cpv_codes": cpvs,
                    "participation": "contract-linked-winner",
                    "joint_participation": false,
                    "multi_supplier_lot": false,
                });
                if let Some(kind) = lot_id_kind {
                    lot_record["lot_id_kind"] = json!(kind);
                }
                if !entry.lots.contains(&lot_record) {
                    entry.lots.push(lot_record);
                }
                continue;
            };

            let entry = companies
                .entry(participant.enterprise_number.clone())
                .or_default();
            if let Some(name) = participant.name.as_deref().filter(|name| !name.is_empty()) {
                entry.names.insert(name.to_owned());
            }
            entry.cpvs.extend(cpvs.iter().cloned());
            entry.joint |= participant.joint_participation;
            if participant.joint_participation {
                joint_suppliers.insert(participant.enterprise_number.clone());
            }
            let mut lot_record = json!({
                "lot_id": lot_id,
                "cpv_codes": cpvs,
                "organization_reference": evidence_organization,
                "tendering_party_reference": participant.tendering_party_reference,
                "group_lead": participant.group_lead,
                "contract_references": participant.contract_references,
                "participation": if participant.joint_participation { "joint" } else { "contract-linked" },
                "joint_participation": participant.joint_participation,
                "multi_supplier_lot": multi_supplier_lot,
            });
            if let Some(kind) = lot_id_kind {
                lot_record["lot_id_kind"] = json!(kind);
            }
            if !entry.lots.contains(&lot_record) {
                entry.lots.push(lot_record);
            }
        }
        report.joint_company_lot_participations += joint_suppliers.len();
    }

    Ok(companies
        .into_iter()
        .filter_map(|(enterprise_number, evidence)| {
            if evidence.cpvs.is_empty() {
                return None;
            }
            let winner_names = evidence.names.into_iter().collect::<Vec<_>>();
            let raw = json!({
                "source": "bosa",
                "evidence_basis": "selected-result-and-settled-contract-links",
                "bosa_workspace_id": notice.workspace_id,
                "bosa_notice_id": notice.notice_id,
                "bosa_dossier_id": notice.dossier_id,
                "bosa_publication_reference_numbers_bda": notice.bda_references,
                "joint_participation": evidence.joint,
                "winning_lots": evidence.lots,
                "amounts_used_for_ranking": false,
            });
            Some((
                enterprise_number.clone(),
                AwardRecord {
                    publication_number: publication.to_owned(),
                    title: notice.title.clone(),
                    buyer: notice.buyer.clone(),
                    winner_identifiers: vec![enterprise_number],
                    winner_names,
                    cpv_codes: evidence.cpvs.into_iter().collect(),
                    values: Vec::new(),
                    links: vec![format!(
                        "https://www.publicprocurement.be/publication-workspaces/{}",
                        notice.workspace_id
                    )],
                    raw,
                },
            ))
        })
        .collect())
}

/// Named tenderers per lot, gated on a settled contract in that lot.
fn bidders_for_notice(publication: &str, notice: &SourceNotice) -> Vec<BosaLotBidder> {
    let dossier_id = notice.dossier_id.as_ref().map(|value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    });
    let mut by_lot = BTreeMap::<String, Vec<&Value>>::new();
    for lot in &notice.award_lots {
        if let Some(lot_id) = lot
            .get("lot_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            by_lot.entry(lot_id.to_owned()).or_default().push(lot);
        }
    }
    let mut output = Vec::new();
    for (lot_id, rows) in by_lot {
        let mut tenderers = BTreeMap::<String, (&Value, bool)>::new();
        for lot in &rows {
            for entry in lot
                .get("linked_tenderers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(organization) = entry
                    .get("organization_reference")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                else {
                    continue;
                };
                let won = has_contract_reference(entry);
                let slot = tenderers
                    .entry(organization.to_owned())
                    .or_insert((entry, won));
                slot.1 |= won;
            }
        }
        if !tenderers.values().any(|(_, won)| *won) {
            continue;
        }
        let parties = tenderers
            .values()
            .map(|(entry, _)| {
                entry
                    .get("tendering_party_reference")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect::<BTreeSet<_>>()
            .len();
        let first = |key: &str| {
            rows.iter()
                .find_map(|lot| lot.get(key).and_then(Value::as_str).map(str::to_owned))
        };
        let cpvs = rows
            .iter()
            .flat_map(|lot| {
                lot.get("cpv_codes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        for (organization, (entry, won)) in tenderers {
            let raw_identifier = entry
                .get("identifier")
                .and_then(Value::as_str)
                .map(str::to_owned);
            output.push(BosaLotBidder {
                publication_number: publication.to_owned(),
                dossier_id: dossier_id.clone(),
                lot_id: lot_id.clone(),
                organization_reference: organization,
                enterprise_number: raw_identifier
                    .as_deref()
                    .and_then(canonical_belgian_enterprise),
                raw_identifier,
                name: entry.get("name").and_then(Value::as_str).map(str::to_owned),
                tendering_party_reference: entry
                    .get("tendering_party_reference")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                group_lead: entry.get("group_lead").and_then(Value::as_bool),
                outcome: if won { "won" } else { "not-awarded" },
                lot_tenderers: parties,
                lowest_tender_value: first("lowest_tender_value"),
                highest_tender_value: first("highest_tender_value"),
                cpv_codes: cpvs.clone(),
            });
        }
    }
    output
}

fn contract_participants(
    lot: &Value,
    report: &mut BosaAwardHistoryReport,
) -> Result<(Vec<ContractParticipant>, bool), Box<dyn Error>> {
    let linked = lot
        .get("contract_linked_tenderers")
        .and_then(Value::as_array);
    if let Some(winner) = lot
        .get("winner_identifier")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        let Some(enterprise_number) = canonical_belgian_enterprise(winner) else {
            report.skipped_non_belgian_or_invalid_ids += 1;
            return Ok((Vec::new(), true));
        };
        let mut evidence = None;
        if let Some(linked) = linked {
            let contract_entries = linked
                .iter()
                .filter(|entry| has_contract_reference(entry))
                .collect::<Vec<_>>();
            let organizations = contract_entries
                .iter()
                .filter_map(|entry| entry.get("organization_reference").and_then(Value::as_str))
                .collect::<BTreeSet<_>>();
            if organizations.is_empty() {
                return Err(format!(
                    "BOSA winner ID has no settled-contract link on lot {}",
                    lot.get("lot_id")
                        .and_then(Value::as_str)
                        .unwrap_or("(unknown)")
                )
                .into());
            }
            if organizations.len() != 1 {
                return Err(format!(
                    "BOSA winner ID conflicts with multiple contract-linked organizations on lot {}",
                    lot.get("lot_id").and_then(Value::as_str).unwrap_or("(unknown)")
                )
                .into());
            }
            let linked_ids = contract_entries
                .iter()
                .filter_map(|entry| entry.get("identifier").and_then(Value::as_str))
                .filter_map(canonical_belgian_enterprise)
                .collect::<BTreeSet<_>>();
            if linked_ids.len() != 1
                || !linked_ids.contains(&canonical_belgian_enterprise(winner).unwrap_or_default())
            {
                return Err(format!(
                    "BOSA winner ID disagrees with contract-linked organization on lot {}",
                    lot.get("lot_id")
                        .and_then(Value::as_str)
                        .unwrap_or("(unknown)")
                )
                .into());
            }
            evidence = Some((
                organizations
                    .iter()
                    .next()
                    .expect("one organization")
                    .to_string(),
                contract_entries,
            ));
        }
        let participant = if let Some((organization, entries)) = evidence {
            let contracts = entries
                .iter()
                .flat_map(|entry| {
                    entry
                        .get("contract_references")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                })
                .collect::<BTreeSet<_>>();
            let parties = entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("tendering_party_reference")
                        .and_then(Value::as_str)
                })
                .collect::<BTreeSet<_>>();
            let group_leads = entries
                .iter()
                .filter_map(|entry| entry.get("group_lead").and_then(Value::as_bool))
                .collect::<BTreeSet<_>>();
            let name = lot
                .get("winner_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    entries.iter().find_map(|entry| {
                        entry.get("name").and_then(Value::as_str).map(str::to_owned)
                    })
                });
            ContractParticipant {
                enterprise_number,
                name,
                organization_reference: Some(organization),
                tendering_party_reference: (parties.len() == 1)
                    .then(|| parties.into_iter().next().expect("one party").to_owned()),
                group_lead: (group_leads.len() == 1)
                    .then(|| *group_leads.iter().next().expect("one group-lead value")),
                contract_references: contracts.into_iter().collect(),
                joint_participation: false,
            }
        } else {
            ContractParticipant {
                enterprise_number,
                name: lot
                    .get("winner_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                organization_reference: None,
                tendering_party_reference: None,
                group_lead: None,
                contract_references: Vec::new(),
                joint_participation: false,
            }
        };
        return Ok((vec![participant], true));
    }

    let Some(linked) = linked else {
        return Ok((Vec::new(), false));
    };
    let mut by_organization = BTreeMap::<String, ContractParticipant>::new();
    let mut party_members = HashMap::<String, BTreeSet<String>>::new();
    let contract_entries = linked
        .iter()
        .filter(|entry| has_contract_reference(entry))
        .collect::<Vec<_>>();
    let has_contract_evidence = !contract_entries.is_empty();
    for entry in contract_entries {
        let Some(organization) = entry
            .get("organization_reference")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        if let Some(party) = entry
            .get("tendering_party_reference")
            .and_then(Value::as_str)
        {
            party_members
                .entry(party.to_owned())
                .or_default()
                .insert(organization.to_owned());
        }
        let Some(identifier) = entry.get("identifier").and_then(Value::as_str) else {
            report.skipped_non_belgian_or_invalid_ids += 1;
            continue;
        };
        let Some(enterprise_number) = canonical_belgian_enterprise(identifier) else {
            report.skipped_non_belgian_or_invalid_ids += 1;
            continue;
        };
        let contracts = entry
            .get("contract_references")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let candidate = ContractParticipant {
            enterprise_number,
            name: entry.get("name").and_then(Value::as_str).map(str::to_owned),
            organization_reference: Some(organization.to_owned()),
            tendering_party_reference: entry
                .get("tendering_party_reference")
                .and_then(Value::as_str)
                .map(str::to_owned),
            group_lead: entry.get("group_lead").and_then(Value::as_bool),
            contract_references: contracts,
            joint_participation: false,
        };
        by_organization
            .entry(organization.to_owned())
            .and_modify(|current| {
                current
                    .contract_references
                    .extend(candidate.contract_references.clone());
                current.contract_references.sort();
                current.contract_references.dedup();
                if current.name.is_none() {
                    current.name = candidate.name.clone();
                }
                if current.group_lead.is_none() {
                    current.group_lead = candidate.group_lead;
                }
            })
            .or_insert(candidate);
    }

    for participant in by_organization.values_mut() {
        participant.joint_participation = participant
            .tendering_party_reference
            .as_ref()
            .and_then(|party| party_members.get(party))
            .is_some_and(|members| members.len() > 1);
    }

    Ok((
        by_organization.into_values().collect(),
        has_contract_evidence,
    ))
}

fn has_contract_reference(entry: &Value) -> bool {
    entry
        .get("contract_references")
        .and_then(Value::as_array)
        .is_some_and(|references| {
            references.iter().any(|value| {
                value
                    .as_str()
                    .is_some_and(|reference| !reference.trim().is_empty())
            })
        })
}

fn is_uuid_suffix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn canonical_belgian_enterprise(raw: &str) -> Option<String> {
    let raw = raw.trim().to_ascii_uppercase();
    // BOSA CompanyID can append the organization's UUID to the legal number.
    // Strip only that exact suffix form; the number itself still has to pass
    // the strict Belgian length and checksum validation below.
    let identifier = match raw.split_once('_') {
        Some((identifier, suffix)) if is_uuid_suffix(suffix) => identifier,
        Some(_) => return None,
        None => raw.as_str(),
    };
    // BOSA uses an explicit BE VAT prefix for some Belgian suppliers. Do not
    // strip arbitrary country/scheme letters: e.g. KRS or DE numbers can have
    // ten digits after punctuation is removed and otherwise look like a KBO ID.
    let number = identifier
        .strip_prefix("TVA BE")
        .or_else(|| identifier.strip_prefix("TVABE"))
        .or_else(|| identifier.strip_prefix("BE"))
        .unwrap_or(identifier);
    if number.chars().any(|character| {
        !character.is_ascii_digit()
            && !character.is_ascii_whitespace()
            && !matches!(character, '.' | '-')
    }) {
        return None;
    }
    let mut digits = number
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    if digits.len() == 9 {
        digits.insert(0, '0');
    }
    if digits.len() != 10
        || !matches!(digits.as_bytes()[0], b'0' | b'1')
        || digits.bytes().all(|digit| digit == b'0')
    {
        return None;
    }
    let first_eight = digits[..8].parse::<u32>().ok()?;
    let check_digits = digits[8..].parse::<u32>().ok()?;
    if check_digits != 97 - first_eight % 97 {
        return None;
    }
    normalize_enterprise_number(&digits).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lot(status: &str, cpvs: &[&str], linked: Value) -> Value {
        json!({
            "lot_id": "LOT-1",
            "winner_selection_status": status,
            "winner_identifier": null,
            "winner_name": null,
            "cpv_codes": cpvs,
            "reported_value": "2500000.00",
            "contract_linked_tenderers": linked,
        })
    }

    fn notice(lots: Vec<Value>) -> SourceNotice {
        SourceNotice {
            workspace_id: "workspace-uuid".into(),
            notice_id: "notice-uuid".into(),
            title: "BOSA award".into(),
            buyer: Some("Buyer".into()),
            dossier_id: Some(json!(12345)),
            bda_references: Some(json!(["BDA-1"])),
            award_lots: lots,
        }
    }

    #[test]
    fn contract_link_superset_wins_and_disjoint_evidence_conflicts() {
        let with_contract = notice(vec![lot(
            "selec-w",
            &["45000000"],
            json!([{"organization_reference":"ORG-1","identifier":"0202239951","contract_references":["CON-1"]}]),
        )]);
        let without = notice(vec![lot(
            "selec-w",
            &["45000000"],
            json!([{"organization_reference":"ORG-1","identifier":"0202239951","contract_references":[]}]),
        )]);
        let other = notice(vec![lot(
            "selec-w",
            &["45000000"],
            json!([{"organization_reference":"ORG-2","identifier":"0403170701","contract_references":["CON-2"]}]),
        )]);
        assert!(contract_links(&with_contract).is_superset(&contract_links(&without)));
        assert!(!contract_links(&with_contract).is_superset(&contract_links(&other)));
        let mut swapped = with_contract.clone();
        swapped.title = "FR titel / NL titel".into();
        let mut original = with_contract.clone();
        original.title = "NL titel / FR titel".into();
        assert!(same_notice_text(&original, &swapped));
    }

    fn snapshot_file(directory: &Path, name: &str, lots: &[Value]) -> std::path::PathBuf {
        let path = directory.join(format!("{name}.duckdb"));
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(
            "CREATE TABLE tenders (source VARCHAR, notice_type VARCHAR, title VARCHAR, buyer VARCHAR, raw VARCHAR)"
        ).unwrap();
        let raw = json!({
            "bosa_environment": "prod", "bosa_publication_workspace_id": "w",
            "bosa_notice_id": "n", "bosa_award_lots": lots,
        });
        connection
            .execute(
                "INSERT INTO tenders VALUES ('bda', 'can-general', 'Title', NULL, ?)",
                duckdb::params![raw.to_string()],
            )
            .unwrap();
        path
    }

    #[test]
    fn snapshot_merge_preserves_all_bidders_in_every_source_order() {
        let directory = std::env::temp_dir().join(format!("bosa-merge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let winner = json!({"organization_reference":"ORG-1", "identifier":"0202239951",
            "tendering_party_reference":"TP-1", "contract_references":["CON-1"]});
        let mut paths = Vec::new();
        for index in 0..3 {
            let mut evidence = lot("selec-w", &["45000000"], json!([winner.clone()]));
            let mut bidders = vec![winner.clone()];
            if index > 0 {
                bidders.push(json!({"organization_reference":format!("ORG-{}", index+1),
                    "tendering_party_reference":format!("TP-{}", index+1),
                    "contract_references":[]}));
            }
            evidence["linked_tenderers"] = json!(bidders);
            paths.push(snapshot_file(&directory, &index.to_string(), &[evidence]));
        }
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let batch = collect_bosa_award_history(&order.map(|i| &paths[i])).unwrap();
            assert_eq!(batch.bidders.len(), 3, "source order {order:?}");
            assert_eq!(
                batch.bidders.iter().filter(|b| b.outcome == "won").count(),
                1
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn equal_contract_links_do_not_hide_conflicting_supplier_identity() {
        let directory =
            std::env::temp_dir().join(format!("bosa-conflict-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let mut a = lot(
            "selec-w",
            &["45000000"],
            json!([{
                "organization_reference":"ORG-1", "identifier":"0202239951", "contract_references":["CON-1"]
            }]),
        );
        a["linked_tenderers"] = a["contract_linked_tenderers"].clone();
        let mut b = a.clone();
        b["contract_linked_tenderers"][0]["identifier"] = json!("0403170701");
        b["linked_tenderers"] = b["contract_linked_tenderers"].clone();
        let paths = [
            snapshot_file(&directory, "a", &[a]),
            snapshot_file(&directory, "b", &[b]),
        ];
        for order in [[0, 1], [1, 0]] {
            assert!(collect_bosa_award_history(&order.map(|i| &paths[i])).is_err());
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn gaining_contract_links_preserves_earlier_named_non_winners() {
        let winner = json!({"organization_reference":"ORG-1", "identifier":"0202239951",
            "contract_references":["CON-1"]});
        let mut earlier = lot("selec-w", &["45000000"], json!([]));
        earlier["linked_tenderers"] = json!([
            {"organization_reference":"ORG-1", "identifier":"0202239951", "contract_references":[]},
            {"organization_reference":"ORG-2", "contract_references":[]}
        ]);
        let mut later = lot("selec-w", &["45000000"], json!([winner.clone()]));
        later["linked_tenderers"] = json!([winner]);
        let forward = merge_lots(&[earlier.clone()], &[later.clone()]).unwrap();
        let reverse = merge_lots(&[later], &[earlier]).unwrap();
        assert_eq!(forward, reverse);
        let bidders = bidders_for_notice("bosa:w:n", &notice(forward));
        assert_eq!(bidders.len(), 2);
        assert_eq!(bidders[0].outcome, "won");
        assert_eq!(bidders[1].outcome, "not-awarded");
    }

    #[test]
    fn differing_multi_result_rows_are_rejected_without_guessing_correspondence() {
        let first = lot("selec-w", &["45000000"], json!([]));
        let mut second = first.clone();
        second["reported_value"] = json!("100.00");
        let a = [first.clone(), second.clone()];
        assert!(merge_lots(&a, &[second.clone(), first]).is_ok());
        second["linked_tenderers"] = json!([]);
        assert!(merge_lots(&a, &[second]).is_err());
    }

    #[test]
    fn multi_buyer_order_does_not_make_snapshots_conflict() {
        assert_eq!(
            canonical_buyer("B dept / A dept"),
            canonical_buyer("A dept / B dept")
        );
        assert_eq!(canonical_buyer("Single"), "Single");
    }

    #[test]
    fn bidders_mark_winners_and_others_only_when_a_contract_settled() {
        let tenderer = |org: &str, id: &str, contracts: Value| {
            json!({"organization_reference": org, "tendering_party_reference": format!("TPA-{org}"),
                   "identifier": id, "name": org, "contract_references": contracts})
        };
        let mut settled = lot("selec-w", &["45000000"], json!([]));
        settled["linked_tenderers"] = json!([
            tenderer("ORG-1", "BE 0202239951", json!(["CON-1"])),
            tenderer("ORG-2", "0403170701", json!([])),
            tenderer("ORG-3", "NL123456789B01", json!([])),
        ]);
        settled["lowest_tender_value"] = json!("100.00");
        let mut unsettled = lot("selec-w", &["45000000"], json!([]));
        unsettled["lot_id"] = json!("LOT-2");
        unsettled["linked_tenderers"] = json!([tenderer("ORG-4", "0403170701", json!([]))]);
        let bidders = bidders_for_notice("bosa:w:n", &notice(vec![settled, unsettled]));
        assert_eq!(bidders.len(), 3, "unsettled lot must not yield losers");
        let by_org = |org: &str| {
            bidders
                .iter()
                .find(|b| b.organization_reference == org)
                .unwrap()
        };
        assert_eq!(by_org("ORG-1").outcome, "won");
        assert_eq!(
            by_org("ORG-1").enterprise_number.as_deref(),
            Some("0202.239.951")
        );
        assert_eq!(by_org("ORG-2").outcome, "not-awarded");
        assert_eq!(by_org("ORG-3").enterprise_number, None);
        assert!(bidders
            .iter()
            .all(|b| b.lot_tenderers == 3 && b.dossier_id.as_deref() == Some("12345")));
        assert_eq!(
            by_org("ORG-2").lowest_tender_value.as_deref(),
            Some("100.00")
        );
    }

    #[test]
    fn synthetic_award_item_kind_survives_company_history_normalization() {
        let mut evidence = lot(
            "selec-w",
            &["31000000"],
            json!([{
                "organization_reference":"ORG-1",
                "identifier":"0760524243",
                "name":"Cebeo",
                "contract_references":["ECO/2017/019 - ID4165"]
            }]),
        );
        evidence["lot_id"] = json!("AWARD-ITEM-1");
        evidence["lot_id_kind"] = json!("award-item");
        evidence["winner_identifier"] = json!("0760524243");
        evidence["winner_name"] = json!("Cebeo");
        let mut report = BosaAwardHistoryReport::default();
        let records = records_for_notice(
            &publication_key("workspace-uuid", "notice-uuid"),
            &notice(vec![evidence]),
            &mut report,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].1.raw["winning_lots"][0]["lot_id"],
            "AWARD-ITEM-1"
        );
        assert_eq!(
            records[0].1.raw["winning_lots"][0]["lot_id_kind"],
            "award-item"
        );
    }

    #[test]
    fn name_only_award_is_not_company_history_and_is_counted_as_skipped() {
        let mut evidence = lot(
            "selec-w",
            &["31000000"],
            json!([{
                "organization_reference":"",
                "identifier":null,
                "name":"nv Voorbeeld Leverancier",
                "contract_references":["ECO/2017/019 - ID4165"]
            }]),
        );
        evidence["lot_id"] = json!("AWARD-ITEM-1");
        evidence["lot_id_kind"] = json!("award-item");
        evidence["winner_name"] = json!("nv Voorbeeld Leverancier");
        let source_notice = notice(vec![evidence]);
        let mut report = BosaAwardHistoryReport::default();
        let records = records_for_notice(
            &publication_key("workspace-uuid", "notice-uuid"),
            &source_notice,
            &mut report,
        )
        .unwrap();

        assert!(records.is_empty());
        assert_eq!(report.selected_lots, 1);
        assert_eq!(report.skipped_lots_without_usable_supplier_identity, 1);
        assert_eq!(report.skipped_lots_without_contract_link, 0);
        assert_eq!(report.skipped_non_belgian_or_invalid_ids, 0);
    }

    #[test]
    fn multi_supplier_contract_members_get_cpv_only_joint_history() {
        let lots = vec![lot(
            "selec-w",
            &["45210000", "45211000"],
            json!([
                {"organization_reference":"ORG-1","identifier":"BE 0404.484.060","name":"Lead BV","tendering_party_reference":"TPA-1","group_lead":true,"contract_references":["CON-1"]},
                {"organization_reference":"ORG-2","identifier":"0402511693","name":"Partner NV","tendering_party_reference":"TPA-1","group_lead":false,"contract_references":["CON-1"]},
                {"organization_reference":"ORG-3","identifier":"BE9999999999","name":"Unlinked bidder","tendering_party_reference":"TPA-2","group_lead":true,"contract_references":[]}
            ]),
        )];
        let mut report = BosaAwardHistoryReport::default();
        let publication = publication_key("workspace-uuid", "notice-uuid");
        let records = records_for_notice(&publication, &notice(lots), &mut report).unwrap();
        assert_eq!(records.len(), 2);
        for (enterprise, award) in records {
            assert_eq!(award.publication_number, publication);
            assert_eq!(award.values, Vec::<rust_decimal::Decimal>::new());
            assert_eq!(award.cpv_codes, ["45210000", "45211000"]);
            assert_eq!(award.raw["source"], "bosa");
            assert_eq!(award.raw["amounts_used_for_ranking"], false);
            assert_eq!(award.raw["joint_participation"], true);
            assert_eq!(award.raw["winning_lots"][0]["participation"], "joint");
            assert!(award.links[0].contains("workspace-uuid"));
            if enterprise == "0404.484.060" {
                assert_eq!(award.winner_names, ["Lead BV"]);
            } else {
                assert_eq!(enterprise, "0402.511.693");
                assert_eq!(award.winner_names, ["Partner NV"]);
            }
        }
        assert_eq!(report.selected_lots, 1);
        assert_eq!(report.joint_company_lot_participations, 2);
    }

    #[test]
    fn multiple_selected_contract_results_for_one_lot_preserve_each_supplier() {
        let mut ds_units_lot_one = lot(
            "selec-w",
            &["34221000"],
            json!([{
                "organization_reference":"ORG-0004",
                "identifier":"0760524243",
                "name":"DS Units",
                "tendering_party_reference":"TPA-0001",
                "contract_references":["CON-0001"]
            }]),
        );
        ds_units_lot_one["lot_id"] = json!("LOT-0001");
        ds_units_lot_one["winner_identifier"] = json!("0760524243");
        ds_units_lot_one["winner_name"] = json!("DS Units");
        let mut ds_units_lot_two = lot(
            "selec-w",
            &["34221000"],
            json!([{
                "organization_reference":"ORG-0004",
                "identifier":"0760524243",
                "name":"DS Units",
                "tendering_party_reference":"TPA-0001",
                "contract_references":["CON-0002"]
            }]),
        );
        ds_units_lot_two["lot_id"] = json!("LOT-0002");
        ds_units_lot_two["winner_identifier"] = json!("0760524243");
        ds_units_lot_two["winner_name"] = json!("DS Units");
        let mut losberger_lot_two = lot(
            "selec-w",
            &["34221000"],
            json!([{
                "organization_reference":"ORG-0005",
                "identifier":"0788752035",
                "name":"Losberger De Boer Benelux B.V.",
                "tendering_party_reference":"TPA-0002",
                "contract_references":["CON-0003"]
            }]),
        );
        losberger_lot_two["lot_id"] = json!("LOT-0002");
        losberger_lot_two["winner_identifier"] = json!("0788752035");
        losberger_lot_two["winner_name"] = json!("Losberger De Boer Benelux B.V.");
        let lots = vec![ds_units_lot_one, ds_units_lot_two, losberger_lot_two];
        let mut source_notice = notice(lots);
        source_notice.workspace_id = "05d3b51d-e655-4fa2-990d-a8013331dfa0".into();
        source_notice.notice_id = "ab1c718a-25d0-4e61-a5f8-e8cd3e39a85a".into();
        let mut report = BosaAwardHistoryReport::default();
        let publication = publication_key(&source_notice.workspace_id, &source_notice.notice_id);
        let records = records_for_notice(&publication, &source_notice, &mut report).unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(
            report.selected_lots, 2,
            "two lot IDs remain two lots despite three result rows"
        );
        for (enterprise, award) in records {
            assert_eq!(award.publication_number, publication);
            assert_eq!(award.cpv_codes, ["34221000"]);
            assert!(award.values.is_empty());
            match enterprise.as_str() {
                "0760.524.243" => {
                    assert_eq!(award.winner_names, ["DS Units"]);
                    let winning_lots = award.raw["winning_lots"].as_array().unwrap();
                    assert_eq!(winning_lots.len(), 2);
                    assert_eq!(winning_lots[0]["lot_id"], "LOT-0001");
                    assert_eq!(winning_lots[0]["multi_supplier_lot"], false);
                    assert_eq!(winning_lots[0]["contract_references"], json!(["CON-0001"]));
                    assert_eq!(winning_lots[1]["lot_id"], "LOT-0002");
                    assert_eq!(winning_lots[1]["multi_supplier_lot"], true);
                    assert_eq!(winning_lots[1]["contract_references"], json!(["CON-0002"]));
                }
                "0788.752.035" => {
                    assert_eq!(award.winner_names, ["Losberger De Boer Benelux B.V."]);
                    let winning_lots = award.raw["winning_lots"].as_array().unwrap();
                    assert_eq!(winning_lots.len(), 1);
                    assert_eq!(winning_lots[0]["lot_id"], "LOT-0002");
                    assert_eq!(winning_lots[0]["multi_supplier_lot"], true);
                    assert_eq!(winning_lots[0]["contract_references"], json!(["CON-0003"]));
                }
                other => panic!("unexpected supplier {other}"),
            }
        }
    }

    #[test]
    fn parser_unique_winner_is_used_but_bosa_amount_is_never_imported() {
        let mut selected = lot(
            "selec-w",
            &["45210000"],
            json!([{"organization_reference":"ORG-1","identifier":"0404484060","name":"Sole Winner BV","tendering_party_reference":"TPA-1","group_lead":true,"contract_references":["CON-1"]}]),
        );
        selected["winner_identifier"] = json!("0404484060");
        selected["winner_name"] = json!("Sole Winner BV");
        let mut report = BosaAwardHistoryReport::default();
        let records = records_for_notice(
            &publication_key("workspace-uuid", "notice-uuid"),
            &notice(vec![selected]),
            &mut report,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, "0404.484.060");
        assert!(records[0].1.values.is_empty());
        assert_eq!(records[0].1.cpv_codes, ["45210000"]);
        assert_eq!(
            records[0].1.raw["winning_lots"][0]["organization_reference"],
            "ORG-1"
        );
        assert_eq!(
            records[0].1.raw["winning_lots"][0]["contract_references"][0],
            "CON-1"
        );
        assert_eq!(records[0].1.raw["winning_lots"][0]["group_lead"], true);

        // Older parser output may contain only its contract-link-gated scalar.
        let mut legacy = lot("selec-w", &["45210000"], json!([]));
        legacy
            .as_object_mut()
            .unwrap()
            .remove("contract_linked_tenderers");
        legacy["winner_identifier"] = json!("0404484060");
        let legacy = records_for_notice(
            &publication_key("workspace-uuid", "legacy-notice"),
            &notice(vec![legacy]),
            &mut report,
        )
        .unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].0, "0404.484.060");
    }

    #[test]
    fn explicit_contract_links_must_agree_with_a_scalar_winner() {
        let mut selected = lot("selec-w", &["45210000"], json!([]));
        selected["winner_identifier"] = json!("0404484060");
        let mut report = BosaAwardHistoryReport::default();
        assert!(records_for_notice(
            &publication_key("workspace-uuid", "notice-uuid"),
            &notice(vec![selected.clone()]),
            &mut report,
        )
        .is_err());

        selected["contract_linked_tenderers"] = json!([{
            "organization_reference":"ORG-1",
            "identifier":"0402511693",
            "contract_references":["CON-1"]
        }]);
        assert!(records_for_notice(
            &publication_key("workspace-uuid", "notice-uuid"),
            &notice(vec![selected]),
            &mut report,
        )
        .is_err());
    }

    #[test]
    fn non_awards_and_lots_without_cpv_do_not_enter_history() {
        let lots = vec![
            lot(
                "open-nw",
                &["45210000"],
                json!([{"organization_reference":"ORG-1","identifier":"0404484060","contract_references":["CON-1"]}]),
            ),
            lot(
                "selec-w",
                &[],
                json!([{"organization_reference":"ORG-2","identifier":"0404484060","contract_references":["CON-2"]}]),
            ),
        ];
        let mut report = BosaAwardHistoryReport::default();
        let records = records_for_notice(
            &publication_key("workspace-uuid", "notice-uuid"),
            &notice(lots),
            &mut report,
        )
        .unwrap();
        assert!(records.is_empty());
        assert_eq!(report.selected_lots, 1);
        assert_eq!(report.skipped_lots_without_cpv, 1);
    }

    #[test]
    fn supplier_identifiers_require_a_belgian_scheme_and_valid_check_digits() {
        for value in [
            "0404484060",
            "0404.484.060",
            "BE 0404.484.060",
            "TVABE0404484060",
            "404484060",
        ] {
            assert_eq!(
                canonical_belgian_enterprise(value),
                Some("0404.484.060".into()),
                "{value}"
            );
        }
        assert_eq!(
            canonical_belgian_enterprise("TVA BE 0436.672.125"),
            Some("0436.672.125".into())
        );
        assert_eq!(
            canonical_belgian_enterprise("0418403956"),
            Some("0418.403.956".into())
        );
        assert_eq!(
            canonical_belgian_enterprise("548.736.423_d1d5e601-d1a6-4539-aa72-3b674aa7819f"),
            Some("0548.736.423".into())
        );
        assert_eq!(
            canonical_belgian_enterprise("0244195916_f71a5956-a527-456e-b2ea-3485dcf99abe"),
            Some("0244.195.916".into())
        );
        for value in [
            "0123456789",     // ten digits, but a bad KBO checksum
            "123456789",      // nine digits that normalize to the same bad checksum
            "KRS 0000534382", // Polish register identifier
            "0044610 2G3",    // foreign alpha-numeric registration
            "DE150509863",    // foreign country scheme
            "CHE108298884",   // Swiss register identifier
            "BE069783235",    // BE-prefixed but checksum-invalid
            "FR12345678901",
            "2123456789",
            "0000000000",
            "0548736423_not-a-uuid",
            "0548736423_d1d5e601-d1a6-4539-aa72-3b674aa7819",
            "0548736423_d1d5e601-d1a6-4539-aa72-3b674aa7819f_extra",
        ] {
            assert_eq!(canonical_belgian_enterprise(value), None, "{value}");
        }
    }
}
