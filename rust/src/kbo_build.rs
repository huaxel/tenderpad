use duckdb::Connection;
use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

pub const DEFAULT_CSV_DIR: &str = "data/kbo/_csv";
pub const DEFAULT_OUTPUT: &str = "data/kbo/kbo_enrich.duckdb";

const TABLES: [&str; 6] = [
    "kbo_enterprise",
    "kbo_establishment",
    "kbo_address",
    "kbo_activity",
    "kbo_denomination",
    "kbo_code",
];

const INDEXES: [&str; 3] = [
    "CREATE INDEX idx_addr_entity ON kbo_address(EntityNumber)",
    "CREATE INDEX idx_addr_zip ON kbo_address(Zipcode)",
    "CREATE INDEX idx_estab_entity ON kbo_establishment(EnterpriseNumber)",
];

const READ_OPTIONS: &str = "header=true, quote='\"', delim=',', auto_detect=false";

fn sql_escape(value: &str) -> String {
    value.replace('\'', "''")
}

/// Optional record of the row counts a complete export contains, written next
/// to the CSVs as `expected_rows.csv` with `table,rows` lines. Absent means the
/// import runs without the check rather than refusing to work.
const EXPECTED_ROWS_FILE: &str = "expected_rows.csv";

fn write_expected_rows(
    csv_dir: &Path,
    counts: &HashMap<String, usize>,
) -> Result<(), Box<dyn Error>> {
    let mut rows: Vec<String> = counts
        .iter()
        .map(|(table, count)| format!("{table},{count}"))
        .collect();
    rows.sort();
    // Published atomically so an interrupted write cannot leave a half
    // manifest that would mis-describe a later import.
    let staging = csv_dir.join(format!("{EXPECTED_ROWS_FILE}.staging"));
    let mut file = File::create(&staging)?;
    file.write_all(
        format!(
            "# Row counts of the export these CSVs came from. The first successful\n\
             # import records them, and later imports must match: a download cut at a\n\
             # line boundary has no malformed row to reject. Check the numbers against\n\
             # the snapshot documented in data/kbo/README.md.\n{}\n",
            rows.join("\n")
        )
        .as_bytes(),
    )?;
    file.sync_all()?;
    fs::rename(&staging, csv_dir.join(EXPECTED_ROWS_FILE))?;
    println!("Recorded row counts in {EXPECTED_ROWS_FILE} for future truncation checks");
    Ok(())
}

fn read_expected_rows(csv_dir: &Path) -> Result<Option<HashMap<String, usize>>, Box<dyn Error>> {
    let path = csv_dir.join(EXPECTED_ROWS_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)?;
    let mut expected = HashMap::new();
    for (number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((table, rows)) = line.split_once(',') else {
            return Err(format!(
                "{} line {}: expected 'table,rows'",
                path.display(),
                number + 1
            )
            .into());
        };
        let table = table.trim();
        let rows: usize = rows.trim().replace('_', "").parse().map_err(|_| {
            format!(
                "{} line {}: '{rows}' is not a row count",
                path.display(),
                number + 1
            )
        })?;
        if !TABLES.contains(&table) {
            return Err(format!(
                "{} line {}: '{table}' is not a KBO table",
                path.display(),
                number + 1
            )
            .into());
        }
        expected.insert(table.to_owned(), rows);
    }
    Ok(Some(expected))
}

fn table_columns(table: &str) -> &'static [&'static str] {
    match table {
        "kbo_enterprise" => &[
            "EnterpriseNumber",
            "Status",
            "JuridicalSituation",
            "TypeOfEnterprise",
            "JuridicalForm",
            "JuridicalFormCAC",
            "StartDate",
        ],
        "kbo_establishment" => &["EstablishmentNumber", "StartDate", "EnterpriseNumber"],
        "kbo_address" => &[
            "EntityNumber",
            "TypeOfAddress",
            "CountryNL",
            "CountryFR",
            "Zipcode",
            "MunicipalityNL",
            "MunicipalityFR",
            "StreetNL",
            "StreetFR",
            "HouseNumber",
            "Box",
            "ExtraAddressInfo",
            "DateStrikingOff",
        ],
        "kbo_activity" => &[
            "EntityNumber",
            "ActivityGroup",
            "NaceVersion",
            "NaceCode",
            "Classification",
        ],
        "kbo_denomination" => &[
            "EntityNumber",
            "Language",
            "TypeOfDenomination",
            "Denomination",
        ],
        "kbo_code" => &["Category", "Code", "Language", "Description"],
        _ => &[],
    }
}

