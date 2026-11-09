use aanbestedingen_matcher::recommendations::corpus_evidence;
use aanbestedingen_matcher::store::TenderStore;
use aanbestedingen_matcher::ted::{
    incremental_floor, month_chunks, SearchPage, SearchScope, SearchWindow, TedClient,
    DEFAULT_LOOKBACK_DAYS, DEFAULT_QUERY, INCREMENTAL_OVERLAP_DAYS, MAX_BACKFILL_MONTHS,
    MAX_TED_SEARCH_RESULTS,
};
use chrono::NaiveDate;
use std::env;
use std::time::Duration;

/// Fetch cap when the caller does not name one. A windowed refresh has to cover
/// everything published in the window, not a single page.
const DEFAULT_INGEST_LIMIT: usize = MAX_TED_SEARCH_RESULTS;

#[derive(Debug, PartialEq)]
struct TedIngestOptions {
    query: String,
    limit: usize,
    scope: SearchScope,
    days: i64,
    /// Re-read from the last recorded refresh (minus the overlap) instead of the
    /// whole lookback window. Off by default: narrowing a window changes which
    /// population a run covers, and that must be a visible choice.
    incremental: bool,
    /// Days re-read even when the previous run was recent, so a notice touched
    /// after its window closed is still refreshed.
    overlap_days: i64,
    /// Closed publication range for a history backfill; when set, `--days` and
    /// `--scope` are ignored because a backfill walks the whole archive in
    /// month-sized chunks.
    from: Option<NaiveDate>,
    until: Option<NaiveDate>,
    database: String,
}

fn parse_args<I>(args: I) -> Result<Option<TedIngestOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut query = DEFAULT_QUERY.to_owned();
    let mut limit = DEFAULT_INGEST_LIMIT;
    let mut scope = SearchScope::default();
    let mut days = DEFAULT_LOOKBACK_DAYS;
    let mut incremental = false;
    let mut overlap_days = INCREMENTAL_OVERLAP_DAYS;
    let mut overlap_given = false;
    let mut from = None;
    let mut until = None;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut arguments = args.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--query" => query = arguments.next().ok_or("--query needs a value")?,
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
            "--scope" => {
                scope = SearchScope::parse(&arguments.next().ok_or("--scope needs a value")?)?
            }
            "--days" => {
                let value: i64 = arguments.next().ok_or("--days needs a value")?.parse()?;
                if value < 0 {
                    return Err("--days must not be negative".into());
                }
                days = value;
            }
            "--incremental" => incremental = true,
            "--overlap-days" => {
                let value: i64 = arguments
                    .next()
                    .ok_or("--overlap-days needs a value")?
                    .parse()?;
                if !(0..=365).contains(&value) {
                    return Err("--overlap-days must be between 0 and 365".into());
                }
                overlap_days = value;
                overlap_given = true;
            }
            "--from" => {
                let value = arguments.next().ok_or("--from needs a value")?;
                from = Some(NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").map_err(
                    |_| -> Box<dyn std::error::Error> {
                        format!("--from must be a YYYY-MM-DD date, got {value}").into()
                    },
                )?);
            }
            "--to" => {
                let value = arguments.next().ok_or("--to needs a value")?;
                until = Some(NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").map_err(
                    |_| -> Box<dyn std::error::Error> {
                        format!("--to must be a YYYY-MM-DD date, got {value}").into()
                    },
                )?);
            }
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--help" | "-h" => return Ok(None),
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }

    match (from, until) {
        (Some(from), Some(until)) => {
            if from > until {
                return Err(format!("--from {from} is after --to {until}").into());
            }
            let months = month_chunks(from, until);
            if months.len() > MAX_BACKFILL_MONTHS {
                return Err(format!(
                    "a backfill covers at most {MAX_BACKFILL_MONTHS} monthly chunk(s); \
                     {} requested; split it into several runs",
                    months.len()
                )
                .into());
            }
        }
        (None, None) => {}
        _ => return Err("--from and --to must be given together".into()),
    }

    if incremental && from.is_some() {
        return Err(
            "--incremental applies to a rolling refresh; a --from/--to backfill already names \
             its range"
                .into(),
        );
    }
    if overlap_given && !incremental {
        return Err("--overlap-days only applies together with --incremental".into());
    }
    // `--days 0 --incremental` is allowed: the recorded watermark bounds the window,
    // and whether there is one can only be known with the database in hand, so that
    // decision belongs to `run`.

    Ok(Some(TedIngestOptions {
        query,
        limit,
        scope,
        days,
        incremental,
        overlap_days,
        from,
        until,
        database,
    }))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: ted-ingest [--query QUERY] [--limit N] [--scope latest|active|all] [--days N] [--database PATH]\n\
             \n\
             Fetches a window of TED's population (default: the active population of the last \
             {DEFAULT_LOOKBACK_DAYS} days). Do not use --scope latest for a refresh: it covers a \
             single official-journal issue and cannot backfill.\n\
             \n\
             --limit caps one search, so on a window walked month by month it applies to each\n\
             chunk: --days 90 --limit 100 stores up to 100 per month, not 100 in total. It is\n\
             a sampling knob; a refresh that covers the population leaves it at the\n\
             default.\n\
             \n\
             History backfill, one month-sized chunk per call so the response budget never binds:\n\
             \n\
             ted-ingest --from 2024-01-01 --to 2024-06-30 --database PATH\n\
             \n\
             Incremental refresh, starting from the last recorded refresh plus an overlap instead \
             of the whole window:\n\
             \n\
             ted-ingest --days 30 --incremental --database PATH\n\
             \n\
             Each chunk is stored as it completes and a failed chunk is retried once, so a failure \
             names the chunk that has to be resumed and everything before it stays in the database."
        );
        return Ok(());
    };
    // UTC, matching the watermark. `refresh_runs.started_at` is a UTC timestamp and
    // `last_refresh_date` reads its UTC date, so a local date west of UTC can lag
    // it by a day; `incremental_floor` would then treat the watermark as being in
    // the future and fall back to the full window, silently cancelling the
    // incremental run.
    run(
        options,
        &TedClient::default(),
        chrono::Utc::now().date_naive(),
    )
}

