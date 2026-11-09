//! TED CSV award history import (DG GROW annual CAN files, 2006–2021).
//!
//! The Search API covers recent competitions; these archives are the only
//! bulk source of *who won what for how much* across years: one row per
//! (notice, lot) with buyer, winner name, award value, CPV and SME flag.
//! Winner identifiers are mostly absent or feral, so winners resolve to KBO
//! through [`normalize_name`] (57.4% of distinct 2021 Belgian winners) plus
//! `---` consortium-member splitting (≈64% linkable combined, measured
//! 2026-10-23). Unresolved rows are kept with `join_confidence = "none"` —
//! training reads high-confidence joins only, never silent mismatches.
//!
//! Ambiguity rule: several entities can share a normalized name (notably an
//! enterprise and its establishments, e.g. `RENEWI BELGIUM` vs the
//! tail-stripped `Renewi Belgium - Roeselare`). Enterprise numbers
//! (leading 0/1) win over establishment numbers (leading 2); remaining ties
//! take the smallest number. The summary counts ambiguous resolutions.

use rust_decimal::Decimal;
use std::collections::{BTreeSet, HashMap};

/// Legal-form and generic-corporate trailing tokens, longest first. Port of
/// the measured Python prototype — keep both in sync when extending.
const FORM_SUFFIXES: &[&str] = &[
    "VENNOOTSCHAP",
    "NAAMLOZE",
    "SOCIETE",
    "ANONYME",
    "COMMV",
    "BVBA",
    "CVBA",
    "CVOA",
    "SCRL",
    "SPRL",
    "ASBL",
    "SARL",
    "GMBH",
    "GCV",
    "VOF",
    "SNC",
    "SCS",
    "SRL",
    "VZW",
    "LTD",
    "SAS",
    "SRO",
    "EBV",
    "EESV",
    "GIE",
    "SCE",
    "CVSO",
    "NV",
    "SA",
    "BV",
    "CV",
    "SC",
    "SE",
];

fn unaccented_upper(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => {
                'A'
            }
            'ç' | 'Ç' => 'C',
            'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => 'E',
            'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
            'ñ' | 'Ñ' => 'N',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'O',
            'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
            'ý' | 'ÿ' | 'Ý' => 'Y',
            'æ' => 'Æ',
            'œ' => 'Œ',
            other => other.to_ascii_uppercase(),
        })
        .collect()
}

/// Canonical winner-name key: upper, unaccented, consortium tail and slash
/// variants cut, punctuation blanked, single letters dropped, legal-form
/// suffixes stripped.
pub fn normalize_name(value: &str) -> String {
    let upper = unaccented_upper(value);
    // "Ville de Bruxelles - Stad Brussel" and "BESIX Group SA/NV" keep the
    // head variant only.
    let head = upper
        .split(['–', '—'])
        .next()
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default();
    // "Renowi Belgium - Roeselare" (hyphen without surrounding spaces can
    // survive the split above when glued): cut at " - " explicitly.
    let head = match head.find(" - ") {
        Some(index) => &head[..index],
        None => head,
    };
    let mut tokens: Vec<&str> = head
        .split(|c: char| ".,'\"/&+()-:;!? ".contains(c))
        .filter(|token| token.len() > 1 || token.chars().all(|c| c.is_ascii_digit()))
        .collect();
    while tokens
        .last()
        .is_some_and(|last| FORM_SUFFIXES.contains(last))
    {
        tokens.pop();
    }
    tokens.join(" ")
}

