//! Corpus-IDF char-ngram text similarity (TF-IDF cosine).
//!
//! `activity_match` falls back to text overlap when no CPV↔NACE mapping
//! fires. Raw overlap counts boilerplate ("services", "fourniture") the same
//! as substance; weighting char 3–5 grams by corpus IDF fixes that without
//! any learned model. Tables in `rust/data/text_tfidf.json` (60k terms over
//! 3,441 tender notices); held-out retrieval 0.293 MRR / 58.7% top-5,
//! same protocol as the frozen-e5 and TF-IDF numbers in
//! `docs/ml-retrieval-note.md`.
//!
//! The pipeline mirrors the validated prototype exactly: lowercase words
//! split like [`tokens`], padded char n-grams (`char_wb`, 3–5), raw term
//! counts × `ln((1+N)/(1+df)) + 1`, L2 norms, cosine. Out-of-vocabulary
//! grams drop; empty sides score 0.0.

use std::collections::HashMap;
use std::sync::OnceLock;

fn tables() -> &'static HashMap<String, f64> {
    static TABLES: OnceLock<HashMap<String, f64>> = OnceLock::new();
    TABLES.get_or_init(|| {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../data/text_tfidf.json"))
                .expect("text_tfidf.json should parse");
        document["idf"]
            .as_object()
            .expect("idf map should be an object")
            .iter()
            .map(|(term, idf)| (term.clone(), idf.as_f64().expect("idf should be a number")))
            .collect()
    })
}

/// Char 3–5 gram slices of a blank-padded word, borrowed — no allocation
/// per window. Byte ranges come from char boundaries, so slicing is safe
/// for multibyte text.
fn gram_slices(padded: &str) -> Vec<&str> {
    let bytes: Vec<usize> = padded
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(padded.len()))
        .collect();
    let mut slices = Vec::new();
    for width in 3..=5 {
        if bytes.len() - 1 < width {
            continue;
        }
        for start in 0..=(bytes.len() - 1 - width) {
            slices.push(&padded[bytes[start]..bytes[start + width]]);
        }
    }
    slices
}

/// Sorted in-vocabulary (gram, tf-idf weight) pairs. Sorted (not hashed)
/// order keeps float summation identical every run — bit-stability is a
/// contract (regression-tested), not an optimization. Only in-vocabulary
/// grams allocate; OOV windows drop against the table before owning.
fn vectorize(text: &str, stopwords: &std::collections::HashSet<String>) -> Vec<(String, f64)> {
    let tables = tables();
    let lowered = text.to_lowercase();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for word in lowered
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| token.chars().count() >= 4 && !stopwords.contains(*token))
    {
        // Count once per distinct word, then scale its grams: identical to
        // per-occurrence accumulation, without re-slicing repeats.
        *counts.entry(word).or_default() += 1;
    }
    // (gram, raw count) pairs; the same gram recurs across words, so
    // counts merge below — TF sums occurrences before weighting.
    let mut pairs: Vec<(String, usize)> = Vec::new();
    for (word, word_count) in counts {
        // Fresh per word so gram slices never outlive their padded owner.
        let mut scratch: HashMap<&str, usize> = HashMap::new();
        let padded = format!(" {word} ");
        for gram in gram_slices(&padded) {
            // IDF probe before owning: OOV grams never allocate.
            if tables.contains_key(gram) {
                *scratch.entry(gram).or_default() += 1;
            }
        }
        for (gram, gram_count) in scratch.drain() {
            pairs.push((gram.to_owned(), gram_count * word_count));
        }
    }
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    // Merge equal grams in sorted order, then weight once: integer total
    // first, single float multiply — bit-identical to per-occurrence
    // accumulation, and every downstream sum stays in gram order.
    let mut weighted: Vec<(String, f64)> = Vec::with_capacity(pairs.len());
    let mut index = 0;
    while index < pairs.len() {
        let mut total = pairs[index].1;
        let mut end = index + 1;
        while end < pairs.len() && pairs[end].0 == pairs[index].0 {
            total += pairs[end].1;
            end += 1;
        }
        let idf = tables
            .get(pairs[index].0.as_str())
            .expect("probed gram should be in-vocabulary");
        weighted.push((std::mem::take(&mut pairs[index].0), total as f64 * idf));
        index = end;
    }
    weighted
}