/// The ingest itself, with the client and the calendar day injected.
///
/// Separate from `main` so the composed path — recorded watermark, chosen
/// window, the query that reaches TED, and the provenance row — can be checked
/// against a fake endpoint instead of being inferred from three unit tests.
fn run(
    options: TedIngestOptions,
    client: &TedClient,
    today: NaiveDate,
) -> Result<(), Box<dyn std::error::Error>> {
    let TedIngestOptions {
        query,
        limit,
        scope,
        days,
        incremental,
        overlap_days,
        from,
        until,
        database,
    } = options;

    if let (Some(from), Some(until)) = (from, until) {
        return backfill_with(
            &query,
            limit,
            from,
            until,
            &database,
            client,
            ChunkRetry::default(),
        );
    }

    // `--days 0` means no publication bound, and that is one unbounded page
    // sequence: TED refuses pages past roughly sixty, so it cannot complete —
    // it would spend ~60 requests and then fail with nothing stored. The scope
    // that inspects a single official-journal issue is exempt, because it has no
    // range to walk. Enforced here rather than in `parse_args` so every caller,
    // including tests that build options directly, gets the same rule.
    let floor = (days > 0).then(|| today - chrono::Duration::days(days));
    // The watermark is read before the window is chosen: with one recorded, `--days`
    // is irrelevant, and its presence is what makes the walk bounded.
    let last = if incremental {
        recorded_watermark(&database)?
    } else {
        None
    };
    let since = if incremental {
        incremental_floor(days, today, last, overlap_days)
    } else {
        floor
    };
    // A window with no publication bound is one page sequence, and TED refuses those
    // past roughly sixty pages: it would spend ~60 requests and fail with nothing
    // stored. The check is on the *window*, not on `days`, because `--days 0
    // --incremental` against a store that recorded a refresh is bounded by its
    // watermark.
    if since.is_none() && scope != SearchScope::Latest {
        return Err(
            "an unbounded refresh cannot clear TED's ~60-page wall; use --from/--to for a \
             history range, --incremental on a store that recorded a refresh, or \
             --scope latest to inspect the current issue"
                .into(),
        );
    }
    // Announced only after the bound is known to be usable, and naming the window that
    // was actually chosen: an overlap the floor dropped must not be claimed.
    if incremental {
        match (last, since) {
            (Some(last), Some(since)) => println!(
                "Incremental: last recorded refresh {last}; fetching since {since} — that \
                 refresh minus {overlap_days} day(s) of overlap, which also covers any \
                 idle period since it"
            ),
            (None, Some(since)) => println!(
                "Incremental: no recorded refresh in this database, so the full {days}-day \
                 window from {since} is the only safe floor"
            ),
            (_, None) => {
                println!("Incremental: no window bound applies, and --scope latest walks no range")
            }
        }
    }
    let window = SearchWindow::rolling(since, scope);
    // A window wider than one chunk is walked month by month instead of in one
    // deep page sequence: TED refuses pages past roughly sixty with `400`
    // (`docs/mission.md`), and at the measured density (~63 notices/day) a
    // 30-day window is ~19 pages. A single 180-day search would ask for ~113
    // pages and die at the wall having stored nothing, because the search
    // completes before the first upsert.
    let bounds: Vec<(Option<NaiveDate>, Option<NaiveDate>)> = match since {
        Some(from) if (today - from).num_days() > MAX_SINGLE_WINDOW_DAYS => {
            let chunks = month_chunks(from, today);
            // An incremental floor can reach further back than `--days` (that is the
            // point: an idle store is fetched from its watermark), so this path needs
            // the same ceiling the explicit backfill has. Without it a stale or
            // corrupt watermark turns one scheduled run into an unbounded walk.
            if chunks.len() > MAX_BACKFILL_MONTHS {
                return Err(format!(
                    "the incremental floor {from} needs {} monthly chunk(s), past the \
                     {MAX_BACKFILL_MONTHS}-chunk ceiling; backfill it deliberately: \
                     ted-ingest --from {} --to {} --database {database}",
                    chunks.len(),
                    from,
                    today
                )
                .into());
            }
            let last = chunks.len().saturating_sub(1);
            chunks
                .into_iter()
                .enumerate()
                .map(|(index, (from, until))| {
                    // The final chunk stays open-ended, exactly like an unchunked
                    // rolling window. TED pre-releases OJ S issues, so a notice can
                    // carry a publication date after `today`; bounding the last
                    // chunk would include it below 30 days and drop it above.
                    (Some(from), (index != last).then_some(until))
                })
                .collect()
        }
        _ => vec![(window.published_since, window.published_until)],
    };
    let chunked = bounds.len() > 1;
    // The description this run will record. Built before the walk so a failed
    // chunk can still leave provenance for the notices it already stored.
    let window_desc = match (window.published_since, floor) {
        (None, _) => "no publication bound".to_owned(),
        (Some(since), Some(floor)) if since == floor => format!("days={days}"),
        (Some(since), _) => format!("days={days} since={since}"),
    };
    println!(
        "Fetching up to {limit} notices{}: scope {}, {}",
        if chunked { " per chunk" } else { "" },
        window.scope.as_str(),
        match window.published_since {
            Some(date) => format!("published since {date}"),
            None => "no publication bound".to_owned(),
        }
    );
    if chunked {
        println!(
            "Walking {} month-sized chunk(s) so no single page sequence reaches TED's page wall",
            bounds.len()
        );
    }
    let started = std::time::Instant::now();
    // Opened lazily: a search that fails must leave no database behind, which is
    // what `a_failed_incremental_fetch_creates_no_database` pins.
    let mut store: Option<TenderStore> = None;
    let mut notices_seen = 0_usize;
    let mut stored = 0_usize;
    let mut opportunities = 0_usize;
    // Summed across chunks. `month_chunks` yields disjoint, consecutive ranges
    // (`month_chunks_cover_the_range_without_gaps_or_overlap`), and TED's
    // `totalNoticeCount` is the untruncated population of the query it answered,
    // so the sum is the population of the whole run. Keeping only the first
    // chunk's number made the dashboard read "N fetched of Y" with a Y that
    // covered a quarter of the range.
    let mut total_notice_count: i64 = 0;
    let mut total_reported = false;
    // A chunk that reports no population makes the sum incomplete, so the run's
    // total becomes unknown rather than under-reported.
    let mut total_missing = false;
    for (index, (chunk_from, chunk_until)) in bounds.iter().enumerate() {
        let chunk_window = SearchWindow {
            scope,
            published_since: *chunk_from,
            published_until: *chunk_until,
        };
        if chunked {
            println!(
                "  chunk {}/{}: {} .. {}",
                index + 1,
                bounds.len(),
                render_bound(*chunk_from),
                chunk_until.map(|date| date.to_string()).unwrap_or_default()
            );
        }
        // A chunk is retried once, like a backfill chunk: the page-level retries
        // live in the client, this catches the failure that outlasted them.
        let SearchPage {
            tenders,
            total_notice_count: total,
            arrived,
        } = match fetch_chunk(client, &query, limit, &chunk_window, ChunkRetry::default()) {
            Ok(result) => result,
            Err(error) => {
                if !chunked {
                    return Err(error);
                }
                // Notices from the chunks that completed are already stored, so
                // record them: an empty `refresh_runs` next to stored rows is
                // provenance that says nothing happened.
                let provenance_note = record_stopped_refresh(
                    store.as_ref(),
                    scope.as_str(),
                    &window_desc,
                    index,
                    bounds.len(),
                    "fetch failed",
                    notices_seen,
                    stored,
                    started,
                );
                return Err(format!(
                    "window chunk {} .. {} failed after {stored} notice(s) stored: {error}\n\
                     The rest of the range can be fetched as history, which uses scope ALL and \
                     records a backfill run: ted-ingest --from {} --to {} --database {database}\
                     {provenance_note}",
                    render_bound(*chunk_from),
                    render_bound(*chunk_until),
                    render_bound(*chunk_from),
                    today
                )
                .into());
            }
        };
        // Count what TED delivered, not what survived conversion: the shortfall this
        // run reports is against TED's own population.
        notices_seen += arrived;
        opportunities += tenders
            .iter()
            .filter(|tender| tender.is_opportunity())
            .count();
        if store.is_none() {
            store = Some(TenderStore::open(&database)?);
        }
        // A write failure is not a fetch failure, but it loses provenance the same
        // way: the chunks already stored would be left without a `refresh_runs` row,
        // which is the state the comment above forbids. Every stop inside this loop
        // therefore funnels through `record_stopped_refresh`.
        let chunk_stored = match store
            .as_ref()
            .expect("store was just opened")
            .upsert_many(tenders.iter())
        {
            Ok(count) => count,
            Err(error) => {
                let provenance_note = record_stopped_refresh(
                    store.as_ref(),
                    scope.as_str(),
                    &window_desc,
                    index,
                    bounds.len(),
                    "write failed",
                    notices_seen,
                    stored,
                    started,
                );
                return Err(format!(
                    "storing window chunk {} .. {} failed after {stored} notice(s) stored: \
                     {error}\nThe chunks already stored are kept. Resume with: \
                     ted-ingest --from {} --to {} --database {database}{provenance_note}",
                    render_bound(*chunk_from),
                    render_bound(*chunk_until),
                    render_bound(*chunk_from),
                    today
                )
                .into());
            }
        };
        stored += chunk_stored;
        // TED reports the population of the query it answered. Fetching fewer notices
        // than it reports means `--limit` cut the page sequence short, so this window
        // is only partly stored. Recording a completed refresh here would move the
        // watermark to today, and every later incremental run starts after the range
        // that was never fetched — the loss the incremental design exists to avoid.
        // Reporting the shortfall and leaving the watermark alone keeps it recoverable.
        if let Some(reported) = total {
            if (arrived as i64) < reported {
                let provenance_note = record_stopped_refresh(
                    store.as_ref(),
                    scope.as_str(),
                    &window_desc,
                    index + 1,
                    bounds.len(),
                    "stopped by the notice cap",
                    notices_seen,
                    stored,
                    started,
                );
                // Raising the limit is only a remedy while the population still fits
                // under the cap; past that, the window has to get narrower, and a
                // larger --limit would be refused at argument parsing.
                let remedy = if reported > MAX_TED_SEARCH_RESULTS as i64 {
                    format!(
                        "The client's {MAX_TED_SEARCH_RESULTS}-notice cap is the binding limit, so \
                         the window is too wide for one walk: narrow it (smaller --days, or a \
                         --from/--to range per month)"
                    )
                } else {
                    format!(
                        "Raise --limit (the client caps it at {MAX_TED_SEARCH_RESULTS}), or fetch \
                         the range as history"
                    )
                };
                return Err(format!(
                    "TED reported {reported} notice(s) for {} .. {} but only {arrived} arrived \
                     (--limit {limit}), so this window is incomplete and the watermark is left \
                     unchanged.\n{remedy}: ted-ingest --from {} --to {} --database \
                     {database}{provenance_note}",
                    render_bound(*chunk_from),
                    render_bound(*chunk_until),
                    render_bound(*chunk_from),
                    today
                )
                .into());
            }
        }
        match total {
            Some(total) => {
                total_notice_count = total_notice_count.saturating_add(total);
                total_reported = true;
            }
            None => total_missing = true,
        }
    }
    let total_notice_count = (total_reported && !total_missing).then_some(total_notice_count);
    let store = store.expect("at least one chunk ran");
    let window_desc = if chunked {
        format!("{window_desc} ({} chunks)", bounds.len())
    } else {
        window_desc
    };
    let run_id = store.record_refresh_run(
        "refresh",
        window.scope.as_str(),
        &window_desc,
        notices_seen as i64,
        stored as i64,
        total_notice_count,
        started.elapsed().as_millis() as i64,
    )?;
    // Whole-store evidence snapshot for the dashboard: same definition as
    // `report`, computed once here instead of per status request. Skipped when
    // the store exceeds the reporting cap, so a huge import cannot blow the
    // ingest's memory on the snapshot alone.
    const MAX_EVIDENCE_TENDERS: usize = 100_000;
    if store.count()? <= MAX_EVIDENCE_TENDERS as i64 {
        let snapshot = store.tenders(MAX_EVIDENCE_TENDERS)?;
        let evidence = corpus_evidence(&snapshot);
        store.record_refresh_evidence(run_id, &evidence)?;
    }
    println!(
        "Fetched {notices_seen} notices ({opportunities} calls for competition); stored {stored}; \
         database contains {} total",
        store.count()?
    );
    if let Some(total) = total_notice_count {
        println!("TED reports {total} notices in this population");
    }
    Ok(())
}

