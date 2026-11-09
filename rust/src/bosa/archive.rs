//! Bounded, in-memory inspection of ZIP attachments.
//!
//! Members are read into memory only, never written to disk, and member names
//! are labels only: they are never joined to a filesystem path. Nested archives
//! are listed (kind `zip`) but not opened. Encrypted members, symlinks, unsafe
//! names, duplicates and anything over a bound are skipped with a stated reason,
//! and `truncated` says when a bound stopped the walk, so what was not read is
//! never mistaken for what was absent.
//!
//! Like `ooxml_text`, this runs in-process, not in the OS sandbox.
use super::classify::{manifest_entry, ManifestEntry};
use super::documents::MAX_ATTACHMENT_BYTES;
use serde::Serialize;
use std::collections::HashSet;
use std::io::{Cursor, Read};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_members: usize,
    pub max_member_bytes: u64,
    pub max_total_bytes: u64,
}

pub const DEFAULT_LIMITS: Limits = Limits {
    max_members: 500,
    max_member_bytes: MAX_ATTACHMENT_BYTES,
    max_total_bytes: 128 * 1024 * 1024,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    UnsafeName,
    DuplicateName,
    Encrypted,
    Symlink,
    MemberTooLarge,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedMember {
    /// Display copy of the archive's name with control and bidirectional-override
    /// characters removed and long names shortened; never used as a path.
    pub name: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveReport {
    /// One entry per member that was read; `original_name` is the in-archive path.
    pub members: Vec<ManifestEntry>,
    pub skipped: Vec<SkippedMember>,
    /// True when the member-count, total-size or inflation bound stopped the walk;
    /// members after that point were neither read nor listed.
    pub truncated: bool,
    /// Entries the central directory's end record declares beyond what the reader
    /// returned (the reader keeps one entry per name, so these are likely members
    /// hidden behind a duplicate name). A heuristic from attacker-controllable
    /// bytes: nonzero is a warning, zero is not proof that nothing is hidden.
    pub suspected_hidden_entries: usize,
}

/// A name is safe to show and to treat as a label if it cannot escape a directory
/// or smuggle control characters. This is defence in depth: names are never used
/// as paths anyway.
/// Control characters and invisible bidirectional/zero-width controls that can
/// make a label display as something else.
fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

const MAX_NAME_BYTES: usize = 1024;

fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.chars().any(is_unsafe_char)
        && !name.split('/').any(|part| part == ".." || part == ".")
        && name.as_bytes().get(1).is_none_or(|b| *b != b':')
}

/// Entry count declared by the end-of-central-directory record, if it is a
/// plain (non-zip64) archive. The zip reader keeps one entry per name, so a
/// shortfall against its own count reveals members hidden behind a duplicate name.
fn declared_entry_count(bytes: &[u8]) -> Option<usize> {
    let window_start = bytes.len().saturating_sub(22 + 65_535);
    let at = bytes[window_start..]
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")?
        + window_start;
    let count = u16::from_le_bytes([*bytes.get(at + 10)?, *bytes.get(at + 11)?]);
    (count != u16::MAX).then_some(usize::from(count))
}

/// Visit each readable member's bytes without keeping them, honouring `limits`.
pub fn walk_members(
    bytes: &[u8],
    limits: Limits,
    mut visit: impl FnMut(&str, &[u8]),
) -> Result<(Vec<SkippedMember>, bool, usize), String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("invalid zip: {e}"))?;
    let mut skipped = Vec::new();
    let suspected_hidden =
        declared_entry_count(bytes).map_or(0, |declared| declared.saturating_sub(archive.len()));
    let mut seen = HashSet::new();
    let (mut total, mut truncated, mut read_members) = (0u64, false, 0usize);
    // Every inflated byte counts here, kept or discarded, so oversized members
    // cannot be used to burn unbounded CPU behind the total-size cap.
    let mut inflated = 0u64;
    for index in 0..archive.len() {
        let (name, encrypted, symlink, is_dir, declared) = {
            let raw = archive
                .by_index_raw(index)
                .map_err(|e| format!("invalid zip entry: {e}"))?;
            (
                raw.name().map(|n| n.into_owned()).ok(),
                raw.encrypted(),
                raw.is_symlink(),
                raw.is_dir(),
                raw.size(),
            )
        };
        if is_dir {
            continue;
        }
        if read_members + skipped.len() >= limits.max_members {
            truncated = true;
            break;
        }
        // A name that is not valid UTF-8 cannot be a trustworthy label.
        let Some(name) = name else {
            skipped.push(SkippedMember {
                name: "<non-UTF-8 name>".to_owned(),
                reason: SkipReason::UnsafeName,
            });
            continue;
        };
        let skip = |reason| SkippedMember {
            name: name
                .chars()
                .filter(|c| !is_unsafe_char(*c))
                .take(200)
                .collect(),
            reason,
        };
        if !is_safe_name(&name) {
            skipped.push(skip(SkipReason::UnsafeName));
            continue;
        }
        if !seen.insert(name.clone()) {
            skipped.push(skip(SkipReason::DuplicateName));
            continue;
        }
        if encrypted {
            skipped.push(skip(SkipReason::Encrypted));
            continue;
        }
        if symlink {
            skipped.push(skip(SkipReason::Symlink));
            continue;
        }
        if declared > limits.max_member_bytes {
            skipped.push(skip(SkipReason::MemberTooLarge));
            continue;
        }
        let Ok(entry) = archive.by_index(index) else {
            skipped.push(skip(SkipReason::Unreadable));
            continue;
        };
        // Bound by bytes actually decompressed, not by the declared size.
        let mut data = Vec::new();
        let read = entry
            .take(limits.max_member_bytes + 1)
            .read_to_end(&mut data);
        // Count what was inflated even when the read then fails (a forged size or
        // CRC error surfaces only after the data was produced).
        inflated = inflated.saturating_add(data.len() as u64);
        if inflated > limits.max_total_bytes.saturating_mul(2) {
            truncated = true;
            break;
        }
        if read.is_err() {
            skipped.push(skip(SkipReason::Unreadable));
            continue;
        }
        if data.len() as u64 > limits.max_member_bytes {
            skipped.push(skip(SkipReason::MemberTooLarge));
            continue;
        }
        if total + data.len() as u64 > limits.max_total_bytes {
            truncated = true;
            break;
        }
        total += data.len() as u64;
        read_members += 1;
        visit(&name, &data);
    }
    Ok((skipped, truncated, suspected_hidden))
}

