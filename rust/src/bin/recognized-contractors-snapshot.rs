use aanbestedingen_matcher::kbo::normalize_enterprise_number;
use aanbestedingen_matcher::kbo_live::{LiveClient, LiveCredentials, LiveEnterpriseLookup};
use aanbestedingen_matcher::recognized_contractors::{
    build_snapshot, KboMatch, RecognitionSnapshot,
};
use std::collections::BTreeSet;
use std::env;
use std::io::{Error, ErrorKind, Write};
use std::path::{Path, PathBuf};

fn same_enterprise_number(reported: &str, requested: &str) -> bool {
    match (
        normalize_enterprise_number(reported),
        normalize_enterprise_number(requested),
    ) {
        (Ok(reported), Ok(requested)) => reported == requested,
        _ => false,
    }
}

fn verification_ids(
    snapshot: &RecognitionSnapshot,
) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let ids: BTreeSet<String> = snapshot
        .approval_rows
        .iter()
        .filter(|row| row.kbo_match == KboMatch::UnmatchedLocalSnapshot)
        .filter_map(|row| row.enterprise_number_normalized.clone())
        .collect();
    if ids.len() > 100 {
        return Err(format!(
            "refusing {} live KBO requests; the per-run safety limit is 100",
            ids.len()
        )
        .into());
    }
    Ok(ids)
}

fn verify_unmatched(snapshot: &mut RecognitionSnapshot) -> Result<(), Box<dyn std::error::Error>> {
    let ids = verification_ids(snapshot)?;
    eprintln!(
        "checking {} locally unmatched enterprise IDs against live KBO",
        ids.len()
    );
    let client = LiveClient::new(LiveCredentials::from_env()?);
    verify_batch(snapshot, ids, |id| {
        client.read_enterprise_with_raw_number(id)
    })
}

