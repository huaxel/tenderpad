//! Reverse search: rank KBO companies for one stored tender (buyer-side
//! prospecting: "who could bid on this?").
//!
//! ```text
//! reverse-match bda d7599c62-43bb-4ba7-8841-baeb214fb7e2 --top 20
//! ```
//!
//! The candidate pool derives from the tender's CPV codes through the same
//! CPV–NACE evidence ladder the forward activity match climbs (exact classes
//! where known, else division prefixes), nationwide; `--nace` overrides it
//! explicitly, which is also the escape hatch for tenders carrying no CPV
//! codes at all. `--region` hard-filters to a zip prefix (`1` is all of
//! Brussels) or a commune-name substring; the tender's own place of
//! performance additionally orders ties through the region signal, when the
//! import captured one.
//!
//! Scores are exactly what the forward ranking would compute for each pair
//! (same `rank` function, plus stored preferences/history/feedback for the
//! few judged companies); the order is deterministic: score, topical text
//! overlap, region signal, enterprise number. Read-only.

use aanbestedingen_matcher::analysis::{
    analysis_cache_key, analyze, ConfiguredAnalyzer, DETERMINISTIC_CACHE_IDENTITY,
};
use aanbestedingen_matcher::kbo::KboRepository;
use aanbestedingen_matcher::recommendations::{
    apply_prospect_decision, company_matches_region_filter, nace_prefixes_for_tender,
    rank_prospects, sort_prospects, CompanyProspect, ProspectContext, DEFAULT_PROSPECT_CANDIDATES,
    MAX_PROSPECT_CANDIDATES,
};
use aanbestedingen_matcher::store::TenderStore;
use aanbestedingen_matcher::{AwardRecord, CompanyPreferences, CompanyProfile};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::env;

#[derive(Debug, PartialEq)]
struct Options {
    source: String,
    external_id: String,
    database: String,
    kbo_database: String,
    top: usize,
    candidates: usize,
    nace: Vec<String>,
    region: Option<String>,
    deep: usize,
}

fn parse_nace_list(value: &str) -> Result<Vec<String>, String> {
    let mut prefixes = Vec::new();
    for part in value.split(',') {
        let prefix = part.trim().to_owned();
        if prefix.is_empty()
            || prefix.len() > 5
            || !prefix.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(format!("--nace takes 1-5 digit prefixes, got {part:?}"));
        }
        prefixes.push(prefix);
    }
    if prefixes.is_empty() {
        return Err("--nace needs at least one prefix".to_owned());
    }
    Ok(prefixes)
}

fn parse_args<I>(args: I) -> Result<Option<Options>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut top = 20_usize;
    let mut candidates = DEFAULT_PROSPECT_CANDIDATES;
    let mut nace: Vec<String> = Vec::new();
    let mut region: Option<String> = None;
    let mut deep = 0_usize;
    let mut positional: Vec<String> = Vec::new();
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--kbo-database" => {
                kbo_database = args_next(&mut arguments, "--kbo-database")?;
            }
            "--top" => {
                top = args_next(&mut arguments, "--top")?.parse()?;
                if top == 0 {
                    return Err("--top must be at least 1".into());
                }
            }
            "--candidates" => {
                candidates = args_next(&mut arguments, "--candidates")?.parse()?;
                if candidates == 0 || candidates > MAX_PROSPECT_CANDIDATES {
                    return Err(format!(
                        "--candidates must be between 1 and {MAX_PROSPECT_CANDIDATES}"
                    )
                    .into());
                }
            }
            "--nace" => nace = parse_nace_list(&args_next(&mut arguments, "--nace")?)?,
            "--region" => region = Some(args_next(&mut arguments, "--region")?),
            "--deep" => {
                deep = args_next(&mut arguments, "--deep")?.parse()?;
                if deep > 10 {
                    return Err("--deep must be between 0 and 10".into());
                }
            }
            "--help" | "-h" => return Ok(None),
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other}").into())
            }
            value => positional.push(value.to_owned()),
        }
    }
    if positional.len() != 2 {
        return Err("usage: reverse-match SOURCE EXTERNAL_ID [--top N] [--candidates N] [--nace 62,63] [--region 1]".into());
    }
    Ok(Some(Options {
        source: positional[0].clone(),
        external_id: positional[1].clone(),
        database,
        kbo_database,
        top,
        candidates,
        nace,
        region,
        deep,
    }))
}

