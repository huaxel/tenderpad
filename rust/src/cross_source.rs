//! Detect the same procurement arriving from two sources.
//!
//! Belgian notices reach this product from TED (EU-level) and from the Brussels
//! inventory and e-Notification imports (regional, below-threshold). The same call
//! can legitimately appear in both, and the ladder would then offer it twice.
//!
//! This module only *reports* likely duplicates with the evidence behind each
//! match — buyer, CPV set, title overlap and deadline distance — so a rule can be
//! judged against a real dossier before anything is merged. Merging on a guess
//! would hide a genuine second procurement, which is worse than showing a
//! duplicate.

use crate::Tender;

/// Similarity above which two titles are treated as the same text.
pub const TITLE_SIMILARITY_THRESHOLD: f64 = 0.6;

/// One stored notice that looks like the same procurement as an incoming notice.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CrossSourceMatch {
    pub source: String,
    pub external_id: String,
    pub title: String,
    /// Jaccard overlap of the titles' meaningful words (0–1).
    pub title_similarity: f64,
    pub same_cpvs: bool,
    pub same_deadline: bool,
    pub deadline_delta_days: Option<i64>,
}

fn words(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 4)
        .map(ToOwned::to_owned)
        .collect()
}

fn normalized_buyer(tender: &Tender) -> Option<String> {
    let buyer = tender.buyer.as_deref()?;
    let normalized = buyer
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn jaccard(
    left: &std::collections::HashSet<String>,
    right: &std::collections::HashSet<String>,
) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count() as f64;
    let union = left.union(right).count() as f64;
    intersection / union
}

fn deadline_delta(left: &Tender, right: &Tender) -> Option<i64> {
    let parse = |value: &str| chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").ok();
    let left = left.deadline.as_deref().and_then(parse)?;
    let right = right.deadline.as_deref().and_then(parse)?;
    Some((left - right).num_days())
}

/// Stored notices from a *different* source that look like the same procurement.
///
/// A candidate must share the buyer and the CPV set; the title overlap then decides
/// whether it is reported. `deadline_delta_days` is evidence, not a filter: a
/// corrigendum legitimately moves the deadline, and the caller should see by how
/// much rather than have the match silently dropped.
pub fn find_probable_duplicates(incoming: &Tender, stored: &[Tender]) -> Vec<CrossSourceMatch> {
    let Some(buyer) = normalized_buyer(incoming) else {
        return Vec::new();
    };
    let incoming_cpvs = {
        let mut cpvs = incoming.cpv_codes.clone();
        cpvs.sort();
        cpvs.dedup();
        cpvs
    };
    let incoming_words = words(&incoming.title);
    let mut matches = Vec::new();
    for candidate in stored {
        if candidate.source == incoming.source {
            continue;
        }
        if normalized_buyer(candidate).as_deref() != Some(buyer.as_str()) {
            continue;
        }
        let mut cpvs = candidate.cpv_codes.clone();
        cpvs.sort();
        cpvs.dedup();
        if cpvs != incoming_cpvs {
            continue;
        }
        let similarity = jaccard(&incoming_words, &words(&candidate.title));
        if similarity < TITLE_SIMILARITY_THRESHOLD {
            continue;
        }
        matches.push(CrossSourceMatch {
            source: candidate.source.clone(),
            external_id: candidate.external_id.clone(),
            title: candidate.title.clone(),
            title_similarity: (similarity * 100.0).round() / 100.0,
            same_cpvs: true,
            same_deadline: incoming.deadline.is_some() && incoming.deadline == candidate.deadline,
            deadline_delta_days: deadline_delta(incoming, candidate),
        });
    }
    matches.sort_by(|a, b| {
        b.title_similarity
            .partial_cmp(&a.title_similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.external_id.cmp(&b.external_id))
    });
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tender(
        source: &str,
        id: &str,
        buyer: &str,
        title: &str,
        cpv: &str,
        deadline: &str,
    ) -> Tender {
        Tender {
            source: source.to_owned(),
            external_id: id.to_owned(),
            title: title.to_owned(),
            buyer: Some(buyer.to_owned()),
            cpv_codes: vec![cpv.to_owned()],
            deadline: Some(deadline.to_owned()),
            ..Tender::default()
        }
    }

    #[test]
    fn reports_the_same_procurement_from_another_source() {
        let incoming = tender(
            "bda",
            "bda-1",
            "Stad Gent",
            "Elektrische installatie school",
            "45310000",
            "2027-01-15",
        );
        let stored = vec![
            tender(
                "ted",
                "ted-1",
                "Stad Gent",
                "Elektrische installatie school",
                "45310000",
                "2027-01-15",
            ),
            tender(
                "ted",
                "ted-2",
                "Stad Gent",
                "Schilderwerken school",
                "45440000",
                "2027-01-15",
            ),
            tender(
                "ted",
                "ted-3",
                "Stad Antwerpen",
                "Elektrische installatie school",
                "45310000",
                "2027-01-15",
            ),
        ];
        let matches = find_probable_duplicates(&incoming, &stored);
        assert_eq!(matches.len(), 1, "only the same buyer, CPVs and title");
        assert_eq!(matches[0].external_id, "ted-1");
        assert_eq!(matches[0].title_similarity, 1.0);
        assert!(matches[0].same_deadline);
        assert_eq!(matches[0].deadline_delta_days, Some(0));
    }

    #[test]
    fn a_moved_deadline_is_reported_not_filtered() {
        let incoming = tender(
            "bda",
            "bda-1",
            "Stad Gent",
            "Elektrische installatie school",
            "45310000",
            "2027-02-20",
        );
        let stored = vec![tender(
            "ted",
            "ted-1",
            "Stad Gent",
            "Elektrische installatie school",
            "45310000",
            "2027-01-15",
        )];
        let matches = find_probable_duplicates(&incoming, &stored);
        assert_eq!(matches.len(), 1);
        assert!(!matches[0].same_deadline);
        assert_eq!(matches[0].deadline_delta_days, Some(36));
    }

    #[test]
    fn ignores_the_same_source_and_unknown_buyers() {
        let incoming = tender(
            "bda",
            "bda-1",
            "Stad Gent",
            "Elektrische installatie",
            "45310000",
            "2027-01-15",
        );
        let same_source = vec![tender(
            "bda",
            "bda-2",
            "Stad Gent",
            "Elektrische installatie",
            "45310000",
            "2027-01-15",
        )];
        assert!(find_probable_duplicates(&incoming, &same_source).is_empty());

        let mut anonymous = incoming.clone();
        anonymous.buyer = None;
        assert!(find_probable_duplicates(&anonymous, &same_source).is_empty());
    }

    #[test]
    fn a_different_cpv_set_is_not_a_match() {
        let incoming = tender(
            "bda",
            "bda-1",
            "Stad Gent",
            "Elektrische installatie",
            "45310000",
            "2027-01-15",
        );
        let stored = vec![tender(
            "ted",
            "ted-1",
            "Stad Gent",
            "Elektrische installatie",
            "45440000",
            "2027-01-15",
        )];
        assert!(find_probable_duplicates(&incoming, &stored).is_empty());
    }
}
