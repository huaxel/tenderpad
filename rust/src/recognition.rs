//! Belgian contractor recognition ("erkenning" / "agréation") as stated in
//! notice text.
//!
//! Recognition is graded in classes (1–8, capping the contract amount) and in
//! categories / subcategories (a letter plus optional digits, describing the
//! kind of works). Works above an amount set by royal decree may only be
//! executed by a contractor recognised in the matching class and category, or
//! by one who proves it meets the recognition conditions (Wet van 20 maart 1991
//! houdende regeling van de erkenning van aannemers van werken, art. 3, first
//! paragraph). The classes and the category letters come from the KB van
//! 26 september 1991, art. 3 and 4; both texts are in `docs/legal/bronnen/`.
//! Belgian notices state the requirement in the selection criteria, for example:
//!
//! ```text
//! Erkenning en registratie van aannemers: C (Wegenbouwkundige werken), Klasse 7
//! Agréation et enregistrement des entrepreneurs: D (Entreprises générales de
//! bâtiments), Classe 8
//! ```
//!
//! Measured on a 30-day TED Belgian window (2026-10-18): 176 works calls for
//! competition, 49 mentioning recognition, 48 stating a class — always in
//! `selection-criterion-description-lot`, with 189 tokens ranging over classes
//! 2–8, i.e. several lots in one notice can demand different classes. BOSA can
//! also carry an explicit clause only in the notice description; this module
//! labels that weaker source separately. Many notices state no recognition
//! requirement in indexed notice text, so it may live only in tender documents.
//!
//! This module reports what a notice states and how that relates to what a
//! company declared. It is deliberately *not* an eligibility decision: a
//! non-recognised bidder may still be evaluated and needs at least a
//! provisional recognition at award.

use crate::{matcher_tables, CompanyPreferences, Tender};
use serde::Serialize;

/// Where the recognition clause was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecognitionTextSource {
    Requirements,
    Description,
}

/// What a notice states about recognition, verbatim from its source text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecognitionStatement {
    /// Source is explicit because descriptions are less structured than criteria.
    pub source: Option<RecognitionTextSource>,
    /// The clause makes class depend on an aggregate lot amount not computed here.
    pub lot_aggregation_needs_review: bool,
    /// Recognition is mentioned, even when no class or category is given.
    pub mentioned: bool,
    /// Every class number stated, deduplicated and sorted ascending.
    pub classes: Vec<u8>,
    /// Every category/subcategory code stated, deduplicated and sorted.
    pub categories: Vec<String>,
}

impl RecognitionStatement {
    pub fn is_empty(&self) -> bool {
        !self.mentioned && self.classes.is_empty() && self.categories.is_empty()
    }

    /// The most demanding class stated. Recognition in a class also covers the
    /// lower classes, so the highest stated class is the binding one.
    pub fn highest_class(&self) -> Option<u8> {
        self.classes.last().copied()
    }
}

/// How a notice's stated recognition requirement relates to a declared one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecognitionFit {
    /// The notice states categories and the company declared none, so that
    /// dimension cannot be checked — the class may still fit, but calling it
    /// covered would reassure the reader about something nobody verified.
    CategoryUndeclared { required: Vec<String> },
    /// Nothing is declared, so nothing can be compared (today's behaviour).
    Undeclared,
    /// Recognition is mentioned, but the notice states no class or category.
    Unstated,
    /// The declared recognition covers every class and category stated.
    Covered,
    /// The notice states a class above the declared one.
    AboveDeclaredClass { required: u8, declared: u8 },
    /// The notice states categories the company did not declare.
    CategoryMismatch {
        required: Vec<String>,
        declared: Vec<String>,
    },
}

/// The official recognition category letters, as defined by art. 4 of the KB van
/// 26 september 1991 (I, J, O, Q and R are not used). A category may carry
/// digits: `D17`, `P1`, `T4`.
const CATEGORY_LETTERS: &str = "ABCDEFGHKLMNPSTUV";

fn normalize(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|character| match character {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            other => other,
        })
        .collect()
}

fn explicit_description_recognition(text: &str) -> bool {
    let text = normalize(text);
    [
        "agreation requise",
        "agreation exigee",
        "agreation obligatoire",
        "agreation suivante",
        "titulaire au minimum de l'agreation",
        "vereiste erkenning",
        "verplichte erkenning",
        "erkenning vereist",
        "erkenning en registratie van aannemers",
        "recognition required",
    ]
    .iter()
    .any(|marker| contains_phrase(&text, marker))
}