/// Whether text has any in-vocabulary grams under the shipped corpus model.
pub fn has_in_vocabulary_ngrams(text: &str, stopwords: &std::collections::HashSet<String>) -> bool {
    !vectorize(text, stopwords).is_empty()
}

/// Cosine similarity under corpus-IDF char-ngram weights. 0.0 when either
/// side has no in-vocabulary grams — unknown text is neutral, not a match.
/// Both inputs arrive pre-sorted, so the dot product is a linear merge and
/// every sum accumulates in gram order: bit-identical across runs.
pub fn tfidf_cosine(left: &str, right: &str, stopwords: &std::collections::HashSet<String>) -> f64 {
    let left = vectorize(left, stopwords);
    let right = vectorize(right, stopwords);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    // Straightforward ordered accumulation: norms in gram order, then the
    // merge for the dot.
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (_, weight) in &left {
        left_norm += weight * weight;
    }
    for (_, weight) in &right {
        right_norm += weight * weight;
    }
    let mut left_index = 0;
    let mut right_index = 0;
    while left_index < left.len() && right_index < right.len() {
        let (left_gram, left_weight) = &left[left_index];
        let (right_gram, right_weight) = &right[right_index];
        match left_gram.cmp(right_gram) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                dot += left_weight * right_weight;
                left_index += 1;
                right_index += 1;
            }
        }
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher_tables;

    fn stopwords() -> std::collections::HashSet<String> {
        matcher_tables().stopwords.clone()
    }

    #[test]
    fn identical_texts_score_one() {
        let score = tfidf_cosine(
            "travaux de construction",
            "travaux de construction",
            &stopwords(),
        );
        assert!((score - 1.0).abs() < 1e-9, "identical texts: {score}");
    }

    #[test]
    fn cosine_is_bit_stable_across_calls() {
        // HashMap iteration order used to jitter the float sum by 1 ulp
        // run-to-run (caught by the reverse-search review). Sorted
        // accumulation pins identical bits for identical inputs.
        let first = tfidf_cosine(
            "réalisation analyses terres déchets sol",
            "analyses environnementales déchets sols pollués",
            &stopwords(),
        );
        for _ in 0..25 {
            assert_eq!(
                tfidf_cosine(
                    "réalisation analyses terres déchets sol",
                    "analyses environnementales déchets sols pollués",
                    &stopwords(),
                )
                .to_bits(),
                first.to_bits()
            );
        }
        // Pinned: any summation-order change flips these bits.
        assert_eq!(first.to_bits(), 0x3fdc9868a59090fe);
    }

    #[test]
    fn empty_sides_score_zero() {
        assert_eq!(tfidf_cosine("", "travaux", &stopwords()), 0.0);
        assert_eq!(tfidf_cosine("travaux", "", &stopwords()), 0.0);
        // Stopwords-only ("de", "het" are short or listed) carries no grams.
        assert_eq!(tfidf_cosine("de het van", "de het van", &stopwords()), 0.0);
    }

    #[test]
    fn related_pair_outscores_mismatch() {
        // sklearn cross-check (same vocab/idf pipeline): 0.4468 vs 0.0192.
        let related = tfidf_cosine(
            "réalisation analyses terres déchets sol",
            "analyses environnementales déchets sols pollués",
            &stopwords(),
        );
        let unrelated = tfidf_cosine(
            "réalisation analyses terres déchets sol",
            "fourniture mobilier bureau chaises",
            &stopwords(),
        );
        assert!(
            (related - 0.4468).abs() < 0.01,
            "related pair should match sklearn: {related}"
        );
        assert!(
            (unrelated - 0.0192).abs() < 0.01,
            "mismatch should match sklearn: {unrelated}"
        );
        assert!(related > unrelated);
    }
}
