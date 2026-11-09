use aanbestedingen_matcher::analysis::{
    analysis_cache_key, analyze, ConfiguredAnalyzer, DETERMINISTIC_CACHE_IDENTITY,
};
use aanbestedingen_matcher::kbo::{normalize_enterprise_number, KboRepository};
use aanbestedingen_matcher::recommendations::{
    apply_capacity_triage, apply_jev_decision, is_eligible, rank, sort_by_deadline, sort_by_score,
    CapacityLimits, DEFAULT_RECOMMENDATION_CANDIDATES, MAX_RECOMMENDATION_CANDIDATES,
};
use aanbestedingen_matcher::store::{nbb_capacity_turnover, TenderStore};
use aanbestedingen_matcher::Tender;
use chrono::Utc;
use rust_decimal::prelude::ToPrimitive;
use std::collections::HashMap;
use std::env;
use std::error::Error;

fn csv_field(value: &str) -> String {
    // Neutralize spreadsheet formula injection: quoting alone does not stop
    // Excel/LibreOffice from evaluating cells starting with =, +, -, @.
    // Prefixing with a single quote forces text interpretation; leading
    // whitespace/control characters used to dodge the check are skipped when
    // detecting the trigger but preserved in the output.
    let trimmed = value.trim_start_matches(|c: char| c.is_whitespace() || c.is_control());
    let dangerous = matches!(
        trimmed.chars().next(),
        Some('=') | Some('+') | Some('-') | Some('@') | Some('|') | Some('%')
    );
    if dangerous {
        format!("\"'{}\"", value.replace('"', "\"\""))
    } else {
        format!("\"{}\"", value.replace('"', "\"\""))
    }
}

/// Open the tender store for a run.
///
/// Writing fresh decisions needs a writable handle, but a missing file must
/// never become an empty shortlist: that is a path typo, not "no matching
/// tenders", and the difference matters to whoever reads the output.
fn open_store(database: &str, deep: usize) -> Result<TenderStore, Box<dyn Error>> {
    if !std::path::Path::new(database).exists() {
        return Err(format!("tender database {database} does not exist").into());
    }
    if deep > 0 {
        Ok(TenderStore::open(database)?)
    } else {
        Ok(TenderStore::open_read_only(database)?)
    }
}

/// Return the exact turnover input the CLI has historically used and a safe,
/// period-aware provenance note for explicit capacity triage.

#[derive(Debug, PartialEq)]
struct RecommendOptions {
    enterprise_number: String,
    database: String,
    kbo_database: String,
    limit: usize,
    candidates: usize,
    sort: String,
    format: String,
    due_within: Option<i64>,
    deep: usize,
    annual_turnover: Option<f64>,
    max_bid_capacity: Option<f64>,
    capacity_triage: bool,
    /// Opt-in: withhold stored turnover older than this many days. Unset keeps
    /// the historical behaviour of using the newest filing whatever its age.
    max_filing_age_days: Option<i64>,
}

fn validate_options(options: &RecommendOptions) -> Result<(), String> {
    if options.limit == 0 {
        return Err("--limit must be at least 1".to_owned());
    }
    if options.candidates == 0 {
        return Err("--candidates must be at least 1".to_owned());
    }
    if options.candidates > MAX_RECOMMENDATION_CANDIDATES {
        return Err(format!(
            "--candidates must be at most {MAX_RECOMMENDATION_CANDIDATES}"
        ));
    }
    if !matches!(options.sort.as_str(), "score" | "deadline") {
        return Err("--sort must be score or deadline".to_owned());
    }
    if !matches!(options.format.as_str(), "json" | "csv") {
        return Err("--format must be json or csv".to_owned());
    }
    if options
        .due_within
        .is_some_and(|days| !(0..=365).contains(&days))
    {
        return Err("--due-within must be between 0 and 365 days".to_owned());
    }
    if !(0..=10).contains(&options.deep) {
        return Err("--deep must be between 0 and 10".to_owned());
    }
    if options.annual_turnover.is_some_and(|val| val <= 0.0) {
        return Err("--annual-turnover must be positive".to_owned());
    }
    if options.max_bid_capacity.is_some_and(|val| val <= 0.0) {
        return Err("--max-bid-capacity must be positive".to_owned());
    }
    // A negative limit withholds every filing, so a typo would silently turn all
    // capacity evidence into unknown. Reject it rather than appear to work.
    if options.max_filing_age_days.is_some_and(|days| days < 0) {
        return Err("--max-filing-age-days must not be negative".to_owned());
    }
    Ok(())
}

