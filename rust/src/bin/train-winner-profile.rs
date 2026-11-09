//! Build `rust/data/winner_profile.json` from award history + KBO.
//!
//! ```bash
//! just train-winner-profile
//! ```
//!
//! Reads linked, high-confidence, unambiguous history rows (exact and
//! normalized joins only — member rows describe consortium mates, not
//! winners), resolves each winner's NACE divisions from the snapshot, and
//! writes smoothed log-odds tables with provenance. Deterministic: same
//! databases, same bytes out.

use aanbestedingen_matcher::winner_profile::build_logodds;
use std::collections::{BTreeSet, HashMap};
use std::env;

#[derive(Debug, PartialEq)]
struct TrainOptions {
    database: String,
    kbo_database: String,
    output: String,
}

fn parse_args<I>(args: I) -> Result<Option<TrainOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut output = "rust/data/winner_profile.json".to_owned();
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?
            }
            "--output" => output = arguments.next().ok_or("--output needs a value")?,
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => return Err(format!("unexpected positional: {argument}").into()),
        }
    }
    Ok(Some(TrainOptions {
        database,
        kbo_database,
        output,
    }))
}

fn division_of(code: &str) -> Option<String> {
    let trimmed = code.trim();
    (!trimmed.is_empty()).then(|| trimmed.get(..2).unwrap_or(trimmed).to_owned())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: train-winner-profile [--database PATH] [--kbo-database PATH] [--output PATH]"
        );
        return Ok(());
    };
    let TrainOptions {
        database,
        kbo_database,
        output,
    } = options;
    let store = duckdb::Connection::open_with_flags(
        database.as_str(),
        duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
    )?;
    let mut statement = store.prepare(
        "SELECT winner_enterprise, cpv, year FROM ted_awards_history \
         WHERE winner_enterprise IS NOT NULL \
         AND join_confidence IN ('exact', 'normalized') \
         AND ambiguous = false AND cpv IS NOT NULL",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut history: Vec<(String, String, i64)> = Vec::new();
    for row in rows {
        history.push(row?);
    }
    println!("linked history rows: {}", history.len());
    let winners: BTreeSet<&str> = history
        .iter()
        .map(|(enterprise, _, _)| enterprise.as_str())
        .collect();
    let kbo = duckdb::Connection::open_with_flags(
        kbo_database.as_str(),
        duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
    )?;
    let placeholders = winners.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT DISTINCT EntityNumber, NaceCode FROM kbo_activity WHERE EntityNumber IN ({placeholders})"
    );
    let mut statement = kbo.prepare(&sql)?;
    let params: Vec<&dyn duckdb::ToSql> = winners
        .iter()
        .map(|enterprise| enterprise as &dyn duckdb::ToSql)
        .collect();
    let nace_rows = statement.query_map(params.as_slice(), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut profile: HashMap<String, BTreeSet<String>> = HashMap::new();
    for row in nace_rows {
        let (enterprise, code) = row?;
        if let Some(division) = division_of(&code) {
            profile.entry(enterprise).or_default().insert(division);
        }
    }
    println!("winners with NACE divisions: {}", profile.len());
    let mut pairs: Vec<(Vec<String>, String)> = Vec::new();
    let mut cpv_support: BTreeSet<String> = BTreeSet::new();
    let mut years: BTreeSet<i64> = BTreeSet::new();
    for (enterprise, cpv, year) in &history {
        let Some(cpv_division) = division_of(cpv) else {
            continue;
        };
        let Some(divisions) = profile.get(enterprise) else {
            continue;
        };
        if divisions.is_empty() {
            continue;
        }
        cpv_support.insert(cpv_division.clone());
        years.insert(*year);
        pairs.push((divisions.iter().cloned().collect(), cpv_division));
    }
    println!("training pairs: {}", pairs.len());
    let tables = build_logodds(&pairs, cpv_support.len());
    let document = serde_json::json!({
        "provenance": {
            "built_by": "train-winner-profile",
            "history_rows": history.len(),
            "training_pairs": pairs.len(),
            "years": years.into_iter().collect::<Vec<_>>(),
            "filter": "join_confidence in (exact, normalized), unambiguous, cpv present, winner with NACE",
        },
        "cpv_divisions": cpv_support.len(),
        "logodds": tables,
    });
    std::fs::write(&output, serde_json::to_string_pretty(&document)?)?;
    println!("wrote {output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> std::vec::IntoIter<String> {
        values
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn defaults_point_at_repo_data() {
        let options = parse_args(arguments(&[]))
            .expect("args should parse")
            .expect("options should exist");
        assert_eq!(options.database, "data/tenders-rust.duckdb");
        assert_eq!(options.output, "rust/data/winner_profile.json");
    }

    #[test]
    fn rejects_positionals_and_unknown_flags() {
        assert!(parse_args(arguments(&["x.csv"])).is_err());
        assert!(parse_args(arguments(&["--bogus"])).is_err());
    }
}
