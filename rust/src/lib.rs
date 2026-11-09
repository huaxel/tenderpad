use chrono::{Local, NaiveDate};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

pub mod analysis;
pub mod award_xml;
pub mod awards;
pub mod bda;
pub mod bosa;
pub mod bosa_award_history;
pub mod bxl_inventory;
pub mod cpv;
pub mod cross_source;
pub mod eval_contract;
pub mod eval_ranking;
#[cfg(feature = "network")]
mod http_retry;
pub mod kbo;
pub mod kbo_build;
#[cfg(feature = "network")]
pub mod kbo_live;
pub mod nbb;
#[cfg(feature = "network")]
pub mod nbb_api;
pub mod nbb_bundle;
pub mod nbb_evidence;
pub mod nbb_xbrl;
pub mod pras_build;
mod rebuild;
pub mod recognition;
pub mod recognition_catalog;
pub mod recognized_contractors;
pub mod recommendations;
pub mod requirements;
#[cfg(feature = "network")]
pub mod service_calls;
pub mod store;
pub mod ted;
pub mod ted_csv;
pub mod text_tfidf;
pub mod winner_profile;

// Matcher DATA lives in rust/data/matcher_tables.json. Scoring formulas stay in
// code; only the tables below are data-driven.
#[derive(Debug, Deserialize)]
struct WarningTerm {
    needle: String,
    warning: String,
    #[serde(default)]
    word_boundary: bool,
}

#[derive(Debug, Deserialize)]
struct MatcherWeights {
    activity: f64,
    region: f64,
    value: f64,
    history: f64,
    /// Graded feasibility signals. The fit weights above were scaled to make
    /// room, so adding them changed resolution rather than emphasis.
    deadline: f64,
    recognition: f64,
    requirements: f64,
    /// Learned winner-profile affinity. Zero until a calibration run on
    /// held-out pairs earns it weight; zero keeps every total identical.
    #[serde(default)]
    winner_profile: f64,
}

#[derive(Debug, Deserialize)]
struct DeadlineBand {
    max_days: i64,
    score: f64,
}

#[derive(Debug, Deserialize)]
struct DeadlineRunway {
    /// Score when the notice states no deadline at all.
    unknown: f64,
    bands: Vec<DeadlineBand>,
}

#[derive(Debug, Deserialize)]
struct RequirementBand {
    max_count: usize,
    score: f64,
}

#[derive(Debug, Deserialize)]
struct RequirementLoad {
    bands: Vec<RequirementBand>,
}

#[derive(Debug, Deserialize)]
struct RecognitionScores {
    covered: f64,
    undeclared: f64,
    unstated: f64,
    category_mismatch: f64,
    above_declared_class: f64,
}

#[derive(Debug, Deserialize)]
struct ConfidenceConfig {
    base: f64,
    per_reason: f64,
    cap: f64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RecognitionTerms {
    /// Words that introduce a class number in Dutch, French and English notice
    /// text (`klasse 7`, `classe 8`, `categorie 3`).
    pub class: Vec<String>,
    /// Words that mention recognition at all, even without a class.
    pub proof: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RequirementSignalRule {
    pub kind: String,
    pub label: String,
    pub terms: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MatcherTables {
    stopwords: HashSet<String>,
    cpv_to_nace_prefixes: HashMap<String, Vec<String>>,
    warning_terms: Vec<WarningTerm>,
    language_aliases: HashMap<String, String>,
    weights: MatcherWeights,
    language_match_bonus: f64,
    language_mismatch_penalty: f64,
    excluded_term_penalty: f64,
    confidence: ConfidenceConfig,
    previous_award_capacity_factor: String,
    pub(crate) requirement_signals: Vec<RequirementSignalRule>,
    pub(crate) recognition_terms: RecognitionTerms,
    deadline_runway: DeadlineRunway,
    requirement_load: RequirementLoad,
    recognition_scores: RecognitionScores,
    zipcode_provinces: ZipcodeProvinces,
    #[serde(default)]
    cpv_class_to_nace_classes: HashMap<String, Vec<String>>,
    #[serde(default)]
    activity_scores: ActivityScores,
}

/// Activity signal by evidence strength, from `matcher_tables.json`.
#[derive(Debug, Default, Deserialize)]
struct ActivityScores {
    #[serde(default = "default_specific_activity")]
    class_match: f64,
    #[serde(default = "default_division_activity")]
    division_match: f64,
}

fn default_specific_activity() -> f64 {
    1.0
}

fn default_division_activity() -> f64 {
    0.9
}

/// Belgian zipcode ranges mapped to the NUTS 2 province code TED uses in
/// `place-of-performance`. Coarse on purpose: provinces, never municipalities.
#[derive(Debug, Deserialize)]
struct ZipcodeProvinces {
    ranges: Vec<ZipcodeRange>,
}

#[derive(Debug, Deserialize)]
struct ZipcodeRange {
    from: u32,
    to: u32,
    nuts: String,
}

impl ZipcodeProvinces {
    fn province_for(&self, zipcode: &str) -> Option<&str> {
        let digits: String = zipcode
            .chars()
            .filter(|character| character.is_ascii_digit())
            .collect();
        if digits.len() != 4 {
            return None;
        }
        let value: u32 = digits.parse().ok()?;
        self.ranges
            .iter()
            .find(|range| (range.from..=range.to).contains(&value))
            .map(|range| range.nuts.as_str())
    }
}

impl MatcherTables {
    fn capacity_factor(&self) -> Decimal {
        self.previous_award_capacity_factor
            .parse()
            .expect("capacity factor should be a decimal")
    }

    /// Needles backing one warning label, in table order (used to classify
    /// qualification risk from the same terms that produce warnings).
    fn needles_for_warning<'a>(&'a self, warning: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.warning_terms
            .iter()
            .filter(move |term| term.warning == warning)
            .map(|term| term.needle.as_str())
    }
}

pub(crate) fn matcher_tables() -> &'static MatcherTables {
    static TABLES: OnceLock<MatcherTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        serde_json::from_str(include_str!("../data/matcher_tables.json"))
            .expect("matcher_tables.json should parse")
    })
}

pub(crate) fn parse_content_length(value: Option<&str>) -> Option<usize> {
    value.and_then(|value| value.trim().parse::<usize>().ok())
}

/// Map typographic apostrophes to ASCII ones. TED and BDA text regularly
/// contains U+2019 (for example "chiffre d'affaires"), which would otherwise
/// silently miss the ASCII term tables in the matcher and requirement
/// signals.
pub fn is_web_link(value: &str) -> bool {
    // TED's `links` payload is free-form, and a bare `starts_with("http")`
    // also admits `http-evil:` or `http:foo`. `document_urls` reaches API
    // clients and, through `source_url`, the dashboard's `href`.
    value.starts_with("http://") || value.starts_with("https://")
}

pub fn normalize_apostrophes(text: &str) -> String {
    text.replace('\u{2019}', "'")
}

