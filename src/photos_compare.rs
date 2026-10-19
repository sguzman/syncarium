//! Local-only comparison of two rclone lsjson snapshots.
//!
//! Both inputs are private metadata, and the report contains counts only.
//! This deliberately does not compare file content or certify completeness.
use anyhow::{bail, ensure, Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::{
    no_symlink_ancestors, parse_rclone_inventory, Config, Item,
    MAX_PROVIDER_LISTING_BYTES,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReferenceKind {
    /// Both snapshots represent metadata returned by cloud providers.
    Provider,
    /// The reference snapshot is an independent listing of local archive files.
    Archive,
}

#[derive(Serialize)]
struct Report {
    source: String,
    reference_kind: &'static str,
    reference_files: u64,
    reference_bytes: u64,
    candidate_files: u64,
    candidate_bytes: u64,
    shared_paths: u64,
    same_size: u64,
    differing_size: u64,
    reference_only: u64,
    candidate_only: u64,
    /// Only when both snapshots are provider lists and both fields are present.
    differing_provider_ids: u64,
    differing_modified_times: u64,
    /// Candidate bytes absent from the reference or found at different sizes.
    candidate_bytes_not_size_matched: u64,
    reference_zero_byte_files: u64,
    candidate_zero_byte_files: u64,
    /// Even a completely matching result does not prove content or API coverage.
    caveat: &'static str,
}

fn snapshot(path: &Path, name: &str) -> Result<Vec<Item>> {
    ensure!(path.is_absolute(), "comparison inputs must be absolute private paths");
    no_symlink_ancestors(path)?;
    let stat = fs::symlink_metadata(path).context("reading private inventory metadata")?;
    ensure!(stat.file_type().is_file(), "inventory input is not an ordinary file");
    ensure!(stat.len() <= MAX_PROVIDER_LISTING_BYTES,
        "inventory input exceeds the permitted provider-listing ceiling");
    let bytes = fs::read(path).context("reading private inventory")?;
    parse_rclone_inventory(&bytes, name)
}

/// Remote metadata comparisons are intentionally ignored when the reference
/// lists local files: local inode IDs and mtimes aren't Google/Apple asset IDs.
fn provider_field<'a>(item: &'a Item, field: &str) -> Result<Option<String>> {
    let parsed: Value = serde_json::from_str(&item.version)
        .context("invalid provider metadata in comparison inventory")?;
    Ok(parsed.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string))
}

fn summarize(
    source: &str,
    reference: &[Item],
    candidate: &[Item],
    kind: ReferenceKind,
) -> Result<Report> {
    let mut report = Report {
        source: source.into(),
        reference_kind: match kind {
            ReferenceKind::Provider => "provider",
            ReferenceKind::Archive => "archive",
        },
        reference_files: 0,
        reference_bytes: 0,
        candidate_files: 0,
        candidate_bytes: 0,
        shared_paths: 0,
        same_size: 0,
        differing_size: 0,
        reference_only: 0,
        candidate_only: 0,
        differing_provider_ids: 0,
        differing_modified_times: 0,
        candidate_bytes_not_size_matched: 0,
        reference_zero_byte_files: 0,
        candidate_zero_byte_files: 0,
        caveat: "Paths and reported byte sizes are compared, not file content. Matching snapshots do not certify that iCloud listed every asset, preserved originals, or included Live Photo companions. Reference and candidate must represent the same scoped hierarchy with matching relative paths.",
    };
    let mut by_path = HashMap::new();
    for item in reference {
        ensure!(by_path.insert(item.path.as_str(), item).is_none(),
            "reference contains duplicate paths");
        report.reference_files = report.reference_files.checked_add(1)
            .context("reference count overflow")?;
        report.reference_bytes = report.reference_bytes.checked_add(item.size)
            .context("reference byte total overflow")?;
        report.reference_zero_byte_files += u64::from(item.size == 0);
    }
    for item in candidate {
        report.candidate_files = report.candidate_files.checked_add(1)
            .context("candidate count overflow")?;
        report.candidate_bytes = report.candidate_bytes.checked_add(item.size)
            .context("candidate byte total overflow")?;
        report.candidate_zero_byte_files += u64::from(item.size == 0);
        if let Some(prior) = by_path.get(item.path.as_str()) {
            report.shared_paths += 1;
            if prior.size == item.size {
                report.same_size += 1;
            } else {
                report.differing_size += 1;
                report.candidate_bytes_not_size_matched = report
                    .candidate_bytes_not_size_matched.checked_add(item.size)
                    .context("candidate mismatch byte total overflow")?;
            }
            if kind == ReferenceKind::Provider {
                let original_id = provider_field(prior, "id")?;
                let current_id = provider_field(item, "id")?;
                if original_id.is_some() && current_id.is_some()
                    && original_id != current_id
                {
                    report.differing_provider_ids += 1;
                }
                let original_time = provider_field(prior, "modtime")?;
                let current_time = provider_field(item, "modtime")?;
                if original_time.is_some() && current_time.is_some()
                    && original_time != current_time
                {
                    report.differing_modified_times += 1;
                }
            }
        } else {
            report.candidate_only += 1;
            report.candidate_bytes_not_size_matched = report
                .candidate_bytes_not_size_matched.checked_add(item.size)
                .context("candidate-only byte total overflow")?;
        }
    }
    report.reference_only = report.reference_files.saturating_sub(report.shared_paths);
    Ok(report)
}

