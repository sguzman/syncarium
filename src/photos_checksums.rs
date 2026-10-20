//! Explicit offline full-file SHA-256 verification against trusted checksums.
//!
//! This module does not CREATE an expected hash from the archive it is
//! checking: a self-derived fingerprint would not prove authenticity.
//! The operator supplies a private reference from an independently trusted
//! source, or one deliberately retained earlier as a local integrity baseline.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::Path;

use crate::{
    check_mount, no_symlink_ancestors, safe_relative, Backend, Config,
    MAX_PROVIDER_LISTING_BYTES,
};

const MAX_FILES: usize = 10_000;
const MAX_MIB: u64 = 4096;
const BUFFER_SIZE: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    path: String,
    size: u64,
    sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct Report {
    source: String,
    expected_files: u64,
    expected_bytes: u64,
    checked_files: u64,
    matched_sha256: u64,
    mismatched_sha256: u64,
    missing_local_files: u64,
    size_conflicts: u64,
    unsafe_or_unreadable: u64,
    missing_reference_checksums: u64,
    zero_byte_expected: u64,
    deferred_file_limit: u64,
    deferred_byte_budget: u64,
    disk_bytes_reserved: u64,
    max_files: usize,
    max_disk_bytes: u64,
    caveat: &'static str,
}

/// Only a reference from outside the *current* archive can authenticate
/// cloud originals. A locally made baseline only detects later drift.
fn parse_reference(raw: &[u8]) -> Result<Vec<Expected>> {
    let mut expected: Vec<Expected> =
        serde_json::from_slice(raw).context("invalid checksum reference JSON")?;
    ensure!(!expected.is_empty(), "checksum reference is empty, not complete");
    ensure!(expected.len() <= 1_000_000,
        "checksum reference exceeds one million entries");
    let mut seen = HashSet::with_capacity(expected.len());
    for entry in &mut expected {
        safe_relative(&entry.path).context("unsafe private checksum reference path")?;
        ensure!(seen.insert(entry.path.clone()),
            "duplicate paths in checksum reference");
        if let Some(digest) = &mut entry.sha256 {
            ensure!(digest.len() == 64 &&
                digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "expected SHA-256 must contain exactly 64 hex characters");
            digest.make_ascii_lowercase();
        }
    }
    expected.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(expected)
}

fn same_identity(before: &Metadata, after: &Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return false;
        }
    }
    true
}

/// Reads exactly the expected number of file bytes, and no more.
/// Return failures rather than a hash of partially read or concurrently
/// changed files. Checking metadata is best effort against hostile races.
fn hash_existing_file(target: &Path, expected_size: u64) -> Result<String> {
    no_symlink_ancestors(target)?;
    let original = fs::symlink_metadata(target)?;
    ensure!(original.file_type().is_file(),
        "archive media is not a regular file");
    ensure!(original.len() == expected_size, "archive media changed size");
    let mut file = File::open(target)?;
    let opened = file.metadata()?;
    ensure!(same_identity(&original, &opened),
        "archive media changed while being opened");
    let mut sha = Sha256::new();
    let mut left = expected_size;
    let mut buffer = [0u8; BUFFER_SIZE];
    while left > 0 {
        let limit = left.min(BUFFER_SIZE as u64) as usize;
        let n = file.read(&mut buffer[..limit])?;
        ensure!(n > 0, "archive media ended during SHA-256 verification");
        sha.update(&buffer[..n]);
        left -= n as u64;
    }
    let completed = file.metadata()?;
    ensure!(same_identity(&opened, &completed),
        "archive media changed during SHA-256 verification");
    Ok(hex::encode(sha.finalize()))
}

