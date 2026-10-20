//! Offline comparison of explicit Apple-style asset/resource relationships.
//! rclone lsjson does not itself provide PhotoKit pairedVideo asset mapping.
//! No pairing is ever inferred from filenames; both inputs are private JSON.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use crate::{no_symlink_ancestors, parse_rclone_inventory, safe_relative,
    Backend, Config, Item, MAX_PROVIDER_LISTING_BYTES};

const MAX_ASSETS: usize = 1_000_000;
const MAX_RESOURCES: usize = 2_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Scope { Sample, ClaimedComplete }

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind { Photo, Video, LivePhoto, Other }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    schema_version: u32,
    scope: Scope,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    asset_id: String,
    kind: Kind,
    resources: Vec<Resource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    resource_type: String,
    path: Option<String>,
    size: Option<u64>,
}

#[derive(Serialize)]
struct Report {
    source: String,
    evidence_scope: &'static str,
    declared_assets: u64,
    declared_live_photo_assets: u64,
    declared_resources: u64,
    resources_with_explicit_paths: u64,
    resources_without_paths: u64,
    resources_without_sizes: u64,
    resources_missing_from_listing: u64,
    resource_size_conflicts: u64,
    unknown_resource_roles: u64,
    ambiguous_original_resource_roles: u64,
    live_assets_missing_original_photo_role: u64,
    live_assets_missing_original_paired_video_role: u64,
    live_assets_with_declared_original_roles: u64,
    live_assets_with_both_original_resources_listed: u64,
    listing_files: u64,
    listing_files_without_asset_mapping: u64,
    apple_completeness_proven: bool,
    caveat: &'static str,
}

fn recognized_role(role: &str) -> bool {
    matches!(role,
        "photo" | "video" | "audio" | "alternate_photo" |
        "paired_video" | "full_size_photo" | "full_size_video" |
        "full_size_paired_video" | "adjustment_data" |
        "adjustment_base_photo" | "adjustment_base_video" |
        "adjustment_base_paired_video")
}