fn contains_phrase(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(start, matched)| {
        let before_is_boundary = text[..start]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_alphanumeric());
        let end = start + matched.len();
        let after_is_boundary = text[end..]
            .chars()
            .next()
            .is_none_or(|character| !character.is_alphanumeric());
        before_is_boundary && after_is_boundary
    })
}

fn description_has_category_class_pair(text: &str, class_terms: &[String]) -> bool {
    let text = normalize(text);
    let classes = class_numbers(&text, class_terms);
    !classes.is_empty()
        && category_codes(&text, class_terms)
            .into_iter()
            .any(|(position, _)| category_is_labeled(&text, position, &classes, class_terms))
}

/// Extract a recognition clause from structured criteria, falling back to description.
pub fn recognition_statement(tender: &Tender) -> RecognitionStatement {
    let terms = &matcher_tables().recognition_terms;
    // Proof words are matched case-insensitively but *without* stripping
    // accents, so French `agréé` / `agrément` / `agrégation` are recognised
    // while an English notice that merely says "agree" is not. Both accented and
    // unaccented spellings are listed in the table, because Belgian notices use
    // both. The class words below are accent-folded on both sides instead.
    let proof: Vec<String> = terms.proof.iter().map(|term| term.to_lowercase()).collect();
    let mentions_proof = |text: &str| {
        let lowered = text.to_lowercase();
        proof.iter().any(|term| lowered.contains(term.as_str()))
    };
    // Prefer structured criteria. Some BOSA notices state a concrete clause only
    // in the description; retain that weaker provenance instead of omitting it.
    let requirements_mention = tender.requirements.iter().any(|r| mentions_proof(&r.text));
    let source = if requirements_mention {
        RecognitionTextSource::Requirements
    } else if explicit_description_recognition(&tender.description)
        || (mentions_proof(&tender.description)
            && description_has_category_class_pair(&tender.description, &terms.class))
    {
        RecognitionTextSource::Description
    } else {
        return RecognitionStatement::default();
    };
    let mut statement = RecognitionStatement {
        source: Some(source),
        mentioned: true,
        ..RecognitionStatement::default()
    };
    // Scan every structured requirement, but never merge description text into
    // criteria. Description fallback is used only when criteria have no proof.
    let texts: Vec<&str> = match source {
        RecognitionTextSource::Requirements => tender
            .requirements
            .iter()
            .map(|r| r.text.as_str())
            .collect(),
        RecognitionTextSource::Description => vec![tender.description.as_str()],
    };
    for requirement_text in texts {
        if requirement_text.trim().is_empty() {
            continue;
        }
        let text = normalize(requirement_text);
        if text.contains("cumul") && (text.contains("lot") || text.contains("percel")) {
            statement.lot_aggregation_needs_review = true;
        }
        let classes = class_numbers(&text, &terms.class);
        for (_, class) in &classes {
            statement.classes.push(*class);
        }
        for (position, category) in category_codes(&text, &terms.class) {
            if category_is_labeled(&text, position, &classes, &terms.class) {
                statement.categories.push(category);
            }
        }
    }
    statement.classes.sort_unstable();
    statement.classes.dedup();
    statement.categories.sort_unstable();
    statement.categories.dedup();
    statement
}

/// `klasse 7`, `classe 8`, `klasse 7 *`, `categorie 3` — a class word followed by
/// a single digit. Only 1–8 is a recognition class: `klasse 9` and the `12` in
/// `class 12` are ignored rather than guessed at. Returns the byte offset of the
/// class word with the number, which the category rule uses to tell a category
/// code from an ordinary itemized list.
fn class_numbers(text: &str, class_terms: &[String]) -> Vec<(usize, u8)> {
    let mut found = Vec::new();
    for term in class_terms {
        let term = normalize(term);
        for (position, _) in text.match_indices(&term) {
            let after_term = &text[position + term.len()..];
            let after_term =
                after_term.trim_start_matches([' ', '\t', '\n', '\r', '.', ':', '*', '(', ')']);
            let mut characters = after_term.char_indices();
            let Some((_, digit)) = characters.next() else {
                continue;
            };
            if !digit.is_ascii_digit() {
                continue;
            }
            if let Some((_, next)) = characters.next() {
                if next.is_ascii_digit() {
                    continue;
                }
            }
            if let Ok(class) = digit.to_string().parse::<u8>() {
                if (1..=8).contains(&class) {
                    found.push((position, class));
                }
            }
        }
    }
    found.sort_unstable_by_key(|(position, _)| *position);
    found
}

