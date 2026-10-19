//! Read-only, privacy-preserving inspection of an rclone photo listing.
//!
//! A same-stem .heic/.mov pair is a *candidate* Live Photo companion,
//! never proof of a Live Photo or of completeness. Regular photos need not
//! have a video companion, and Apple may expose assets under other names.
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::{
    check_mount, no_symlink_ancestors, parse_rclone_inventory, read_items, Backend,
    Config, Item, MAX_PROVIDER_LISTING_BYTES,
};

#[derive(Default)]
struct StemGroup {
    stills: u64,
    videos: u64,
}

#[derive(Serialize)]
struct Summary {
    source: String,
    listed_files: u64,
    listed_bytes: u64,
    still_images: u64,
    motion_files: u64,
    sidecars: u64,
    other_or_unrecognized: u64,
    zero_byte_files: u64,
    missing_provider_id: u64,
    missing_modified_time: u64,
    possible_still_motion_pairs: u64,
    ambiguous_shared_stems: u64,
    still_stems_without_matching_video: u64,
    video_stems_without_matching_still: u64,
    /// Only known format extensions are included, not arbitrary user suffixes.
    format_counts: BTreeMap<String, u64>,
    caveat: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Still,
    Video,
    Sidecar,
    Other,
}

fn classify(extension: &str) -> MediaKind {
    match extension {
        "heic" | "heif" | "jpg" | "jpeg" | "png" | "gif" | "tif"
        | "tiff" | "dng" | "webp" | "avif" | "bmp" | "raw"
        | "cr2" | "cr3" | "nef" | "arw" | "orf" | "raf"
        | "rw2" | "pef" | "srw" => MediaKind::Still,
        "mov" | "mp4" | "m4v" | "avi" | "mts" | "m2ts"
        | "webm" | "mpg" | "mpeg" | "3gp" => MediaKind::Video,
        "aae" | "xmp" | "thm" => MediaKind::Sidecar,
        _ => MediaKind::Other,
    }
}

