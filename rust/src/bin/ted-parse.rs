//! Parse one TED notice JSON document from stdin, print the Tender.
//!
//! Test entry point for cross-language differential tests (mirrors the
//! aanbestedingen-matcher stdin binary); not a user-facing command.
use aanbestedingen_matcher::ted::try_notice_to_tender;
use std::io::{self, Read};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    if input.trim().is_empty() {
        return Err("expected a TED notice JSON document on stdin".into());
    }
    let notice: serde_json::Value = serde_json::from_str(&input)?;
    let tender = try_notice_to_tender(&notice)?;
    println!("{}", serde_json::to_string_pretty(&tender)?);
    Ok(())
}