/// Split a `---`-joined consortium winner into member names.
pub fn split_members(value: &str) -> Vec<String> {
    value
        .replace('—', "-")
        .split("---")
        .map(str::trim)
        .filter(|member| !member.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn prefer_enterprise(numbers: &mut [String]) {
    numbers.sort_by_key(|number| (!number.starts_with(['0', '1']), number.clone()));
}

/// One resolved history row: a (notice, winner) pair with its join evidence.
#[derive(Debug, Clone)]
pub struct HistoryAward {
    pub year: i64,
    pub notice_url: Option<String>,
    pub buyer_name: Option<String>,
    pub winner_raw: String,
    pub winner_member: Option<String>,
    pub winner_enterprise: Option<String>,
    pub join_confidence: &'static str,
    pub award_value: Option<Decimal>,
    pub cpv: Option<String>,
    pub dt_award: Option<String>,
    pub sme: Option<String>,
    pub number_offers: Option<i64>,
    pub ambiguous: bool,
}

/// Resolve Belgian buyer-side rows against a normalized denomination map
/// (`normalized name -> entity numbers`). Exact raw match outranks normalized
/// whole-name match outranks consortium-member match.
pub fn resolve_rows(
    year: i64,
    rows: Vec<HistoryRow>,
    exact: &HashMap<String, Vec<String>>,
    normalized: &HashMap<String, Vec<String>>,
) -> Vec<HistoryAward> {
    let mut resolved = Vec::new();
    for row in rows {
        let raw = row.winner.trim().to_owned();
        if raw.is_empty() {
            continue;
        }
        let mut award = HistoryAward {
            year,
            notice_url: row.notice_url,
            buyer_name: row.buyer,
            winner_raw: raw.clone(),
            winner_member: None,
            winner_enterprise: None,
            join_confidence: "none",
            award_value: row.value,
            cpv: row.cpv,
            dt_award: row.dt_award,
            sme: row.sme,
            number_offers: row.offers,
            ambiguous: false,
        };
        let upper = raw.to_uppercase();
        if let Some(numbers) = exact.get(&upper) {
            let mut numbers = numbers.clone();
            prefer_enterprise(&mut numbers);
            award.ambiguous = numbers.len() > 1;
            award.winner_enterprise = numbers.into_iter().next();
            award.join_confidence = "exact";
        } else if let Some(numbers) = normalized.get(&normalize_name(&raw)) {
            let mut numbers = numbers.clone();
            prefer_enterprise(&mut numbers);
            award.ambiguous = numbers.len() > 1;
            award.winner_enterprise = numbers.into_iter().next();
            award.join_confidence = "normalized";
        } else {
            for member in split_members(&raw) {
                if let Some(numbers) = normalized.get(&normalize_name(&member)) {
                    let mut numbers = numbers.clone();
                    prefer_enterprise(&mut numbers);
                    award.ambiguous = numbers.len() > 1;
                    award.winner_enterprise = numbers.into_iter().next();
                    award.winner_member = Some(member);
                    award.join_confidence = "member";
                    break;
                }
            }
        }
        resolved.push(award);
    }
    resolved
}

/// One raw Belgian-buyer row from a CAN CSV (all-varchar read).
#[derive(Debug, Clone, Default)]
pub struct HistoryRow {
    pub notice_url: Option<String>,
    pub buyer: Option<String>,
    pub winner: String,
    pub value: Option<Decimal>,
    pub cpv: Option<String>,
    pub dt_award: Option<String>,
    pub sme: Option<String>,
    pub offers: Option<i64>,
}

fn empty_to_none(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value.trim().to_owned())
}

fn parse_decimal(value: String) -> Option<Decimal> {
    let cleaned = value.trim().replace(',', ".");
    (!cleaned.is_empty())
        .then(|| cleaned.parse().ok())
        .flatten()
}

/// TED's "no value reported" sentinel, seen on redacted-winner rows.
/// No Belgian contract is worth ten trillion euro; treat as unknown.
fn parse_award_value(value: String) -> Option<Decimal> {
    const SENTINEL: &str = "9999999999999.99";
    let parsed = parse_decimal(value)?;
    if parsed == SENTINEL.parse().ok()? {
        None
    } else {
        Some(parsed)
    }
}

fn parse_int(value: String) -> Option<i64> {
    (!value.trim().is_empty())
        .then(|| value.trim().parse().ok())
        .flatten()
}

/// Belgian-buyer rows from one CAN export (dirty input: the codebook warns
/// values are unverified contracting-authority output).
pub fn read_belgian_rows(
    connection: &duckdb::Connection,
    csv: &str,
) -> Result<Vec<HistoryRow>, Box<dyn std::error::Error>> {
    let mut statement = connection.prepare(
        "SELECT TED_NOTICE_URL, CAE_NAME, WIN_NAME, AWARD_VALUE_EURO, CPV, \
         DT_AWARD, B_CONTRACTOR_SME, NUMBER_OFFERS \
         FROM read_csv(?, header = true, all_varchar = true) \
         WHERE ISO_COUNTRY_CODE = 'BE'",
    )?;
    let rows = statement.query_map([csv], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    let mut history = Vec::new();
    for row in rows {
        let (url, buyer, winner, value, cpv, dt, sme, offers) = row?;
        let Some(winner) = winner.filter(|name| !name.trim().is_empty()) else {
            continue;
        };
        history.push(HistoryRow {
            notice_url: url.and_then(empty_to_none),
            buyer: buyer.and_then(empty_to_none),
            winner,
            value: value.and_then(parse_award_value),
            cpv: cpv.and_then(empty_to_none),
            dt_award: dt.and_then(empty_to_none),
            sme: sme.and_then(empty_to_none),
            offers: offers.and_then(parse_int),
        });
    }
    Ok(history)
}

/// Exact-upper and normalized denomination keys.
type DenominationMaps = (HashMap<String, Vec<String>>, HashMap<String, Vec<String>>);

/// Normalized denomination map from the KBO snapshot: exact-upper and
/// normalized keys resolve in Rust without one query per winner.
pub fn denomination_maps(
    connection: &duckdb::Connection,
) -> Result<DenominationMaps, Box<dyn std::error::Error>> {
    let mut statement =
        connection.prepare("SELECT DISTINCT Denomination, EntityNumber FROM kbo_denomination")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
        ))
    })?;
    let mut exact: HashMap<String, Vec<String>> = HashMap::new();
    let mut normalized: HashMap<String, Vec<String>> = HashMap::new();
    let mut enterprises: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        let (denomination, entity) = row?;
        let (Some(denomination), Some(entity)) = (denomination, entity) else {
            continue;
        };
        exact
            .entry(denomination.to_uppercase())
            .or_default()
            .push(entity.clone());
        normalized
            .entry(normalize_name(&denomination))
            .or_default()
            .push(entity.clone());
        enterprises.insert(entity);
    }
    for numbers in exact.values_mut().chain(normalized.values_mut()) {
        numbers.sort();
        numbers.dedup();
    }
    let _ = enterprises;
    Ok((exact, normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_vectors_match_the_measured_prototype() {
        for (input, expected) in [
            ("Proximus SA", "PROXIMUS"),
            ("BEDDELEEM NV", "BEDDELEEM"),
            ("SECO SCRL", "SECO"),
            ("Renowi Belgium - Roeselare", "RENOWI BELGIUM"),
            ("BESIX Group SA/NV", "BESIX GROUP"),
            ("Eiffage Énergie Systèmes", "EIFFAGE ENERGIE SYSTEMES"),
            ("Ville de Bruxelles - Stad Brussel", "VILLE DE BRUXELLES"),
            ("Delen Private Bank", "DELEN PRIVATE BANK"),
        ] {
            assert_eq!(normalize_name(input), expected, "input: {input}");
        }
    }

    #[test]
    fn consortium_members_split_on_triple_dash() {
        let members = split_members("AXA BELGIUM nv --- KBC VERZEKERINGEN nv");
        assert_eq!(members.len(), 2);
        assert_eq!(normalize_name(&members[0]), "AXA BELGIUM");
        assert_eq!(normalize_name(&members[1]), "KBC VERZEKERINGEN");
    }

    #[test]
    fn resolution_prefers_exact_then_normalized_then_member() {
        let exact = HashMap::from([("PROXIMUS".to_owned(), vec!["0202.239.951".to_owned()])]);
        let normalized = HashMap::from([
            ("BEDDELEEM".to_owned(), vec!["0400.000.001".to_owned()]),
            ("AXA BELGIUM".to_owned(), vec!["0400.000.002".to_owned()]),
        ]);
        let rows = vec![
            HistoryRow {
                winner: "PROXIMUS".to_owned(),
                ..HistoryRow::default()
            },
            HistoryRow {
                winner: "Beddeleem NV".to_owned(),
                ..HistoryRow::default()
            },
            HistoryRow {
                winner: "AXA BELGIUM nv --- Someone Else".to_owned(),
                ..HistoryRow::default()
            },
            HistoryRow {
                winner: "Unknown Entity XYZ".to_owned(),
                ..HistoryRow::default()
            },
        ];
        let resolved = resolve_rows(2021, rows, &exact, &normalized);
        assert_eq!(resolved[0].join_confidence, "exact");
        assert_eq!(
            resolved[0].winner_enterprise.as_deref(),
            Some("0202.239.951")
        );
        assert_eq!(resolved[1].join_confidence, "normalized");
        assert_eq!(resolved[2].join_confidence, "member");
        assert_eq!(resolved[2].winner_member.as_deref(), Some("AXA BELGIUM nv"));
        assert_eq!(resolved[3].join_confidence, "none");
        assert!(resolved[3].winner_enterprise.is_none());
    }

    #[test]
    fn ten_trillion_sentinel_parses_as_unknown() {
        assert_eq!(parse_award_value("9999999999999.99".to_owned()), None);
        assert_eq!(parse_award_value("9999999999999,99".to_owned()), None);
        assert_eq!(
            parse_award_value("657596280.00".to_owned()),
            Some(Decimal::new(65759628000, 2))
        );
        assert_eq!(parse_award_value("".to_owned()), None);
    }

    #[test]
    fn enterprises_win_over_establishments() {
        let normalized = HashMap::from([(
            "RENEWI BELGIUM".to_owned(),
            vec!["2.035.349.515".to_owned(), "0429.366.144".to_owned()],
        )]);
        let resolved = resolve_rows(
            2021,
            vec![HistoryRow {
                winner: "Renewi Belgium".to_owned(),
                ..HistoryRow::default()
            }],
            &HashMap::new(),
            &normalized,
        );
        assert_eq!(
            resolved[0].winner_enterprise.as_deref(),
            Some("0429.366.144")
        );
        assert!(resolved[0].ambiguous);
    }
}
