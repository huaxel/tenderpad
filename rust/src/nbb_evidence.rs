//! Offline strict evidence extraction from a trusted, paired v2 bundle.
//! Missing codes remain absent. Hashes pin bytes, not source authenticity or
//! financial correctness. As with bundle verification, do not mutate while read.
use crate::{
    nbb_bundle::{verify_bundle, BundleResult, Manifest, RESTRICTION},
    nbb_xbrl::{parse_consult_xbrl_with_evidence, ExpandedName, ExpectedFiling, FactEvidence},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs::OpenOptions, io::Read, path::Path};

const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

fn read_regular(path: &Path, limit: u64) -> BundleResult<Vec<u8>> {
    if !path.symlink_metadata()?.file_type().is_file() {
        return Err("evidence file must be regular, not a symlink".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > limit {
        return Err("invalid evidence file type or size".into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err("evidence file exceeds size limit".into());
    }
    Ok(bytes)
}
fn name(name: &ExpandedName) -> Value {
    json!({"namespace": name.namespace, "local": name.local})
}
fn fact(f: &FactEvidence) -> Value {
    json!({
        "id": f.id, "ordinal": f.ordinal, "metric": name(&f.metric),
        "context_ref": f.context_ref,
        "context": {
            "identifier": f.context.identifier,
            "identifier_scheme": f.context.identifier_scheme,
            "instant": f.context.instant, "start": f.context.start, "end": f.context.end,
            "typed_dimensions": f.context.typed_dimensions,
            "dimensions": f.context.dimensions.iter().map(|(d, m)|
                json!({"dimension": name(d), "member": name(m)})).collect::<Vec<_>>()
        },
        "raw_value": f.raw_value, "decimals": f.decimals,
        "unit_ref": f.unit_ref, "unit_measure": f.unit_measure.as_ref().map(name)
    })
}
/// Verify the caller's lowercase SHA-256 pin before interpreting the manifest.
/// The full JSON is returned only after every filing has passed strict parsing.
/// Values are exact decimal strings; unsupported/missing codes are absent.
pub fn extract_bundle(path: &Path, expected_manifest_sha256: &str) -> BundleResult<Value> {
    if expected_manifest_sha256.len() != 64
        || !expected_manifest_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("expected manifest SHA-256 must be 64 lowercase hex characters".into());
    }
    if !path.symlink_metadata()?.file_type().is_dir() {
        return Err("bundle must be a directory, not a symlink".into());
    }
    let bytes = read_regular(&path.join("manifest.json"), MAX_MANIFEST_BYTES)?;
    if format!("{:x}", Sha256::digest(&bytes)) != expected_manifest_sha256 {
        return Err("trusted manifest SHA-256 mismatch".into());
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    if manifest.version != 2 {
        return Err("strict evidence requires a paired v2 bundle; no PDF fallback".into());
    }
    let verified = verify_bundle(path)?;
    if serde_json::to_value(&verified)? != serde_json::to_value(&manifest)? {
        return Err("manifest changed during verification".into());
    }
    let mut filings = Vec::new();
    for filing in &manifest.files {
        let artifact = filing.xbrl.as_ref().ok_or("missing paired XBRL")?;
        let bytes = read_regular(&path.join(&artifact.filename), MAX_ARTIFACT_BYTES)?;
        if bytes.len() as u64 != artifact.receipt.bytes
            || format!("{:x}", Sha256::digest(&bytes)) != artifact.receipt.sha256
        {
            return Err("XBRL changed before strict extraction".into());
        }
        let rows = parse_consult_xbrl_with_evidence(
            std::str::from_utf8(&bytes)?,
            &filing.reference,
            ExpectedFiling {
                enterprise_number: &manifest.enterprise,
                current_start: &filing.period.start,
                current_end: &filing.period.end,
            },
        )?;
        let periods: Vec<_> = rows.iter().map(|row| {
            let values: serde_json::Map<String, Value> = row.filing.values.iter()
                .map(|(code, value)| (code.clone(), Value::String(value.to_string()))).collect();
            let contributors: serde_json::Map<String, Value> = row.contributors.iter()
                .map(|(code, facts)| (code.clone(), Value::Array(facts.iter().map(fact).collect()))).collect();
            json!({
                "period_kind": if row.filing.period_start == filing.period.start && row.filing.period_end == filing.period.end {"current"} else {"comparative"},
                "period_start": row.filing.period_start, "period_end": row.filing.period_end,
                "fiscal_year": row.filing.fiscal_year, "values": values,
                "contributors": contributors,
                "declared_dates": row.declared_dates.iter().map(fact).collect::<Vec<_>>()
            })
        }).collect();
        filings.push(json!({
            "reference": filing.reference, "enterprise": manifest.enterprise,
            "manifest_sha256": expected_manifest_sha256,
            "pdf": {"filename": filing.filename, "receipt": filing.receipt},
            "xbrl": {"filename": artifact.filename, "receipt": artifact.receipt},
            "periods": periods
        }));
    }
    Ok(json!({
        "schema_version": 1, "restriction": RESTRICTION,
        "supported_codes": ["70", "10/15", "9901", "9087", "17", "42", "170/4", "43", "8801"],
        "missing_values": "absent; never zero-filled",
        "manifest_sha256": expected_manifest_sha256,
        "enterprise": manifest.enterprise, "filings": filings
    }))
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "network")]
    use super::*;
    #[cfg(feature = "network")]
    use crate::{
        nbb_api::{Capture, FilingReference, NbbError},
        nbb_bundle::{create_bundle, create_bundle_with_xbrl, Source},
    };
    #[cfg(feature = "network")]
    use std::{fs, path::PathBuf};
    #[cfg(feature = "network")]
    use uuid::Uuid;

    #[cfg(feature = "network")]
    struct Temp(PathBuf);
    #[cfg(feature = "network")]
    impl Temp {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("nbb-evidence-test-{}", Uuid::new_v4())))
        }
        fn pin(&self) -> String {
            format!(
                "{:x}",
                Sha256::digest(fs::read(self.0.join("manifest.json")).unwrap())
            )
        }
    }
    #[cfg(feature = "network")]
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(feature = "network")]
    fn context(id: &str, bas: &str, prd: &str, extra: &str) -> String {
        format!(
            r#"<context id="{id}"><entity><identifier scheme="http://www.fgov.be">0123456789</identifier></entity><period><instant>2024-06-30</instant></period><scenario><xbrldi:explicitMember dimension="dim:bas">bas:{bas}</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:prd">prd:{prd}</xbrldi:explicitMember>{extra}</scenario></context>"#
        )
    }
    #[cfg(feature = "network")]
    fn xml() -> String {
        let mut text = r#"<xbrl xmlns="http://www.xbrl.org/2003/instance" xmlns:xbrldi="http://xbrl.org/2006/xbrldi" xmlns:met="http://www.nbb.be/be/fr/cbso/dict/met" xmlns:dim="http://www.nbb.be/be/fr/cbso/dict/dim" xmlns:bas="http://www.nbb.be/be/fr/cbso/dict/dom/bas" xmlns:prd="http://www.nbb.be/be/fr/cbso/dict/dom/prd" xmlns:part="http://www.nbb.be/be/fr/cbso/dict/dom/part" xmlns:mmt="http://www.nbb.be/be/fr/cbso/dict/dom/mmt"><unit id="EUR"><measure xmlns:money="http://www.xbrl.org/2003/iso4217">money:EUR</measure></unit>"#.to_owned();
        for (id, prd, value) in [
            ("now", "m1", "9007199254740993.01"),
            ("prior", "m2", "123.40"),
        ] {
            text.push_str(&context(id, "m53", prd, ""));
            text.push_str(&format!(r#"<met:am2 id="fact-{id}" contextRef="{id}" unitRef="EUR" decimals="2">{value}</met:am2>"#));
        }
        text.push_str(r#"<met:am2 id="agreeing" contextRef="now" unitRef="EUR" decimals="INF">9007199254740993.010</met:am2>"#);
        text.push_str(&context("profit", "m44", "m1", ""));
        text.push_str(r#"<met:am2 id="profit-fact" contextRef="profit" unitRef="EUR" decimals="2">-12.34</met:am2>"#);
        for (id, prd, mmt, value) in [
            ("start", "m1", "m1", "2023-07-01"),
            ("end", "m1", "m2", "2024-06-30"),
            ("previous-start", "m2", "m1", "2022-01-01"),
            ("previous-end", "m2", "m2", "2022-12-31"),
        ] {
            let extra = format!(
                r#"<xbrldi:explicitMember dimension="dim:part">part:m2</xbrldi:explicitMember><xbrldi:explicitMember dimension="dim:mmt">mmt:{mmt}</xbrldi:explicitMember>"#
            );
            text.push_str(&context(id, "m27", prd, &extra));
            text.push_str(&format!(
                r#"<met:dte1 contextRef="{id}">{value}</met:dte1>"#
            ));
        }
        text.push_str("</xbrl>");
        text
    }
    #[cfg(feature = "network")]
    struct Synthetic;
    #[cfg(feature = "network")]
    impl Source for Synthetic {
        #[cfg(feature = "network")]
        fn reference(&self, reference: &str) -> Result<FilingReference, NbbError> {
            Ok(serde_json::from_value(json!({"ReferenceNumber": reference, "EnterpriseNumber": "0123456789", "Currency": "EUR", "ExerciseDates": {"startDate": "2023-07-01", "endDate": "2024-06-30"}})).unwrap())
        }
        #[cfg(feature = "network")]
        fn pdf(&self, reference: &str) -> Result<Capture, NbbError> {
            let bytes = b"%PDF-1.7\nsynthetic\n%%EOF".to_vec();
            Ok(Capture {
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                bytes,
                url: format!("https://ws.cbso.nbb.be/authentic/deposit/{reference}/accountingData"),
                request_id: Uuid::new_v4(),
                retrieved_at: "2026-01-02T10:00:00Z".into(),
                media_type: "application/pdf".into(),
            })
        }
        #[cfg(feature = "network")]
        fn xbrl(&self, reference: &str) -> Result<Capture, NbbError> {
            let mut capture = self.pdf(reference)?;
            capture.bytes = xml().into_bytes();
            capture.sha256 = format!("{:x}", Sha256::digest(&capture.bytes));
            capture.media_type = "application/x.xbrl".into();
            Ok(capture)
        }
    }
    #[cfg(feature = "network")]
    fn bundle() -> Temp {
        let t = Temp::new();
        create_bundle_with_xbrl(
            &Synthetic,
            "0123456789",
            &["2025-1".into(), "2025-2".into()],
            &t.0,
        )
        .unwrap();
        t
    }
    #[cfg(feature = "network")]
    #[test]
    fn exact_values_all_contributors_and_provenance_per_reference() {
        let t = bundle();
        let pin = t.pin();
        let evidence = extract_bundle(&t.0, &pin).unwrap();
        assert_eq!(evidence["schema_version"], 1);
        assert_eq!(evidence["restriction"], RESTRICTION);
        assert_eq!(evidence["filings"].as_array().unwrap().len(), 2);
        for (i, reference) in ["2025-1", "2025-2"].iter().enumerate() {
            let f = &evidence["filings"][i];
            assert_eq!(f["reference"], *reference);
            assert_eq!(f["enterprise"], "0123456789");
            assert_eq!(f["manifest_sha256"], pin);
            assert_eq!(
                f["xbrl"]["receipt"]["sha256"],
                format!("{:x}", Sha256::digest(xml().as_bytes()))
            );
            assert_eq!(
                f["pdf"]["receipt"]["sha256"],
                Synthetic.pdf(reference).unwrap().sha256
            );
            let periods = f["periods"].as_array().unwrap();
            assert_eq!(periods.len(), 2);
            let current = periods
                .iter()
                .find(|p| p["period_kind"] == "current")
                .unwrap();
            let prior = periods
                .iter()
                .find(|p| p["period_kind"] == "comparative")
                .unwrap();
            assert_eq!(current["values"]["70"], "9007199254740993.01");
            assert_eq!(prior["values"]["70"], "123.40");
            assert_eq!(current["values"]["9901"], "-12.34");
            assert!(prior["values"].get("9901").is_none());
            assert_eq!(prior["period_end"], "2022-12-31");
            assert!(current["values"].get("17").is_none());
            assert_eq!(current["declared_dates"].as_array().unwrap().len(), 4);
            let facts = current["contributors"]["70"].as_array().unwrap();
            assert_eq!(facts.len(), 2);
            assert_eq!(facts[1]["raw_value"], "9007199254740993.010");
            assert_eq!(facts[1]["id"], "agreeing");
            assert!(facts[1]["ordinal"].as_u64() > facts[0]["ordinal"].as_u64());
            assert_eq!(
                facts[1]["metric"]["namespace"],
                "http://www.nbb.be/be/fr/cbso/dict/met"
            );
            assert_eq!(facts[1]["context_ref"], "now");
            assert_eq!(
                facts[1]["context"]["identifier_scheme"],
                "http://www.fgov.be"
            );
            assert_eq!(facts[1]["context"]["instant"], "2024-06-30");
            assert_eq!(
                facts[1]["context"]["dimensions"][0]["member"]["local"],
                "m53"
            );
            assert_eq!(facts[1]["unit_measure"]["local"], "EUR");
            assert_eq!(facts[1]["unit_ref"], "EUR");
        }
        serde_json::from_str::<Value>(&serde_json::to_string(&evidence).unwrap()).unwrap();
    }
    #[cfg(feature = "network")]
    #[test]
    fn rejects_untrusted_manifest_before_parse_and_requires_v2() {
        let t = Temp::new();
        fs::create_dir(&t.0).unwrap();
        fs::write(t.0.join("manifest.json"), b"not JSON").unwrap();
        assert!(extract_bundle(&t.0, &"0".repeat(64))
            .unwrap_err()
            .to_string()
            .contains("trusted manifest"));
        for pin in ["A".repeat(64), "0".repeat(63), "g".repeat(64)] {
            assert!(extract_bundle(&t.0, &pin).is_err());
        }
        let v1 = Temp::new();
        create_bundle(&Synthetic, "0123456789", &["2025-1".into()], &v1.0).unwrap();
        assert!(extract_bundle(&v1.0, &v1.pin())
            .unwrap_err()
            .to_string()
            .contains("paired v2"));
    }
    #[cfg(feature = "network")]
    #[test]
    fn rejects_corrupted_artifacts_and_changed_manifest() {
        for filename in ["2025-1.pdf", "2025-1.xbrl", "manifest.json"] {
            let t = bundle();
            let pin = t.pin();
            fs::write(t.0.join(filename), b"corrupt").unwrap();
            assert!(extract_bundle(&t.0, &pin).is_err());
        }
    }
}
