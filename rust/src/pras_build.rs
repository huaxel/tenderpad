use duckdb::Connection;
use std::collections::HashMap;
use std::error::Error;
#[cfg(test)]
use std::fs;
use std::path::Path;

pub const DEFAULT_GEOJSON: &str = "data/kbo/pras_affectations.geojson";
pub const DEFAULT_GPKG: &str = "data/kbo/BeStBrussels_31370_GPKG_04000.gpkg";
pub const DEFAULT_OUTPUT: &str = "data/kbo/pras_enrich.duckdb";
pub const EXPECTED_ZONES: usize = 7_954;

fn sql_escape(value: &str) -> String {
    value.replace('\'', "''")
}

pub fn build(
    geojson: impl AsRef<Path>,
    output: impl AsRef<Path>,
    force: bool,
    expected_zones: usize,
    gpkg: Option<impl AsRef<Path>>,
) -> Result<HashMap<String, usize>, Box<dyn Error>> {
    let geojson = geojson.as_ref();
    let output = output.as_ref();

    if !geojson.exists() {
        return Err(format!("missing GeoJSON export: {}", geojson.display()).into());
    }

    let gpkg_ref = gpkg.as_ref().map(|p| p.as_ref());
    if let Some(gpkg_path) = gpkg_ref {
        if !gpkg_path.exists() {
            return Err(format!("missing BeStBrussels Geopackage: {}", gpkg_path.display()).into());
        }
    }

    let mut inputs = vec![geojson];
    if let Some(gpkg) = gpkg_ref {
        inputs.push(gpkg);
    }
    let staging_owner = crate::rebuild::Rebuild::new(output, &inputs, force)?;
    let connection = Connection::open(staging_owner.path())?;
    connection.execute_batch("INSTALL spatial; LOAD spatial;")?;

    let geojson_str = geojson
        .to_str()
        .ok_or("GeoJSON path contains invalid UTF-8")?;
    let escaped_geojson = sql_escape(geojson_str);

    connection.execute_batch(&format!(
        r#"
        CREATE TABLE pras_zones AS SELECT
            id,
            AFFECTATION,
            NAME_FR,
            NAME_NL,
            geom::GEOMETRY AS geometry
        FROM ST_Read('{escaped_geojson}');
        "#
    ))?;

    let zone_count: usize =
        connection.query_row("SELECT count(*) FROM pras_zones", [], |row| row.get(0))?;

    if zone_count != expected_zones {
        return Err(format!("expected {expected_zones} zones, got {zone_count}").into());
    }

    let mut counts = HashMap::new();
    counts.insert("pras_zones".to_owned(), zone_count);

    if let Some(gpkg_path) = gpkg_ref {
        let gpkg_str = gpkg_path
            .to_str()
            .ok_or("GPKG path contains invalid UTF-8")?;
        let escaped_gpkg = sql_escape(gpkg_str);

        connection.execute_batch(&format!(
            r#"
            CREATE TABLE be_streets AS SELECT
                objectidentifier,
                spelling_fr AS name_fr,
                spelling_nl AS name_nl,
                status
            FROM ST_Read('{escaped_gpkg}', layer='BrusselsStreetname_04000');

            CREATE TABLE be_addrs AS SELECT
                objectidentifier AS soid,
                hasstreetname_objectidentifier AS street_oid,
                housenumber AS house_number,
                TRY_CAST(split_part(housenumber, '/', 1) AS INTEGER)
                    AS house_number_int,
                boxnumber AS box,
                haspostalinfo_objectidentifier AS zipcode,
                hasmunicipality_objectidentifier AS municipality_oid,
                status,
                x,
                y,
                geom::GEOMETRY AS geometry
            FROM ST_Read('{escaped_gpkg}', layer='BrusselsAddressL72_04000');

            CREATE INDEX idx_be_streets_oid ON be_streets(objectidentifier);
            CREATE INDEX idx_be_addrs_soid ON be_addrs(soid);
            CREATE INDEX idx_be_addrs_street ON be_addrs(street_oid);
            "#
        ))?;

        let street_count: usize =
            connection.query_row("SELECT count(*) FROM be_streets", [], |row| row.get(0))?;
        let addr_count: usize =
            connection.query_row("SELECT count(*) FROM be_addrs", [], |row| row.get(0))?;

        counts.insert("be_streets".to_owned(), street_count);
        counts.insert("be_addrs".to_owned(), addr_count);
    }

    connection.execute_batch("CHECKPOINT")?;
    drop(connection);
    staging_owner.publish()?;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
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

    fn write_geojson(path: &Path, content: &str) {
        let mut file = File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    const SAMPLE_GEOJSON: &str = r#"{
      "type": "FeatureCollection",
      "features": [
        {
          "type": "Feature",
          "id": "Affectations.1",
          "geometry": {
            "type": "MultiPolygon",
            "coordinates": [[[[0.0, 0.0], [0.0, 10.0], [10.0, 10.0], [10.0, 0.0], [0.0, 0.0]]]]
          },
          "properties": {
            "AFFECTATION": "AfZHabitat",
            "NAME_FR": "habitation",
            "NAME_NL": "wonen"
          }
        },
        {
          "type": "Feature",
          "id": "Affectations.2",
          "geometry": {
            "type": "MultiPolygon",
            "coordinates": [[[[20.0, 20.0], [20.0, 30.0], [30.0, 20.0], [20.0, 20.0]]]]
          },
          "properties": {
            "AFFECTATION": "AfZPark",
            "NAME_FR": "parc",
            "NAME_NL": "park"
          }
        }
      ]
    }"#;

    #[test]
    fn test_build_projects_zones_with_geometry() {
        let temp_dir = TempDir::new("pras-test");
        let geojson = temp_dir.path().join("zones.geojson");
        write_geojson(&geojson, SAMPLE_GEOJSON);
        let output = temp_dir.path().join("out.duckdb");

        let counts = build(&geojson, &output, false, 2, Option::<&Path>::None).unwrap();
        assert_eq!(counts.get("pras_zones"), Some(&2));

        let con = Connection::open(&output).unwrap();
        con.execute_batch("LOAD spatial;").unwrap();
        let mut stmt = con
            .prepare(
                "SELECT id, AFFECTATION, NAME_FR, NAME_NL, ST_Area(geometry), ST_GeometryType(geometry) FROM pras_zones ORDER BY id",
            )
            .unwrap();

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            rows,
            vec![
                (
                    "Affectations.1".to_string(),
                    "AfZHabitat".to_string(),
                    "habitation".to_string(),
                    "wonen".to_string(),
                    100.0,
                    "MULTIPOLYGON".to_string(),
                ),
                (
                    "Affectations.2".to_string(),
                    "AfZPark".to_string(),
                    "parc".to_string(),
                    "park".to_string(),
                    50.0,
                    "MULTIPOLYGON".to_string(),
                ),
            ]
        );
    }

    #[test]
    fn audit_failed_rebuild_preserves_previous_database() {
        let dir = TempDir::new("pras-preserve");
        let input = dir.path().join("zones.geojson");
        let output = dir.path().join("out.duckdb");
        write_geojson(&input, SAMPLE_GEOJSON);
        build(&input, &output, false, 2, Option::<&Path>::None).unwrap();
        let before = fs::read(&output).unwrap();
        write_geojson(&input, &SAMPLE_GEOJSON.replace("habitation", "changed"));
        assert!(build(&input, &output, true, 3, Option::<&Path>::None).is_err());
        assert_eq!(fs::read(&output).unwrap(), before);
    }

    #[test]
    fn audit_pras_rejects_output_input_aliases() {
        let dir = TempDir::new("pras-alias");
        let input = dir.path().join("zones.geojson");
        write_geojson(&input, SAMPLE_GEOJSON);
        assert!(build(&input, &input, true, 2, Option::<&Path>::None).is_err());
        assert_eq!(fs::read_to_string(&input).unwrap(), SAMPLE_GEOJSON);
        let gpkg = dir.path().join("input.gpkg");
        fs::write(&gpkg, b"must survive").unwrap();
        assert!(build(&input, &gpkg, true, 2, Some(&gpkg)).is_err());
        assert_eq!(fs::read(&gpkg).unwrap(), b"must survive");
    }

    #[test]
    fn test_zone_count_guard_rejects_partial_exports() {
        let temp_dir = TempDir::new("pras-test");
        let geojson = temp_dir.path().join("zones.geojson");
        let single_feature = r#"{
          "type": "FeatureCollection",
          "features": [
            {
              "type": "Feature",
              "id": "Affectations.1",
              "geometry": {
                "type": "MultiPolygon",
                "coordinates": [[[[0.0, 0.0], [0.0, 10.0], [10.0, 10.0], [10.0, 0.0], [0.0, 0.0]]]]
              },
              "properties": {
                "AFFECTATION": "AfZHabitat",
                "NAME_FR": "habitation",
                "NAME_NL": "wonen"
              }
            }
          ]
        }"#;
        write_geojson(&geojson, single_feature);
        let output = temp_dir.path().join("out.duckdb");

        let err = build(&geojson, &output, false, 2, Option::<&Path>::None).unwrap_err();
        assert!(err.to_string().contains("expected 2 zones, got 1"));
    }

    #[test]
    fn test_refuses_to_overwrite_without_force() {
        let temp_dir = TempDir::new("pras-test");
        let geojson = temp_dir.path().join("zones.geojson");
        write_geojson(&geojson, SAMPLE_GEOJSON);
        let output = temp_dir.path().join("out.duckdb");

        build(&geojson, &output, false, 2, Option::<&Path>::None).unwrap();
        let err = build(&geojson, &output, false, 2, Option::<&Path>::None).unwrap_err();
        assert!(err.to_string().contains("pass force=True"));

        let counts = build(&geojson, &output, true, 2, Option::<&Path>::None).unwrap();
        assert_eq!(counts.get("pras_zones"), Some(&2));
    }

    #[test]
    fn test_missing_geojson_fails_fast() {
        let temp_dir = TempDir::new("pras-test");
        let geojson = temp_dir.path().join("absent.geojson");
        let output = temp_dir.path().join("out.duckdb");

        let err = build(&geojson, &output, false, 2, Option::<&Path>::None).unwrap_err();
        assert!(err.to_string().contains("missing GeoJSON export"));
    }

    #[test]
    fn test_missing_gpkg_fails_fast() {
        let temp_dir = TempDir::new("pras-test");
        let geojson = temp_dir.path().join("zones.geojson");
        write_geojson(&geojson, SAMPLE_GEOJSON);
        let output = temp_dir.path().join("out.duckdb");
        let absent_gpkg = temp_dir.path().join("absent.gpkg");

        let err = build(&geojson, &output, false, 2, Some(&absent_gpkg)).unwrap_err();
        assert!(err.to_string().contains("missing BeStBrussels Geopackage"));
    }
}
