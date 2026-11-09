use crate::store::{is_retryable_write_error, OPEN_LOCK_ATTEMPTS, OPEN_LOCK_DELAY};
use crate::{Activity, Address, CompanyProfile};
use duckdb::params;
use duckdb::{AccessMode, Config, Connection};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::thread;

pub fn normalize_enterprise_number(value: &str) -> Result<String, String> {
    let mut digits = value
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    if digits.len() == 9 {
        digits.insert(0, '0');
    }
    if digits.len() != 10 {
        return Err(format!(
            "Expected 9 or 10 digits, got {} from {value:?}",
            digits.len()
        ));
    }
    Ok(format!(
        "{}.{}.{}",
        &digits[..4],
        &digits[4..7],
        &digits[7..]
    ))
}

pub struct KboRepository {
    connection: Connection,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct KboSnapshotMetadata {
    /// Source string from KBO `meta.csv` (`DD-MM-YYYY`), not a freshness judgment.
    pub snapshot_date: Option<String>,
    pub extract_timestamp: Option<String>,
    pub extract_type: Option<String>,
    pub extract_number: Option<String>,
    pub version: Option<String>,
}

impl KboRepository {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, duckdb::Error> {
        for attempt in 0..OPEN_LOCK_ATTEMPTS {
            let config = Config::default().access_mode(AccessMode::ReadOnly)?;
            match Connection::open_with_flags(path.as_ref(), config) {
                Ok(connection) => return Ok(Self { connection }),
                Err(error)
                    if attempt + 1 < OPEN_LOCK_ATTEMPTS && is_retryable_write_error(&error) =>
                {
                    // Same bounded wait as the tender store: `kbo-rebuild`
                    // holds the write lock for the whole rebuild.
                    thread::sleep(OPEN_LOCK_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("the lock retry loop returns from inside its body")
    }

    pub fn health_check(&self) -> Result<(), duckdb::Error> {
        self.connection
            .query_row("SELECT count(*) FROM kbo_enterprise", [], |_| Ok(()))
    }

    /// Source snapshot metadata imported from the official `meta.csv` when
    /// present. Legacy KBO databases without the metadata table return None.
    pub fn snapshot_metadata(
        &self,
    ) -> Result<Option<KboSnapshotMetadata>, Box<dyn std::error::Error>> {
        let table_count: i64 = self.connection.query_row(
            "SELECT count(*) FROM information_schema.tables \
             WHERE lower(table_name) = 'kbo_source_metadata'",
            [],
            |row| row.get(0),
        )?;
        if table_count == 0 {
            return Ok(None);
        }

        let mut statement = self
            .connection
            .prepare("SELECT Variable, Value FROM kbo_source_metadata")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?;
        let mut values = HashMap::new();
        for row in rows {
            let (Some(key), Some(value)) = row? else {
                continue;
            };
            if values.insert(key.clone(), value).is_some() {
                return Err(format!("duplicate KBO snapshot metadata key {key:?}").into());
            }
        }
        let snapshot_date = values.remove("SnapshotDate");
        Ok(Some(KboSnapshotMetadata {
            snapshot_date,
            extract_timestamp: values.remove("ExtractTimestamp"),
            extract_type: values.remove("ExtractType"),
            extract_number: values.remove("ExtractNumber"),
            version: values.remove("Version"),
        }))
    }

    /// Bulk version of [`lookup`]: the same rows for many enterprises, loaded
    /// with chunked `IN` queries instead of one round-trip per company.
    /// Measured 2026-10-23: per-company `lookup` scored ~45 ms/company
    /// (eight sequential queries), timing out a 25k-company reverse search;
    /// the batched form loads the same pool in seconds. Order follows the
    /// input with duplicates removed.
    pub fn lookup_batch(
        &self,
        enterprise_numbers: &[String],
    ) -> Result<Vec<CompanyProfile>, Box<dyn std::error::Error>> {
        let mut seen = HashSet::new();
        let mut ordered: Vec<String> = Vec::new();
        for number in enterprise_numbers {
            if seen.insert(number.clone()) {
                ordered.push(number.clone());
            }
        }
        let mut profiles: HashMap<String, CompanyProfile> = HashMap::new();
        for chunk in ordered.chunks(2000) {
            let refs: Vec<&dyn duckdb::ToSql> = chunk
                .iter()
                .map(|item| item as &dyn duckdb::ToSql)
                .collect();
            let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
            let mut statement = self.connection.prepare(&format!(
                "SELECT EnterpriseNumber, Status, JuridicalSituation, \
                 JuridicalForm, CAST(StartDate AS VARCHAR) FROM kbo_enterprise \
                 WHERE EnterpriseNumber IN ({placeholders})"
            ))?;
            for row in statement.query_map(&refs[..], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })? {
                let (number, status, situation, form, start) = row?;
                profiles.insert(
                    number.clone(),
                    CompanyProfile {
                        enterprise_number: number,
                        status,
                        juridical_situation: situation,
                        juridical_form: form,
                        start_date: start,
                        names: Vec::new(),
                        activities: Vec::new(),
                        addresses: Vec::new(),
                        establishments: Vec::new(),
                        ..Default::default()
                    },
                );
            }
            let mut statement = self.connection.prepare(&format!(
                "SELECT DISTINCT EntityNumber, Denomination FROM kbo_denomination \
                 WHERE EntityNumber IN ({placeholders}) AND Denomination IS NOT NULL \
                 ORDER BY 1, 2"
            ))?;
            for row in statement.query_map(&refs[..], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (id, name) = row?;
                if let Some(profile) = profiles.get_mut(&id) {
                    profile.names.push(name);
                }
            }
            let mut statement = self.connection.prepare(&format!(
                "SELECT a.EntityNumber, a.NaceCode, a.Classification, \
                 MAX(CASE WHEN UPPER(c.Language) IN ('FR', '1') THEN c.Description END), \
                 MAX(CASE WHEN UPPER(c.Language) IN ('NL', '2') THEN c.Description END) \
                 FROM kbo_activity AS a LEFT JOIN kbo_code AS c \
                 ON UPPER(c.Category) IN ('NACE2025', 'NACE') AND c.Code = a.NaceCode \
                 WHERE a.EntityNumber IN ({placeholders}) \
                 GROUP BY a.EntityNumber, a.NaceCode, a.Classification ORDER BY 1, 2"
            ))?;
            for row in statement.query_map(&refs[..], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    Activity {
                        code: row.get(1)?,
                        classification: row.get(2)?,
                        french: row.get(3)?,
                        dutch: row.get(4)?,
                    },
                ))
            })? {
                let (id, activity) = row?;
                if let Some(profile) = profiles.get_mut(&id) {
                    profile.activities.push(activity);
                }
            }
            let mut statement = self.connection.prepare(&format!(
                "SELECT EntityNumber, TypeOfAddress, Zipcode, MunicipalityNL, \
                 MunicipalityFR, StreetNL, StreetFR, HouseNumber, Box \
                 FROM kbo_address WHERE EntityNumber IN ({placeholders}) \
                 AND DateStrikingOff IS NULL \
                 ORDER BY EntityNumber, TypeOfAddress, Zipcode, StreetNL"
            ))?;
            for row in statement.query_map(&refs[..], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    Address {
                        address_type: row.get(1)?,
                        zipcode: row.get(2)?,
                        municipality_nl: row.get(3)?,
                        municipality_fr: row.get(4)?,
                        street_nl: row.get(5)?,
                        street_fr: row.get(6)?,
                        house_number: row.get(7)?,
                        box_value: row.get(8)?,
                    },
                ))
            })? {
                let (id, address) = row?;
                if let Some(profile) = profiles.get_mut(&id) {
                    profile.addresses.push(address);
                }
            }
            let mut statement = self.connection.prepare(&format!(
                "SELECT EnterpriseNumber, EstablishmentNumber FROM kbo_establishment \
                 WHERE EnterpriseNumber IN ({placeholders}) ORDER BY 1, 2"
            ))?;
            for row in statement.query_map(&refs[..], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (id, establishment) = row?;
                if let Some(profile) = profiles.get_mut(&id) {
                    profile.establishments.push(establishment);
                }
            }
        }
        Ok(ordered
            .into_iter()
            .filter_map(|number| profiles.remove(&number))
            .collect())
    }

    /// Candidate enterprises holding an activity under one of the NACE
    /// prefixes, for reverse search pools. `EntityNumber` mixes enterprises
    /// (`0.`/`1.`) and establishment units (`2.`–`8.`): the latter resolve to
    /// their parent enterprise so a Brussels branch scores as its company.
    /// Sorted, deduplicated, and capped at `limit` for bounded runs; the
    /// second return is the pre-cap total so callers can report recall.
    /// The cap keeps the lowest enterprise numbers, i.e. the oldest
    /// registrations — deterministic, establishment-friendly, and disclosed
    /// to callers rather than silently sampled.
    pub fn enterprises_for_nace(
        &self,
        prefixes: &[String],
        limit: usize,
    ) -> Result<(Vec<String>, usize), Box<dyn std::error::Error>> {
        if prefixes.is_empty() {
            return Err("NACE prefix list must not be empty".into());
        }
        for prefix in prefixes {
            if prefix.is_empty()
                || prefix.len() > 5
                || !prefix.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(format!("NACE prefix must be 1-5 digits, got {prefix:?}").into());
            }
        }
        let filter = prefixes
            .iter()
            .map(|prefix| format!("NaceCode LIKE '{prefix}%'"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut statement = self.connection.prepare(&format!(
            "SELECT DISTINCT EntityNumber FROM kbo_activity WHERE {filter}"
        ))?;
        let hits = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut enterprises: Vec<String> = Vec::new();
        let mut units: Vec<String> = Vec::new();
        for id in hits {
            match id.chars().next() {
                Some('0') | Some('1') => enterprises.push(id),
                _ => units.push(id),
            }
        }
        for chunk in units.chunks(2000) {
            let refs: Vec<&dyn duckdb::ToSql> = chunk
                .iter()
                .map(|item| item as &dyn duckdb::ToSql)
                .collect();
            let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
            let mut statement = self.connection.prepare(&format!(
                "SELECT DISTINCT EnterpriseNumber FROM kbo_establishment \
                 WHERE EstablishmentNumber IN ({placeholders})"
            ))?;
            for row in statement.query_map(&refs[..], |row| row.get::<_, String>(0))? {
                enterprises.push(row?);
            }
        }
        enterprises.sort();
        enterprises.dedup();
        let total = enterprises.len();
        enterprises.truncate(limit);
        Ok((enterprises, total))
    }

    pub fn lookup(
        &self,
        value: &str,
    ) -> Result<Option<CompanyProfile>, Box<dyn std::error::Error>> {
        let enterprise_number = normalize_enterprise_number(value)?;
        let enterprise = self.connection.query_row(
            r#"
            SELECT EnterpriseNumber, Status, JuridicalSituation,
                   JuridicalForm, CAST(StartDate AS VARCHAR)
            FROM kbo_enterprise
            WHERE EnterpriseNumber = ?
            "#,
            params![enterprise_number],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        );
        let (enterprise_number, status, juridical_situation, juridical_form, start_date) =
            match enterprise {
                Ok(value) => value,
                Err(duckdb::Error::QueryReturnedNoRows) => return Ok(None),
                Err(error) => return Err(error.into()),
            };

        let mut names_statement = self.connection.prepare(
            r#"
            SELECT DISTINCT Denomination
            FROM kbo_denomination
            WHERE EntityNumber = ? AND Denomination IS NOT NULL
            ORDER BY Denomination
            "#,
        )?;
        let names = names_statement
            .query_map(params![&enterprise_number], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;

        let mut activities_statement = self.connection.prepare(
            r#"
            SELECT a.NaceCode, a.Classification,
                   MAX(CASE WHEN UPPER(c.Language) IN ('FR', '1') THEN c.Description END),
                   MAX(CASE WHEN UPPER(c.Language) IN ('NL', '2') THEN c.Description END)
            FROM kbo_activity AS a
            LEFT JOIN kbo_code AS c
              ON UPPER(c.Category) IN ('NACE2025', 'NACE') AND c.Code = a.NaceCode
            WHERE a.EntityNumber = ?
            GROUP BY a.NaceCode, a.Classification
            ORDER BY a.NaceCode
            "#,
        )?;
        let activities = activities_statement
            .query_map(params![&enterprise_number], |row| {
                Ok(Activity {
                    code: row.get(0)?,
                    classification: row.get(1)?,
                    french: row.get(2)?,
                    dutch: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<Activity>, _>>()?;

        let mut addresses_statement = self.connection.prepare(
            r#"
            SELECT TypeOfAddress, Zipcode, MunicipalityNL, MunicipalityFR,
                   StreetNL, StreetFR, HouseNumber, Box
            FROM kbo_address
            WHERE EntityNumber = ? AND DateStrikingOff IS NULL
            ORDER BY TypeOfAddress, Zipcode, StreetNL
            "#,
        )?;
        let addresses = addresses_statement
            .query_map(params![&enterprise_number], |row| {
                Ok(Address {
                    address_type: row.get(0)?,
                    zipcode: row.get(1)?,
                    municipality_nl: row.get(2)?,
                    municipality_fr: row.get(3)?,
                    street_nl: row.get(4)?,
                    street_fr: row.get(5)?,
                    house_number: row.get(6)?,
                    box_value: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<Address>, _>>()?;

        let mut establishments_statement = self.connection.prepare(
            r#"
            SELECT EstablishmentNumber
            FROM kbo_establishment
            WHERE EnterpriseNumber = ?
            ORDER BY EstablishmentNumber
            "#,
        )?;
        let establishments = establishments_statement
            .query_map(params![&enterprise_number], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;

        Ok(Some(CompanyProfile {
            enterprise_number,
            status,
            juridical_situation,
            juridical_form,
            start_date,
            names,
            activities,
            addresses,
            establishments,
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch_fixture(suffix: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("aanbestedingen-kbo-batch-{suffix}.duckdb"));
        let _ = std::fs::remove_file(&path);
        let connection = Connection::open(&path).expect("fixture database should open");
        connection
            .execute_batch(
                "CREATE TABLE kbo_enterprise (EnterpriseNumber VARCHAR, Status VARCHAR, \
                 JuridicalSituation VARCHAR, JuridicalForm VARCHAR, StartDate DATE); \
                 CREATE TABLE kbo_denomination (EntityNumber VARCHAR, Denomination VARCHAR); \
                 CREATE TABLE kbo_activity (EntityNumber VARCHAR, NaceCode VARCHAR, \
                 Classification VARCHAR); \
                 CREATE TABLE kbo_code (Category VARCHAR, Code VARCHAR, Language VARCHAR, \
                 Description VARCHAR); \
                 CREATE TABLE kbo_address (EntityNumber VARCHAR, TypeOfAddress VARCHAR, \
                 Zipcode VARCHAR, MunicipalityNL VARCHAR, MunicipalityFR VARCHAR, \
                 StreetNL VARCHAR, StreetFR VARCHAR, HouseNumber VARCHAR, Box VARCHAR, \
                 DateStrikingOff DATE); \
                 CREATE TABLE kbo_establishment (EnterpriseNumber VARCHAR, \
                 EstablishmentNumber VARCHAR);",
            )
            .expect("fixture schema should build");
        connection
            .execute(
                "INSERT INTO kbo_enterprise VALUES \
            ('0631.997.362', 'AC', NULL, NULL, NULL), ('0418.547.872', 'AC', NULL, NULL, NULL), \
            ('0475.396.406', 'AC', NULL, NULL, NULL)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO kbo_denomination VALUES \
            ('0631.997.362', 'DADA CONSULTANCY'), ('0418.547.872', 'WORLDLINE')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO kbo_activity VALUES \
            ('0631.997.362', '63100', 'MAIN'), ('0418.547.872', '63100', 'MAIN'), \
            ('2.100.000.001', '62100', 'MAIN'), ('0475.396.406', '46460', 'MAIN')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO kbo_code VALUES \
            ('Nace2025', '63100', 'NL', 'hosting en gegevensverwerking'), \
            ('Nace2025', '63100', 'FR', 'hébergement'), \
            ('Nace2025', '62100', 'NL', 'programmeren'), \
            ('NACE', '46460', '2', 'Groothandel in farmaceutische producten'), \
            ('NACE', '46460', '1', 'Commerce de gros de produits pharmaceutiques')",
                [],
            )
            .unwrap();
        connection.execute("INSERT INTO kbo_address VALUES \
            ('0631.997.362', 'REGO', '1000', 'Brussel', 'Bruxelles', NULL, NULL, NULL, NULL, NULL), \
            ('0418.547.872', 'REGO', '1210', 'Sint-Joost', 'Saint-Josse', NULL, NULL, NULL, NULL, NULL)", []).unwrap();
        connection
            .execute(
                "INSERT INTO kbo_establishment VALUES \
            ('0475.396.406', '2.100.000.001'), ('0631.997.362', '2.100.000.002')",
                [],
            )
            .unwrap();
        drop(connection);
        path
    }

    /// The batched form must return exactly what per-company `lookup` sees:
    /// names, NACE descriptions, addresses and establishments.
    #[test]
    fn lookup_batch_matches_individual_lookups() {
        let path = batch_fixture("parity");
        let repository = KboRepository::open(&path).expect("repository should open");
        assert_eq!(repository.snapshot_metadata().unwrap(), None);
        let ids = vec![
            "0631.997.362".to_owned(),
            "0418.547.872".to_owned(),
            "0475.396.406".to_owned(),
        ];
        let batched = repository.lookup_batch(&ids).expect("batch should load");
        assert_eq!(batched.len(), 3);
        for (company, id) in batched.iter().zip(ids.iter()) {
            let single = repository
                .lookup(id)
                .expect("lookup should run")
                .expect("company should exist");
            assert_eq!(company.enterprise_number, single.enterprise_number);
            assert_eq!(company.names, single.names);
            assert_eq!(company.activities.len(), single.activities.len());
            assert_eq!(company.addresses.len(), single.addresses.len());
            assert_eq!(company.establishments, single.establishments);
        }
        assert_eq!(batched[0].names, vec!["DADA CONSULTANCY".to_owned()]);
        assert_eq!(
            batched[0].activities[0].dutch.as_deref(),
            Some("hosting en gegevensverwerking")
        );
        assert_eq!(
            batched[2].activities[0].dutch.as_deref(),
            Some("Groothandel in farmaceutische producten")
        );
        let pharmacy = repository
            .lookup("0475.396.406")
            .expect("legacy NACE lookup should run")
            .expect("pharmacy should exist");
        assert_eq!(
            pharmacy.activities[0].dutch.as_deref(),
            Some("Groothandel in farmaceutische producten")
        );
        assert_eq!(
            pharmacy.activities[0].french.as_deref(),
            Some("Commerce de gros de produits pharmaceutiques")
        );
        assert_eq!(batched[0].establishments, vec!["2.100.000.002".to_owned()]);
        let _ = std::fs::remove_file(&path);
    }

    /// Establishment units resolve to their parent enterprise; unknown ids are
    /// skipped and bad prefixes rejected instead of interpolated into SQL.
    #[test]
    fn nace_pool_maps_units_to_parents() {
        let path = batch_fixture("pool");
        let repository = KboRepository::open(&path).expect("repository should open");
        let (pool, total) = repository
            .enterprises_for_nace(&["63".to_owned()], 100)
            .expect("pool should load");
        assert_eq!(
            pool,
            vec!["0418.547.872".to_owned(), "0631.997.362".to_owned()]
        );
        assert_eq!(total, 2);
        let (pool, total) = repository
            .enterprises_for_nace(&["62".to_owned()], 100)
            .expect("pool should load");
        // Only hit is the establishment unit 2.100.000.001, held by CERTIPOST.
        assert_eq!(pool, vec!["0475.396.406".to_owned()]);
        assert_eq!(total, 1);
        let (pool, total) = repository
            .enterprises_for_nace(&["63".to_owned()], 1)
            .expect("pool should load");
        assert_eq!(pool.len(), 1);
        assert_eq!(total, 2, "the total reports pre-cap recall");
        assert!(repository
            .enterprises_for_nace(&["62'".to_owned()], 100)
            .is_err());
        assert!(repository
            .enterprises_for_nace(&["".to_owned()], 100)
            .is_err());
        assert!(repository.enterprises_for_nace(&[], 100).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn accepts_common_formats() {
        assert_eq!(
            normalize_enterprise_number("BE0200.065.765"),
            Ok("0200.065.765".to_owned())
        );
        assert_eq!(
            normalize_enterprise_number("0200065765"),
            Ok("0200.065.765".to_owned())
        );
    }

    #[test]
    fn pads_legacy_nine_digit_value() {
        assert_eq!(
            normalize_enterprise_number("200.065.765"),
            Ok("0200.065.765".to_owned())
        );
    }

    #[test]
    fn rejects_invalid_values() {
        assert!(normalize_enterprise_number("1234").is_err());
    }
}
