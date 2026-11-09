use crate::kbo::{normalize_enterprise_number, KboRepository};
use crate::store::TenderStore;
use crate::{match_company_to_tender, AwardRecord, CompanyPreferences, CompanyProfile, Tender};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::path::Path;

/// Evaluation identity for a tender. Feedback and tenders are keyed by
/// `(source, external_id)` in the store; using only the external id lets
/// feedback for one source label a same-id tender from another source. The
/// length prefix prevents delimiter collisions when identifiers contain `/`.
pub fn tender_key(source: &str, external_id: &str) -> String {
    format!("{}:{source}{external_id}", source.len())
}

#[derive(Debug, Clone)]
pub struct EvalCase {
    pub name: String,
    /// Relevance labels that matched neither a candidate nor a superseded
    /// version of one, so they could not be evaluated.
    pub unmatched_labels: usize,
    /// How many placed labels carry a bid outcome (bid/no-bid/won/lost).
    pub outcome_labels: usize,
    pub company: CompanyProfile,
    pub candidates: Vec<Tender>,
    pub relevance: HashMap<String, f64>,
    pub history: Vec<AwardRecord>,
    pub preferences: Option<CompanyPreferences>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseMetric {
    pub name: String,
    pub precision_at_k: f64,
    /// Precision over pursuable-grade labels only (grade >= 2.0): of the judged
    /// top-k, how many are realistically pursuable, not merely topic-relevant.
    pub pursuable_precision_at_k: f64,
    pub ndcg_at_k: f64,
    /// Placed labels behind this case.
    pub judged: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalResult {
    pub cases: Vec<CaseMetric>,
    pub mean_precision_at_k: f64,
    pub mean_ndcg_at_k: f64,
}

/// Judged-only precision at k: unjudged candidates are excluded from the
/// denominator, matching the Python evaluation reference. This differs from
/// conventional P@k when fewer than k items have relevance judgments.
pub fn precision_at_k(ranked_ids: &[String], relevance: &HashMap<String, f64>, k: usize) -> f64 {
    let top: Vec<&String> = ranked_ids
        .iter()
        .take(k)
        .filter(|id| relevance.contains_key(*id))
        .collect();
    if top.is_empty() {
        return 0.0;
    }
    let hits = top
        .iter()
        .filter(|id| relevance.get(id.as_str()).copied().unwrap_or(0.0) >= 1.0)
        .count();
    hits as f64 / top.len() as f64
}

fn dcg(ranked_ids: &[&String], relevance: &HashMap<String, f64>, k: usize) -> f64 {
    let mut total = 0.0;
    for (idx, id) in ranked_ids.iter().take(k).enumerate() {
        let position = idx + 1;
        let gain = 2.0_f64.powf(relevance.get(id.as_str()).copied().unwrap_or(0.0)) - 1.0;
        total += gain / ((position + 1) as f64).log2();
    }
    total
}

pub fn ndcg_at_k(ranked_ids: &[String], relevance: &HashMap<String, f64>, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let judged_top: Vec<&String> = ranked_ids
        .iter()
        .take(k)
        .filter(|id| relevance.contains_key(*id))
        .collect();
    let mut ideal: Vec<&String> = relevance.keys().collect();
    ideal.sort_by(|a, b| {
        let rel_a = relevance.get(*a).copied().unwrap_or(0.0);
        let rel_b = relevance.get(*b).copied().unwrap_or(0.0);
        rel_b
            .partial_cmp(&rel_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let ideal_k: Vec<&String> = ideal.into_iter().take(judged_top.len()).collect();
    let best = dcg(&ideal_k, relevance, judged_top.len());
    if best == 0.0 {
        return 1.0;
    }
    let actual = dcg(&judged_top, relevance, judged_top.len());
    actual / best
}

/// Precision over pursuable labels only: unjudged candidates are excluded like
/// above, and topic-only grades (1.0) count as misses — the mission asks for
/// relevant *and* pursuable.
pub fn pursuable_precision_at_k(
    ranked_ids: &[String],
    relevance: &HashMap<String, f64>,
    k: usize,
) -> f64 {
    let top: Vec<&String> = ranked_ids
        .iter()
        .take(k)
        .filter(|id| relevance.contains_key(*id))
        .collect();
    if top.is_empty() {
        return 0.0;
    }
    let hits = top
        .iter()
        .filter(|id| relevance.get(id.as_str()).copied().unwrap_or(0.0) >= 2.0)
        .count();
    hits as f64 / top.len() as f64
}

pub fn rank_baseline(
    company: &CompanyProfile,
    candidates: &[Tender],
    history: &[AwardRecord],
    preferences: Option<&CompanyPreferences>,
) -> Vec<String> {
    let default_prefs = CompanyPreferences::default();
    let prefs = preferences.unwrap_or(&default_prefs);
    let candidates = crate::collapse_dossier_members(candidates.to_vec());
    let mut scored: Vec<(f64, String)> = candidates
        .iter()
        .map(|tender| {
            let result = match_company_to_tender(company, tender, history, prefs);
            (
                result.score,
                tender_key(&tender.source, &tender.external_id),
            )
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    scored.into_iter().map(|(_, id)| id).collect()
}

pub fn evaluate<F>(cases: &[EvalCase], ranker: F, k: usize) -> Result<EvalResult, Box<dyn Error>>
where
    F: Fn(&EvalCase) -> Vec<String>,
{
    if k == 0 {
        return Err("k must be at least 1".into());
    }
    if cases.is_empty() {
        return Err("evaluate needs at least one case".into());
    }
    let mut scored = Vec::new();
    for case in cases {
        let ranked = ranker(case);
        scored.push(CaseMetric {
            name: case.name.clone(),
            precision_at_k: precision_at_k(&ranked, &case.relevance, k),
            pursuable_precision_at_k: pursuable_precision_at_k(&ranked, &case.relevance, k),
            ndcg_at_k: ndcg_at_k(&ranked, &case.relevance, k),
            judged: case.relevance.len(),
        });
    }
    let mean_precision_at_k =
        scored.iter().map(|item| item.precision_at_k).sum::<f64>() / scored.len() as f64;
    let mean_ndcg_at_k =
        scored.iter().map(|item| item.ndcg_at_k).sum::<f64>() / scored.len() as f64;
    Ok(EvalResult {
        cases: scored,
        mean_precision_at_k,
        mean_ndcg_at_k,
    })
}

impl EvalCase {
    /// Grade mix of placed labels: (pursuable 2.0, topic-only 1.0, not 0.0).
    pub fn grade_counts(&self) -> (usize, usize, usize) {
        let mut pursuable = 0;
        let mut topic = 0;
        let mut not = 0;
        for grade in self.relevance.values() {
            if *grade >= 2.0 {
                pursuable += 1;
            } else if *grade >= 1.0 {
                topic += 1;
            } else {
                not += 1;
            }
        }
        (pursuable, topic, not)
    }

    /// Share of ranking candidates carrying a label — the denominator honesty
    /// check. A low number means the metric is arithmetic, not evidence.
    pub fn judged_coverage(&self) -> f64 {
        if self.candidates.is_empty() {
            return 0.0;
        }
        self.relevance.len() as f64 / self.candidates.len() as f64
    }
}

/// Use the same identity as candidate version selection. In particular, a
/// source-issued scope must prevent label transfer to a same-title neighbour.
fn transfer_identity(tender: &Tender) -> Option<crate::ProcurementKey> {
    tender.version_key()
}

pub fn feedback_case(
    enterprise_number: &str,
    kbo_database: impl AsRef<Path>,
    tender_database: impl AsRef<Path>,
    candidates: usize,
) -> Result<EvalCase, Box<dyn Error>> {
    let kbo = KboRepository::open(kbo_database.as_ref())?;
    let company = kbo
        .lookup(enterprise_number)?
        .ok_or_else(|| format!("Company not found: {enterprise_number}"))?;
    let normalized = normalize_enterprise_number(enterprise_number)?;
    let store = TenderStore::open_read_only(tender_database.as_ref())?;
    let feedback = store.feedback_details_for(&normalized)?;
    let history = store.awards_for(&normalized)?;
    let preferences = store.preferences_for(&normalized)?;
    let today = chrono::Local::now().date_naive();
    // Ladder slots, not rows: dossier members collapse here so coverage
    // divides by what the ranking actually shows.
    let open_tenders =
        crate::collapse_dossier_members(store.open_tenders(candidates, today, None)?);

    let open_ids: std::collections::HashSet<String> = open_tenders
        .iter()
        .map(|t| tender_key(&t.source, &t.external_id))
        .collect();
    // A label belongs to the procurement, not to one publication of it. TED
    // republishes a call when its deadline moves, and the ladder keeps only the
    // operative version, so a label recorded against the earlier notice would
    // vanish from the evaluation — the one measurement the product has — without
    // the user ever being told.
    let candidates_by_identity: HashMap<_, _> = open_tenders
        .iter()
        .filter_map(|tender| {
            Some((
                transfer_identity(tender)?,
                tender_key(&tender.source, &tender.external_id),
            ))
        })
        .collect();
    // Dossier members collapsed out of the ladder carry their labels to the
    // surviving candidate via the shared group key.
    let candidates_by_group: HashMap<_, _> = open_tenders
        .iter()
        .map(|tender| {
            (
                crate::dossier_group_key(tender),
                tender_key(&tender.source, &tender.external_id),
            )
        })
        .collect();

    let mut relevance = HashMap::new();
    let mut unmatched_labels = 0_usize;
    let mut outcome_labels = 0_usize;
    for ((source, external_id), label) in feedback {
        // Graded: pursuable-true (2.0) outranks topic-only relevant (1.0);
        // pursuable-false reads as not relevant (0.0) even when topical.
        let score = match (label.relevant, label.pursuable) {
            (_, Some(true)) => 2.0,
            (_, Some(false)) => 0.0,
            (true, None) => 1.0,
            (false, None) => 0.0,
        };
        let has_outcome = label.outcome.is_some();
        let key = tender_key(&source, &external_id);
        if open_ids.contains(&key) {
            relevance.insert(key, score);
            if has_outcome {
                outcome_labels += 1;
            }
            continue;
        }
        let carried = store
            .tender(&source, &external_id)
            .ok()
            .flatten()
            .and_then(|superseded| {
                transfer_identity(&superseded)
                    .and_then(|identity| candidates_by_identity.get(&identity).cloned())
                    .or_else(|| {
                        candidates_by_group
                            .get(&crate::dossier_group_key(&superseded))
                            .cloned()
                    })
            });
        match carried {
            Some(key) => {
                if let std::collections::hash_map::Entry::Vacant(slot) = relevance.entry(key) {
                    slot.insert(score);
                    if has_outcome {
                        outcome_labels += 1;
                    }
                }
            }
            None => unmatched_labels += 1,
        }
    }

    if relevance.is_empty() {
        return Err(format!(
            "No recorded feedback for {normalized} covers a current candidate; mark tenders relevant first"
        )
        .into());
    }

    Ok(EvalCase {
        name: format!("feedback:{normalized}"),
        unmatched_labels,
        outcome_labels,
        company,
        candidates: open_tenders,
        relevance,
        history,
        preferences: Some(preferences),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::Connection;
    use rust_decimal::Decimal;
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

    fn test_company() -> CompanyProfile {
        CompanyProfile {
            enterprise_number: "0404.484.060".to_string(),
            status: Some("AC".to_string()),
            juridical_situation: None,
            juridical_form: None,
            start_date: Some("2020-01-01".to_string()),
            names: vec!["Example Installations".to_string()],
            activities: vec![],
            addresses: vec![],
            establishments: vec![],
            ..Default::default()
        }
    }

    fn test_tender(external_id: &str, title: &str) -> Tender {
        Tender {
            source: "test".to_string(),
            external_id: external_id.to_string(),
            title: title.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn expired_folder_judgment_is_unmatched_not_transferred_to_same_title_folder() {
        let temp = TempDir::new("eval-source-scope");
        let deadline = future_deadline();
        let mut expired = sample_open_tender("folder-A", Some("2000-01-01"));
        expired.source = "bda".into();
        expired.raw = serde_json::json!({"contract_folder_id":"folder-A"});
        let open = Tender {
            external_id: "folder-B".into(),
            deadline: Some(deadline.clone()),
            raw: serde_json::json!({"contract_folder_id":"folder-B"}),
            ..expired.clone()
        };
        let control = sample_open_tender("control", Some(&deadline));
        let (kbo_db, tender_db) = setup_databases(temp.path(), vec![expired, open, control]);
        let store = TenderStore::open(&tender_db).unwrap();
        store
            .record_feedback(
                "0404.484.060",
                "bda",
                "folder-A",
                true,
                Some(true),
                Some("won"),
                None,
            )
            .unwrap();
        store
            .record_feedback("0404.484.060", "test", "control", true, None, None, None)
            .unwrap();
        drop(store);
        let case = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap();
        assert_eq!(case.candidates.len(), 2);
        assert_eq!(case.relevance.len(), 1);
        assert!(case.relevance.contains_key(&tender_key("test", "control")));
        assert!(!case.relevance.contains_key(&tender_key("bda", "folder-B")));
        assert_eq!(case.unmatched_labels, 1);
        assert_eq!(case.outcome_labels, 0);
    }

    #[test]
    fn feedback_transfer_respects_source_folder_identity() {
        let row = |folder: &str| Tender {
            source: "bda".into(),
            external_id: folder.into(),
            title: "Same title".into(),
            buyer: Some("Buyer".into()),
            raw: serde_json::json!({"contract_folder_id": folder}),
            ..Tender::default()
        };
        assert_ne!(
            transfer_identity(&row("folder-A")),
            transfer_identity(&row("folder-B")),
            "a judgment cannot move to an unrelated source folder with the same title"
        );
    }

    #[test]
    fn evaluation_identity_has_no_delimiter_collisions() {
        assert_ne!(tender_key("ted/a", "b"), tender_key("ted", "a/b"));
        assert_ne!(tender_key("", "a"), tender_key("a", ""));
    }

    #[test]
    fn test_precision_at_k_counts_grades_above_threshold() {
        let ranked = vec!["t1".to_string(), "t2".to_string(), "t3".to_string()];
        let mut relevance = HashMap::new();
        relevance.insert("t1".to_string(), 2.0);
        relevance.insert("t2".to_string(), 0.0);
        relevance.insert("t3".to_string(), 1.0);

        assert_eq!(precision_at_k(&ranked, &relevance, 2), 0.5);
        assert!((precision_at_k(&ranked, &relevance, 3) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(precision_at_k(&[], &relevance, 3), 0.0);
        assert_eq!(precision_at_k(&ranked, &relevance, 0), 0.0);
    }

    #[test]
    fn test_ndcg_rewards_ideal_ordering() {
        let mut relevance = HashMap::new();
        relevance.insert("t1".to_string(), 2.0);
        relevance.insert("t2".to_string(), 0.0);
        relevance.insert("t3".to_string(), 1.0);

        let ideal = vec!["t1".to_string(), "t3".to_string(), "t2".to_string()];
        assert_eq!(ndcg_at_k(&ideal, &relevance, 3), 1.0);

        let reversed = vec!["t2".to_string(), "t3".to_string(), "t1".to_string()];
        let reversed_score = ndcg_at_k(&reversed, &relevance, 3);
        assert!(reversed_score > 0.5);
        assert!(reversed_score < 0.6);
    }

    #[test]
    fn test_ndcg_without_relevance_is_trivially_perfect() {
        let empty = HashMap::new();
        assert_eq!(
            ndcg_at_k(&["t1".to_string(), "t2".to_string()], &empty, 2),
            1.0
        );
    }

    #[test]
    fn test_precision_ignores_unjudged_candidates() {
        let ranked = vec![
            "u1".to_string(),
            "t2".to_string(),
            "u2".to_string(),
            "t1".to_string(),
        ];
        let mut relevance = HashMap::new();
        relevance.insert("t1".to_string(), 2.0);
        relevance.insert("t2".to_string(), 0.0);

        assert_eq!(precision_at_k(&ranked, &relevance, 4), 0.5);
        assert_eq!(precision_at_k(&ranked, &relevance, 2), 0.0);
        assert_eq!(
            precision_at_k(&["u1".to_string(), "u2".to_string()], &relevance, 2),
            0.0
        );
    }

    #[test]
    fn test_ndcg_ignores_unjudged_positions() {
        let mut relevance = HashMap::new();
        relevance.insert("t1".to_string(), 2.0);
        relevance.insert("t2".to_string(), 0.0);

        let ranked1 = vec!["u1".to_string(), "u2".to_string(), "t1".to_string()];
        assert_eq!(ndcg_at_k(&ranked1, &relevance, 3), 1.0);

        let ranked2 = vec!["t2".to_string(), "u1".to_string(), "t1".to_string()];
        let mixed = ndcg_at_k(&ranked2, &relevance, 3);
        assert!(mixed > 0.5);
        assert!(mixed < 0.7);
    }

    #[test]
    fn zero_cutoff_is_not_a_perfect_evaluation() {
        assert_eq!(ndcg_at_k(&[], &HashMap::new(), 0), 0.0);
        assert!(evaluate(&[], |_| vec![], 0)
            .unwrap_err()
            .to_string()
            .contains("k must be at least 1"));
    }

    #[test]
    fn test_evaluate_rejects_empty_cases() {
        let err = evaluate(&[], |_| vec![], 3).unwrap_err();
        assert!(err.to_string().contains("at least one case"));
    }

    #[test]
    fn test_evaluate_separates_perfect_from_adversarial_rankers() {
        let mut relevance = HashMap::new();
        relevance.insert("t1".to_string(), 2.0);
        relevance.insert("t2".to_string(), 0.0);
        relevance.insert("t3".to_string(), 1.0);

        let case = EvalCase {
            name: "synthetic".to_string(),
            unmatched_labels: 0,
            outcome_labels: 0,
            company: test_company(),
            candidates: vec![
                test_tender("t1", "First"),
                test_tender("t2", "Second"),
                test_tender("t3", "Third"),
            ],
            relevance,
            history: vec![],
            preferences: None,
        };

        let perfect = evaluate(
            &[case.clone()],
            |_| vec!["t1".to_string(), "t3".to_string(), "t2".to_string()],
            3,
        )
        .unwrap();
        assert_eq!(perfect.cases[0].ndcg_at_k, 1.0);
        assert_eq!(perfect.mean_ndcg_at_k, 1.0);

        let adversarial = evaluate(
            &[case],
            |_| vec!["t2".to_string(), "t3".to_string(), "t1".to_string()],
            3,
        )
        .unwrap();
        assert!(adversarial.cases[0].ndcg_at_k < perfect.cases[0].ndcg_at_k);
        assert!(adversarial.cases[0].precision_at_k >= 0.0);
    }

    #[test]
    fn pursuable_precision_counts_only_pursuable_grades() {
        let mut relevance = HashMap::new();
        relevance.insert("t1".to_string(), 2.0);
        relevance.insert("t2".to_string(), 1.0);
        relevance.insert("t3".to_string(), 0.0);
        let ranked = vec!["t1".to_string(), "t2".to_string(), "t3".to_string()];
        assert_eq!(precision_at_k(&ranked, &relevance, 3), 2.0 / 3.0);
        assert_eq!(pursuable_precision_at_k(&ranked, &relevance, 3), 1.0 / 3.0);
    }

    #[test]
    fn graded_metric_separates_ranking_change_from_tie_order() {
        // Same grades, two rankers: one leads with pursuable (2.0), the other
        // with topic-only (1.0). nDCG must prefer the grade-aware order, while
        // swapping two equal grades must not move any metric — ties are noise.
        let mut relevance = HashMap::new();
        relevance.insert("p".to_string(), 2.0);
        relevance.insert("t".to_string(), 1.0);
        relevance.insert("n".to_string(), 0.0);
        let grade_first = vec!["p".to_string(), "t".to_string(), "n".to_string()];
        let topic_first = vec!["t".to_string(), "p".to_string(), "n".to_string()];
        assert!(ndcg_at_k(&grade_first, &relevance, 3) > ndcg_at_k(&topic_first, &relevance, 3));
        assert!(
            pursuable_precision_at_k(&grade_first, &relevance, 1)
                > pursuable_precision_at_k(&topic_first, &relevance, 1)
        );
        let mut tied = HashMap::new();
        tied.insert("a".to_string(), 1.0);
        tied.insert("b".to_string(), 1.0);
        let ab = vec!["a".to_string(), "b".to_string()];
        let ba = vec!["b".to_string(), "a".to_string()];
        assert_eq!(ndcg_at_k(&ab, &tied, 2), ndcg_at_k(&ba, &tied, 2));
        assert_eq!(
            pursuable_precision_at_k(&ab, &tied, 2),
            pursuable_precision_at_k(&ba, &tied, 2)
        );
    }

    #[test]
    fn case_reports_grade_mix_and_coverage() {
        let mut relevance = HashMap::new();
        relevance.insert("p".to_string(), 2.0);
        relevance.insert("t".to_string(), 1.0);
        relevance.insert("n".to_string(), 0.0);
        let case = EvalCase {
            name: "synthetic".to_string(),
            unmatched_labels: 0,
            outcome_labels: 1,
            company: test_company(),
            candidates: vec![
                test_tender("p", "P"),
                test_tender("t", "T"),
                test_tender("n", "N"),
                test_tender("u", "U"),
            ],
            relevance,
            history: vec![],
            preferences: None,
        };
        assert_eq!(case.grade_counts(), (1, 1, 1));
        assert_eq!(case.judged_coverage(), 0.75);
    }

    #[test]
    fn test_rank_baseline_is_deterministic_and_complete() {
        let company = test_company();
        let candidates = vec![
            test_tender("b", "Beta works"),
            test_tender("a", "Alpha works"),
            test_tender("c", "Gamma works"),
        ];

        let first = rank_baseline(&company, &candidates, &[], None);
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["4:testa", "4:testb", "4:testc"]);
        assert_eq!(first, rank_baseline(&company, &candidates, &[], None));
    }

    fn setup_databases(
        dir: &Path,
        tenders: Vec<Tender>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let kbo_db = dir.join("kbo.duckdb");
        let con = Connection::open(&kbo_db).unwrap();
        con.execute_batch(
            r#"
            CREATE TABLE kbo_enterprise (
                EnterpriseNumber VARCHAR, Status VARCHAR, JuridicalSituation VARCHAR,
                JuridicalForm VARCHAR, StartDate DATE
            );
            CREATE TABLE kbo_denomination (
                EntityNumber VARCHAR, Denomination VARCHAR
            );
            CREATE TABLE kbo_activity (
                EntityNumber VARCHAR, NaceCode VARCHAR, Classification VARCHAR
            );
            CREATE TABLE kbo_code (
                Category VARCHAR, Code VARCHAR, Language VARCHAR, Description VARCHAR
            );
            CREATE TABLE kbo_address (
                EntityNumber VARCHAR, TypeOfAddress VARCHAR, Zipcode VARCHAR,
                MunicipalityNL VARCHAR, MunicipalityFR VARCHAR, StreetNL VARCHAR,
                StreetFR VARCHAR, HouseNumber VARCHAR, Box VARCHAR, DateStrikingOff DATE
            );
            CREATE TABLE kbo_establishment (
                EnterpriseNumber VARCHAR, EstablishmentNumber VARCHAR
            );
            INSERT INTO kbo_enterprise VALUES ('0404.484.060', 'AC', NULL, NULL, '2020-01-01');
            INSERT INTO kbo_denomination VALUES ('0404.484.060', 'Example');
            "#,
        )
        .unwrap();

        let tender_db = dir.join("tenders.duckdb");
        let store = TenderStore::open(&tender_db).unwrap();
        for tender in tenders {
            store.upsert(&tender).unwrap();
        }

        (kbo_db, tender_db)
    }

    fn future_deadline() -> String {
        (chrono::Local::now().date_naive() + chrono::Duration::days(30))
            .format("%Y-%m-%d")
            .to_string()
    }

    fn sample_open_tender(external_id: &str, deadline: Option<&str>) -> Tender {
        // The title carries the id so each fixture tender is a distinct
        // procurement: three notices with the same buyer, title and CPV are
        // versions of one call, and the ladder now collapses those.
        Tender {
            source: "test".to_string(),
            external_id: external_id.to_string(),
            title: format!("Elektrische installatie werken {external_id}"),
            buyer: Some("Stad Gent".to_string()),
            description: "Elektrische installatie werken".to_string(),
            cpv_codes: vec!["45310000".to_string()],
            regions: vec!["Gent".to_string()],
            estimated_value: Some(Decimal::new(50000, 0)),
            deadline: deadline.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    /// A label belongs to the procurement, not to the publication that was on
    /// screen when it was recorded: a corrigendum republishes the call and the
    /// ladder keeps only the operative version, so the label must follow it.
    #[test]
    fn relevance_survives_a_republished_notice() {
        let future_deadline = future_deadline();
        let temp_dir = TempDir::new("eval-transfer");
        let original = sample_open_tender("original", Some(future_deadline.as_str()));
        // Same buyer, title and CPV, with the deadline moved: a corrigendum.
        let mut extended = sample_open_tender("extended", Some("2030-01-15"));
        extended.title = original.title.clone();
        let (kbo_db, tender_db) = setup_databases(temp_dir.path(), vec![original, extended]);

        let store = TenderStore::open(&tender_db).unwrap();
        store
            .record_feedback("0404.484.060", "test", "original", true, None, None, None)
            .unwrap();

        let case = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap();
        // The ladder keeps the extended publication, and the label followed it.
        assert_eq!(case.candidates.len(), 1, "one procurement, one candidate");
        assert_eq!(case.candidates[0].external_id, "extended");
        assert_eq!(
            case.relevance.get(&tender_key("test", "extended")),
            Some(&1.0),
            "topic-only label grades 1.0 and moved to the surviving version"
        );
        assert_eq!(case.unmatched_labels, 0);
    }

    /// A label recorded against a dossier file that later collapses out of
    /// the ladder still grades the surviving candidate.
    #[test]
    fn relevance_survives_dossier_collapse() {
        let future_deadline = future_deadline();
        let temp_dir = TempDir::new("eval-collapse");
        let mut fragment = sample_open_tender("fragment", Some(future_deadline.as_str()));
        fragment.title = "Bits Bodies annex".to_string();
        fragment.description = "short".to_string();
        fragment.raw = serde_json::json!({"dossier_container": "dossier-1"});
        let mut survivor = sample_open_tender("survivor", Some(future_deadline.as_str()));
        // Distinct titles so version collapse keeps both; only the dossier
        // key groups them.
        survivor.title = "Bits Bodies bestek".to_string();
        survivor.description = "a much longer bestek description with real substance".to_string();
        survivor.raw = serde_json::json!({"dossier_container": "dossier-1"});
        let (kbo_db, tender_db) = setup_databases(temp_dir.path(), vec![fragment, survivor]);

        let store = TenderStore::open(&tender_db).unwrap();
        store
            .record_feedback("0404.484.060", "test", "fragment", true, None, None, None)
            .unwrap();

        let case = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap();
        assert_eq!(
            case.relevance.get(&tender_key("test", "survivor")),
            Some(&1.0),
            "fragment label grades the surviving candidate"
        );
        assert_eq!(case.unmatched_labels, 0);
    }

    #[test]
    fn test_builds_graded_case_from_recorded_feedback() {
        let future_deadline = future_deadline();
        let temp_dir = TempDir::new("eval-test");
        let (kbo_db, tender_db) = setup_databases(
            temp_dir.path(),
            vec![
                sample_open_tender("open-yes", Some(future_deadline.as_str())),
                sample_open_tender("open-no", Some(future_deadline.as_str())),
                sample_open_tender("expired", Some("2020-01-01")),
            ],
        );

        let store = TenderStore::open(&tender_db).unwrap();
        store
            .record_feedback("0404.484.060", "test", "open-yes", true, None, None, None)
            .unwrap();
        store
            .record_feedback("0404.484.060", "test", "open-no", false, None, None, None)
            .unwrap();
        store
            .record_feedback("0404.484.060", "test", "expired", true, None, None, None)
            .unwrap();

        let case = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap();
        assert_eq!(case.name, "feedback:0404.484.060");
        let candidate_ids: std::collections::HashSet<String> = case
            .candidates
            .iter()
            .map(|t| t.external_id.clone())
            .collect();
        assert_eq!(
            candidate_ids,
            ["open-yes".to_string(), "open-no".to_string()]
                .into_iter()
                .collect()
        );
        assert_eq!(case.relevance.get("4:testopen-yes"), Some(&1.0));
        assert_eq!(case.relevance.get("4:testopen-no"), Some(&0.0));
        assert_eq!(case.relevance.get("4:testexpired"), None);
        assert_eq!(case.outcome_labels, 0);
    }

    #[test]
    fn pursuable_outranks_topic_only_and_outcomes_are_counted() {
        let future_deadline = future_deadline();
        let temp_dir = TempDir::new("eval-test");
        let (kbo_db, tender_db) = setup_databases(
            temp_dir.path(),
            vec![
                sample_open_tender("pursue", Some(future_deadline.as_str())),
                sample_open_tender("topic", Some(future_deadline.as_str())),
            ],
        );

        let store = TenderStore::open(&tender_db).unwrap();
        store
            .record_feedback(
                "0404.484.060",
                "test",
                "pursue",
                true,
                Some(true),
                Some("bid"),
                None,
            )
            .unwrap();
        store
            .record_feedback("0404.484.060", "test", "topic", true, None, None, None)
            .unwrap();

        let case = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap();
        assert_eq!(case.relevance.get("4:testpursue"), Some(&2.0));
        assert_eq!(case.relevance.get("4:testtopic"), Some(&1.0));
        assert_eq!(case.outcome_labels, 1);
        // Pursuable-false reads as not relevant even when topical.
        store
            .record_feedback(
                "0404.484.060",
                "test",
                "topic",
                true,
                Some(false),
                None,
                None,
            )
            .unwrap();
        let case = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap();
        assert_eq!(case.relevance.get("4:testtopic"), Some(&0.0));
    }

    #[test]
    fn test_feedback_does_not_leak_across_sources_with_same_id() {
        let future_deadline = future_deadline();
        let temp_dir = TempDir::new("eval-test");
        let mut ted_tender = sample_open_tender("shared-id", Some(future_deadline.as_str()));
        ted_tender.source = "ted".to_string();
        let mut bda_tender = sample_open_tender("shared-id", Some(future_deadline.as_str()));
        bda_tender.source = "bda".to_string();
        let (kbo_db, tender_db) = setup_databases(temp_dir.path(), vec![ted_tender, bda_tender]);

        let store = TenderStore::open(&tender_db).unwrap();
        store
            .record_feedback("0404.484.060", "ted", "shared-id", true, None, None, None)
            .unwrap();

        let case = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap();
        assert_eq!(case.relevance.get("3:tedshared-id"), Some(&1.0));
        assert_eq!(case.relevance.get("3:bdashared-id"), None);
        let ranked = rank_baseline(&case.company, &case.candidates, &[], None);
        assert!(ranked.contains(&"3:tedshared-id".to_string()));
        assert!(ranked.contains(&"3:bdashared-id".to_string()));
    }

    #[test]
    fn test_filters_expired_tenders_before_candidate_limit() {
        let future_deadline = future_deadline();
        let temp_dir = TempDir::new("eval-test");
        let (kbo_db, tender_db) = setup_databases(
            temp_dir.path(),
            vec![
                sample_open_tender("expired-first", Some("2020-01-01")),
                sample_open_tender("open-later", Some(future_deadline.as_str())),
            ],
        );
        let store = TenderStore::open(&tender_db).unwrap();
        store
            .record_feedback("0404.484.060", "test", "open-later", true, None, None, None)
            .unwrap();

        let case = feedback_case("0404.484.060", &kbo_db, &tender_db, 1).unwrap();
        assert_eq!(case.candidates.len(), 1);
        assert_eq!(case.candidates[0].external_id, "open-later");
        assert_eq!(case.relevance.get("4:testopen-later"), Some(&1.0));
    }

    #[test]
    fn test_raises_without_usable_feedback() {
        let future_deadline = future_deadline();
        let temp_dir = TempDir::new("eval-test");
        let (kbo_db, tender_db) = setup_databases(
            temp_dir.path(),
            vec![sample_open_tender("open", Some(future_deadline.as_str()))],
        );

        let err = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap_err();
        assert!(err.to_string().contains("No recorded feedback"));

        let store = TenderStore::open(&tender_db).unwrap();
        store
            .record_feedback(
                "0404.484.060",
                "test",
                "missing-tender",
                true,
                None,
                None,
                None,
            )
            .unwrap();

        let err2 = feedback_case("0404.484.060", &kbo_db, &tender_db, 100).unwrap_err();
        assert!(err2.to_string().contains("current candidate"));
    }

    #[test]
    fn test_missing_tender_database_is_not_created() {
        let temp_dir = TempDir::new("eval-test");
        let (kbo_db, tender_db) = setup_databases(temp_dir.path(), vec![]);
        std::fs::remove_file(&tender_db).unwrap();

        assert!(feedback_case("0404.484.060", &kbo_db, &tender_db, 10).is_err());
        assert!(!tender_db.exists());
    }

    #[test]
    fn test_raises_for_unknown_company() {
        let temp_dir = TempDir::new("eval-test");
        let (kbo_db, tender_db) = setup_databases(temp_dir.path(), vec![]);

        let err = feedback_case("0200.065.765", &kbo_db, &tender_db, 100).unwrap_err();
        assert!(err.to_string().contains("Company not found"));
    }
}
