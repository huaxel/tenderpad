use aanbestedingen_matcher::store::{TenderSnapshotRow, TenderStore};
use serde_json::json;
use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_SNAPSHOT_TENDERS: usize = 10_000;
const MAX_SNAPSHOT_PAYLOAD_BYTES: i64 = 128 * 1024 * 1024;
const MAX_SNAPSHOT_ROW_BYTES: i64 = 4 * 1024 * 1024;

#[derive(Debug, PartialEq)]
struct Options {
    database: PathBuf,
    source_database: PathBuf,
}

#[derive(Debug, PartialEq)]
struct ImportReport {
    source_rows: usize,
    source_payload_bytes: i64,
    upsert_attempted: usize,
    destination_before: i64,
    destination_after: i64,
}

fn parse_args<I>(args: I) -> Result<Option<Options>, Box<dyn Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut database = None;
    let mut source_database = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--database" => {
                if database.is_some() {
                    return Err("--database may be supplied only once".into());
                }
                database = Some(PathBuf::from(args.next().ok_or("--database needs a path")?));
            }
            "--source-db" => {
                if source_database.is_some() {
                    return Err("--source-db may be supplied only once per run".into());
                }
                source_database = Some(PathBuf::from(
                    args.next().ok_or("--source-db needs a path")?,
                ));
            }
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }
    let database = database.ok_or("--database is required")?;
    let source_database = source_database.ok_or("--source-db is required")?;
    Ok(Some(Options {
        database,
        source_database,
    }))
}

#[derive(Debug)]
struct SourceSnapshot {
    tenders: Vec<TenderSnapshotRow>,
    payload_bytes: i64,
}

fn load_production_snapshot(path: &Path) -> Result<SourceSnapshot, Box<dyn Error>> {
    if !path.is_file() {
        return Err(format!("BOSA source database does not exist: {}", path.display()).into());
    }
    let source = TenderStore::open_read_only(path)?;
    let total_count = source.count()?;
    let bosa_count = source.count_source("bda")?;
    if total_count != bosa_count {
        return Err(format!(
            "source database contains {} non-BOSA tender rows; use a Production BOSA snapshot only",
            total_count - bosa_count
        )
        .into());
    }
    let count = usize::try_from(bosa_count)?;
    if count == 0 {
        return Err(format!("BOSA source database is empty: {}", path.display()).into());
    }
    if count > MAX_SNAPSHOT_TENDERS {
        return Err(format!(
            "source database has {count} rows; the safe per-source limit is {MAX_SNAPSHOT_TENDERS}"
        )
        .into());
    }
    let (payload_bytes, largest_row_bytes) = source.tender_payload_size_bytes()?;
    if payload_bytes > MAX_SNAPSHOT_PAYLOAD_BYTES || largest_row_bytes > MAX_SNAPSHOT_ROW_BYTES {
        return Err(format!(
            "source tender payload is too large ({payload_bytes} total bytes, largest row {largest_row_bytes}); limits are {MAX_SNAPSHOT_PAYLOAD_BYTES} total and {MAX_SNAPSHOT_ROW_BYTES} per row"
        )
        .into());
    }
    let tenders = source.tenders_with_raw_for_source("bda", MAX_SNAPSHOT_TENDERS)?;
    if tenders.len() != count {
        return Err(format!(
            "source database returned {} of {count} rows; refusing a truncated import",
            tenders.len()
        )
        .into());
    }

    validate_snapshot_rows(&tenders)?;
    Ok(SourceSnapshot {
        tenders,
        payload_bytes,
    })
}

