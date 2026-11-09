use aanbestedingen_matcher::award_xml::parse_award_xml;
use aanbestedingen_matcher::awards::{
    award_xml_mapping_complete, award_xml_url, awards_for_identifier, fetch_award_xml,
    normalize_award_identifier, scope_award_to_identifier,
};
use aanbestedingen_matcher::kbo::normalize_enterprise_number;
use aanbestedingen_matcher::store::TenderStore;
use aanbestedingen_matcher::ted::{SearchScope, SearchWindow, TedClient, MAX_TED_SEARCH_RESULTS};
use chrono::{Duration, NaiveDate, Utc};
use serde_json::Value;
use std::collections::BTreeSet;
use std::env;

#[derive(Debug, PartialEq)]
struct AwardIngestOptions {
    enterprise_number: Option<String>,
    limit: usize,
    database: String,
    /// Ingest every Belgian award notice published in the last N days, for every
    /// winner, instead of one company's history.
    window: Option<i64>,
    /// Explicit closed publication range for a historical backfill.
    range: Option<(NaiveDate, NaiveDate)>,
}

fn terminal_safe(value: &str) -> String {
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            safe.push_str(&format!("\\u{{{:04X}}}", character as u32));
        } else {
            safe.push(character);
        }
    }
    safe
}

fn parse_args<I>(args: I) -> Result<Option<AwardIngestOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut raw_enterprise_number = None;
    let mut limit = 100_usize;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut window = None;
    let mut from = None;
    let mut to = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--limit" => {
                let value: usize = arguments.next().ok_or("--limit needs a value")?.parse()?;
                if value == 0 || value > MAX_TED_SEARCH_RESULTS {
                    return Err(format!(
                        "TED search limit must be between 1 and {MAX_TED_SEARCH_RESULTS}"
                    )
                    .into());
                }
                limit = value;
            }
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--window" => {
                let value: i64 = arguments.next().ok_or("--window needs a value")?.parse()?;
                if !(1..=3650).contains(&value) {
                    return Err("--window must be between 1 and 3650 days".into());
                }
                window = Some(value);
            }
            "--from" => {
                from = Some(NaiveDate::parse_from_str(
                    &arguments.next().ok_or("--from needs YYYY-MM-DD")?,
                    "%Y-%m-%d",
                )?)
            }
            "--to" => {
                to = Some(NaiveDate::parse_from_str(
                    &arguments.next().ok_or("--to needs YYYY-MM-DD")?,
                    "%Y-%m-%d",
                )?)
            }
            "--help" | "-h" => return Ok(None),
            argument if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}").into())
            }
            argument => {
                if raw_enterprise_number.replace(argument.to_owned()).is_some() {
                    return Err("only one enterprise number is allowed".into());
                }
            }
        }
    }
    let range = match (from, to) {
        (Some(from), Some(to)) if from <= to => Some((from, to)),
        (Some(_), Some(_)) => return Err("--from must not be after --to".into()),
        (None, None) => None,
        _ => return Err("--from and --to must be given together".into()),
    };
    let modes = [
        raw_enterprise_number.is_some(),
        window.is_some(),
        range.is_some(),
    ];
    if modes.iter().filter(|mode| **mode).count() != 1 {
        return Err("give exactly one of: an enterprise number, --window, or --from/--to".into());
    }
    let enterprise_number = raw_enterprise_number
        .map(|number| normalize_enterprise_number(&number))
        .transpose()?;
    Ok(Some(AwardIngestOptions {
        enterprise_number,
        limit,
        database,
        window,
        range,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!("Usage: award-ingest ENTERPRISE_NUMBER [--limit N] [--database PATH]");
        println!("       award-ingest --window DAYS [--database PATH]");
        println!("       award-ingest --from YYYY-MM-DD --to YYYY-MM-DD [--database PATH]");
        println!();
        println!("With --window, every Belgian award notice published in that many");
        println!("days is stored for each of its winners, so `history_match` works for");
        println!("every company instead of only the ones looked up by hand.");
        return Ok(());
    };
    let AwardIngestOptions {
        enterprise_number,
        limit,
        database,
        window,
        range,
    } = options;
    if let Some(days) = window {
        // UTC, matching the store's `current_timestamp` and ted-ingest: a local day
        // west of UTC would ask for a window that ends before the row it just wrote.
        let today = Utc::now().date_naive();
        return ingest_window(
            &database,
            today - Duration::days(days),
            today,
            &TedClient::default(),
        );
    }
    if let Some((from, to)) = range {
        return ingest_window(&database, from, to, &TedClient::default());
    }
    let enterprise_number = enterprise_number.ok_or("enterprise number is required")?;

    let awards = awards_for_identifier(&enterprise_number, &TedClient::default(), limit)?;
    let store = TenderStore::open(database)?;
    let target_digits = normalize_award_identifier(&enterprise_number);
    let mut updates: Vec<(
        String,
        Option<aanbestedingen_matcher::AwardRecord>,
        Vec<aanbestedingen_matcher::AwardLot>,
    )> = Vec::new();
    let mut exact_lots = 0;
    for award in &awards {
        let Some(xml_url) = award_xml_url(&award.links) else {
            let scoped = scope_award_to_identifier(award, &enterprise_number, None);
            updates.push((award.publication_number.clone(), scoped, Vec::new()));
            continue;
        };
        let lots = match fetch_award_xml(xml_url)
            .and_then(|xml| parse_award_xml(&xml, &award.publication_number))
        {
            Ok(lots) => lots,
            Err(error) => {
                eprintln!(
                    "Warning: preserving previous data for {}: {}",
                    terminal_safe(&award.publication_number),
                    terminal_safe(&error.to_string())
                );
                continue;
            }
        };
        if !award_xml_mapping_complete(award, &lots) {
            let scoped = scope_award_to_identifier(award, &enterprise_number, None);
            updates.push((award.publication_number.clone(), scoped, Vec::new()));
            continue;
        }
        let matching = lots
            .into_iter()
            .filter(|lot| {
                lot.winner_identifier
                    .as_deref()
                    .is_some_and(|winner| normalize_award_identifier(winner) == target_digits)
            })
            .collect::<Vec<_>>();
        let scoped = scope_award_to_identifier(award, &enterprise_number, Some(&matching));
        exact_lots += matching.len();
        updates.push((award.publication_number.clone(), scoped, matching));
    }
    let update_refs = updates
        .iter()
        .map(|(publication, award, lots)| (publication.as_str(), award.as_ref(), lots.as_slice()));
    store.replace_award_refresh(&enterprise_number, update_refs)?;
    let stored_awards = updates
        .iter()
        .filter(|(_, award, _)| award.is_some())
        .count();
    println!(
        "Stored {} awards and {exact_lots} exact lots; history contains {} total",
        stored_awards,
        store.awards_for(&enterprise_number)?.len()
    );
    Ok(())
}

/// Normalises a TED winner identifier to the store's enterprise format, or
/// `None` when it is not a Belgian enterprise number (a French SIRET would create
/// a history row that can never be looked up).
fn belgian_enterprise(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 10 || !(digits.starts_with('0') || digits.starts_with('1')) {
        return None;
    }
    normalize_enterprise_number(&digits).ok()
}

/// Store every Belgian award notice in the window for each of its winners.
///
/// One company lookup at a time (`awards_for_identifier`) leaves `history_match`
/// neutral for everyone else — measured 2026-10-19, that term is 15% of the score
/// and was unused for every company in a sector audit. TED refuses pages beyond
/// roughly sixty, so the window is walked a month at a time.
fn ingest_window(
    database: &str,
    start: NaiveDate,
    today: NaiveDate,
    client: &TedClient,
) -> Result<(), Box<dyn std::error::Error>> {
    let fields = [
        "publication-number",
        "notice-title",
        "buyer-name",
        "winner-name",
        "winner-identifier",
        "classification-cpv",
        "result-value-lot",
        "result-value-notice",
        "links",
    ];
    let store = TenderStore::open(database)?;
    let mut notices_seen = 0_usize;
    let mut awards_stored = 0_usize;
    let mut companies: BTreeSet<String> = BTreeSet::new();
    // Summed over the month windows, like a chunked refresh: TED's
    // `totalNoticeCount` is the population of the query it answered, so the sum
    // covers the walk. Without it an award run records `fetched` with no
    // denominator, and a month that answered short is indistinguishable from a
    // complete scan — the failure the tender refresh already refuses to hide.
    let mut population: i64 = 0;
    let mut population_reported = false;
    // One month without a population makes the sum incomplete, and an
    // under-reported total would read as truncation that never happened.
    let mut population_missing = false;
    let started = std::time::Instant::now();
    let mut month = start;
    while month <= today {
        let until = (month + Duration::days(31)).min(today);
        let window = SearchWindow {
            scope: SearchScope::All,
            published_since: Some(month),
            published_until: Some(until),
        };
        // A month that fails keeps the earlier months' rows, so the run is recorded
        // before returning: awards next to no run row is provenance claiming nothing
        // was fetched, and the record says how far the walk actually got.
        let (notices, total) = match client.search_raw_window_with_total(
            "buyer-country=BEL AND notice-type=can-standard",
            &fields,
            MAX_TED_SEARCH_RESULTS,
            &window,
        ) {
            Ok(result) => result,
            Err(error) => {
                // `awards-partial` says the walk stopped; a total summed from the
                // completed months would equal what they delivered, so the row
                // would claim coverage it never had. Unknown is the honest entry.
                let _ = store.record_refresh_run(
                    "awards-partial",
                    "ACTIVE",
                    &format!("awards {start}..{today} (failed at {month})"),
                    notices_seen as i64,
                    awards_stored as i64,
                    None,
                    started.elapsed().as_millis() as i64,
                );
                return Err(error);
            }
        };
        match total {
            Some(total) => {
                population = population.saturating_add(total);
                population_reported = true;
            }
            None => population_missing = true,
        }
        notices_seen += notices.len();
        // One award row per company that won, but one XML download per notice.
        for notice in &notices {
            let (stored, touched) =
                store_notice_awards(&store, notice, |url| fetch_award_xml(url).ok())?;
            awards_stored += stored;
            companies.extend(touched);
        }
        eprintln!(
            "window {month}..{until}: {} notices seen, {} awards stored, {} companies so far",
            notices.len(),
            awards_stored,
            companies.len()
        );
        month = until + Duration::days(1);
    }
    let total_notice_count = (population_reported && !population_missing).then_some(population);
    // TED reporting more notices than the walk received means the population was
    // not covered, however healthy the pages looked. Award rows are additive
    // history rather than a watermark, so the run keeps what it stored — but it
    // records itself as partial so "did the weekly scan cover its window?" has an
    // answer, instead of a complete-looking row that found less news.
    let short = total_notice_count.filter(|total| (notices_seen as i64) < *total);
    let kind = if short.is_some() {
        "awards-partial"
    } else {
        "awards"
    };
    store.record_refresh_run(
        kind,
        "ACTIVE",
        &format!("awards {start}..{today}"),
        notices_seen as i64,
        awards_stored as i64,
        total_notice_count,
        started.elapsed().as_millis() as i64,
    )?;
    if let Some(total) = total_notice_count {
        println!("TED reports {total} award notices in this population");
    }
    println!(
        "Scanned {notices_seen} award notices from {start} to {today}; stored \
         {awards_stored} award records for {} companies",
        companies.len()
    );
    if let Some(total) = short {
        println!(
            "WARNING: TED reports {total} award notices in this window and the walk received \
             {notices_seen}; the run is recorded as awards-partial. Re-run the window once the \
             index settles."
        );
    }
    Ok(())
}

/// Store one notice's awards for every Belgian winner, downloading the notice's
/// lot XML **once**.
///
/// A consortium lists every member as a winner and the lot XML is one document
/// describing all of them, so fetching it inside the per-winner loop sent
/// identical bytes to a rate-limited endpoint once per member.
///
/// The trade-off is deliberate and worth stating: one download now serves the
/// whole notice, so the evidence is deterministic (every member is scoped from
/// the same bytes) and costs one request instead of N — but a download that fails
/// affects every member of that notice, where the old loop gave each winner an
/// independent chance of success. `scope_award_to_identifier` treats a
/// multi-winner notice without lot detail as ambiguous and clears the CPV codes
/// and values, so a failure here blanks detail for all of them rather than for
/// one. `fetch_award_xml` already retries a throttled or transient response
/// before that happens.
///
/// Returns how many award rows were stored and which companies they belong to.
fn store_notice_awards<F>(
    store: &TenderStore,
    notice: &Value,
    fetch_xml: F,
) -> Result<(usize, BTreeSet<String>), Box<dyn std::error::Error>>
where
    F: FnOnce(&str) -> Option<Vec<u8>>,
{
    let Ok(award) = aanbestedingen_matcher::awards::try_notice_to_award(notice) else {
        return Ok((0, BTreeSet::new()));
    };
    let mut winners: BTreeSet<String> = BTreeSet::new();
    for winner in &award.winner_identifiers {
        if let Some(number) = belgian_enterprise(winner) {
            winners.insert(number);
        }
    }
    if winners.is_empty() {
        // No Belgian winner: nothing to store and no reason to spend a request.
        return Ok((0, BTreeSet::new()));
    }
    // Lot detail is what makes the CPV evidence exact; when it cannot be fetched
    // the notice-level award is still worth storing.
    let lots = award_xml_url(&award.links)
        .and_then(fetch_xml)
        .and_then(|xml| parse_award_xml(&xml, &award.publication_number).ok())
        .unwrap_or_default();
    let mut stored = 0_usize;
    let mut companies = BTreeSet::new();
    for enterprise in winners {
        let scoped = scope_award_to_identifier(
            &award,
            &enterprise,
            (!lots.is_empty()).then_some(lots.as_slice()),
        );
        let Some(scoped) = scoped else { continue };
        store.upsert_award(&enterprise, &scoped)?;
        stored += 1;
        companies.insert(enterprise);
    }
    Ok((stored, companies))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> std::path::PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("aanbestedingen-{name}-{suffix}.duckdb"))
    }

    /// Answer one search request with `(status, body)`, then stop. The client reads
    /// only the status and body it is given, so this stays a single connection.
    fn serve_once(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("the fixture should bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("an address"));
        let handle = std::thread::spawn(move || {
            for (status, body) in responses {
                let (stream, _) = listener.accept().expect("a connection");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .expect("a read timeout");
                let mut reader = BufReader::new(stream);
                let mut length = 0_usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).expect("a header") == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("Content-Length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body_bytes = vec![0; length];
                use std::io::Read;
                let _ = reader.read_exact(&mut body_bytes);
                write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status} \r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("a response");
            }
        });
        (endpoint, handle)
    }
    /// A consortium notice lists every member as a winner, and its lot XML is one
    /// document for all of them. Fetching it inside the per-winner loop sent the
    /// same bytes to a rate-limited endpoint once per member.
    #[test]
    fn award_xml_is_fetched_once_per_notice_not_once_per_winner() {
        use std::cell::Cell;
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-awards-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        let notice = json!({
            "publication-number": "123456-2026",
            "notice-title": "Works",
            "buyer-name": "Stad Evere",
            "winner-identifier": ["0123456789", "0404.483.367", "FR12345678901"],
            "winner-name": ["A", "B", "C"],
            "result-value-lot": ["1000"],
            "links": ["https://ted.europa.eu/en/notice/-/detail/123456-2026/xml"]
        });
        let downloads = Cell::new(0_usize);
        let (stored, companies) = store_notice_awards(&store, &notice, |_url| {
            downloads.set(downloads.get() + 1);
            None
        })
        .expect("awards should store");
        assert_eq!(
            downloads.get(),
            1,
            "one download serves every winner of the notice"
        );
        assert_eq!(
            stored, 2,
            "both Belgian winners get a row; the foreign identifier does not"
        );
        assert_eq!(companies.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// A completed window leaves one run row, and that row is *not* the store's
    /// "last refresh": an award fetch answers a different question from a tender
    /// refresh, and letting it become the last run would misreport tender coverage.
    #[test]
    fn a_completed_window_records_an_award_run_apart_from_tender_coverage() {
        let path = scratch("award-run");
        let notice = json!({
            "publication-number": "123456-2026",
            "notice-title": "Works",
            "buyer-name": "Stad Evere",
            "winner-identifier": ["0123456789"],
            "winner-name": ["A"],
            "result-value-lot": ["1000"]
        });
        let (endpoint, handle) = serve_once(vec![(
            200,
            json!({"notices": [notice], "totalNoticeCount": 1}).to_string(),
        )]);
        let client = TedClient {
            endpoint,
            timeout: std::time::Duration::from_secs(10),
        };
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).expect("a date");
        ingest_window(path.to_str().expect("utf-8"), today, today, &client)
            .expect("the window should ingest");
        handle.join().expect("the fixture should finish");
        let store = TenderStore::open(&path).expect("store should reopen");
        let run = store
            .latest_award_run()
            .expect("read should work")
            .expect("the award run must be recorded");
        assert_eq!(run.kind, "awards");
        assert_eq!(run.window, "awards 2026-11-07..2026-11-07");
        assert_eq!(run.fetched, 1, "one award notice was scanned");
        assert_eq!(run.stored, 1, "one award row was stored");
        assert_eq!(
            run.total_notice_count,
            Some(1),
            "the walk received everything TED reported, so the row is complete"
        );
        assert_eq!(
            store
                .latest_refresh_run()
                .expect("read should work")
                .map(|r| r.kind),
            None,
            "an award run must not become the tender store's last run"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// TED's `totalNoticeCount` is the population of the month it answered. Summed
    /// over the walk it gives the run a denominator, so a month that came back
    /// short is visible as `fetched < total_notice_count` and the run says so in
    /// its kind instead of looking like a scan that found less news.
    #[test]
    fn a_month_that_answers_short_records_a_partial_run() {
        let path = scratch("award-population");
        let notice = json!({
            "publication-number": "123456-2026",
            "notice-title": "Works",
            "buyer-name": "Stad Evere",
            "winner-identifier": ["0123456789"],
            "winner-name": ["A"],
            "result-value-lot": ["1000"]
        });
        // TED says three notices exist in the window and delivers one of them.
        let (endpoint, handle) = serve_once(vec![(
            200,
            json!({"notices": [notice], "totalNoticeCount": 3}).to_string(),
        )]);
        let client = TedClient {
            endpoint,
            timeout: std::time::Duration::from_secs(10),
        };
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).expect("a date");
        ingest_window(path.to_str().expect("utf-8"), today, today, &client)
            .expect("the window should ingest");
        handle.join().expect("the fixture should finish");
        let store = TenderStore::open(&path).expect("store should reopen");
        let run = store
            .latest_award_run()
            .expect("read should work")
            .expect("the award run must be recorded");
        assert_eq!(run.fetched, 1);
        assert_eq!(
            run.kind, "awards-partial",
            "a walk that received less than TED reported must not claim completeness"
        );
        assert_eq!(
            run.total_notice_count,
            Some(3),
            "the run keeps TED's population, so the shortfall stays quantifiable"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The walk is a month at a time, so the run's population is a sum. A month
    /// that reports none makes the sum incomplete: under-reporting it would read
    /// as truncation that never happened, so the run records unknown instead.
    #[test]
    fn a_month_without_a_population_makes_the_summed_total_unknown() {
        let path = scratch("award-population-missing");
        let notice = json!({
            "publication-number": "123456-2026",
            "notice-title": "Works",
            "buyer-name": "Stad Evere",
            "winner-identifier": ["0123456789"],
            "winner-name": ["A"],
            "result-value-lot": ["1000"]
        });
        let (endpoint, handle) = serve_once(vec![
            (
                200,
                json!({"notices": [notice.clone()], "totalNoticeCount": 5}).to_string(),
            ),
            (200, json!({"notices": [notice]}).to_string()),
        ]);
        let client = TedClient {
            endpoint,
            timeout: std::time::Duration::from_secs(10),
        };
        // Spans two month windows: 2026-10-06..2026-11-05, then 2026-11-06..2026-11-07.
        let start = NaiveDate::from_ymd_opt(2026, 10, 6).expect("a date");
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).expect("a date");
        ingest_window(path.to_str().expect("utf-8"), start, today, &client)
            .expect("the window should ingest");
        handle.join().expect("the fixture should finish");
        let store = TenderStore::open(&path).expect("store should reopen");
        let run = store
            .latest_award_run()
            .expect("read should work")
            .expect("the award run must be recorded");
        assert_eq!(run.fetched, 2, "both months were walked");
        assert_eq!(
            run.total_notice_count, None,
            "one month reported no population, so the sum is unknown, not 5"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A month that fails keeps the earlier months' awards, so the run is recorded
    /// as partial with how far it got. Awards next to no run row is provenance
    /// claiming nothing happened.
    #[test]
    fn a_failed_window_records_a_partial_run() {
        let path = scratch("award-partial");
        let (endpoint, handle) = serve_once(vec![(400, String::new())]);
        let client = TedClient {
            endpoint,
            timeout: std::time::Duration::from_secs(10),
        };
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).expect("a date");
        let error = ingest_window(path.to_str().expect("utf-8"), today, today, &client)
            .expect_err("a rejected search must fail the window");
        handle.join().expect("the fixture should finish");
        assert!(
            error.to_string().contains("400"),
            "the cause is kept: {error}"
        );
        let store = TenderStore::open(&path).expect("store should reopen");
        let run = store
            .latest_award_run()
            .expect("read should work")
            .expect("the failed run must still be recorded");
        assert_eq!(run.kind, "awards-partial", "it is not coverage");
        assert!(
            run.window.contains("failed at"),
            "the row says how far the walk got: {}",
            run.window
        );
        assert_eq!(store.latest_refresh_run().expect("read should work"), None);
        let _ = std::fs::remove_file(&path);
    }

    /// The per-winner loop is gone, so the download count must not scale with the
    /// number of winners: four members still cost one request.
    #[test]
    fn download_count_does_not_scale_with_winner_count() {
        use std::cell::Cell;
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-awards-wide-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        let winners: Vec<String> = (0..4).map(|n| format!("01234567{n:02}")).collect();
        let notice = json!({
            "publication-number": "223344-2026",
            "notice-title": "Works",
            "winner-identifier": winners,
            "result-value-lot": ["1000"],
            "links": ["https://ted.europa.eu/en/notice/-/detail/223344-2026/xml"]
        });
        let downloads = Cell::new(0_usize);
        let (stored, _) = store_notice_awards(&store, &notice, |_url| {
            downloads.set(downloads.get() + 1);
            None
        })
        .expect("awards should store");
        assert_eq!(stored, 4);
        assert_eq!(downloads.get(), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// A notice with no Belgian winner must not spend a request at all.
    #[test]
    fn a_notice_without_a_belgian_winner_downloads_nothing() {
        use std::cell::Cell;
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("aanbestedingen-awards-none-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        let notice = json!({
            "publication-number": "556677-2026",
            "notice-title": "Works",
            "winner-identifier": ["FR12345678901"],
            "result-value-lot": ["1000"],
            "links": ["https://ted.europa.eu/en/notice/-/detail/556677-2026/xml"]
        });
        let downloads = Cell::new(0_usize);
        let (stored, companies) = store_notice_awards(&store, &notice, |_url| {
            downloads.set(downloads.get() + 1);
            None
        })
        .expect("nothing to store is not a failure");
        assert_eq!(stored, 0);
        assert!(companies.is_empty());
        assert_eq!(downloads.get(), 0, "no Belgian winner, no download");
        let _ = std::fs::remove_file(&path);
    }

    /// The shared download has to keep per-winner scoping intact: the lot XML
    /// describes every member of a consortium, and each member must be stored with
    /// only its own lots. The other tests mock the download away, so nothing else
    /// would catch a scoping regression here.
    #[test]
    fn each_winner_keeps_only_its_own_lots_from_the_shared_download() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aanbestedingen-awards-scope-{suffix}.duckdb"));
        let store = TenderStore::open(&path).expect("store should open");
        let notice = json!({
            "publication-number": "778899-2026",
            "notice-title": "Works",
            "winner-identifier": ["0123456789", "0404.483.367"],
            "result-value-lot": ["100", "200"],
            "links": ["https://ted.europa.eu/en/notice/-/detail/778899-2026/xml"]
        });
        // Two lots, one per member, each with its own CPV. The code has to sit on the
        // element that carries the lot ID: `lot_cpv_index` keys CPVs by the lot's
        // `ID schemeName="lot"`, and a notice-wide classification would be copied
        // to every lot rather than exercised as scoping.
        let xml = br#"<ContractAwardNotice>
              <LotResult>
                <TenderLot><ID schemeName="lot">LOT-1</ID><ItemClassificationCode>72000000</ItemClassificationCode></TenderLot>
                <LotValue><TotalAmount currencyID="EUR">100</TotalAmount></LotValue>
                <LotTender>
                  <ID schemeName="tender">TEN-1</ID>
                  <TenderingParty><ID schemeName="tendering-party">TPA-1</ID></TenderingParty>
                  <TenderLot><ID schemeName="lot">LOT-1</ID></TenderLot>
                </LotTender>
                <TenderingParty>
                  <ID schemeName="tendering-party">TPA-1</ID>
                  <Tenderer><ID schemeName="organization">ORG-1</ID></Tenderer>
                </TenderingParty>
                <Party>
                  <PartyIdentification><ID>ORG-1</ID></PartyIdentification>
                  <PartyLegalEntity><CompanyID>0123456789</CompanyID></PartyLegalEntity>
                </Party>
              </LotResult>
              <LotResult>
                <TenderLot><ID schemeName="lot">LOT-2</ID><ItemClassificationCode>45000000</ItemClassificationCode></TenderLot>
                <LotValue><TotalAmount currencyID="EUR">200</TotalAmount></LotValue>
                <LotTender>
                  <ID schemeName="tender">TEN-2</ID>
                  <TenderingParty><ID schemeName="tendering-party">TPA-2</ID></TenderingParty>
                  <TenderLot><ID schemeName="lot">LOT-2</ID></TenderLot>
                </LotTender>
                <TenderingParty>
                  <ID schemeName="tendering-party">TPA-2</ID>
                  <Tenderer><ID schemeName="organization">ORG-2</ID></Tenderer>
                </TenderingParty>
                <Party>
                  <PartyIdentification><ID>ORG-2</ID></PartyIdentification>
                  <PartyLegalEntity><CompanyID>0404483367</CompanyID></PartyLegalEntity>
                </Party>
              </LotResult>
            </ContractAwardNotice>"#;
        let parsed = parse_award_xml(xml, "778899-2026").expect("fixture must parse");
        assert_eq!(
            parsed.len(),
            2,
            "the fixture must describe two lots, got {parsed:?}"
        );
        assert_eq!(parsed[0].lot_id, "LOT-1", "{parsed:?}");
        assert_eq!(
            parsed[0].winner_identifier.as_deref(),
            Some("0123456789"),
            "lot 1 winner: {parsed:?}"
        );
        assert_eq!(
            parsed[1].winner_identifier.as_deref(),
            Some("0404483367"),
            "lot 2 winner: {parsed:?}"
        );
        assert_eq!(
            parsed[0].cpv_codes,
            vec!["72000000".to_owned()],
            "{parsed:?}"
        );
        assert_eq!(
            parsed[1].cpv_codes,
            vec!["45000000".to_owned()],
            "{parsed:?}"
        );
        let downloads = std::cell::Cell::new(0_usize);
        let (stored, companies) = store_notice_awards(&store, &notice, |_url| {
            downloads.set(downloads.get() + 1);
            Some(xml.to_vec())
        })
        .expect("awards should store");
        assert_eq!(downloads.get(), 1, "one download served both members");
        assert_eq!(stored, 2);
        assert_eq!(companies.len(), 2);
        // Winners are stored under their normalised enterprise number.
        let first = store.awards_for("0123.456.789").expect("read should work");
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].cpv_codes,
            vec!["72000000".to_owned()],
            "the first member must not inherit the second member's lot"
        );
        let second = store.awards_for("0404.483.367").expect("read should work");
        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0].cpv_codes,
            vec!["45000000".to_owned()],
            "the second member keeps only its own lot"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// TED writes winners as `BE 0838560941`, `0404.483.367` or bare digits.
    /// Foreign winners must be dropped: a SIRET in the history table can never be
    /// looked up and would inflate the company count.
    #[test]
    fn winner_identifiers_normalise_to_belgian_enterprise_numbers() {
        assert_eq!(
            belgian_enterprise("BE 0838560941").as_deref(),
            Some("0838.560.941")
        );
        assert_eq!(
            belgian_enterprise("0404.483.367").as_deref(),
            Some("0404.483.367")
        );
        assert_eq!(
            belgian_enterprise("0406486616").as_deref(),
            Some("0406.486.616")
        );
        // A French SIRET and a short number are not Belgian enterprise numbers.
        assert_eq!(belgian_enterprise("55210055400013"), None);
        assert_eq!(belgian_enterprise("12345"), None);
        assert_eq!(belgian_enterprise(""), None);
    }

    #[test]
    fn terminal_safe_escapes_control_characters() {
        assert_eq!(
            terminal_safe("notice\u{1b}[31m\n"),
            "notice\\u{001B}[31m\\u{000A}"
        );
    }

    #[test]
    fn parses_flags_before_positional_enterprise_number() {
        let options = parse_args(
            ["--database", "test.duckdb", "--limit", "50", "0404484060"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number.as_deref(), Some("0404.484.060"));
        assert_eq!(options.limit, 50);
        assert_eq!(options.database, "test.duckdb");
    }

    #[test]
    fn parses_positional_enterprise_number_before_flags() {
        let options = parse_args(
            ["0404.484.060", "--limit", "20"]
                .into_iter()
                .map(String::from),
        )
        .expect("arguments should parse")
        .expect("help was not requested");
        assert_eq!(options.enterprise_number.as_deref(), Some("0404.484.060"));
        assert_eq!(options.limit, 20);
    }

    #[test]
    fn accepts_help_without_enterprise_number() {
        assert!(parse_args(["--help"].into_iter().map(String::from))
            .expect("help should parse")
            .is_none());
    }

    #[test]
    fn rejects_duplicate_enterprise_numbers() {
        assert!(parse_args(["0404484060", "0404484060"].into_iter().map(String::from),).is_err());
    }

    #[test]
    fn requires_enterprise_number() {
        assert!(parse_args(std::iter::empty::<String>()).is_err());
    }

    #[test]
    fn rejects_missing_values_and_unknown_arguments() {
        assert!(parse_args(["0404484060", "--limit"].into_iter().map(String::from),).is_err());
        assert!(parse_args(["0404484060", "--database"].into_iter().map(String::from),).is_err());
        assert!(parse_args(["0404484060", "--unknown"].into_iter().map(String::from),).is_err());
    }

    #[test]
    fn rejects_invalid_or_out_of_range_limits() {
        assert!(parse_args(
            ["0404484060", "--limit", "abc"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
        assert!(parse_args(["0404484060", "--limit", "0"].into_iter().map(String::from)).is_err());
        assert!(parse_args(
            [
                "0404484060",
                "--limit",
                &(MAX_TED_SEARCH_RESULTS + 1).to_string(),
            ]
            .into_iter()
            .map(String::from),
        )
        .is_err());
    }

    #[test]
    fn explicit_range_is_exclusive_with_other_modes() {
        let args = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let options = parse_args(args(&["--from", "2022-01-01", "--to", "2022-12-31"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            options.range,
            Some((
                NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(),
                NaiveDate::from_ymd_opt(2022, 12, 31).unwrap()
            ))
        );
        assert!(parse_args(args(&["--from", "2022-01-01"])).is_err());
        assert!(parse_args(args(&["--from", "2023-01-01", "--to", "2022-01-01"])).is_err());
        assert!(parse_args(args(&[
            "--window",
            "30",
            "--from",
            "2022-01-01",
            "--to",
            "2022-02-01"
        ]))
        .is_err());
        assert!(parse_args(args(&[
            "0404.483.367",
            "--from",
            "2022-01-01",
            "--to",
            "2022-02-01"
        ]))
        .is_err());
    }
}