/// The last recorded refresh date, read without creating a store.
///
/// A read-only open matters: opening a writable store here would create an
/// empty database even when the run then fails to fetch anything, which breaks
/// the loud-clean-failure property (no database, no row).
fn recorded_watermark(database: &str) -> Result<Option<NaiveDate>, Box<dyn std::error::Error>> {
    if !std::path::Path::new(database).exists() {
        return Ok(None);
    }
    TenderStore::open_read_only(database)?.last_refresh_date()
}

/// The widest window one page sequence may cover.
///
/// TED's search refuses pages past roughly sixty with `400` (`docs/mission.md`),
/// and at the measured ~63 notices/day a 30-day window is ~19 pages, so 30 days
/// keeps a single sequence well inside the wall. A wider window is walked in
/// month chunks instead, which is the same constraint the award ingest and the
/// history backfill already respect.
const MAX_SINGLE_WINDOW_DAYS: i64 = 30;

/// Retry budget for one backfill chunk.
///
/// A chunk is already retried page by page; this is the second layer for a
/// failure that outlasted those page attempts (a throttle that has to cool
/// down, a truncated response). Two attempts and a short pause keep a long
/// backfill from stopping for a single blip while still failing loudly.
///
/// The retry is unconditional, including for a rejected query, because the TED
/// client's errors are still untyped strings and guessing from the message would
/// be worse than one extra request: a wrong query fails on the first chunk, not
/// on the thirtieth, and the printed first-attempt error still names it.
#[derive(Debug, Clone, Copy)]
struct ChunkRetry {
    attempts: usize,
    pause: Duration,
}

impl Default for ChunkRetry {
    fn default() -> Self {
        Self {
            attempts: 2,
            pause: Duration::from_secs(5),
        }
    }
}

/// Walk a closed publication range in month-sized chunks, storing each chunk as
/// it completes: a single call for a year would exceed the 64 MiB response
/// budget, and a failure late in a long run must not throw away the months
/// already fetched. The client is injected, so the failure path can be tested
/// without touching the network.
fn backfill_with(
    query: &str,
    limit: usize,
    from: NaiveDate,
    until: NaiveDate,
    database: &str,
    client: &TedClient,
    retry: ChunkRetry,
) -> Result<(), Box<dyn std::error::Error>> {
    let chunks = month_chunks(from, until);
    println!(
        "Backfilling {from} .. {until} in {} month-sized chunk(s), up to {limit} notices each",
        chunks.len()
    );
    let store = TenderStore::open(database)?;
    let mut total = 0_usize;
    let mut stored_total = 0_usize;
    for (index, (chunk_from, chunk_until)) in chunks.iter().enumerate() {
        let window = SearchWindow::range(*chunk_from, *chunk_until);
        println!(
            "  chunk {}/{}: {chunk_from} .. {chunk_until}",
            index + 1,
            chunks.len()
        );
        let chunk_started = std::time::Instant::now();
        let tenders = match fetch_chunk(client, query, limit, &window, retry) {
            Ok(tenders) => tenders,
            Err(error) => {
                return Err(format!(
                    "chunk {chunk_from} .. {chunk_until} failed after {stored_total} notice(s) \
                     stored: {error}\n\
                     Resume from the failed chunk with: --from {chunk_from} --to {until}"
                )
                .into())
            }
        };
        let SearchPage {
            tenders,
            total_notice_count,
            arrived,
        } = tenders;
        // A backfill chunk that delivered fewer notices than TED reported is incomplete
        // because `--limit` cut it short. Backfill rows never move the watermark, so
        // this is not the permanent loss a refresh would cause — but a month that is
        // silently partial is still wrong provenance, so say so rather than imply the
        // chunk is whole.
        if let Some(reported) = total_notice_count {
            if (arrived as i64) < reported {
                println!(
                    "    WARNING: TED reported {reported} notice(s) for {chunk_from} .. \
                     {chunk_until} but only {arrived} arrived (--limit {limit}); this chunk is \
                     incomplete"
                );
            }
        }
        let stored = store.upsert_many(tenders.iter())?;
        total += tenders.len();
        stored_total += stored;
        store.record_refresh_run(
            "backfill",
            window.scope.as_str(),
            &format!("{chunk_from}..{chunk_until}"),
            tenders.len() as i64,
            stored as i64,
            total_notice_count,
            chunk_started.elapsed().as_millis() as i64,
        )?;
        println!("    fetched {} notice(s), stored {stored}", tenders.len());
    }
    println!(
        "Backfill complete: {total} notice(s) fetched, {stored_total} stored; database contains {} total",
        store.count()?
    );
    Ok(())
}