/// The up to `max_chars` characters before `position`, without assuming byte
/// offsets: notice text is full of multi-byte characters (curly apostrophes,
/// accented French), and slicing at `position - n` bytes panicked on them.
fn preceding_text(text: &str, position: usize, max_chars: usize) -> String {
    let position = position.min(text.len());
    let mut characters = text[..position]
        .chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>();
    characters.reverse();
    characters.into_iter().collect()
}

/// How far a category code may sit from the class token it belongs to. Real
/// notices write `C (Wegenbouwkundige werken), Klasse 7` and
/// `classe 8 lot 2: P1 (installations électriques), T4 (…)`, i.e. within a
/// clause; a scoring list such as `A (prijs) 60 punten … B (kwaliteit)` is far
/// away and is not a recognition category.
const CATEGORY_CLASS_DISTANCE: usize = 60;

/// Whether a `X (`-shaped token is a recognition category rather than the first
/// item of any parenthesised list: it must sit next to a class number, or be
/// introduced by a category word (`categorie D`).
fn category_is_labeled(
    text: &str,
    position: usize,
    classes: &[(usize, u8)],
    class_terms: &[String],
) -> bool {
    if classes
        .iter()
        .any(|(class_position, _)| position.abs_diff(*class_position) <= CATEGORY_CLASS_DISTANCE)
    {
        return true;
    }
    let preceding = preceding_text(text, position, 30);
    class_terms
        .iter()
        .any(|term| preceding.contains(&normalize(term)))
}

/// Category codes that a notice writes as `D (…), `, `P1 (…), ` — a category
/// letter from the official list, optional digits, followed by a parenthesis —
/// or writes as `categorie D` / `categorie D17` without the description.
/// Returns each code with the byte offset where it starts.
fn category_codes(text: &str, class_terms: &[String]) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    for (index, character) in text.char_indices() {
        let letter = character.to_ascii_uppercase();
        if !letter.is_ascii_uppercase() || !CATEGORY_LETTERS.contains(letter) {
            continue;
        }
        // Only a standalone token: the letter must not be part of a word.
        if index > 0 {
            let previous = bytes[index - 1];
            if previous.is_ascii_alphanumeric() {
                continue;
            }
        }
        let mut cursor = index + character.len_utf8();
        let mut digits = String::new();
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() && digits.len() < 2 {
            digits.push(bytes[cursor] as char);
            cursor += 1;
        }
        let mut after = text[cursor..].chars();
        let next = after.next();
        let followed_by_description = if matches!(next, Some(' ') | None) {
            matches!(after.next(), Some('('))
        } else {
            next == Some('(')
        };
        let tail = text[cursor..].trim_start_matches([' ', '\t', '\n', '\r', '.', ':', ',', ';']);
        let followed_by_class = class_terms.iter().any(|term| {
            let term = normalize(term);
            (term.starts_with("class") || term.starts_with("klass")) && tail.starts_with(&term)
        });
        let introduced_by_label = category_label_before(text, index, class_terms);
        if !followed_by_description && !followed_by_class && !introduced_by_label {
            continue;
        }
        found.push((index, format!("{letter}{digits}")));
    }
    found
}

/// Whether a category word (`categorie`, `catégorie`, `kategorie`) introduces the
/// code at `position`. Only category words count: `klasse`/`classe`/`class`
/// introduce the numeric class, not a letter code.
fn category_label_before(text: &str, position: usize, class_terms: &[String]) -> bool {
    let preceding = preceding_text(text, position, 20);
    let preceding = preceding.trim_end_matches(|character: char| {
        character.is_whitespace() || matches!(character, ':' | '=')
    });
    class_terms.iter().any(|term| {
        let term = normalize(term);
        (term.starts_with("cat") || term.starts_with("kateg")) && preceding.ends_with(&term)
    })
}

