use crate::{matcher_tables, normalize_apostrophes, parse_money, TenderRequirement};
use regex::Regex;
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct RequirementSignal {
    pub kind: String,
    pub label: String,
    pub values: Vec<Decimal>,
}

fn amount_patterns() -> &'static [Regex; 2] {
    static PATTERNS: OnceLock<[Regex; 2]> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // The character class includes '-' for Belgian "125.000,-" notation.
            Regex::new(r"(?i)(?:€|eur)\s*([\d\s.,-]+)").unwrap(),
            Regex::new(r"(?i)([\d\s.,-]+)\s*(?:€|eur)").unwrap(),
        ]
    })
}

fn amounts(text: &str) -> Vec<Decimal> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for pattern in amount_patterns() {
        for capture in pattern.captures_iter(text) {
            // Sentences put punctuation right after amounts ("EUR 125.000.",
            // "125.000,-"), so strip trailing non-digits before parsing.
            let cleaned =
                capture[1].trim_end_matches(|character: char| !character.is_ascii_digit());
            if let Some(value) = parse_money(cleaned) {
                if seen.insert(value) {
                    result.push(value);
                }
            }
        }
    }
    result
}

pub fn analyze_requirements(requirements: &[TenderRequirement]) -> Vec<RequirementSignal> {
    let mut signals = Vec::new();
    let mut seen = HashSet::new();
    for requirement in requirements {
        let lowered = normalize_apostrophes(&requirement.text).to_lowercase();
        let values = amounts(&requirement.text);
        // First matching rule from matcher_tables.json wins, mirroring the
        // Python reference's elif chain.
        let (kind, label) = matcher_tables()
            .requirement_signals
            .iter()
            .find(|rule| rule.terms.iter().any(|term| lowered.contains(term)))
            .map(|rule| (rule.kind.as_str(), rule.label.as_str()))
            .unwrap_or((&*requirement.kind, "Manual requirement review"));
        let signal = RequirementSignal {
            kind: kind.to_owned(),
            label: label.to_owned(),
            values,
        };
        if seen.insert(signal.clone()) {
            signals.push(signal);
        }
    }
    signals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_many_distinct_requirements_keep_first_seen_order() {
        let mut requirements: Vec<_> = (1..=10_000)
            .map(|value| TenderRequirement {
                text: format!("Minimum omzet EUR {value},00"),
                ..TenderRequirement::default()
            })
            .collect();
        requirements.push(requirements[0].clone());
        let signals = analyze_requirements(&requirements);
        assert_eq!(signals.len(), 10_000);
        for (i, signal) in signals.iter().enumerate() {
            assert_eq!(signal.values, vec![Decimal::from(i + 1)]);
        }
        let same_value = vec![
            TenderRequirement {
                text: "Minimum omzet EUR 10".into(),
                ..TenderRequirement::default()
            },
            TenderRequirement {
                text: "Minimum omzet EUR 10,00".into(),
                ..TenderRequirement::default()
            },
        ];
        assert_eq!(analyze_requirements(&same_value).len(), 1);
    }

    #[test]
    fn extracts_financial_and_credential_signals() {
        let requirements = vec![
            TenderRequirement {
                text: "Minimum omzet van EUR 125.000.".to_owned(),
                ..TenderRequirement::default()
            },
            TenderRequirement {
                text: "Erkenning klasse 5".to_owned(),
                ..TenderRequirement::default()
            },
        ];
        let signals = analyze_requirements(&requirements);
        assert_eq!(signals[0].kind, "financial");
        assert_eq!(signals[0].values, vec![Decimal::new(125_000, 0)]);
        assert_eq!(signals[1].kind, "credential");
    }

    #[test]
    fn extracts_amounts_with_trailing_punctuation() {
        let requirements = vec![
            TenderRequirement {
                text: "Omzet van 3.000,- EUR gevraagd".to_owned(),
                ..TenderRequirement::default()
            },
            TenderRequirement {
                text: "Waarborg van EUR 50.000,50.".to_owned(),
                ..TenderRequirement::default()
            },
        ];
        let signals = analyze_requirements(&requirements);
        assert_eq!(signals[0].values, vec![Decimal::new(3_000, 0)]);
        assert_eq!(signals[1].values, vec![Decimal::new(5_000_050, 2)]);
    }

    #[test]
    fn financial_signal_matches_typographic_apostrophe() {
        let requirements = vec![TenderRequirement {
            text: "Chiffre d\u{2019}affaires minimum exigé".to_owned(),
            ..TenderRequirement::default()
        }];
        let signals = analyze_requirements(&requirements);
        assert_eq!(signals[0].kind, "financial");
    }
}
