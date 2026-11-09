//! Parse NBB Consult XBRL exports into `NbbFiling`s (one per period present).
//!
//! Two taxonomy generations share this parser: concept-based (up to 2021,
//! `pfs:Turnover`-style names with Current/Preceding contexts) and dimensional
//! (2022 onwards, generic `met:am2` facts qualified by `bas` members for the
//! chart code and `prd:m1/m2` for current/previous period). The empirical
//! dictionaries below were verified against the filed PDFs; anything mapped
//! but conflicting aborts the file (drift alarm), unmapped concepts are
//! ignored like unknown CSV codes.

use crate::nbb::NbbFiling;
use chrono::{Datelike, NaiveDate};
use quick_xml::events::Event;
use quick_xml::name::{QName, ResolveResult};
use quick_xml::NsReader;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;

/// (bas member, metric element) -> chart code, verified against PDF long-form:
/// m53/am2 carries 70; one m53/am1 shape also carries 70 when ntr:m17 and
/// part:m4 are present. m44/am2 carries 9901, m73/dec1 carries 9087, and
/// m37/am2 carries total equity (10/15). m38/am1 can carry contributed capital
/// (10/11), so it is deliberately not treated as total equity.
///
/// Staff facts distinguish labour categories: wkr:m1 means employees, not
/// total available labour. The audited 9087 shape is typ:m16 (Average),
/// qlt:m3 (FTE), part:m6 (explanatory disclosures), wkr:m1 (Employees).
/// Other wkr members describe hired temporary staff/personnel made available;
/// they must not be silently added to, or substituted for, employee FTE.
///
/// `allowed` lists the only dimensions a mapped fact may carry. Breakdown
/// proliferation (gender, regime, movement slices) is normal taxonomy
/// evolution, not drift: extra-dimension facts are skipped, never guessed
/// at. Skips stay visible (missing rows in the import output); true
/// conflicts between included facts still abort the file.
///
/// sts-qualified facts are excluded: sts marks sub-views, not statement
/// lines (observed: part:m5 + sts:m11 carries a segment 9901 that differs
/// from the headline part:m4 figure). A period left with no mappable facts
/// yields no row; a filing with nothing mappable at all is refused.
struct DimMap {
    bas: &'static str,
    metric: &'static str,
    code: &'static str,
    required: &'static [(&'static str, &'static str)],
    allowed: &'static [&'static str],
}

const DIM_CODES: &[DimMap] = &[
    DimMap {
        bas: "m53",
        metric: "am2",
        code: "70",
        required: &[],
        allowed: &["bas", "ntr", "part", "prd"],
    },
    DimMap {
        bas: "m53",
        metric: "am1",
        code: "70",
        required: &[("ntr", "m17"), ("part", "m4")],
        allowed: &["bas", "ntr", "part", "prd"],
    },
    DimMap {
        bas: "m44",
        metric: "am2",
        code: "9901",
        required: &[],
        allowed: &["bas", "ntr", "part", "prd"],
    },
    DimMap {
        bas: "m73",
        metric: "dec1",
        code: "9087",
        required: &[("wkr", "m1")],
        allowed: &["bas", "part", "prd", "qlt", "typ", "wkr"],
    },
    DimMap {
        bas: "m37",
        metric: "am2",
        code: "10/15",
        required: &[],
        allowed: &["bas", "ntr", "part", "prd"],
    },
];

/// Strict evidence-only debt shapes, verified against the filed PDF: page 8
/// (17, 42, 170/4, 43) and page 27 (8801). All use monetary EUR units.
/// These must not widen the legacy importer's four-code contract.
const STRICT_DIM_CODES: &[DimMap] = &[
    DimMap {
        bas: "m50",
        metric: "am1",
        code: "17",
        required: &[("part", "m3"), ("rst", "m1")],
        allowed: &["bas", "part", "rst", "prd"],
    },
    DimMap {
        bas: "m50",
        metric: "am1",
        code: "42",
        required: &[("part", "m3"), ("rst", "m3")],
        allowed: &["bas", "part", "rst", "prd"],
    },
    DimMap {
        bas: "m50",
        metric: "am1",
        code: "170/4",
        required: &[("ntr", "m3"), ("part", "m3"), ("rst", "m1")],
        allowed: &["bas", "ntr", "part", "rst", "prd"],
    },
    DimMap {
        bas: "m50",
        metric: "am1",
        code: "43",
        required: &[("ntr", "m3"), ("part", "m3"), ("rst", "m2")],
        allowed: &["bas", "ntr", "part", "rst", "prd"],
    },
    DimMap {
        bas: "m50",
        metric: "am1",
        code: "8801",
        required: &[("ntr", "m3"), ("part", "m6"), ("rst", "m3")],
        allowed: &["bas", "ntr", "part", "rst", "prd"],
    },
];

/// Concept local name -> chart code (pre-2022 taxonomy). 9903
/// (`GainLossBeforeTaxes`) stays deliberately unmapped, same as the CSV path.
const CONCEPT_CODES: &[(&str, &str)] = &[
    ("Turnover", "70"),
    ("OperatingProfitLoss", "9901"),
    ("Equity", "10/15"),
    (
        "EmployeesRecordedPersonnelRegisterAverageNumberEmployeesCalculatedFullTimeEquivalents",
        "9087",
    ),
    (
        "AverageNumberEmployeesPersonnelRegisterTotalFullTimeEquivalents",
        "9087",
    ),
];

const MAX_XBRL_DEPTH: usize = 256;
const MAX_XBRL_ELEMENTS: usize = 1_000_000;

const XBRLI: &str = "http://www.xbrl.org/2003/instance";

/// An expanded XML name, independent of the document's prefix choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpandedName {
    pub namespace: String,
    pub local: String,
}

#[derive(Clone, Debug, Default)]
pub struct ContextEvidence {
    pub identifier: String,
    pub identifier_scheme: String,
    pub instant: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub dimensions: Vec<(ExpandedName, ExpandedName)>,
    pub typed_dimensions: bool,
}

#[derive(Clone, Debug)]
pub struct FactEvidence {
    pub id: Option<String>,
    /// One-based element ordinal in the original document (including empty elements).
    pub ordinal: usize,
    pub metric: ExpandedName,
    pub context_ref: String,
    pub context: ContextEvidence,
    pub raw_value: String,
    pub decimals: Option<String>,
    pub unit_ref: Option<String>,
    pub unit_measure: Option<ExpandedName>,
}

pub struct FilingEvidence {
    pub filing: NbbFiling,
    pub contributors: BTreeMap<String, Vec<FactEvidence>>,
    pub declared_dates: Vec<FactEvidence>,
}

pub struct ExpectedFiling<'a> {
    pub enterprise_number: &'a str,
    pub current_start: &'a str,
    pub current_end: &'a str,
}

#[derive(Default)]
struct CtxBuilder {
    evidence: ContextEvidence,
    verified_structure: bool,
    identifiers: usize,
    instant: Option<String>,
    start: Option<String>,
    end: Option<String>,
    dims: Vec<(String, String)>,
    unverified_dims: bool,
}

struct Fact {
    metric: String,
    context: String,
    value: String,
    decimals: Option<String>,
    nbb_metric: bool,
    id: Option<String>,
    ordinal: usize,
    expanded_metric: ExpandedName,
    unit_ref: Option<String>,
    nil: bool,
    verified_attributes: bool,
}

fn local_name(raw: &[u8], decoder: quick_xml::encoding::Decoder) -> Result<String, Box<dyn Error>> {
    let name: String = decoder.decode(raw)?.into_owned();
    Ok(name.rsplit(':').next().unwrap_or(&name).to_owned())
}

fn attributes(
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> Result<HashMap<String, String>, Box<dyn Error>> {
    let mut out = HashMap::new();
    for attribute in element.attributes().with_checks(false) {
        let attribute = attribute?;
        let key: String = decoder.decode(attribute.key.as_ref())?.into_owned();
        let key = key.rsplit(':').next().unwrap_or(&key).to_owned();
        let value: String = attribute
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)?
            .into_owned();
        out.insert(key, value);
    }
    Ok(out)
}

fn unqualified_attributes(element: &quick_xml::events::BytesStart<'_>, names: &[&[u8]]) -> bool {
    element.attributes().all(|a| {
        a.is_ok_and(|a| {
            let raw = a.key.as_ref();
            let local = raw.rsplit(|b| *b == b':').next().unwrap_or(raw);
            !names.contains(&local) || raw == local
        })
    })
}

fn expanded(reader: &NsReader<&[u8]>, raw: &[u8]) -> ExpandedName {
    let (ns, local) = reader.resolver().resolve_element(QName(raw));
    ExpandedName {
        namespace: match ns {
            ResolveResult::Bound(ns) => String::from_utf8_lossy(ns.as_ref()).into_owned(),
            _ => String::new(),
        },
        local: String::from_utf8_lossy(local.as_ref()).into_owned(),
    }
}

