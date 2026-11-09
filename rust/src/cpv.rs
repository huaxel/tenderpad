//! CPV + NUTS reference labels from the EU eForms SDK (compile-time data).
//!
//! Codes arrive from TED as opaque numbers (`"72000000"`); this module turns
//! them into human labels without a network call. Data files live in
//! `rust/data/` next to `matcher_tables.json` and are embedded with
//! `include_str!`, so binaries work offline.
//!
//! Provenance: OP-TED eForms-SDK release 1.15.1 (2026-07-20),
//! `codelists/cpv.gc` (CPV 2008, 9,454 codes) and
//! `codelists/nuts_nuts-bel-lvl3.gc` (45 Belgian NUTS regions).
//! Regen: download both `.gc` files, extract `code`, `Name`,
//! `parentCode`, `nld_label`, `fra_label` per row into the same JSON shape.
//! Coverage check 2026-10-23: all 1,396 distinct CPVs in the qc-seed corpus
//! resolve — `cpv_label_nl` returning `None` on a live code means the SDK
//! moved, not that the code is bad.

use std::collections::HashMap;
use std::sync::OnceLock;

// Field names mirror the Genericode column IDs in the vendored files; serde
// renames keep them idiomatic in Rust. The JSON keeps full fidelity
// (incl. fra_label) for future use — structs only take what code reads.
#[derive(Debug, serde::Deserialize)]
struct CpvEntry {
    #[serde(default, rename = "Name")]
    name: Option<String>,
    #[serde(default, rename = "parentCode")]
    parent_code: Option<String>,
    #[serde(default)]
    nld_label: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct NutsEntry {
    #[serde(default, rename = "Name")]
    name: Option<String>,
    #[serde(default)]
    nld_label: Option<String>,
}

fn cpv_table() -> &'static HashMap<String, CpvEntry> {
    static TABLE: OnceLock<HashMap<String, CpvEntry>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("../data/cpv.json")).expect("cpv.json should parse")
    })
}

fn nuts_table() -> &'static HashMap<String, NutsEntry> {
    static TABLE: OnceLock<HashMap<String, NutsEntry>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("../data/nuts-bel.json"))
            .expect("nuts-bel.json should parse")
    })
}

/// Dutch label for a CPV code, falling back to the English name.
/// `None` = code not in the vendored SDK snapshot.
pub fn cpv_label_nl(code: &str) -> Option<&'static str> {
    let entry = cpv_table().get(code)?;
    entry.nld_label.as_deref().or(entry.name.as_deref())
}

/// Whether an 8-digit candidate is a real CPV 2008 code in the vendored
/// snapshot. Free-text extraction (notably the BDA PDF fallback, which takes
/// any 8-digit run) otherwise stores amounts, phone fragments and reference
/// numbers as CPVs, and those phantom codes then drive false CPV–NACE sector
/// matches. The ninth check digit cannot be used instead: per OP-TED the
/// documented formula disagrees with the published codes for the vast
/// majority of the vocabulary, so only table membership is reliable.
/// A code newer than the vendored SDK reads as unknown; that staleness is
/// the documented meaning of `None` from the label lookups, not proof of
/// junk, so callers extracting from untrusted text should keep the rejects
/// auditable rather than silently trusting the filter.
pub fn cpv_is_known(code: &str) -> bool {
    cpv_table().contains_key(code)
}

/// English (source-language) name for a CPV code.
pub fn cpv_name_en(code: &str) -> Option<&'static str> {
    cpv_table().get(code)?.name.as_deref()
}

/// Parent code in the CPV hierarchy (`None` for division tops / unknown).
pub fn cpv_parent(code: &str) -> Option<&'static str> {
    cpv_table().get(code)?.parent_code.as_deref()
}

/// Label for a Belgian NUTS region code (e.g. `BE100`).
/// `None` = not a Belgian NUTS code in the vendored snapshot.
pub fn nuts_be_label(code: &str) -> Option<&'static str> {
    let entry = nuts_table().get(code)?;
    entry.nld_label.as_deref().or(entry.name.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_cpv_codes_resolve_nl_and_en() {
        assert_eq!(
            cpv_label_nl("72000000"),
            Some("IT-diensten: adviezen, softwareontwikkeling, internet en ondersteuning")
        );
        assert_eq!(cpv_label_nl("45000000"), Some("Bouwwerkzaamheden"));
        assert_eq!(cpv_name_en("33600000"), Some("Pharmaceutical products"));
    }

    #[test]
    fn cpv_hierarchy_links_child_to_parent() {
        assert_eq!(cpv_parent("33600000"), Some("33000000"));
    }

    #[test]
    fn known_cpv_membership_separates_real_codes_from_digit_runs() {
        assert!(cpv_is_known("72415000"));
        assert!(cpv_is_known("90500000"));
        // Amounts / reference fragments seen in BDA PDFs, not CPV codes.
        assert!(!cpv_is_known("26272702"));
        assert!(!cpv_is_known("22349611"));
        assert!(!cpv_is_known("27408000"));
    }

    #[test]
    fn unknown_cpv_code_returns_none() {
        assert_eq!(cpv_label_nl("00000000"), None);
        assert_eq!(cpv_name_en("99999999"), None);
    }

    #[test]
    fn nuts_be_regions_resolve() {
        assert!(nuts_be_label("BE100").is_some());
        assert!(nuts_be_label("BE211").is_some());
        // Non-Belgian region is out of scope for the BE-tailored table.
        assert_eq!(nuts_be_label("NL310"), None);
    }

    #[test]
    fn every_cpv_entry_has_an_english_name() {
        let missing = cpv_table().iter().filter(|(_, e)| e.name.is_none()).count();
        assert_eq!(missing, 0);
    }
}