fn inspect(root: &Path, source: &str, reference: &[u8],
    max_files: usize, max_bytes: u64) -> Result<Report>
{
    ensure!((1..=MAX_FILES).contains(&max_files),
        "max-files must be 1..10000");
    ensure!(max_bytes > 0 && max_bytes <= MAX_MIB * 1024 * 1024,
        "invalid local disk byte budget");
    ensure!(root.is_dir(), "source archive destination missing");
    no_symlink_ancestors(root)?;
    let expected = parse_reference(reference)?;
    let mut report = Report {
        source: source.to_owned(), expected_files: 0, expected_bytes: 0,
        checked_files: 0, matched_sha256: 0, mismatched_sha256: 0,
        missing_local_files: 0, size_conflicts: 0, unsafe_or_unreadable: 0,
        missing_reference_checksums: 0, zero_byte_expected: 0,
        deferred_file_limit: 0, deferred_byte_budget: 0,
        disk_bytes_reserved: 0, max_files, max_disk_bytes: max_bytes,
        caveat: "Matching SHA-256 proves byte equality to the supplied reference, not that the reference came from Apple or that iCloud enumerated every original asset. A self-generated local baseline only detects later changes. Deferred or missing-hash items are NOT verified.",
    };
    for item in &expected {
        report.expected_files += 1;
        report.expected_bytes = report.expected_bytes.checked_add(item.size)
            .context("checksum reference size total overflow")?;
        let Some(expected_hash) = item.sha256.as_deref() else {
            report.missing_reference_checksums += 1;
            continue;
        };
        if item.size == 0 {
            report.zero_byte_expected += 1;
            continue;
        }
        if report.checked_files >= max_files as u64 {
            report.deferred_file_limit += 1;
            continue;
        }
        if item.size > max_bytes - report.disk_bytes_reserved {
            report.deferred_byte_budget += 1;
            continue;
        }
        // Reserve the complete anticipated file size even if an I/O error
        // occurs. The cap is on this tool's accepted file read attempts.
        report.checked_files += 1;
        report.disk_bytes_reserved += item.size;
        let relative = safe_relative(&item.path)?;
        let target = root.join(relative);
        if no_symlink_ancestors(&target).is_err() {
            report.unsafe_or_unreadable += 1;
            continue;
        }
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                report.missing_local_files += 1;
                continue;
            }
            Err(_) => {
                report.unsafe_or_unreadable += 1;
                continue;
            }
        };
        if !metadata.file_type().is_file() {
            report.unsafe_or_unreadable += 1;
        } else if metadata.len() != item.size {
            report.size_conflicts += 1;
        } else {
            match hash_existing_file(&target, item.size) {
                Ok(actual) if actual == expected_hash => report.matched_sha256 += 1,
                Ok(_) => report.mismatched_sha256 += 1,
                Err(_) => report.unsafe_or_unreadable += 1,
            }
        }
    }
    Ok(report)
}