fn summarize(source: &str, items: &[Item]) -> Result<Summary> {
    let mut report = Summary {
        source: source.to_string(),
        listed_files: 0,
        listed_bytes: 0,
        still_images: 0,
        motion_files: 0,
        sidecars: 0,
        other_or_unrecognized: 0,
        zero_byte_files: 0,
        missing_provider_id: 0,
        missing_modified_time: 0,
        possible_still_motion_pairs: 0,
        ambiguous_shared_stems: 0,
        still_stems_without_matching_video: 0,
        video_stems_without_matching_still: 0,
        format_counts: BTreeMap::new(),
        caveat: "Filename-based companion matches are only candidates. An unpaired photo is not evidence that its Live Photo video is missing. This report cannot establish complete Apple Photos enumeration, originals, edits, or remote identity.",
    };
    // Internal matching keys are never logged or serialized.
    let mut stems: HashMap<(String, String), StemGroup> = HashMap::new();
    for item in items {
        report.listed_files = report.listed_files.checked_add(1)
            .context("photo count overflow")?;
        report.listed_bytes = report.listed_bytes.checked_add(item.size)
            .context("photo byte total overflow")?;
        if item.size == 0 {
            report.zero_byte_files += 1;
        }
        let metadata: serde_json::Value = serde_json::from_str(&item.version)
            .context("invalid version metadata in provider listing")?;
        if metadata.get("id").is_none_or(serde_json::Value::is_null) {
            report.missing_provider_id += 1;
        }
        if metadata.get("modtime").is_none_or(serde_json::Value::is_null) {
            report.missing_modified_time += 1;
        }
        let path = Path::new(&item.path);
        let extension = path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let kind = classify(&extension);
        match kind {
            MediaKind::Still => report.still_images += 1,
            MediaKind::Video => report.motion_files += 1,
            MediaKind::Sidecar => report.sidecars += 1,
            MediaKind::Other => report.other_or_unrecognized += 1,
        }
        if kind != MediaKind::Other {
            *report.format_counts.entry(extension).or_insert(0) += 1;
        }
        if kind == MediaKind::Still || kind == MediaKind::Video {
            // Require the same directory as well as the same filename stem.
            // This reduces false positives from reused camera filenames.
            let folder = path.parent().unwrap_or_else(|| Path::new(""))
                .to_string_lossy().to_lowercase();
            let stem = path.file_stem().and_then(|value| value.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !stem.is_empty() {
                let group = stems.entry((folder, stem)).or_default();
                match kind {
                    MediaKind::Still => group.stills += 1,
                    MediaKind::Video => group.videos += 1,
                    _ => {}
                }
            }
        }
    }
    for group in stems.values() {
        match (group.stills, group.videos) {
            (0, _) => report.video_stems_without_matching_still += 1,
            (_, 0) => report.still_stems_without_matching_video += 1,
            (1, 1) => report.possible_still_motion_pairs += 1,
            _ => report.ambiguous_shared_stems += 1,
        }
    }
    Ok(report)
}

/// Without --execute or an explicit local fixture, this command NEVER opens
/// a cloud connection. Even --execute reads metadata only, not photo bytes.
pub(crate) fn inspect(
    cfg: &Config,
    source_name: &str,
    fixture: Option<&Path>,
    execute: bool,
    json: bool,
) -> Result<()> {
    let source = cfg.sources.iter().find(|s| s.name == source_name)
        .with_context(|| format!("unknown configured source {source_name}"))?;
    ensure!(matches!(source.backend, Backend::Rclone { .. }),
        "photo inventory diagnostic requires an rclone source");
    if fixture.is_none() && !execute {
        println!("PREVIEW ONLY: photos-inventory --source {source_name} --execute would perform one metadata-only rclone listing.");
        println!("No provider access, photo bytes, checkpoints, receipts or archive changes attempted.");
        return Ok(());
    }
    check_mount(cfg)?;
    let items = if let Some(path) = fixture {
        ensure!(path.is_absolute(), "private listing fixture must use absolute path");
        no_symlink_ancestors(path)?;
        let metadata = fs::symlink_metadata(path)?;
        ensure!(metadata.file_type().is_file(), "fixture is not a regular file");
        ensure!(metadata.len() <= MAX_PROVIDER_LISTING_BYTES,
            "private fixture exceeds bounded provider inventory size");
        let raw = fs::read(path)?;
        parse_rclone_inventory(&raw, &source.name)?
    } else {
        let listing = read_items(source, Duration::from_secs(180))?;
        ensure!(listing.exports.is_empty(),
            "unexpected Workspace export items in photo listing");
        listing.files
    };
    let report = summarize(&source.name, &items)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Read-only photo inventory for {}", report.source);
        println!("{} files ({} bytes): {} stills, {} videos, {} sidecars, {} other",
            report.listed_files, report.listed_bytes, report.still_images,
            report.motion_files, report.sidecars, report.other_or_unrecognized);
        println!("{} possible still/video pairs, {} ambiguous groups",
            report.possible_still_motion_pairs, report.ambiguous_shared_stems);
        println!("{} records without provider IDs, {} without modified time, {} zero-byte",
            report.missing_provider_id, report.missing_modified_time,
            report.zero_byte_files);
        println!("CAUTION: {}", report.caveat);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidate_pairs_are_same_folder_and_stem_and_never_claim_missing() {
        let fixture = serde_json::json!([
            {"Path":"2025/IMG_1001.HEIC","Size":1500,"ModTime":"2025-01-01","ID":"a"},
            {"Path":"2025/IMG_1001.MOV","Size":5000,"ModTime":"2025-01-01","ID":"b"},
            {"Path":"2025/IMG_1002.JPG","Size":1200,"ModTime":"2025-01-01"},
            {"Path":"2026/IMG_1002.MOV","Size":7000},
            {"Path":"2025/IMG_1002.AAE","Size":120},
            {"Path":"2025/custom.personaldata","Size":30}
        ]);
        let raw = serde_json::to_vec(&fixture).unwrap();
        let items = parse_rclone_inventory(&raw, "photos").unwrap();
        let report = summarize("photos", &items).unwrap();
        assert_eq!(report.listed_files, 6);
        assert_eq!(report.possible_still_motion_pairs, 1);
        assert_eq!(report.still_stems_without_matching_video, 1);
        assert_eq!(report.video_stems_without_matching_still, 1);
        assert_eq!(report.sidecars, 1);
        assert_eq!(report.missing_provider_id, 4);
        assert_eq!(report.missing_modified_time, 2);
        let json = serde_json::to_string(&report).unwrap();
        for secret in ["IMG_1001", "IMG_1002", "custom", "personaldata", "2025/", "2026/"] {
            assert!(!json.contains(secret), "private path leaked into report");
        }
        assert!(json.contains("Filename-based companion matches"));
    }

    #[test]
    fn duplicate_paths_and_missing_lengths_are_not_a_valid_inventory() {
        let duplicates = br#"[
            {"Path":"folder/photo.heic","Size":1},
            {"Path":"folder/photo.heic","Size":1}
        ]"#;
        assert!(parse_rclone_inventory(duplicates, "photos").is_err());
        let invalid = br#"[{"Path":"photo.heic"}]"#;
        assert!(parse_rclone_inventory(invalid, "photos").is_err());
    }

    #[test]
    fn empty_album_is_a_valid_observation_not_completeness_proof() {
        let items = parse_rclone_inventory(b"[]", "photos").unwrap();
        let report = summarize("photos", &items).unwrap();
        assert_eq!(report.listed_files, 0);
        assert_eq!(report.possible_still_motion_pairs, 0);
        assert!(report.caveat.contains("cannot establish complete"));
    }
}