/// Compare a stated requirement with a declared recognition.
pub fn recognition_fit(
    statement: &RecognitionStatement,
    preferences: &CompanyPreferences,
) -> RecognitionFit {
    let declared_class = preferences.recognition_class;
    let declared_categories = normalized_categories(&preferences.recognition_categories);
    if declared_class.is_none() && declared_categories.is_empty() {
        return RecognitionFit::Undeclared;
    }
    if statement.classes.is_empty() && statement.categories.is_empty() {
        return RecognitionFit::Unstated;
    }
    if let (Some(required), Some(declared)) = (statement.highest_class(), declared_class) {
        if required > declared {
            return RecognitionFit::AboveDeclaredClass { required, declared };
        }
    }
    if !statement.categories.is_empty() {
        if declared_categories.is_empty() {
            return RecognitionFit::CategoryUndeclared {
                required: statement.categories.clone(),
            };
        }
        if !statement.categories.iter().any(|category| {
            declared_categories
                .iter()
                .any(|declared| related_categories(category, declared))
        }) {
            return RecognitionFit::CategoryMismatch {
                required: statement.categories.clone(),
                declared: declared_categories.into_iter().collect(),
            };
        }
    }
    RecognitionFit::Covered
}

/// Whether two stated category codes describe the same family. Recognition is
/// granted per category and subcategory (`D` for general building works, `D17`
/// for tiling), and the notices name either level, so a code that prefixes the
/// other is treated as related. Reporting a mismatch is therefore reserved for
/// genuinely different families — the conservative direction for a warning that
/// must never act as an eligibility verdict.
fn related_categories(stated: &str, declared: &str) -> bool {
    stated.starts_with(declared) || declared.starts_with(stated)
}