fn verify_batch<F>(
    snapshot: &mut RecognitionSnapshot,
    ids: BTreeSet<String>,
    mut lookup: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnMut(&str) -> Result<LiveEnterpriseLookup, Box<dyn std::error::Error>>,
{
    let mut outcomes = std::collections::BTreeMap::new();
    let mut exact = 0;
    let mut not_found = 0;
    let mut different_id = 0;
    let mut balance = None;
    let total = ids.len();
    // Apply outcomes only after the entire plan completes. An incomplete batch
    // must neither change the snapshot nor reach normal publication.
    for (index, id) in ids.into_iter().enumerate() {
        let result = lookup(&id);
        let verified_at_utc = chrono::Utc::now().to_rfc3339();
        let outcome = match result {
            Ok(result) => {
                balance = result.remaining_requests.or(balance);
                match result.profile {
                    Some(profile) => {
                        let reported_number = result.reported_enterprise_number.unwrap_or_default();
                        let returned_digits: String = reported_number
                            .chars()
                            .filter(char::is_ascii_digit)
                            .collect();
                        if same_enterprise_number(&reported_number, &id) {
                            exact += 1;
                            KboMatch::ExactLiveApi {
                                verified_at_utc,
                                current_status: profile.status,
                            }
                        } else {
                            different_id += 1;
                            let returned = if returned_digits.len() == 10 {
                                format!(
                                    "{}.{}.{}",
                                    &returned_digits[..4],
                                    &returned_digits[4..7],
                                    &returned_digits[7..]
                                )
                            } else if reported_number.is_empty() {
                                "unreported by KBO".to_owned()
                            } else {
                                reported_number
                            };
                            KboMatch::LiveDifferentId {
                                verified_at_utc,
                                returned_enterprise_number: returned,
                            }
                        }
                    }
                    None => {
                        not_found += 1;
                        KboMatch::LiveNotFound { verified_at_utc }
                    }
                }
            }
            Err(error) => {
                return Err(format!(
                    "live KBO verification stopped for {id}: {error}; batch incomplete"
                )
                .into());
            }
        };
        outcomes.insert(id, outcome);
        if index + 1 < total && balance.is_some_and(|left| left <= 0) {
            return Err(format!(
                "KBO reported exhausted request balance; stopping before {} remaining ID(s); batch incomplete",
                total - index - 1
            )
            .into());
        }
    }
    for row in &mut snapshot.approval_rows {
        if let Some(id) = row.enterprise_number_normalized.as_ref() {
            if let Some(outcome) = outcomes.get(id) {
                row.kbo_match = outcome.clone();
            }
        }
    }
    let exact_ids: BTreeSet<_> = snapshot
        .approval_rows
        .iter()
        .filter(|row| row.kbo_match.is_exact())
        .filter_map(|row| row.enterprise_number_normalized.as_ref())
        .collect();
    let unmatched_ids: BTreeSet<_> = snapshot
        .approval_rows
        .iter()
        .filter(|row| {
            matches!(
                &row.kbo_match,
                KboMatch::UnmatchedLocalSnapshot
                    | KboMatch::LiveNotFound { .. }
                    | KboMatch::LiveDifferentId { .. }
                    | KboMatch::LiveLookupFailed { .. }
            )
        })
        .filter_map(|row| row.enterprise_number_normalized.as_ref())
        .collect();
    snapshot.report.distinct_ids_exactly_matched = exact_ids.len();
    snapshot.report.distinct_ids_unmatched = unmatched_ids.len();
    snapshot.report.rows_exactly_matched = snapshot
        .approval_rows
        .iter()
        .filter(|row| row.kbo_match.is_exact())
        .count();
    eprintln!(
        "live KBO verification: {exact} exact IDs, {not_found} not found, {different_id} different ID, 0 failed{}",
        balance.map(|n| format!("; {n} requests left today")).unwrap_or_default()
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SnapshotOptions {
    pub(crate) input: PathBuf,
    pub(crate) kbo_database: PathBuf,
    pub(crate) kbo_snapshot_as_of: String,
    pub(crate) register_as_of: chrono::NaiveDate,
    pub(crate) output: Option<PathBuf>,
    pub(crate) verify_unmatched_live: bool,
}

pub(crate) fn parse_args<I>(args: I) -> Result<Option<SnapshotOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut input = None;
    let mut kbo_database = PathBuf::from("data/kbo/kbo_enrich.duckdb");
    let mut kbo_snapshot_as_of = None;
    let mut register_as_of = None;
    let mut output = None;
    let mut verify_unmatched_live = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kbo-database" => {
                kbo_database = args.next().ok_or("--kbo-database needs a path")?.into()
            }
            "--kbo-snapshot-as-of" => {
                kbo_snapshot_as_of = Some(args.next().ok_or("--kbo-snapshot-as-of needs a date")?)
            }
            "--register-as-of" => {
                register_as_of = Some(args.next().ok_or("--register-as-of needs a date")?)
            }
            "--output" => output = Some(PathBuf::from(args.next().ok_or("--output needs a path")?)),
            "--verify-unmatched-live" => verify_unmatched_live = true,
            "--help" | "-h" => {
                println!("usage: recognized-contractors-snapshot <export.xlsx> --kbo-snapshot-as-of YYYY-MM-DD --register-as-of YYYY-MM-DD [--kbo-database PATH] [--verify-unmatched-live] [--output PATH]");
                println!("  --verify-unmatched-live checks only local-snapshot misses; requires KBO_PUB_USER/KBO_PUB_PASSWORD (maximum 100 requests)");
                println!("  --verify-unmatched-live stops on any lookup error or exhausted balance with work remaining; incomplete batches publish nothing");
                println!("  --output requires a new path; existing files, directories and links are refused");
                return Ok(None);
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown option: {value}").into())
            }
            value if input.is_none() => input = Some(PathBuf::from(value)),
            value => return Err(format!("unexpected argument: {value}").into()),
        }
    }
    let input = input.ok_or("an input XLSX export is required")?;
    let snapshot_date = kbo_snapshot_as_of
        .ok_or("--kbo-snapshot-as-of is required; do not imply an unknown KBO snapshot date")?;
    chrono::NaiveDate::parse_from_str(&snapshot_date, "%Y-%m-%d")
        .map_err(|e| format!("--kbo-snapshot-as-of must be YYYY-MM-DD: {e}"))?;
    let register_as_of =
        register_as_of.ok_or("--register-as-of is required for date-based approval status")?;
    let register_as_of = chrono::NaiveDate::parse_from_str(&register_as_of, "%Y-%m-%d")
        .map_err(|e| format!("--register-as-of must be YYYY-MM-DD: {e}"))?;
    Ok(Some(SnapshotOptions {
        input,
        kbo_database,
        kbo_snapshot_as_of: snapshot_date,
        register_as_of,
        output,
        verify_unmatched_live,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        return Ok(());
    };
    run(options)
}

fn ensure_new_output(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Unlike exists(), this also refuses dangling symlinks before acquisition.
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "output {} already exists; choose a new path",
                path.display()
            ),
        )
        .into()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn publish_snapshot(path: &Path, json: &str) -> Result<(), Box<dyn std::error::Error>> {
    // This exclusive open, not the preflight, protects against paths appearing
    // later. Never truncate or follow an existing output, and never remove a
    // failed output by name: retain its bytes for inspection instead.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            Error::new(
                error.kind(),
                format!("cannot create new output {}: {error}", path.display()),
            )
        })?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn run(options: SnapshotOptions) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = options.output.as_deref() {
        ensure_new_output(path)?;
    }
    let snapshot = build_snapshot(
        &options.input,
        &options.kbo_database,
        &options.kbo_snapshot_as_of,
        options.register_as_of,
    )?;
    finish_snapshot(snapshot, options, verify_unmatched)
}

