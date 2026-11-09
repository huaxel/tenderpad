use std::io::{self, Read};

use aanbestedingen_matcher::{match_request, MatchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    if input.trim().is_empty() {
        return Err("expected a MatchRequest JSON document on stdin".into());
    }

    let request: MatchRequest = serde_json::from_str(&input)?;
    let result = match_request(request);
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
