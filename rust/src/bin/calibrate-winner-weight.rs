//! Calibrate the winner-profile weight on held-out awards.
//!
//! ```bash
//! just calibrate-winner-weight
//! ```
//!
//! Retrieval task, no humans: for each held-out award tender, rank the true
//! winner among K random enterprises with the real scorer and measure MRR /
//! top-5 rate at each candidate weight. Totals for weight w are recomputed
//! arithmetically from one scoring pass — language and exclusion adjustments
//! are constant within a tender (same tender, default preferences), so only
//! the fit points are rescaled: `(1-w)*fit + w*100*signal`. Scores within a
//! tender never approach the 100 cap in this setup (asserted); if that ever
//! trips, the arithmetic needs the cap mirrored and this binary must fail
//! loudly rather than rank on wrong totals.

use aanbestedingen_matcher::kbo::KboRepository;
use aanbestedingen_matcher::{score_breakdown, CompanyPreferences, Tender};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;

const WEIGHTS: [f64; 6] = [0.0, 0.02, 0.05, 0.08, 0.12, 0.18];

#[derive(Debug, PartialEq)]
struct CalibrateOptions {
    database: String,
    kbo_database: String,
    tenders: usize,
    candidates: usize,
    seed: u64,
}

fn parse_args<I>(args: I) -> Result<Option<CalibrateOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut tenders = 400_usize;
    let mut candidates = 20_usize;
    let mut seed = 42_u64;
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?
            }
            "--tenders" => {
                tenders = arguments
                    .next()
                    .ok_or("--tenders needs a value")?
                    .parse()
                    .map_err(|_| "--tenders must be an integer")?
            }
            "--candidates" => {
                candidates = arguments
                    .next()
                    .ok_or("--candidates needs a value")?
                    .parse()
                    .map_err(|_| "--candidates must be an integer")?
            }
            "--seed" => {
                seed = arguments
                    .next()
                    .ok_or("--seed needs a value")?
                    .parse()
                    .map_err(|_| "--seed must be an integer")?
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => return Err(format!("unexpected positional: {argument}").into()),
        }
    }
    Ok(Some(CalibrateOptions {
        database,
        kbo_database,
        tenders,
        candidates,
        seed,
    }))
}

fn failure(message: &str) -> Box<dyn std::error::Error> {
    format!("KBO lookup failed: {message}").into()
}