struct Parsed {
    root_verified: bool,
    duplicate_units: bool,
    units: HashMap<String, Vec<ExpandedName>>,
    contexts: HashMap<String, CtxBuilder>,
    facts: Vec<Fact>,
    enterprise_ids: Vec<String>,
    entity_name: Option<String>,
}

fn parse_document(text: &str) -> Result<Parsed, Box<dyn Error>> {
    let mut reader = NsReader::from_str(text);
    reader.config_mut().trim_text(true);
    let decoder = reader.decoder();
    let mut parsed = Parsed {
        root_verified: false,
        duplicate_units: false,
        units: HashMap::new(),
        contexts: HashMap::new(),
        facts: Vec::new(),
        enterprise_ids: Vec::new(),
        entity_name: None,
    };
    let mut stack: Vec<(String, HashMap<String, String>)> = Vec::new();
    let mut elements = 0_usize;
    let mut text_target: Option<String> = None;
    let mut text_buf = String::new();
    let mut cur_context: Option<(String, CtxBuilder)> = None;
    let mut cur_fact: Option<Fact> = None;
    let mut cur_unit: Option<(String, Vec<ExpandedName>)> = None;
    let mut unit_valid = true;
    let mut cur_dim: Option<(String, bool)> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                elements += 1;
                if elements > MAX_XBRL_ELEMENTS || stack.len() + 1 > MAX_XBRL_DEPTH {
                    return Err("nbb xbrl exceeds parser limits".into());
                }
                let name = local_name(element.name().as_ref(), decoder)?;
                let attrs = attributes(&element, decoder)?;
                let qname = expanded(&reader, element.name().as_ref());
                if stack.is_empty() {
                    parsed.root_verified = qname.namespace == XBRLI && name == "xbrl";
                }
                if let Some((_, ctx)) = cur_context.as_mut() {
                    let ns = if matches!(name.as_str(), "explicitMember" | "typedMember") {
                        "http://xbrl.org/2006/xbrldi"
                    } else {
                        XBRLI
                    };
                    if matches!(
                        name.as_str(),
                        "entity"
                            | "identifier"
                            | "period"
                            | "instant"
                            | "startDate"
                            | "endDate"
                            | "scenario"
                            | "segment"
                            | "explicitMember"
                            | "typedMember"
                    ) {
                        ctx.verified_structure &= qname.namespace == ns;
                        let parent = stack.last().map(|(n, _)| n.as_str());
                        ctx.verified_structure &= match name.as_str() {
                            "entity" | "period" | "scenario" => parent == Some("context"),
                            "identifier" | "segment" => parent == Some("entity"),
                            "instant" | "startDate" | "endDate" => parent == Some("period"),
                            "explicitMember" | "typedMember" => {
                                matches!(parent, Some("scenario" | "segment"))
                            }
                            _ => true,
                        };
                        ctx.verified_structure &=
                            unqualified_attributes(&element, &[b"scheme", b"dimension"]);
                    }
                    if name == "typedMember" {
                        ctx.evidence.typed_dimensions = true;
                    }
                }
                if cur_unit.is_some()
                    && (name != "measure" || stack.last().is_none_or(|(n, _)| n != "unit"))
                {
                    unit_valid = false;
                }
                if name == "unit" {
                    cur_unit = Some((attrs.get("id").cloned().unwrap_or_default(), Vec::new()));
                    unit_valid =
                        qname.namespace == XBRLI && unqualified_attributes(&element, &[b"id"]);
                }
                if name == "context" {
                    let id = attrs.get("id").cloned().unwrap_or_default();
                    cur_context = Some((
                        id,
                        CtxBuilder {
                            verified_structure: qname.namespace == XBRLI
                                && unqualified_attributes(&element, &[b"id"]),
                            ..CtxBuilder::default()
                        },
                    ));
                    text_target = None;
                } else if name == "identifier" {
                    // Entity identifiers only: a footnote or segment could
                    // theoretically nest another one, and two distinct ids
                    // must abort rather than silently pick one.
                    if stack.iter().any(|(n, _)| n == "entity") {
                        if let Some((_, ctx)) = cur_context.as_mut() {
                            ctx.identifiers += 1;
                            ctx.evidence.identifier_scheme =
                                attrs.get("scheme").cloned().unwrap_or_default();
                            ctx.verified_structure &=
                                stack.last().is_some_and(|(n, _)| n == "entity");
                        }
                        text_target = Some("identifier".to_owned());
                        text_buf.clear();
                    } else {
                        text_target = None;
                    }
                } else if name == "instant" || name == "startDate" || name == "endDate" {
                    if cur_context.is_some() {
                        text_target = Some(name.clone());
                        text_buf.clear();
                    } else {
                        text_target = None;
                    }
                } else if name == "explicitMember" {
                    cur_dim = attrs.get("dimension").map(|dim| {
                        let verified = matches!(reader.resolver().resolve_element(QName(dim.as_bytes())).0,
                            ResolveResult::Bound(ns) if ns.as_ref() == b"http://www.nbb.be/be/fr/cbso/dict/dim");
                        (dim.clone(), verified)
                    });
                    text_target = Some("member".to_owned());
                    text_buf.clear();
                } else if name == "measure" && cur_unit.is_some() {
                    unit_valid &= qname.namespace == XBRLI;
                    text_target = Some("measure".to_owned());
                    text_buf.clear();
                } else if name == "EntityCurrentLegalName" {
                    text_target = Some("entity_name".to_owned());
                    text_buf.clear();
                } else if let Some(ctx) = attrs.get("contextRef") {
                    let decimals = attrs.get("decimals").cloned();
                    let nbb_metric = matches!(reader.resolver().resolve_element(element.name()).0,
                            ResolveResult::Bound(ns)
                                if ns.as_ref() == b"http://www.nbb.be/be/fr/cbso/dict/met");
                    cur_fact = Some(Fact {
                        metric: name.clone(),
                        context: ctx.clone(),
                        value: String::new(),
                        decimals,
                        nbb_metric,
                        id: attrs.get("id").cloned(),
                        ordinal: elements,
                        expanded_metric: qname,
                        unit_ref: attrs.get("unitRef").cloned(),
                        nil: attrs.contains_key("nil"),
                        verified_attributes: unqualified_attributes(
                            &element,
                            &[b"id", b"contextRef", b"unitRef", b"decimals"],
                        ),
                    });
                    text_target = Some("fact".to_owned());
                    text_buf.clear();
                } else {
                    text_target = None;
                }
                stack.push((name, attrs));
            }
            Ok(Event::Empty(element)) => {
                elements += 1;
                let attrs = attributes(&element, decoder)?;
                if let Some(ctx) = attrs.get("contextRef") {
                    let metric = expanded(&reader, element.name().as_ref());
                    parsed.facts.push(Fact {
                        nbb_metric: metric.namespace == "http://www.nbb.be/be/fr/cbso/dict/met",
                        metric: metric.local.clone(),
                        context: ctx.clone(),
                        value: String::new(),
                        decimals: attrs.get("decimals").cloned(),
                        id: attrs.get("id").cloned(),
                        ordinal: elements,
                        expanded_metric: metric,
                        unit_ref: attrs.get("unitRef").cloned(),
                        nil: attrs.contains_key("nil"),
                        verified_attributes: unqualified_attributes(
                            &element,
                            &[b"id", b"contextRef", b"unitRef", b"decimals"],
                        ),
                    });
                }
                if let Some((_, ctx)) = cur_context.as_mut() {
                    // Empty context children cannot supply any verified evidence.
                    ctx.verified_structure = false;
                }
                if cur_unit.is_some() {
                    unit_valid = false;
                }
                if elements > MAX_XBRL_ELEMENTS {
                    return Err("nbb xbrl exceeds parser limits".into());
                }
            }
            Ok(Event::Text(text)) => {
                if text_target.is_some() {
                    let decoded: String = decoder.decode(text.as_ref())?.into_owned();
                    text_buf.push_str(&decoded);
                }
            }
            Ok(Event::End(element)) => {
                let name = local_name(element.name().as_ref(), decoder)?;
                match text_target.as_deref() {
                    Some("identifier") if name == "identifier" => {
                        parsed.enterprise_ids.push(text_buf.trim().to_owned());
                        if let Some((_, ctx)) = cur_context.as_mut() {
                            ctx.evidence.identifier = text_buf.trim().to_owned();
                        }
                    }
                    Some("entity_name") if name == "EntityCurrentLegalName" => {
                        if parsed.entity_name.is_none() {
                            parsed.entity_name = Some(text_buf.trim().to_owned());
                        }
                    }
                    Some("fact") => {
                        if let Some(mut fact) = cur_fact.take() {
                            fact.value = text_buf.trim().to_owned();
                            parsed.facts.push(fact);
                        }
                    }
                    Some("measure") if name == "measure" => {
                        if let Some((_, measures)) = cur_unit.as_mut() {
                            measures.push(expanded(&reader, text_buf.trim().as_bytes()));
                        }
                    }
                    Some(t)
                        if cur_context.is_some() && stack.iter().any(|(n, _)| n == "context") =>
                    {
                        if let Some((_, builder)) = cur_context.as_mut() {
                            match t {
                                "instant" if name == "instant" => {
                                    builder.verified_structure &= builder.instant.is_none();
                                    builder.instant = Some(text_buf.trim().to_owned())
                                }
                                "startDate" if name == "startDate" => {
                                    builder.start = Some(text_buf.trim().to_owned())
                                }
                                "endDate" if name == "endDate" => {
                                    builder.end = Some(text_buf.trim().to_owned())
                                }
                                "member" if name == "explicitMember" => {
                                    if let Some((dim, dimension_verified)) = cur_dim.take() {
                                        let member = text_buf.trim().to_owned();
                                        let domain = format!(
                                            "http://www.nbb.be/be/fr/cbso/dict/dom/{}",
                                            dim.rsplit(':').next().unwrap_or(&dim)
                                        );
                                        // NsReader retains this end tag's bindings until the next read.
                                        let member_verified = matches!(reader.resolver().resolve_element(QName(member.as_bytes())).0,
                                            ResolveResult::Bound(ns) if ns.as_ref() == domain.as_bytes());
                                        builder.evidence.dimensions.push((
                                            expanded(&reader, dim.as_bytes()),
                                            expanded(&reader, member.as_bytes()),
                                        ));
                                        builder.unverified_dims |=
                                            !dimension_verified || !member_verified;
                                        let dim = dim.rsplit(':').next().unwrap_or(&dim).to_owned();
                                        let member =
                                            member.rsplit(':').next().unwrap_or(&member).to_owned();
                                        if builder.dims.iter().any(|(key, _)| key == &dim) {
                                            return Err("duplicate XBRL context dimension".into());
                                        }
                                        builder.dims.push((dim, member));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if name == "unit" {
                    if let Some((id, mut measures)) = cur_unit.take() {
                        if !unit_valid {
                            measures.clear();
                        }
                        if parsed.units.insert(id, measures).is_some() {
                            parsed.duplicate_units = true;
                        }
                    }
                }
                if name == "context" {
                    if let Some((id, mut builder)) = cur_context.take() {
                        builder.evidence.instant = builder.instant.clone();
                        builder.evidence.start = builder.start.clone();
                        builder.evidence.end = builder.end.clone();
                        if !id.is_empty() && parsed.contexts.insert(id, builder).is_some() {
                            return Err("duplicate XBRL context identifier".into());
                        }
                    }
                }
                text_target = None;
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("nbb xbrl parse error: {e}").into()),
            _ => {}
        }
    }
    Ok(parsed)
}

fn normalize_kbo(raw: &str) -> String {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() == 9 {
        format!("0{digits}")
    } else {
        digits
    }
}

/// XBRL `decimals` states precision, not a scale: `INF` is exact and a
/// non-negative N is accurate to N decimal places (`2` = cents). A negative
/// value (`-3` = thousands) means a rounded filing; those still abort rather
/// than be mixed with exact figures. Amounts are assumed EUR (no
/// currency-tagged counterexample exists in Consult or API captures).
fn check_decimals(decimals: &Option<String>, code: &str, year: i64) -> Result<(), String> {
    match decimals.as_deref() {
        None | Some("INF") => Ok(()),
        Some(other) if other.parse::<u32>().is_ok() => Ok(()),
        Some(other) => Err(format!(
            "{code} FY{year} is rounded to {other} decimals, aborting rather than scaling silently"
        )),
    }
}

fn insert_checked(
    values: &mut BTreeMap<(String, i64), Decimal>,
    code: &str,
    year: i64,
    value: Decimal,
) -> Result<(), String> {
    match values.get(&(code.to_owned(), year)) {
        Some(existing) if *existing == value => Ok(()),
        Some(existing) => Err(format!(
            "conflicting {code} values for FY{year}: {existing} vs {value}"
        )),
        None => {
            values.insert((code.to_owned(), year), value);
            Ok(())
        }
    }
}

fn parse_decimal(raw: &str) -> Option<Decimal> {
    raw.replace(' ', "").parse().ok()
}

/// Source-declared accounting dates, not XBRL duration contexts. The NBB
/// dictionary labels mmt:m1/m2 as Start/End date and prd:m1/m2 as current/prior.
/// Only the audited identification shape is accepted; event/person/sub-view dates
/// cannot supply an accounting boundary. Duplicate disagreements abort the file.
fn declared_dimensional_periods(
    parsed: &Parsed,
) -> Result<BTreeMap<String, (String, String)>, String> {
    let mut boundaries: BTreeMap<(String, String), NaiveDate> = BTreeMap::new();
    for fact in &parsed.facts {
        if fact.value.is_empty() {
            continue;
        }
        if !fact.nbb_metric || fact.metric != "dte1" {
            continue;
        }
        let Some(ctx) = parsed.contexts.get(&fact.context) else {
            continue;
        };
        let dims: BTreeMap<&str, &str> = ctx
            .dims
            .iter()
            .map(|(d, m)| (d.as_str(), m.as_str()))
            .collect();
        if ctx.unverified_dims
            || ctx.dims.len() != 4
            || dims.len() != 4
            || dims.get("bas") != Some(&"m27")
            || dims.get("part") != Some(&"m2")
            || !matches!(dims.get("prd"), Some(&"m1" | &"m2"))
            || !matches!(dims.get("mmt"), Some(&"m1" | &"m2"))
        {
            continue;
        }
        if fact.value.len() != 10 || !fact.value.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
            return Err("non-canonical declared accounting date".to_owned());
        }
        let date = NaiveDate::parse_from_str(&fact.value, "%Y-%m-%d")
            .map_err(|_| "invalid declared accounting date".to_owned())?;
        if date.format("%Y-%m-%d").to_string() != fact.value {
            return Err("non-canonical declared accounting date".to_owned());
        }
        let key = (dims["prd"].to_owned(), dims["mmt"].to_owned());
        if boundaries
            .insert(key, date)
            .is_some_and(|prior| prior != date)
        {
            return Err("conflicting declared accounting dates".to_owned());
        }
    }
    let mut periods = BTreeMap::new();
    for prd in ["m1", "m2"] {
        let start = boundaries.get(&(prd.to_owned(), "m1".to_owned()));
        let end = boundaries.get(&(prd.to_owned(), "m2".to_owned()));
        // Partial metadata never invents the absent endpoint; keep the existing
        // instant-only evidence path instead.
        if let (Some(start), Some(end)) = (start, end) {
            if start > end {
                return Err("declared accounting period starts after it ends".to_owned());
            }
            periods.insert(prd.to_owned(), (start.to_string(), end.to_string()));
        }
    }
    if let (Some(current), Some(prior)) = (periods.get("m1"), periods.get("m2")) {
        if prior.1 >= current.0 || prior.1[..4] == current.1[..4] {
            return Err(
                "declared accounting periods overlap or share a fiscal-year key".to_owned(),
            );
        }
    }
    Ok(periods)
}

/// Parse one Consult XBRL export into one filing per period present
/// (current and, when tagged, previous). `source_ref` identifies the filing
/// document and is shared by both rows.
pub fn parse_consult_xbrl(text: &str, source_ref: &str) -> Result<Vec<NbbFiling>, String> {
    Ok(parse_selected(text, source_ref, None)?
        .into_iter()
        .map(|e| e.filing)
        .collect())
}

/// Strict dimensional-only parsing of headline and audited debt codes. Requires
/// caller-verified identity and current declared accounting boundaries. This
/// checks XML evidence, not taxonomy validity or authenticity of the source.
pub fn parse_consult_xbrl_with_evidence(
    text: &str,
    source_ref: &str,
    expected: ExpectedFiling<'_>,
) -> Result<Vec<FilingEvidence>, String> {
    parse_selected(text, source_ref, Some(expected))
}

fn evidence(parsed: &Parsed, fact: &Fact) -> FactEvidence {
    FactEvidence {
        id: fact.id.clone(),
        ordinal: fact.ordinal,
        metric: fact.expanded_metric.clone(),
        context_ref: fact.context.clone(),
        context: parsed.contexts[&fact.context].evidence.clone(),
        raw_value: fact.value.clone(),
        decimals: fact.decimals.clone(),
        unit_ref: fact.unit_ref.clone(),
        unit_measure: fact
            .unit_ref
            .as_ref()
            .and_then(|u| parsed.units.get(u))
            .filter(|m| m.len() == 1)
            .map(|m| m[0].clone()),
    }
}

fn strict_context(ctx: &CtxBuilder, expected: &ExpectedFiling<'_>) -> Result<(), String> {
    if !ctx.verified_structure
        || ctx.unverified_dims
        || ctx.evidence.typed_dimensions
        || ctx.identifiers != 1
        || normalize_kbo(&ctx.evidence.identifier) != expected.enterprise_number
        || ctx.evidence.identifier_scheme != "http://www.fgov.be"
        || ctx
            .instant
            .as_ref()
            .is_none_or(|i| NaiveDate::parse_from_str(i, "%Y-%m-%d").is_err() || i.len() != 10)
        || ctx.start.is_some()
        || ctx.end.is_some()
    {
        return Err(
            "unverified identity, namespace, dimensions or period in selected context".into(),
        );
    }
    Ok(())
}

fn parse_selected(
    text: &str,
    source_ref: &str,
    expected: Option<ExpectedFiling<'_>>,
) -> Result<Vec<FilingEvidence>, String> {
    let parsed = parse_document(text).map_err(|e| e.to_string())?;
    let mut ids: Vec<String> = parsed
        .enterprise_ids
        .iter()
        .map(|id| normalize_kbo(id))
        .filter(|id| !id.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    if ids.len() != 1 {
        return Err(format!(
            "expected one enterprise identifier, found {}",
            ids.len()
        ));
    }
    let enterprise_number = ids.pop().expect("one id checked");
    if let Some(expected) = &expected {
        if !parsed.root_verified
            || parsed.duplicate_units
            || expected.enterprise_number.len() != 10
            || !expected
                .enterprise_number
                .bytes()
                .all(|b| b.is_ascii_digit())
            || enterprise_number != expected.enterprise_number
        {
            return Err("strict XBRL identity, root namespace or unit ambiguity".into());
        }
    }
    let mut contributors: BTreeMap<(String, i64), Vec<FactEvidence>> = BTreeMap::new();
    let mut declared_dates = Vec::new();
    let dimensional = parsed
        .contexts
        .values()
        .any(|c| c.dims.iter().any(|(d, _)| d == "prd"));

    // (chart code, fiscal year) -> value, with consensus across breakdowns.
    let mut values: BTreeMap<(String, i64), Decimal> = BTreeMap::new();
    // fiscal year -> (period_start, period_end). Keep a dimensional context's
    // instant only when it belongs to the period selected by its prd marker;
    // a current-year instant on a prd:m2 comparative is not that prior period's
    // end date and must remain unknown.
    let mut periods: BTreeMap<i64, (String, String)> = BTreeMap::new();

    if dimensional {
        let declared = declared_dimensional_periods(&parsed)?;
        if let Some(expected) = &expected {
            if declared.get("m1")
                != Some(&(
                    expected.current_start.to_owned(),
                    expected.current_end.to_owned(),
                ))
            {
                return Err("missing or mismatched full declared current accounting period".into());
            }
            for fact in &parsed.facts {
                if fact.metric != "dte1" || !fact.nbb_metric {
                    continue;
                }
                let Some(ctx) = parsed.contexts.get(&fact.context) else {
                    continue;
                };
                let dims: BTreeMap<_, _> = ctx
                    .dims
                    .iter()
                    .map(|(d, m)| (d.as_str(), m.as_str()))
                    .collect();
                if dims.get("bas") == Some(&"m27")
                    && dims.get("part") == Some(&"m2")
                    && dims.len() == 4
                    && dims.contains_key("mmt")
                    && dims.contains_key("prd")
                    && !ctx.unverified_dims
                {
                    strict_context(ctx, expected)?;
                    if !declared.contains_key(dims["prd"])
                        || !matches!(dims["mmt"], "m1" | "m2")
                        || fact.nil
                        || fact.value.is_empty()
                        || !fact.verified_attributes
                    {
                        return Err(
                            "incomplete or invalid declared accounting date evidence".into()
                        );
                    }
                    declared_dates.push(evidence(&parsed, fact));
                }
            }
        }
        let mut end_year: Option<i64> = None;
        for ctx in parsed.contexts.values() {
            if let Some(instant) = ctx.instant.as_deref() {
                let year: i64 = instant
                    .get(..4)
                    .and_then(|y| y.parse().ok())
                    .ok_or_else(|| format!("bad instant date: {instant:?}"))?;
                end_year = Some(end_year.map_or(year, |e| e.max(year)));
            }
        }
        let end_year = declared
            .get("m1")
            .and_then(|(_, end)| NaiveDate::parse_from_str(end, "%Y-%m-%d").ok())
            .map(|date| i64::from(date.year()))
            .or(end_year)
            .ok_or_else(|| "no instant periods in dimensional filing".to_string())?;
        let prior_year = declared
            .get("m2")
            .map(|(_, end)| end[..4].parse::<i64>().expect("validated date year"))
            .unwrap_or(end_year - 1);
        if prior_year >= end_year {
            return Err(
                "declared accounting periods cannot share or reverse fiscal-year keys".to_owned(),
            );
        }
        for fact in &parsed.facts {
            if expected.is_none() && fact.value.is_empty() {
                continue;
            }
            let Some(ctx) = parsed.contexts.get(&fact.context) else {
                if expected.is_some() && DIM_CODES.iter().any(|m| m.metric == fact.metric) {
                    return Err("dimensional metric references an absent context".into());
                }
                continue;
            };
            let prd = ctx
                .dims
                .iter()
                .find(|(d, _)| d == "prd")
                .map(|(_, m)| m.as_str());
            let year = match prd {
                Some("m1" | "m2") => declared
                    .get(prd.expect("matched period"))
                    .map(|(_, end)| end[..4].parse::<i64>().expect("validated date year"))
                    .unwrap_or(if prd == Some("m1") {
                        end_year
                    } else {
                        end_year - 1
                    }),
                _ => continue,
            };
            let bas = ctx
                .dims
                .iter()
                .find(|(d, _)| d == "bas")
                .map(|(_, m)| m.as_str());
            let metric = fact.metric.rsplit(':').next().unwrap_or(&fact.metric);
            let Some(map) = DIM_CODES
                .iter()
                .chain(STRICT_DIM_CODES.iter().filter(|_| expected.is_some()))
                .find(|m| {
                    Some(m.bas) == bas
                        && m.metric == metric
                        && m.required.iter().all(|(d, m)| {
                            ctx.dims
                                .iter()
                                .find(|(dd, _)| dd == d)
                                .is_some_and(|(_, v)| v == m)
                        })
                        && ctx
                            .dims
                            .iter()
                            .all(|(d, _)| m.allowed.contains(&d.as_str()))
                })
            else {
                continue;
            };
            if let Some(expected) = &expected {
                strict_context(ctx, expected)?;
                if map.code == "9087"
                    && [("typ", "m16"), ("qlt", "m3"), ("part", "m6"), ("wkr", "m1")]
                        .iter()
                        .any(|(d, m)| !ctx.dims.iter().any(|(dd, mm)| dd == d && mm == m))
                {
                    return Err(
                        "strict employee FTE requires the complete audited headline shape".into(),
                    );
                }
                if !fact.nbb_metric || fact.nil || !fact.verified_attributes {
                    return Err("unverified selected metric namespace or nil fact".into());
                }
                if !prd.is_some_and(|p| declared.contains_key(p)) {
                    return Err("selected fact lacks full declared accounting period".into());
                }
                let measure = fact
                    .unit_ref
                    .as_ref()
                    .and_then(|u| parsed.units.get(u))
                    .filter(|m| m.len() == 1)
                    .map(|m| &m[0]);
                let (ns, local) = if map.code == "9087" {
                    (XBRLI, "pure")
                } else {
                    ("http://www.xbrl.org/2003/iso4217", "EUR")
                };
                if !measure.is_some_and(|m| m.namespace == ns && m.local == local) {
                    return Err(format!("invalid or missing unit for {}", map.code));
                }
            }
            // Do not attach verified source dates to unverified metric/dimension
            // namespaces. The legacy no-metadata path remains unchanged.
            if !declared.is_empty() && (ctx.unverified_dims || !fact.nbb_metric) {
                continue;
            }
            if let Some(instant) = ctx.instant.as_deref() {
                if let Some((_, current_end)) = declared.get("m1") {
                    let compatible = match prd {
                        Some("m1") => instant == current_end,
                        // NBB comparative facts often use the current filing's
                        // instant. Either explicitly declared end is legitimate.
                        Some("m2") => declared.get("m2").is_none_or(|(_, prior_end)| {
                            instant == current_end || instant == prior_end
                        }),
                        _ => true,
                    };
                    if !compatible {
                        return Err(
                            "financial context instant disagrees with declared accounting periods"
                                .to_owned(),
                        );
                    }
                }
            }
            let code = map.code;
            check_decimals(&fact.decimals, code, year)?;
            let value: Decimal = parse_decimal(&fact.value).ok_or_else(|| {
                format!("amount not decimal for {code} FY{year}: {:?}", fact.value)
            })?;
            insert_checked(&mut values, code, year, value)?;
            if expected.is_some() {
                contributors
                    .entry((code.to_owned(), year))
                    .or_default()
                    .push(evidence(&parsed, fact));
            }
            let period_end = ctx
                .instant
                .as_deref()
                .filter(|instant| {
                    instant.get(..4).and_then(|year| year.parse::<i64>().ok()) == Some(year)
                })
                .unwrap_or_default();
            let period = periods.entry(year).or_default();
            if let Some((start, end)) = prd.and_then(|prd| declared.get(prd)) {
                *period = (start.clone(), end.clone());
            }
            if period.1.is_empty() && !period_end.is_empty() {
                period.1 = period_end.to_owned();
            }
        }
    } else {
        if expected.is_some() {
            return Err("strict evidence requires dimensional accounting periods".into());
        }
        for fact in &parsed.facts {
            if fact.value.is_empty() {
                continue;
            }
            let metric = fact.metric.rsplit(':').next().unwrap_or(&fact.metric);
            let Some(code) = CONCEPT_CODES
                .iter()
                .find(|(concept, _)| *concept == metric)
                .map(|(_, code)| *code)
            else {
                continue;
            };
            let Some(ctx) = parsed.contexts.get(&fact.context) else {
                continue;
            };
            let (start, end) = match (&ctx.start, &ctx.end, &ctx.instant) {
                (Some(s), Some(e), _) => (s.clone(), e.clone()),
                (None, None, Some(i)) => (String::new(), i.clone()),
                _ => continue,
            };
            let year: i64 = end
                .get(..4)
                .and_then(|y| y.parse().ok())
                .ok_or_else(|| format!("bad period end date: {end:?}"))?;
            check_decimals(&fact.decimals, code, year)?;
            let value: Decimal = parse_decimal(&fact.value).ok_or_else(|| {
                format!("amount not decimal for {code} FY{year}: {:?}", fact.value)
            })?;
            insert_checked(&mut values, code, year, value)?;
            periods.entry(year).or_insert((start, end));
        }
    }

    if let Some(expected) = &expected {
        if !periods
            .values()
            .any(|(s, e)| s == expected.current_start && e == expected.current_end)
        {
            return Err("no mapped current-period evidence".into());
        }
    }
    if values.is_empty() {
        return Err("no mappable facts in xbrl filing".to_string());
    }
    let mut years: Vec<i64> = values.keys().map(|(_, y)| *y).collect();
    years.sort();
    years.dedup();
    Ok(years
        .into_iter()
        .map(|year| {
            let (period_start, period_end) = periods.remove(&year).unwrap_or_default();
            let mut filing_values = BTreeMap::new();
            for ((code, y), value) in &values {
                if *y == year {
                    filing_values.insert(code.clone(), *value);
                }
            }
            let filing = NbbFiling {
                enterprise_number: enterprise_number.clone(),
                enterprise_name: parsed.entity_name.clone().unwrap_or_default(),
                period_start,
                period_end,
                fiscal_year: year,
                values: filing_values,
                source_ref: source_ref.to_owned(),
            };
            let mut sources = BTreeMap::new();
            for code in filing.values.keys() {
                if let Some(facts) = contributors.remove(&(code.clone(), year)) {
                    sources.insert(code.clone(), facts);
                }
            }
            FilingEvidence {
                filing,
                contributors: sources,
                declared_dates: declared_dates.clone(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIM_SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xbrl xmlns="http://www.xbrl.org/2003/instance" xmlns:xbrldi="http://xbrl.org/2006/xbrldi">
<context id="c1"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2024-06-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m53</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m1</xbrldi:explicitMember></scenario></context>
<context id="c2"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2024-06-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m53</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m2</xbrldi:explicitMember></scenario></context>
<context id="c3"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2024-06-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m44</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m1</xbrldi:explicitMember></scenario></context>
<met:am2 contextRef="c1" unitRef="EUR" decimals="INF">20687443.71</met:am2>
<met:am2 contextRef="c2" unitRef="EUR" decimals="INF">13261682.56</met:am2>
<met:am2 contextRef="c3" unitRef="EUR" decimals="INF">51700.55</met:am2>
<met:str1 contextRef="c1">not a number, ignored</met:str1>
</xbrl>"#;

    const CONCEPT_SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xbrli:xbrl xmlns:xbrli="http://www.xbrl.org/2003/instance" xmlns:pfs="http://www.nbb.be/be/fr/pfs/ci/2017-04-01">
<xbrli:context id="CurrentDuration"><xbrli:entity><xbrli:identifier scheme="http://www.fgov.be">BE0537875391</xbrli:identifier></xbrli:entity><xbrli:period><xbrli:startDate>2020-07-01</xbrli:startDate><xbrli:endDate>2021-06-30</xbrli:endDate></xbrli:period></xbrli:context>
<xbrli:context id="PrecedingDuration"><xbrli:entity><xbrli:identifier scheme="http://www.fgov.be">BE0537875391</xbrli:identifier></xbrli:entity><xbrli:period><xbrli:startDate>2019-07-01</xbrli:startDate><xbrli:endDate>2020-06-30</xbrli:endDate></xbrli:period></xbrli:context>
<pfs:Turnover contextRef="CurrentDuration" unitRef="EUR" decimals="INF">18193629.14</pfs:Turnover>
<pfs:Turnover contextRef="PrecedingDuration" unitRef="EUR" decimals="INF">20580536.72</pfs:Turnover>
<pfs:OperatingProfitLoss contextRef="CurrentDuration" unitRef="EUR" decimals="INF">104442.42</pfs:OperatingProfitLoss>
<pfs:Equity contextRef="CurrentDuration" unitRef="EUR" decimals="INF">18550</pfs:Equity>
</xbrli:xbrl>"#;

    #[test]
    fn decimals_precision_accepts_cents_and_rejects_rounded_or_malformed() {
        for ok in [None, Some("INF"), Some("0"), Some("2")] {
            assert!(
                check_decimals(&ok.map(str::to_owned), "10/15", 2025).is_ok(),
                "{ok:?}"
            );
        }
        for bad in ["-3", "-6", "x"] {
            assert!(
                check_decimals(&Some(bad.to_owned()), "10/15", 2025).is_err(),
                "{bad}"
            );
        }
    }

    fn strict_sample() -> String {
        let metadata = [
            date_metadata("m1", "m1", "2023-07-01", ""),
            date_metadata("m1", "m2", "2024-06-30", ""),
            date_metadata("m2", "m1", "2022-07-01", ""),
            date_metadata("m2", "m2", "2023-06-30", ""),
        ]
        .concat();
        with_date_metadata(&metadata).replace("</xbrl>", r#"<unit id="EUR"><measure xmlns:money="http://www.xbrl.org/2003/iso4217">money:EUR</measure></unit><unit id="pure"><measure>pure</measure></unit></xbrl>"#)
    }

    fn strict_parse(text: &str) -> Result<Vec<FilingEvidence>, String> {
        parse_consult_xbrl_with_evidence(
            text,
            "capture",
            ExpectedFiling {
                enterprise_number: "0537875391",
                current_start: "2023-07-01",
                current_end: "2024-06-30",
            },
        )
    }

    #[test]
    fn strict_evidence_preserves_all_agreeing_contributors_and_dates() {
        let text = strict_sample().replace("</xbrl>", r#"<met:am2 id="duplicate" contextRef="c1" unitRef="EUR" decimals="2">20687443.710</met:am2></xbrl>"#);
        let rows = strict_parse(&text).unwrap();
        assert_eq!(rows.len(), 2);
        let row = &rows[1];
        assert_eq!(row.filing.source_ref, "capture");
        assert_eq!(row.contributors["70"].len(), 2);
        assert_eq!(row.declared_dates.len(), 4);
        let sources = &row.contributors["70"];
        assert_eq!(sources[1].id.as_deref(), Some("duplicate"));
        assert!(sources[1].ordinal > sources[0].ordinal);
        assert_eq!(sources[1].context_ref, "c1");
        assert_eq!(sources[1].raw_value, "20687443.710");
        assert_eq!(sources[1].decimals.as_deref(), Some("2"));
        assert_eq!(
            sources[1].metric.namespace,
            "http://www.nbb.be/be/fr/cbso/dict/met"
        );
        assert_eq!(sources[1].unit_measure.as_ref().unwrap().local, "EUR");
        assert_eq!(sources[1].context.identifier_scheme, "http://www.fgov.be");
        assert_eq!(sources[1].context.dimensions.len(), 2);
        assert_eq!(
            parse_consult_xbrl(&text, "capture").unwrap()[1].values,
            row.filing.values
        );
    }

    #[test]
    fn strict_evidence_rejects_unsafe_selected_contexts_units_and_metadata() {
        let sample = strict_sample();
        for bad in [
            sample.replace("money:EUR", "money:USD"),
            sample.replace("</xbrl>", r#"<met:am2 contextRef="c1" unitRef="EUR" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:nil="true"/></xbrl>"#),
            sample.replace("</xbrl>", r#"<met:am2 contextRef="c1" unitRef="EUR"></met:am2></xbrl>"#),
            sample.replace("<measure xmlns:money", "<measure><measure xmlns:money")
                .replace("money:EUR</measure>", "money:EUR</measure></measure>"),
            sample.replace("http://www.xbrl.org/2003/iso4217", "urn:spoof"),
            sample.replace(r#"unitRef="EUR""#, r#"unitRef="missing""#),
            sample.replace("<measure xmlns:money", "<divide><measure xmlns:money").replace("money:EUR</measure>", "money:EUR</measure></divide>"),
            sample.replace("<scenario>", r#"<scenario><xbrldi:typedMember dimension="dim:foo"><v>x</v></xbrldi:typedMember>"#),
            sample.replace(r#"<identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2024-06-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m53"#, r#"</entity><period><instant>2024-06-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m53"#),
            sample.replace("http://www.fgov.be", "urn:spoof"),
            sample.replace("http://www.xbrl.org/2003/instance", "urn:spoof"),
            sample.replace("http://www.nbb.be/be/fr/cbso/dict/met", "urn:spoof"),
            sample.replace("2023-07-01", "2023-08-01"),
            sample.replace("2024-06-30</met:dte1>", "2024-07-01</met:dte1>"),
            sample.replace("<instant>2024-06-30</instant>", "<instant>2024-06-29</instant>"),
            sample.replace("</xbrl>", r#"<met:am2 contextRef="c1" unitRef="EUR">999</met:am2></xbrl>"#),
            DIM_SAMPLE.to_owned(), CONCEPT_SAMPLE.to_owned(),
        ] {
            assert!(strict_parse(&bad).is_err(), "unexpected acceptance: {bad}");
        }
    }

    #[test]
    fn strict_units_resolve_qnames_instead_of_prefix_names() {
        let sample = strict_sample();
        let renamed = sample
            .replace("xmlns:money=", "xmlns:cash=")
            .replace("money:EUR", "cash:EUR");
        assert!(strict_parse(&renamed).is_ok());
        let fte_ctx = r#"<context id="staff"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2024-06-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m73</xbrldi:explicitMember><xbrldi:explicitMember xmlns:typ="http://www.nbb.be/be/fr/cbso/dict/dom/typ" dimension="dim:typ">typ:m16</xbrldi:explicitMember><xbrldi:explicitMember xmlns:qlt="http://www.nbb.be/be/fr/cbso/dict/dom/qlt" dimension="dim:qlt">qlt:m3</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:part">part:m6</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m1</xbrldi:explicitMember><xbrldi:explicitMember xmlns:wkr="http://www.nbb.be/be/fr/cbso/dict/dom/wkr" dimension="dim:wkr">wkr:m1</xbrldi:explicitMember></scenario></context><met:dec1 contextRef="staff" unitRef="pure">165.5</met:dec1>"#;
        let fte = sample.replace("</xbrl>", &format!("{fte_ctx}</xbrl>"));
        for (from, to) in [
            ("typ:m16", "typ:m17"),
            ("qlt:m3", "qlt:m4"),
            ("part:m6", "part:m4"),
        ] {
            let bad = fte.replace(from, to);
            assert!(strict_parse(&bad).is_err());
            assert!(parse_consult_xbrl(&bad, "legacy").is_ok());
        }
        for dimension in ["typ", "qlt", "part"] {
            let marker = format!("dimension=\"dim:{dimension}\"");
            let marker_start = fte.find(&marker).unwrap();
            let start = fte[..marker_start].rfind("<xbrldi:explicitMember").unwrap();
            let end = marker_start
                + fte[marker_start..]
                    .find("</xbrldi:explicitMember>")
                    .unwrap()
                + "</xbrldi:explicitMember>".len();
            let mut bad = fte.clone();
            bad.replace_range(start..end, "");
            assert!(strict_parse(&bad).is_err());
            assert!(parse_consult_xbrl(&bad, "legacy").is_ok());
        }
        let rows = strict_parse(&fte).unwrap();
        assert_eq!(
            rows[1].contributors["9087"][0]
                .unit_measure
                .as_ref()
                .unwrap()
                .namespace,
            XBRLI
        );
        assert!(strict_parse(&fte.replace(r#"unitRef="pure""#, r#"unitRef="EUR""#)).is_err());
    }

    fn debt_fact(id: &str, prd: &str, dims: &[(&str, &str)], value: &str) -> String {
        let members: String = [("bas", "m50"), ("prd", prd)].iter().chain(dims.iter())
            .map(|(d, m)| format!(r#"<xbrldi:explicitMember xmlns:{d}="http://www.nbb.be/be/fr/cbso/dict/dom/{d}" dimension="dim:{d}">{d}:{m}</xbrldi:explicitMember>"#)).collect();
        format!(
            r#"<context id="{id}"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2024-06-30</instant></period><scenario>{members}</scenario></context><met:am1 id="fact-{id}" contextRef="{id}" unitRef="EUR" decimals="2">{value}</met:am1>"#
        )
    }

    const DEBT_SHAPES: &[(&str, &[(&str, &str)])] = &[
        ("17", &[("part", "m3"), ("rst", "m1")]),
        ("42", &[("part", "m3"), ("rst", "m3")]),
        ("170/4", &[("ntr", "m3"), ("part", "m3"), ("rst", "m1")]),
        ("43", &[("ntr", "m3"), ("part", "m3"), ("rst", "m2")]),
        ("8801", &[("ntr", "m3"), ("part", "m6"), ("rst", "m3")]),
    ];

    #[test]
    fn strict_debt_isolates_codes_periods_and_evidence_without_legacy_changes() {
        let sample = strict_sample();
        let baseline = parse_consult_xbrl(&sample, "capture").unwrap();
        for (code, dims) in DEBT_SHAPES {
            for (prd, year) in [("m1", 2024), ("m2", 2023)] {
                let fact = debt_fact("debt", prd, dims, "1234.50");
                let text = sample.replace("</xbrl>", &format!("{fact}</xbrl>"));
                let rows = strict_parse(&text).unwrap();
                for row in &rows {
                    for (other, _) in DEBT_SHAPES {
                        assert_eq!(
                            row.filing.values.get(*other).copied(),
                            (*other == *code && row.filing.fiscal_year == year)
                                .then_some(Decimal::new(123450, 2)),
                            "{code} {prd} -> {other}"
                        );
                    }
                    if row.filing.fiscal_year == year {
                        let sources = &row.contributors[*code];
                        assert_eq!(sources.len(), 1);
                        assert_eq!(sources[0].id.as_deref(), Some("fact-debt"));
                        assert_eq!(sources[0].context_ref, "debt");
                        assert_eq!(sources[0].raw_value, "1234.50");
                        assert_eq!(sources[0].unit_measure.as_ref().unwrap().local, "EUR");
                    }
                }
                let unchanged = parse_consult_xbrl(&text, "capture").unwrap();
                assert_eq!(baseline.len(), unchanged.len());
                for (before, after) in baseline.iter().zip(unchanged.iter()) {
                    assert_eq!(before.values, after.values);
                }
            }
        }
    }

    #[test]
    fn strict_debt_excludes_breakdowns_wrong_members_metrics_and_periods() {
        let sample = strict_sample();
        let baseline = strict_parse(&sample).unwrap();
        for (code, dims) in DEBT_SHAPES {
            let mut excluded = Vec::new();
            for extra in ["spec", "sts", "ntr", "other"] {
                if dims.iter().any(|(d, _)| *d == extra) {
                    continue;
                }
                let mut altered = dims.to_vec();
                altered.push((extra, "m99"));
                excluded.push(debt_fact("excluded", "m1", &altered, "987.00"));
            }
            for index in 0..dims.len() {
                let mut altered = dims.to_vec();
                altered[index].1 = "m99";
                excluded.push(debt_fact("excluded", "m1", &altered, "987.00"));
                let mut altered = dims.to_vec();
                altered.remove(index);
                let fact = debt_fact("missing", "m1", &altered, "987.00");
                let rows =
                    strict_parse(&sample.replace("</xbrl>", &format!("{fact}</xbrl>"))).unwrap();
                // Without ntr, 170/4 becomes the independently valid 17 shape.
                assert!(!rows[1].filing.values.contains_key(*code));
            }
            excluded.push(debt_fact("excluded", "m3", dims, "987.00"));
            excluded
                .push(debt_fact("excluded", "m1", dims, "987.00").replace("met:am1", "met:am2"));
            for fact in excluded {
                let rows =
                    strict_parse(&sample.replace("</xbrl>", &format!("{fact}</xbrl>"))).unwrap();
                for (before, after) in baseline.iter().zip(rows.iter()) {
                    assert_eq!(before.filing.values, after.filing.values, "{fact}");
                }
            }
        }
    }

    #[test]
    fn strict_debt_requires_eur_and_detects_conflicting_contributors() {
        for (code, dims) in DEBT_SHAPES {
            let fact = debt_fact("first", "m1", dims, "123.00");
            for bad in [
                fact.replace(r#"unitRef="EUR""#, r#"unitRef="pure""#),
                fact.replace(r#"unitRef="EUR""#, r#"unitRef="missing""#),
                fact.replace(r#"unitRef="EUR""#, ""),
            ] {
                let text = strict_sample().replace("</xbrl>", &format!("{bad}</xbrl>"));
                assert!(
                    strict_parse(&text).err().unwrap().contains("unit"),
                    "{code}"
                );
            }
            let duplicate = debt_fact("second", "m1", dims, "123.00");
            let text = strict_sample().replace("</xbrl>", &format!("{fact}{duplicate}</xbrl>"));
            assert_eq!(strict_parse(&text).unwrap()[1].contributors[*code].len(), 2);
            let conflict = text.replace(">123.00</met:am1></xbrl>", ">124.00</met:am1></xbrl>");
            assert!(strict_parse(&conflict)
                .err()
                .unwrap()
                .contains(&format!("conflicting {code}")));
        }
    }

    fn date_metadata(prd: &str, mmt: &str, date: &str, extra: &str) -> String {
        format!(
            r#"<context id="d-{prd}-{mmt}-{date}"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2024-06-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m27</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:part">part:m2</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:{prd}</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:mmt">mmt:{mmt}</xbrldi:explicitMember>{extra}</scenario></context><met:dte1 contextRef="d-{prd}-{mmt}-{date}">{date}</met:dte1>"#
        )
    }

    fn with_date_metadata(metadata: &str) -> String {
        DIM_SAMPLE.replace("<xbrl xmlns=", r#"<xbrl xmlns:met="http://www.nbb.be/be/fr/cbso/dict/met" xmlns:dim="http://www.nbb.be/be/fr/cbso/dict/dim" xmlns:bas="http://www.nbb.be/be/fr/cbso/dict/dom/bas" xmlns:part="http://www.nbb.be/be/fr/cbso/dict/dom/part" xmlns:prd="http://www.nbb.be/be/fr/cbso/dict/dom/prd" xmlns:mmt="http://www.nbb.be/be/fr/cbso/dict/dom/mmt" xmlns="#)
            .replace("</xbrl>", &format!("{metadata}</xbrl>"))
    }

    #[test]
    fn declared_periods_preserve_non_calendar_and_nonconsecutive_years() {
        let metadata = [
            date_metadata("m1", "m1", "2023-07-01", ""),
            date_metadata("m1", "m2", "2024-06-30", ""),
            date_metadata("m2", "m1", "2022-01-01", ""),
            date_metadata("m2", "m2", "2022-12-31", ""),
        ]
        .concat();
        let filings = parse_consult_xbrl(&with_date_metadata(&metadata), "synthetic").unwrap();
        assert_eq!(filings.len(), 2);
        assert_eq!(
            (
                filings[0].fiscal_year,
                filings[0].period_start.as_str(),
                filings[0].period_end.as_str()
            ),
            (2022, "2022-01-01", "2022-12-31")
        );
        assert_eq!(
            (
                filings[1].fiscal_year,
                filings[1].period_start.as_str(),
                filings[1].period_end.as_str()
            ),
            (2024, "2023-07-01", "2024-06-30")
        );
        assert_eq!(filings[0].values["70"], Decimal::new(1326168256, 2));
        assert_eq!(filings[1].values["70"], Decimal::new(2068744371, 2));
    }

    #[test]
    fn declared_periods_do_not_invent_missing_boundaries() {
        let metadata = date_metadata("m1", "m2", "2024-06-30", "");
        let filings = parse_consult_xbrl(&with_date_metadata(&metadata), "synthetic").unwrap();
        assert!(filings.iter().all(|f| f.period_start.is_empty()));
        assert!(filings[0].period_end.is_empty());
    }

    #[test]
    fn declared_period_conflicts_and_invalid_dates_abort() {
        for metadata in [
            [
                date_metadata("m1", "m1", "2023-07-01", ""),
                date_metadata("m1", "m1", "2023-08-01", ""),
            ]
            .concat(),
            date_metadata("m1", "m1", "2023-02-29", ""),
            date_metadata("m1", "m1", "+10000-01-01", ""),
            [
                date_metadata("m1", "m1", "2024-07-01", ""),
                date_metadata("m1", "m2", "2024-06-30", ""),
            ]
            .concat(),
        ] {
            assert!(parse_consult_xbrl(&with_date_metadata(&metadata), "synthetic").is_err());
        }
    }

    #[test]
    fn scoped_or_spoofed_dates_cannot_supply_boundaries() {
        let metadata = [
            date_metadata("m1", "m1", "2023-07-01", ""),
            date_metadata("m1", "m2", "2024-06-30", ""),
        ]
        .concat();
        for xbrl in [
            with_date_metadata(&metadata)
                .replace("http://www.nbb.be/be/fr/cbso/dict/met", "urn:decoy:met"),
            with_date_metadata(&metadata)
                .replace("http://www.nbb.be/be/fr/cbso/dict/dom/mmt", "urn:decoy:mmt"),
            with_date_metadata(&metadata)
                .replace("http://www.nbb.be/be/fr/cbso/dict/dim", "urn:decoy:dim"),
            with_date_metadata(&date_metadata(
                "m1",
                "m1",
                "2023-07-01",
                r#"<xbrldi:explicitMember dimension="dim:evt">evt:m1</xbrldi:explicitMember>"#,
            )),
        ] {
            let filings = parse_consult_xbrl(&xbrl, "synthetic").unwrap();
            assert!(filings.iter().all(|f| f.period_start.is_empty()));
        }
    }

    #[test]
    fn declared_periods_cannot_collapse_into_one_fiscal_year() {
        let metadata = [
            date_metadata("m1", "m1", "2024-02-01", ""),
            date_metadata("m1", "m2", "2024-06-30", ""),
            date_metadata("m2", "m1", "2023-07-01", ""),
            date_metadata("m2", "m2", "2024-01-31", ""),
        ]
        .concat();
        assert!(parse_consult_xbrl(&with_date_metadata(&metadata), "synthetic").is_err());
    }

    #[test]
    fn declared_current_end_must_match_mapped_current_instants() {
        let metadata = [
            date_metadata("m1", "m1", "2023-07-01", ""),
            date_metadata("m1", "m2", "2024-05-31", ""),
        ]
        .concat();
        assert!(
            parse_consult_xbrl(&with_date_metadata(&metadata), "synthetic")
                .unwrap_err()
                .contains("instant disagrees")
        );
    }

    #[test]
    fn verified_periods_are_not_attached_to_spoofed_value_dimensions() {
        let metadata = [
            date_metadata("m1", "m1", "2023-07-01", ""),
            date_metadata("m1", "m2", "2024-06-30", ""),
        ]
        .concat();
        let xbrl = with_date_metadata(&metadata).replace(
            r#"<context id="c1">"#,
            r#"<context id="c1" xmlns:bas="urn:decoy:bas">"#,
        );
        let filings = parse_consult_xbrl(&xbrl, "synthetic").unwrap();
        let current = filings.iter().find(|f| f.fiscal_year == 2024).unwrap();
        assert!(!current.values.contains_key("70"));
        assert!(current.values.contains_key("9901"));
    }

    #[test]
    fn identical_declared_dates_agree_and_partial_periods_stay_unknown() {
        let start = date_metadata("m1", "m1", "2023-07-01", "");
        let duplicate = start.replace("d-m1-m1-2023-07-01", "duplicate-start");
        let metadata = [
            start,
            duplicate,
            date_metadata("m1", "m2", "2024-06-30", ""),
        ]
        .concat();
        assert!(parse_consult_xbrl(&with_date_metadata(&metadata), "synthetic").is_ok());
        let prior_only = [
            date_metadata("m2", "m1", "2022-01-01", ""),
            date_metadata("m2", "m2", "2022-12-31", ""),
            date_metadata("m1", "m1", "2023-07-01", ""),
        ]
        .concat();
        let filings = parse_consult_xbrl(&with_date_metadata(&prior_only), "synthetic").unwrap();
        assert_eq!(filings[0].period_start, "2022-01-01");
        assert_eq!(filings[0].fiscal_year, 2022);
        assert!(filings[1].period_start.is_empty());
    }

    #[test]
    fn duplicate_context_ids_and_dimensions_abort() {
        let duplicate_id = DIM_SAMPLE.replace("</xbrl>", r#"<context id="c1"></context></xbrl>"#);
        assert!(parse_consult_xbrl(&duplicate_id, "synthetic")
            .unwrap_err()
            .contains("duplicate XBRL context identifier"));
        let duplicate_dim = DIM_SAMPLE.replace("</scenario>", r#"<xbrldi:explicitMember dimension="dim:prd">prd:m2</xbrldi:explicitMember></scenario>"#);
        assert!(parse_consult_xbrl(&duplicate_dim, "synthetic")
            .unwrap_err()
            .contains("duplicate XBRL context dimension"));
    }

    #[test]
    fn dimensional_model_yields_both_periods() {
        let filings = parse_consult_xbrl(DIM_SAMPLE, "2024-00489680").expect("sample parses");
        assert_eq!(filings.len(), 2);
        assert_eq!(filings[0].fiscal_year, 2023);
        assert_eq!(filings[0].period_end, "");
        assert_eq!(
            filings[0].values.get("70"),
            Some(&Decimal::new(1326168256, 2))
        );
        assert!(!filings[0].values.contains_key("9901"));
        assert_eq!(filings[1].fiscal_year, 2024);
        assert_eq!(filings[1].period_end, "2024-06-30");
        assert_eq!(
            filings[1].values.get("70"),
            Some(&Decimal::new(2068744371, 2))
        );
        assert_eq!(
            filings[1].values.get("9901"),
            Some(&Decimal::new(5170055, 2))
        );
        assert!(filings.iter().all(|f| f.source_ref == "2024-00489680"));
    }

    #[test]
    fn verified_am1_turnover_requires_the_matching_statement_dimensions() {
        let xbrl = r#"<?xml version="1.0"?>
<xbrl xmlns="http://www.xbrl.org/2003/instance" xmlns:xbrldi="http://xbrl.org/2006/xbrldi" xmlns:met="urn:nbb:met">
<context id="current"><entity><identifier scheme="http://www.fgov.be">0862542014</identifier></entity><period><instant>2025-12-31</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m53</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:ntr">ntr:m17</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:part">part:m4</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m1</xbrldi:explicitMember></scenario></context>
<context id="previous"><entity><identifier scheme="http://www.fgov.be">0862542014</identifier></entity><period><instant>2025-12-31</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m53</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:ntr">ntr:m17</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:part">part:m4</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m2</xbrldi:explicitMember></scenario></context>
<context id="other-statement"><entity><identifier scheme="http://www.fgov.be">0862542014</identifier></entity><period><instant>2025-12-31</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m53</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:ntr">ntr:m18</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:part">part:m4</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m1</xbrldi:explicitMember></scenario></context>
<met:am1 contextRef="current" unitRef="EUR" decimals="INF">550204</met:am1>
<met:am1 contextRef="previous" unitRef="EUR" decimals="INF">574895</met:am1>
<met:am1 contextRef="other-statement" unitRef="EUR" decimals="INF">999999</met:am1>
</xbrl>"#;
        let filings = parse_consult_xbrl(xbrl, "2026-00538678").expect("sample parses");
        let prior = filings.iter().find(|f| f.fiscal_year == 2024).unwrap();
        let current = filings.iter().find(|f| f.fiscal_year == 2025).unwrap();
        assert_eq!(prior.values.get("70"), Some(&Decimal::new(574895, 0)));
        assert_eq!(prior.period_end, "");
        assert_eq!(current.values.get("70"), Some(&Decimal::new(550204, 0)));
        assert_eq!(current.period_end, "2025-12-31");
    }

    #[test]
    fn dimensional_comparative_end_is_kept_when_the_context_states_it() {
        let xbrl = DIM_SAMPLE.replace(
            "<context id=\"c2\"><entity><identifier scheme=\"http://www.fgov.be\">0537875391</identifier></entity><period><instant>2024-06-30",
            "<context id=\"c2\"><entity><identifier scheme=\"http://www.fgov.be\">0537875391</identifier></entity><period><instant>2023-06-30",
        );
        let filings = parse_consult_xbrl(&xbrl, "explicit-prior-end").expect("sample parses");
        let prior = filings
            .iter()
            .find(|filing| filing.fiscal_year == 2023)
            .expect("comparative period retained");
        assert_eq!(prior.period_end, "2023-06-30");
    }

    #[test]
    fn dimensional_contributed_capital_is_not_total_equity() {
        let xbrl = DIM_SAMPLE.replace(
            "</xbrl>",
            r#"<context id="equity"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2026-04-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m37</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:ntr">ntr:m4</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:part">part:m3</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m1</xbrldi:explicitMember></scenario></context>
<context id="contribution"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2026-04-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:m38</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:ntr">ntr:m4</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:part">part:m3</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:m1</xbrldi:explicitMember></scenario></context>
<met:am2 contextRef="equity" unitRef="EUR" decimals="INF">31181135.16</met:am2>
<met:am1 contextRef="contribution" unitRef="EUR" decimals="INF">2536000</met:am1>"#,
        );
        let filings = parse_consult_xbrl(&xbrl, "verified-capital-versus-equity")
            .expect("distinct equity and contribution facts must not conflict");
        let current = filings
            .iter()
            .find(|filing| filing.fiscal_year == 2026)
            .expect("current period retained");
        assert_eq!(
            current.values.get("10/15"),
            Some(&Decimal::new(3_118_113_516, 2))
        );
    }

    #[test]
    fn concept_model_uses_explicit_period_dates() {
        let filings = parse_consult_xbrl(CONCEPT_SAMPLE, "2021-71900090").expect("sample parses");
        assert_eq!(filings.len(), 2);
        assert_eq!(filings[0].fiscal_year, 2020);
        assert_eq!(filings[0].period_start, "2019-07-01");
        assert_eq!(
            filings[0].values.get("70"),
            Some(&Decimal::new(2058053672, 2))
        );
        assert_eq!(filings[1].fiscal_year, 2021);
        assert_eq!(
            filings[1].values.get("70"),
            Some(&Decimal::new(1819362914, 2))
        );
        assert_eq!(
            filings[1].values.get("9901"),
            Some(&Decimal::new(10444242, 2))
        );
        // BE-prefixed identifier normalizes to the plain KBO.
        assert!(filings.iter().all(|f| f.enterprise_number == "0537875391"));
    }

    #[test]
    fn conflicting_breakdowns_abort_the_file() {
        // same code+period now disagrees with itself: add a duplicate fact.
        let bad = DIM_SAMPLE.replace(
            "<met:str1",
            "<met:am2 contextRef=\"c2\" unitRef=\"EUR\" decimals=\"INF\">13261682.57</met:am2><met:str1",
        );
        let err = parse_consult_xbrl(&bad, "2024-00489680").expect_err("must abort");
        assert!(err.contains("conflicting 70"), "unexpected error: {err}");
    }

    #[test]
    fn sts_qualified_sub_views_are_ignored() {
        // c2 gains an sts qualifier and a divergent value: it must be
        // ignored instead of conflicting with the headline fact.
        let with_subview = DIM_SAMPLE.replace(
            "prd:m2</xbrldi:explicitMember></scenario></context>",
            "prd:m2</xbrldi:explicitMember><xbrldi:explicitMember dimension=\"dim:sts\">sts:m11</xbrldi:explicitMember></scenario></context>",
        );
        let with_subview = with_subview.replace(">13261682.56</met:am2>", ">99999999.99</met:am2>");
        let filings = parse_consult_xbrl(&with_subview, "2024-00489680")
            .expect("sts sub-view must not conflict");
        // c2 was FY2023's only 70 fact: excluded, so no FY2023 row, and the
        // divergent value never touches FY2024.
        assert_eq!(filings.len(), 1);
        assert_eq!(filings[0].fiscal_year, 2024);
        assert_eq!(
            filings[0].values.get("70"),
            Some(&Decimal::new(2068744371, 2))
        );
    }

    #[test]
    fn rounded_figures_abort_instead_of_scaling_silently() {
        let rounded = DIM_SAMPLE.replace("decimals=\"INF\">20687443.71<", "decimals=\"-3\">20687<");
        let err = parse_consult_xbrl(&rounded, "2024-00489680").expect_err("must abort");
        assert!(err.contains("rounded to -3"), "unexpected error: {err}");
    }

    #[test]
    fn filings_without_mappable_facts_are_refused() {
        let err = parse_consult_xbrl(
            r#"<?xml version="1.0"?><xbrl xmlns="http://www.xbrl.org/2003/instance"><context id="c1"><entity><identifier scheme="http://www.fgov.be">0537875391</identifier></entity><period><instant>2024-06-30</instant></period></context></xbrl>"#,
            "empty",
        )
        .expect_err("must refuse");
        assert!(err.contains("no mappable facts"), "unexpected error: {err}");
    }
}