fn finish_snapshot<F>(
    mut snapshot: RecognitionSnapshot,
    options: SnapshotOptions,
    verify: F,
) -> Result<(), Box<dyn std::error::Error>>
where
    F: FnOnce(&mut RecognitionSnapshot) -> Result<(), Box<dyn std::error::Error>>,
{
    if options.verify_unmatched_live {
        verify(&mut snapshot)?;
    }
    let json = serde_json::to_string_pretty(&snapshot)?;
    if let Some(path) = options.output {
        publish_snapshot(&path, &json)?;
        eprintln!("wrote snapshot to {}", path.display());
    } else {
        println!("{json}");
    }
    eprintln!("join report: {} approval rows; {} joinable IDs; {} invalid-format IDs; {} exact matches; {} unmatched IDs; {} missing-ID rows; {} invalid-ID rows; {} repeated rows",
        snapshot.report.approval_rows,
        snapshot.report.distinct_ids_joinable,
        snapshot.report.distinct_ids_invalid_format,
        snapshot.report.distinct_ids_exactly_matched,
        snapshot.report.distinct_ids_unmatched,
        snapshot.report.rows_missing_identifier,
        snapshot.report.rows_with_invalid_identifier,
        snapshot.report.repeated_enterprise_rows,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verification_snapshot(ids: &[usize]) -> RecognitionSnapshot {
        let rows: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                serde_json::json!({
                    "source_row": index + 2,
                    "validity_as_of": "unknown_decision_date",
                    "enterprise_number_raw": format!("0000.000.{id:03}"),
                    "enterprise_number_normalized": format!("0000.000.{id:03}"),
                    "category_class_parse_status": "missing",
                    "recognition_credentials": [],
                    "kbo_match": {"status": "unmatched_local_snapshot"}
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "schema_version": 3, "generated_at_utc": "synthetic",
            "source_file": "synthetic.xlsx", "source_sha256": "synthetic",
            "source_worksheet": "Audit", "kbo_database": "unused.duckdb",
            "kbo_snapshot_as_of": "2026-11-06", "register_as_of": "2026-11-06",
            "approval_rows": rows,
            "report": aanbestedingen_matcher::recognized_contractors::JoinReport::default()
        }))
        .unwrap()
    }

    fn verification_reply(id: &str, balance: Option<i64>, found: bool) -> LiveEnterpriseLookup {
        LiveEnterpriseLookup {
            profile: found.then(|| {
                serde_json::from_value(serde_json::json!({
                    "enterprise_number": id, "status": "AC"
                }))
                .unwrap()
            }),
            reported_enterprise_number: found.then(|| id.to_owned()),
            remaining_requests: balance,
        }
    }

    #[test]
    fn verification_any_error_stops_without_applying_partial_results() {
        for error in [
            "authentication",
            "quota",
            "transport",
            "invalid XML",
            "local guard",
        ] {
            for fail_at in [1, 2] {
                let mut snapshot = verification_snapshot(&[1, 2, 3]);
                let before = serde_json::to_value(&snapshot).unwrap();
                let ids = verification_ids(&snapshot).unwrap();
                let mut calls = 0;
                let result = verify_batch(&mut snapshot, ids, |id| {
                    calls += 1;
                    if calls == fail_at {
                        Err(error.into())
                    } else {
                        Ok(verification_reply(id, Some(20), true))
                    }
                });
                assert!(result.is_err(), "{error} at {fail_at}");
                assert_eq!(calls, fail_at, "{error}");
                assert_eq!(serde_json::to_value(&snapshot).unwrap(), before);
            }
        }
    }

    #[test]
    fn verification_exhausted_balance_stops_before_next_id() {
        for balance in [0, -1] {
            for found in [false, true] {
                let mut snapshot = verification_snapshot(&[1, 2, 3]);
                let before = serde_json::to_value(&snapshot).unwrap();
                let ids = verification_ids(&snapshot).unwrap();
                let mut calls = 0;
                let result = verify_batch(&mut snapshot, ids, |id| {
                    calls += 1;
                    Ok(verification_reply(id, Some(balance), found))
                });
                assert!(result.is_err());
                assert_eq!(calls, 1);
                assert_eq!(serde_json::to_value(&snapshot).unwrap(), before);
            }
        }
    }

    #[test]
    fn verification_final_zero_balance_is_complete_and_clean_absence_is_valid() {
        let mut snapshot = verification_snapshot(&[1, 2]);
        let ids = verification_ids(&snapshot).unwrap();
        let mut calls = 0;
        verify_batch(&mut snapshot, ids, |id| {
            calls += 1;
            Ok(verification_reply(
                id,
                Some(if calls == 1 { 1 } else { 0 }),
                calls == 1,
            ))
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(snapshot.report.distinct_ids_exactly_matched, 1);
        assert_eq!(snapshot.report.distinct_ids_unmatched, 1);
        assert!(matches!(
            snapshot.approval_rows[1].kbo_match,
            KboMatch::LiveNotFound { .. }
        ));
    }

    #[test]
    fn verification_unknown_balance_does_not_invent_exhaustion() {
        let mut snapshot = verification_snapshot(&[1, 2, 3]);
        let ids = verification_ids(&snapshot).unwrap();
        let mut calls = 0;
        verify_batch(&mut snapshot, ids, |id| {
            calls += 1;
            Ok(verification_reply(id, None, false))
        })
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(snapshot.report.distinct_ids_unmatched, 3);
    }

    #[test]
    fn verification_sorts_deduplicates_and_keeps_local_matches() {
        let mut snapshot = verification_snapshot(&[3, 1, 2, 1, 4]);
        snapshot.approval_rows[4].kbo_match = KboMatch::ExactLocalSnapshot;
        let ids = verification_ids(&snapshot).unwrap();
        let mut calls = Vec::new();
        verify_batch(&mut snapshot, ids, |id| {
            calls.push(id.to_owned());
            Ok(verification_reply(id, Some(20), !id.ends_with("002")))
        })
        .unwrap();
        assert_eq!(calls, ["0000.000.001", "0000.000.002", "0000.000.003"]);
        assert_eq!(snapshot.report.distinct_ids_exactly_matched, 3);
        assert_eq!(snapshot.report.rows_exactly_matched, 4);
        assert_eq!(snapshot.report.distinct_ids_unmatched, 1);
        assert_eq!(
            snapshot.approval_rows[4].kbo_match,
            KboMatch::ExactLocalSnapshot
        );
    }

    #[test]
    fn verification_admission_preserves_the_100_id_limit() {
        let mut ids: Vec<_> = (1..=100).collect();
        ids.push(1);
        assert_eq!(
            verification_ids(&verification_snapshot(&ids))
                .unwrap()
                .len(),
            100
        );
        ids.push(101);
        assert!(verification_ids(&verification_snapshot(&ids)).is_err());
    }

    #[test]
    fn verification_incomplete_batch_never_creates_normal_output() {
        for failure in ["lookup error", "balance exhausted"] {
            let fixture = OutputFixture::new();
            let output = fixture.path("new.json");
            let snapshot = verification_snapshot(&[1, 2, 3]);
            let mut calls = 0;
            let result = finish_snapshot(snapshot, fixture.options(output.clone()), |snapshot| {
                let ids = verification_ids(snapshot)?;
                verify_batch(snapshot, ids, |id| {
                    calls += 1;
                    if failure == "lookup error" && calls == 2 {
                        Err("quota fault".into())
                    } else {
                        Ok(verification_reply(
                            id,
                            Some(if failure == "balance exhausted" {
                                0
                            } else {
                                20
                            }),
                            true,
                        ))
                    }
                })
            });
            assert!(result.is_err());
            assert_eq!(calls, if failure == "lookup error" { 2 } else { 1 });
            assert!(!output.exists());
        }
    }

    #[test]
    fn verification_complete_final_zero_balance_can_publish_unchanged_schema() {
        let fixture = OutputFixture::new();
        let output = fixture.path("new.json");
        let snapshot = verification_snapshot(&[1]);
        finish_snapshot(snapshot, fixture.options(output.clone()), |snapshot| {
            verify_batch(snapshot, verification_ids(snapshot)?, |id| {
                Ok(verification_reply(id, Some(0), false))
            })
        })
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 3);
        assert_eq!(
            value["approval_rows"][0]["kbo_match"]["status"],
            "live_not_found"
        );
    }

    struct OutputFixture(PathBuf);

    impl OutputFixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("recognition-output-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn options(&self, output: PathBuf) -> SnapshotOptions {
            SnapshotOptions {
                input: self.path("unavailable.xlsx"),
                kbo_database: self.path("unavailable.duckdb"),
                kbo_snapshot_as_of: "2026-11-06".into(),
                register_as_of: chrono::NaiveDate::from_ymd_opt(2026, 11, 6).unwrap(),
                output: Some(output),
                verify_unmatched_live: true,
            }
        }
    }

    impl Drop for OutputFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn output_fresh_path_publishes_complete_json_with_newline() {
        let fixture = OutputFixture::new();
        let output = fixture.path("new.json");
        ensure_new_output(&output).unwrap();
        publish_snapshot(&output, r#"{"audit":"synthetic"}"#).unwrap();
        assert_eq!(
            std::fs::read(&output).unwrap(),
            b"{\"audit\":\"synthetic\"}\n"
        );
    }

    #[test]
    fn output_existing_file_refuses_before_input_reads_or_live_verification() {
        let fixture = OutputFixture::new();
        let output = fixture.path("prior.json");
        std::fs::write(&output, b"prior snapshot").unwrap();
        let error = run(fixture.options(output.clone())).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(std::fs::read(&output).unwrap(), b"prior snapshot");
        assert!(!fixture.path("unavailable.xlsx").exists());
        assert!(!fixture.path("unavailable.duckdb").exists());
    }

    #[test]
    fn output_database_alias_refuses_before_input_reads() {
        let fixture = OutputFixture::new();
        let database = fixture.path("snapshot.duckdb");
        std::fs::write(&database, b"synthetic database bytes").unwrap();
        let mut options = fixture.options(database.clone());
        options.kbo_database = database.clone();
        let error = run(options).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read(database).unwrap(),
            b"synthetic database bytes"
        );
    }

    #[test]
    fn output_workbook_and_hardlink_aliases_refuse_before_parsing() {
        let fixture = OutputFixture::new();
        let input = fixture.path("source.xlsx");
        let alias = fixture.path("alias.json");
        std::fs::write(&input, b"synthetic workbook bytes").unwrap();
        std::fs::hard_link(&input, &alias).unwrap();
        for output in [&input, &alias] {
            let mut options = fixture.options(output.clone());
            options.input = input.clone();
            let error = run(options).unwrap_err();
            assert!(error.to_string().contains("already exists"), "{error}");
            assert_eq!(std::fs::read(&input).unwrap(), b"synthetic workbook bytes");
            assert_eq!(std::fs::read(&alias).unwrap(), b"synthetic workbook bytes");
        }
    }

    #[test]
    fn output_directory_refuses_without_changing_its_contents() {
        let fixture = OutputFixture::new();
        let output = fixture.path("directory");
        std::fs::create_dir(&output).unwrap();
        let marker = output.join("keep");
        std::fs::write(&marker, b"owned fixture").unwrap();
        assert!(ensure_new_output(&output).is_err());
        assert!(publish_snapshot(&output, "{}").is_err());
        assert_eq!(std::fs::read(marker).unwrap(), b"owned fixture");
    }

    #[cfg(unix)]
    #[test]
    fn output_symlinks_including_dangling_links_refuse_without_following() {
        let fixture = OutputFixture::new();
        let input = fixture.path("source.xlsx");
        let missing = fixture.path("missing-target");
        std::fs::write(&input, b"synthetic source").unwrap();
        for (name, target) in [("source-link", &input), ("dangling-link", &missing)] {
            let output = fixture.path(name);
            std::os::unix::fs::symlink(target, &output).unwrap();
            let error = run(fixture.options(output.clone())).unwrap_err();
            assert!(error.to_string().contains("already exists"), "{error}");
            assert_eq!(std::fs::read_link(&output).unwrap(), *target);
        }
        assert_eq!(std::fs::read(input).unwrap(), b"synthetic source");
        assert!(!missing.exists());
    }

    #[test]
    fn output_racing_file_or_hardlink_creation_cannot_be_overwritten() {
        let fixture = OutputFixture::new();
        let source = fixture.path("source.xlsx");
        std::fs::write(&source, b"source evidence").unwrap();
        for name in ["racing-file.json", "racing-hardlink.json"] {
            let output = fixture.path(name);
            ensure_new_output(&output).unwrap();
            if name == "racing-file.json" {
                std::fs::write(&output, b"other writer").unwrap();
            } else {
                std::fs::hard_link(&source, &output).unwrap();
            }
            let before = std::fs::read(&output).unwrap();
            assert!(publish_snapshot(&output, "{}").is_err());
            assert_eq!(std::fs::read(output).unwrap(), before);
            assert_eq!(std::fs::read(&source).unwrap(), b"source evidence");
        }
    }

    #[cfg(unix)]
    #[test]
    fn output_racing_symlink_creation_cannot_be_followed() {
        let fixture = OutputFixture::new();
        let source = fixture.path("source.xlsx");
        let missing = fixture.path("missing-target");
        std::fs::write(&source, b"source evidence").unwrap();
        for (name, target) in [("racing-link", &source), ("racing-dangling", &missing)] {
            let output = fixture.path(name);
            ensure_new_output(&output).unwrap();
            std::os::unix::fs::symlink(target, &output).unwrap();
            assert!(publish_snapshot(&output, "{}").is_err());
            assert_eq!(std::fs::read_link(output).unwrap(), *target);
        }
        assert_eq!(std::fs::read(source).unwrap(), b"source evidence");
        assert!(!missing.exists());
    }

    #[test]
    fn output_missing_parent_fails_without_creating_directories() {
        let fixture = OutputFixture::new();
        let output = fixture.path("missing-parent/new.json");
        ensure_new_output(&output).unwrap();
        assert!(publish_snapshot(&output, "{}").is_err());
        assert!(!fixture.path("missing-parent").exists());
    }

    fn to_args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn canonical_enterprise_identity_accepts_legacy_nine_digit_reply() {
        assert!(same_enterprise_number("202239951", "0202.239.951"));
    }

    #[test]
    fn canonical_enterprise_identity_rejects_a_different_number() {
        assert!(!same_enterprise_number("202239952", "0202.239.951"));
        assert!(!same_enterprise_number("unreported", "0202.239.951"));
    }

    #[test]
    fn parse_args_accepts_valid_arguments() {
        let args = to_args(&[
            "export.xlsx",
            "--kbo-snapshot-as-of",
            "2026-10-30",
            "--register-as-of",
            "2026-10-30",
            "--output",
            "target/snapshot.json",
            "--verify-unmatched-live",
        ]);
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.input, PathBuf::from("export.xlsx"));
        assert_eq!(parsed.kbo_snapshot_as_of, "2026-10-30");
        assert_eq!(
            parsed.register_as_of,
            chrono::NaiveDate::from_ymd_opt(2026, 10, 30).unwrap()
        );
        assert_eq!(parsed.output, Some(PathBuf::from("target/snapshot.json")));
        assert!(parsed.verify_unmatched_live);
    }

    #[test]
    fn parse_args_handles_help() {
        let args = to_args(&["--help"]);
        assert!(parse_args(args).unwrap().is_none());
    }

    #[test]
    fn parse_args_requires_dates_and_input() {
        assert!(parse_args(to_args(&[])).is_err());
        assert!(parse_args(to_args(&["export.xlsx"])).is_err());
        assert!(parse_args(to_args(&[
            "export.xlsx",
            "--kbo-snapshot-as-of",
            "2026-10-30"
        ]))
        .is_err());
        assert!(parse_args(to_args(&[
            "export.xlsx",
            "--kbo-snapshot-as-of",
            "invalid-date",
            "--register-as-of",
            "2026-10-30"
        ]))
        .is_err());
    }
}
