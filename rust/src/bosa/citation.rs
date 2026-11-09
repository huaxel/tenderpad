//! Machine check that a proposed quote is verbatim source text, and where.
//!
//! This is the invariant behind "no citation, no claim" (attachment strategy,
//! stage 5; benchmark RQ4 part a/b): a candidate requirement, whether from a
//! rule or a model, is kept only if its quote is an exact substring of one
//! retained segment. It proves the words exist at that place. It does **not**
//! prove the quote supports the claim or applies to the lot; that stays a
//! reviewer judgment.
//!
//! `NotFound` is only evidence of absence when the extraction was complete: check
//! `ExtractedText::truncated` (and `failed_sheets`) before treating it as such.
//!
//! Matching is strictly verbatim in the first pass. If no verbatim match is found,
//! a second pass tests for typographic normalization (canonicalising apostrophes
//! like ' vs ’ and quotation marks like " vs « » “ ”). This distinction is preserved
//! in `CitationCheck` so the benchmark protocol can report strict verbatim fidelity
//! (RQ4.a) separately from typographic-invariant fidelity (RQ4.b) without silent
//! loosening. Case folding, whitespace collapsing or fuzzy matching remain rejected.
use super::ooxml_text::TextSegment;
use serde::Serialize;

/// Shorter quotes match almost anywhere and prove nothing.
pub const MIN_QUOTE_CHARS: usize = 12;
const MAX_REPORTED_SPANS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceSpan {
    /// The segment's own location (`paragraph 3`, `Sheet1!B7`, `page 4`, ...).
    pub location: String,
    /// Half-open range in Unicode characters (not bytes) within that segment's text.
    pub char_span: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum CitationCheck {
    /// The quote occurs verbatim. More than one span means the place is
    /// ambiguous; the caller must not pick one silently.
    Verbatim {
        spans: Vec<SourceSpan>,
        /// True when more matches existed than are listed.
        more_matches_not_listed: bool,
    },
    /// The quote occurs with typographic variation only (apostrophes like ' vs ’
    /// or quotation marks like " vs « » “ ”). Reported distinctly so benchmark
    /// evaluators can isolate typographic drift from model hallucinations.
    NormalizedTypographic {
        spans: Vec<SourceSpan>,
        /// True when more matches existed than are listed.
        more_matches_not_listed: bool,
    },
    /// Not an exact or typographically normalized substring of any single segment.
    /// A quote spanning two segments (for example two table cells) is rejected here by design.
    NotFound,
    /// Empty, whitespace-only, or shorter than `MIN_QUOTE_CHARS` characters.
    QuoteTooShort,
}

impl CitationCheck {
    /// Verbatim and located at exactly one place.
    pub fn is_unique_verbatim(&self) -> bool {
        matches!(self, Self::Verbatim { spans, more_matches_not_listed: false } if spans.len() == 1)
    }

    /// Verbatim match, whether unique or repeated.
    pub fn is_verbatim(&self) -> bool {
        matches!(self, Self::Verbatim { .. })
    }

    /// Typographically normalized match, whether unique or repeated.
    pub fn is_normalized_typographic(&self) -> bool {
        matches!(self, Self::NormalizedTypographic { .. })
    }

    /// Located at exactly one place (either verbatim or typographically normalized).
    pub fn is_unique_match(&self) -> bool {
        matches!(
            self,
            Self::Verbatim { spans, more_matches_not_listed: false }
                | Self::NormalizedTypographic { spans, more_matches_not_listed: false }
            if spans.len() == 1
        )
    }

    /// Access the located spans regardless of verbatim or normalized status.
    pub fn spans(&self) -> &[SourceSpan] {
        match self {
            Self::Verbatim { spans, .. } | Self::NormalizedTypographic { spans, .. } => spans,
            Self::NotFound | Self::QuoteTooShort => &[],
        }
    }
}

/// Canonicalise typographic apostrophes and quotation marks to ASCII equivalents.
///
/// Each mapped character is a single Unicode scalar value (`char`), guaranteeing
/// 1:1 character offset preservation between original and canonicalised text.
pub fn canonicalize_typographic_char(c: char) -> char {
    match c {
        // Apostrophes & single quotation marks -> straight ASCII apostrophe '\'' (U+0027)
        '\'' | '\u{2019}' | '\u{2018}' | '\u{201A}' | '\u{201B}' | '\u{02BC}' | '\u{00B4}'
        | '\u{0060}' | '\u{2032}' => '\'',
        // Double quotation marks & guillemets -> straight ASCII double quote '"' (U+0022)
        '"' | '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{00AB}' | '\u{00BB}' | '\u{2033}' => '"',
        other => other,
    }
}

/// Canonicalise all apostrophes and quotation marks in a string.
pub fn canonicalize_typography(s: &str) -> String {
    s.chars().map(canonicalize_typographic_char).collect()
}

pub fn verify_quote(segments: &[TextSegment], quote: &str) -> CitationCheck {
    if quote.trim().chars().count() < MIN_QUOTE_CHARS || quote != quote.trim() {
        return CitationCheck::QuoteTooShort;
    }
    let quote_chars = quote.chars().count();
    let mut spans = Vec::new();
    let mut more = false;

    // Pass 1: exact verbatim substring search
    'segments: for segment in segments {
        let mut from = 0;
        while let Some(found) = segment.text[from..].find(quote) {
            let byte_start = from + found;
            if spans.len() == MAX_REPORTED_SPANS {
                more = true;
                break 'segments;
            }
            let start = segment.text[..byte_start].chars().count();
            spans.push(SourceSpan {
                location: segment.location.clone(),
                char_span: (start, start + quote_chars),
            });
            // Step past the first character of the match so overlapping matches count.
            from = byte_start
                + segment.text[byte_start..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8);
        }
    }
    if !spans.is_empty() {
        return CitationCheck::Verbatim {
            spans,
            more_matches_not_listed: more,
        };
    }

    // Pass 2: typographic normalization (apostrophes and quotes only)
    let norm_quote = canonicalize_typography(quote);
    let mut norm_spans = Vec::new();
    let mut norm_more = false;

    'norm_segments: for segment in segments {
        let norm_segment = canonicalize_typography(&segment.text);
        let mut from = 0;
        while let Some(found) = norm_segment[from..].find(&norm_quote) {
            let byte_start = from + found;
            if norm_spans.len() == MAX_REPORTED_SPANS {
                norm_more = true;
                break 'norm_segments;
            }
            // Because each canonicalized character is a 1:1 scalar mapping, the character offset
            // in norm_segment is identical to the character offset in segment.text.
            let start = norm_segment[..byte_start].chars().count();
            norm_spans.push(SourceSpan {
                location: segment.location.clone(),
                char_span: (start, start + quote_chars),
            });
            from = byte_start
                + norm_segment[byte_start..]
                    .chars()
                    .next()
                    .map_or(1, char::len_utf8);
        }
    }

    if !norm_spans.is_empty() {
        CitationCheck::NormalizedTypographic {
            spans: norm_spans,
            more_matches_not_listed: norm_more,
        }
    } else {
        CitationCheck::NotFound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(location: &str, text: &str) -> TextSegment {
        TextSegment {
            location: location.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn an_exact_quote_is_located_by_segment_and_character_span() {
        let segments = [
            seg("paragraph 1", "Algemene bepalingen"),
            seg(
                "paragraph 2",
                "Minimale jaaromzet van 500.000 EUR over de laatste drie jaar.",
            ),
        ];
        let check = verify_quote(&segments, "jaaromzet van 500.000 EUR");
        assert_eq!(
            check,
            CitationCheck::Verbatim {
                spans: vec![SourceSpan {
                    location: "paragraph 2".into(),
                    char_span: (9, 34)
                }],
                more_matches_not_listed: false
            }
        );
        assert!(check.is_unique_verbatim());
    }

    #[test]
    fn spans_count_characters_not_bytes() {
        let segments = [seg(
            "paragraph 1",
            "Évaluation: capacité économique minimale requise",
        )];
        let CitationCheck::Verbatim { spans, .. } =
            verify_quote(&segments, "capacité économique minimale")
        else {
            panic!("expected a match");
        };
        // "Évaluation: " is 12 characters but 13 bytes.
        assert_eq!(spans[0].char_span, (12, 40));
    }

    #[test]
    fn a_paraphrase_or_changed_whitespace_or_case_is_rejected_not_repaired() {
        let segments = [seg(
            "paragraph 1",
            "De inschrijver heeft een omzet van minstens 1 miljoen",
        )];
        for quote in [
            "inschrijver moet een omzet hebben van 1 miljoen",
            "De inschrijver  heeft een omzet",
            "DE INSCHRIJVER HEEFT EEN OMZET",
        ] {
            assert_eq!(verify_quote(&segments, quote), CitationCheck::NotFound);
        }
    }

    #[test]
    fn a_quote_spanning_two_segments_is_not_found() {
        let segments = [
            seg("table 1 row 1 cell 1", "Gewicht prijs"),
            seg("table 1 row 1 cell 2", "zestig procent"),
        ];
        assert_eq!(
            verify_quote(&segments, "Gewicht prijs zestig procent"),
            CitationCheck::NotFound
        );
    }

    #[test]
    fn repeated_text_reports_every_place_and_is_not_unique() {
        let segments = [
            seg("paragraph 1", "zie de bijzondere voorwaarden hierboven"),
            seg("paragraph 9", "zie de bijzondere voorwaarden hieronder"),
        ];
        let check = verify_quote(&segments, "zie de bijzondere voorwaarden");
        let CitationCheck::Verbatim { spans, .. } = &check else {
            panic!("expected matches");
        };
        assert_eq!(spans.len(), 2);
        assert!(!check.is_unique_verbatim());
    }

    #[test]
    fn overlapping_matches_inside_one_segment_are_both_reported() {
        let segments = [seg("paragraph 1", "aaaaaaaaaaaaa")];
        let CitationCheck::Verbatim { spans, .. } = verify_quote(&segments, "aaaaaaaaaaaa") else {
            panic!("expected matches");
        };
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn short_empty_or_untrimmed_quotes_prove_nothing() {
        let segments = [seg("paragraph 1", "omzet van 1 miljoen euro")];
        for quote in [
            "",
            "   ",
            "omzet",
            " omzet van 1 miljoen ",
            "omzet van 1 miljoen\n",
        ] {
            assert_eq!(verify_quote(&segments, quote), CitationCheck::QuoteTooShort);
        }
    }

    #[test]
    fn match_listing_is_bounded_and_says_so() {
        let text = "x".repeat(12 + 30);
        let segments = [seg("paragraph 1", &text)];
        let CitationCheck::Verbatim {
            spans,
            more_matches_not_listed,
        } = verify_quote(&segments, &"x".repeat(12))
        else {
            panic!("expected matches");
        };
        assert_eq!(spans.len(), MAX_REPORTED_SPANS);
        assert!(more_matches_not_listed);
    }

    #[test]
    fn no_segments_means_not_found() {
        assert_eq!(
            verify_quote(&[], "een voldoende lange quote"),
            CitationCheck::NotFound
        );
    }

    #[test]
    fn typographic_quote_matching_french_curly_apostrophe_in_source() {
        let text = "Le chiffre d’affaires annuel minimal est de 1.000.000 EUR.";
        let segments = [seg("paragraph 1", text)];
        // Quote uses straight ASCII apostrophe, source uses curly apostrophe U+2019
        let quote = "chiffre d'affaires annuel minimal";
        let check = verify_quote(&segments, quote);
        assert_eq!(
            check,
            CitationCheck::NormalizedTypographic {
                spans: vec![SourceSpan {
                    location: "paragraph 1".into(),
                    char_span: (3, 36)
                }],
                more_matches_not_listed: false,
            }
        );
        assert!(check.is_normalized_typographic());
        assert!(!check.is_verbatim());
        assert!(check.is_unique_match());
        // Verify that the char_span indexes the exact source text slice
        let chars: Vec<char> = text.chars().collect();
        let slice: String = chars[3..36].iter().collect();
        assert_eq!(slice, "chiffre d’affaires annuel minimal");
    }

    #[test]
    fn typographic_quote_matching_curly_apostrophe_in_quote() {
        let text = "Le chiffre d'affaires annuel minimal est de 1.000.000 EUR.";
        let segments = [seg("paragraph 1", text)];
        // Quote uses curly apostrophe U+2019, source uses straight ASCII apostrophe
        let quote = "chiffre d’affaires annuel minimal";
        let check = verify_quote(&segments, quote);
        assert_eq!(
            check,
            CitationCheck::NormalizedTypographic {
                spans: vec![SourceSpan {
                    location: "paragraph 1".into(),
                    char_span: (3, 36)
                }],
                more_matches_not_listed: false,
            }
        );
        assert!(check.is_normalized_typographic());
        assert!(!check.is_verbatim());
        assert!(check.is_unique_match());
    }

    #[test]
    fn typographic_quote_matching_dutch_quotation_marks() {
        let text = "Voldoen aan de eis „professionele integriteit” is verplicht.";
        let segments = [seg("art 3", text)];
        // Quote uses straight ASCII double quotes
        let quote = "\"professionele integriteit\"";
        let check = verify_quote(&segments, quote);
        assert_eq!(
            check,
            CitationCheck::NormalizedTypographic {
                spans: vec![SourceSpan {
                    location: "art 3".into(),
                    char_span: (19, 46)
                }],
                more_matches_not_listed: false,
            }
        );
        assert!(check.is_normalized_typographic());
        let chars: Vec<char> = text.chars().collect();
        let slice: String = chars[19..46].iter().collect();
        assert_eq!(slice, "„professionele integriteit”");
    }

    #[test]
    fn typographic_quote_matching_french_guillemets() {
        let text = "Se référer aux «normes minimales» applicables.";
        let segments = [seg("art 5", text)];
        // Quote uses straight ASCII double quotes around the French guillemet term
        let quote = "\"normes minimales\"";
        let check = verify_quote(&segments, quote);
        assert_eq!(
            check,
            CitationCheck::NormalizedTypographic {
                spans: vec![SourceSpan {
                    location: "art 5".into(),
                    char_span: (15, 33)
                }],
                more_matches_not_listed: false,
            }
        );
        assert!(check.is_normalized_typographic());
        let chars: Vec<char> = text.chars().collect();
        let slice: String = chars[15..33].iter().collect();
        assert_eq!(slice, "«normes minimales»");
    }

    #[test]
    fn verbatim_takes_precedence_over_typographic_normalization() {
        let text = "Le chiffre d’affaires annuel minimal est de 1.000.000 EUR.";
        let segments = [seg("paragraph 1", text)];
        // Quote has the identical curly apostrophe as source: must be Verbatim, not NormalizedTypographic
        let quote = "chiffre d’affaires annuel minimal";
        let check = verify_quote(&segments, quote);
        assert_eq!(
            check,
            CitationCheck::Verbatim {
                spans: vec![SourceSpan {
                    location: "paragraph 1".into(),
                    char_span: (3, 36)
                }],
                more_matches_not_listed: false,
            }
        );
        assert!(check.is_verbatim());
        assert!(!check.is_normalized_typographic());
        assert!(check.is_unique_verbatim());
        assert!(check.is_unique_match());
    }

    #[test]
    fn typographic_normalization_does_not_forgive_whitespace_case_or_paraphrase() {
        let text = "Le chiffre d’affaires annuel minimal est de 1.000.000 EUR.";
        let segments = [seg("paragraph 1", text)];
        for quote in [
            "Le Chiffre d'affaires annuel", // case difference
            "chiffre  d'affaires annuel",   // extra whitespace
            "chiffre daffaires annuel",     // omitted apostrophe
            "chiffre d'affaire annuel",     // singular instead of plural
        ] {
            assert_eq!(verify_quote(&segments, quote), CitationCheck::NotFound);
        }
    }

    #[test]
    fn spans_helper_returns_slice_for_verbatim_and_normalized() {
        let text = "Le chiffre d’affaires annuel minimal est de 1.000.000 EUR.";
        let segments = [seg("paragraph 1", text)];

        let v_check = verify_quote(&segments, "chiffre d’affaires annuel minimal");
        assert_eq!(v_check.spans().len(), 1);
        assert_eq!(v_check.spans()[0].char_span, (3, 36));

        let t_check = verify_quote(&segments, "chiffre d'affaires annuel minimal");
        assert_eq!(t_check.spans().len(), 1);
        assert_eq!(t_check.spans()[0].char_span, (3, 36));

        assert_eq!(CitationCheck::NotFound.spans(), &[]);
        assert_eq!(CitationCheck::QuoteTooShort.spans(), &[]);
    }
}
