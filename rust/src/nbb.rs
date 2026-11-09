//! NBB Consult CSV ingestion — the manual bridge until API access lands.
//!
//! Format (NBB-generated, rigid): `"key","value"` header rows plus `S` rows
//! `"S","<code>","<period>","<value>"`. Only period `"1"` (current) rows are
//! read. Unknown codes are ignored; a malformed `S` row is a hard error —
//! format drift must alarm, never silently parse.
//!
//! Mapping (Belgian standard chart; 9903 deliberately unmapped — its label is
//! ambiguous, and an ambiguous code is worse than a missing one):
//! `70` turnover (Omzet) · `9087` average FTE · `10/15` equity (Eigen vermogen)
//! `9901` operating profit (Bedrijfswinst). Verify against the first real
//! filing's long-form labels before trusting a new code.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::store::NbbFinancials;

/// Parse one Consult CSV export. `source_ref` is the filing reference
/// (normally the filename stem, e.g. `2024-00489680`).
///
/// Consult quirk (verified euro-exact against the filed PDFs): the main
/// statement codes (70, 9087, 9901, 9903) carry the *previous* period's
/// figures, not the header period's — they match the PDF's *Vorig boekjaar*
/// column. Social-balance *detail* rows (1011, 1021, ...) instead match the
/// current-period breakdown tables, so the export is mixed BY SECTION, not
/// uniformly shifted. Only the four mapped codes are trusted (all verified
/// previous-period); any new code needs its own PDF check first.
/// `fiscal_year` and `period_start/end` therefore describe the previous
/// period for the mapped codes; `source_ref` still identifies the filing.
pub fn parse_consult_csv(text: &str, source_ref: &str) -> Result<NbbFiling, String> {
    let mut header: BTreeMap<String, String> = BTreeMap::new();
    let mut values: BTreeMap<String, Decimal> = BTreeMap::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_quoted(line)
            .ok_or_else(|| format!("line {}: not a quoted record: {line:?}", lineno + 1))?;
        match fields.as_slice() {
            [key, value] => {
                header.insert(key.clone(), value.clone());
            }
            [tag, code, period, value] if tag == "S" => {
                if period != "1" {
                    continue;
                }
                let amount: Decimal = value
                    .parse()
                    .map_err(|_| format!("line {}: amount not decimal: {value:?}", lineno + 1))?;
                values.insert(code.clone(), amount);
            }
            _ => {
                return Err(format!(
                    "line {}: unexpected field count {}: {line:?}",
                    lineno + 1,
                    fields.len()
                ));
            }
        }
    }
    let enterprise_number = header
        .get("enterpriseNumber")
        .ok_or("missing enterpriseNumber header".to_string())?;
    // The mapped codes below are the previous period's (see doc comment),
    // so the year comes from previousPeriodEndDate. A filing without
    // comparatives has no defined meaning here: fail loud rather than
    // mislabel the year.
    let prev_end = header
        .get("previousPeriodEndDate")
        .ok_or("missing previousPeriodEndDate header".to_string())?;
    let fiscal_year: i64 = prev_end
        .get(..4)
        .and_then(|y| y.parse().ok())
        .ok_or_else(|| format!("bad previousPeriodEndDate: {prev_end:?}"))?;
    Ok(NbbFiling {
        enterprise_number: enterprise_number.clone(),
        enterprise_name: header.get("enterpriseName").cloned().unwrap_or_default(),
        period_start: header
            .get("previousPeriodStartDate")
            .cloned()
            .unwrap_or_default(),
        period_end: prev_end.clone(),
        fiscal_year,
        values,
        source_ref: source_ref.to_owned(),
    })
}

/// Split a `"a","b"` line respecting quoted commas and `""` escapes.
/// Anything else is `None`: the format is machine-generated, so deviation
/// is drift, not dialect.
fn split_quoted(line: &str) -> Option<Vec<String>> {
    let inner = line.strip_prefix('"')?.strip_suffix('"')?;
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            match chars.peek() {
                // `""` inside a field is an escaped quote.
                Some('"') => {
                    chars.next();
                    cur.push('"');
                }
                // `","` ends the field iff a new quoted field follows.
                Some(',') => {
                    chars.next();
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        fields.push(std::mem::take(&mut cur));
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        } else {
            cur.push(c);
        }
    }
    fields.push(cur);
    Some(fields)
}