/// Normalise declared categories for comparison: trimmed, upper-cased, and
/// without an optional `SUB`/`SUBCAT` prefix or trailing punctuation.
pub fn normalized_categories(categories: &[String]) -> Vec<String> {
    let mut normalized = categories
        .iter()
        .map(|category| {
            category
                .trim()
                .trim_end_matches(['.', ',', ';'])
                .to_ascii_uppercase()
                .replace(' ', "")
        })
        .filter(|category| !category.is_empty())
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

/// Reject a recognition declaration that could not be compared to a notice:
/// a class outside 1–8, or a code that is not a category letter with at most
/// two digits. Returns the error to show the caller.
pub fn validate_recognition(class: Option<u8>, categories: &[String]) -> Result<(), String> {
    if let Some(class) = class {
        if !(1..=8).contains(&class) {
            return Err(format!(
                "recognition class must be between 1 and 8, got {class}"
            ));
        }
    }
    for category in normalized_categories(categories) {
        let mut characters = category.chars();
        let first = characters.next().unwrap_or_default();
        if !CATEGORY_LETTERS.contains(first) {
            return Err(format!(
                "recognition category {category} must start with one of {CATEGORY_LETTERS}"
            ));
        }
        let digits = characters.as_str();
        if digits.len() > 2 || !digits.chars().all(|digit| digit.is_ascii_digit()) {
            return Err(format!(
                "recognition category {category} must be a letter with up to two digits, like D17"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TenderRequirement;

    fn tender_with(requirements: &[&str]) -> Tender {
        Tender {
            requirements: requirements
                .iter()
                .map(|text| TenderRequirement {
                    text: (*text).to_owned(),
                    ..TenderRequirement::default()
                })
                .collect(),
            ..Tender::default()
        }
    }

    fn preferences(class: Option<u8>, categories: &[&str]) -> CompanyPreferences {
        CompanyPreferences {
            recognition_class: class,
            recognition_categories: categories.iter().map(|c| (*c).to_owned()).collect(),
            ..CompanyPreferences::default()
        }
    }

    /// The exact strings measured on real Belgian notices on 2026-10-18.
    #[test]
    fn extracts_the_class_and_category_from_real_notice_text() {
        let tender = tender_with(&[
            "Erkenning en registratie van aannemers: C (Wegenbouwkundige werken), Klasse 7",
        ]);
        let statement = recognition_statement(&tender);
        assert_eq!(statement.classes, vec![7]);
        assert_eq!(statement.categories, vec!["C"]);
        assert!(statement.mentioned);

        let french = tender_with(&[
            "Agréation et enregistrement des entrepreneurs: D (Entreprises générales de bâtiments), Classe 8",
        ]);
        let statement = recognition_statement(&french);
        assert_eq!(statement.classes, vec![8]);
        assert_eq!(statement.categories, vec!["D"]);

        let subcategory = tender_with(&[
            "Erkenning vereist; verplichte vermelding: classe 8 lot 2: P1 (installations électriques), T4 (equipements)",
        ]);
        let statement = recognition_statement(&subcategory);
        assert_eq!(statement.classes, vec![8]);
        assert_eq!(statement.categories, vec!["P1", "T4"]);
    }

    /// BOSA notice descriptions can carry the complete clause without a structured requirement.
    #[test]
    fn parses_real_bosa_description_wording_without_promoting_it_to_criteria() {
        let tender = Tender {
            description: "Agréation requise D classe 5. En cas d'attribution de plusieurs lots, le soumissionnaire doit posséder la classe d'agréation correspondant au montant cumulé des lots.".to_owned(),
            ..Tender::default()
        };
        let statement = recognition_statement(&tender);
        assert_eq!(statement.source, Some(RecognitionTextSource::Description));
        assert!(statement.mentioned);
        assert_eq!(statement.classes, vec![5]);
        assert_eq!(statement.categories, vec!["D"]);
        assert!(statement.lot_aggregation_needs_review);
    }

    #[test]
    fn recognizes_other_bosa_category_class_wordings_and_rejects_incidental_proof_words() {
        let dutch = Tender {
            description: "VEREISTE ERKENNING: categorie C De aanbestedende overheid is van oordeel dat deze werken behoren tot de klasse 4.".to_owned(),
            ..Tender::default()
        };
        let statement = recognition_statement(&dutch);
        assert_eq!(statement.source, Some(RecognitionTextSource::Description));
        assert_eq!(statement.classes, vec![4]);
        assert_eq!(statement.categories, vec!["C"]);

        let french = Tender {
            description: "Agréation requise : les soumissionnaires doivent être titulaires au minimum de l'agréation suivante : Classe 3, Sous-catégorie P1 (Installations électriques des bâtiments)".to_owned(),
            ..Tender::default()
        };
        let statement = recognition_statement(&french);
        assert_eq!(statement.source, Some(RecognitionTextSource::Description));
        assert_eq!(statement.classes, vec![3]);
        assert_eq!(statement.categories, vec!["P1"]);

        for incidental in [
            "Le nouvel uniforme doit aussi refléter classe et reconnaissance.",
            "Les matériaux sont évacués vers une décharge agréée ou un centre de recyclage.",
            "De werken gebeuren door personeel met de vereiste erkenningen.",
            "Marché de reconnaissance de plaques minéralogiques.",
        ] {
            let statement = recognition_statement(&Tender {
                description: incidental.to_owned(),
                ..Tender::default()
            });
            assert_eq!(statement, RecognitionStatement::default(), "{incidental}");
        }
    }

    /// Real notices place the clause in the last requirement, and one notice
    /// can state several classes because it is split into lots.
    #[test]
    fn scans_every_requirement_and_keeps_every_class() {
        let tender = tender_with(&[
            "L'équipe doit compter un chef de projet.",
            "Agréation et enregistrement des entrepreneurs: D (Entreprises générales de bâtiments), Classe 8",
            "Erkenning: C (Wegenbouwkundige werken), Klasse 3",
        ]);
        let statement = recognition_statement(&tender);
        assert_eq!(statement.classes, vec![3, 8]);
        assert_eq!(statement.highest_class(), Some(8));
        assert_eq!(statement.categories, vec!["C", "D"]);
    }

    /// Wording measured on the notices the first version missed: French
    /// `agréé` / `agrément` / `agrégation` and the Dutch `erkenningsregels`.
    /// Accents must not be folded away, because folding turns `agréé` into
    /// `agree` and would fire on English notices that merely agree to terms.
    #[test]
    fn catches_every_spelling_seen_in_real_notices() {
        for (wording, expected_mention) in [
            (
                "L'installateur doit être agréé par le Ministre de l'Intérieur",
                true,
            ),
            (
                "Le candidat doit disposer d'un agrément pour pratiquer ces branches",
                true,
            ),
            (
                "Il est renvoyé à l'agrégation des entrepreneurs telle que prévue",
                true,
            ),
            (
                "De erkenningsregels voor aannemers zijn van toepassing",
                true,
            ),
            (
                "Erkenning en registratie van aannemers: C (Wegenbouwkundige werken), Klasse 5",
                true,
            ),
            (
                "AGREATION ET ENREGISTREMENT DES ENTREPRENEURS: D, Classe 8",
                true,
            ),
            (
                "The parties agree that this contract is governed by Belgian law",
                false,
            ),
            (
                "De inschrijver verklaart in te stemmen met de algemene voorwaarden",
                false,
            ),
        ] {
            let statement = recognition_statement(&tender_with(&[wording]));
            assert_eq!(
                statement.mentioned, expected_mention,
                "wording: {wording} -> {statement:?}"
            );
        }
        // The Dutch stem also carries a class and subcategory through.
        let statement = recognition_statement(&tender_with(&[
            "Erkenningsregels: C (Wegenbouwkundige werken), Klasse 5",
        ]));
        assert_eq!(statement.classes, vec![5]);
        assert_eq!(statement.categories, vec!["C"]);
    }

    /// Numbers that are not recognition requirements must not become one: the
    /// class/category scan only runs on notices that mention recognition.
    #[test]
    fn ignores_class_and_category_numbers_without_recognition_context() {
        for wording in [
            "Class 3 laser equipment must be used for the measurements",
            "Afvoer van categorie 3 afval volgens het Vlaams reglement",
            "Categorie 2 persoonlijke beschermingsmiddelen zijn verplicht",
            "A (prijs) 60 punten, B (kwaliteit) 40 punten, Klasse 4 materiaal",
        ] {
            let statement = recognition_statement(&tender_with(&[wording]));
            assert!(statement.is_empty(), "wording: {wording} -> {statement:?}");
        }
        // The same numbers next to a recognition mention are read out.
        let statement =
            recognition_statement(&tender_with(&["Erkenning vereist. Klasse 4, categorie D"]));
        assert_eq!(statement.classes, vec![4]);
        assert_eq!(statement.categories, vec!["D"]);
        // A scoring list inside a recognition notice is still not a category.
        let statement = recognition_statement(&tender_with(&[
            "Erkenning vereist volgens de regels. Gunningscriteria: A (prijs) 60 punten, B (kwaliteit) 40 punten, C (dienstverlening) 20 punten",
        ]));
        assert!(statement.categories.is_empty(), "got {statement:?}");
    }

    /// Real notices contain curly quotes and accented text; the category and
    /// label scans look at the text *before* a code, and slicing those by byte
    /// offset panicked on the first multi-byte character it crossed.
    #[test]
    fn handles_multibyte_text_before_a_category_code() {
        let statement = recognition_statement(&tender_with(&[
            "Door in te schrijven op deze opdracht verklaart de inschrijver zich niet in een toestand van uitsluiting te bevinden, zoals bedoeld in artikel 67 - 69 van de wet van 17 juni 2016 inzake overheidsopdrachten en latere wijzigingen “overeenkomstig artikel 67” — erkenning van aannemers: C (Wegenbouwkundige werken), Klasse 7",
        ]));
        assert_eq!(statement.classes, vec![7]);
        assert_eq!(statement.categories, vec!["C"]);
        assert!(statement.mentioned);
    }

    #[test]
    fn recognises_a_mention_without_a_class() {
        let tender = tender_with(&[
            "Conformément à l'article 70 de l'AR du 18 avril 2017, le pouvoir adjudicateur se satisfait de la preuve de l'agréation comme décrit ci-dessous",
        ]);
        let statement = recognition_statement(&tender);
        assert!(statement.mentioned);
        assert!(statement.classes.is_empty());
        assert_eq!(
            recognition_fit(&statement, &preferences(Some(5), &["C"])),
            RecognitionFit::Unstated
        );
    }

    #[test]
    fn ignores_text_that_is_not_a_recognition_class() {
        let tender = tender_with(&[
            "De opdracht wordt gegund op basis van de prijs; klasse 9 bestaat niet.",
            "English notice: class 12 of the personnel must hold a certificate.",
            "Erkenning: C (Wegenbouwkundige werken), klasse 4",
        ]);
        let statement = recognition_statement(&tender);
        assert_eq!(statement.classes, vec![4]);
        assert_eq!(statement.categories, vec!["C"]);
    }

    #[test]
    fn compares_stated_class_with_the_declared_one() {
        let tender = tender_with(&[
            "Erkenning en registratie van aannemers: C (Wegenbouwkundige werken), Klasse 7",
        ]);
        let statement = recognition_statement(&tender);
        // The same class covers it, and a higher class covers it too.
        assert_eq!(
            recognition_fit(&statement, &preferences(Some(7), &["C"])),
            RecognitionFit::Covered
        );
        assert_eq!(
            recognition_fit(&statement, &preferences(Some(8), &["C", "D17"])),
            RecognitionFit::Covered
        );
        // Categories are only compared when the notice states any.
        assert_eq!(
            recognition_fit(&statement, &preferences(None, &["D17"])),
            RecognitionFit::CategoryMismatch {
                required: vec!["C".to_owned()],
                declared: vec!["D17".to_owned()],
            }
        );
    }

    #[test]
    fn reports_a_declared_class_below_the_required_one() {
        let tender = tender_with(&[
            "Erkenning en registratie van aannemers: C (Wegenbouwkundige werken), Klasse 7",
        ]);
        assert_eq!(
            recognition_fit(
                &recognition_statement(&tender),
                &preferences(Some(3), &["C"])
            ),
            RecognitionFit::AboveDeclaredClass {
                required: 7,
                declared: 3
            }
        );
    }

    /// Declaring a class without categories leaves the category dimension
    /// unverified: the notice must say so instead of reporting a fit.
    #[test]
    fn a_stated_category_with_no_declared_categories_is_flagged() {
        let tender = tender_with(&[
            "Agréation et enregistrement des entrepreneurs: D (Entreprises générales de bâtiments), Classe 3",
        ]);
        let statement = recognition_statement(&tender);
        assert_eq!(
            recognition_fit(&statement, &preferences(Some(8), &[])),
            RecognitionFit::CategoryUndeclared {
                required: vec!["D".to_owned()],
            }
        );
        // With a category declared it is compared as before.
        assert_eq!(
            recognition_fit(&statement, &preferences(Some(8), &["D"])),
            RecognitionFit::Covered
        );
    }

    #[test]
    fn reports_a_category_the_company_did_not_declare() {
        let tender = tender_with(&[
            "Agréation et enregistrement des entrepreneurs: D (Entreprises générales de bâtiments), Classe 3",
        ]);
        assert_eq!(
            recognition_fit(
                &recognition_statement(&tender),
                &preferences(Some(8), &["C"])
            ),
            RecognitionFit::CategoryMismatch {
                required: vec!["D".to_owned()],
                declared: vec!["C".to_owned()],
            }
        );
        assert_eq!(
            recognition_fit(
                &recognition_statement(&tender),
                &preferences(Some(4), &["D17"])
            ),
            RecognitionFit::Covered
        );
    }

    #[test]
    fn treats_category_and_subcategory_as_the_same_family() {
        let general = tender_with(&["Agréation: D (Entreprises générales de bâtiments), Classe 3"]);
        let subcategory = tender_with(&["Erkenning: D17 (Tegelwerken), Klasse 3"]);
        assert_eq!(
            recognition_fit(
                &recognition_statement(&general),
                &preferences(None, &["D17"])
            ),
            RecognitionFit::Covered
        );
        assert_eq!(
            recognition_fit(
                &recognition_statement(&subcategory),
                &preferences(None, &["D"])
            ),
            RecognitionFit::Covered
        );
        assert_eq!(
            recognition_fit(
                &recognition_statement(&subcategory),
                &preferences(None, &["P1"])
            ),
            RecognitionFit::CategoryMismatch {
                required: vec!["D17".to_owned()],
                declared: vec!["P1".to_owned()],
            }
        );
    }

    #[test]
    fn nothing_to_compare_stays_undeclared() {
        let tender = tender_with(&["Klasse 7 vereist"]);
        let statement = recognition_statement(&tender);
        assert_eq!(
            recognition_fit(&statement, &CompanyPreferences::default()),
            RecognitionFit::Undeclared
        );
    }

    #[test]
    fn validates_declarations() {
        assert!(validate_recognition(Some(5), &["C".to_owned(), "D17".to_owned()]).is_ok());
        assert!(validate_recognition(None, &[]).is_ok());
        assert!(validate_recognition(Some(0), &[]).is_err());
        assert!(validate_recognition(Some(9), &[]).is_err());
        assert!(validate_recognition(None, &["I1".to_owned()]).is_err());
        assert!(validate_recognition(None, &["D123".to_owned()]).is_err());
        assert!(validate_recognition(None, &["17".to_owned()]).is_err());
        assert_eq!(normalized_categories(&[" d17 ".to_owned()]), vec!["D17"]);
    }
}