/// One backfill chunk, tried at most `retry.attempts` times.
///
/// Renders a chunk bound for an operator-facing message. An open end is spelled
/// "open" rather than left blank, so "2026-11-06 .." cannot read like a typo.
fn render_bound(date: Option<NaiveDate>) -> String {
    date.map(|date| date.to_string())
        .unwrap_or_else(|| "open".to_owned())
}

/// Records what a walk that stopped early actually stored.
///
/// The kind is `refresh-partial`, never `refresh`: `last_refresh_date` reads only
/// completed refreshes, so a stopped walk cannot move the watermark and the next
/// incremental run starts before the range this one never fetched. The population
/// is recorded as unknown rather than as the completed chunks' sum, because the
/// chunks that were never fetched have no reported population.
///
/// Returns the note to append to the failure message: provenance that silently did
/// not happen is the same as no provenance at all.
#[allow(clippy::too_many_arguments)]
fn record_stopped_refresh(
    store: Option<&TenderStore>,
    scope: &str,
    window_desc: &str,
    chunks_walked: usize,
    chunks_total: usize,
    stop: &str,
    notices_seen: usize,
    stored: usize,
    started: std::time::Instant,
) -> String {
    let Some(store) = store else {
        // Nothing was stored either, so there is nothing to attribute.
        return String::new();
    };
    // "N of M chunks stored" read as progress even when the chunk that answered
    // short was counted as stored, so a run could say "1 of 1 chunks stored,
    // incomplete from chunk 1". Count the chunks the walk *reached* and name why
    // it stopped; a single-chunk window has no progress to report.
    let progress = if chunks_total > 1 {
        format!("{chunks_walked} of {chunks_total} chunks walked, {stop}")
    } else {
        stop.to_owned()
    };
    match store.record_refresh_run(
        "refresh-partial",
        scope,
        &format!("{window_desc} ({progress})"),
        notices_seen as i64,
        stored as i64,
        None,
        started.elapsed().as_millis() as i64,
    ) {
        Ok(_) => String::new(),
        Err(write) => format!("\nWARNING: the partial run could not be recorded: {write}"),
    }
}