#[derive(Debug, Clone, PartialEq)]
pub struct NbbFiling {
    pub enterprise_number: String,
    pub enterprise_name: String,
    pub period_start: String,
    pub period_end: String,
    pub fiscal_year: i64,
    pub values: BTreeMap<String, Decimal>,
    pub source_ref: String,
}

fn code(values: &BTreeMap<String, Decimal>, key: &str) -> Option<Decimal> {
    values.get(key).copied()
}

/// Map a parsed filing onto the store row. Unmapped codes stay out;
/// a missing field is NULL (unknown), never zero.
pub fn to_financials(filing: &NbbFiling, method: &str, source_format: &str) -> NbbFinancials {
    NbbFinancials {
        enterprise_kbo: filing.enterprise_number.clone(),
        fiscal_year: filing.fiscal_year,
        period_start: (!filing.period_start.is_empty()).then(|| filing.period_start.clone()),
        period_end: (!filing.period_end.is_empty()).then(|| filing.period_end.clone()),
        turnover: code(&filing.values, "70"),
        staff_fte: code(&filing.values, "9087"),
        equity: code(&filing.values, "10/15"),
        result: code(&filing.values, "9901"),
        source_ref: filing.source_ref.clone(),
        source_format: source_format.to_owned(),
        acquisition_method: method.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\"key\",\"N\"\n\"enterpriseNumber\",\"0123456789\"\n\"enterpriseName\",\"Example BV, Antwerp\"\n\"periodStartDate\",\"2023-07-01\"\n\"periodEndDate\",\"2024-06-30\"\n\"previousPeriodStartDate\",\"2022-07-01\"\n\"previousPeriodEndDate\",\"2023-06-30\"\n\"S\",\"70\",\"1\",\"13261682.56\"\n\"S\",\"9087\",\"1\",\"10.1\"\n\"S\",\"10/15\",\"1\",\"18550\"\n\"S\",\"9901\",\"1\",\"27923.49\"\n\"S\",\"9999\",\"1\",\"1\"\n\"S\",\"70\",\"2\",\"999\"\n";

    #[test]
    fn parses_header_and_previous_period_values() {
        let f = parse_consult_csv(SAMPLE, "2024-00001").expect("sample should parse");
        assert_eq!(f.enterprise_number, "0123456789");
        assert_eq!(f.enterprise_name, "Example BV, Antwerp");
        // rubric values are the previous period's: year comes from
        // previousPeriodEndDate, not the header period
        assert_eq!(f.fiscal_year, 2023);
        assert_eq!(f.period_start, "2022-07-01");
        assert_eq!(f.period_end, "2023-06-30");
        // quoted comma in the name survives; period-"2" code-70 row ignored
        assert_eq!(f.values.get("70"), Some(&Decimal::new(1326168256, 2)));
        assert_eq!(f.values.get("9087"), Some(&Decimal::new(101, 1)));
        assert!(f.values.contains_key("9999")); // unknown codes kept, mapping drops them
    }

    #[test]
    fn maps_codes_and_leaves_missing_null() {
        let f = parse_consult_csv(SAMPLE, "2024-00001").expect("sample should parse");
        let fin = to_financials(&f, "manual-consult", "csv");
        assert_eq!(fin.turnover, Some(Decimal::new(1326168256, 2)));
        assert_eq!(fin.staff_fte, Some(Decimal::new(101, 1)));
        assert_eq!(fin.equity, Some(Decimal::new(18550, 0)));
        assert_eq!(fin.result, Some(Decimal::new(2792349, 2)));
        assert_eq!(fin.period_start.as_deref(), Some("2022-07-01"));
        assert_eq!(fin.period_end.as_deref(), Some("2023-06-30"));
        assert_eq!(fin.acquisition_method, "manual-consult");
    }

    #[test]
    fn rejects_malformed_rows_loudly() {
        assert!(parse_consult_csv("\"key\",\"N\"\nBROKEN LINE\n", "x").is_err());
        assert!(parse_consult_csv("\"key\",\"N\"\n\"S\",\"70\",\"1\"\n", "x").is_err());
        assert!(parse_consult_csv("\"key\",\"N\"\n\"S\",\"70\",\"1\",\"abc\"\n", "x").is_err());
        assert!(parse_consult_csv("\"enterpriseNumber\",\"1\"\n", "x").is_err());
    }
}
