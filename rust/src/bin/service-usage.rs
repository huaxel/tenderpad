use aanbestedingen_matcher::service_calls::{Ledger, Policy, Service};
use chrono::{NaiveDate, Utc};
use std::{collections::BTreeMap, error::Error};

#[derive(Debug, PartialEq)]
struct Options {
    date: Option<String>,
    json: bool,
}
fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, Box<dyn Error>> {
    let mut options = Options {
        date: Some(Utc::now().date_naive().to_string()),
        json: false,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => options.date = None,
            "--date" => {
                options.date = Some(
                    NaiveDate::parse_from_str(
                        &args.next().ok_or("--date needs YYYY-MM-DD")?,
                        "%Y-%m-%d",
                    )?
                    .to_string(),
                )
            }
            "--json" => options.json = true,
            "--help" | "-h" => return Ok(None),
            _ => return Err("unknown argument".into()),
        }
    }
    Ok(Some(options))
}
fn main() -> Result<(), Box<dyn Error>> {
    let Some(options) = parse(std::env::args().skip(1))? else {
        println!("Usage: service-usage [--date YYYY-MM-DD|--all] [--json]\nDefault: today's UTC reservations. This command makes no external calls.");
        return Ok(());
    };
    let ledger = Ledger::from_env()?;
    let rows = ledger
        .summary()?
        .into_iter()
        .filter(|r| options.date.as_ref().is_none_or(|d| *d == r.date))
        .collect::<Vec<_>>();
    let services = [
        Service::Kbo,
        Service::Nbb,
        Service::BosaInt,
        Service::BosaProd,
    ];
    let mut policies = BTreeMap::new();
    for service in services {
        let policy = Policy::from_env(service)?;
        policies.insert(service.name(),serde_json::json!({"daily_limit":policy.daily_limit,"min_interval_ms":policy.min_interval_ms}));
    }
    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"ledger_directory":ledger.directory(),"date":options.date,"usage":rows,"current_local_policies":policies,"scope":"UTC reservations recorded by these clients in this local ledger; not provider quota or all account activity"})
            )?
        );
    } else {
        println!("Ledger: {}", ledger.directory().display());
        println!(
            "UTC DAY     SERVICE      ATTEMPTS  HTTP 2XX  HTTP OTHER  TRANSPORT  UNKNOWN  CAP NOW"
        );
        for row in &rows {
            let ok: u64 = row
                .statuses
                .iter()
                .filter(|(status, _)| (200..300).contains(*status))
                .map(|(_, n)| n)
                .sum();
            let other: u64 = row.statuses.values().sum::<u64>() - ok;
            println!(
                "{}  {:<11} {:>8} {:>9} {:>11} {:>10} {:>8} {:>8}",
                row.date,
                row.service.name(),
                row.attempts,
                ok,
                other,
                row.transport_errors,
                row.unknown,
                Policy::from_env(row.service)?.daily_limit
            );
        }
        if rows.is_empty() {
            println!("No recorded calls for this selection (not evidence of zero earlier/provider calls).");
        }
        println!("Local policy: 100 calls/day and 1000ms spacing per service by default; environment overrides may apply.");
        println!("Attempts include reservations that may not have been sent. UNKNOWN includes in-flight/crashed calls. HTTP status is recorded at headers, not proof of a complete download.");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn dates_and_machine_readable_output_are_explicit() {
        let options = parse(args(&["--date", "2026-11-03", "--json"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            options,
            Options {
                date: Some("2026-11-03".into()),
                json: true
            }
        );
        assert_eq!(parse(args(&["--all"])).unwrap().unwrap().date, None);
        assert!(parse(args(&["--date", "invalid"])).is_err());
        assert!(parse(args(&["--delete"])).is_err());
        assert!(parse(args(&["--help"])).unwrap().is_none());
    }
}