/// Word-boundary match mirroring the Python reference's `\biso\b`, treating
/// alphanumeric characters and underscores as word characters.
fn contains_word(haystack: &str, needle: &str) -> bool {
    let is_word_character = |character: Option<char>| {
        character.is_some_and(|value| value.is_alphanumeric() || value == '_')
    };
    let mut search_from = 0;
    while let Some(found) = haystack[search_from..].find(needle) {
        let start = search_from + found;
        let end = start + needle.len();
        let preceded_by_word =
            start > 0 && is_word_character(haystack[..start].chars().next_back());
        let followed_by_word = is_word_character(haystack[end..].chars().next());
        if !preceded_by_word && !followed_by_word {
            return true;
        }
        search_from = end;
    }
    false
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Activity {
    pub code: String,
    #[serde(default)]
    pub classification: String,
    pub french: Option<String>,
    pub dutch: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Address {
    #[serde(rename = "type", default)]
    pub address_type: String,
    pub zipcode: Option<String>,
    pub municipality_nl: Option<String>,
    pub municipality_fr: Option<String>,
    pub street_nl: Option<String>,
    pub street_fr: Option<String>,
    pub house_number: Option<String>,
    #[serde(rename = "box")]
    pub box_value: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CompanyProfile {
    pub enterprise_number: String,
    pub status: Option<String>,
    #[serde(default)]
    pub juridical_situation: Option<String>,
    #[serde(default)]
    pub juridical_form: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub activities: Vec<Activity>,
    #[serde(default)]
    pub addresses: Vec<Address>,
    #[serde(default)]
    pub establishments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sector_description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary_cpv_prefixes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TenderRequirement {
    pub text: String,
    #[serde(default = "default_requirement_kind")]
    pub kind: String,
    #[serde(default = "default_true")]
    pub mandatory: bool,
    pub source_page: Option<u32>,
}

impl Default for TenderRequirement {
    fn default() -> Self {
        Self {
            text: String::new(),
            kind: default_requirement_kind(),
            mandatory: true,
            source_page: None,
        }
    }
}

fn default_requirement_kind() -> String {
    "other".to_owned()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Tender {
    pub source: String,
    pub external_id: String,
    pub title: String,
    pub buyer: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub cpv_codes: Vec<String>,
    #[serde(default)]
    pub regions: Vec<String>,
    pub estimated_value: Option<Decimal>,
    /// Per-lot values from TED's `-lot` value fields, in notice order. Empty
    /// means one indivisible contract (or an unknown lot structure) and all
    /// size logic falls back to `estimated_value`. Lots have no stable IDs
    /// in the Search API and the parallel arrays are ragged (CPV counts need
    /// not match lot counts), so only values are modelled — never per-lot
    /// CPV or criteria attribution. Measured 2026-10-23: 159/2,025 notices
    /// carry several lot values; lot deadlines never diverge within a notice.
    #[serde(default)]
    pub lot_values: Vec<Decimal>,
    pub deadline: Option<String>,
    /// When TED published this notice. Stored so coverage can be reported as a
    /// date range ("our data covers … to …") rather than only a refresh time.
    pub publication_date: Option<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    /// TED notice type, e.g. `cn-standard` for a call for competition and
    /// `can-standard` for an award result. `None` means unknown (BDA imports
    /// and rows stored before the type was requested), which is treated as an
    /// opportunity so old data is never silently dropped.
    pub notice_type: Option<String>,
    #[serde(default)]
    pub requirements: Vec<TenderRequirement>,
    #[serde(default)]
    pub document_urls: Vec<String>,
    #[serde(default)]
    pub raw: serde_json::Value,
}

/// Source evidence takes precedence over the legacy display-text heuristic.
/// Different explicit scopes can never become versions of each other merely
/// because their title, buyer and CPVs match.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ProcurementKey {
    SourceScope {
        source: String,
        field: &'static str,
        value: String,
    },
    DisplayText {
        source: String,
        buyer: String,
        cpvs: Vec<String>,
        title: String,
    },
}

/// Source-scoped dossier/candidate grouping. A BOSA publication workspace owns
/// its own notice lifecycle; sharing a folder does not establish precedence
/// across workspaces. Manual dossier members use ContractFolderID, then their
/// import container, then row identity. This key is internal, not a stored ID.
pub fn dossier_group_key(tender: &Tender) -> String {
    let scope = tender.source_notice_scope().or_else(|| {
        tender
            .raw
            .get("dossier_container")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(|value| ("dossier_container", value))
    });
    let (field, value) = scope.unwrap_or(("tender", &tender.external_id));
    // The source length makes arbitrary source/ID delimiters unambiguous.
    format!("{field}:{}:{}:{value}", tender.source.len(), tender.source)
}

/// Collapse dossier members to one candidate per group, preserving first-seen
/// order. The survivor prefers a folder-identified row (the structured notice
/// over its file renderings), then the longest description. Members stay in
/// the store for audit; only the ladder slot collapses. Eval labels recorded
/// against collapsed members carry to the survivor via the same key.
pub fn collapse_dossier_members(tenders: Vec<Tender>) -> Vec<Tender> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<Tender>> =
        std::collections::HashMap::new();
    for tender in tenders {
        let key = dossier_group_key(&tender);
        groups.entry(key.clone()).or_default().push(tender);
        if !order.contains(&key) {
            order.push(key);
        }
    }
    let mut collapsed = Vec::new();
    for key in order {
        let members = groups.remove(&key).expect("grouped key should exist");
        collapsed.push(pick_dossier_survivor(members));
    }
    collapsed
}

/// The most informative row wins: a recorded buyer first (zip-member XML
/// rows are `file:`-prefixed too, so the prefix alone cannot tell them from
/// renderings), then non-`file:` identities, then the longest description.
/// Stable order keeps input order on full ties.
fn is_structured_row(tender: &Tender) -> bool {
    !tender.external_id.starts_with("file:")
}

fn survivor_rank(tender: &Tender) -> (u8, u8, std::cmp::Reverse<usize>) {
    (
        u8::from(tender.buyer.as_ref().is_none_or(|buyer| buyer.is_empty())),
        u8::from(!is_structured_row(tender)),
        std::cmp::Reverse(tender.description.len()),
    )
}

/// Index of the survivor by the same rule, for callers that fuse (rather
/// than drop) the rest. Strict improvement keeps input order on ties.
pub fn dossier_survivor_index(members: &[Tender]) -> usize {
    let mut best = 0;
    for (index, tender) in members.iter().enumerate().skip(1) {
        if survivor_rank(tender) < survivor_rank(&members[best]) {
            best = index;
        }
    }
    best
}

fn pick_dossier_survivor(mut members: Vec<Tender>) -> Tender {
    let survivor = dossier_survivor_index(&members);
    members.remove(survivor)
}

/// The source is part of the key on purpose: the same procurement imported from
/// two sources is the separate cross-source duplicate question, which stays open.
/// The survivor is the latest publication, with deadline and identity as
/// deterministic tie-breakers when publication authority is equal or absent.
pub fn collapse_notice_versions(tenders: Vec<Tender>) -> Vec<Tender> {
    let mut best: std::collections::HashMap<ProcurementKey, Tender> =
        std::collections::HashMap::new();
    let mut ungrouped: Vec<Tender> = Vec::new();
    for tender in tenders {
        let Some(key) = tender.version_key() else {
            ungrouped.push(tender);
            continue;
        };
        match best.get(&key) {
            Some(current) if version_rank(&tender) <= version_rank(current) => {}
            _ => {
                best.insert(key, tender);
            }
        }
    }
    let mut collapsed: Vec<Tender> = best.into_values().collect();
    collapsed.extend(ungrouped);
    // Stable order so the caller's deadline ordering survives the collapse.
    collapsed.sort_by(|left, right| {
        left.deadline
            .is_none()
            .cmp(&right.deadline.is_none())
            .then_with(|| left.deadline.cmp(&right.deadline))
            .then_with(|| left.external_id.cmp(&right.external_id))
            .then_with(|| left.source.cmp(&right.source))
    });
    collapsed
}

/// Publication authority precedes deadline: corrections can shorten or remove it.
/// Date ordering does not establish cross-workspace or legal lifecycle precedence.
fn version_rank(tender: &Tender) -> (&str, &str, &str) {
    (
        tender.publication_date.as_deref().unwrap_or(""),
        tender.deadline.as_deref().unwrap_or(""),
        tender.external_id.as_str(),
    )
}

fn normalized_key_text(value: &str) -> String {
    value
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whole euros with thousands separators (`€384,390`): warning text must
/// stay readable across six-figure values without locale machinery.
fn format_money(value: Decimal) -> String {
    let mut whole = value.round().to_string();
    let negative = whole.starts_with('-');
    if negative {
        whole.remove(0);
    }
    let grouped: String = whole
        .chars()
        .rev()
        .collect::<Vec<_>>()
        .chunks(3)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(",")
        .chars()
        .rev()
        .collect();
    format!("{}€{grouped}", if negative { "-" } else { "" })
}

impl Tender {
    /// How many lots the notice offers. One (or zero recorded values) means
    /// the notice is assessed as a single contract at `estimated_value`.
    pub fn lot_count(&self) -> usize {
        self.lot_values.len().max(1)
    }

    pub fn min_lot_value(&self) -> Option<Decimal> {
        self.lot_values.iter().copied().min()
    }

    pub fn max_lot_value(&self) -> Option<Decimal> {
        self.lot_values.iter().copied().max()
    }

    /// The value size logic assesses: the smallest lot when lots exist (a
    /// company bids one lot, not the summed programme), otherwise the
    /// notice total. This is what kills the false too-large on multi-lot
    /// notices such as 654610-2026 (€1.24M total, five €165–384k lots).
    pub fn assessed_value(&self) -> Option<Decimal> {
        self.min_lot_value().or(self.estimated_value)
    }

    /// Lots differ enough that the per-lot check matters: at least two lots
    /// and the largest at least twice the smallest.
    pub fn lots_differ(&self) -> bool {
        match (self.min_lot_value(), self.max_lot_value()) {
            (Some(min), Some(max)) => self.lot_values.len() >= 2 && max >= min * Decimal::new(2, 0),
            _ => false,
        }
    }

    /// Whether this notice asks for bids. Award results, contract-award
    /// modifications, PINs and VEATs are published the same way but cannot be
    /// applied for, so they must not be ranked as opportunities.
    pub fn is_opportunity(&self) -> bool {
        self.notice_type
            .as_deref()
            .map(is_competition_notice_type)
            .unwrap_or(true)
    }

    /// Exact retained source scope, not a title-derived procedure claim.
    fn source_notice_scope(&self) -> Option<(&'static str, &str)> {
        for field in ["bosa_publication_workspace_id", "contract_folder_id"] {
            if let Some(value) = self
                .raw
                .get(field)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
            {
                return Some((field, value));
            }
        }
        None
    }

    /// Prefer a source-issued scope even when buyer/title/classification has
    /// changed. With no explicit identity, retain the existing source-local
    /// display-text heuristic; its lack of lifecycle authority remains a limit.
    pub(crate) fn version_key(&self) -> Option<ProcurementKey> {
        if let Some((field, value)) = self.source_notice_scope() {
            return Some(ProcurementKey::SourceScope {
                source: self.source.clone(),
                field,
                value: value.to_owned(),
            });
        }
        let buyer = normalized_key_text(self.buyer.as_deref()?);
        if buyer.is_empty() {
            return None;
        }
        let mut cpvs = self.cpv_codes.clone();
        cpvs.sort();
        cpvs.dedup();
        Some(ProcurementKey::DisplayText {
            source: self.source.clone(),
            buyer,
            cpvs,
            title: normalized_key_text(&self.title),
        })
    }

    pub fn searchable_text(&self) -> String {
        let mut parts = vec![self.title.clone(), self.description.clone()];
        parts.extend(
            self.requirements
                .iter()
                .map(|requirement| requirement.text.clone()),
        );
        normalize_apostrophes(&parts.join(" "))
    }
}

/// TED notice types a company can answer. `cn*` are calls for competition
/// (`cn-standard`, `cn-social`, `cn-desg`); `pin-cfc-*` is a prior information
/// notice **used as the call for competition**, a real opportunity in the
/// accelerated procedure. Award results (`can*`), award modifications, plain
/// PINs (`pin-only`), VEATs, qualification systems (`qu-sy`) and other
/// second-stage notices are not opportunities. Keep this in step with the SQL
/// filter in `store.rs`.
pub fn is_competition_notice_type(notice_type: &str) -> bool {
    notice_type.starts_with("cn") || notice_type.starts_with("pin-cfc")
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AwardRecord {
    pub publication_number: String,
    #[serde(default)]
    pub title: String,
    pub buyer: Option<String>,
    #[serde(default)]
    pub winner_identifiers: Vec<String>,
    #[serde(default)]
    pub winner_names: Vec<String>,
    #[serde(default)]
    pub cpv_codes: Vec<String>,
    #[serde(default)]
    pub values: Vec<Decimal>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AwardLot {
    pub publication_number: String,
    pub lot_id: String,
    pub winner_identifier: Option<String>,
    pub winner_name: Option<String>,
    pub value: Option<Decimal>,
    #[serde(default)]
    pub lowest_value: Option<Decimal>,
    #[serde(default)]
    pub highest_value: Option<Decimal>,
    #[serde(default)]
    pub cpv_codes: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct CompanyPreferences {
    #[serde(default)]
    pub confirmed_activities: Vec<String>,
    #[serde(default)]
    pub excluded_terms: Vec<String>,
    #[serde(default)]
    pub preferred_regions: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    pub minimum_value: Option<Decimal>,
    pub maximum_value: Option<Decimal>,
    /// Belgian works recognition class (1–8) the company holds, if any. Only
    /// used to compare with what a notice states; never an eligibility verdict.
    pub recognition_class: Option<u8>,
    /// Recognition categories/subcategories the company holds (`C`, `D17`).
    #[serde(default)]
    pub recognition_categories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct MatchResult {
    pub score: f64,
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
}

/// Largest value the store's `DECIMAL(18, 2)` columns accept (16 integer
/// digits). Values above it fail validation with a 422 error.
const MAX_VALUE_LIMIT: Decimal = Decimal::from_parts(0xA763_FFFF, 0x0DE0_B6B3, 0, false, 2);

pub fn validate_value_limits(
    minimum: Option<Decimal>,
    maximum: Option<Decimal>,
) -> Result<(), &'static str> {
    if minimum.is_some_and(|value| value.is_sign_negative())
        || maximum.is_some_and(|value| value.is_sign_negative())
    {
        return Err("value limits must be non-negative");
    }
    if minimum.is_some_and(|value| value > MAX_VALUE_LIMIT)
        || maximum.is_some_and(|value| value > MAX_VALUE_LIMIT)
    {
        return Err("value limits must not exceed 9999999999999999.99");
    }
    if minimum
        .zip(maximum)
        .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        return Err("minimum value cannot exceed maximum value");
    }
    Ok(())
}

pub fn parse_money(value: &str) -> Option<Decimal> {
    // Strip currency markers in any order like the Python reference's
    // `while text[0] in "€eEuUrR"` loop; staged prefix trims would leave
    // residues such as "UE100" behind.
    let value = value
        .trim()
        .replace([' ', '\u{00a0}'], "")
        .trim_start_matches(['€', 'e', 'E', 'u', 'U', 'r', 'R'])
        .to_owned();
    let comma = value.rfind(',');
    let dot = value.rfind('.');
    let normalized = match (comma, dot) {
        (Some(comma), Some(dot)) if comma > dot => value.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => value.replace(',', ""),
        (Some(_), None) => value.replace(',', "."),
        (None, Some(dot))
            if value.matches('.').count() == 1 && value.len().saturating_sub(dot + 1) == 3 =>
        {
            value.replace('.', "")
        }
        (None, Some(_)) if value.matches('.').count() > 1 => {
            let parts = value.split('.').collect::<Vec<_>>();
            if parts.last().is_some_and(|part| part.len() <= 2) {
                format!(
                    "{}.{last}",
                    parts[..parts.len() - 1].join(""),
                    last = parts.last()?
                )
            } else {
                parts.join("")
            }
        }
        _ => value,
    };
    normalized.parse().ok()
}

/// Drop blank entries from parsed code lists. Multilingual TED fields can
/// yield empty strings on language fallback; keeping them lets two records
/// "match" on a shared empty string (e.g. a false historical CPV hit).
/// Pre-existing rows may already contain blanks, so matching also ignores
/// them defensively.
pub fn clean_codes(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn tokens(text: &str) -> HashSet<String> {
    // Underscore counts as a word character to match the Python reference's
    // `[\wÀ-ÿ]{4,}` tokenizer.
    text.to_lowercase()
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| token.chars().count() >= 4 && !matcher_tables().stopwords.contains(*token))
        .map(ToOwned::to_owned)
        .collect()
}

fn cpv_nace_prefixes(cpv_prefix: &str) -> &[String] {
    matcher_tables()
        .cpv_to_nace_prefixes
        .get(cpv_prefix)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// NACE prefixes one CPV code contributes to a reverse-search pool: the
/// exact-class mapping where the class table knows it, else the broader
/// division prefixes — the same evidence ladder `activity_match` climbs.
pub(crate) fn nace_prefixes_for_cpv(cpv: &str) -> Vec<String> {
    let tables = matcher_tables();
    let mut prefixes = Vec::new();
    if let Some(class) = cpv.get(..4) {
        if let Some(naces) = tables.cpv_class_to_nace_classes.get(class) {
            prefixes.extend(naces.iter().cloned());
        }
    }
    if let Some(prefix) = cpv.get(..2) {
        prefixes.extend(cpv_nace_prefixes(prefix).iter().cloned());
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

/// CPV division prefixes associated with a company's registered activities,
/// derived from the official `cpv_to_nace_prefixes` crosswalk table when not
/// explicitly declared on the company profile.
pub fn primary_cpv_prefixes_for_company(company: &CompanyProfile) -> Vec<String> {
    if !company.primary_cpv_prefixes.is_empty() {
        return company.primary_cpv_prefixes.clone();
    }
    let nace_divisions = company
        .activities
        .iter()
        .filter_map(|activity| activity.code.get(..2))
        .collect::<BTreeSet<_>>();
    if nace_divisions.is_empty() {
        return Vec::new();
    }
    let tables = matcher_tables();
    let mut prefixes = Vec::new();
    for (cpv_division, nace_list) in &tables.cpv_to_nace_prefixes {
        if nace_list
            .iter()
            .any(|nace| nace_divisions.contains(nace.as_str()))
        {
            prefixes.push(cpv_division.clone());
        }
    }
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

/// Topical similarity between a company's activity descriptions and the
/// tender text, without the CPV–NACE short-circuits. The forward ranking uses
/// this only when no CPV sector match fires; reverse search additionally uses
/// it as a documented tie-break inside coarse division-match tiers, where the
/// headline score alone ties thousands of companies. Only monotonicity is
/// contractual (higher = more topical): the underlying similarity measure may
/// evolve (e.g. TF-IDF cosine) without breaking tie-break consumers.
pub fn activity_text_score(company: &CompanyProfile, tender: &Tender) -> f64 {
    activity_text_score_with(company, tender, &CompanyPreferences::default())
}

/// Experimental text similarity between a tender title and the company's
/// distinct prior award titles. Kept separate from exact CPV history; this is
/// diagnostic only until evaluated against human relevance labels.
pub fn award_title_history_similarity(history: &[AwardRecord], tender: &Tender) -> f64 {
    if tender.title.trim().is_empty() {
        return 0.0;
    }
    let history_titles = history
        .iter()
        .filter_map(|award| {
            let title = award.title.trim();
            (!title.is_empty()).then(|| title.to_lowercase())
        })
        .collect::<BTreeSet<_>>();
    if history_titles.is_empty() {
        return 0.0;
    }
    let history_text = history_titles.into_iter().collect::<Vec<_>>().join(" ");
    crate::text_tfidf::tfidf_cosine(&history_text, &tender.title, &matcher_tables().stopwords)
}

fn activity_text_inputs(
    company: &CompanyProfile,
    tender: &Tender,
    preferences: &CompanyPreferences,
) -> (String, String) {
    let activity_text = company
        .activities
        .iter()
        .map(|activity| {
            format!(
                "{} {}",
                activity.french.as_deref().unwrap_or_default(),
                activity.dutch.as_deref().unwrap_or_default()
            )
        })
        .chain(preferences.confirmed_activities.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    (activity_text, tender.searchable_text())
}

pub(crate) fn activity_text_score_with(
    company: &CompanyProfile,
    tender: &Tender,
    preferences: &CompanyPreferences,
) -> f64 {
    let (activity_text, tender_text) = activity_text_inputs(company, tender, preferences);
    if activity_text.is_empty() || tokens(&tender_text).is_empty() {
        return 0.0;
    }

    // Corpus-IDF char-ngram cosine: boilerplate overlap weighs nearly
    // nothing next to substance. Empty sides stay 0.0 by construction.
    crate::text_tfidf::tfidf_cosine(&activity_text, &tender_text, &matcher_tables().stopwords)
}

/// Explain why the text diagnostic may be zero or unavailable. `covered`
/// means both inputs have model vocabulary; a zero score can still mean no
/// shared grams, rather than missing text.
pub fn activity_text_coverage_with(
    company: &CompanyProfile,
    tender: &Tender,
    preferences: &CompanyPreferences,
) -> &'static str {
    let (activity_text, tender_text) = activity_text_inputs(company, tender, preferences);
    if activity_text.trim().is_empty() {
        return "no_company_activity_text";
    }
    if tokens(&tender_text).is_empty() {
        return "no_tender_text";
    }
    let stopwords = &matcher_tables().stopwords;
    let company_covered = crate::text_tfidf::has_in_vocabulary_ngrams(&activity_text, stopwords);
    let tender_covered = crate::text_tfidf::has_in_vocabulary_ngrams(&tender_text, stopwords);
    match (company_covered, tender_covered) {
        (true, true) => "covered",
        (false, true) => "company_text_out_of_vocabulary",
        (true, false) => "tender_text_out_of_vocabulary",
        (false, false) => "both_texts_out_of_vocabulary",
    }
}

fn activity_match(
    company: &CompanyProfile,
    tender: &Tender,
    preferences: &CompanyPreferences,
) -> f64 {
    let text_score = activity_text_score_with(company, tender, preferences);

    let nace_divisions = company
        .activities
        .iter()
        .filter_map(|activity| activity.code.get(..2))
        .collect::<HashSet<_>>();
    let nace_classes = company
        .activities
        .iter()
        .filter_map(|activity| activity.code.get(..4))
        .collect::<HashSet<_>>();
    let tables = matcher_tables();
    // A CPV class matched to a registered NACE class is specific evidence; a
    // division-level match is real but coarser, so the two must not tie.
    let class_match = tender
        .cpv_codes
        .iter()
        .filter_map(|cpv| cpv.get(..4))
        .any(|class| {
            tables
                .cpv_class_to_nace_classes
                .get(class)
                .is_some_and(|naces| {
                    naces
                        .iter()
                        .any(|nace| nace_classes.contains(nace.as_str()))
                })
        });
    if class_match {
        return tables.activity_scores.class_match.min(1.0);
    }
    let division_match = tender.cpv_codes.iter().any(|cpv| {
        cpv.get(..2).is_some_and(|prefix| {
            cpv_nace_prefixes(prefix)
                .iter()
                .any(|nace| nace_divisions.contains(nace.as_str()))
        })
    });
    if division_match {
        return tables.activity_scores.division_match.min(1.0);
    }
    text_score
}

/// Place-of-performance values TED uses when the buyer means "anywhere" rather
/// than a place: `anyw-eea` (anywhere in the EEA) and the country-level
/// `BEZZZ`. A Belgian company is not geographically excluded by those, so they
/// must not score like a remote municipality — measured 2026-10-19: without
/// this, 37 notices in one window were scored as if their location were
/// unsuitable.
fn is_anywhere_region(region: &str) -> bool {
    let region = region.trim().to_lowercase();
    region.starts_with("anyw") || region == "bezzz"
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct GeographicMatchEvidence {
    /// The existing region signal used by the ranker; diagnostic consumers do
    /// not add or reweight it.
    pub signal: f64,
    /// Stable explanation code without exposing company address or preference text.
    pub status: &'static str,
}

fn region_match(
    company: &CompanyProfile,
    tender: &Tender,
    preferences: &CompanyPreferences,
) -> f64 {
    geographic_match_evidence(company, tender, preferences).signal
}

/// Explain the existing geographic match and its coverage without exposing
/// addresses or changing the scoring formula.
pub fn geographic_match_evidence(
    company: &CompanyProfile,
    tender: &Tender,
    preferences: &CompanyPreferences,
) -> GeographicMatchEvidence {
    let locations: Vec<&String> = tender
        .regions
        .iter()
        .filter(|region| !is_anywhere_region(region))
        .collect();
    if locations.is_empty() {
        // No place stated, or "anywhere": unknown geography is neutral, but
        // the explanation preserves the difference between absence and scope.
        return GeographicMatchEvidence {
            signal: 0.5,
            status: if tender.regions.is_empty() {
                "no_tender_location"
            } else {
                "unrestricted_tender_location"
            },
        };
    }
    let searchable = tender.searchable_text().to_lowercase();
    if !preferences.preferred_regions.is_empty() {
        let matches = preferences.preferred_regions.iter().any(|preferred| {
            let preferred = preferred.trim().to_lowercase();
            !preferred.is_empty()
                && (locations
                    .iter()
                    .any(|region| region.trim().to_lowercase() == preferred)
                    || searchable.contains(&preferred))
        });
        return GeographicMatchEvidence {
            signal: if matches { 1.0 } else { 0.0 },
            status: if matches {
                "preferred_region_match"
            } else {
                "preferred_region_no_match"
            },
        };
    }

    let company_text = company
        .addresses
        .iter()
        .flat_map(|address| {
            [
                address.zipcode.as_deref(),
                address.municipality_nl.as_deref(),
                address.municipality_fr.as_deref(),
            ]
        })
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let tables = matcher_tables();
    let provinces = company
        .addresses
        .iter()
        .filter_map(|address| address.zipcode.as_deref())
        .filter_map(|zipcode| tables.zipcode_provinces.province_for(zipcode))
        .collect::<Vec<_>>();
    // TED states place-of-performance as NUTS codes (`BE224`), so comparing them
    // with a town name or a zipcode can never succeed: measured 2026-10-19, 2,845
    // of 4,922 region tokens in one window were NUTS codes, the name-shaped ones
    // were foreign country codes, and a company with a real address scored 0.0 on
    // every notice. Without this, the region term could only ever fire for a
    // company that spelled out preferred regions.
    let mut parent_level_only = false;
    for region in &locations {
        let code = region.trim().to_uppercase();
        if provinces.iter().any(|province| code.starts_with(province)) {
            return GeographicMatchEvidence {
                signal: 1.0,
                status: "company_province_match",
            };
        }
        if provinces
            .iter()
            .any(|province| province.starts_with(code.as_str()))
        {
            // A coarser level than the province (`BE2` for all of Flanders) does
            // not say the notice is near the company, only that it is not
            // excluded. Unknown, like `anywhere`, must not read as a match.
            parent_level_only = true;
        }
        if company_text.contains(&region.to_lowercase()) {
            return GeographicMatchEvidence {
                signal: 1.0,
                status: "company_address_text_match",
            };
        }
    }
    if parent_level_only {
        GeographicMatchEvidence {
            signal: 0.5,
            status: "coarse_region_neutral",
        }
    } else if provinces.is_empty() {
        GeographicMatchEvidence {
            signal: 0.5,
            status: "company_region_unavailable",
        }
    } else {
        GeographicMatchEvidence {
            signal: 0.0,
            status: "no_geographic_match",
        }
    }
}

fn normalized_language(value: &str) -> String {
    let normalized = value.trim().to_lowercase();
    matcher_tables()
        .language_aliases
        .get(&normalized)
        .cloned()
        .unwrap_or(normalized)
}

fn language_match(tender: &Tender, preferences: &CompanyPreferences) -> Option<bool> {
    if preferences.languages.is_empty() || tender.languages.is_empty() {
        return None;
    }
    let preferred = preferences
        .languages
        .iter()
        .map(|language| normalized_language(language))
        .filter(|language| !language.is_empty())
        .collect::<HashSet<_>>();
    let offered = tender
        .languages
        .iter()
        .map(|language| normalized_language(language))
        .filter(|language| !language.is_empty())
        .collect::<HashSet<_>>();
    if preferred.is_empty() || offered.is_empty() {
        return None;
    }
    Some(preferred.intersection(&offered).next().is_some())
}

/// Compare without overflowing the finite Decimal representation. The
/// capacity multiplier is positive: overflow means the threshold is beyond
/// the representable upper/lower bound, so the comparison is still known.
fn above_award_capacity(value: Decimal, previous_max: Decimal) -> bool {
    previous_max
        .checked_mul(matcher_tables().capacity_factor())
        .map_or_else(
            || previous_max.is_sign_negative(),
            |threshold| value > threshold,
        )
}

fn value_fit(tender: &Tender, preferences: &CompanyPreferences, history: &[AwardRecord]) -> f64 {
    // Assessed at the smallest lot when lots exist: a company bids one lot,
    // not the summed programme. Single-contract notices fall back to the
    // notice total, exactly as before.
    let Some(value) = tender.assessed_value() else {
        return 0.5;
    };
    if preferences
        .minimum_value
        .is_some_and(|minimum| value < minimum)
        || preferences
            .maximum_value
            .is_some_and(|maximum| value > maximum)
    {
        return 0.0;
    }
    if preferences.minimum_value.is_some() || preferences.maximum_value.is_some() {
        return 1.0;
    }
    let previous_values = history
        .iter()
        .flat_map(|award| award.values.iter().copied())
        .collect::<Vec<_>>();
    let Some(previous_max) = previous_values.iter().copied().max() else {
        return 0.5;
    };
    if !above_award_capacity(value, previous_max) {
        1.0
    } else {
        0.25
    }
}

/// The component scores that make up a `MatchResult`, exposed so callers can
/// build structured decisions without parsing human-readable reason strings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchSignals {
    pub activity: f64,
    pub region: f64,
    pub value_fit: f64,
    pub history_match: f64,
    /// How much preparation time is left, graded in bands (see
    /// `deadline_runway` in `matcher_tables.json`).
    pub deadline_runway: f64,
    /// Whether the stated recognition requirement fits what the company
    /// declared. `0.5` means "not enough information", which is not a penalty.
    pub recognition_fit: f64,
    /// Manual qualification work implied by the extracted requirements.
    pub requirement_load: f64,
    /// Learned NACE×CPV affinity of past winners (0.5 = no information).
    /// Zero-weighted until calibration; see `winner_profile`.
    pub winner_profile: f64,
}

pub fn match_signals(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
) -> MatchSignals {
    match_signals_on(
        company,
        tender,
        history,
        preferences,
        Local::now().date_naive(),
    )
}

/// As `match_signals`, with an explicit "today" so evaluations are reproducible.
pub fn match_signals_on(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
    today: NaiveDate,
) -> MatchSignals {
    let previous_cpvs = history
        .iter()
        .flat_map(|award| award.cpv_codes.iter().cloned())
        .filter(|cpv| !cpv.trim().is_empty())
        .collect::<HashSet<_>>();
    let history_match = if history.is_empty() {
        0.5
    } else if tender
        .cpv_codes
        .iter()
        .filter(|cpv| !cpv.trim().is_empty())
        .any(|cpv| previous_cpvs.contains(cpv))
    {
        1.0
    } else {
        0.0
    };
    MatchSignals {
        activity: activity_match(company, tender, preferences),
        region: region_match(company, tender, preferences),
        value_fit: value_fit(tender, preferences, history),
        history_match,
        deadline_runway: deadline_runway(tender, today),
        recognition_fit: recognition_fit_component(tender, preferences),
        requirement_load: requirement_load(tender),
        winner_profile: crate::winner_profile::winner_profile_fit(company, tender),
    }
}

/// How much preparation time is left, as a graded band. A notice with no
/// deadline scores `unknown` rather than zero: not knowing is not a fault.
pub fn deadline_runway(tender: &Tender, today: NaiveDate) -> f64 {
    let table = &matcher_tables().deadline_runway;
    let Some(days) = days_until_deadline(tender, today) else {
        return table.unknown;
    };
    for band in &table.bands {
        if days <= band.max_days {
            return band.score;
        }
    }
    table
        .bands
        .last()
        .map(|band| band.score)
        .unwrap_or(table.unknown)
}

/// Days left before the submission deadline, when the notice states one.
pub fn days_until_deadline(tender: &Tender, today: NaiveDate) -> Option<i64> {
    tender
        .deadline
        .as_deref()
        .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
        .map(|deadline| (deadline - today).num_days())
}

/// Feasibility of the recognition gate as a graded score. Notices that state
/// nothing, and companies that declare nothing, both score the neutral `0.5`:
/// the mission forbids turning missing information into a verdict.
pub fn recognition_fit_component(tender: &Tender, preferences: &CompanyPreferences) -> f64 {
    use crate::recognition::{recognition_fit, recognition_statement, RecognitionFit};
    let scores = &matcher_tables().recognition_scores;
    let statement = recognition_statement(tender);
    if statement.is_empty() {
        return scores.unstated;
    }
    match recognition_fit(&statement, preferences) {
        RecognitionFit::Covered => scores.covered,
        RecognitionFit::Undeclared => scores.undeclared,
        RecognitionFit::Unstated => scores.unstated,
        RecognitionFit::CategoryMismatch { .. } => scores.category_mismatch,
        RecognitionFit::CategoryUndeclared { .. } => scores.undeclared,
        RecognitionFit::AboveDeclaredClass { .. } => scores.above_declared_class,
    }
}

/// Manual qualification work, graded by how many mandatory requirements were
/// extracted. Requirement text is stored verbatim, so this is a hint about
/// effort, not a judgement about the contract.
pub fn requirement_load(tender: &Tender) -> f64 {
    let table = &matcher_tables().requirement_load;
    let mandatory = tender
        .requirements
        .iter()
        .filter(|requirement| requirement.mandatory)
        .count();
    for band in &table.bands {
        if mandatory <= band.max_count {
            return band.score;
        }
    }
    table.bands.last().map(|band| band.score).unwrap_or(1.0)
}

/// The dominant qualification risk for a tender, classified from the same
/// term tables that produce warnings. "certification" mirrors the
/// certificate/certification warning terms (a bare "iso " hit only produces
/// the lowercase ISO warning); "capacity" mirrors the turnover terms and the
/// above-previous-award-range warning.
pub fn qualification_risk(tender: &Tender, history: &[AwardRecord]) -> &'static str {
    let tables = matcher_tables();
    let lowered = tender.searchable_text().to_lowercase();
    if tables
        .needles_for_warning("Certification or certificate requirement")
        .any(|needle| lowered.contains(needle))
    {
        return "certification";
    }
    let above_previous_range = tender.assessed_value().is_some_and(|value| {
        history
            .iter()
            .flat_map(|award| award.values.iter().copied())
            .max()
            .is_some_and(|previous_max| above_award_capacity(value, previous_max))
    });
    let turnover_required = tables
        .needles_for_warning("Minimum turnover requirement")
        .collect::<Vec<_>>()
        .into_iter()
        .any(|term| lowered.contains(term));
    if above_previous_range || turnover_required {
        return "capacity";
    }
    "none_obvious"
}

/// Reassurance wording that never invents a class: a company may declare
/// categories without a class, and "within your declared class 0" is nonsense.
fn recognition_covered_reason(
    statement: &crate::recognition::RecognitionStatement,
    preferences: &CompanyPreferences,
) -> String {
    match preferences.recognition_class {
        Some(class) => format!(
            "Recognition requirement is within your declared class {class}{}",
            recognition_suffix(statement)
        ),
        None => format!(
            "Recognition category matches the one you declared{}",
            recognition_suffix(statement)
        ),
    }
}

/// ` (C)` / ` (C, D17)` for recognition messages, or an empty string when the
/// notice states no category.
fn recognition_suffix(statement: &crate::recognition::RecognitionStatement) -> String {
    if statement.categories.is_empty() {
        String::new()
    } else {
        format!(" ({})", statement.categories.join(", "))
    }
}

/// One weighted term of the score: the graded signal, the weight it carries and
/// the points it contributed. Weights come from `matcher_tables.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoreTerm {
    pub signal: f64,
    pub weight: f64,
    pub points: f64,
}

/// Why a notice scored what it scored. The weighted terms plus the two
/// adjustments that are not part of the sum (`language`, `excluded_term`) add up
/// to `total`, so a score can be explained rather than guessed at, and the
/// explanation cannot drift from the score because both are computed here once.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoreBreakdown {
    pub activity: ScoreTerm,
    /// Explain-only char-ngram similarity between company activity text and
    /// tender text. It is intentionally not part of `total`; activity scoring
    /// still prefers mapped CPV–NACE evidence when available.
    pub activity_text_similarity: f64,
    pub region: ScoreTerm,
    pub value: ScoreTerm,
    pub history: ScoreTerm,
    pub deadline: ScoreTerm,
    pub recognition: ScoreTerm,
    pub requirements: ScoreTerm,
    pub winner_profile: ScoreTerm,
    /// `Some(true)` when the notice's language is one the company listed.
    pub language: Option<bool>,
    pub language_points: f64,
    /// Negative when the notice contains a term the company excluded.
    pub excluded_term_points: f64,
    /// The score before the single-decimal rounding the API reports.
    pub total: f64,
}

fn score_term(signal: f64, weight: f64) -> ScoreTerm {
    ScoreTerm {
        signal,
        weight,
        points: signal * weight * 100.0,
    }
}

/// Computes the score and its decomposition together, so `--explain` and the
/// ranking can never disagree about a number.
pub fn score_breakdown(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
) -> ScoreBreakdown {
    score_breakdown_on(
        company,
        tender,
        history,
        preferences,
        Local::now().date_naive(),
    )
}

pub fn score_breakdown_on(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
    today: NaiveDate,
) -> ScoreBreakdown {
    let signals = match_signals_on(company, tender, history, preferences, today);
    let tables = matcher_tables();
    let weights = &tables.weights;
    let activity = score_term(signals.activity, weights.activity);
    let region = score_term(signals.region, weights.region);
    let value = score_term(signals.value_fit, weights.value);
    let history_term = score_term(signals.history_match, weights.history);
    let deadline = score_term(signals.deadline_runway, weights.deadline);
    let recognition = score_term(signals.recognition_fit, weights.recognition);
    let requirements = score_term(signals.requirement_load, weights.requirements);
    let winner_profile = score_term(signals.winner_profile, weights.winner_profile);
    let mut score = (activity.points
        + region.points
        + value.points
        + history_term.points
        + deadline.points
        + recognition.points
        + requirements.points
        + winner_profile.points)
        .min(100.0);
    let language = language_match(tender, preferences);
    let mut language_points = 0.0;
    if language == Some(true) {
        score = (score + tables.language_match_bonus).min(100.0);
        language_points = tables.language_match_bonus;
    } else if language == Some(false) {
        let before = score;
        score = (score - tables.language_mismatch_penalty).max(0.0);
        language_points = score - before;
    }
    let lowered = tender.searchable_text().to_lowercase();
    let mut excluded_term_points = 0.0;
    if preferences
        .excluded_terms
        .iter()
        .any(|term| !term.trim().is_empty() && lowered.contains(&term.to_lowercase()))
    {
        let before = score;
        score = (score - tables.excluded_term_penalty).max(0.0);
        excluded_term_points = score - before;
    }
    ScoreBreakdown {
        activity,
        activity_text_similarity: activity_text_score_with(company, tender, preferences),
        region,
        value,
        history: history_term,
        deadline,
        recognition,
        requirements,
        winner_profile,
        language,
        language_points,
        excluded_term_points,
        total: score,
    }
}

pub fn match_company_to_tender(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
) -> MatchResult {
    match_company_to_tender_on(
        company,
        tender,
        history,
        preferences,
        Local::now().date_naive(),
    )
}

pub fn match_company_to_tender_on(
    company: &CompanyProfile,
    tender: &Tender,
    history: &[AwardRecord],
    preferences: &CompanyPreferences,
    today: NaiveDate,
) -> MatchResult {
    let signals = match_signals_on(company, tender, history, preferences, today);
    let activity = signals.activity;
    let region = signals.region;
    let history_match = signals.history_match;
    let deadline_runway = signals.deadline_runway;

    let tables = matcher_tables();
    // Fit and feasibility together: the three graded signals are what separate
    // notices that the coarse fit components put in one tie.
    let score = score_breakdown_on(company, tender, history, preferences, today).total;
    let language_match = language_match(tender, preferences);
    let mut reasons = Vec::new();
    if language_match == Some(true) {
        reasons.push("Tender language matches preferred languages".to_owned());
    }
    if activity >= 0.5 {
        reasons.push("Tender activity matches registered company activities".to_owned());
    }
    if region == 1.0 {
        reasons.push("Tender location matches a company municipality".to_owned());
    }
    if tender.estimated_value.is_some() {
        reasons.push("Contract value is available for capacity review".to_owned());
    }
    // Only the reassuring half belongs among the reasons; the cautions are
    // added below, where the warning list exists.
    let days_left = days_until_deadline(tender, today);
    if let Some(days) = days_left {
        if deadline_runway >= 0.75 && days >= 0 {
            reasons.push(format!("Deadline leaves {days} days to prepare"));
        }
    }

    let previous_values = history
        .iter()
        .flat_map(|award| award.values.iter().copied())
        .collect::<Vec<_>>();
    if let (Some(value), Some(previous_max)) = (
        tender.assessed_value(),
        previous_values.iter().copied().max(),
    ) {
        if !above_award_capacity(value, previous_max) {
            reasons.push("Contract value is within the previous award range".to_owned());
        }
    }
    if history_match == 1.0 {
        reasons.push("CPV category matches a previous awarded contract".to_owned());
    }

    let lowered = tender.searchable_text().to_lowercase();
    // The "iso" entry is matched on word boundaries so sentence-final
    // "ISO." still fires.
    let mut warnings = tables
        .warning_terms
        .iter()
        .filter(|term| {
            if term.word_boundary {
                contains_word(&lowered, &term.needle)
            } else {
                lowered.contains(&term.needle)
            }
        })
        .map(|term| term.warning.clone())
        .collect::<Vec<_>>();
    if language_match == Some(false) {
        warnings.push("Tender language does not match preferred languages".to_owned());
    }

    if let (Some(value), Some(previous_max)) = (
        tender.assessed_value(),
        previous_values.iter().copied().max(),
    ) {
        if above_award_capacity(value, previous_max) {
            warnings.push("Contract value is above the previous award range".to_owned());
        }
    }
    // Lots differ in size: the assessed smallest lot may fit while the
    // notice total misleads, so the check-per-lot note is load-bearing.
    if tender.lots_differ() {
        if let (Some(min), Some(max)) = (tender.min_lot_value(), tender.max_lot_value()) {
            warnings.push(format!(
                "Offered as {} lots ({}–{}): assess per lot, the total is the sum, not the entry ticket",
                tender.lot_count(),
                format_money(min),
                format_money(max),
            ));
        }
    }
    if preferences
        .excluded_terms
        .iter()
        .any(|term| !term.trim().is_empty() && lowered.contains(&term.to_lowercase()))
    {
        warnings.push("Contains a user-excluded term".to_owned());
    }

    if let Some(days) = days_left {
        if days < 0 {
            // Already closed: the ranking excludes these, and a negative
            // "days to prepare" sentence would be nonsense anywhere else.
            warnings.push("Submission deadline has passed".to_owned());
        } else if deadline_runway <= 0.5 {
            warnings.push(format!("Only {days} days left before the deadline"));
        }
    }

    // Recognition ("erkenning"/"agréation") is a feasibility gate for works, and
    // Belgian notices state the required class and category. Report how the
    // stated requirement relates to what the company declared; never decide
    // eligibility, and stay silent when the notice states nothing.
    let statement = crate::recognition::recognition_statement(tender);
    if !statement.is_empty() {
        match crate::recognition::recognition_fit(&statement, preferences) {
            crate::recognition::RecognitionFit::AboveDeclaredClass { required, declared } => {
                let mut warning = format!(
                    "States recognition class {required}{}; above your declared class {declared}",
                    recognition_suffix(&statement)
                );
                if statement.classes.len() > 1 {
                    warning.push_str("; several lots state different classes (check per lot)");
                }
                warnings.push(warning);
            }
            crate::recognition::RecognitionFit::CategoryMismatch { required, declared } => {
                warnings.push(format!(
                    "States recognition categor{} {}; you declared {}",
                    if required.len() > 1 { "ies" } else { "y" },
                    required.join(", "),
                    declared.join(", ")
                ));
            }
            crate::recognition::RecognitionFit::CategoryUndeclared { required } => {
                warnings.push(format!(
                    "Notice states recognition categor{} {}; you have not declared categories — add \
                     them to have this checked",
                    if required.len() > 1 { "ies" } else { "y" },
                    required.join(", ")
                ));
            }
            crate::recognition::RecognitionFit::Unstated => warnings.push(
                "Requires recognition, but the notice states no class or category — check the tender \
                 documents"
                    .to_owned(),
            ),
            crate::recognition::RecognitionFit::Covered => {
                reasons.push(recognition_covered_reason(&statement, preferences))
            }
            crate::recognition::RecognitionFit::Undeclared => {}
        }
    }

    // Text mentioning both "certificate" and "certification" must not repeat
    // the same requirement warning.
    let mut seen = HashSet::new();
    warnings.retain(|warning| seen.insert(warning.clone()));

    // Confidence counts *positive evidence*, not reasons. It used to be
    // `base + per_reason * reasons.len()`, which meant two notices with the same
    // score and the same evidence differed by a tenth of the range because one
    // happened to print "Contract value is available for capacity review" — a
    // statement about our data, not about the match. Measured 2026-10-19 on a
    // 25-item ladder: confidence was 0.65-0.85 and followed the bullet count.
    let evidence = [
        signals.activity >= 0.5,
        signals.region == 1.0,
        signals.history_match == 1.0,
        signals.recognition_fit >= 1.0,
        signals.value_fit == 1.0,
    ]
    .into_iter()
    .filter(|present| *present)
    .count();

    MatchResult {
        // round_ties_even matches the Python reference's banker's rounding
        // (round(..., 1)); plain round() rounds half away and diverges on
        // exact-half scores such as 61.25.
        score: (score * 10.0).round_ties_even() / 10.0,
        // Round like the Python reference (round(..., 2)) so serialized
        // confidence matches exactly instead of exposing float artifacts.
        confidence: ((tables.confidence.base + tables.confidence.per_reason * evidence as f64)
            .min(tables.confidence.cap)
            * 100.0)
            .round_ties_even()
            / 100.0,
        reasons,
        warnings,
    }
}

#[derive(Debug, Deserialize)]
pub struct MatchRequest {
    pub company: CompanyProfile,
    pub tender: Tender,
    #[serde(default)]
    pub history: Vec<AwardRecord>,
    #[serde(default)]
    pub preferences: CompanyPreferences,
}

pub fn match_request(request: MatchRequest) -> MatchResult {
    match_company_to_tender(
        &request.company,
        &request.tender,
        &request.history,
        &request.preferences,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_today_makes_score_independent_of_wall_clock() {
        let tender = Tender {
            deadline: Some("2026-11-20".to_owned()),
            ..Tender::default()
        };
        let prefs = CompanyPreferences::default();
        let at = |y, m, d| {
            score_breakdown_on(
                &company(),
                &tender,
                &[],
                &prefs,
                NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            )
            .total
        };
        // 20 days left scores in a different runway band than 3 days left.
        assert!(at(2026, 10, 31) > at(2026, 11, 17));
        assert_eq!(at(2026, 10, 31), at(2026, 10, 31));
        let via = match_company_to_tender_on(
            &company(),
            &tender,
            &[],
            &prefs,
            NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(),
        );
        assert!((via.score - at(2026, 10, 31)).abs() <= 0.05 + 1e-9);
    }

    use super::*;

    /// The breakdown is the score: every weighted term plus the adjustments must
    /// add up, and the total must equal what the ranking reports. Without this a
    /// `--explain` view could quietly disagree with the number it explains.
    #[test]
    fn score_breakdown_adds_up_to_the_reported_score() {
        let tender = Tender {
            source: "ted".to_owned(),
            external_id: "1-2026".to_owned(),
            title: "Elektrische installatie van een school".to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            regions: vec!["Gent".to_owned()],
            estimated_value: Some(Decimal::new(250_000, 0)),
            deadline: Some(
                (chrono::Local::now().date_naive() + chrono::Duration::days(60)).to_string(),
            ),
            languages: vec!["nl".to_owned()],
            ..Tender::default()
        };
        let preferences = CompanyPreferences {
            confirmed_activities: vec!["43210".to_owned()],
            preferred_regions: vec!["Gent".to_owned()],
            languages: vec!["nl".to_owned()],
            ..CompanyPreferences::default()
        };
        let company = company();
        let breakdown = score_breakdown(&company, &tender, &[], &preferences);
        let summed = breakdown.activity.points
            + breakdown.region.points
            + breakdown.value.points
            + breakdown.history.points
            + breakdown.deadline.points
            + breakdown.recognition.points
            + breakdown.requirements.points
            + breakdown.winner_profile.points
            + breakdown.language_points
            + breakdown.excluded_term_points;
        assert!(
            (summed - breakdown.total).abs() < 1e-9,
            "terms {summed} should equal the total {}",
            breakdown.total
        );
        let reported = match_company_to_tender(&company, &tender, &[], &preferences);
        assert_eq!(
            reported.score,
            (breakdown.total * 10.0).round_ties_even() / 10.0
        );
        // A matching activity and region must actually earn their weight.
        assert!(breakdown.activity.points > 0.0);
        assert_eq!(breakdown.region.signal, 1.0);
        assert_eq!(breakdown.language, Some(true));
    }

    #[test]
    fn award_title_history_similarity_prefers_related_contract_titles() {
        let history = vec![AwardRecord {
            title: "Development and maintenance of enterprise planning software".to_owned(),
            ..AwardRecord::default()
        }];
        let related = Tender {
            title: "Enterprise planning software maintenance".to_owned(),
            ..Tender::default()
        };
        let unrelated = Tender {
            title: "Supply of classroom furniture and office chairs".to_owned(),
            ..Tender::default()
        };
        let related_score = award_title_history_similarity(&history, &related);
        let unrelated_score = award_title_history_similarity(&history, &unrelated);
        assert!(related_score > unrelated_score);
        assert!(related_score > 0.0);
        assert_eq!(award_title_history_similarity(&[], &related), 0.0);
    }

    /// Generic business words must not earn activity credit. Measured
    /// 2026-10-19: a pharmacist (NACE 47730, "Detailhandel in farmaceutische
    /// producten") scored 0.14 activity on a food-supply notice because both
    /// texts contain "produits"/"producten"; that is not evidence of sector fit.
    /// A republished call with an extended deadline is one ladder entry, and the
    /// survivor is the version that is still biddable.
    /// A CPV class matched to a registered NACE class is specific evidence; a
    /// division-level match is real but coarser and must score lower, so two
    /// notices in the same sector stop tying.
    /// Confidence must follow evidence, not the number of printed bullets: a
    /// deadline or a value-availability note is not proof of a match.
    #[test]
    fn confidence_follows_evidence_not_bullet_count() {
        let base = Tender {
            source: "ted".to_owned(),
            external_id: "1".to_owned(),
            title: "Elektrische installatie".to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            regions: vec!["Gent".to_owned()],
            ..Tender::default()
        };
        let bare = match_company_to_tender(&company(), &base, &[], &CompanyPreferences::default());
        // Same notice, plus a deadline runway bullet that carries no evidence.
        let with_bullet = Tender {
            deadline: Some(
                (chrono::Local::now().date_naive() + chrono::Duration::days(60)).to_string(),
            ),
            ..base.clone()
        };
        let louder = match_company_to_tender(
            &company(),
            &with_bullet,
            &[],
            &CompanyPreferences::default(),
        );
        assert!(
            louder.reasons.len() > bare.reasons.len(),
            "the deadline must add a reason"
        );
        assert_eq!(
            bare.confidence, louder.confidence,
            "a non-evidence reason must not raise confidence"
        );
        // Real evidence does: a previous award in the same CPV category.
        let history = vec![AwardRecord {
            publication_number: "award".to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            values: vec![Decimal::new(60_000, 0)],
            ..AwardRecord::default()
        }];
        let evidenced = match_company_to_tender(
            &company(),
            &with_bullet,
            &history,
            &CompanyPreferences::default(),
        );
        assert!(
            evidenced.confidence > louder.confidence,
            "a previous award in the same category is evidence: {} vs {}",
            evidenced.confidence,
            louder.confidence
        );
    }

    #[test]
    fn activity_is_graded_by_how_specific_the_match_is() {
        let mut company = company();
        company.activities = vec![Activity {
            code: "46460".to_owned(),
            classification: "MAIN".to_owned(),
            french: Some("Commerce de gros de produits pharmaceutiques".to_owned()),
            dutch: Some("Groothandel in farmaceutische producten".to_owned()),
        }];
        let tender_for = |external_id: &str, cpv: &str| Tender {
            source: "ted".to_owned(),
            external_id: external_id.to_owned(),
            title: "Fourniture de produits pharmaceutiques".to_owned(),
            cpv_codes: vec![cpv.to_owned()],
            ..Tender::default()
        };
        // 3310 is mapped to NACE class 4646: the company's own class.
        let specific = match_signals(
            &company,
            &tender_for("specific", "33100000"),
            &[],
            &CompanyPreferences::default(),
        );
        // 3399 shares only the division 33 -> 46 mapping.
        let coarse = match_signals(
            &company,
            &tender_for("coarse", "33990000"),
            &[],
            &CompanyPreferences::default(),
        );
        assert_eq!(specific.activity, 1.0, "class match should be full credit");
        assert_eq!(coarse.activity, 0.9, "division match should be weaker");
    }

    #[test]
    fn collapsing_versions_keeps_the_operative_notice() {
        let version =
            |external_id: &str, deadline: &str, published: &str, cpv: &str, buyer: &str| Tender {
                source: "ted".to_owned(),
                external_id: external_id.to_owned(),
                title: "BOUWEN VAN EEN KANTOOR- EN LABOGEOUW".to_owned(),
                buyer: Some(buyer.to_owned()),
                cpv_codes: vec![cpv.to_owned()],
                deadline: Some(deadline.to_owned()),
                publication_date: Some(published.to_owned()),
                notice_type: Some("cn-standard".to_owned()),
                ..Tender::default()
            };
        let original = version("1-2024", "2024-09-16", "2024-08-01", "45210000", "imec vzw");
        let extended = version("2-2024", "2024-10-16", "2024-09-01", "45210000", "imec vzw");
        let republication = version("3-2024", "2024-10-16", "2024-09-20", "45210000", "imec vzw");
        let other_lot = version("4-2024", "2024-10-16", "2024-09-01", "45300000", "imec vzw");
        let other_buyer = version(
            "5-2024",
            "2024-10-16",
            "2024-09-01",
            "45210000",
            "KU Leuven",
        );
        let collapsed = collapse_notice_versions(vec![
            original,
            extended,
            republication,
            other_lot,
            other_buyer,
        ]);
        let mut ids = collapsed
            .iter()
            .map(|tender| tender.external_id.as_str())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec!["3-2024", "4-2024", "5-2024"],
            "one survivor per procurement, keeping the latest publication"
        );
    }

    #[test]
    fn published_version_authority_is_independent_of_deadline_and_input_order() {
        for deadline in [None, Some("2026-11-01"), Some("2026-12-20")] {
            let old = Tender {
                source: "ted".into(),
                external_id: "old".into(),
                title: "Synthetic procurement".into(),
                buyer: Some("Buyer".into()),
                deadline: Some("2026-11-30".into()),
                publication_date: Some("2026-10-01".into()),
                ..Tender::default()
            };
            let new = Tender {
                external_id: "new".into(),
                deadline: deadline.map(str::to_owned),
                publication_date: Some("2026-11-01".into()),
                ..old.clone()
            };
            for input in [
                vec![old.clone(), new.clone()],
                vec![new.clone(), old.clone()],
            ] {
                let selected = collapse_notice_versions(input);
                assert_eq!(selected.len(), 1);
                assert_eq!(selected[0].external_id, "new");
                assert_eq!(collapse_notice_versions(selected)[0].external_id, "new");
            }
        }
    }

    #[test]
    fn source_scopes_cannot_be_collapsed_by_display_text_or_shared_local_ids() {
        for field in [
            "contract_folder_id",
            "dossier_container",
            "bosa_publication_workspace_id",
        ] {
            let row = |source: &str, id: &str, identity: &str| Tender {
                source: source.into(),
                external_id: id.into(),
                title: "Same visible title".into(),
                buyer: Some("Same buyer".into()),
                cpv_codes: vec!["33600000".into()],
                raw: serde_json::json!({field: identity}),
                ..Tender::default()
            };
            let first = row("bda", "first", "shared");
            let other_source = row("bda-int", "second", "shared");
            for input in [
                vec![first.clone(), other_source.clone()],
                vec![other_source.clone(), first.clone()],
            ] {
                assert_eq!(
                    collapse_dossier_members(input.clone()).len(),
                    2,
                    "cross-source {field}"
                );
                assert_eq!(collapse_notice_versions(input).len(), 2);
            }
            // Archive/container names alone are not version authority. This
            // assertion concerns explicit source folder/workspace identities.
            if field != "dossier_container" {
                let different = row("bda", "third", "different");
                assert_eq!(
                    collapse_notice_versions(vec![first.clone(), different.clone()]).len(),
                    2,
                    "different {field} must not be overwritten by a title match"
                );
                assert_eq!(collapse_dossier_members(vec![first, different]).len(), 2);
            }
        }
    }

    #[test]
    fn source_scope_selection_survives_display_edits_and_has_unambiguous_keys() {
        for field in ["contract_folder_id", "bosa_publication_workspace_id"] {
            let old = Tender {
                source: "bda".into(),
                external_id: "old".into(),
                title: "Original title".into(),
                buyer: Some("Original buyer".into()),
                publication_date: Some("2026-10-01".into()),
                raw: serde_json::json!({field:"scope"}),
                ..Tender::default()
            };
            let new = Tender {
                external_id: "new".into(),
                title: "Corrected title".into(),
                buyer: None,
                cpv_codes: vec!["33600000".into()],
                publication_date: Some("2026-11-01".into()),
                ..old.clone()
            };
            for rows in [
                vec![old.clone(), new.clone()],
                vec![new.clone(), old.clone()],
            ] {
                let selected = collapse_notice_versions(rows);
                assert_eq!(selected.len(), 1);
                assert_eq!(selected[0].external_id, "new");
            }
        }
        let left = Tender {
            source: "a:b".into(),
            external_id: "c".into(),
            ..Tender::default()
        };
        let right = Tender {
            source: "a".into(),
            external_id: "b:c".into(),
            ..Tender::default()
        };
        assert_ne!(dossier_group_key(&left), dossier_group_key(&right));
    }

    #[test]
    fn dossier_group_key_prefers_folder_over_container() {
        let tender = Tender {
            source: "bda".to_owned(),
            external_id: "file:notice.pdf".to_owned(),
            raw: serde_json::json!({
                "contract_folder_id": "FOLDER-9",
                "dossier_container": "dossier-1",
            }),
            ..Tender::default()
        };
        assert_eq!(
            dossier_group_key(&tender),
            "contract_folder_id:3:bda:FOLDER-9"
        );
    }

    #[test]
    fn dossier_collapse_prefers_folder_row_and_keeps_order() {
        // File rows arrive with the folder id already propagated at ingest;
        // collapse only groups by the shared key.
        let file = |id: &str, description: &str| Tender {
            source: "bda".to_owned(),
            external_id: id.to_owned(),
            title: "Bestek".to_owned(),
            description: description.to_owned(),
            raw: serde_json::json!({
                "contract_folder_id": "FOLDER-9",
                "dossier_container": "dossier-1",
            }),
            ..Tender::default()
        };
        let notice = Tender {
            source: "bda".to_owned(),
            external_id: "folder-9".to_owned(),
            title: "Bestek".to_owned(),
            buyer: Some("KU Leuven".to_owned()),
            raw: serde_json::json!({
                "contract_folder_id": "FOLDER-9",
                "dossier_container": "dossier-1",
            }),
            ..Tender::default()
        };
        let other = Tender {
            source: "bda".to_owned(),
            external_id: "other".to_owned(),
            raw: serde_json::json!({"dossier_container": "dossier-2"}),
            ..Tender::default()
        };
        let collapsed = collapse_dossier_members(vec![
            file("file:a.pdf", "a long bestek description with substance"),
            notice,
            file("file:b.pdf", "x"),
            other,
        ]);
        assert_eq!(collapsed.len(), 2, "six files, two procurements");
        assert_eq!(
            collapsed[0].external_id, "folder-9",
            "the structured notice wins over its renderings"
        );
        assert_eq!(collapsed[1].external_id, "other");
    }

    #[test]
    fn buyer_beats_longer_text_in_survivor_pick() {
        // Zip-member XML rows are `file:`-prefixed too: only the recorded
        // buyer tells them apart from renderings (Agence FAVV case).
        let rendering = Tender {
            source: "bda".to_owned(),
            external_id: "file:notice.pdf".to_owned(),
            description: "a very long rendered notice text with substance".to_owned(),
            raw: serde_json::json!({"dossier_container": "dossier-1"}),
            ..Tender::default()
        };
        let notice = Tender {
            source: "bda".to_owned(),
            external_id: "file:notice.xml".to_owned(),
            buyer: Some("Agence Fédérale".to_owned()),
            description: "short".to_owned(),
            raw: serde_json::json!({"dossier_container": "dossier-1"}),
            ..Tender::default()
        };
        let collapsed = collapse_dossier_members(vec![rendering, notice]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].external_id, "file:notice.xml");
    }

    #[test]
    fn same_container_different_folders_stay_split() {
        let row = |id: &str, folder: &str| Tender {
            source: "bda".to_owned(),
            external_id: id.to_owned(),
            raw: serde_json::json!({
                "contract_folder_id": folder,
                "dossier_container": "shared-download",
            }),
            ..Tender::default()
        };
        let collapsed =
            collapse_dossier_members(vec![row("first", "FOLDER-A"), row("second", "FOLDER-B")]);
        assert_eq!(
            collapsed.len(),
            2,
            "one download may bundle several dossiers"
        );
    }

    #[test]
    fn generic_business_words_do_not_earn_activity_credit() {
        let company = CompanyProfile {
            enterprise_number: "0536.191.947".to_owned(),
            names: vec!["APOTHEEK AN LEYMAN".to_owned()],
            activities: vec![Activity {
                code: "47730".to_owned(),
                classification: "MAIN".to_owned(),
                french: Some("Commerce de détail de produits pharmaceutiques".to_owned()),
                dutch: Some("Detailhandel in farmaceutische producten".to_owned()),
            }],
            ..Default::default()
        };
        let products_only = Tender {
            source: "ted".to_owned(),
            external_id: "food".to_owned(),
            title: "Levering van producten voor de keuken".to_owned(),
            description:
                "De opdracht betreft de levering van producten en de diensten rond de levering."
                    .to_owned(),
            ..Tender::default()
        };
        let signals = match_signals(
            &company,
            &products_only,
            &[],
            &CompanyPreferences::default(),
        );
        assert_eq!(
            signals.activity, 0.0,
            "a shared generic word must not be sector evidence"
        );
        // The qualifying word still counts: a pharmaceutical notice still matches.
        let pharmaceutical = Tender {
            source: "ted".to_owned(),
            external_id: "pharma".to_owned(),
            title: "Levering van farmaceutische producten".to_owned(),
            ..Tender::default()
        };
        let signals = match_signals(
            &company,
            &pharmaceutical,
            &[],
            &CompanyPreferences::default(),
        );
        assert!(
            signals.activity > 0.0,
            "the sector word must still earn credit, got {}",
            signals.activity
        );
    }

    #[test]
    fn embedded_matcher_tables_match_reference_values() {
        // Pins the Rust-owned matcher tables to observed scoring behavior.
        let tables = matcher_tables();
        assert_eq!(tables.stopwords.len(), 99);
        for word in ["van", "and", "pour", "andere"] {
            assert!(tables.stopwords.contains(word), "missing stopword {word}");
        }
        // Generic business vocabulary and contract boilerplate, derived from the
        // two corpora: a shared "produits" or "services" is not sector evidence.
        for word in [
            "produits",
            "producten",
            "activiteiten",
            "services",
            "belgium",
        ] {
            assert!(tables.stopwords.contains(word), "missing stopword {word}");
        }
        // Function words, not sector evidence. Measured 2026-10-23: on the
        // Etterbeek hosting dossier a supermarket outscored a hosting company
        // on shared "niet"/"waarbij" alone.
        for word in ["niet", "waarbij", "worden", "zonder", "sont", "cette"] {
            assert!(tables.stopwords.contains(word), "missing stopword {word}");
        }
        // CPV 45 covers construction plus the divisions the award data supports
        // (real-estate developers, metal structures, and after two years of
        // awards, metal fabrication; wave 2 adds repair/installation,
        // fabricated metal and membership organisations from 2017-2021
        // history validated on temporal halves).
        let nace_45 = tables
            .cpv_to_nace_prefixes
            .get("45")
            .expect("CPV 45 mapping");
        assert_eq!(
            nace_45.iter().map(String::as_str).collect::<Vec<_>>(),
            ["25", "28", "33", "41", "42", "43", "68", "91"]
        );
        // Pairs derived from Belgian award outcomes, kept only when the held-out
        // half reproduced them: office support supplying business services, and
        // wholesalers supplying medical goods.
        assert!(tables
            .cpv_to_nace_prefixes
            .get("79")
            .is_some_and(|divs| divs.iter().any(|d| d == "82")));
        assert!(tables
            .cpv_to_nace_prefixes
            .get("33")
            .is_some_and(|divs| divs.iter().any(|d| d == "46")));
        assert_eq!(tables.warning_terms.len(), 8);
        assert!(tables
            .warning_terms
            .iter()
            .any(|term| term.needle == "iso" && term.word_boundary));
        assert_eq!(
            tables.language_aliases.get("nld").map(String::as_str),
            Some("nl")
        );
        let weights_sum = tables.weights.activity
            + tables.weights.region
            + tables.weights.value
            + tables.weights.history
            + tables.weights.deadline
            + tables.weights.recognition
            + tables.weights.requirements
            + tables.weights.winner_profile;
        assert!((weights_sum - 1.0).abs() < 1e-12);
        // The graded bands are looked up in order, so their ceilings must
        // increase strictly or a band would be unreachable.
        for pair in tables.deadline_runway.bands.windows(2) {
            assert!(
                pair[0].max_days < pair[1].max_days,
                "deadline bands must increase: {} then {}",
                pair[0].max_days,
                pair[1].max_days
            );
        }
        for pair in tables.requirement_load.bands.windows(2) {
            assert!(
                pair[0].max_count < pair[1].max_count,
                "requirement bands must increase: {} then {}",
                pair[0].max_count,
                pair[1].max_count
            );
        }
        assert!(tables
            .deadline_runway
            .bands
            .iter()
            .any(|band| band.score == 1.0));
        assert!(tables
            .requirement_load
            .bands
            .iter()
            .any(|band| band.score == 1.0));
        assert_eq!(tables.capacity_factor(), Decimal::new(15, 1));
        assert_eq!(tables.requirement_signals.len(), 4);
        assert_eq!(tables.requirement_signals[0].kind, "financial");
    }

    #[test]
    fn parses_content_length_with_optional_whitespace() {
        assert_eq!(parse_content_length(Some(" 123 ")), Some(123));
        assert_eq!(parse_content_length(Some("not-a-number")), None);
        assert_eq!(parse_content_length(None), None);
    }

    #[test]
    fn validates_value_limits() {
        assert!(
            validate_value_limits(Some(Decimal::new(10, 0)), Some(Decimal::new(20, 0))).is_ok()
        );
        assert!(validate_value_limits(Some(Decimal::new(-1, 0)), None).is_err());
        assert!(
            validate_value_limits(Some(Decimal::new(20, 0)), Some(Decimal::new(10, 0))).is_err()
        );
    }

    #[test]
    fn rejects_values_beyond_the_decimal_column() {
        let over = Decimal::new(10_000_000_000_000_000, 0);
        assert!(validate_value_limits(Some(over), None).is_err());
        assert!(validate_value_limits(None, Some(over)).is_err());
        // The const is the DECIMAL(18, 2) maximum and is still accepted.
        assert_eq!(MAX_VALUE_LIMIT, Decimal::new(999_999_999_999_999_999, 2));
        assert!(validate_value_limits(Some(MAX_VALUE_LIMIT), Some(MAX_VALUE_LIMIT)).is_ok());
    }

    #[test]
    fn parses_common_money_formats_exactly() {
        assert_eq!(parse_money("1.234,56"), Some(Decimal::new(123_456, 2)));
        assert_eq!(parse_money("125.000"), Some(Decimal::new(125_000, 0)));
        assert_eq!(parse_money("125000.50"), Some(Decimal::new(12_500_050, 2)));
    }

    #[test]
    fn strips_currency_markers_in_any_order_like_the_python_reference() {
        assert_eq!(parse_money("EUR 100"), Some(Decimal::new(100, 0)));
        // Odd but contractual: the reference strips any leading run of
        // currency-marker letters, so this parses rather than failing.
        assert_eq!(parse_money("RUE100"), Some(Decimal::new(100, 0)));
    }

    fn company() -> CompanyProfile {
        CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: Some("AC".to_owned()),
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec!["Example Installations".to_owned()],
            activities: vec![Activity {
                code: "43210".to_owned(),
                classification: "MAIN".to_owned(),
                french: Some("Installation électrique".to_owned()),
                dutch: Some("Elektrische installatie".to_owned()),
            }],
            addresses: vec![Address {
                address_type: "REGO".to_owned(),
                zipcode: Some("9000".to_owned()),
                municipality_nl: Some("Gent".to_owned()),
                municipality_fr: Some("Gand".to_owned()),
                ..Address::default()
            }],
            establishments: vec![],
            ..Default::default()
        }
    }

    /// Measured 2026-10-23: "niet"/"waarbij" alone carried a supermarket
    /// past a hosting company on the Etterbeek dossier. Function words must
    /// never count as activity overlap.
    #[test]
    fn tokens_drop_dutch_and_french_function_words() {
        let kept = tokens("niet waarbij worden zonder sont cette hosting website");
        assert_eq!(
            kept,
            ["hosting", "website"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    fn activity_company(code: &str, french: &str, dutch: &str) -> CompanyProfile {
        CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec![],
            activities: vec![Activity {
                code: code.to_owned(),
                classification: "MAIN".to_owned(),
                french: Some(french.to_owned()),
                dutch: Some(dutch.to_owned()),
            }],
            addresses: vec![],
            establishments: vec![],
            ..Default::default()
        }
    }

    /// The tie-break reverse search uses inside division-match tiers must see
    /// real topical overlap, not boilerplate: a hosting description outscores
    /// an unrelated one on a hosting tender even when both share the tier.
    #[test]
    fn activity_text_score_separates_topical_from_generic_overlap() {
        let tender = Tender {
            title: "Hosting en onderhoud gemeentelijke website".to_owned(),
            description: "De opdracht omvat hosting, onderhoud en ondersteuning van de website.\
                De inschrijver toont financiële draagkracht en verzekeringen."
                .to_owned(),
            ..Tender::default()
        };
        let hosting = activity_company(
            "63100",
            "hébergement informatique",
            "hosting van websites en onderhoud",
        );
        let retail = activity_company(
            "47110",
            "commerce de détail alimentaire",
            "detailhandel in voedingsmiddelen",
        );
        let hosting_score = activity_text_score(&hosting, &tender);
        let retail_score = activity_text_score(&retail, &tender);
        assert!(
            hosting_score > retail_score,
            "hosting {hosting_score} should beat retail {retail_score}"
        );
        assert!(
            retail_score < 0.5,
            "generic overlap stays low: {retail_score}"
        );
    }

    #[test]
    fn tokens_keep_underscores_like_the_python_reference() {
        assert!(tokens("co_operatie together")
            .iter()
            .any(|token| token == "co_operatie"));
    }

    #[test]
    fn confidence_is_rounded_like_the_python_reference() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "1".to_owned(),
            title: "Elektrische installatie in Gent".to_owned(),
            regions: vec!["Gent".to_owned()],
            estimated_value: Some(Decimal::new(65_000, 0)),
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert_eq!(result.reasons.len(), 3);
        // Activity (partial text) and region are the two pieces of positive
        // evidence here: 0.45 + 2 x 0.1.
        assert_eq!(result.confidence, 0.65);
    }

    #[test]
    fn language_preferences_change_score() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "language".to_owned(),
            title: "Electrical installation".to_owned(),
            languages: vec!["nld".to_owned()],
            ..Tender::default()
        };
        let matching = match_company_to_tender(
            &company(),
            &tender,
            &[],
            &CompanyPreferences {
                languages: vec!["nl".to_owned()],
                ..CompanyPreferences::default()
            },
        );
        let mismatching = match_company_to_tender(
            &company(),
            &tender,
            &[],
            &CompanyPreferences {
                languages: vec!["fr".to_owned()],
                ..CompanyPreferences::default()
            },
        );
        assert!(matching.score > mismatching.score);
        assert!(matching
            .reasons
            .contains(&"Tender language matches preferred languages".to_owned()));
        assert!(mismatching
            .warnings
            .contains(&"Tender language does not match preferred languages".to_owned()));
    }

    #[test]
    fn blank_excluded_terms_are_ignored() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "blank-excluded".to_owned(),
            title: "Elektrische installatie in Gent".to_owned(),
            ..Tender::default()
        };
        let clean =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        let blanked = match_company_to_tender(
            &company(),
            &tender,
            &[],
            &CompanyPreferences {
                excluded_terms: vec![String::new(), "   ".to_owned()],
                ..CompanyPreferences::default()
            },
        );
        assert_eq!(blanked.score, clean.score);
        assert_eq!(blanked.warnings, clean.warnings);
        let flagged = match_company_to_tender(
            &company(),
            &tender,
            &[],
            &CompanyPreferences {
                excluded_terms: vec!["gent".to_owned()],
                ..CompanyPreferences::default()
            },
        );
        assert!(flagged.score < clean.score);
        assert!(flagged
            .warnings
            .contains(&"Contains a user-excluded term".to_owned()));
    }

    #[test]
    fn qualification_risk_classifies_from_the_same_terms_as_warnings() {
        let certified = Tender {
            source: "test".to_owned(),
            external_id: "1".to_owned(),
            title: "ISO 9001 certificate required".to_owned(),
            ..Tender::default()
        };
        assert_eq!(qualification_risk(&certified, &[]), "certification");
        let iso_only = Tender {
            source: "test".to_owned(),
            external_id: "2".to_owned(),
            title: "ISO 9001 vereist".to_owned(),
            ..Tender::default()
        };
        assert_eq!(qualification_risk(&iso_only, &[]), "none_obvious");
        let above_range = Tender {
            source: "test".to_owned(),
            external_id: "3".to_owned(),
            title: "Electrical installation".to_owned(),
            estimated_value: Some(Decimal::new(200_000, 0)),
            ..Tender::default()
        };
        let history = vec![AwardRecord {
            publication_number: "award".to_owned(),
            values: vec![Decimal::new(50_000, 0)],
            ..AwardRecord::default()
        }];
        assert_eq!(qualification_risk(&above_range, &history), "capacity");
        let turnover = Tender {
            source: "test".to_owned(),
            external_id: "4".to_owned(),
            title: "Vereist een minimum omzet".to_owned(),
            ..Tender::default()
        };
        assert_eq!(qualification_risk(&turnover, &[]), "capacity");
    }

    #[test]
    fn activity_and_region_match() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "1".to_owned(),
            title: "Elektrische installatie in Gent".to_owned(),
            regions: vec!["Gent".to_owned()],
            estimated_value: Some(Decimal::new(65_000, 0)),
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(result.score > 50.0);
        assert!(!result.reasons.is_empty());
        let signals = match_signals(&company(), &tender, &[], &CompanyPreferences::default());
        // No CPV, so the corpus-IDF text fallback decides: an electrical
        // installer against "Elektrische installatie" scores 0.766, not the
        // old count-ratio 0.5 — shared substance outweighs shared boilerplate.
        assert!((signals.activity - 0.7661).abs() < 0.001);
        assert_eq!(signals.region, 1.0);
        assert_eq!(
            geographic_match_evidence(&company(), &tender, &CompanyPreferences::default()),
            GeographicMatchEvidence {
                signal: 1.0,
                status: "company_address_text_match",
            }
        );
        assert_eq!(signals.value_fit, 0.5);
        assert_eq!(signals.history_match, 0.5);
    }

    #[test]
    fn geographic_evidence_distinguishes_preference_miss_from_missing_coverage() {
        let tender = Tender {
            regions: vec!["Gent".to_owned()],
            ..Tender::default()
        };
        let miss = geographic_match_evidence(
            &company(),
            &tender,
            &CompanyPreferences {
                preferred_regions: vec!["Liège".to_owned()],
                ..CompanyPreferences::default()
            },
        );
        assert_eq!(miss.signal, 0.0);
        assert_eq!(miss.status, "preferred_region_no_match");

        let mut without_address = company();
        without_address.addresses.clear();
        let unresolved =
            geographic_match_evidence(&without_address, &tender, &CompanyPreferences::default());
        assert_eq!(unresolved.signal, 0.5);
        assert_eq!(unresolved.status, "company_region_unavailable");

        let everywhere = Tender {
            regions: vec!["anyw-eea".to_owned()],
            ..Tender::default()
        };
        let no_location =
            geographic_match_evidence(&company(), &everywhere, &CompanyPreferences::default());
        assert_eq!(no_location.signal, 0.5);
        assert_eq!(no_location.status, "unrestricted_tender_location");

        let missing_location = geographic_match_evidence(
            &company(),
            &Tender::default(),
            &CompanyPreferences::default(),
        );
        assert_eq!(missing_location.signal, 0.5);
        assert_eq!(missing_location.status, "no_tender_location");
    }

    #[test]
    fn blank_cpv_codes_do_not_count_as_history_match() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "blank".to_owned(),
            title: "Works".to_owned(),
            cpv_codes: vec![String::new(), "   ".to_owned()],
            ..Tender::default()
        };
        let history = vec![AwardRecord {
            publication_number: "award".to_owned(),
            cpv_codes: vec![String::new()],
            ..AwardRecord::default()
        }];
        let signals = match_signals(
            &company(),
            &tender,
            &history,
            &CompanyPreferences::default(),
        );
        assert_eq!(signals.history_match, 0.0);
        assert_eq!(
            clean_codes(vec![String::new(), "45".to_owned()]),
            vec!["45"]
        );
    }

    #[test]
    fn audit_extreme_award_values_do_not_panic_scoring() {
        for maximum in [Decimal::MAX, Decimal::MIN] {
            let tender = Tender {
                estimated_value: Some(Decimal::ONE),
                ..Tender::default()
            };
            let history = vec![AwardRecord {
                values: vec![maximum],
                ..AwardRecord::default()
            }];
            let result = match_company_to_tender(
                &company(),
                &tender,
                &history,
                &CompanyPreferences::default(),
            );
            assert!(result.score.is_finite());
            let risk = qualification_risk(&tender, &history);
            assert_eq!(risk == "capacity", maximum.is_sign_negative());
        }
    }

    /// Measured 2026-10-23 on TED 654610-2026: €1,238,890 total over five
    /// €165–384k lots. Size logic must assess the smallest lot, not the sum.
    fn multi_lot_tender() -> Tender {
        Tender {
            source: "test".to_owned(),
            external_id: "lots".to_owned(),
            title: "Soil analyses".to_owned(),
            estimated_value: Some(Decimal::new(1_238_890, 0)),
            lot_values: vec![
                Decimal::new(384_390, 0),
                Decimal::new(164_580, 0),
                Decimal::new(181_400, 0),
                Decimal::new(200_600, 0),
                Decimal::new(307_920, 0),
            ],
            ..Tender::default()
        }
    }

    #[test]
    fn multi_lot_assesses_smallest_lot_against_preferences() {
        let tender = multi_lot_tender();
        assert_eq!(tender.assessed_value(), Some(Decimal::new(164_580, 0)));
        // The €1.24M total exceeds a €500k ceiling; the €165k smallest lot
        // does not — the notice stays biddable instead of scoring 0.0.
        let preferences = CompanyPreferences {
            maximum_value: Some(Decimal::new(500_000, 0)),
            ..CompanyPreferences::default()
        };
        let signals = match_signals(&company(), &tender, &[], &preferences);
        assert_eq!(signals.value_fit, 1.0);
    }

    #[test]
    fn multi_lot_has_no_false_above_range_warning() {
        let tender = multi_lot_tender();
        // Previous best €300k × 1.5 = €450k: above the total, below nothing
        // biddable — the warning must stay silent on the assessed €165k.
        let history = vec![AwardRecord {
            publication_number: "award".to_owned(),
            values: vec![Decimal::new(300_000, 0)],
            ..AwardRecord::default()
        }];
        let result = match_company_to_tender(
            &company(),
            &tender,
            &history,
            &CompanyPreferences::default(),
        );
        assert!(
            result
                .reasons
                .iter()
                .any(|reason| reason == "Contract value is within the previous award range"),
            "assessed lot should read as within range: {:?}",
            result.reasons
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| warning == "Contract value is above the previous award range"),
            "total must not trigger the capacity warning: {:?}",
            result.warnings
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("5 lots") && warning.contains("€164,580–€384,390")),
            "differing lots must carry the per-lot note: {:?}",
            result.warnings
        );
    }

    #[test]
    fn single_contract_assessment_is_unchanged() {
        let tender = Tender {
            estimated_value: Some(Decimal::new(200_000, 0)),
            ..Tender::default()
        };
        assert_eq!(tender.assessed_value(), Some(Decimal::new(200_000, 0)));
        assert!(!tender.lots_differ());
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(!result
            .warnings
            .iter()
            .any(|warning| warning.contains("lots")));
    }

    fn activity_for_company_cpv(nace: &str, cpv: &str) -> f64 {
        let company = CompanyProfile {
            enterprise_number: "test".to_owned(),
            status: None,
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: Vec::new(),
            activities: vec![Activity {
                code: nace.to_owned(),
                classification: "MAIN".to_owned(),
                // Labels keep the text path non-empty; class/division
                // matching reads codes only.
                french: Some("activité".to_owned()),
                dutch: Some("activiteit".to_owned()),
            }],
            addresses: Vec::new(),
            establishments: Vec::new(),
            ..Default::default()
        };
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "t".to_owned(),
            // Title keeps tender tokens non-empty; matching reads codes.
            title: "procurement".to_owned(),
            cpv_codes: vec![cpv.to_owned()],
            ..Tender::default()
        };
        activity_match(&company, &tender, &CompanyPreferences::default())
    }

    #[test]
    fn wave_two_class_pair_fires_class_match() {
        // Taxi NACE 4932 winning special-transport CPV 6013: support 72/40,
        // lift 51/80 across temporal halves.
        assert_eq!(activity_for_company_cpv("49321", "60130000"), 1.0);
    }

    #[test]
    fn wave_two_division_pair_fires_division_match() {
        // R&D NACE 72 winning environmental CPV 90: support 28/25.
        assert_eq!(activity_for_company_cpv("72100", "90000000"), 0.9);
    }

    #[test]
    fn money_formats_whole_grouped_euros() {
        assert_eq!(format_money(Decimal::new(384_390, 0)), "€384,390");
        assert_eq!(format_money(Decimal::new(1_238_890, 0)), "€1,238,890");
        assert_eq!(format_money(Decimal::new(900, 0)), "€900");
    }

    #[test]
    fn historical_capacity_warning() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "large".to_owned(),
            title: "Electrical installation".to_owned(),
            estimated_value: Some(Decimal::new(200_000, 0)),
            ..Tender::default()
        };
        let history = vec![AwardRecord {
            publication_number: "award".to_owned(),
            values: vec![Decimal::new(50_000, 0)],
            ..AwardRecord::default()
        }];
        let result = match_company_to_tender(
            &company(),
            &tender,
            &history,
            &CompanyPreferences::default(),
        );
        assert!(result
            .warnings
            .contains(&"Contract value is above the previous award range".to_owned()));
    }

    /// A notice that can be performed anywhere in the EEA must not score as if
    /// its location were unsuitable: `anyw-eea` and the country-level `BEZZZ`
    /// are "no particular place", not a remote municipality.
    #[test]
    fn anywhere_regions_are_neutral_not_remote() {
        for region in ["anyw-eea", "BEZZZ"] {
            let tender = Tender {
                source: "test".to_owned(),
                external_id: "anywhere".to_owned(),
                title: "Software services".to_owned(),
                regions: vec![region.to_owned()],
                ..Tender::default()
            };
            let signals = match_signals(&company(), &tender, &[], &CompanyPreferences::default());
            assert_eq!(signals.region, 0.5, "{region} should be neutral");
        }
        // A stated place still scores normally, and a preference can still
        // match inside a mixed list.
        let mixed = Tender {
            source: "test".to_owned(),
            external_id: "mixed".to_owned(),
            title: "Software services".to_owned(),
            regions: vec!["anyw-eea".to_owned(), "BE100".to_owned()],
            ..Tender::default()
        };
        let preferences = CompanyPreferences {
            preferred_regions: vec!["be100".to_owned()],
            ..CompanyPreferences::default()
        };
        assert_eq!(
            match_signals(&company(), &mixed, &[], &preferences).region,
            1.0
        );
    }

    /// The province table must map the ranges a Belgian reader would expect.
    #[test]
    fn zipcode_ranges_map_to_provinces() {
        let provinces = &matcher_tables().zipcode_provinces;
        for (zipcode, expected) in [
            ("1000", "BE10"),
            ("1300", "BE31"),
            ("1500", "BE24"),
            ("2000", "BE21"),
            ("3000", "BE24"),
            ("3920", "BE22"),
            ("4000", "BE33"),
            ("5000", "BE35"),
            ("6700", "BE34"),
            ("7000", "BE32"),
            ("8000", "BE25"),
            ("9000", "BE23"),
        ] {
            assert_eq!(
                provinces.province_for(zipcode),
                Some(expected),
                "zipcode {zipcode}"
            );
        }
        assert_eq!(provinces.province_for(""), None);
        assert_eq!(provinces.province_for("abc"), None);
    }

    /// A notice states its place as a NUTS code, so a company registered in the
    /// same province must match it, a different province must not, and a coarser
    /// region level must stay unknown rather than count as a match.
    #[test]
    fn region_matches_by_province_for_nuts_codes() {
        let mut company = company();
        company.addresses = vec![Address {
            address_type: "REGO".to_owned(),
            zipcode: Some("3920".to_owned()),
            municipality_nl: Some("Lommel".to_owned()),
            municipality_fr: Some("Lommel".to_owned()),
            ..Address::default()
        }];
        let for_regions = |regions: Vec<&str>| {
            let tender = Tender {
                source: "ted".to_owned(),
                external_id: "1".to_owned(),
                title: "Elektrische installatie".to_owned(),
                regions: regions.into_iter().map(str::to_owned).collect(),
                ..Tender::default()
            };
            match_signals(&company, &tender, &[], &CompanyPreferences::default()).region
        };
        assert_eq!(for_regions(vec!["BE224"]), 1.0, "same province");
        assert_eq!(for_regions(vec!["BE22"]), 1.0, "province itself");
        assert_eq!(for_regions(vec!["BE100"]), 0.0, "different province");
        assert_eq!(for_regions(vec!["BE2"]), 0.5, "coarser level is unknown");
        assert_eq!(for_regions(vec!["anyw-eea"]), 0.5, "anywhere is neutral");
        // A company whose zipcode is unknown cannot be judged: unknown, not remote.
        let mut unmapped = company.clone();
        unmapped.addresses = vec![Address {
            address_type: "REGO".to_owned(),
            zipcode: Some("0000".to_owned()),
            ..Address::default()
        }];
        let tender = Tender {
            source: "ted".to_owned(),
            external_id: "2".to_owned(),
            title: "Elektrische installatie".to_owned(),
            regions: vec!["BE224".to_owned()],
            ..Tender::default()
        };
        assert_eq!(
            match_signals(&unmapped, &tender, &[], &CompanyPreferences::default()).region,
            0.5
        );
    }

    #[test]
    fn preferred_region_matches_structured_region_code() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "region-code".to_owned(),
            title: "Electrical works".to_owned(),
            regions: vec!["BE234".to_owned()],
            ..Tender::default()
        };
        let preferences = CompanyPreferences {
            preferred_regions: vec!["be234".to_owned()],
            ..CompanyPreferences::default()
        };
        let result = match_company_to_tender(&company(), &tender, &[], &preferences);
        // The graded feasibility signals moved this baseline: the tender has no
        // deadline (neutral 0.5 runway) and no recognition statement; the
        // IDF text fallback adds 0.375 x 100 x activity delta over 39.4.
        assert_eq!(result.score, 45.2);
    }

    /// The score is exactly the weighted sum of the components it reports, so a
    /// reader can reconstruct it and a new component cannot be added to the
    /// score without appearing in `MatchSignals`.
    #[test]
    fn score_is_the_weighted_sum_of_the_reported_signals() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "composition".to_owned(),
            title: "Electrical installation works".to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            regions: vec!["BE234".to_owned()],
            estimated_value: Some(Decimal::new(50_000, 0)),
            deadline: Some((Local::now().date_naive() + chrono::Duration::days(45)).to_string()),
            requirements: vec![TenderRequirement {
                text: "Erkenning: C (Wegenbouwkundige werken), Klasse 5".to_owned(),
                ..TenderRequirement::default()
            }],
            ..Tender::default()
        };
        let preferences = CompanyPreferences {
            recognition_class: Some(7),
            recognition_categories: vec!["C".to_owned()],
            ..CompanyPreferences::default()
        };
        let signals = match_signals(&company(), &tender, &[], &preferences);
        let tables = matcher_tables();
        let weights = &tables.weights;
        let expected = ((signals.activity * weights.activity)
            + (signals.region * weights.region)
            + (signals.value_fit * weights.value)
            + (signals.history_match * weights.history)
            + (signals.deadline_runway * weights.deadline)
            + (signals.recognition_fit * weights.recognition)
            + (signals.requirement_load * weights.requirements))
            * 100.0;
        let result = match_company_to_tender(&company(), &tender, &[], &preferences);
        assert!(
            (result.score - (expected * 10.0).round() / 10.0).abs() < 0.05,
            "score {} should follow the reported signals ({expected})",
            result.score
        );
        assert_eq!(
            signals.recognition_fit, 1.0,
            "class 7 covers the stated class 5"
        );
        assert_eq!(
            signals.deadline_runway, 1.0,
            "45 days is the comfortable band"
        );
    }

    #[test]
    fn cpv_family_matches_nace_activity() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "cpv".to_owned(),
            title: "Travaux techniques".to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(result.score >= 50.0);
    }

    #[test]
    fn engineering_cpv_family_matches_engineering_nace() {
        let company = CompanyProfile {
            enterprise_number: "0123.456.789".to_owned(),
            status: Some("AC".to_owned()),
            juridical_situation: None,
            juridical_form: None,
            start_date: None,
            names: vec!["Bureau d'études".to_owned()],
            activities: vec![Activity {
                code: "71121".to_owned(),
                classification: "MAIN".to_owned(),
                french: Some("Ingénierie et études techniques".to_owned()),
                dutch: Some("Ingenieursstudies en technisch ontwerp".to_owned()),
            }],
            addresses: vec![],
            establishments: vec![],
            ..Default::default()
        };
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "engineering".to_owned(),
            title: "Stabiliteitsstudie brug".to_owned(),
            cpv_codes: vec!["71310000".to_owned()],
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company, &tender, &[], &CompanyPreferences::default());
        assert!(result.score >= 50.0);
    }

    #[test]
    fn text_similarity_remains_visible_when_cpv_nace_match_drives_score() {
        let company = company();
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "text-diagnostic".to_owned(),
            title: "Elektrische installatie en onderhoud gebouwen".to_owned(),
            description: "Uitvoering van elektrische installaties en onderhoud van gebouwen"
                .to_owned(),
            cpv_codes: vec!["45310000".to_owned()],
            ..Tender::default()
        };
        let breakdown = score_breakdown(&company, &tender, &[], &CompanyPreferences::default());
        let result =
            match_company_to_tender(&company, &tender, &[], &CompanyPreferences::default());

        assert_eq!(breakdown.activity.signal, 1.0);
        assert!(breakdown.activity_text_similarity > 0.0);
        assert_eq!(
            activity_text_coverage_with(&company, &tender, &CompanyPreferences::default()),
            "covered"
        );
        assert_eq!(
            activity_text_coverage_with(
                &company,
                &Tender::default(),
                &CompanyPreferences::default()
            ),
            "no_tender_text"
        );
        let mut no_activity_text = company.clone();
        no_activity_text.activities.clear();
        assert_eq!(
            activity_text_coverage_with(&no_activity_text, &tender, &CompanyPreferences::default()),
            "no_company_activity_text"
        );
        assert!(
            (breakdown.total - result.score).abs() < 0.051,
            "breakdown remains the unrounded source of the displayed score"
        );
        // The diagnostic is deliberately not an extra weighted contribution.
        assert_eq!(breakdown.activity.points, 100.0 * breakdown.activity.weight);
    }

    #[test]
    fn unrelated_cpv_family_does_not_match_nace_activity() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "unrelated".to_owned(),
            title: "Stabiliteitsstudie brug".to_owned(),
            cpv_codes: vec!["71310000".to_owned()],
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(result.score < 50.0);
    }

    #[test]
    fn primary_cpv_prefixes_derived_from_nace_and_crosswalk() {
        let mut it_company = CompanyProfile {
            activities: vec![Activity {
                code: "62010".to_owned(),
                classification: "MAIN".to_owned(),
                french: None,
                dutch: None,
            }],
            ..Default::default()
        };
        // Derived from NACE 62 via cpv_to_nace_prefixes
        let derived = primary_cpv_prefixes_for_company(&it_company);
        assert_eq!(derived, vec!["30", "48", "72"]);

        // Explicit profile prefixes take precedence
        it_company.primary_cpv_prefixes = vec!["72000000".to_owned()];
        let explicit = primary_cpv_prefixes_for_company(&it_company);
        assert_eq!(explicit, vec!["72000000"]);

        // Cleaning company NACE 81
        let cleaning_company = CompanyProfile {
            activities: vec![Activity {
                code: "81210".to_owned(),
                classification: "MAIN".to_owned(),
                french: None,
                dutch: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            primary_cpv_prefixes_for_company(&cleaning_company),
            vec!["77", "90"]
        );

        // Empty activities gives empty prefixes
        let empty_company = CompanyProfile::default();
        assert!(primary_cpv_prefixes_for_company(&empty_company).is_empty());
    }

    #[test]
    fn requirement_warning_is_explicit() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "2".to_owned(),
            title: "Electrical works".to_owned(),
            requirements: vec![TenderRequirement {
                text: "ISO 9001 certificate and minimum omzet".to_owned(),
                ..TenderRequirement::default()
            }],
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(result
            .warnings
            .contains(&"ISO certification requirement".to_owned()));
        assert!(result
            .warnings
            .contains(&"Minimum turnover requirement".to_owned()));
    }

    /// The recognition gate as it appears in real notices: the requirement text
    /// is already stored, and a declared class decides whether it is a warning
    /// or a reassurance. It must never read as an eligibility verdict.
    #[test]
    fn recognition_requirement_warns_or_reassures_from_declared_class() {
        let tender = Tender {
            source: "ted".to_owned(),
            external_id: "645688-2026".to_owned(),
            title: "Wegenwerken".to_owned(),
            requirements: vec![TenderRequirement {
                text:
                    "Erkenning en registratie van aannemers: C (Wegenbouwkundige werken), Klasse 7"
                        .to_owned(),
                ..TenderRequirement::default()
            }],
            ..Tender::default()
        };

        // Declared class below the requirement: a warning naming both numbers.
        let below = CompanyPreferences {
            recognition_class: Some(3),
            recognition_categories: vec!["C".to_owned()],
            ..CompanyPreferences::default()
        };
        let result = match_company_to_tender(&company(), &tender, &[], &below);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("class 7") && warning.contains("declared class 3")),
            "expected a class warning, got {:?}",
            result.warnings
        );

        // Declared class covering it: a reason, and no class warning.
        let above = CompanyPreferences {
            recognition_class: Some(7),
            recognition_categories: vec!["C".to_owned()],
            ..CompanyPreferences::default()
        };
        let result = match_company_to_tender(&company(), &tender, &[], &above);
        assert!(
            result
                .reasons
                .iter()
                .any(|reason| reason.contains("within your declared class 7")),
            "expected a reassurance, got {:?}",
            result.reasons
        );
        assert!(!result
            .warnings
            .iter()
            .any(|warning| warning.contains("recognition")));

        // Nothing declared keeps today's behaviour: the generic credential
        // warning from the term tables, and no comparative claim.
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(!result
            .reasons
            .iter()
            .any(|reason| reason.contains("declared class")));
        assert!(!result
            .warnings
            .iter()
            .any(|warning| warning.contains("declared class")));
    }

    #[test]
    fn duplicate_warning_terms_collapse_into_one_warning() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "duplicates".to_owned(),
            title: "Certificate and certification required".to_owned(),
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert_eq!(
            result
                .warnings
                .iter()
                .filter(|warning| *warning == "Certification or certificate requirement")
                .count(),
            1
        );
    }

    #[test]
    fn iso_warning_matches_sentence_final_iso() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "iso-end".to_owned(),
            title: "Quality management conforme ISO.".to_owned(),
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(result
            .warnings
            .contains(&"ISO certification requirement".to_owned()));
        assert!(!result
            .warnings
            .contains(&"Certification or certificate requirement".to_owned()));
    }

    #[test]
    fn iso_warning_ignores_longer_words_containing_iso() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "iso-prefix".to_owned(),
            title: "Isolation works and isolation panels".to_owned(),
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(!result
            .warnings
            .contains(&"ISO certification requirement".to_owned()));
    }

    #[test]
    fn turnover_warning_matches_typographic_apostrophe() {
        let tender = Tender {
            source: "test".to_owned(),
            external_id: "apostrophe".to_owned(),
            title: "Chiffre d\u{2019}affaires minimum exigé".to_owned(),
            ..Tender::default()
        };
        let result =
            match_company_to_tender(&company(), &tender, &[], &CompanyPreferences::default());
        assert!(result
            .warnings
            .contains(&"Minimum turnover requirement".to_owned()));
    }

    #[test]
    fn contains_word_respects_word_boundaries() {
        // The matcher lowercases the haystack before calling this helper.
        assert!(contains_word("conforme iso.", "iso"));
        assert!(contains_word("iso-9001 certified", "iso"));
        assert!(!contains_word("isolation panels", "iso"));
        assert!(!contains_word("iso_x system", "iso"));
        assert!(contains_word("iso", "iso"));
        assert!(!contains_word("certification", "iso"));
    }
}