fn read_sql(table: &str, csv_dir: &Path) -> Result<String, Box<dyn Error>> {
    let base_name = table.strip_prefix("kbo_").unwrap_or(table);
    let csv_path = csv_dir.join(format!("{base_name}.csv"));
    let path_str = csv_path.to_str().ok_or("CSV path contains invalid UTF-8")?;
    let escaped_path = sql_escape(path_str);
    let columns = table_columns(table);
    let names = columns
        .iter()
        .map(|col| format!("'{col}': 'VARCHAR'"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "read_csv('{escaped_path}', {READ_OPTIONS}, columns={{{names}}})"
    ))
}

pub fn build(
    csv_dir: impl AsRef<Path>,
    output: impl AsRef<Path>,
    force: bool,
) -> Result<HashMap<String, usize>, Box<dyn Error>> {
    let csv_dir = csv_dir.as_ref();
    let output = output.as_ref();

    let mut missing = Vec::new();
    for table in TABLES {
        let base_name = table.strip_prefix("kbo_").unwrap_or(table);
        let csv_file = csv_dir.join(format!("{base_name}.csv"));
        if !csv_file.exists() {
            missing.push(format!("{base_name}.csv"));
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "missing CSVs in {}: {}",
            csv_dir.display(),
            missing.join(", ")
        )
        .into());
    }

    let mut inputs: Vec<_> = TABLES
        .iter()
        .map(|table| {
            csv_dir.join(format!(
                "{}.csv",
                table.strip_prefix("kbo_").unwrap_or(table)
            ))
        })
        .collect();
    let metadata_csv = csv_dir.join("meta.csv");
    if metadata_csv.is_file() {
        inputs.push(metadata_csv.clone());
    }
    let input_refs: Vec<_> = inputs.iter().map(|path| path.as_path()).collect();
    let staging_owner = crate::rebuild::Rebuild::new(output, &input_refs, force)?;
    let staging = staging_owner.path();

    let build_result = (|| -> Result<HashMap<String, usize>, Box<dyn Error>> {
        let connection = Connection::open(&staging)?;

        let r_enterprise = read_sql("kbo_enterprise", csv_dir)?;
        connection.execute_batch(&format!(
            r#"
        CREATE TABLE kbo_enterprise AS SELECT
            NULLIF(EnterpriseNumber, '') AS EnterpriseNumber,
            NULLIF(Status, '') AS Status,
            NULLIF(JuridicalSituation, '') AS JuridicalSituation,
            TypeOfEnterprise::BIGINT AS TypeOfEnterprise,
            NULLIF(JuridicalForm, '') AS JuridicalForm,
            NULLIF(JuridicalFormCAC, '') AS JuridicalFormCAC,
            STRPTIME(StartDate, '%d-%m-%Y')::DATE AS StartDate
        FROM {r_enterprise};
        "#
        ))?;

        let r_establishment = read_sql("kbo_establishment", csv_dir)?;
        connection.execute_batch(&format!(
            r#"
        CREATE TABLE kbo_establishment AS SELECT
            NULLIF(EstablishmentNumber, '') AS EstablishmentNumber,
            STRPTIME(StartDate, '%d-%m-%Y')::DATE AS StartDate,
            NULLIF(EnterpriseNumber, '') AS EnterpriseNumber
        FROM {r_establishment};
        "#
        ))?;

        let r_address = read_sql("kbo_address", csv_dir)?;
        connection.execute_batch(&format!(
            r#"
        CREATE TABLE kbo_address AS SELECT
            NULLIF(EntityNumber, '') AS EntityNumber,
            NULLIF(TypeOfAddress, '') AS TypeOfAddress,
            NULLIF(CountryNL, '') AS CountryNL,
            NULLIF(CountryFR, '') AS CountryFR,
            NULLIF(Zipcode, '') AS Zipcode,
            NULLIF(MunicipalityNL, '') AS MunicipalityNL,
            NULLIF(MunicipalityFR, '') AS MunicipalityFR,
            NULLIF(StreetNL, '') AS StreetNL,
            NULLIF(StreetFR, '') AS StreetFR,
            NULLIF(HouseNumber, '') AS HouseNumber,
            NULLIF(Box, '') AS Box,
            NULLIF(ExtraAddressInfo, '') AS ExtraAddressInfo,
            TRY_STRPTIME(DateStrikingOff, '%d-%m-%Y')::DATE AS DateStrikingOff
        FROM {r_address};
        "#
        ))?;

        let r_activity = read_sql("kbo_activity", csv_dir)?;
        connection.execute_batch(&format!(
            r#"
        CREATE TABLE kbo_activity AS SELECT
            NULLIF(EntityNumber, '') AS EntityNumber,
            NULLIF(NaceCode, '') AS NaceCode,
            NULLIF(Classification, '') AS Classification
        FROM {r_activity}
        WHERE NaceVersion = '2025' AND Classification = 'MAIN';
        "#
        ))?;

        let r_denomination = read_sql("kbo_denomination", csv_dir)?;
        connection.execute_batch(&format!(
            r#"
        CREATE TABLE kbo_denomination AS SELECT
            NULLIF(EntityNumber, '') AS EntityNumber,
            Language::BIGINT AS Language,
            NULLIF(TypeOfDenomination, '') AS TypeOfDenomination,
            NULLIF(Denomination, '') AS Denomination
        FROM {r_denomination};
        "#
        ))?;

        let r_code = read_sql("kbo_code", csv_dir)?;
        connection.execute_batch(&format!(
            r#"
        CREATE TABLE kbo_code AS SELECT
            NULLIF(Category, '') AS Category,
            NULLIF(Code, '') AS Code,
            NULLIF(Language, '') AS Language,
            NULLIF(Description, '') AS Description
        FROM {r_code};
        "#
        ))?;

        connection
            .execute_batch("CREATE TABLE kbo_source_metadata (Variable VARCHAR, Value VARCHAR)")?;
        if metadata_csv.is_file() {
            let path_str = metadata_csv
                .to_str()
                .ok_or("KBO metadata CSV path contains invalid UTF-8")?;
            let escaped_path = sql_escape(path_str);
            connection.execute_batch(&format!(
                "INSERT INTO kbo_source_metadata SELECT NULLIF(Variable, ''), NULLIF(Value, '') \
                 FROM read_csv('{escaped_path}', {READ_OPTIONS}, \
                 columns={{'Variable': 'VARCHAR', 'Value': 'VARCHAR'}})"
            ))?;
        }

        for statement in INDEXES {
            connection.execute_batch(statement)?;
        }

        let mut counts = HashMap::new();
        for table in TABLES {
            let count: usize =
                connection.query_row(&format!(r#"SELECT count(*) FROM "{table}""#), [], |row| {
                    row.get(0)
                })?;
            counts.insert(table.to_owned(), count);
        }

        let empty: Vec<&str> = TABLES
            .iter()
            .filter(|table| counts.get(**table).is_some_and(|count| *count == 0))
            .copied()
            .collect();
        if !empty.is_empty() {
            return Err(format!(
                "refusing to accept rebuild with empty tables: {}",
                empty.join(", ")
            )
            .into());
        }

        // A download cut at a line boundary is indistinguishable from a complete
        // smaller extract by shape alone, so compare against the counts recorded
        // for a known-good snapshot. The first successful import records that
        // baseline, which arms the check for every import after it: compare the
        // recorded numbers against the counts documented in data/kbo/README.md
        // to confirm the baseline itself came from a complete export.
        if let Some(expected) = read_expected_rows(csv_dir)? {
            let mut mismatched = Vec::new();
            for (table, want) in &expected {
                if let Some(got) = counts.get(table) {
                    if got != want {
                        mismatched.push(format!("{table}: expected {want}, imported {got}"));
                    }
                }
            }
            if !mismatched.is_empty() {
                return Err(format!(
                    "imported row counts do not match {}: {} - the export is probably truncated",
                    EXPECTED_ROWS_FILE,
                    mismatched.join("; ")
                )
                .into());
            }
        } else {
            write_expected_rows(csv_dir, &counts)?;
        }

        connection.execute_batch("CHECKPOINT")?;
        Ok(counts)
    })();
    let counts = build_result?;
    staging_owner.publish()?;

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(prefix: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let count = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should advance")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("{prefix}-{}-{nanos}-{count}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_csv(directory: &Path, name: &str, content: &str) {
        let mut file = File::create(directory.join(name)).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn sample_csvs(directory: &Path) {
        write_csv(
            directory,
            "enterprise.csv",
            "\"EnterpriseNumber\",\"Status\",\"JuridicalSituation\",\"TypeOfEnterprise\",\"JuridicalForm\",\"JuridicalFormCAC\",\"StartDate\"\n\
             \"0200.065.765\",\"AC\",\"000\",\"2\",\"416\",\"\",\"09-08-1960\"\n",
        );
        write_csv(
            directory,
            "establishment.csv",
            "\"EstablishmentNumber\",\"StartDate\",\"EnterpriseNumber\"\n\
             \"2.000.000.339\",\"01-11-1974\",\"0403.449.823\"\n",
        );
        write_csv(
            directory,
            "address.csv",
            "\"EntityNumber\",\"TypeOfAddress\",\"CountryNL\",\"CountryFR\",\"Zipcode\",\"MunicipalityNL\",\"MunicipalityFR\",\"StreetNL\",\"StreetFR\",\"HouseNumber\",\"Box\",\"ExtraAddressInfo\",\"DateStrikingOff\"\n\
             \"0200.065.765\",\"REGO\",\"\",\"\",\"9070\",\"Destelbergen\",\"Destelbergen\",\"Panhuisstraat\",\"Panhuisstraat\",\"1\",\"\",\"\",\"\"\n\
             \"0200.065.765\",\"REGO\",\"\",\"\",\"9070\",\"Destelbergen\",\"Destelbergen\",\"Oude Straat\",\"Oude Straat\",\"2\",\"A\",\"\",\"06-06-2024\"\n",
        );
        write_csv(
            directory,
            "activity.csv",
            "\"EntityNumber\",\"ActivityGroup\",\"NaceVersion\",\"NaceCode\",\"Classification\"\n\
             \"0200.065.765\",\"006\",\"2025\",\"84130\",\"MAIN\"\n\
             \"0200.065.765\",\"001\",\"2025\",\"68121\",\"MAIN\"\n\
             \"0200.065.765\",\"001\",\"2025\",\"84130\",\"SECO\"\n\
             \"0200.065.765\",\"001\",\"2008\",\"84130\",\"MAIN\"\n\
             \"0200.065.765\",\"001\",\"2003\",\"70111\",\"MAIN\"\n",
        );
        write_csv(
            directory,
            "denomination.csv",
            "\"EntityNumber\",\"Language\",\"TypeOfDenomination\",\"Denomination\"\n\
             \"0200.065.765\",\"2\",\"001\",\"Veneco\"\n",
        );
        write_csv(
            directory,
            "code.csv",
            "\"Category\",\"Code\",\"Language\",\"Description\"\n\
             \"Nace2025\",\"84130\",\"FR\",\"Promotion immobilière\"\n",
        );
    }

    #[test]
    fn test_build_types_dates_and_nulls() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let output = temp_dir.path().join("out.duckdb");

        let counts = build(temp_dir.path(), &output, false).unwrap();
        assert_eq!(counts.get("kbo_enterprise"), Some(&1));

        struct EnterpriseRow {
            num: String,
            status: Option<String>,
            sit: Option<String>,
            type_ent: i64,
            form: Option<String>,
            cac: Option<String>,
            start: String,
        }

        let con = Connection::open(&output).unwrap();
        let row = con
            .query_row(
                "SELECT EnterpriseNumber, Status, JuridicalSituation, TypeOfEnterprise, JuridicalForm, JuridicalFormCAC, CAST(StartDate AS VARCHAR) FROM kbo_enterprise",
                [],
                |row| {
                    Ok(EnterpriseRow {
                        num: row.get(0)?,
                        status: row.get(1)?,
                        sit: row.get(2)?,
                        type_ent: row.get(3)?,
                        form: row.get(4)?,
                        cac: row.get(5)?,
                        start: row.get(6)?,
                    })
                },
            )
            .unwrap();

        assert_eq!(row.num, "0200.065.765");
        assert_eq!(row.status, Some("AC".to_string()));
        assert_eq!(row.sit, Some("000".to_string()));
        assert_eq!(row.type_ent, 2);
        assert_eq!(row.form, Some("416".to_string()));
        assert_eq!(row.cac, None);
        assert_eq!(row.start, "1960-08-09");

        let estab_start: String = con
            .query_row(
                "SELECT CAST(StartDate AS VARCHAR) FROM kbo_establishment",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(estab_start, "1974-11-01");

        let addr1: (Option<String>, Option<String>) = con
            .query_row(
                "SELECT Box, CAST(DateStrikingOff AS VARCHAR) FROM kbo_address WHERE HouseNumber = '1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(addr1.0, None);
        assert_eq!(addr1.1, None);

        let addr2: (Option<String>, Option<String>) = con
            .query_row(
                "SELECT Box, CAST(DateStrikingOff AS VARCHAR) FROM kbo_address WHERE HouseNumber = '2'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(addr2.0, Some("A".to_string()));
        assert_eq!(addr2.1, Some("2024-06-06".to_string()));
    }

    #[test]
    fn test_source_metadata_is_preserved_and_exposed() {
        let without_metadata = TempDir::new("kbo-test");
        sample_csvs(without_metadata.path());
        let legacy_output = without_metadata.path().join("legacy.duckdb");
        build(without_metadata.path(), &legacy_output, false).unwrap();
        let legacy = crate::kbo::KboRepository::open(&legacy_output).unwrap();
        assert_eq!(
            legacy.snapshot_metadata().unwrap(),
            Some(crate::kbo::KboSnapshotMetadata {
                snapshot_date: None,
                extract_timestamp: None,
                extract_type: None,
                extract_number: None,
                version: None,
            })
        );
        drop(legacy);

        let with_metadata = TempDir::new("kbo-test");
        sample_csvs(with_metadata.path());
        write_csv(
            with_metadata.path(),
            "meta.csv",
            "\"Variable\",\"Value\"\n\
             \"SnapshotDate\",\"24-06-2026\"\n\
             \"ExtractTimestamp\",\"25-06-2026 09:17:28\"\n\
             \"ExtractType\",\"full\"\n\
             \"ExtractNumber\",\"401\"\n\
             \"Version\",\"1.0.0\"\n",
        );
        let output = with_metadata.path().join("with-metadata.duckdb");
        build(with_metadata.path(), &output, false).unwrap();
        let repository = crate::kbo::KboRepository::open(&output).unwrap();
        assert_eq!(
            repository.snapshot_metadata().unwrap(),
            Some(crate::kbo::KboSnapshotMetadata {
                snapshot_date: Some("24-06-2026".to_owned()),
                extract_timestamp: Some("25-06-2026 09:17:28".to_owned()),
                extract_type: Some("full".to_owned()),
                extract_number: Some("401".to_owned()),
                version: Some("1.0.0".to_owned()),
            })
        );
    }

    #[test]
    fn test_activity_keeps_current_main_codes_with_raw_multiplicity() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        write_csv(
            temp_dir.path(),
            "activity.csv",
            "\"EntityNumber\",\"ActivityGroup\",\"NaceVersion\",\"NaceCode\",\"Classification\"\n\
             \"0200.065.765\",\"006\",\"2025\",\"84130\",\"MAIN\"\n\
             \"0200.065.765\",\"006\",\"2025\",\"84130\",\"MAIN\"\n\
             \"0200.065.765\",\"001\",\"2025\",\"68121\",\"MAIN\"\n\
             \"0200.065.765\",\"003\",\"2025\",\"68121\",\"MAIN\"\n\
             \"0200.065.765\",\"001\",\"2025\",\"84130\",\"SECO\"\n\
             \"0200.065.765\",\"001\",\"2008\",\"84130\",\"MAIN\"\n\
             \"0200.065.765\",\"001\",\"2003\",\"70111\",\"MAIN\"\n",
        );
        let output = temp_dir.path().join("out.duckdb");
        build(temp_dir.path(), &output, true).unwrap();

        let con = Connection::open(&output).unwrap();
        let mut stmt = con
            .prepare("SELECT EntityNumber, NaceCode, Classification FROM kbo_activity ORDER BY NaceCode, EntityNumber")
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            rows,
            vec![
                (
                    "0200.065.765".to_string(),
                    "68121".to_string(),
                    "MAIN".to_string()
                ),
                (
                    "0200.065.765".to_string(),
                    "68121".to_string(),
                    "MAIN".to_string()
                ),
                (
                    "0200.065.765".to_string(),
                    "84130".to_string(),
                    "MAIN".to_string()
                ),
                (
                    "0200.065.765".to_string(),
                    "84130".to_string(),
                    "MAIN".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_denomination_language_is_typed_and_indexes_exist() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let output = temp_dir.path().join("out.duckdb");
        build(temp_dir.path(), &output, false).unwrap();

        let con = Connection::open(&output).unwrap();
        let lang: i64 = con
            .query_row("SELECT Language FROM kbo_denomination", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(lang, 2);

        let mut stmt = con
            .prepare("SELECT index_name FROM duckdb_indexes()")
            .unwrap();
        let indexes = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<std::collections::HashSet<_>, _>>()
            .unwrap();

        assert!(indexes.contains("idx_addr_entity"));
        assert!(indexes.contains("idx_addr_zip"));
        assert!(indexes.contains("idx_estab_entity"));
    }

    #[test]
    fn test_refuses_to_overwrite_without_force() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let output = temp_dir.path().join("out.duckdb");
        build(temp_dir.path(), &output, false).unwrap();

        let err = build(temp_dir.path(), &output, false).unwrap_err();
        assert!(err.to_string().contains("pass force=True"));

        let counts = build(temp_dir.path(), &output, true).unwrap();
        assert_eq!(counts.get("kbo_code"), Some(&1));
    }

    #[test]
    fn failed_publication_preserves_output_and_cleans_owned_staging() {
        let dir = TempDir::new("kbo-publication-test");
        sample_csvs(dir.path());
        let output = dir.path().join("existing-directory");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("marker"), b"preserve me").unwrap();
        assert!(build(dir.path(), &output, true).is_err());
        assert_eq!(fs::read(output.join("marker")).unwrap(), b"preserve me");
        assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".db-rebuild-")
        }));
    }

    #[test]
    fn rebuild_refuses_existing_wal_without_deleting_it() {
        let dir = TempDir::new("kbo-wal-test");
        sample_csvs(dir.path());
        let output = dir.path().join("out.duckdb");
        let wal = dir.path().join("out.duckdb.wal");
        fs::write(&wal, b"recovery data").unwrap();
        assert!(build(dir.path(), &output, true)
            .unwrap_err()
            .to_string()
            .contains("WAL"));
        assert_eq!(fs::read(wal).unwrap(), b"recovery data");
        assert!(!output.exists());
    }

    #[test]
    fn test_output_aliasing_an_input_is_rejected_before_deletion() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let input = temp_dir.path().join("enterprise.csv");

        let err = build(temp_dir.path(), &input, true).unwrap_err();
        assert!(err.to_string().contains("must not alias"));
        assert!(
            input.exists(),
            "aliased input must survive the rejected rebuild"
        );
    }

    #[test]
    fn test_empty_tables_are_rejected_and_previous_db_survives() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let output = temp_dir.path().join("out.duckdb");
        build(temp_dir.path(), &output, false).unwrap();
        let before = fs::read(&output).unwrap();

        // Truncate one export to header-only: the activity table would be empty.
        write_csv(
            temp_dir.path(),
            "activity.csv",
            "\"EntityNumber\",\"ActivityGroup\",\"NaceVersion\",\"NaceCode\",\"Classification\"\n",
        );
        let err = build(temp_dir.path(), &output, true).unwrap_err();
        assert!(err.to_string().contains("empty tables"));
        assert_eq!(fs::read(&output).unwrap(), before);
    }

    /// A download cut exactly at a line boundary has no malformed row to
    /// reject, so the recorded counts are the only signal that catches it.
    #[test]
    fn test_the_first_import_arms_the_check_for_later_ones() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let output = temp_dir.path().join("out.duckdb");
        let manifest = temp_dir.path().join("expected_rows.csv");
        assert!(!manifest.exists(), "the baseline starts absent");

        let counts = build(temp_dir.path(), &output, false).unwrap();
        let recorded = fs::read_to_string(&manifest).expect("the first import records a baseline");
        for (table, rows) in &counts {
            assert!(
                recorded.contains(&format!("{table},{rows}")),
                "baseline is missing {table}: {recorded}"
            );
        }

        // A later import that is one whole row short of the recorded snapshot
        // is refused, because the shape of every remaining row is still valid.
        let activity = fs::read_to_string(temp_dir.path().join("address.csv")).unwrap();
        let lines: Vec<&str> = activity.trim_end_matches('\n').split('\n').collect();
        write_csv(
            temp_dir.path(),
            "address.csv",
            &format!("{}\n", lines[..lines.len() - 1].join("\n")),
        );

        let error = build(temp_dir.path(), &output, true)
            .expect_err("a short export must not import as complete");
        assert!(
            error.to_string().contains("probably truncated"),
            "unexpected error: {error}"
        );
        // The baseline is not overwritten by the refused import.
        assert_eq!(
            fs::read_to_string(&manifest).unwrap(),
            recorded,
            "a refused import must not rewrite the baseline"
        );
    }

    #[test]
    fn test_counts_must_match_the_recorded_snapshot() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let output = temp_dir.path().join("out.duckdb");
        let counts = build(temp_dir.path(), &output, false).unwrap();

        // Counts recorded from a complete import let the next one be checked.
        let manifest: String = counts
            .iter()
            .map(|(table, rows)| format!("{table},{rows}\n"))
            .collect();
        write_csv(temp_dir.path(), "expected_rows.csv", &manifest);
        write_csv(temp_dir.path(), "expected_rows.csv", &manifest);
        build(temp_dir.path(), &output, true).expect("matching counts still import");

        // One row short of the recorded snapshot is exactly what a truncated
        // download looks like, and the row shapes are all still valid.
        let short: String = counts
            .iter()
            .map(|(table, rows)| {
                let rows = if table == "kbo_denomination" {
                    rows - 1
                } else {
                    *rows
                };
                format!("{table},{rows}\n")
            })
            .collect();
        write_csv(temp_dir.path(), "expected_rows.csv", &short);
        let error = build(temp_dir.path(), &output, true)
            .expect_err("a short export must not import as complete");
        let message = error.to_string();
        assert!(
            message.contains("probably truncated") && message.contains("kbo_denomination"),
            "unexpected error: {message}"
        );

        // The check is about the file, not the database: with the manifest
        // removed the same inputs import again.
        fs::remove_file(temp_dir.path().join("expected_rows.csv")).unwrap();
        build(temp_dir.path(), &output, true).expect("without a manifest nothing is compared");
    }

    #[test]
    fn test_a_malformed_manifest_fails_rather_than_being_ignored() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let output = temp_dir.path().join("out.duckdb");

        write_csv(
            temp_dir.path(),
            "expected_rows.csv",
            "kbo_activity,not-a-number\n",
        );
        let error = build(temp_dir.path(), &output, false).unwrap_err();
        assert!(error.to_string().contains("not a row count"), "{error}");

        write_csv(temp_dir.path(), "expected_rows.csv", "kbo_unknown,5\n");
        let error = build(temp_dir.path(), &output, false).unwrap_err();
        assert!(error.to_string().contains("not a KBO table"), "{error}");
    }

    /// Truncating an export mid-row must not import as if it were complete.
    /// This pins whatever the reader actually does, so a future change that
    /// starts padding short rows is caught here.
    #[test]
    fn test_truncated_csv_row_does_not_import_silently() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        let output = temp_dir.path().join("out.duckdb");
        build(temp_dir.path(), &output, false).unwrap();

        // Cut the last activity row in half: a download that stopped part way.
        let activity = fs::read_to_string(temp_dir.path().join("activity.csv")).unwrap();
        let truncated = activity.trim_end_matches('\n');
        let mut lines: Vec<&str> = truncated.split('\n').collect();
        let last = lines.pop().expect("at least one row");
        lines.push(&last[..last.len() / 2]);
        write_csv(
            temp_dir.path(),
            "activity.csv",
            &format!("{}\n", lines.join("\n")),
        );

        match build(temp_dir.path(), &output, true) {
            Ok(counts) => panic!("a mid-row truncated export imported as complete: {counts:?}"),
            Err(error) => {
                let message = error.to_string();
                assert!(
                    !message.contains("empty tables"),
                    "truncation should be reported as malformed input, not as empty tables: {message}"
                );
            }
        }
    }

    #[test]
    fn test_missing_csv_fails_fast() {
        let temp_dir = TempDir::new("kbo-test");
        sample_csvs(temp_dir.path());
        fs::remove_file(temp_dir.path().join("code.csv")).unwrap();
        let output = temp_dir.path().join("out.duckdb");

        let err = build(temp_dir.path(), &output, false).unwrap_err();
        assert!(err.to_string().contains("missing CSVs"));
    }
}
