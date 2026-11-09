//! Interactive judging queue for a human labeling hour.
//!
//! Walks the QC-framework sample for one company — every unjudged top-20
//! row plus a seeded random 10% of unjudged ranks 21–100 — printing a
//! compact brief per candidate and recording single-keystroke grades:
//! `p` pursuable (2.0), `r` topic-relevant (1.0), `n` not (0.0),
//! `s` skip, `q` quit. Anything after the letter becomes the reason.
//! Already-judged rows never reappear; re-running resumes the queue.

use aanbestedingen_matcher::cpv::cpv_label_nl;
use aanbestedingen_matcher::kbo::{normalize_enterprise_number, KboRepository};
use aanbestedingen_matcher::recommendations::{rank, Recommendation};
use aanbestedingen_matcher::store::{operator_label, FeedbackUpdate, TenderStore};
use chrono::Utc;
use std::collections::HashSet;
use std::env;
use std::io::{self, BufRead, Write};

const MAX_CANDIDATES: usize = 100_000;
const TOP_JUDGE_RANKS: usize = 20;
const TAIL_JUDGE_RANKS: usize = 100;
const TAIL_JUDGE_PERCENT: usize = 10;

#[derive(Debug, PartialEq)]
struct JudgeOptions {
    enterprise_number: String,
    database: String,
    kbo_database: String,
    candidates: usize,
    seed: u64,
    limit: usize,
}

fn parse_args<I>(args: I) -> Result<Option<JudgeOptions>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = args.into_iter();
    let mut raw_enterprise_number = None;
    let mut database = "data/tenders-rust.duckdb".to_owned();
    let mut kbo_database = "data/kbo/kbo_enrich.duckdb".to_owned();
    let mut candidates = 100_usize;
    let mut seed = 2026_1023_u64;
    let mut limit = usize::MAX;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--database" => database = arguments.next().ok_or("--database needs a value")?,
            "--kbo-database" => {
                kbo_database = arguments.next().ok_or("--kbo-database needs a value")?
            }
            "--candidates" => {
                let value: usize = arguments
                    .next()
                    .ok_or("--candidates needs a value")?
                    .parse()?;
                if value == 0 || value > MAX_CANDIDATES {
                    return Err(
                        format!("--candidates must be between 1 and {MAX_CANDIDATES}").into(),
                    );
                }
                candidates = value;
            }
            "--seed" => {
                seed = arguments.next().ok_or("--seed needs a value")?.parse()?;
            }
            "--limit" => {
                limit = arguments.next().ok_or("--limit needs a value")?.parse()?;
                if limit == 0 {
                    return Err("--limit must be at least 1".into());
                }
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
    let enterprise_number = normalize_enterprise_number(
        &raw_enterprise_number.ok_or("enterprise number is required")?,
    )?;
    Ok(Some(JudgeOptions {
        enterprise_number,
        database,
        kbo_database,
        candidates,
        seed,
        limit,
    }))
}

/// Deterministic xorshift64* — the sample must be reproducible run to run.
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
}

/// Seeded shuffle: identical queue for identical inputs.
fn shuffled<R>(rng: &mut Rng, mut items: Vec<R>) -> Vec<R> {
    for index in (1..items.len()).rev() {
        let other = (rng.next() % (index as u64 + 1)) as usize;
        items.swap(index, other);
    }
    items
}