fn args_next<I>(args: &mut I, flag: &str) -> Result<String, Box<dyn std::error::Error>>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| format!("{flag} needs a value").into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: reverse-match SOURCE EXTERNAL_ID [--top 20]");
        println!("       [--candidates 20000] [--nace 62,63] [--region 1] [--deep N]");
        println!("       [--database PATH] [--kbo-database PATH]");
        println!();
        println!("Ranks KBO companies for one stored tender. The pool derives");
        println!("from the tender's CPV codes unless --nace overrides it.");
        return Ok(());
    };

    // Writing fresh deep decisions needs a writable handle, but a missing
    // file must never become an empty shortlist: that is a path typo, not
    // "no matching companies".
    if !std::path::Path::new(&options.database).exists() {
        return Err(format!("tender database {} does not exist", options.database).into());
    }
    let store = if options.deep > 0 {
        TenderStore::open(&options.database)?
    } else {
        TenderStore::open_read_only(&options.database)?
    };
    let tender = store
        .tender(&options.source, &options.external_id)?
        .ok_or("tender not found in this database")?;
    let prefixes = if options.nace.is_empty() {
        let derived = nace_prefixes_for_tender(&tender);
        if derived.is_empty() {
            return Err("the tender carries no CPV codes, so no NACE pool can be derived: pass --nace explicitly (e.g. --nace 62,63)".into());
        }
        derived
    } else {
        options.nace.clone()
    };

    let repository = KboRepository::open(&options.kbo_database)?;
    let (pool, pool_total) = repository.enterprises_for_nace(&prefixes, options.candidates)?;
    eprintln!(
        "pool: {} of {} enterprises for NACE prefixes [{}]",
        pool.len(),
        pool_total,
        prefixes.join(", ")
    );
    let mut companies = repository.lookup_batch(&pool)?;
    if let Some(region) = &options.region {
        companies.retain(|company| company_matches_region_filter(company, region));
        eprintln!("pool after --region {region}: {}", companies.len());
    }
    // Stored ranking context is rare: resolve it only for judged companies,
    // with defaults for everyone else (identical scores, three queries saved
    // per unjudged candidate).
    let judged: HashSet<String> = store.enterprises_with_context()?;
    let mut contexts: HashMap<String, ProspectContext> = HashMap::new();
    for company in &companies {
        if judged.contains(&company.enterprise_number) {
            let preferences: CompanyPreferences =
                store.preferences_for(&company.enterprise_number)?;
            let history: Vec<AwardRecord> = store.awards_for(&company.enterprise_number)?;
            let feedback = store.feedback_for(&company.enterprise_number)?;
            contexts.insert(
                company.enterprise_number.clone(),
                (preferences, history, feedback),
            );
        }
    }
    if !contexts.is_empty() {
        eprintln!("stored context applied to {} companies", contexts.len());
    }
    let mut prospects: Vec<CompanyProspect> = rank_prospects(&companies, &tender, &contexts);
    if options.deep > 0 {
        // Analyzer rerank over the head of the baseline list, mirroring
        // `recommend --deep`: fresh decisions persist in the analysis cache.
        let analyzer = ConfiguredAnalyzer::from_env()?;
        let provider = analyzer
            .as_ref()
            .map(ConfiguredAnalyzer::cache_identity)
            .unwrap_or_else(|| DETERMINISTIC_CACHE_IDENTITY.to_owned());
        let companies_by_id: HashMap<&str, &CompanyProfile> = companies
            .iter()
            .map(|company| (company.enterprise_number.as_str(), company))
            .collect();
        let default_context: ProspectContext =
            (CompanyPreferences::default(), Vec::new(), HashMap::new());
        for prospect in prospects.iter_mut().take(options.deep) {
            let Some(company) = companies_by_id.get(prospect.enterprise_number.as_str()) else {
                continue;
            };
            let (preferences, history, _) = contexts
                .get(&company.enterprise_number)
                .unwrap_or(&default_context);
            let cache_key = analysis_cache_key(&provider, company, &tender, history, preferences);
            let mut decision = store.cached_analysis(&cache_key)?;
            if decision.is_none() {
                let fresh = if let Some(remote) = analyzer.as_ref() {
                    remote.analyze(company, &tender, history, preferences)?
                } else {
                    analyze(company, &tender, history, preferences)
                };
                store.save_analysis(&cache_key, &fresh)?;
                decision = Some(fresh);
            }
            apply_prospect_decision(
                prospect,
                &decision.expect("a decision is set on every path"),
            );
        }
        sort_prospects(&mut prospects);
    }
    let shown: Vec<serde_json::Value> = prospects
        .into_iter()
        .take(options.top)
        .map(|prospect| {
            json!({
                "enterprise": prospect.enterprise_number,
                "name": prospect.name,
                "nace": prospect.nace_codes,
                "municipality": prospect.municipality,
                "zip": prospect.zip,
                "score": prospect.score,
                "confidence": prospect.confidence,
                "text_score": prospect.text_score,
                "region_signal": prospect.region_signal,
                "reasons": prospect.reasons,
                "warnings": prospect.warnings,
            })
        })
        .collect();
    let mut notes = vec![
        "order is score, topical text overlap, region signal, enterprise number — deterministic, never insertion order".to_owned(),
    ];
    if tender.regions.is_empty() {
        notes.push(
            "the tender states no place of performance, so the region tie-break is neutral for every company".to_owned(),
        );
    }
    if pool_total > options.candidates {
        notes.push(format!(
            "pool capped at {} of {} enterprises (lowest numbers first, i.e. oldest registrations): raise --candidates for full recall",
            options.candidates, pool_total
        ));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "tender": {
                "source": tender.source,
                "external_id": tender.external_id,
                "title": tender.title,
                "buyer": tender.buyer,
                "deadline": tender.deadline,
                "cpv_codes": tender.cpv_codes,
            },
            "pool": {
                "nace_prefixes": prefixes,
                "pool_total": pool_total,
                "pool_scored": companies.len(),
                "pool_capped_at": options.candidates,
            },
            "prospects": shown,
            "notes": notes,
        }))?
    );
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
    fn takes_a_tender_and_optional_pool_flags() {
        let options = parse_args(arguments(&[
            "bda", "notice-1", "--top", "5", "--nace", "62,63", "--region", "1",
        ]))
        .expect("arguments should parse")
        .expect("options");
        assert_eq!(options.source, "bda");
        assert_eq!(options.external_id, "notice-1");
        assert_eq!(options.top, 5);
        assert_eq!(options.nace, vec!["62".to_owned(), "63".to_owned()]);
        assert_eq!(options.region.as_deref(), Some("1"));
        assert_eq!(options.candidates, DEFAULT_PROSPECT_CANDIDATES);
    }

    #[test]
    fn rejects_bad_pool_arguments() {
        assert!(parse_args(arguments(&["bda"])).is_err());
        assert!(parse_args(arguments(&["bda", "x", "--top", "0"])).is_err());
        assert!(parse_args(arguments(&["bda", "x", "--candidates", "0"])).is_err());
        assert!(parse_args(arguments(&[
            "bda",
            "x",
            "--candidates",
            &(MAX_PROSPECT_CANDIDATES + 1).to_string()
        ]))
        .is_err());
        assert!(parse_args(arguments(&["bda", "x", "--nace", "62,ABCDE"])).is_err());
        assert!(parse_args(arguments(&["bda", "x", "--nace", ""])).is_err());
        assert!(parse_args(arguments(&["bda", "x", "--unknown"])).is_err());
        assert!(parse_args(arguments(&["--help"]))
            .expect("help should parse")
            .is_none());
    }

    #[test]
    fn deep_flag_is_bounded_like_recommend() {
        let options = parse_args(arguments(&["bda", "x", "--deep", "10"]))
            .expect("arguments should parse")
            .expect("options");
        assert_eq!(options.deep, 10);
        assert!(parse_args(arguments(&["bda", "x", "--deep", "11"])).is_err());
        let options = parse_args(arguments(&["bda", "x"]))
            .expect("arguments should parse")
            .expect("options");
        assert_eq!(options.deep, 0);
    }

    #[test]
    fn nace_list_parsing_trims_and_validates() {
        assert_eq!(
            parse_nace_list("62, 6310").expect("valid prefixes"),
            vec!["62".to_owned(), "6310".to_owned()]
        );
        assert!(parse_nace_list("123456").is_err());
    }
}