/// Deterministic xorshift64* — no rand dependency for a sampling helper.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: calibrate-winner-weight [--database PATH] [--kbo-database PATH] \
             [--tenders N] [--candidates K] [--seed S]"
        );
        return Ok(());
    };
    let CalibrateOptions {
        database,
        kbo_database,
        tenders: tender_limit,
        candidates: candidate_count,
        seed,
    } = options;
    let store = duckdb::Connection::open_with_flags(
        database.as_str(),
        duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?,
    )?;
    let mut statement = store.prepare(
        "SELECT winner_enterprise, cpv, CAST(award_value AS VARCHAR), buyer_name FROM ted_awards_history \
         WHERE year >= 2020 AND winner_enterprise IS NOT NULL \
         AND join_confidence IN ('exact', 'normalized') AND ambiguous = false \
         AND cpv IS NOT NULL",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut awards: Vec<(String, String, Option<String>, Option<String>)> = Vec::new();
    for row in rows {
        awards.push(row?);
    }
    let pool: Vec<String> = {
        let mut set = BTreeSet::new();
        for (enterprise, _, _, _) in &awards {
            set.insert(enterprise.clone());
        }
        set.into_iter().collect()
    };
    println!(
        "held-out awards: {}; candidate pool: {} enterprises",
        awards.len(),
        pool.len()
    );
    let repository = KboRepository::open(kbo_database)?;
    // Same-sector candidate map: enterprises sharing at least one NACE
    // division. Ranking the winner among lookalikes is the informative
    // task — random candidates are already perfectly separated by the
    // activity signal, so any weight only perturbs a perfect ranking.
    let mut sector: HashMap<String, BTreeSet<String>> = HashMap::new();
    for enterprise in &pool {
        let divisions = match repository.lookup(enterprise) {
            Ok(Some(profile)) => profile
                .activities
                .iter()
                .filter_map(|activity| {
                    let code = activity.code.trim();
                    (!code.is_empty()).then(|| code.get(..2).unwrap_or(code).to_owned())
                })
                .collect::<BTreeSet<_>>(),
            _ => BTreeSet::new(),
        };
        for division in divisions {
            sector
                .entry(division)
                .or_default()
                .insert(enterprise.clone());
        }
    }
    let preferences = CompanyPreferences::default();
    let history: Vec<aanbestedingen_matcher::AwardRecord> = Vec::new();
    let mut rng = Rng(seed.max(1));
    let mut reciprocal_ranks: BTreeMap<String, f64> = BTreeMap::new();
    let mut top_five: BTreeMap<String, usize> = BTreeMap::new();
    let mut scored_tenders = 0_usize;
    let mut capped = 0_usize;
    let mut tied_tenders = 0_usize;
    let mut tender_index = 0_usize;
    // Sequential scan with wraparound: deterministic coverage without
    // shuffling the whole table into memory.
    while scored_tenders < tender_limit && tender_index < awards.len() {
        let (winner, cpv, value, buyer) = &awards[tender_index % awards.len()];
        tender_index += 1;
        let winner_profile = match repository.lookup(winner) {
            Ok(Some(profile)) => profile,
            Ok(None) => continue,
            Err(error) => return Err(failure(&error.to_string())),
        };
        let winner_divisions: BTreeSet<String> = winner_profile
            .activities
            .iter()
            .filter_map(|activity| {
                let code = activity.code.trim();
                (!code.is_empty()).then(|| code.get(..2).unwrap_or(code).to_owned())
            })
            .collect();
        // Same-sector lookalikes first, random fill only when the sector
        // is too small to field a full roster.
        let mut lookalikes: Vec<&String> = winner_divisions
            .iter()
            .filter_map(|division| sector.get(division))
            .flatten()
            .filter(|enterprise| *enterprise != winner)
            .collect();
        lookalikes.sort();
        lookalikes.dedup();
        let mut roster = vec![winner_profile];
        let mut seen = BTreeSet::from([winner.clone()]);
        let mut cursor = 0_usize;
        while roster.len() <= candidate_count && cursor < lookalikes.len() {
            // Deterministic rotation through the sector roster.
            let candidate = lookalikes[(cursor + rng.below(lookalikes.len())) % lookalikes.len()];
            cursor += 1;
            if !seen.insert(candidate.clone()) {
                continue;
            }
            match repository.lookup(candidate) {
                Ok(Some(profile)) => roster.push(profile),
                Ok(None) => {}
                Err(error) => return Err(failure(&error.to_string())),
            }
        }
        let mut draws = 0_usize;
        while roster.len() <= candidate_count && draws < candidate_count * 50 {
            draws += 1;
            let candidate = &pool[rng.below(pool.len())];
            if !seen.insert(candidate.clone()) {
                continue;
            }
            match repository.lookup(candidate) {
                Ok(Some(profile)) => roster.push(profile),
                Ok(None) => {}
                Err(error) => return Err(failure(&error.to_string())),
            }
        }
        if roster.len() <= candidate_count {
            continue;
        }
        let tender = Tender {
            source: "calibration".to_owned(),
            external_id: format!("cal-{scored_tenders}"),
            title: String::new(),
            buyer: buyer.clone(),
            cpv_codes: vec![cpv.clone()],
            estimated_value: value
                .as_deref()
                .and_then(|text| text.replace(',', ".").parse().ok()),
            notice_type: Some("cn-standard".to_owned()),
            ..Tender::default()
        };
        // One scoring pass per candidate; weight variants recomputed below.
        let mut fits: Vec<(f64, f64)> = Vec::new();
        for company in &roster {
            let breakdown = score_breakdown(company, &tender, &history, &preferences);
            let fit_points =
                breakdown.total - breakdown.language_points - breakdown.excluded_term_points;
            if breakdown.total > 99.0 {
                capped += 1;
            }
            fits.push((fit_points, breakdown.winner_profile.signal));
        }
        // Tie diagnostic: a stable descending sort parks index 0 (the
        // winner) first on full ties, which would fake a perfect MRR.
        let distinct_base: BTreeSet<u64> = fits
            .iter()
            .map(|(fit, _)| (fit * 1_000_000.0).round() as u64)
            .collect();
        if distinct_base.len() < fits.len() {
            tied_tenders += 1;
        }
        for weight in WEIGHTS {
            let mut totals: Vec<(f64, bool)> = fits
                .iter()
                .enumerate()
                .map(|(index, (fit, signal))| {
                    ((1.0 - weight) * fit + weight * 100.0 * signal, index == 0)
                })
                .collect();
            totals.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let rank = totals
                .iter()
                .position(|(_, is_winner)| *is_winner)
                .map(|position| position + 1)
                .unwrap_or(totals.len());
            let key = format!("{weight:.2}");
            *reciprocal_ranks.entry(key.clone()).or_default() += 1.0 / rank as f64;
            if rank <= 5 {
                *top_five.entry(key).or_default() += 1;
            }
        }
        scored_tenders += 1;
    }
    assert!(
        capped == 0,
        "score cap binds on {capped} candidates; weight arithmetic is invalid"
    );
    println!("scored tenders: {scored_tenders}");
    println!("tenders with tied base totals: {tied_tenders}");
    // A tied base parks the winner first under the stable sort, so a
    // perfect weight-0 MRR on all-tied tenders measures sort order, not
    // discrimination. The verdict is only meaningful when totals vary.
    if tied_tenders == scored_tenders && scored_tenders > 0 {
        println!(
            "verdict: KEEP 0.0 — base totals tie on synthetic tenders, so no marginal weight value is measurable here; nonzero weights only perturb ties. Reopen with real outcome pairs."
        );
    }
    for weight in WEIGHTS {
        let key = format!("{weight:.2}");
        let mrr = reciprocal_ranks.get(&key).copied().unwrap_or(0.0) / scored_tenders as f64;
        let top5 = top_five.get(&key).copied().unwrap_or(0);
        println!(
            "weight {key}: MRR {mrr:.4} top-5 {top5}/{scored_tenders} ({:.1}%)",
            100.0 * top5 as f64 / scored_tenders as f64,
        );
    }
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
    fn defaults_select_held_out_task() {
        let options = parse_args(arguments(&[]))
            .expect("args should parse")
            .expect("options should exist");
        assert_eq!(options.tenders, 400);
        assert_eq!(options.candidates, 20);
        assert_eq!(options.seed, 42);
    }

    #[test]
    fn rng_is_deterministic() {
        let mut first = Rng(42);
        let mut second = Rng(42);
        for _ in 0..10 {
            assert_eq!(first.next(), second.next());
        }
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(arguments(&["--bogus"])).is_err());
        assert!(parse_args(arguments(&["positional"])).is_err());
    }
}