fn print_csv(recommendations: &[aanbestedingen_matcher::recommendations::Recommendation]) {
    // Canonical CSV output fields for tender shortlists.
    println!(
        "source,external_id,title,buyer,deadline,estimated_value,regions,cpv_codes,languages,score,confidence,source_url,warnings,reasons,requirement_signals"
    );
    for item in recommendations {
        let signals = item
            .requirement_signals
            .iter()
            .map(|signal| signal.label.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let fields = [
            item.source.clone(),
            item.external_id.clone(),
            item.title.clone(),
            item.buyer.clone().unwrap_or_default(),
            item.deadline.clone().unwrap_or_default(),
            item.estimated_value
                .map(|value| value.to_string())
                .unwrap_or_default(),
            item.regions.join("; "),
            item.cpv_codes.join("; "),
            item.languages.join("; "),
            item.score.to_string(),
            item.confidence.to_string(),
            item.source_url.clone().unwrap_or_default(),
            item.warnings.join("; "),
            item.reasons.join("; "),
            signals,
        ];
        println!(
            "{}",
            fields
                .iter()
                .map(|field| csv_field(field))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
}

fn parse_args<I>(args: I) -> Result<Option<RecommendOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut raw_enterprise_number = None;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut limit = 20_usize;
    let mut candidates = DEFAULT_RECOMMENDATION_CANDIDATES;
    let mut sort = "score".to_owned();
    let mut format = "json".to_owned();
    let mut due_within = None;
    let mut deep = 0_usize;
    let mut annual_turnover = None;
    let mut max_bid_capacity = None;
    let mut capacity_triage = false;
    let mut max_filing_age_days = None;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?
            }
            "--limit" => limit = arguments.next().ok_or("--limit needs a value")?.parse()?,
            "--candidates" => {
                candidates = arguments
                    .next()
                    .ok_or("--candidates needs a value")?
                    .parse()?
            }
            "--sort" => sort = arguments.next().ok_or("--sort needs a value")?,
            "--format" => format = arguments.next().ok_or("--format needs a value")?,
            "--due-within" => {
                due_within = Some(
                    arguments
                        .next()
                        .ok_or("--due-within needs a value")?
                        .parse()?,
                )
            }
            "--deep" => deep = arguments.next().ok_or("--deep needs a value")?.parse()?,
            "--annual-turnover" => {
                annual_turnover = Some(
                    arguments
                        .next()
                        .ok_or("--annual-turnover needs a value")?
                        .parse()?,
                );
            }
            "--max-bid-capacity" => {
                max_bid_capacity = Some(
                    arguments
                        .next()
                        .ok_or("--max-bid-capacity needs a value")?
                        .parse()?,
                );
            }
            "--capacity-triage" => capacity_triage = true,
            "--max-filing-age-days" => {
                max_filing_age_days = Some(
                    arguments
                        .next()
                        .ok_or("--max-filing-age-days needs a value")?
                        .parse()?,
                );
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into());
            }
            argument => {
                if raw_enterprise_number.replace(argument.to_owned()).is_some() {
                    return Err("only one enterprise number is allowed".into());
                }
            }
        }
    }

    let enterprise_number = normalize_enterprise_number(
        &raw_enterprise_number.ok_or("enterprise number is required")?,
    )?;
    Ok(Some(RecommendOptions {
        enterprise_number,
        database,
        kbo_database,
        limit,
        candidates,
        sort,
        format,
        due_within,
        deep,
        annual_turnover,
        max_bid_capacity,
        capacity_triage,
        max_filing_age_days,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: recommend ENTERPRISE_NUMBER [--database PATH] [--kbo-database PATH] [--limit N] [--candidates N] [--sort score|deadline] [--due-within DAYS] [--deep N] [--annual-turnover EUR] [--max-bid-capacity EUR] [--capacity-triage] [--max-filing-age-days N] [--format json|csv]"
        );
        return Ok(());
    };
    validate_options(&options)?;
    let RecommendOptions {
        enterprise_number,
        database,
        kbo_database,
        limit,
        candidates,
        sort,
        format,
        due_within,
        deep,
        annual_turnover,
        max_bid_capacity,
        capacity_triage,
        max_filing_age_days,
    } = options;
    let repository = KboRepository::open(kbo_database)?;
    let company = repository
        .lookup(&enterprise_number)?
        .ok_or("company not found")?;
    let store = open_store(&database, deep)?;
    let today = Utc::now().date_naive();
    let mut tenders = store.open_tenders(candidates, today, due_within)?;
    tenders.retain(|tender| is_eligible(tender, today, due_within));
    let preferences = store.preferences_for(&company.enterprise_number)?;
    let history = store.awards_for(&company.enterprise_number)?;
    let feedback = store.feedback_for(&company.enterprise_number)?;
    let mut recommendations = rank(
        &company,
        &tenders,
        &history,
        &preferences,
        &feedback,
        tenders.len(),
    );
    let nbb_financials = if capacity_triage && annual_turnover.is_none() {
        store
            .nbb_financials_for(&company.enterprise_number)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let (stored_turnover, turnover_provenance) =
        nbb_capacity_turnover(&nbb_financials, today, options.max_filing_age_days);
    if let Some(provenance) = &turnover_provenance {
        if stored_turnover.is_none() {
            // The figure exists but was withheld, so say that rather than
            // printing a provenance line for a value that is not being used.
            let age = match provenance.age_days {
                Some(days) => format!("{days} days old as of {}", provenance.as_of),
                None => format!(
                    "undated (no period_end as of {}), so its age cannot be established",
                    provenance.as_of
                ),
            };
            let limit = match max_filing_age_days {
                Some(days) => format!("the {days} day limit"),
                None => "the configured limit".to_owned(),
            };
            eprintln!(
                "Capacity triage: stored NBB turnover for fiscal_year={} is {age}, which is not within {limit}; capacity is treated as unknown.",
                provenance.fiscal_year
            );
        } else {
            eprintln!("{}", provenance.notice_with_limit(max_filing_age_days));
        }
    }
    let effective_turnover = annual_turnover.or(stored_turnover);
    let effective_bid_cap = max_bid_capacity.or_else(|| {
        if capacity_triage {
            preferences.maximum_value.and_then(|m| m.to_f64())
        } else {
            None
        }
    });
    let capacity_limits = CapacityLimits::new(effective_turnover, effective_bid_cap);
    // Cite the filing in the barrier itself, so the demotion is auditable in the
    // JSON output and not only in this stderr line. An explicit --annual-turnover
    // wins over the stored figure, so its provenance does not apply.
    let capacity_provenance = if annual_turnover.is_none() {
        turnover_provenance.as_ref()
    } else {
        None
    };
    if capacity_triage || !capacity_limits.is_empty() {
        let tenders_by_key: HashMap<(String, String), &Tender> = tenders
            .iter()
            .map(|tender| ((tender.source.clone(), tender.external_id.clone()), tender))
            .collect();
        apply_capacity_triage(
            &mut recommendations,
            &tenders_by_key,
            capacity_limits,
            capacity_provenance,
            capacity_triage,
        );
        if sort != "deadline" {
            sort_by_score(&mut recommendations);
        }
    }
    if sort == "deadline" {
        sort_by_deadline(&mut recommendations);
    }
    if deep > 0 {
        // Jev-equivalent deep rerank. Mirrors the Python reference CLI: fresh
        // analyzer decisions persist in the analysis cache, unlike the API's
        // read-only deep mode which only reuses hits.
        let analyzer = ConfiguredAnalyzer::from_env()?;
        let provider = analyzer
            .as_ref()
            .map(ConfiguredAnalyzer::cache_identity)
            .unwrap_or_else(|| DETERMINISTIC_CACHE_IDENTITY.to_owned());
        let tenders_by_key: HashMap<(String, String), &Tender> = tenders
            .iter()
            .map(|tender| ((tender.source.clone(), tender.external_id.clone()), tender))
            .collect();
        for item in recommendations.iter_mut().take(deep) {
            let Some(tender) = tenders_by_key
                .get(&(item.source.clone(), item.external_id.clone()))
                .copied()
            else {
                continue;
            };
            let cache_key = analysis_cache_key(&provider, &company, tender, &history, &preferences);
            let mut decision = store.cached_analysis(&cache_key)?;
            if decision.is_none() {
                let fresh = if let Some(ref remote) = analyzer {
                    remote.analyze(&company, tender, &history, &preferences)?
                } else {
                    analyze(&company, tender, &history, &preferences)
                };
                store.save_analysis(&cache_key, &fresh)?;
                decision = Some(fresh);
            }
            apply_jev_decision(item, &decision.expect("a decision is set on every path"));
        }
        if sort == "deadline" {
            sort_by_deadline(&mut recommendations);
        } else {
            sort_by_score(&mut recommendations);
        }
    }
    recommendations.truncate(limit);
    if format == "csv" {
        print_csv(&recommendations);
    } else {
        println!("{}", serde_json::to_string_pretty(&recommendations)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aanbestedingen_matcher::store::NbbFinancials;
    use chrono::NaiveDate;

    #[test]
    fn a_missing_database_is_an_error_rather_than_an_empty_shortlist() {
        let missing = "/nonexistent/aanbestedingen-missing.duckdb";
        for deep in [0_usize, 5] {
            let error = match open_store(missing, deep) {
                Ok(_) => panic!("a missing database must not open as an empty store"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("does not exist"),
                "unexpected error for deep={deep}: {error}"
            );
        }
    }

    #[test]
    fn an_existing_database_opens_in_both_modes() {
        let directory = std::env::temp_dir().join(format!("aan-recommend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("tenders.duckdb");
        let path_text = path.to_str().expect("utf-8 path");
        TenderStore::open(path_text).expect("create store");

        for deep in [0_usize, 5] {
            open_store(path_text, deep).unwrap_or_else(|error| panic!("deep={deep}: {error}"));
        }

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn parses_flags_before_positional_enterprise_number() {
        let options = parse_args(
            ["--limit", "10", "--sort", "deadline", "0404484060"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.limit, 10);
        assert_eq!(options.sort, "deadline");
    }

    #[test]
    fn parses_positional_enterprise_number_before_flags() {
        let options = parse_args(
            ["0404.484.060", "--format", "csv"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number, "0404.484.060");
        assert_eq!(options.format, "csv");
    }

    #[test]
    fn accepts_help_without_enterprise_number() {
        assert!(parse_args(["--help"].into_iter().map(String::from))
            .expect("help should parse")
            .is_none());
    }

    #[test]
    fn parses_deep_flag() {
        let options = parse_args(["0404484060", "--deep", "5"].into_iter().map(String::from))
            .expect("arguments should parse")
            .expect("help was not requested");
        assert_eq!(options.deep, 5);
        assert_eq!(options.enterprise_number, "0404.484.060");
    }

    #[test]
    fn default_candidate_window_matches_the_reference_default() {
        // The CLI used to default to MAX (100_000) while the API and the Python
        // CLI default to 10_000, so the "same" command scanned ten times more.
        let options = parse_args(["0404.484.060"].into_iter().map(String::from))
            .expect("arguments should parse")
            .expect("help was not requested");
        assert_eq!(options.candidates, DEFAULT_RECOMMENDATION_CANDIDATES);
    }

    #[test]
    fn parses_capacity_flags() {
        let options = parse_args(
            [
                "0404484060",
                "--annual-turnover",
                "850000",
                "--max-bid-capacity",
                "350000",
                "--capacity-triage",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.annual_turnover, Some(850_000.0));
        assert_eq!(options.max_bid_capacity, Some(350_000.0));
        assert!(options.capacity_triage);
    }

    #[test]
    fn stored_turnover_comes_from_the_current_period_not_a_comparative_one() {
        // End-to-end through the real store: capacity triage reads the first
        // row of nbb_financials_for, so the listing order decides whether the
        // ranking sees turnover at all. A comparative period discloses none.
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-rec-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("database should open");
        let filing = |end: Option<&str>, turnover: Option<i64>, ref_: &str| NbbFinancials {
            enterprise_kbo: "0123456789".to_owned(),
            fiscal_year: 2024,
            period_start: end.map(|_| "2024-01-01".to_owned()),
            period_end: end.map(str::to_owned),
            turnover: turnover.map(|value| rust_decimal::Decimal::new(value, 0)),
            staff_fte: None,
            equity: None,
            result: None,
            source_ref: ref_.to_owned(),
            source_format: "xbrl".to_owned(),
            acquisition_method: "api-archive-batch".to_owned(),
        };
        store
            .upsert_nbb_financials(&filing(Some("2024-12-31"), Some(500_000), "current"))
            .expect("current row should store");
        // Stored second, so fetch order alone would put the comparative first.
        store
            .upsert_nbb_financials(&filing(None, None, "comparative"))
            .expect("comparative row should store");

        let financials = store
            .nbb_financials_for("0123.456.789")
            .expect("listing should succeed");
        let (turnover, notice) = nbb_capacity_turnover(
            &financials,
            NaiveDate::from_ymd_opt(2026, 11, 4).expect("valid date"),
            None,
        );
        assert_eq!(
            turnover,
            Some(500_000.0),
            "the current period discloses turnover and must be selected"
        );
        let notice = notice.expect("a notice should accompany stored turnover");
        assert_eq!(
            notice.period_end, "2024-12-31",
            "unexpected notice: {notice:?}"
        );
        assert!(notice.notice().contains("period_end=2024-12-31"));
        let _ = std::fs::remove_file(&path);
    }

    #[allow(clippy::too_many_arguments)]
    fn test_options(
        limit: usize,
        candidates: usize,
        sort: &str,
        format: &str,
        due_within: Option<i64>,
        deep: usize,
        annual_turnover: Option<f64>,
        max_bid_capacity: Option<f64>,
    ) -> RecommendOptions {
        RecommendOptions {
            enterprise_number: "0404.484.060".to_owned(),
            database: "test.duckdb".to_owned(),
            kbo_database: "kbo.duckdb".to_owned(),
            limit,
            candidates,
            sort: sort.to_owned(),
            format: format.to_owned(),
            due_within,
            deep,
            annual_turnover,
            max_bid_capacity,
            capacity_triage: false,
            max_filing_age_days: None,
        }
    }

    #[test]
    fn rejects_a_negative_filing_age_limit() {
        let mut options = test_options(20, 1000, "score", "json", Some(30), 0, None, None);
        options.max_filing_age_days = Some(-1);
        assert_eq!(
            validate_options(&options),
            Err("--max-filing-age-days must not be negative".to_owned())
        );
        // Zero is an extreme but meaningful bound, not a typo.
        options.max_filing_age_days = Some(0);
        assert!(validate_options(&options).is_ok());
        options.max_filing_age_days = Some(730);
        assert!(validate_options(&options).is_ok());
    }

    #[test]
    fn accepts_valid_options() {
        assert!(validate_options(&test_options(
            20,
            1000,
            "score",
            "json",
            Some(30),
            0,
            None,
            None
        ))
        .is_ok());
        assert!(validate_options(&test_options(
            20,
            1000,
            "deadline",
            "csv",
            None,
            10,
            Some(850000.0),
            Some(350000.0)
        ))
        .is_ok());
    }

    #[test]
    fn rejects_missing_option_values() {
        for option in [
            "--database",
            "--kbo-database",
            "--limit",
            "--candidates",
            "--sort",
            "--format",
            "--due-within",
            "--deep",
            "--annual-turnover",
            "--max-bid-capacity",
        ] {
            assert!(parse_args(["0404484060", option].into_iter().map(String::from),).is_err());
        }
    }

    #[test]
    fn csv_field_neutralizes_formula_prefixes() {
        assert_eq!(csv_field("plain title"), "\"plain title\"");
        assert_eq!(
            csv_field("=HYPERLINK(\"http://evil\")"),
            "\"'=HYPERLINK(\"\"http://evil\"\")\""
        );
        assert_eq!(csv_field("  +cmd"), "\"'  +cmd\"");
        assert_eq!(csv_field("\t=1+1"), "\"'\t=1+1\"");
        assert_eq!(csv_field("@mention"), "\"'@mention\"");
        assert_eq!(csv_field("-5"), "\"'-5\"");
    }

    #[test]
    fn rejects_invalid_options() {
        assert!(
            validate_options(&test_options(0, 1000, "score", "json", None, 0, None, None)).is_err()
        );
        assert!(
            validate_options(&test_options(20, 0, "score", "json", None, 0, None, None)).is_err()
        );
        assert!(validate_options(&test_options(
            20,
            MAX_RECOMMENDATION_CANDIDATES + 1,
            "score",
            "json",
            None,
            0,
            None,
            None
        ))
        .is_err());
        assert!(validate_options(&test_options(
            20, 1000, "random", "json", None, 0, None, None
        ))
        .is_err());
        assert!(
            validate_options(&test_options(20, 1000, "score", "xml", None, 0, None, None)).is_err()
        );
        assert!(validate_options(&test_options(
            20,
            1000,
            "score",
            "json",
            Some(366),
            0,
            None,
            None
        ))
        .is_err());
        assert!(validate_options(&test_options(
            20, 1000, "score", "json", None, 11, None, None
        ))
        .is_err());
        assert!(validate_options(&test_options(
            20,
            1000,
            "score",
            "json",
            None,
            0,
            Some(-1.0),
            None
        ))
        .is_err());
        assert!(validate_options(&test_options(
            20,
            1000,
            "score",
            "json",
            None,
            0,
            None,
            Some(0.0)
        ))
        .is_err());
    }
}
