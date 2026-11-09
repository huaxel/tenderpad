use aanbestedingen_matcher::bosa::{BosaClient, BosaConfig, Environment};
use serde_json::{json, Value};
use std::env;
use std::error::Error;

#[derive(Debug, PartialEq)]
struct Options {
    environment: Environment,
    dossier_id: u64,
    lot_outcomes: bool,
}

fn parse_args<I>(args: I) -> Result<Option<Options>, Box<dyn Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut environment = Environment::Int;
    let mut dossier_id = None;
    let mut lot_outcomes = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--env" => {
                environment = Environment::parse(&args.next().ok_or("--env needs int or prod")?)?
            }
            "--dossier" => {
                let value = args.next().ok_or("--dossier needs a positive integer")?;
                let parsed: u64 = value.parse()?;
                if parsed == 0 {
                    return Err("--dossier must be positive".into());
                }
                dossier_id = Some(parsed);
            }
            "--lot-outcomes" => lot_outcomes = true,
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }
    let dossier_id = dossier_id.ok_or("--dossier is required")?;
    Ok(Some(Options {
        environment,
        dossier_id,
        lot_outcomes,
    }))
}

fn summarize_workspace(
    workspace: &Value,
    include_lot_outcomes: bool,
) -> Result<Value, Box<dyn Error>> {
    let mut versions = Vec::new();
    if let Some(source_versions) = workspace.get("versions").and_then(Value::as_array) {
        for version in source_versions {
            let mut summary = json!({
                "status": version.get("status"),
                "activationDate": version.get("activationDate"),
                "createdAt": version.get("createdAt"),
                "procedureIdentifier": version.pointer("/dossierSnapshot/procedureIdentifier"),
                "noticeId": version.pointer("/notice/id"),
                "publicationId": version.pointer("/notice/publicationId"),
                "noticeSubType": version.pointer("/notice/subType"),
                "noticeVersion": version.pointer("/notice/noticeVersion"),
            });
            if include_lot_outcomes
                && version.get("status").and_then(Value::as_str) == Some("PUBLISHED")
            {
                let xml = version
                    .pointer("/notice/xmlContent")
                    .and_then(Value::as_str)
                    .ok_or("published notice version has no XML content")?;
                let outcomes =
                    aanbestedingen_matcher::award_xml::parse_lot_outcomes(xml.as_bytes())?;
                summary["lotOutcomes"] = serde_json::to_value(outcomes)?;
                let award_lots =
                    aanbestedingen_matcher::bosa::parse_award_lot_evidence(xml.as_bytes())?;
                summary["awardLotEvidence"] = serde_json::to_value(award_lots)?;
            }
            versions.push(summary);
        }
    }

    Ok(json!({
        "id": workspace.get("id"),
        "dossierId": workspace.get("dossierId"),
        "referenceNumber": workspace.get("referenceNumber"),
        "status": workspace.get("status"),
        "noticeType": workspace.get("noticeType"),
        "formType": workspace.get("formType"),
        "version": workspace.get("version"),
        "versions": versions,
    }))
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: bosa-dossier --dossier ID [--env int|prod] [--lot-outcomes]");
        println!("Lists published workspace/version metadata; notice XML is never printed.");
        println!(
            "--lot-outcomes fetches each workspace and extracts lot outcomes plus awardee/reported-amount evidence (maximum 100 workspaces)."
        );
        println!("Requires BOSA_INT_* credentials (default) or BOSA_PR_* with --env prod.");
        return Ok(());
    };
    let mut client = BosaClient::new(BosaConfig::for_environment(options.environment)?);
    let workspaces = client.publication_workspaces_for_dossier(options.dossier_id)?;
    if options.lot_outcomes && workspaces.len() > 100 {
        return Err(format!(
            "dossier has {} workspaces; --lot-outcomes is limited to 100",
            workspaces.len()
        )
        .into());
    }
    let summaries: Vec<Value> = workspaces
        .iter()
        .map(|workspace| {
            if options.lot_outcomes {
                let id = workspace
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("workspace listing entry has no ID")?;
                let detail = client.publication_workspace(id)?;
                summarize_workspace(&detail, true)
            } else {
                summarize_workspace(workspace, false)
            }
        })
        .collect::<Result<_, Box<dyn Error>>>()?;
    let report = json!({
        "dossierId": options.dossier_id,
        "workspaceCount": summaries.len(),
        "includesLotOutcomes": options.lot_outcomes,
        "workspaces": summaries,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn requires_positive_dossier_id_and_defaults_to_int() {
        assert_eq!(
            parse_args(args(&["--dossier", "1709882"])).unwrap(),
            Some(Options {
                environment: Environment::Int,
                dossier_id: 1709882,
                lot_outcomes: false,
            })
        );
        assert!(parse_args(args(&[])).is_err());
        assert!(parse_args(args(&["--dossier", "0"])).is_err());
        assert!(parse_args(args(&["--dossier", "not-a-number"])).is_err());
    }

    #[test]
    fn accepts_production_and_help() {
        let options = parse_args(args(&[
            "--env",
            "prod",
            "--dossier",
            "1709882",
            "--lot-outcomes",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(options.environment, Environment::Prod);
        assert!(options.lot_outcomes);
        assert_eq!(parse_args(args(&["--help"])).unwrap(), None);
    }

    #[test]
    fn summary_omits_payload_fields_and_keeps_notice_provenance() {
        let workspace = json!({
            "id": "workspace-id",
            "dossierId": 1709882,
            "referenceNumber": "pub03",
            "status": "PUBLISHED",
            "noticeType": "can-general",
            "formType": "AWARD",
            "version": 2,
            "versions": [{
                "status": "PUBLISHED",
                "activationDate": "2026-10-27",
                "createdAt": "2026-10-26T13:52:52",
                "notice": {
                    "id": "notice-id",
                    "publicationId": "742331-2026",
                    "subType": "29",
                    "noticeVersion": 1,
                    "xmlContent": "must not be emitted"
                },
                "dossierSnapshot": {"procedureIdentifier": "procedure-id"}
            }, {
                "status": "PUBLISHED",
                "activationDate": "2026-10-28",
                "createdAt": "2026-10-28T12:00:00",
                "notice": {"id": "later-notice-id"},
                "dossierSnapshot": {"procedureIdentifier": "different-procedure-id"}
            }]
        });
        let summary = summarize_workspace(&workspace, false).unwrap();
        assert_eq!(summary["referenceNumber"], "pub03");
        assert_eq!(
            summary["versions"][0]["procedureIdentifier"],
            "procedure-id"
        );
        assert_eq!(
            summary["versions"][1]["procedureIdentifier"],
            "different-procedure-id"
        );
        assert_eq!(summary["versions"][0]["publicationId"], "742331-2026");
        assert!(summary["versions"][0].get("xmlContent").is_none());
    }

    #[test]
    fn lot_outcome_mode_extracts_results_without_emitting_xml() {
        let workspace = json!({
            "id": "workspace-id",
            "referenceNumber": "002",
            "versions": [{
                "status": "PUBLISHED",
                "activationDate": "2026-10-28",
                "createdAt": "2026-10-28T12:00:00",
                "notice": {
                    "id": "notice-id",
                    "xmlContent": "<ContractAwardNotice><LotResult><ID schemeName=\"Lot\">LOT-1</ID><TenderResultCode listName=\"winner-selection-status\">clos-nw</TenderResultCode><DecisionReasonCode>no-rece</DecisionReasonCode></LotResult></ContractAwardNotice>"
                }
            }]
        });
        let summary = summarize_workspace(&workspace, true).unwrap();
        assert_eq!(summary["versions"][0]["lotOutcomes"][0]["lot_id"], "LOT-1");
        assert_eq!(
            summary["versions"][0]["lotOutcomes"][0]["winner_selection_status"],
            "clos-nw"
        );
        assert_eq!(
            summary["versions"][0]["lotOutcomes"][0]["non_award_reason"],
            "no-rece"
        );
        assert_eq!(
            summary["versions"][0]["awardLotEvidence"][0]["lot_id"],
            "LOT-1"
        );
        assert!(summary["versions"][0]["awardLotEvidence"][0]["winner_name"].is_null());
        assert!(summary["versions"][0].get("xmlContent").is_none());
        assert!(summary["versions"][0].get("notice").is_none());
    }
}