pub fn inspect_archive(bytes: &[u8]) -> Result<ArchiveReport, String> {
    inspect_archive_with(bytes, DEFAULT_LIMITS)
}

pub fn inspect_archive_with(bytes: &[u8], limits: Limits) -> Result<ArchiveReport, String> {
    inspect_archive_visiting(bytes, limits, |_, _| {})
}

/// Like `inspect_archive_with`, also handing each member's manifest row and bytes
/// to `on_member` while they are in memory, so callers can run a further stage
/// without the archive layer retaining anything.
pub fn inspect_archive_visiting(
    bytes: &[u8],
    limits: Limits,
    mut on_member: impl FnMut(&ManifestEntry, &[u8]),
) -> Result<ArchiveReport, String> {
    let mut members = Vec::new();
    let (skipped, truncated, suspected_hidden_entries) =
        walk_members(bytes, limits, |name, data| {
            let file_name = name.rsplit('/').next().unwrap_or(name);
            let mut entry = manifest_entry(file_name, data);
            entry.original_name = name.to_owned();
            on_member(&entry, data);
            members.push(entry);
        })?;
    Ok(ArchiveReport {
        members,
        skipped,
        truncated,
        suspected_hidden_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bosa::classify::FileKind;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn build(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in members {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    const PDF: &[u8] = b"%PDF-1.7 minimal";

    #[test]
    fn members_are_classified_with_in_archive_paths() {
        let archive = build(&[
            ("bestek/Bestek.pdf", PDF),
            ("plan.dwg", b"AC1027 not a known kind"),
            ("data/lijst.xlsx", b"not really xlsx"),
        ]);
        let report = inspect_archive(&archive).unwrap();
        assert!(!report.truncated && report.skipped.is_empty());
        let got: Vec<_> = report
            .members
            .iter()
            .map(|m| (m.original_name.as_str(), m.kind, m.extension_matches_kind))
            .collect();
        assert_eq!(
            got,
            vec![
                ("bestek/Bestek.pdf", FileKind::Pdf, true),
                ("plan.dwg", FileKind::Unknown, false),
                ("data/lijst.xlsx", FileKind::Unknown, false),
            ]
        );
        assert_eq!(report.members[0].size_bytes, PDF.len() as u64);
    }

    #[test]
    fn unsafe_names_are_skipped_and_never_read() {
        let archive = build(&[
            ("../evil.pdf", PDF),
            ("/abs.pdf", PDF),
            ("a/../../b.pdf", PDF),
            ("dir\\win.pdf", PDF),
            ("C:/x.pdf", PDF),
            ("./dot.pdf", PDF),
            ("ok.pdf", PDF),
        ]);
        let report = inspect_archive(&archive).unwrap();
        assert_eq!(report.members.len(), 1);
        assert_eq!(report.members[0].original_name, "ok.pdf");
        assert_eq!(report.skipped.len(), 6);
        assert!(report
            .skipped
            .iter()
            .all(|s| s.reason == SkipReason::UnsafeName));
    }

    #[test]
    fn a_nested_archive_is_listed_but_not_opened() {
        let inner = build(&[("secret.pdf", PDF)]);
        let outer = build(&[("inner.zip", inner.as_slice()), ("top.pdf", PDF)]);
        let report = inspect_archive(&outer).unwrap();
        let names: Vec<_> = report
            .members
            .iter()
            .map(|m| m.original_name.as_str())
            .collect();
        assert_eq!(names, ["inner.zip", "top.pdf"]);
        assert_eq!(report.members[0].kind, FileKind::Zip);
    }

    #[test]
    fn duplicate_names_never_yield_two_members() {
        // The writer refuses duplicates, so build two distinct names and patch one
        // (same length) in both the local and central headers.
        let mut bytes = build(&[("a.pdf", PDF), ("b.pdf", PDF)]);
        let needle = b"b.pdf";
        let mut at = 0;
        while let Some(found) = bytes[at..].windows(5).position(|w| w == needle) {
            bytes[at + found] = b'a';
            at += found + 5;
        }
        // Either the reader rejects the archive (fail closed) or the duplicate is
        // skipped and reported; two members with one name is never acceptable.
        match inspect_archive(&bytes) {
            Err(_) => {}
            Ok(report) => {
                assert_eq!(report.members.len(), 1);
                assert_eq!(
                    report.suspected_hidden_entries, 1,
                    "the hidden duplicate is reported"
                );
            }
        }
    }

    #[test]
    fn oversized_members_are_skipped_by_actual_size() {
        let big = vec![b'a'; 200];
        let archive = build(&[("big.txt", &big), ("small.pdf", PDF)]);
        let limits = Limits {
            max_member_bytes: 100,
            ..DEFAULT_LIMITS
        };
        let report = inspect_archive_with(&archive, limits).unwrap();
        assert_eq!(report.members.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::MemberTooLarge);
        assert!(!report.truncated);
    }

    #[test]
    fn total_size_and_member_count_bounds_truncate_and_say_so() {
        let archive = build(&[("a.pdf", PDF), ("b.pdf", PDF), ("c.pdf", PDF)]);
        let by_total = inspect_archive_with(
            &archive,
            Limits {
                max_total_bytes: (PDF.len() * 2) as u64,
                ..DEFAULT_LIMITS
            },
        )
        .unwrap();
        assert!(by_total.truncated);
        assert_eq!(by_total.members.len(), 2);
        let by_count = inspect_archive_with(
            &archive,
            Limits {
                max_members: 2,
                ..DEFAULT_LIMITS
            },
        )
        .unwrap();
        assert!(by_count.truncated);
        assert_eq!(by_count.members.len(), 2);
    }

    #[test]
    fn the_visitor_sees_each_read_member_once_with_its_bytes() {
        let archive = build(&[
            ("a.pdf", PDF),
            ("../bad.pdf", PDF),
            ("b.pdf", b"%PDF-1.7 two"),
        ]);
        let mut seen = Vec::new();
        let report = inspect_archive_visiting(&archive, DEFAULT_LIMITS, |entry, data| {
            seen.push((entry.original_name.clone(), data.len()));
        })
        .unwrap();
        assert_eq!(
            seen,
            [("a.pdf".to_owned(), PDF.len()), ("b.pdf".to_owned(), 12)]
        );
        assert_eq!(report.members.len(), 2);
    }

    #[test]
    fn a_fake_end_record_in_the_comment_cannot_starve_the_walk() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for name in ["a.pdf", "b.pdf"] {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(PDF).unwrap();
        }
        let mut fake = b"PK\x05\x06".to_vec();
        fake.extend_from_slice(&[0; 4]);
        fake.extend_from_slice(&60_000u16.to_le_bytes());
        fake.extend_from_slice(&60_000u16.to_le_bytes());
        fake.extend_from_slice(&[0; 10]);
        writer.set_raw_comment(fake.into_boxed_slice()).unwrap();
        let report = inspect_archive(&writer.finish().unwrap().into_inner()).unwrap();
        // Both real members are still read; the bogus count is only a warning number.
        assert_eq!(report.members.len(), 2);
        assert!(!report.truncated);
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn discarded_inflation_counts_toward_a_budget() {
        // Honest headers are rejected by the declared-size precheck before any
        // inflation, so forge the declared size in every local and central header.
        let big = vec![b'a'; 4000];
        let members: Vec<(String, &[u8])> = (0..10)
            .map(|i| (format!("big{i}.bin"), big.as_slice()))
            .collect();
        let refs: Vec<(&str, &[u8])> = members.iter().map(|(n, b)| (n.as_str(), *b)).collect();
        let mut bytes = build(&refs);
        for (signature, offset) in [(&b"PK\x03\x04"[..], 22usize), (&b"PK\x01\x02"[..], 24usize)] {
            let mut at = 0;
            while let Some(found) = bytes[at..].windows(4).position(|w| w == signature) {
                let field = at + found + offset;
                bytes[field..field + 4].copy_from_slice(&50u32.to_le_bytes());
                at += found + 4;
            }
        }
        let limits = Limits {
            max_members: 500,
            max_member_bytes: 100,
            max_total_bytes: 150,
        };
        let report = inspect_archive_with(&bytes, limits).unwrap();
        assert!(
            report.truncated,
            "oversized members must not be free to inflate"
        );
        assert!(report.skipped.len() < 10, "the walk stopped early");
    }

    #[test]
    fn bidi_overrides_and_overlong_names_are_unsafe() {
        let archive = build(&[
            ("evil\u{202E}fdp.exe", PDF),
            (&"a".repeat(2000), PDF),
            ("fine.pdf", PDF),
        ]);
        let report = inspect_archive(&archive).unwrap();
        assert_eq!(report.members.len(), 1);
        assert_eq!(report.skipped.len(), 2);
        assert!(report
            .skipped
            .iter()
            .all(|s| s.reason == SkipReason::UnsafeName));
        assert!(report.skipped.iter().all(|s| s.name.chars().count() <= 200));
        assert!(!report.skipped[0].name.contains('\u{202E}'));
    }

    #[test]
    fn symlinks_are_skipped() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink("link.pdf", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        writer
            .start_file("real.pdf", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(PDF).unwrap();
        let report = inspect_archive(&writer.finish().unwrap().into_inner()).unwrap();
        assert_eq!(report.members.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Symlink);
    }

    #[test]
    fn encrypted_members_are_reported_not_read() {
        let mut bytes = build(&[("locked.pdf", PDF), ("open.pdf", PDF)]);
        // Set the encryption bit (general-purpose flag bit 0) on the first
        // central-directory header; the writer cannot produce encrypted members.
        let central = bytes
            .windows(4)
            .position(|w| w == b"PK\x01\x02")
            .expect("central directory");
        bytes[central + 8] |= 1;
        let report = inspect_archive(&bytes).unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::Encrypted);
        assert_eq!(report.members.len(), 1);
        assert_eq!(report.members[0].original_name, "open.pdf");
    }

    #[test]
    fn hostile_names_are_stripped_of_control_characters_in_the_report() {
        let archive = build(&[("bad\nname\u{1b}[31m.pdf", PDF)]);
        let report = inspect_archive(&archive).unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert!(!report.skipped[0].name.chars().any(char::is_control));
    }

    #[test]
    fn a_non_zip_or_truncated_archive_is_an_error() {
        assert!(inspect_archive(b"%PDF-1.7").is_err());
        let mut archive = build(&[("a.pdf", PDF)]);
        archive.truncate(12);
        assert!(inspect_archive(&archive).is_err());
    }
}
