use aanbestedingen_matcher::{
    nbb_api::{KeySlot, NbbClient, Product},
    nbb_bundle::{create_bundle, create_bundle_with_xbrl, verify_bundle, BundleResult},
    nbb_evidence::extract_bundle,
};
use std::{env, path::PathBuf};

const USAGE: &str = "Usage: nbb-bundle capture --enterprise 10DIGITS --reference REF [--reference REF] --output NEWDIR [--xbrl]\n       nbb-bundle verify PATH\n       nbb-bundle extract PATH --expected-manifest-sha256 HEX\nCapture uses Authentic primary credentials from the process environment. Verify is offline.\nPrivate research only; verification checks artifact integrity, not financial correctness.";

enum Command {
    Capture {
        enterprise: String,
        references: Vec<String>,
        output: PathBuf,
        xbrl: bool,
    },
    Verify(PathBuf),
    Extract {
        path: PathBuf,
        sha256: String,
    },
    Help,
}
fn parse(args: impl IntoIterator<Item = String>) -> BundleResult<Command> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        None | Some("--help" | "-h") => Ok(Command::Help),
        Some("verify") => {
            let path = args.next().ok_or("verify requires PATH")?;
            if args.next().is_some() {
                return Err("unexpected verify argument".into());
            }
            Ok(Command::Verify(path.into()))
        }
        Some("extract") => {
            let path = args.next().ok_or("extract requires PATH")?;
            if args.next().as_deref() != Some("--expected-manifest-sha256") {
                return Err("extract requires --expected-manifest-sha256 HEX".into());
            }
            let sha256 = args.next().ok_or("missing manifest SHA-256")?;
            if sha256.len() != 64
                || !sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("manifest SHA-256 must be 64 lowercase hex characters".into());
            }
            if args.next().is_some() {
                return Err("unexpected extract argument".into());
            }
            Ok(Command::Extract {
                path: path.into(),
                sha256,
            })
        }
        Some("capture") => {
            let mut enterprise = None;
            let mut output = None;
            let mut references = Vec::new();
            let mut xbrl = false;
            while let Some(arg) = args.next() {
                if arg == "--xbrl" {
                    if xbrl {
                        return Err("duplicate --xbrl".into());
                    }
                    xbrl = true;
                    continue;
                }
                let value = args.next().ok_or("option requires a value")?;
                match arg.as_str() {
                    "--enterprise" if enterprise.is_none() => enterprise = Some(value),
                    "--output" if output.is_none() => output = Some(PathBuf::from(value)),
                    "--reference" => references.push(value),
                    _ => return Err("unknown or duplicate option".into()),
                }
            }
            if references.is_empty() {
                return Err("at least one --reference is required".into());
            }
            Ok(Command::Capture {
                enterprise: enterprise.ok_or("missing --enterprise")?,
                references,
                output: output.ok_or("missing --output")?,
                xbrl,
            })
        }
        _ => Err("unknown command".into()),
    }
}
fn main() -> BundleResult<()> {
    let manifest = match parse(env::args().skip(1))? {
        Command::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Command::Extract { path, sha256 } => {
            let evidence = extract_bundle(&path, &sha256)?;
            let json = serde_json::to_string_pretty(&evidence)?;
            println!("{json}");
            return Ok(());
        }
        Command::Verify(path) => verify_bundle(&path)?,
        Command::Capture {
            enterprise,
            references,
            output,
            xbrl,
        } => {
            let client = NbbClient::from_env(Product::Authentic, KeySlot::Primary)?;
            if xbrl {
                create_bundle_with_xbrl(&client, &enterprise, &references, &output)?
            } else {
                create_bundle(&client, &enterprise, &references, &output)?
            }
        }
    };
    println!(
        "{} filing(s), schema v{}, enterprise {}; artifact integrity only, not financial correctness",
        manifest.files.len(),
        manifest.version,
        manifest.enterprise
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(s: &str) -> impl Iterator<Item = String> + '_ {
        s.split_whitespace().map(String::from)
    }
    #[test]
    fn extract_requires_explicit_lowercase_hash() {
        let pin = "a".repeat(64);
        assert!(matches!(
            parse(args(&format!(
                "extract /tmp/bundle --expected-manifest-sha256 {pin}"
            )))
            .unwrap(),
            Command::Extract { .. }
        ));
        for command in [
            "extract /tmp/bundle".to_owned(),
            format!(
                "extract /tmp/bundle --expected-manifest-sha256 {}",
                "A".repeat(64)
            ),
            format!("extract /tmp/bundle --expected-manifest-sha256 {pin} extra"),
            "extract /tmp/bundle --expected-manifest-sha256 abc".to_owned(),
        ] {
            assert!(parse(args(&command)).is_err());
        }
    }
    #[test]
    fn offline_verify_and_explicit_capture() {
        assert!(matches!(
            parse(args("verify /tmp/bundle")).unwrap(),
            Command::Verify(_)
        ));
        assert!(parse(args("verify /tmp/bundle extra")).is_err());
        assert!(matches!(
            parse(args(
                "capture --xbrl --enterprise 0123456789 --reference 1 --output /tmp/new"
            ))
            .unwrap(),
            Command::Capture { xbrl: true, .. }
        ));
        assert!(parse(args(
            "capture --xbrl --xbrl --enterprise 0123456789 --reference 1 --output /tmp/new"
        ))
        .is_err());
        assert!(parse(args("capture --enterprise 0123456789 --reference 2025-1 --reference 2024-1 --output /tmp/new")).is_ok());
        assert!(parse(args("capture --enterprise 0123456789 --output /tmp/new")).is_err());
        assert!(parse(args("capture --enterprise 0123456789 --enterprise 0123456789 --reference 1 --output /tmp/new")).is_err());
    }
}