fn resolved_path(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if let Ok(canonical) = fs::canonicalize(path) {
        return Ok(canonical);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file = path.file_name().ok_or("database path has no file name")?;
    Ok(fs::canonicalize(parent)?.join(file))
}

fn import_snapshots(options: &Options) -> Result<ImportReport, Box<dyn Error>> {
    if !options.database.is_file() {
        return Err(format!(
            "destination database must already exist: {}",
            options.database.display()
        )
        .into());
    }
    if !options.source_database.is_file() {
        return Err(format!(
            "BOSA source database does not exist: {}",
            options.source_database.display()
        )
        .into());
    }
    let destination_path = resolved_path(&options.database)?;
    let source_path = resolved_path(&options.source_database)?;
    if source_path == destination_path {
        return Err("destination database must differ from the BOSA source database".into());
    }
    let snapshot = load_production_snapshot(&source_path)?;

    // The full source payload is bounded to one monthly snapshot and validated
    // before the writable destination is opened. The store applies BOSA
    // workspace/folder identity and correction rules in one transaction;
    // unrelated TED/manual awards and user state are retained.
    let destination = TenderStore::open(&options.database)?;
    let destination_before = destination.count()?;
    let upsert_attempted = destination.upsert_snapshot_many(snapshot.tenders.iter())?;
    let destination_after = destination.count()?;
    Ok(ImportReport {
        source_rows: snapshot.tenders.len(),
        source_payload_bytes: snapshot.payload_bytes,
        upsert_attempted,
        destination_before,
        destination_after,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: bosa-snapshot-import --database EXISTING.duckdb --source-db BOSA-PROD-MONTH.duckdb");
        println!("Imports one bounded Production BOSA opportunity snapshot from a local database.");
        println!("Source is read-only; destination must already exist. Existing rows are preserved and BOSA identity/correction rules are applied transactionally. Run once per monthly snapshot.");
        return Ok(());
    };
    let report = import_snapshots(&options)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "source_database": options.source_database.display().to_string(),
            "source_rows": report.source_rows,
            "source_payload_bytes": report.source_payload_bytes,
            "upsert_attempted": report.upsert_attempted,
            "destination_tenders_before": report.destination_before,
            "destination_tenders_after": report.destination_after,
            "note": "Only validated Production BOSA notices from this one snapshot were imported; existing awards, TED notices, and user state were preserved."
        }))?
    );
    Ok(())
}