/// Queue positions (0-based into the ranking): every unjudged top-20 slot,
/// then a seeded 10% of unjudged slots 20–99.
fn queue_positions(
    judged: &HashSet<(String, String)>,
    ranked: &[Recommendation],
    seed: u64,
) -> Vec<usize> {
    let mut queue = Vec::new();
    for (position, item) in ranked.iter().enumerate().take(TOP_JUDGE_RANKS) {
        if !judged.contains(&(item.source.clone(), item.external_id.clone())) {
            queue.push(position);
        }
    }
    let tail: Vec<usize> = ranked
        .iter()
        .enumerate()
        .skip(TOP_JUDGE_RANKS)
        .take(TAIL_JUDGE_RANKS - TOP_JUDGE_RANKS)
        .filter(|(_, item)| !judged.contains(&(item.source.clone(), item.external_id.clone())))
        .map(|(position, _)| position)
        .collect();
    let keep = tail.len() * TAIL_JUDGE_PERCENT / 100;
    let mut sampled = shuffled(&mut Rng(seed), tail);
    sampled.truncate(keep);
    sampled.sort_unstable();
    queue.extend(sampled);
    queue
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fused member filenames from the stored row, short enough to read.
fn member_files(store: &TenderStore, item: &Recommendation) -> Vec<String> {
    let Ok(Some(tender)) = store.tender(&item.source, &item.external_id) else {
        return Vec::new();
    };
    match tender
        .raw
        .get("dossier_members")
        .and_then(|value| value.as_array())
    {
        Some(members) => members
            .iter()
            .filter_map(|member| member.as_str())
            .map(|member| {
                let decoded = member.replace("%2F", "/").replace('%', "/");
                let short = decoded
                    .rsplit('/')
                    .next()
                    .unwrap_or(member)
                    .trim_start_matches("file:");
                clean(short)
            })
            .take(8)
            .collect(),
        None => Vec::new(),
    }
}

fn brief(position: usize, item: &Recommendation, store: &TenderStore) -> String {
    let cpvs: Vec<String> = item
        .cpv_codes
        .iter()
        .take(3)
        .map(|code| {
            cpv_label_nl(code)
                .map(|label| format!("{code} {label}"))
                .unwrap_or_else(|| code.clone())
        })
        .collect();
    let value = item
        .estimated_value
        .map(|amount| format!(" €{amount}"))
        .unwrap_or_default();
    let deadline = item.deadline.as_deref().unwrap_or("no deadline");
    let reasons: Vec<String> = item
        .reasons
        .iter()
        .take(2)
        .map(|reason| clean(reason))
        .collect();
    let warnings: Vec<String> = item
        .warnings
        .iter()
        .take(2)
        .map(|warning| clean(warning))
        .collect();
    let members = if item.dossier_members > 1 {
        format!(" · {} dossier files", item.dossier_members)
    } else {
        String::new()
    };
    let files = member_files(store, item);
    let files = if files.is_empty() {
        String::new()
    } else {
        format!("\nfiles: {}", files.join(" | "))
    };
    let excerpt = match store.tender(&item.source, &item.external_id) {
        Ok(Some(tender)) => {
            let text = clean(&tender.description);
            if text.is_empty() {
                String::new()
            } else {
                let end = text
                    .char_indices()
                    .map(|(index, _)| index)
                    .take(600)
                    .last()
                    .unwrap_or(0);
                format!("\n\"{}…\"", &text[..end])
            }
        }
        _ => String::new(),
    };
    format!(
        "── #{position} score {:.1}{members} ──\n{} — {}{} · {deadline}\nCPV: {}{files}{excerpt}\n+ {}\n! {}",
        item.score,
        clean(&item.title),
        clean(item.buyer.as_deref().unwrap_or("unknown buyer")),
        value,
        if cpvs.is_empty() {
            "none".to_owned()
        } else {
            cpvs.join("; ")
        },
        if reasons.is_empty() {
            "—".to_owned()
        } else {
            reasons.join(" / ")
        },
        if warnings.is_empty() {
            "—".to_owned()
        } else {
            warnings.join(" / ")
        },
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(options) = parse_args(env::args().skip(1))? else {
        println!(
            "Usage: judge ENTERPRISE_NUMBER [--database PATH] [--kbo-database PATH] [--candidates N] [--seed N] [--limit N]"
        );
        return Ok(());
    };
    let repository = KboRepository::open(&options.kbo_database)?;
    let company = repository
        .lookup(&options.enterprise_number)?
        .ok_or("company not found")?;
    let store = TenderStore::open(&options.database)?;
    let judged: HashSet<(String, String)> = store
        .feedback_details_for(&company.enterprise_number)?
        .into_keys()
        .collect();
    let today = Utc::now().date_naive();
    let tenders = store.open_tenders(options.candidates, today, None)?;
    if tenders.is_empty() {
        return Err("store holds no open tenders; pass a seeded store".into());
    }
    let preferences = store.preferences_for(&company.enterprise_number)?;
    let history = store.awards_for(&company.enterprise_number)?;
    let feedback = store.feedback_for(&company.enterprise_number)?;
    let ranked = rank(
        &company,
        &tenders,
        &history,
        &preferences,
        &feedback,
        tenders.len(),
    );
    let queue = queue_positions(&judged, &ranked, options.seed);
    let queue: Vec<usize> = queue.into_iter().take(options.limit).collect();
    println!(
        "{} open tenders, {} already judged, {} in this queue (top-20 + seeded 10% of 21–100). Keys: p pursuable · r relevant · n not · s skip · q quit.",
        ranked.len(),
        judged.len(),
        queue.len(),
    );
    let stdin = io::stdin();
    let mut graded = 0_usize;
    for position in queue {
        let item = &ranked[position];
        println!("{}", brief(position + 1, item, &store));
        print!("grade> ");
        io::stdout().flush()?;
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let line = line.trim();
        let (key, reason) = match line.find(' ') {
            Some(index) => (&line[..index], Some(line[index + 1..].trim().to_owned())),
            None => (line, None),
        };
        let (relevant, pursuable) = match key {
            "p" => (true, Some(true)),
            "r" => (true, None),
            "n" => (false, None),
            "s" => continue,
            "q" => break,
            _ => {
                println!("expected p, r, n, s or q — skipped");
                continue;
            }
        };
        let reason = reason.filter(|text| !text.is_empty());
        store.update_feedback(
            &operator_label(),
            &company.enterprise_number,
            &item.source,
            &item.external_id,
            &FeedbackUpdate::replacement(relevant, pursuable, None, reason),
        )?;
        graded += 1;
    }
    println!(
        "graded {graded} this session — run `just eval-ranking {}` for coverage.",
        company.enterprise_number
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_before_positional_enterprise_number() {
        let options = parse_args(
            [
                "--database",
                "test.duckdb",
                "--kbo-database",
                "kbo.duckdb",
                "--candidates",
                "50",
                "--seed",
                "7",
                "--limit",
                "10",
                "0537875391",
            ]
            .into_iter()
            .map(String::from),
        )
        .expect("valid arguments should parse")
        .expect("valid arguments should return options");
        assert_eq!(options.enterprise_number, "0537.875.391");
        assert_eq!(options.candidates, 50);
        assert_eq!(options.seed, 7);
        assert_eq!(options.limit, 10);
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(["0537875391", "--bogus"].into_iter().map(String::from)).is_err());
    }

    #[test]
    fn queue_keeps_rank_order_and_skips_judged() {
        fn item(source: &str, id: &str) -> Recommendation {
            Recommendation {
                source: source.to_owned(),
                external_id: id.to_owned(),
                title: String::new(),
                buyer: None,
                deadline: None,
                estimated_value: None,
                framework_maximum_amounts: Vec::new(),
                regions: Vec::new(),
                cpv_codes: Vec::new(),
                lot_cpv_alignment: None,
                languages: Vec::new(),
                source_url: None,
                requirements: Vec::new(),
                requirement_signals: Vec::new(),
                score: 0.0,
                confidence: 0.0,
                shadow_activity_text_similarity: None,
                shadow_activity_text_similarity_coverage: None,
                shadow_geographic_evidence: None,
                dossier_members: 1,
                feasibility: aanbestedingen_matcher::recommendations::FeasibilityEvidence {
                    status: aanbestedingen_matcher::recommendations::FeasibilityStatus::Unknown,
                    mandatory_requirements: 0,
                    qualification_risk: "",
                },
                reasons: Vec::new(),
                warnings: Vec::new(),
                jev_relevance: None,
                jev_qualification_risk: None,
            }
        }
        let ranked: Vec<Recommendation> = (0..100)
            .map(|index| item("ted", &format!("id-{index}")))
            .collect();
        let judged: HashSet<(String, String)> =
            HashSet::from([("ted".to_owned(), "id-0".to_owned())]);
        let queue = queue_positions(&judged, &ranked, 42);
        // Top-20 minus the judged first row, plus 8 sampled tail rows.
        assert_eq!(queue.len(), 19 + 8);
        assert!(!queue.contains(&0));
        assert!(queue
            .iter()
            .take(19)
            .eq([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19].iter()));
        // Same seed, same queue.
        assert_eq!(queue, queue_positions(&judged, &ranked, 42));
    }
}