/// Page-level retrying (inside the client) handles a throttled page; this layer
/// only catches a chunk that still failed, so a long backfill is not restarted
/// by hand for one blip. The last error is returned unchanged, so the caller's
/// resume-point message keeps describing the real failure.
fn fetch_chunk(
    client: &TedClient,
    query: &str,
    limit: usize,
    window: &SearchWindow,
    retry: ChunkRetry,
) -> Result<SearchPage, Box<dyn std::error::Error>> {
    let attempts = retry.attempts.max(1);
    let mut last_error = None;
    for attempt in 1..=attempts {
        match client.search_window_with_total(query, limit, window) {
            Ok(result) => return Ok(result),
            Err(error) => {
                if attempt < attempts {
                    println!(
                        "    chunk attempt {attempt}/{attempts} failed ({error}); retrying in {}s",
                        retry.pause.as_secs()
                    );
                    std::thread::sleep(retry.pause);
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error.expect("at least one attempt ran"))
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
    fn uses_default_options() {
        assert_eq!(
            parse_args(arguments(&[])).expect("arguments should parse"),
            Some(TedIngestOptions {
                query: DEFAULT_QUERY.to_owned(),
                limit: DEFAULT_INGEST_LIMIT,
                scope: SearchScope::Active,
                days: DEFAULT_LOOKBACK_DAYS,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: "data/tenders-rust.duckdb".to_owned(),
            })
        );
    }

    /// The wiring itself: a recorded refresh narrows the next window, and reading
    /// the watermark must not create a store (a failed run has to leave no
    /// database behind).
    #[test]
    fn an_incremental_run_starts_from_the_recorded_watermark() {
        let path = scratch_path("watermark");
        {
            let store = TenderStore::open(&path).expect("store should open");
            store
                .record_refresh_run("refresh", "ACTIVE", "days=30", 10, 10, None, 5)
                .expect("run should record");
        }
        let last = recorded_watermark(&path.display().to_string()).expect("watermark should read");
        let today = chrono::Utc::now().date_naive();
        assert!(last.is_some(), "a recorded refresh sets the watermark");
        let floor =
            incremental_floor(30, today, last, INCREMENTAL_OVERLAP_DAYS).expect("a bounded window");
        assert!(
            floor > today - chrono::Duration::days(30),
            "the incremental run must start later than the full window: {floor}"
        );
        // An absent database has no watermark, and reading one must not create it.
        let missing = scratch_path("missing");
        assert_eq!(
            recorded_watermark(&missing.display().to_string()).expect("read should be fine"),
            None
        );
        assert!(
            !missing.exists(),
            "a watermark read must not create an empty store"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The widening has a ceiling. Reaching back to a stale watermark is the point,
    /// but an ancient one must fail loudly and name the backfill, not quietly turn a
    /// scheduled refresh into a years-long walk.
    #[test]
    fn an_ancient_watermark_is_refused_rather_than_walked() {
        let path = scratch_path("ancient-watermark");
        let today = chrono::Utc::now().date_naive();
        {
            let store = TenderStore::open(&path).expect("store should open");
            store
                .record_refresh_run("refresh", "ACTIVE", "days=30", 10, 10, None, 5)
                .expect("run should record");
        }
        // 40 months back, which is past the 36-month ceiling.
        let ancient = today - chrono::Duration::days(40 * 30);
        backdate_watermark(&path, ancient);
        let (endpoint, handle) = recording_ted(vec![("200 OK", NOTICE_BODY)]);
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        let error = run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 30,
                incremental: true,
                overlap_days: 7,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &client,
            today,
        )
        .expect_err("an ancient watermark must be refused");
        let message = error.to_string();
        assert!(
            message.contains("ceiling") && message.contains("--from"),
            "the refusal names the ceiling and the way forward: {message}"
        );
        assert!(
            handle.join().unwrap().is_empty(),
            "no request may be sent for a walk that is refused"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The whole incremental path, composed: a recorded run narrows the window,
    /// TED is asked for that narrower population, and the row records it. The
    /// floor maths and the watermark read are unit-tested separately; this checks
    /// that `run` actually joins them, which is what a regression would break.
    #[test]
    fn an_incremental_run_asks_ted_for_the_narrowed_window_and_records_it() {
        let path = scratch_path("e2e-incremental");
        {
            let store = TenderStore::open(&path).expect("store should open");
            store
                .record_refresh_run("refresh", "ACTIVE", "days=30", 10, 10, None, 5)
                .expect("run should record");
        }
        // Derive the day from the run that was just recorded, so this test does not
        // rot when the calendar moves past a hardcoded date.
        let today = TenderStore::open_read_only(&path)
            .expect("store should open read-only")
            .last_refresh_date()
            .expect("watermark should read")
            .expect("the recorded run is the watermark");
        let expected_floor = today - chrono::Duration::days(7);
        let (endpoint, handle) = recording_ted(vec![("200 OK", NOTICE_BODY)]);
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 30,
                incremental: true,
                overlap_days: 7,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &client,
            today,
        )
        .expect("the incremental refresh should run");
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].contains(&format!(
                "publication-date>={}",
                expected_floor.format("%Y%m%d")
            )),
            "TED must be asked for the narrowed window: {}",
            requests[0]
        );
        assert!(
            !requests[0].contains(&format!(
                "publication-date>={}",
                (today - chrono::Duration::days(30)).format("%Y%m%d")
            )),
            "the full 30-day floor must not be sent: {}",
            requests[0]
        );
        let store = TenderStore::open(&path).expect("store should open");
        let latest = store
            .latest_refresh_run()
            .expect("read should work")
            .expect("the run is recorded");
        // The recorded watermark is the run's own day, so the floor is that day
        // minus the overlap.
        assert_eq!(latest.window, format!("days=30 since={expected_floor}"));
        let _ = std::fs::remove_file(&path);
    }

    /// Without a watermark the same code path must send the full configured
    /// window and record exactly `days=N`, so an incremental run on a fresh store
    /// is indistinguishable from an ordinary refresh.
    #[test]
    fn an_incremental_run_without_a_watermark_sends_the_full_window() {
        let path = scratch_path("e2e-full");
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let (endpoint, handle) = recording_ted(vec![("200 OK", NOTICE_BODY)]);
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 30,
                incremental: true,
                overlap_days: 7,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &client,
            today,
        )
        .expect("the refresh should run");
        let requests = handle.join().unwrap();
        assert!(
            requests[0].contains("publication-date>=20261007"),
            "no watermark means the configured 30-day floor: {}",
            requests[0]
        );
        let store = TenderStore::open(&path).expect("store should open");
        assert_eq!(
            store.latest_refresh_run().unwrap().unwrap().window,
            "days=30",
            "a full window records the plain provenance"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The incremental path reads the watermark through a read-only open before
    /// it fetches, and a failed fetch must still leave no database behind.
    #[test]
    fn a_failed_incremental_fetch_creates_no_database() {
        let path = scratch_path("e2e-nodb");
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        // Nothing listens on this port, so the search fails before any store is opened.
        let client = TedClient {
            endpoint: "http://127.0.0.1:1".to_owned(),
            timeout: Duration::from_secs(1),
        };
        assert!(run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 30,
                incremental: true,
                overlap_days: 7,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &client,
            today,
        )
        .is_err());
        assert!(
            !path.exists(),
            "a run that fetched nothing must not create an empty store"
        );
    }

    /// A window wider than one page sequence is walked month by month. TED refuses
    /// pages past roughly sixty, so `refresh-ted 180` used to ask for ~113 pages,
    /// get a `400` at the wall, and store nothing at all.
    #[test]
    fn a_wide_window_is_walked_in_month_chunks() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let from = today - chrono::Duration::days(90);
        let expected_chunks = month_chunks(from, today);
        assert!(expected_chunks.len() > 1, "the fixture must span months");
        let replies = vec![("200 OK", NOTICE_BODY); expected_chunks.len()];
        let (endpoint, handle) = recording_ted(replies);
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        let path = scratch_path("chunked-window");
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 90,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &client,
            today,
        )
        .expect("a wide window must be chunked, not refused by the page wall");
        let requests = handle.join().unwrap();
        assert_eq!(
            requests.len(),
            expected_chunks.len(),
            "one search per month chunk"
        );
        let last = expected_chunks.len() - 1;
        for (index, (request, (chunk_from, chunk_until))) in
            requests.iter().zip(expected_chunks.iter()).enumerate()
        {
            assert!(
                request.contains(&format!(
                    "publication-date>={}",
                    chunk_from.format("%Y%m%d")
                )),
                "each chunk is bounded on its own start: {request}"
            );
            if index == last {
                // The final chunk stays open-ended so a pre-released notice dated
                // after `today` is treated the same as in an unchunked window.
                assert!(
                    !request.contains("publication-date<="),
                    "the last chunk must not cap the upper bound: {request}"
                );
            } else {
                assert!(
                    request.contains(&format!(
                        "publication-date<={}",
                        chunk_until.format("%Y%m%d")
                    )),
                    "each chunk is bounded on its own end: {request}"
                );
            }
        }
        let store = TenderStore::open(&path).expect("store should open");
        let latest = store.latest_refresh_run().unwrap().unwrap();
        assert_eq!(
            latest.window,
            format!("days=90 ({} chunks)", expected_chunks.len()),
            "the recorded window says the range was walked in chunks"
        );
        assert_eq!(
            latest.fetched as usize,
            expected_chunks.len(),
            "fetched counts every chunk, not just the last one"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The reason chunking exists: a failure at TED's page wall used to discard a
    /// whole long fetch. A chunk that fails must keep the chunks already stored and
    /// say how to resume.
    #[test]
    fn a_chunk_failing_at_the_page_wall_keeps_the_chunks_already_stored() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let from = today - chrono::Duration::days(90);
        let expected_chunks = month_chunks(from, today);
        // Exactly the two requests the run will make: the server serves a fixed
        // count, so an extra reply would leave `join` waiting for a connection the
        // run never opens.
        let (endpoint, handle) =
            // Chunk 2 is retried once, so it costs two requests; serving exactly
            // those keeps `join` from waiting on a connection the run never opens.
            recording_ted(vec![
                ("200 OK", NOTICE_BODY),
                ("400 Bad Request", ""),
                ("400 Bad Request", ""),
            ]);
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        let path = scratch_path("chunked-failure");
        let error = run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 90,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &client,
            today,
        )
        .expect_err("the second chunk is rejected");
        let _ = handle.join();
        let message = error.to_string();
        let failed_chunk_start = expected_chunks[1].0;
        assert!(
            message.contains(&failed_chunk_start.to_string()),
            "names the chunk that failed: {message}"
        );
        assert!(
            message.contains("ted-ingest --from"),
            "says how to fetch the rest: {message}"
        );
        assert!(
            message.contains("scope ALL") && message.contains("backfill"),
            "the resume route changes scope and kind, so it must say so: {message}"
        );
        let store = TenderStore::open(&path).expect("the first chunk's store survives");
        assert!(
            store.count().unwrap() >= 1,
            "the chunk that completed must stay stored"
        );
        // Stored rows without a run row would be provenance claiming nothing
        // happened, so the partial walk records itself.
        let partial = store
            .latest_refresh_run()
            .expect("read should work")
            .expect("the partial walk must be recorded");
        assert_eq!(partial.kind, "refresh-partial", "it is not a coverage run");
        assert!(
            partial.window.contains("chunks walked, fetch failed"),
            "the row marks the run as partial: {}",
            partial.window
        );
        assert_eq!(partial.fetched, 1, "it reports what it actually stored");
        assert_eq!(
            partial.total_notice_count, None,
            "a failed walk cannot report the population of the chunks it never fetched"
        );
        // The critical property: a partial walk must not become the watermark, or
        // the next incremental run starts after the range that was never fetched.
        assert_eq!(
            store.last_refresh_date().expect("read should work"),
            None,
            "only a completed refresh sets the watermark"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A population that includes one unconvertible notice is still a complete walk.
    #[test]
    fn an_unconvertible_notice_does_not_read_as_a_shortfall() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        let (endpoint, handle) = recording_ted(vec![("200 OK", UNCONVERTIBLE_BODY)]);
        let path = scratch_path("unconvertible");
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 1,
                incremental: true,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            },
            today,
        )
        .expect("both notices arrived, so the walk is complete");
        let _ = handle.join();
        let store = TenderStore::open(&path).expect("store should open");
        assert_eq!(
            store.count().expect("count should read"),
            1,
            "the notice that converts is stored"
        );
        assert_eq!(
            store
                .latest_refresh_run()
                .expect("read should work")
                .expect("a completed walk records itself")
                .kind,
            "refresh",
            "the walk delivered TED's whole population, so it is coverage"
        );
        assert!(
            store
                .last_refresh_date()
                .expect("read should work")
                .is_some(),
            "a complete walk still advances the watermark"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// When the client's cap is what the window cannot fit, raising `--limit` is not a
    /// remedy the parser would accept, so the message has to say to narrow instead.
    #[test]
    fn a_window_larger_than_the_cap_says_to_narrow_it() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        let (endpoint, handle) = recording_ted(vec![("200 OK", OVER_CAP_BODY)]);
        let path = scratch_path("over-cap");
        let error = run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: MAX_TED_SEARCH_RESULTS,
                scope: SearchScope::Active,
                days: 1,
                incremental: true,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            },
            today,
        )
        .expect_err("12000 notices cannot fit one walk");
        let _ = handle.join();
        let message = error.to_string();
        assert!(
            message.contains("narrow") && !message.contains("Raise --limit"),
            "the remedy must be narrowing, not a limit the parser rejects: {message}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A walk cut short by `--limit` stores a subset while TED reports the whole
    /// population. Recording a completed refresh there would move the watermark to
    /// today, and every later incremental run starts after the range that was never
    /// fetched — a permanent loss. The run must instead record `refresh-partial`,
    /// leave the watermark alone, and name the shortfall.
    #[test]
    fn a_truncated_refresh_does_not_advance_the_watermark() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        let (endpoint, handle) = recording_ted(vec![("200 OK", TRUNCATED_BODY)]);
        let path = scratch_path("truncated-refresh");
        let error = run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 1,
                incremental: true,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            },
            today,
        )
        .expect_err("a window that fetched less than TED reported is not complete");
        let _ = handle.join();
        let message = error.to_string();
        assert!(
            message.contains("40") && message.contains("--limit 10"),
            "the failure names the population and the cap that cut it short: {message}"
        );
        let store = TenderStore::open(&path).expect("the stored notice survives");
        assert_eq!(
            store.count().expect("count should read"),
            1,
            "what was fetched is still stored"
        );
        let recorded = store
            .latest_refresh_run()
            .expect("read should work")
            .expect("the stopped walk records itself");
        assert_eq!(recorded.kind, "refresh-partial", "it is not a coverage run");
        // The store's own description of the run must not read as progress: this
        // window has one chunk, so "1 of 1 chunks stored" said the walk was
        // complete while the row meant the opposite. Name the cause instead.
        assert_eq!(
            recorded.window, "days=1 (stopped by the notice cap)",
            "the row says why the walk stopped, without a chunk count it cannot mean"
        );
        assert_eq!(
            store.last_refresh_date().expect("read should work"),
            None,
            "a truncated window must not become the watermark"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A chunked walk that stops early reports how many chunks it reached, so the
    /// row says where to resume instead of implying the whole range was covered.
    #[test]
    fn a_chunked_walk_that_stops_early_records_how_far_it_got() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        let (endpoint, handle) = recording_ted(vec![("200 OK", TRUNCATED_BODY)]);
        let path = scratch_path("chunked-truncated-refresh");
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 90,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            },
            today,
        )
        .expect_err("the first chunk was cut short, so the run is not complete");
        let _ = handle.join();
        let store = TenderStore::open(&path).expect("the stored notice survives");
        let recorded = store
            .latest_refresh_run()
            .expect("read should work")
            .expect("the stopped walk records itself");
        let chunks_total = month_chunks(today - chrono::Duration::days(90), today).len();
        assert_eq!(recorded.kind, "refresh-partial");
        assert_eq!(
            recorded.window,
            format!("days=90 (1 of {chunks_total} chunks walked, stopped by the notice cap)"),
            "a multi-chunk walk reports how far it got and why it stopped"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// TED reports the population per query, and a chunked run issues one query per
    /// month. Recording only the first chunk's number made the dashboard's
    /// "N fetched of Y" describe a fraction of the range it had just fetched.
    #[test]
    fn a_chunked_run_records_the_population_of_the_whole_range() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 6).unwrap();
        let from = today - chrono::Duration::days(90);
        let expected_chunks = month_chunks(from, today);
        let replies = vec![("200 OK", TOTALLED_BODY); expected_chunks.len()];
        let (endpoint, handle) = recording_ted(replies);
        let path = scratch_path("chunked-total");
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 90,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            },
            today,
        )
        .expect("the chunked run should complete");
        let _ = handle.join();
        let store = TenderStore::open(&path).expect("store should open");
        let latest = store.latest_refresh_run().unwrap().unwrap();
        assert_eq!(
            latest.total_notice_count,
            Some(expected_chunks.len() as i64),
            "the population covers every chunk, not just the first"
        );
        // A run whose endpoint reports no population must stay unknown, not zero.
        let (endpoint, handle) = recording_ted(vec![("200 OK", NOTICE_BODY)]);
        let silent = scratch_path("chunked-no-total");
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 30,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: silent.display().to_string(),
            },
            &TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            },
            today,
        )
        .expect("a single-chunk run should complete");
        let _ = handle.join();
        let store = TenderStore::open(&silent).expect("store should open");
        assert_eq!(
            store
                .latest_refresh_run()
                .unwrap()
                .unwrap()
                .total_notice_count,
            None,
            "an unreported population is unknown, not zero"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&silent);
    }

    /// The default must not be a single official-journal issue: `latest` cannot
    /// backfill, so a refresh would silently lose every day it did not run.
    #[test]
    fn defaults_to_a_window_not_one_issue() {
        let options = parse_args(arguments(&[]))
            .expect("arguments should parse")
            .expect("defaults should be Some");
        assert_eq!(options.scope, SearchScope::Active);
        assert!(options.days > 0);
        assert!(
            options.limit > 1_000,
            "a window needs room to fit, got {}",
            options.limit
        );
    }

    #[test]
    fn accepts_explicit_options() {
        assert_eq!(
            parse_args(arguments(&[
                "--query",
                "buyer-country=BEL",
                "--limit",
                "25",
                "--scope",
                "all",
                "--days",
                "90",
                "--database",
                "test.duckdb",
            ]))
            .expect("arguments should parse"),
            Some(TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 25,
                scope: SearchScope::All,
                days: 90,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: "test.duckdb".to_owned(),
            })
        );
    }

    #[test]
    fn accepts_an_incremental_refresh() {
        let options = parse_args(arguments(&["--incremental", "--overlap-days", "3"]))
            .expect("arguments should parse")
            .expect("options should be Some");
        assert!(options.incremental);
        assert_eq!(options.overlap_days, 3);
        assert_eq!(
            parse_args(arguments(&["--incremental"]))
                .expect("arguments should parse")
                .expect("options should be Some")
                .overlap_days,
            INCREMENTAL_OVERLAP_DAYS,
            "the overlap has a documented default"
        );
        assert!(
            !parse_args(arguments(&[]))
                .expect("arguments should parse")
                .expect("options should be Some")
                .incremental,
            "narrowing the window must stay an explicit choice"
        );
    }

    /// Incremental flags that cannot mean anything are rejected instead of being
    /// silently ignored: a run that looks incremental but is not would quietly
    /// fetch the whole window again.
    #[test]
    fn rejects_incremental_flags_that_cannot_mean_anything() {
        for arguments_list in [
            vec!["--overlap-days", "3"],
            vec![
                "--incremental",
                "--from",
                "2024-01-01",
                "--to",
                "2024-02-01",
            ],
            vec!["--incremental", "--overlap-days", "366"],
        ] {
            assert!(
                parse_args(arguments(&arguments_list)).is_err(),
                "{arguments_list:?} must be rejected"
            );
        }
    }

    /// `--days 0 --incremental` parses: with a recorded watermark it means
    /// "everything since the last run", and only `run` can tell whether there is one.
    #[test]
    fn days_zero_is_accepted_for_an_incremental_run() {
        let options = parse_args(arguments(&["--incremental", "--days", "0"]))
            .expect("the combination should parse")
            .expect("no help was asked for");
        assert_eq!(options.days, 0);
        assert!(options.incremental);
    }

    /// A chunk that fails once (a throttle that outlasted the page retries) must
    /// not stop a long backfill, and the months around it must stay stored.
    #[test]
    fn a_chunk_that_fails_once_is_retried_and_the_backfill_completes() {
        let (endpoint, handle) = mock_ted(vec![
            ("200 OK", NOTICE_BODY),
            ("400 Bad Request", ""),
            ("200 OK", NOTICE_BODY),
        ]);
        let client = TedClient {
            endpoint,
            timeout: Duration::from_secs(5),
        };
        let path = scratch_path("retry");
        let from = NaiveDate::from_ymd_opt(2024, 8, 1).unwrap();
        let until = NaiveDate::from_ymd_opt(2024, 9, 30).unwrap();
        backfill_with(
            "buyer-country=BEL",
            10,
            from,
            until,
            &path.display().to_string(),
            &client,
            ChunkRetry {
                attempts: 2,
                pause: Duration::ZERO,
            },
        )
        .expect("one failed chunk attempt must be retried");
        handle.join().unwrap();
        let store = TenderStore::open(&path).expect("database should open");
        assert_eq!(store.count().unwrap(), 1, "both chunks are stored");
        let _ = std::fs::remove_file(&path);
    }

    /// Serve the given responses in order, so a late chunk can be made to fail
    /// deterministically without touching the network for real.
    fn mock_ted(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("port should bind");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            for (status, response_body) in responses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("Content-Length") {
                            length = value.trim().parse::<usize>().unwrap_or(0);
                        }
                    }
                }
                let mut request_body = vec![0; length];
                let _ = reader.read_exact(&mut request_body);
                let _ = write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
            }
        });
        (endpoint, handle)
    }

    /// Same as `mock_ted`, but returns each request body so a test can assert the
    /// population TED was actually asked for.
    ///
    /// The listener stops after an idle deadline instead of waiting for a fixed
    /// number of connections: a run that makes *fewer* requests than expected (the
    /// regression these tests exist to catch) must fail on the request count, not
    /// hang the suite on a `join` that can never return.
    ///
    /// The deadline is deliberately generous. It only ever elapses when the run
    /// under-requests, and a loaded runner that stalls a client thread for a moment
    /// must not turn that into a spurious transport failure — a flaky gate is worse
    /// than one that takes ten seconds to report a regression.
    /// How long a mock listener waits for the next connection before giving up.
    const IDLE_ACCEPT_DEADLINE: Duration = Duration::from_secs(10);

    fn recording_ted(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("port should bind");
        listener
            .set_nonblocking(true)
            .expect("listener should allow a deadline");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for (status, response_body) in responses {
                let deadline = std::time::Instant::now() + IDLE_ACCEPT_DEADLINE;
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                return bodies;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return bodies,
                    }
                };
                let stream = stream;
                stream
                    .set_nonblocking(false)
                    .expect("accepted stream should block");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("Content-Length") {
                            length = value.trim().parse::<usize>().unwrap_or(0);
                        }
                    }
                }
                let mut request_body = vec![0; length];
                let _ = reader.read_exact(&mut request_body);
                bodies.push(String::from_utf8_lossy(&request_body).to_string());
                let _ = write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
            }
            bodies
        });
        (endpoint, handle)
    }

    /// Move the recorded watermark back, so the floor's width can be controlled
    /// without waiting for months of downtime.
    fn backdate_watermark(path: &std::path::Path, day: NaiveDate) {
        let connection = duckdb::Connection::open(path).expect("database should open");
        connection
            .execute(
                &format!(
                    "UPDATE refresh_runs SET started_at = CAST('{} 06:00:00+00' AS TIMESTAMPTZ)",
                    day.format("%Y-%m-%d")
                ),
                [],
            )
            .expect("watermark should backdate");
    }

    fn scratch_path(name: &str) -> std::path::PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("aanbestedingen-backfill-{name}-{suffix}.duckdb"))
    }

    const NOTICE_BODY: &str = r#"{"notices":[{"publication-number":"backfill-1","notice-title":"Works","notice-type":"cn-standard"}]}"#;

    /// A reply that reports a population matching the one notice it returns, so a
    /// completed walk is possible; the sum across chunks is what the run records.
    const TOTALLED_BODY: &str = r#"{"totalNoticeCount":1,"notices":[{"publication-number":"backfill-1","notice-title":"Works","notice-type":"cn-standard"}]}"#;

    /// The same reply while TED reports a population the page sequence never
    /// delivered: `--limit` cut the walk short.
    const TRUNCATED_BODY: &str = r#"{"totalNoticeCount":40,"notices":[{"publication-number":"backfill-1","notice-title":"Works","notice-type":"cn-standard"}]}"#;

    /// A population larger than the client's own cap, so no `--limit` can bring it
    /// in one walk.
    const OVER_CAP_BODY: &str = r#"{"totalNoticeCount":12000,"notices":[{"publication-number":"ok-1","notice-title":"Works","notice-type":"cn-standard"}]}"#;

    /// TED's population covers a notice this client cannot convert. The walk still
    /// delivered everything TED reported, so it is complete: the shortfall test has
    /// to count arrivals, not converted tenders, or one malformed notice would make
    /// every later refresh fail and never advance the watermark again.
    const UNCONVERTIBLE_BODY: &str = r#"{"totalNoticeCount":2,"notices":[{"publication-number":"ok-1","notice-title":"Works","notice-type":"cn-standard"},{"notice-title":"no identifier"}]}"#;

    /// Both chunks succeed: every month is stored, and one database holds them.
    #[test]
    fn backfill_stores_each_month_chunk() {
        let (endpoint, handle) = mock_ted(vec![("200 OK", NOTICE_BODY), ("200 OK", NOTICE_BODY)]);
        let client = TedClient {
            endpoint,
            timeout: std::time::Duration::from_secs(5),
        };
        let path = scratch_path("ok");
        let from = NaiveDate::from_ymd_opt(2024, 8, 1).unwrap();
        let until = NaiveDate::from_ymd_opt(2024, 9, 30).unwrap();
        backfill_with(
            "buyer-country=BEL",
            10,
            from,
            until,
            &path.display().to_string(),
            &client,
            ChunkRetry {
                attempts: 1,
                pause: Duration::ZERO,
            },
        )
        .expect("backfill should succeed");
        handle.join().unwrap();
        let store = TenderStore::open(&path).expect("database should open");
        assert_eq!(store.count().unwrap(), 1, "the notice is stored once");
        let _ = std::fs::remove_file(&path);
    }

    /// A failing later chunk must keep the months already stored and say which
    /// chunk to resume from — the promise the chunking exists for.
    #[test]
    fn a_failed_chunk_keeps_earlier_months_and_names_the_resume_point() {
        let (endpoint, handle) = mock_ted(vec![("200 OK", NOTICE_BODY), ("400 Bad Request", "")]);
        let client = TedClient {
            endpoint,
            timeout: std::time::Duration::from_secs(5),
        };
        let path = scratch_path("failure");
        let from = NaiveDate::from_ymd_opt(2024, 8, 1).unwrap();
        let until = NaiveDate::from_ymd_opt(2024, 9, 30).unwrap();
        let error = backfill_with(
            "buyer-country=BEL",
            10,
            from,
            until,
            &path.display().to_string(),
            &client,
            ChunkRetry {
                attempts: 1,
                pause: Duration::ZERO,
            },
        )
        .expect_err("the second chunk fails");
        handle.join().unwrap();
        let message = error.to_string();
        assert!(
            message.contains("2024-09-01"),
            "names the failed chunk: {message}"
        );
        assert!(
            message.contains("--from 2024-09-01"),
            "gives the resume point: {message}"
        );
        let store = TenderStore::open(&path).expect("database should open");
        assert_eq!(
            store.count().unwrap(),
            1,
            "the first chunk must survive the failure"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn accepts_a_closed_backfill_range() {
        let options = parse_args(arguments(&[
            "--from",
            "2024-01-15",
            "--to",
            "2024-03-02",
            "--database",
            "test.duckdb",
        ]))
        .expect("arguments should parse")
        .expect("backfill mode");
        assert_eq!(options.from.unwrap().to_string(), "2024-01-15");
        assert_eq!(options.until.unwrap().to_string(), "2024-03-02");
    }

    #[test]
    fn rejects_incomplete_or_nonsensical_backfill_ranges() {
        assert!(parse_args(arguments(&["--from", "2024-01-01"])).is_err());
        assert!(parse_args(arguments(&["--to", "2024-01-01"])).is_err());
        assert!(parse_args(arguments(&["--from", "01-01-2024", "--to", "2024-02-01"])).is_err());
        assert!(parse_args(arguments(&["--from", "2024-03-01", "--to", "2024-01-01"])).is_err());
        // A range longer than the cap is refused rather than run unbounded.
        assert!(parse_args(arguments(&["--from", "2016-01-01", "--to", "2026-01-01"])).is_err());
    }

    #[test]
    fn rejects_unknown_scopes_and_negative_windows() {
        assert!(parse_args(arguments(&["--scope", "everything"])).is_err());
        assert!(parse_args(arguments(&["--days", "-1"])).is_err());
        assert_eq!(
            parse_args(arguments(&["--days", "0"]))
                .expect("arguments should parse")
                .expect("0 parses; whether it is usable depends on the scope")
                .days,
            0
        );
    }

    /// `--days 0` used to mean "no bound" and then spend ~60 requests before
    /// failing at TED's page wall with nothing stored. It is refused before the
    /// first request, and only for a range.
    #[test]
    fn an_unbounded_refresh_is_refused_before_any_request() {
        let path = scratch_path("unbounded");
        let error = run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::All,
                days: 0,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            // Nothing listens here: if the guard were missing, this would be a
            // transport failure instead of the message below.
            &TedClient {
                endpoint: "http://127.0.0.1:1".to_owned(),
                timeout: Duration::from_secs(1),
            },
            NaiveDate::from_ymd_opt(2026, 11, 6).unwrap(),
        )
        .expect_err("an unbounded range must be refused");
        let message = error.to_string();
        assert!(
            message.contains("--from/--to"),
            "the refusal must name the right route: {message}"
        );
        assert!(!path.exists(), "a refused run must not create a database");
    }

    /// "Fetch everything since my last run" is `--days 0 --incremental`, and it is
    /// bounded by the watermark rather than by a window the operator has to invent.
    /// Refusing it on `days == 0` alone contradicted the floor rule that `days` only
    /// decides where a run starts when nothing was recorded.
    #[test]
    fn an_incremental_run_with_no_window_is_bounded_by_its_watermark() {
        let path = scratch_path("days-zero-incremental");
        {
            let store = TenderStore::open(&path).expect("store should open");
            store
                .record_refresh_run("refresh", "ACTIVE", "days=30", 10, 10, None, 5)
                .expect("run should record");
        }
        let today = chrono::Utc::now().date_naive();
        let expected = today - chrono::Duration::days(INCREMENTAL_OVERLAP_DAYS);
        let (endpoint, handle) = recording_ted(vec![("200 OK", NOTICE_BODY)]);
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Active,
                days: 0,
                incremental: true,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            },
            today,
        )
        .expect("a watermark bounds the window, so the run is allowed");
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 1, "one page is enough for ten notices");
        assert!(
            requests[0].contains(&format!("publication-date>={}", expected.format("%Y%m%d"))),
            "it starts at the watermark minus the overlap: {}",
            requests[0]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// One official-journal issue has no range to walk, so the unbounded guard must
    /// not get in its way.
    #[test]
    fn inspecting_the_latest_issue_is_still_allowed() {
        let (endpoint, handle) = recording_ted(vec![("200 OK", NOTICE_BODY)]);
        let path = scratch_path("latest-issue");
        run(
            TedIngestOptions {
                query: "buyer-country=BEL".to_owned(),
                limit: 10,
                scope: SearchScope::Latest,
                days: 0,
                incremental: false,
                overlap_days: INCREMENTAL_OVERLAP_DAYS,
                from: None,
                until: None,
                database: path.display().to_string(),
            },
            &TedClient {
                endpoint,
                timeout: Duration::from_secs(5),
            },
            NaiveDate::from_ymd_opt(2026, 11, 6).unwrap(),
        )
        .expect("inspecting the current issue needs no date bound");
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 1);
        // `publication-date` is also a requested *field*; only a bound carries an
        // operator.
        assert!(
            !requests[0].contains("publication-date>"),
            "LATEST carries no date bound: {}",
            requests[0]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn accepts_help_without_options() {
        for flag in ["--help", "-h"] {
            assert!(parse_args(arguments(&[flag]))
                .expect("help should parse")
                .is_none());
        }
    }

    #[test]
    fn rejects_missing_values() {
        assert!(parse_args(arguments(&["--query"])).is_err());
        assert!(parse_args(arguments(&["--limit"])).is_err());
        assert!(parse_args(arguments(&["--database"])).is_err());
    }

    #[test]
    fn rejects_invalid_or_out_of_range_limits() {
        assert!(parse_args(arguments(&["--limit", "abc"])).is_err());
        assert!(parse_args(arguments(&["--limit", "0"])).is_err());
        assert!(parse_args(arguments(&[
            "--limit",
            &(MAX_TED_SEARCH_RESULTS + 1).to_string(),
        ]))
        .is_err());
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(arguments(&["--unknown"])).is_err());
        assert!(parse_args(arguments(&["positional"])).is_err());
    }
}