fn validate_snapshot_rows(tenders: &[TenderSnapshotRow]) -> Result<(), Box<dyn Error>> {
    if tenders.is_empty() {
        return Err("BOSA source database is empty".into());
    }
    let mut workspaces = HashSet::with_capacity(tenders.len());
    for row in tenders {
        let tender = &row.tender;
        if tender.source != "bda" {
            return Err(format!(
                "source database contains non-BOSA tender source {:?}",
                tender.source
            )
            .into());
        }
        let workspace = tender
            .raw
            .get("bosa_publication_workspace_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or("BOSA snapshot row lacks a publication workspace identity")?;
        Uuid::parse_str(workspace)
            .map_err(|_| format!("BOSA snapshot row has an invalid workspace UUID: {workspace}"))?;
        if !workspaces.insert(workspace.to_owned()) {
            return Err(format!(
                "source database repeats BOSA workspace {workspace}; refusing ambiguous snapshot"
            )
            .into());
        }
        let environment = tender
            .raw
            .get("bosa_environment")
            .and_then(serde_json::Value::as_str);
        if environment != Some("prod") {
            return Err(format!(
                "source database contains a BOSA row outside Production ({environment:?})"
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aanbestedingen_matcher::store::TenderStore;
    use aanbestedingen_matcher::Tender;
    use serde_json::json;

    fn args(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn production_bosa_tender(workspace: &str, folder: &str) -> Tender {
        Tender {
            source: "bda".to_owned(),
            external_id: folder.to_owned(),
            title: "Production BOSA contract".to_owned(),
            raw: json!({
                "bosa_publication_workspace_id": workspace,
                "bosa_notice_id": "notice-1",
                "bosa_environment": "prod",
                "contract_folder_id": folder,
            }),
            ..Tender::default()
        }
    }

    fn unique_dir() -> PathBuf {
        let path = env::temp_dir().join(format!(
            "bosa-snapshot-import-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn stored_fetched_at(path: &Path) -> String {
        let connection = duckdb::Connection::open(path).unwrap();
        connection
            .query_row(
                "SELECT CAST(fetched_at AS VARCHAR) FROM tenders WHERE source = 'bda' AND external_id = 'folder-1'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn requires_destination_and_exactly_one_source_argument() {
        assert!(parse_args(args(&[])).is_err());
        assert!(parse_args(args(&["--database", "out.duckdb"])).is_err());
        let options = parse_args(args(&[
            "--database",
            "out.duckdb",
            "--source-db",
            "october.duckdb",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(options.database, PathBuf::from("out.duckdb"));
        assert_eq!(options.source_database, PathBuf::from("october.duckdb"));
        assert!(parse_args(args(&[
            "--database",
            "out.duckdb",
            "--source-db",
            "one.duckdb",
            "--source-db",
            "two.duckdb",
        ]))
        .is_err());
        assert!(parse_args(args(&["--help"])).unwrap().is_none());
    }

    #[test]
    fn imports_a_production_snapshot_idempotently_and_preserves_other_sources() {
        let dir = unique_dir();
        let source_path = dir.join("source.duckdb");
        let destination_path = dir.join("destination.duckdb");
        let workspace = Uuid::new_v4().to_string();
        {
            let source = TenderStore::open(&source_path).unwrap();
            source
                .upsert(&production_bosa_tender(&workspace, "folder-1"))
                .unwrap();
            let destination = TenderStore::open(&destination_path).unwrap();
            destination
                .upsert(&Tender {
                    source: "ted".to_owned(),
                    external_id: "ted-1".to_owned(),
                    title: "Existing TED notice".to_owned(),
                    ..Tender::default()
                })
                .unwrap();
        }
        {
            let source = duckdb::Connection::open(&source_path).unwrap();
            source
                .execute(
                    "UPDATE tenders SET fetched_at = CAST(? AS TIMESTAMPTZ) WHERE source = 'bda' AND external_id = 'folder-1'",
                    duckdb::params!["2020-01-02 03:04:05+00"],
                )
                .unwrap();
        }
        let source_fetched_at = stored_fetched_at(&source_path);
        let options = Options {
            database: destination_path.clone(),
            source_database: source_path.clone(),
        };
        let first = import_snapshots(&options).unwrap();
        assert_eq!(first.source_rows, 1);
        assert!(first.source_payload_bytes > 0);
        assert_eq!(first.destination_before, 1);
        assert_eq!(first.destination_after, 2);
        assert_eq!(stored_fetched_at(&destination_path), source_fetched_at);
        let second = import_snapshots(&options).unwrap();
        assert_eq!(second.destination_before, 2);
        assert_eq!(second.destination_after, 2);
        assert_eq!(stored_fetched_at(&destination_path), source_fetched_at);
        {
            let destination = TenderStore::open_read_only(&destination_path).unwrap();
            let tenders = destination.tenders(10).unwrap();
            assert_eq!(tenders.len(), 2);
            assert!(tenders.iter().any(|tender| tender.source == "ted"));
            let imported = destination.tender("bda", "folder-1").unwrap().unwrap();
            assert_eq!(imported.raw["bosa_publication_workspace_id"], workspace);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_a_mixed_source_database_before_import() {
        let dir = unique_dir();
        let source_path = dir.join("mixed.duckdb");
        {
            let source = TenderStore::open(&source_path).unwrap();
            source
                .upsert(&production_bosa_tender(
                    &Uuid::new_v4().to_string(),
                    "folder-1",
                ))
                .unwrap();
            source
                .upsert(&Tender {
                    source: "ted".to_owned(),
                    external_id: "ted-1".to_owned(),
                    ..Tender::default()
                })
                .unwrap();
        }
        let error = load_production_snapshot(&source_path).unwrap_err();
        assert!(error.to_string().contains("non-BOSA tender rows"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_an_oversized_row_during_preflight() {
        let dir = unique_dir();
        let source_path = dir.join("oversized.duckdb");
        {
            let source = TenderStore::open(&source_path).unwrap();
            let mut tender = production_bosa_tender(&Uuid::new_v4().to_string(), "folder-1");
            tender.description = "x".repeat(MAX_SNAPSHOT_ROW_BYTES as usize + 1);
            source.upsert(&tender).unwrap();
        }
        let error = load_production_snapshot(&source_path).unwrap_err();
        assert!(error
            .to_string()
            .contains("source tender payload is too large"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_nonproduction_and_nonbosa_source_rows_before_writing() {
        let int_tender = Tender {
            raw: json!({
                "bosa_publication_workspace_id": Uuid::new_v4().to_string(),
                "bosa_environment": "int",
            }),
            ..production_bosa_tender(&Uuid::new_v4().to_string(), "int-folder")
        };
        let wrong_source = Tender {
            source: "ted".to_owned(),
            ..production_bosa_tender(&Uuid::new_v4().to_string(), "ted-folder")
        };
        for tender in [int_tender, wrong_source] {
            let store = TenderStore::open(":memory:").unwrap();
            store.upsert(&tender).unwrap();
            let rows = store
                .tenders(10)
                .unwrap()
                .into_iter()
                .map(|tender| TenderSnapshotRow {
                    tender,
                    fetched_at: "2026-10-30 20:55:52+01".to_owned(),
                })
                .collect::<Vec<_>>();
            assert!(validate_snapshot_rows(&rows).is_err());
        }
    }
}