pub(crate) fn compare(
    cfg: &Config,
    source_name: &str,
    reference_path: &Path,
    candidate_path: &Path,
    kind: ReferenceKind,
    json: bool,
) -> Result<()> {
    ensure!(cfg.sources.iter().any(|src| src.name == source_name),
        "unknown configured source");
    // No check_mount, rclone, auth, network, checkpoint or archive I/O.
    let reference = snapshot(reference_path, "reference")?;
    let candidate = snapshot(candidate_path, "candidate")?;
    let report = summarize(source_name, &reference, &candidate, kind)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Offline photo inventory comparison: {}", report.source);
        println!("Reference: {} files, {} bytes; candidate: {} files, {} bytes",
            report.reference_files, report.reference_bytes,
            report.candidate_files, report.candidate_bytes);
        println!("{} shared paths ({} same-size, {} different-size), {} reference-only, {} candidate-only",
            report.shared_paths, report.same_size, report.differing_size,
            report.reference_only, report.candidate_only);
        if kind == ReferenceKind::Provider {
            println!("{} changed IDs, {} changed provider timestamps (where both supplied)",
                report.differing_provider_ids, report.differing_modified_times);
        }
        println!("Caution: {}", report.caveat);
    }
    let mismatches = report.reference_only + report.candidate_only
        + report.differing_size + report.differing_provider_ids
        + report.differing_modified_times;
    ensure!(report.reference_files > 0 && report.candidate_files > 0,
        "one or both inventories are empty; comparison is inconclusive");
    if mismatches > 0 {
        bail!("{mismatches} mismatched or unmatched inventory entries; no files modified");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(raw: &str) -> Vec<Item> {
        parse_rclone_inventory(raw.as_bytes(), "test").unwrap()
    }

    #[test]
    fn provider_snapshots_count_missing_size_and_identity_drift_without_names() {
        let reference = sample(r#"[
            {"Path":"family/private.heic","Size":10,"ID":"secret-A","ModTime":"old"},
            {"Path":"family/removed.mov","Size":20,"ID":"secret-B","ModTime":"old"},
            {"Path":"family/changed.jpg","Size":30,"ID":"secret-C","ModTime":"old"}]"#);
        let candidate = sample(r#"[
            {"Path":"family/private.heic","Size":10,"ID":"secret-X","ModTime":"new"},
            {"Path":"family/changed.jpg","Size":31,"ID":"secret-C","ModTime":"old"},
            {"Path":"family/added.png","Size":40,"ID":"secret-D","ModTime":"new"}]"#);
        let report = summarize("photos", &reference, &candidate,
            ReferenceKind::Provider).unwrap();
        assert_eq!(report.reference_only, 1);
        assert_eq!(report.candidate_only, 1);
        assert_eq!(report.differing_size, 1);
        assert_eq!(report.same_size, 1);
        assert_eq!(report.differing_provider_ids, 1);
        assert_eq!(report.differing_modified_times, 1);
        assert_eq!(report.candidate_bytes_not_size_matched, 71);
        let output = serde_json::to_string(&report).unwrap();
        for secret in ["private.heic", "secret-", "family", "removed.mov"] {
            assert!(!output.contains(secret));
        }
    }

    #[test]
    fn local_archive_snapshot_does_not_compare_incompatible_inodes_or_timestamps() {
        let local = sample(
            r#"[{"Path":"img.heic","Size":4,"ID":"local-inode","ModTime":"local"}]"#
        );
        let remote = sample(
            r#"[{"Path":"img.heic","Size":4,"ID":"remote-id","ModTime":"remote"}]"#
        );
        let report = summarize("photos", &local, &remote,
            ReferenceKind::Archive).unwrap();
        assert_eq!(report.same_size, 1);
        assert_eq!(report.differing_provider_ids, 0);
        assert_eq!(report.differing_modified_times, 0);
    }

    #[test]
    fn empty_snapshots_cannot_pass_as_complete_archive() {
        let empty = sample("[]");
        let report = summarize("photos", &empty, &empty,
            ReferenceKind::Provider).unwrap();
        assert_eq!(report.reference_files, 0);
        assert!(report.caveat.contains("do not certify"));
    }
}