pub(crate) fn run(cfg: &Config, source_name: &str, reference: &Path,
    max_files: usize, max_mib: u64, execute: bool, json: bool) -> Result<()>
{
    let source = cfg.sources.iter().find(|s| s.name == source_name)
        .context("unknown configured source")?;
    ensure!(matches!(source.backend, Backend::Rclone { .. }),
        "local checksum comparison currently requires an rclone archive source");
    ensure!((1..=MAX_FILES).contains(&max_files) &&
        (1..=MAX_MIB).contains(&max_mib),
        "max-files must be 1..10000 and max-mib must be 1..4096");
    if !execute {
        println!("PREVIEW ONLY: photos-checksums --source {source_name} --execute would compare local archived files to an independently provided SHA-256 reference.");
        println!("At most {max_files} files and {max_mib} MiB of local media will be read. No cloud access or archive changes.");
        return Ok(());
    }
    check_mount(cfg)?;
    ensure!(reference.is_absolute(),
        "private SHA-256 reference path must be absolute");
    no_symlink_ancestors(reference)?;
    let info = fs::symlink_metadata(reference)?;
    ensure!(info.file_type().is_file() && info.len() <= MAX_PROVIDER_LISTING_BYTES,
        "invalid or excessively large checksum reference");
    let bytes = fs::read(reference)
        .context("reading local private SHA-256 reference")?;
    let report = inspect(&source.destination, source_name, &bytes,
        max_files, max_mib * 1024 * 1024)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Local SHA-256 comparison for {}", report.source);
        println!("{} matched, {} hash mismatches, {} missing, {} size conflicts",
            report.matched_sha256, report.mismatched_sha256,
            report.missing_local_files, report.size_conflicts);
        println!("{} missing checksums, {} zero-byte, {} deferred by file limit, {} by byte budget",
            report.missing_reference_checksums, report.zero_byte_expected,
            report.deferred_file_limit, report.deferred_byte_budget);
        println!("Caution: {}", report.caveat);
    }
    let unresolved = report.mismatched_sha256 + report.missing_local_files
        + report.size_conflicts + report.unsafe_or_unreadable
        + report.missing_reference_checksums + report.zero_byte_expected
        + report.deferred_file_limit + report.deferred_byte_budget;
    if unresolved > 0 {
        bail!("{unresolved} media checksum entries failed, lacked evidence, or were deferred");
    }
    ensure!(report.matched_sha256 > 0, "no media checksums verified");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64, content: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "path": path, "size": size,
            "sha256": hex::encode(Sha256::digest(content))
        })
    }

    #[test]
    fn mismatching_same_size_bytes_and_missing_reference_are_not_verified() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("good.heic"), b"abcd").unwrap();
        fs::write(tmp.path().join("changed.mov"), b"wxyz").unwrap();
        let input = serde_json::json!([
            entry("good.heic", 4, b"abcd"),
            entry("changed.mov", 4, b"ABCD"),
            {"path":"unverifiable.heic","size":100}
        ]);
        let report = inspect(tmp.path(), "photos",
            &serde_json::to_vec(&input).unwrap(), 10, 1024).unwrap();
        assert_eq!(report.matched_sha256, 1);
        assert_eq!(report.mismatched_sha256, 1);
        assert_eq!(report.missing_reference_checksums, 1);
        assert_eq!(report.disk_bytes_reserved, 8);
        let output = serde_json::to_string(&report).unwrap();
        assert!(!output.contains("good.heic"));
        assert!(!output.contains("changed.mov"));
        assert!(!output.contains("unverifiable.heic"));
    }

    #[test]
    fn byte_and_file_limits_hold_even_for_large_expected_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a-small.heic"), b"first").unwrap();
        fs::write(tmp.path().join("c-other.heic"), b"second").unwrap();
        let input = serde_json::json!([
            entry("a-small.heic", 5, b"first"),
            entry("b-too-large.heic", 20, b"not-a-real-file"),
            entry("c-other.heic", 6, b"second"),
            entry("d-after.heic", 3, b"last")
        ]);
        let bytes = serde_json::to_vec(&input).unwrap();
        let report = inspect(tmp.path(), "photos", &bytes, 2, 12).unwrap();
        assert_eq!(report.matched_sha256, 2);
        assert_eq!(report.deferred_byte_budget, 1);
        assert_eq!(report.deferred_file_limit, 1);
        assert_eq!(report.disk_bytes_reserved, 11);
        let valid = inspect(tmp.path(), "photos", &bytes, 1, 1024 * 1024).unwrap();
        assert_eq!(valid.matched_sha256, 1);
        assert_eq!(valid.deferred_file_limit, 3);
    }

    #[test]
    fn unsafe_and_duplicate_checksum_paths_fail_closed() {
        let invalid = br#"[{"path":"../outside","size":1,"sha256":null}]"#;
        assert!(parse_reference(invalid).is_err());
        let duplicates = br#"[
            {"path":"same.heic","size":1,"sha256":null},
            {"path":"same.heic","size":1,"sha256":null}
        ]"#;
        assert!(parse_reference(duplicates).is_err());
        let bad_hash = br#"[{"path":"a.heic","size":10,"sha256":"abcd"}]"#;
        assert!(parse_reference(bad_hash).is_err());
        assert!(parse_reference(b"[]").is_err());
    }
}