fn parse_evidence(raw: &[u8]) -> Result<Evidence> {
    let evidence: Evidence = serde_json::from_slice(raw)
        .context("invalid normalized asset resource evidence JSON")?;
    ensure!(evidence.schema_version == 1, "unsupported asset evidence schema version");
    ensure!(!evidence.assets.is_empty() && evidence.assets.len() <= MAX_ASSETS,
        "empty or overly large asset evidence");
    let mut asset_ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut count = 0usize;
    for asset in &evidence.assets {
        ensure!(!asset.asset_id.is_empty() && asset.asset_id.len() <= 1024,
            "invalid private source asset ID");
        ensure!(asset_ids.insert(asset.asset_id.as_str()),
            "duplicate source asset identifier");
        ensure!(!asset.resources.is_empty(), "asset has no declared resources");
        count = count.checked_add(asset.resources.len())
            .context("resource count overflow")?;
        ensure!(count <= MAX_RESOURCES, "too many asset resources");
        for resource in &asset.resources {
            ensure!(!resource.resource_type.is_empty() &&
                resource.resource_type.len() <= 128 &&
                resource.resource_type.bytes().all(|b|
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "invalid normalized resource type");
            if let Some(path) = &resource.path {
                safe_relative(path)?;
                ensure!(paths.insert(path.as_str()),
                    "one listed path assigned to multiple resources");
            }
        }
    }
    Ok(evidence)
}

fn analyze(source: &str, evidence: &Evidence, listing: &[Item]) -> Report {
    let mut result = Report {
        source: source.into(),
        evidence_scope: if evidence.scope == Scope::Sample {
            "sample"
        } else { "claimed_complete" },
        declared_assets: 0, declared_live_photo_assets: 0,
        declared_resources: 0, resources_with_explicit_paths: 0,
        resources_without_paths: 0, resources_without_sizes: 0,
        resources_missing_from_listing: 0, resource_size_conflicts: 0,
        unknown_resource_roles: 0, ambiguous_original_resource_roles: 0,
        live_assets_missing_original_photo_role: 0,
        live_assets_missing_original_paired_video_role: 0,
        live_assets_with_declared_original_roles: 0,
        live_assets_with_both_original_resources_listed: 0,
        listing_files: listing.len() as u64,
        listing_files_without_asset_mapping: 0,
        apple_completeness_proven: false,
        caveat: "The private normalized asset-resource manifest supplies roles and asset identifiers, not rclone. Matching explicitly mapped files and reported sizes does not authenticate that manifest to Apple, verify media bytes, or prove all iCloud assets were enumerated. Filename matches are never accepted as relationships.",
    };
    let listed: HashMap<&str, u64> =
        listing.iter().map(|x| (x.path.as_str(), x.size)).collect();
    let mut mapped = HashSet::new();
    for asset in &evidence.assets {
        result.declared_assets += 1;
        let live = asset.kind == Kind::LivePhoto;
        if live { result.declared_live_photo_assets += 1; }
        let mut photo_roles = 0;
        let mut video_roles = 0;
        let mut photo_listed = false;
        let mut video_listed = false;
        for resource in &asset.resources {
            result.declared_resources += 1;
            if !recognized_role(&resource.resource_type) {
                result.unknown_resource_roles += 1;
            }
            let photo = resource.resource_type == "photo";
            let paired_video = resource.resource_type == "paired_video";
            if photo { photo_roles += 1; }
            if paired_video { video_roles += 1; }
            let Some(path) = resource.path.as_deref() else {
                result.resources_without_paths += 1;
                continue;
            };
            result.resources_with_explicit_paths += 1;
            mapped.insert(path);
            let Some(size) = resource.size else {
                result.resources_without_sizes += 1;
                continue;
            };
            match listed.get(path) {
                Some(actual) if *actual == size => {
                    if photo { photo_listed = true; }
                    if paired_video { video_listed = true; }
                }
                Some(_) => result.resource_size_conflicts += 1,
                None => result.resources_missing_from_listing += 1,
            }
        }
        if live {
            if photo_roles == 0 {
                result.live_assets_missing_original_photo_role += 1;
            }
            if video_roles == 0 {
                result.live_assets_missing_original_paired_video_role += 1;
            }
            if photo_roles > 1 || video_roles > 1 {
                result.ambiguous_original_resource_roles += 1;
            }
            if photo_roles == 1 && video_roles == 1 {
                result.live_assets_with_declared_original_roles += 1;
                if photo_listed && video_listed {
                    result.live_assets_with_both_original_resources_listed += 1;
                }
            }
        }
    }
    result.listing_files_without_asset_mapping = listing.iter()
        .filter(|item| !mapped.contains(item.path.as_str())).count() as u64;
    result
}

fn load(path: &Path) -> Result<Vec<u8>> {
    ensure!(path.is_absolute(), "private input path must be absolute");
    no_symlink_ancestors(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(metadata.file_type().is_file() &&
        metadata.len() <= MAX_PROVIDER_LISTING_BYTES,
        "invalid or excessively large private evidence input");
    fs::read(path).context("reading local private evidence")
}

pub(crate) fn run(cfg: &Config, source: &str, evidence: &Path,
    listing: &Path, json: bool) -> Result<()> {
    let configured = cfg.sources.iter().find(|s| s.name == source)
        .context("unknown configured photo source")?;
    ensure!(matches!(configured.backend, Backend::Rclone { .. }),
        "photo asset evidence requires a configured rclone source");
    // Strictly offline: no check_mount, no provider connection and no writes.
    let data = parse_evidence(&load(evidence)?)?;
    let entries = parse_rclone_inventory(&load(listing)?, "photo-evidence-listing")?;
    let report = analyze(source, &data, &entries);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Offline photo asset-resource evidence for {source}");
        println!("{} asset(s), {} declared Live Photo(s), {} resource(s)",
            report.declared_assets, report.declared_live_photo_assets,
            report.declared_resources);
        println!("{} Live Photo original still/video pairs appear in the saved listing",
            report.live_assets_with_both_original_resources_listed);
        println!("{} missing resource paths, {} missing listing files, {} size conflicts",
            report.resources_without_paths,
            report.resources_missing_from_listing, report.resource_size_conflicts);
        println!("CAUTION: {}", report.caveat);
    }
    let unresolved = report.resources_without_paths + report.resources_without_sizes
        + report.resources_missing_from_listing + report.resource_size_conflicts
        + report.unknown_resource_roles + report.ambiguous_original_resource_roles
        + report.live_assets_missing_original_photo_role
        + report.live_assets_missing_original_paired_video_role;
    if unresolved != 0 || (data.scope == Scope::ClaimedComplete &&
        report.listing_files_without_asset_mapping != 0)
    {
        bail!("asset evidence has unresolved relationships; no files modified");
    }
    ensure!(report.resources_with_explicit_paths > 0,
        "no explicit source relationships mapped to listed files");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(scope: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1, "scope": scope,
            "assets": [{
                "asset_id": "private-live-asset",
                "kind": "live_photo",
                "resources": [
                    {"resource_type": "photo", "path": "a.heic", "size": 4},
                    {"resource_type": "paired_video", "path": "a.mov", "size": 6}
                ]
            }]
        })).unwrap()
    }

    #[test]
    fn explicit_original_relationships_are_counted_without_leaking_ids() {
        let evidence = parse_evidence(&fixture("sample")).unwrap();
        let listing = parse_rclone_inventory(br#"[
            {"Path":"a.heic","Size":4},
            {"Path":"a.mov","Size":6},
            {"Path":"unrelated.mov","Size":8}
        ]"#, "test").unwrap();
        let report = analyze("photos", &evidence, &listing);
        assert_eq!(report.live_assets_with_both_original_resources_listed, 1);
        assert_eq!(report.listing_files_without_asset_mapping, 1);
        assert!(!report.apple_completeness_proven);
        let output = serde_json::to_string(&report).unwrap();
        assert!(!output.contains("private-live-asset"));
        assert!(!output.contains("unrelated.mov"));
    }

    #[test]
    fn an_edited_video_is_not_an_original_paired_video() {
        let mut raw: serde_json::Value =
            serde_json::from_slice(&fixture("sample")).unwrap();
        raw["assets"][0]["resources"][1]["resource_type"] =
            serde_json::json!("full_size_paired_video");
        let evidence = parse_evidence(&serde_json::to_vec(&raw).unwrap()).unwrap();
        let listing = parse_rclone_inventory(br#"[
            {"Path":"a.heic","Size":4},{"Path":"a.mov","Size":6}
        ]"#, "test").unwrap();
        let report = analyze("photos", &evidence, &listing);
        assert_eq!(report.live_assets_missing_original_paired_video_role, 1);
        assert_eq!(report.live_assets_with_both_original_resources_listed, 0);
    }

    #[test]
    fn malformed_duplicate_and_traversing_mappings_are_rejected() {
        let mut mapping: serde_json::Value =
            serde_json::from_slice(&fixture("sample")).unwrap();
        mapping["assets"][0]["resources"][1]["path"] =
            serde_json::json!("a.heic");
        assert!(parse_evidence(&serde_json::to_vec(&mapping).unwrap()).is_err());
        mapping["assets"][0]["resources"][1]["path"] =
            serde_json::json!("../outside");
        assert!(parse_evidence(&serde_json::to_vec(&mapping).unwrap()).is_err());
        assert!(parse_evidence(b"[]").is_err());
    }

    #[test]
    fn claimed_complete_scope_flags_unmapped_listed_files() {
        let evidence = parse_evidence(&fixture("claimed_complete")).unwrap();
        let listing = parse_rclone_inventory(br#"[
            {"Path":"a.heic","Size":4}, {"Path":"a.mov","Size":6},
            {"Path":"unmapped.jpg","Size":4}
        ]"#, "test").unwrap();
        let report = analyze("photos", &evidence, &listing);
        assert_eq!(report.evidence_scope, "claimed_complete");
        assert_eq!(report.listing_files_without_asset_mapping, 1);
    }
}
