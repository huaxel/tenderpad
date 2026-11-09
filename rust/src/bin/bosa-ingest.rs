use aanbestedingen_matcher::bosa::{
    publication_to_tender, BosaClient, BosaConfig, Environment, PublicationSearch,
};
use aanbestedingen_matcher::store::TenderStore;
use aanbestedingen_matcher::Tender;
use chrono::{Duration, NaiveDate};
use serde_json::json;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::error::Error;

#[derive(Debug, PartialEq)]
struct Options {
    environment: Environment,
    database: String,
    from: Option<String>,
    to: Option<String>,
    limit: usize,
    page_size: u32,
    start_page: u32,
    refresh: bool,
    days: Option<i64>,
    terms: Option<String>,
    all_notices: bool,
    xml_attachment_fallback: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, Box<dyn Error>> {
    let mut environment = Environment::Int;
    let mut database = None;
    let mut from = None;
    let mut to = None;
    let mut limit = None;
    let mut refresh = false;
    let mut days = None;
    let mut terms = None;
    let mut all_notices = false;
    let mut xml_attachment_fallback = false;
    let mut page_size = 10;
    let mut start_page = 1;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            return Ok(None);
        }
        if arg == "--all-notices" {
            all_notices = true;
            continue;
        }
        if arg == "--xml-attachment-fallback" {
            xml_attachment_fallback = true;
            continue;
        }
        if arg == "--refresh" {
            refresh = true;
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{arg} needs a value"))?;
        match arg.as_str() {
            "--env" => environment = Environment::parse(&value)?,
            "--database" => database = Some(value),
            "--from" | "--to" => {
                NaiveDate::parse_from_str(&value, "%Y-%m-%d")?;
                if arg == "--from" {
                    from = Some(value);
                } else {
                    to = Some(value);
                }
            }
            "--limit" => limit = Some(value.parse()?),
            "--days" => days = Some(value.parse::<i64>()?),
            "--terms" => terms = Some(value),
            "--page-size" => page_size = value.parse()?,
            "--start-page" => {
                start_page = value.parse()?;
                if start_page == 0 || start_page > i32::MAX as u32 {
                    return Err("--start-page must be 1..2147483647".into());
                }
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if refresh && terms.is_some() {
        return Err("--terms is only supported for samples, not complete refreshes".into());
    }
    if refresh && start_page != 1 {
        return Err("--start-page is only supported for samples".into());
    }
    let limit = limit.unwrap_or(if refresh { 1000 } else { 10 });
    if days.is_some_and(|n| !(1..=365).contains(&n)) {
        return Err("--days must be 1..365".into());
    }
    if days.is_some() && (!refresh || from.is_some() || to.is_some()) {
        return Err("--days requires --refresh and cannot be combined with --from/--to".into());
    }
    if refresh && (from.is_some() != to.is_some()) {
        return Err("refresh needs both --from and --to".into());
    }
    if !(1..=1000).contains(&limit) {
        return Err("--limit must be 1..1000 (bounded intake, not bulk backfill)".into());
    }
    if !(1..=100).contains(&page_size) {
        return Err("--page-size must be 1..100".into());
    }
    if from.as_ref().zip(to.as_ref()).is_some_and(|(a, b)| a > b) {
        return Err("--from must not be after --to".into());
    }
    let database = database
        .filter(|s: &String| !s.trim().is_empty())
        .ok_or("--database is required; use a separate sample database")?;
    Ok(Some(Options {
        environment,
        database,
        from,
        to,
        limit,
        page_size,
        start_page,
        refresh,
        days,
        terms,
        all_notices,
        xml_attachment_fallback,
    }))
}

trait Gateway {
    fn search(&mut self, search: &PublicationSearch) -> Result<Value, Box<dyn Error>>;
    fn workspace(&mut self, id: &str) -> Result<Value, Box<dyn Error>>;
    fn workspace_for_publication(
        &mut self,
        publication: &Value,
        _environment: Environment,
        _xml_attachment_fallback: bool,
    ) -> Result<Value, Box<dyn Error>> {
        let id = publication
            .get("publicationWorkspaceId")
            .and_then(Value::as_str)
            .ok_or("missing workspace ID")?;
        self.workspace(id)
    }
}
impl Gateway for BosaClient {
    fn search(&mut self, search: &PublicationSearch) -> Result<Value, Box<dyn Error>> {
        Ok(self.search_publications(search)?)
    }
    fn workspace(&mut self, id: &str) -> Result<Value, Box<dyn Error>> {
        Ok(self.publication_workspace(id)?)
    }
    fn workspace_for_publication(
        &mut self,
        publication: &Value,
        environment: Environment,
        xml_attachment_fallback: bool,
    ) -> Result<Value, Box<dyn Error>> {
        if xml_attachment_fallback {
            Ok(self.publication_workspace_with_xml_fallback(publication, environment)?)
        } else {
            let id = publication
                .get("publicationWorkspaceId")
                .and_then(Value::as_str)
                .ok_or("missing workspace ID")?;
            self.workspace(id)
        }
    }
}
#[derive(Debug)]
struct Batch {
    tenders: Vec<Tender>,
    total_count: u64,
    pages: u32,
}
fn window(options: &Options, today: NaiveDate) -> (Option<String>, Option<String>) {
    if options.refresh && options.from.is_none() {
        let days = options.days.unwrap_or(7);
        (
            Some((today - Duration::days(days - 1)).to_string()),
            Some(today.to_string()),
        )
    } else {
        (options.from.clone(), options.to.clone())
    }
}
fn validate_refresh_size(options: &Options, total: u64) -> Result<(), Box<dyn Error>> {
    if options.refresh && total > options.limit as u64 {
        return Err(format!("refresh has {total} publications, exceeding --limit {}; narrow the date window (database unchanged)", options.limit).into());
    }
    Ok(())
}
fn fetch_batch(
    options: &Options,
    client: &mut impl Gateway,
    today: NaiveDate,
) -> Result<Batch, Box<dyn Error>> {
    let (from, to) = window(options, today);
    let mut tenders = Vec::new();
    let mut seen = HashSet::new();
    let mut page = options.start_page;
    let mut pages_fetched = 0;
    // Fixed page size across all requests: changing it moves page boundaries.
    let page_size = options.page_size.min(options.limit as u32);
    let mut examined = 0;
    let total_count;
    let mut first_total = None;
    loop {
        let result = client.search(&PublicationSearch {
            terms: options.terms.clone(),
            publication_date_from: from.clone(),
            publication_date_to: to.clone(),
            with_active_submission_deadline: if options.refresh || options.all_notices {
                None
            } else {
                Some(true)
            },
            page: Some(page),
            page_size: Some(page_size),
            ..Default::default()
        })?;
        pages_fetched += 1;
        let total = result["totalCount"].as_u64().ok_or("missing totalCount")?;
        if first_total.is_some_and(|first| first != total) {
            return Err(format!(
                "publication count changed during {}; database unchanged, retry the window",
                if options.refresh { "refresh" } else { "sample" }
            )
            .into());
        }
        validate_refresh_size(options, total)?;
        first_total.get_or_insert(total);
        let publications = result["publications"]
            .as_array()
            .ok_or("missing publications")?;
        if publications.is_empty() {
            let previous_pages_end = u64::from(page - 1) * u64::from(page_size);
            if previous_pages_end < total {
                return Err("premature empty search page; database unchanged".into());
            }
            if page == options.start_page && options.start_page > 1 && total > 0 {
                return Err(
                    "start page is beyond available search results; database unchanged".into(),
                );
            }
        }
        if publications.len() > page_size as usize
            || (publications.len() < page_size as usize
                && u64::from(page) * u64::from(page_size) < total)
        {
            return Err("inconsistent search page length; database unchanged".into());
        }
        for publication in publications {
            if examined == options.limit {
                break;
            }
            examined += 1;
            let id = publication["publicationWorkspaceId"]
                .as_str()
                .ok_or("missing workspace ID")?;
            if !seen.insert(id.to_owned()) {
                return Err(
                    "duplicate workspace across search pages; rerun with a closed date range"
                        .into(),
                );
            }
            let workspace = client.workspace_for_publication(
                publication,
                options.environment,
                options.xml_attachment_fallback,
            )?;
            tenders.push(publication_to_tender(
                publication,
                &workspace,
                options.environment,
            )?);
        }
        if examined == options.limit || u64::from(page) * u64::from(page_size) >= total {
            total_count = first_total.unwrap_or(0);
            break;
        }
        page += 1;
    }
    if options.refresh && examined as u64 != total_count {
        return Err("incomplete refresh; database unchanged".into());
    }
    Ok(Batch {
        tenders,
        total_count,
        pages: pages_fetched,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: bosa-ingest --database PATH [--env int|prod] [--terms TEXT] [--all-notices] [--xml-attachment-fallback] [--from YYYY-MM-DD] [--to YYYY-MM-DD] [--limit 1..1000] [--page-size 1..100] [--start-page 1..2147483647] [--refresh [--days 1..365]]");
        println!("Default: INT active-deadline sample of 10. --refresh: all notice types in a complete rolling 7-day window, maximum 1000; refuses truncation. Explicit --from/--to overrides rolling dates.");
        return Ok(());
    };
    let started = std::time::Instant::now();
    let today = chrono::Utc::now().date_naive();
    let mut client = BosaClient::new(BosaConfig::for_environment(options.environment)?);
    let Batch {
        tenders,
        total_count,
        pages: page,
    } = fetch_batch(&options, &mut client, today)?;
    // Fetch and validate the entire bounded batch before opening/writing the DB.
    let store = TenderStore::open(&options.database)?;
    let stored = store.upsert_many(tenders.iter())?;
    let stored_samples = tenders
        .iter()
        .take(3)
        .map(|t| {
            let workspace = t
                .raw
                .get("bosa_publication_workspace_id")
                .and_then(Value::as_str)
                .ok_or("missing workspace ID")?;
            store
                .bosa_workspace_tender(options.environment.source(), workspace)?
                .ok_or_else(|| "stored workspace not found".into())
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    if options.refresh {
        let (from, to) = window(&options, today);
        store.record_refresh_run(
            "refresh",
            &format!(
                "bosa-{}-all",
                if options.environment == Environment::Prod {
                    "prod"
                } else {
                    "int"
                }
            ),
            &format!("{}..{}", from.unwrap_or_default(), to.unwrap_or_default()),
            tenders.len() as i64,
            stored as i64,
            Some(total_count as i64),
            started.elapsed().as_millis() as i64,
        )?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "environment": if options.environment == Environment::Prod { "prod" } else { "int" },
            "database": options.database, "search_total": total_count,
            "start_page": options.start_page, "pages": page,
            "mode": if options.refresh { "refresh" } else { "sample" },
            "complete": options.start_page == 1 && tenders.len() as u64 == total_count,
            "fetched": tenders.len(), "stored": stored, "database_total": store.count()?,
            "with_buyer": tenders.iter().filter(|t| t.buyer.is_some()).count(),
            "with_cpv": tenders.iter().filter(|t| !t.cpv_codes.is_empty()).count(),
            "with_deadline": tenders.iter().filter(|t| t.deadline.is_some()).count(),
            "with_estimated_value": tenders.iter().filter(|t| t.estimated_value.is_some()).count(),
            "samples": stored_samples.iter().map(|t| json!({
                "workspace": t.raw.get("bosa_publication_workspace_id"),
                "id": t.external_id, "title": t.title, "buyer": t.buyer, "cpvs": t.cpv_codes,
                "deadline": t.deadline, "publication_date": t.publication_date,
                "estimated_value": t.estimated_value, "notice_type": t.notice_type,
                "framework_maximum_amounts": t.raw.get("framework_maximum_amounts"),
            })).collect::<Vec<_>>()
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Result<Option<Options>, Box<dyn Error>> {
        parse_args(values.iter().map(|s| s.to_string()))
    }
    #[test]
    fn requires_explicit_database_and_bounds_requests() {
        assert!(args(&[]).is_err());
        for flags in [
            vec!["--limit", "0"],
            vec!["--limit", "1001"],
            vec!["--page-size", "101"],
            vec!["--start-page", "0"],
            vec!["--start-page", "2147483648"],
            vec!["--env", "pr"],
            vec!["--from", "2026-02-30"],
        ] {
            let mut all = vec!["--database", "sample.duckdb"];
            all.extend(flags);
            assert!(args(&all).is_err());
        }
        assert!(args(&[
            "--database",
            "sample.duckdb",
            "--from",
            "2026-11-01",
            "--to",
            "2026-10-30"
        ])
        .is_err());
        assert!(args(&[
            "--database",
            "sample.duckdb",
            "--refresh",
            "--start-page",
            "2"
        ])
        .is_err());
    }
    #[test]
    fn defaults_to_int_and_accepts_explicit_prod() {
        let options = args(&["--database", "sample.duckdb"]).unwrap().unwrap();
        assert_eq!(options.environment, Environment::Int);
        assert_eq!(options.limit, 10);
        assert_eq!(options.start_page, 1);
        assert!(!options.xml_attachment_fallback);
        assert!(
            args(&["--database", "sample.duckdb", "--xml-attachment-fallback"])
                .unwrap()
                .unwrap()
                .xml_attachment_fallback
        );
        assert_eq!(
            args(&["--env", "prod", "--database", "prod.duckdb"])
                .unwrap()
                .unwrap()
                .environment,
            Environment::Prod
        );
        assert!(args(&["--help"]).unwrap().is_none());
    }
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 30).unwrap()
    }
    fn options(flags: &[&str]) -> Options {
        let mut values = vec!["--database", "sample.duckdb"];
        values.extend(flags);
        args(&values).unwrap().unwrap()
    }
    struct Mock {
        pages: std::collections::VecDeque<Value>,
        searches: Vec<PublicationSearch>,
        workspace_calls: usize,
        fallback_calls: usize,
    }
    impl Mock {
        fn new(pages: Vec<Value>) -> Self {
            Self {
                pages: pages.into(),
                searches: vec![],
                workspace_calls: 0,
                fallback_calls: 0,
            }
        }
    }
    fn page(ids: &[usize], total: usize) -> Value {
        json!({"totalCount": total,"publications":ids.iter().map(|id| json!({"publicationWorkspaceId":format!("workspace-{id}"),"publicationType":"ACTIVE","noticeIds":["notice"]})).collect::<Vec<_>>()})
    }
    impl Gateway for Mock {
        fn search(&mut self, search: &PublicationSearch) -> Result<Value, Box<dyn Error>> {
            self.searches.push(search.clone());
            self.pages
                .pop_front()
                .ok_or_else(|| "unexpected extra search".into())
        }
        fn workspace(&mut self, id: &str) -> Result<Value, Box<dyn Error>> {
            self.workspace_calls += 1;
            Ok(
                json!({"id":id,"noticeType":"cn-general","versions":[{"status":"PUBLISHED","activationDate":"2026-10-30","notice":{"id":"notice","xmlContent":format!("<ContractNotice><ID>notice</ID><ContractFolderID>{id}</ContractFolderID><VersionID>1</VersionID><ProcurementProject><Name>Test</Name></ProcurementProject></ContractNotice>")}}]}),
            )
        }
        fn workspace_for_publication(
            &mut self,
            publication: &Value,
            _environment: Environment,
            xml_attachment_fallback: bool,
        ) -> Result<Value, Box<dyn Error>> {
            self.fallback_calls += usize::from(xml_attachment_fallback);
            self.workspace(
                publication["publicationWorkspaceId"]
                    .as_str()
                    .ok_or("missing workspace ID")?,
            )
        }
    }
    #[test]
    fn xml_attachment_recovery_is_explicitly_opt_in() {
        let mut normal = Mock::new(vec![page(&[1], 1)]);
        fetch_batch(&options(&[]), &mut normal, today()).unwrap();
        assert_eq!(normal.fallback_calls, 0);

        let mut opted_in = Mock::new(vec![page(&[1], 1)]);
        fetch_batch(
            &options(&["--xml-attachment-fallback"]),
            &mut opted_in,
            today(),
        )
        .unwrap();
        assert_eq!(opted_in.fallback_calls, 1);
    }

    #[test]
    fn complete_refresh_walks_fixed_pages_with_overlap_and_all_notice_types() {
        let options = options(&["--refresh", "--limit", "10", "--page-size", "2"]);
        let mut mock = Mock::new(vec![page(&[1, 2], 3), page(&[3], 3)]);
        let batch = fetch_batch(&options, &mut mock, today()).unwrap();
        assert_eq!(batch.tenders.len(), 3);
        assert_eq!(batch.pages, 2);
        assert_eq!(
            mock.searches[0].publication_date_from.as_deref(),
            Some("2026-10-24")
        );
        assert_eq!(
            mock.searches[0].publication_date_to.as_deref(),
            Some("2026-10-30")
        );
        assert!(mock
            .searches
            .iter()
            .all(|s| s.page_size == Some(2) && s.with_active_submission_deadline.is_none()));
        assert_eq!(mock.searches[1].page, Some(2));
    }
    #[test]
    fn sample_can_resume_from_a_later_page_without_replaying_earlier_pages() {
        let requested = options(&["--start-page", "3", "--limit", "2", "--page-size", "2"]);
        let mut mock = Mock::new(vec![page(&[7, 8], 10)]);
        let batch = fetch_batch(&requested, &mut mock, today()).unwrap();
        assert_eq!(batch.tenders.len(), 2);
        assert_eq!(batch.total_count, 10);
        assert_eq!(batch.pages, 1);
        assert_eq!(mock.searches.len(), 1);
        assert_eq!(mock.searches[0].page, Some(3));
        assert!(mock.pages.is_empty());

        let mut missing_page = Mock::new(vec![page(&[], 2)]);
        assert!(fetch_batch(
            &options(&["--start-page", "2", "--limit", "2", "--page-size", "2"]),
            &mut missing_page,
            today()
        )
        .unwrap_err()
        .to_string()
        .contains("start page is beyond available search results"));
    }
    #[test]
    fn capped_refresh_fails_before_fetching_dossiers_but_sample_is_explicitly_partial() {
        let mut mock = Mock::new(vec![page(&[1, 2], 5)]);
        assert!(fetch_batch(
            &options(&["--refresh", "--limit", "3", "--page-size", "2"]),
            &mut mock,
            today()
        )
        .unwrap_err()
        .to_string()
        .contains("exceeding --limit"));
        assert_eq!(mock.workspace_calls, 0);
        let mut mock = Mock::new(vec![page(&[1, 2], 5), page(&[3, 4], 5)]);
        let batch = fetch_batch(
            &options(&["--limit", "3", "--page-size", "2"]),
            &mut mock,
            today(),
        )
        .unwrap();
        assert_eq!(batch.tenders.len(), 3);
        assert_eq!(batch.total_count, 5);
    }
    #[test]
    fn refresh_rejects_count_drift_short_pages_and_duplicates() {
        for pages in [
            vec![page(&[1, 2], 3), page(&[3], 4)],
            vec![page(&[1], 3)],
            vec![page(&[1, 2], 3), page(&[1], 3)],
            vec![page(&[], 3)],
        ] {
            let mut mock = Mock::new(pages);
            assert!(fetch_batch(
                &options(&["--refresh", "--page-size", "2"]),
                &mut mock,
                today()
            )
            .is_err());
        }
        let mut sample = Mock::new(vec![page(&[1, 2], 3), page(&[3], 4)]);
        assert!(fetch_batch(
            &options(&["--limit", "3", "--page-size", "2"]),
            &mut sample,
            today()
        )
        .unwrap_err()
        .to_string()
        .contains("publication count changed during sample"));
    }
    #[test]
    fn rolling_dates_require_refresh_and_cannot_mix_with_explicit_ranges() {
        assert!(args(&["--database", "sample.duckdb", "--days", "7"]).is_err());
        assert!(args(&[
            "--database",
            "sample.duckdb",
            "--refresh",
            "--terms",
            "road"
        ])
        .is_err());
        assert!(args(&["--database", "sample.duckdb", "--refresh", "--days", "0"]).is_err());
        assert!(args(&[
            "--database",
            "sample.duckdb",
            "--refresh",
            "--from",
            "2026-10-01"
        ])
        .is_err());
        assert!(args(&[
            "--database",
            "sample.duckdb",
            "--refresh",
            "--days",
            "2",
            "--from",
            "2026-10-01",
            "--to",
            "2026-10-30"
        ])
        .is_err());
        assert_eq!(
            window(&options(&["--refresh", "--days", "1"]), today()),
            (Some("2026-10-30".into()), Some("2026-10-30".into()))
        );
    }
}
