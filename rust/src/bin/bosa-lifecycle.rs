use aanbestedingen_matcher::bosa::Environment;
use aanbestedingen_matcher::store::TenderStore;
use aanbestedingen_matcher::Tender;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

struct Options {
    database: PathBuf,
    folder: String,
    environment: Environment,
}
fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, Box<dyn Error>> {
    let mut database = None;
    let mut folder = None;
    let mut environment = Environment::Prod;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--database" => database = Some(PathBuf::from(value)),
            "--folder" => folder = Some(value),
            "--env" => environment = Environment::parse(&value)?,
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let folder = folder
        .filter(|s: &String| !s.trim().is_empty())
        .ok_or("--folder is required")?;
    Ok(Some(Options {
        database: database.ok_or("--database is required")?,
        folder,
        environment,
    }))
}
fn lot_outcomes(tender: &Tender) -> Option<BTreeMap<String, Value>> {
    let outcomes = tender.raw.get("bosa_lot_outcomes")?.as_array()?;
    let mut by_lot = BTreeMap::new();
    for outcome in outcomes {
        let Some(lot_id) = outcome.get("lot_id").and_then(Value::as_str) else {
            continue;
        };
        by_lot.insert(
            lot_id.to_owned(),
            json!({
                "winner_selection_status": outcome.get("winner_selection_status"),
                "non_award_reason": outcome.get("non_award_reason"),
            }),
        );
    }
    Some(by_lot)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum WorkspaceReferenceKind {
    Numeric,
    Publication,
    Other,
}

fn workspace_reference_number(value: &str) -> Option<(WorkspaceReferenceKind, u32)> {
    value
        .parse::<u32>()
        .ok()
        .map(|number| (WorkspaceReferenceKind::Numeric, number))
        .or_else(|| {
            value
                .strip_prefix("pub")
                .and_then(|number| number.parse().ok())
                .map(|number| (WorkspaceReferenceKind::Publication, number))
        })
}

/// Compare numbered records from the same BOSA dossier without inferring precedence.
fn sequence_comparisons(notices: &[Tender]) -> Vec<Value> {
    let mut groups: BTreeMap<(String, String), Vec<&Tender>> = BTreeMap::new();
    for tender in notices {
        let (Some(dossier_id), Some(dossier_reference), Some(_workspace_reference)) = (
            tender.raw.get("bosa_dossier_id"),
            tender
                .raw
                .get("bosa_dossier_reference_number")
                .and_then(Value::as_str),
            tender
                .raw
                .get("bosa_workspace_reference_number")
                .and_then(Value::as_str)
                .and_then(workspace_reference_number),
        ) else {
            continue;
        };
        if tender
            .raw
            .get("bosa_lot_outcomes")
            .and_then(Value::as_array)
            .is_none()
        {
            continue;
        }
        groups
            .entry((dossier_id.to_string(), dossier_reference.to_owned()))
            .or_default()
            .push(tender);
    }

    let mut comparisons = Vec::new();
    for ((_dossier_id_key, dossier_reference), mut records) in groups {
        records.sort_by_key(|tender| {
            tender
                .raw
                .get("bosa_workspace_reference_number")
                .and_then(Value::as_str)
                .and_then(workspace_reference_number)
                .unwrap_or((WorkspaceReferenceKind::Other, u32::MAX))
        });
        for pair in records.windows(2) {
            let lower = pair[0];
            let higher = pair[1];
            let lower_ref = lower.raw["bosa_workspace_reference_number"]
                .as_str()
                .unwrap_or_default();
            let higher_ref = higher.raw["bosa_workspace_reference_number"]
                .as_str()
                .unwrap_or_default();
            let (Some((lower_kind, lower_number)), Some((higher_kind, higher_number))) = (
                workspace_reference_number(lower_ref),
                workspace_reference_number(higher_ref),
            ) else {
                continue;
            };
            if lower_kind != higher_kind || lower_number >= higher_number {
                continue;
            }
            let (Some(lower_outcomes), Some(higher_outcomes)) =
                (lot_outcomes(lower), lot_outcomes(higher))
            else {
                continue;
            };
            let lot_ids: std::collections::BTreeSet<_> = lower_outcomes
                .keys()
                .chain(higher_outcomes.keys())
                .cloned()
                .collect();
            let differences: Vec<_> = lot_ids
                .into_iter()
                .filter_map(|lot_id| {
                    let lower_value = lower_outcomes.get(&lot_id);
                    let higher_value = higher_outcomes.get(&lot_id);
                    (lower_value != higher_value).then(|| {
                        json!({
                            "lot_id": lot_id,
                            "lower_reference_outcome": lower_value,
                            "higher_reference_outcome": higher_value,
                        })
                    })
                })
                .collect();
            comparisons.push(json!({
                "dossier_id": lower.raw.get("bosa_dossier_id"),
                "dossier_reference": dossier_reference,
                "lower_workspace_reference": lower_ref,
                "higher_workspace_reference": higher_ref,
                "lower_search_reference": lower.raw.get("bosa_search_reference_number"),
                "higher_search_reference": higher.raw.get("bosa_search_reference_number"),
                "lower_activation_date": lower.raw.get("bosa_activation_date"),
                "higher_activation_date": higher.raw.get("bosa_activation_date"),
                "lower_version_created_at": lower.raw.get("bosa_version_created_at"),
                "higher_version_created_at": higher.raw.get("bosa_version_created_at"),
                "lot_outcome_differences": differences,
                "interpretation": "TED guidance permits a later Result notice to report a decision for a previously ongoing lot. This comparison is per-lot evidence only: a higher workspace reference does not by itself establish chronology or wholesale supersession; retain both notices.",
            }));
        }
    }
    comparisons
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse(std::env::args().skip(1))? else {
        println!("Usage: bosa-lifecycle --database EXISTING --folder CONTRACT_FOLDER_ID [--env prod|int]");
        println!("Read-only evidence from stored BOSA workspaces; absence is not proof of an open or closed procurement.");
        return Ok(());
    };
    let store = TenderStore::open_read_only(&options.database)?;
    let notices = store.bosa_folder_notices(options.environment.source(), &options.folder)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "folder": options.folder,
            "source": options.environment.source(),
            "notice_count": notices.len(),
            "interpretation": "Evidence only: storage may be incomplete; lot codes do not automatically close a call or establish eligibility.",
        "code_legend": {
            "selec-w": "At least one winner was chosen",
            "clos-nw": "No winner was chosen and competition is closed",
            "open-nw": "Winner not yet chosen; competition is still ongoing",
            "source": "https://docs.ted.europa.eu/eforms/latest/reference/code-lists/winner-selection-status.html"
        },
            "sequence_comparisons": sequence_comparisons(&notices),
            "notices": notices.iter().map(|t| json!({
                "external_id": t.external_id, "title": t.title, "notice_type": t.notice_type,
                "publication_date": t.publication_date, "deadline": t.deadline,
                "workspace": t.raw.get("bosa_publication_workspace_id"), "notice_id": t.raw.get("bosa_notice_id"),
                "dossier_id": t.raw.get("bosa_dossier_id"),
                "dossier_reference": t.raw.get("bosa_dossier_reference_number"),
                "workspace_reference": t.raw.get("bosa_workspace_reference_number"),
                "workspace_version": t.raw.get("bosa_workspace_version"),
                "activation_date": t.raw.get("bosa_activation_date"),
                "version_created_at": t.raw.get("bosa_version_created_at"),
                "search_reference": t.raw.get("bosa_search_reference_number"),
                "bda_publication_references": t.raw.get("bosa_publication_reference_numbers_bda"),
                "procedure_type": t.raw.get("bosa_procedure_type"),
                "publication_status": t.raw.get("bosa_publication_type"), "lot_outcomes": t.raw.get("bosa_lot_outcomes"),
            })).collect::<Vec<_>>()
        }))?
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sequence_comparison_reports_lot_changes_without_claiming_precedence() {
        let notice = |reference: &str, status: &str| Tender {
            raw: json!({
                "bosa_dossier_id": 1718132,
                "bosa_dossier_reference_number": "PPP/123",
                "bosa_workspace_reference_number": reference,
                "bosa_search_reference_number": format!("PPP/123_{reference}"),
                "bosa_activation_date": "2026-10-30",
                "bosa_version_created_at": if reference == "002" {
                    "2026-10-30T13:24:43.000000"
                } else {
                    "2026-10-30T13:26:24.000000"
                },
                "bosa_lot_outcomes": [{
                    "lot_id": "LOT-0003",
                    "winner_selection_status": status,
                    "non_award_reason": if status == "clos-nw" { json!("other") } else { Value::Null },
                }]
            }),
            ..Tender::default()
        };
        let lower = notice("002", "open-nw");
        let higher = notice("003", "clos-nw");
        let reports = sequence_comparisons(&[higher, lower]);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0]["lower_workspace_reference"], "002");
        assert_eq!(reports[0]["higher_workspace_reference"], "003");
        assert_eq!(reports[0]["lower_activation_date"], "2026-10-30");
        assert_eq!(reports[0]["higher_activation_date"], "2026-10-30");
        assert_eq!(
            reports[0]["lower_version_created_at"],
            "2026-10-30T13:24:43.000000"
        );
        assert_eq!(
            reports[0]["higher_version_created_at"],
            "2026-10-30T13:26:24.000000"
        );
        assert_eq!(
            reports[0]["lot_outcome_differences"][0]["lot_id"],
            "LOT-0003"
        );
        let interpretation = reports[0]["interpretation"].as_str().unwrap();
        assert!(interpretation.contains("previously ongoing lot"));
        assert!(interpretation.contains("does not by itself establish chronology"));
        assert!(interpretation.contains("retain both notices"));
    }

    #[test]
    fn sequence_comparison_supports_pub_numbered_references_only() {
        let notice = |reference: &str, status: &str| Tender {
            raw: json!({
                "bosa_dossier_id": 42,
                "bosa_dossier_reference_number": "D-42",
                "bosa_workspace_reference_number": reference,
                "bosa_lot_outcomes": [{
                    "lot_id": "LOT-1",
                    "winner_selection_status": status,
                    "non_award_reason": Value::Null,
                }]
            }),
            ..Tender::default()
        };
        let reports = sequence_comparisons(&[
            notice("pub03", "clos-nw"),
            notice("pub02", "open-nw"),
            notice("003", "selec-w"),
            notice("002", "selec-w"),
            notice("publication, attribution, lot 1", "open-nw"),
        ]);
        assert_eq!(reports.len(), 2);
        let publication_pair = reports
            .iter()
            .find(|report| report["lower_workspace_reference"] == "pub02")
            .unwrap();
        assert_eq!(publication_pair["higher_workspace_reference"], "pub03");
        assert_eq!(
            publication_pair["lot_outcome_differences"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let numeric_pair = reports
            .iter()
            .find(|report| report["lower_workspace_reference"] == "002")
            .unwrap();
        assert_eq!(numeric_pair["higher_workspace_reference"], "003");
        assert!(numeric_pair["lot_outcome_differences"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            workspace_reference_number("002"),
            Some((WorkspaceReferenceKind::Numeric, 2))
        );
        assert_eq!(
            workspace_reference_number("pub02"),
            Some((WorkspaceReferenceKind::Publication, 2))
        );
        assert_eq!(
            workspace_reference_number("publication, attribution, lot 1"),
            None
        );
        assert_eq!(workspace_reference_number("avenant 105"), None);
    }

    #[test]
    fn cli_requires_read_only_target_and_folder() {
        assert!(parse(Vec::<String>::new()).is_err());
        assert!(parse(
            ["--database", "x", "--folder", ""]
                .into_iter()
                .map(String::from)
        )
        .is_err());
        let o = parse(
            ["--database", "x", "--folder", "folder"]
                .into_iter()
                .map(String::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.environment, Environment::Prod);
        assert!(parse(["--help"].into_iter().map(String::from))
            .unwrap()
            .is_none());
    }
}
