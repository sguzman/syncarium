//! Bounded, local-only media header inspection for an existing archive.
//!
//! Recognized magic bytes can expose a mislabeled or truncated saved file,
//! but cannot establish original-quality media, correct photo/video pairing,
//! complete cloud enumeration or full-file content integrity.
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use crate::{
    no_symlink_ancestors, parse_rclone_inventory, safe_relative, Backend,
    Config, MAX_PROVIDER_LISTING_BYTES,
};

const MAX_ITEMS: usize = 10_000;
const HEADER_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signature {
    Jpeg, Png, Gif, Tiff, Webp, Heif, Avif, IsoVideo,
}

fn expected_signature(ext: &str) -> Option<Signature> {
    match ext {
        "jpg" | "jpeg" => Some(Signature::Jpeg),
        "png" => Some(Signature::Png),
        "gif" => Some(Signature::Gif),
        "tif" | "tiff" | "dng" => Some(Signature::Tiff),
        "webp" => Some(Signature::Webp),
        "heic" | "heif" => Some(Signature::Heif),
        "avif" => Some(Signature::Avif),
        "mov" | "mp4" | "m4v" => Some(Signature::IsoVideo),
        _ => None,
    }
}

fn iso_brand(header: &[u8], brand: &[u8; 4]) -> bool {
    header.get(8..12) == Some(brand.as_slice()) ||
        header.get(16..).is_some_and(|compatible| {
            compatible.chunks_exact(4).any(|part| part == brand)
        })
}

fn recognize(header: &[u8]) -> Option<Signature> {
    if header.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(Signature::Jpeg);
    }
    if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Signature::Png);
    }
    if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        return Some(Signature::Gif);
    }
    if header.starts_with(b"II\x2a\x00") || header.starts_with(b"MM\x00\x2a") {
        return Some(Signature::Tiff);
    }
    if header.len() >= 12 && header.starts_with(b"RIFF") && header.get(8..12) == Some(b"WEBP") {
        return Some(Signature::Webp);
    }
    // ISO BMFF containers: first box length and ftyp must fit this file.
    // Generic "mif1" by itself does NOT certify HEIC (can be other HEIF).
    if header.len() >= 16 && header.get(4..8) == Some(b"ftyp") {
        let box_size = u32::from_be_bytes(header[0..4].try_into().ok()?) as usize;
        if box_size >= 16 {
            let brands = &header[..header.len().min(box_size)];
            if [b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis"]
                .iter().any(|brand| iso_brand(brands, brand))
            {
                return Some(Signature::Heif);
            }
            if [b"avif", b"avis"].iter().any(|brand| iso_brand(brands, brand)) {
                return Some(Signature::Avif);
            }
            if [b"qt  ", b"isom", b"iso2", b"iso5", b"mp41", b"mp42", b"M4V "]
                .iter().any(|brand| iso_brand(brands, brand))
            {
                return Some(Signature::IsoVideo);
            }
        }
    }
    None
}

#[derive(Serialize)]
struct Report {
    source: String,
    eligible_formats_in_manifest: u64,
    checked_files: u64,
    signature_matches: u64,
    recognized_mismatches: u64,
    inconclusive_headers: u64,
    missing_files: u64,
    size_conflicts: u64,
    unsafe_or_unreadable_files: u64,
    unsupported_formats_skipped: u64,
    eligible_not_checked_due_to_limit: u64,
    max_files: usize,
    caveat: &'static str,
}

fn inspect_local(
    root: &Path,
    source_name: &str,
    manifest: &[u8],
    max_files: usize,
) -> Result<Report> {
    ensure!((1..=MAX_ITEMS).contains(&max_files),
        "maximum files must be between 1 and {MAX_ITEMS}");
    ensure!(root.is_dir(), "configured archive destination missing");
    no_symlink_ancestors(root)?;
    let mut items = parse_rclone_inventory(manifest, source_name)?;
    // Deterministic sample; no retries or scanning beyond the specified cap.
    items.sort_by(|a, b| a.path.cmp(&b.path));
    let mut report = Report {
        source: source_name.into(),
        eligible_formats_in_manifest: 0,
        checked_files: 0,
        signature_matches: 0,
        recognized_mismatches: 0,
        inconclusive_headers: 0,
        missing_files: 0,
        size_conflicts: 0,
        unsafe_or_unreadable_files: 0,
        unsupported_formats_skipped: 0,
        eligible_not_checked_due_to_limit: 0,
        max_files,
        caveat: "Magic-byte matches show only basic file-container compatibility. They do NOT establish an original Apple asset, Live Photo relationship, full-file integrity, or complete iCloud enumeration. Unknown headers are inconclusive, not verified.",
    };
    for item in &items {
        let rel = safe_relative(&item.path)?;
        let ext = rel.extension().and_then(|x| x.to_str())
            .unwrap_or("").to_ascii_lowercase();
        let Some(expected) = expected_signature(&ext) else {
            report.unsupported_formats_skipped += 1;
            continue;
        };
        report.eligible_formats_in_manifest += 1;
        if report.checked_files >= max_files as u64 {
            report.eligible_not_checked_due_to_limit += 1;
            continue;
        }
        report.checked_files += 1;
        let target = root.join(&rel);
        if no_symlink_ancestors(&target).is_err() {
            report.unsafe_or_unreadable_files += 1;
            continue;
        }
        let metadata = match fs::symlink_metadata(&target) {
            Ok(value) => value,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.missing_files += 1;
                continue;
            }
            Err(_) => {
                report.unsafe_or_unreadable_files += 1;
                continue;
            }
        };
        if !metadata.file_type().is_file() {
            report.unsafe_or_unreadable_files += 1;
            continue;
        }
        if metadata.len() != item.size {
            report.size_conflicts += 1;
            continue;
        }
        let mut file = match File::open(&target) {
            Ok(f) => f,
            Err(_) => {
                report.unsafe_or_unreadable_files += 1;
                continue;
            }
        };
        // Avoid counting a file swapped between metadata inspection and open.
        let opened_metadata = match file.metadata() {
            Ok(value) => value,
            Err(_) => {
                report.unsafe_or_unreadable_files += 1;
                continue;
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.dev() != opened_metadata.dev() ||
                metadata.ino() != opened_metadata.ino() ||
                metadata.len() != opened_metadata.len()
            {
                report.unsafe_or_unreadable_files += 1;
                continue;
            }
        }
        let mut header = [0u8; HEADER_BYTES];
        let n = match file.read(&mut header) {
            Ok(n) => n,
            Err(_) => {
                report.unsafe_or_unreadable_files += 1;
                continue;
            }
        };
        match recognize(&header[..n]) {
            Some(actual) if actual == expected => report.signature_matches += 1,
            Some(_) => report.recognized_mismatches += 1,
            None => report.inconclusive_headers += 1,
        }
    }
    Ok(report)
}

pub(crate) fn run(
    cfg: &Config,
    source_name: &str,
    fixture: &Path,
    max_files: usize,
    execute: bool,
    json: bool,
) -> Result<()> {
    let source = cfg.sources.iter().find(|s| s.name == source_name)
        .context("unknown configured photo source")?;
    ensure!(matches!(source.backend, Backend::Rclone { .. }),
        "photo header checks require an rclone archive source");
    ensure!((1..=MAX_ITEMS).contains(&max_files),
        "max-files must be between 1 and {MAX_ITEMS}");
    if !execute {
        println!("PREVIEW ONLY: photos-headers --source {source_name} --execute would read at most {HEADER_BYTES} header bytes from each of at most {max_files} listed local photo/video files.");
        println!("It would use the private local manifest, never contact the cloud, and never modify archive files.");
        return Ok(());
    }
    // Respect the configured mount policy only for actual archive reads.
    crate::check_mount(cfg)?;
    ensure!(fixture.is_absolute(), "manifest fixture path must be absolute");
    no_symlink_ancestors(fixture)?;
    let meta = fs::symlink_metadata(fixture)?;
    ensure!(meta.file_type().is_file() && meta.len() <= MAX_PROVIDER_LISTING_BYTES,
        "invalid or oversized private manifest fixture");
    let raw = fs::read(fixture).context("reading bounded private photo manifest")?;
    let report = inspect_local(&source.destination, source_name, &raw, max_files)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Local-only media signature check: {}", report.source);
        println!("{} checked, {} matching signatures, {} known mismatches, {} inconclusive",
            report.checked_files, report.signature_matches, report.recognized_mismatches,
            report.inconclusive_headers);
        println!("{} missing, {} size conflicts, {} unsafe/unreadable, {} eligible not checked",
            report.missing_files, report.size_conflicts, report.unsafe_or_unreadable_files,
            report.eligible_not_checked_due_to_limit);
        println!("Caution: {}", report.caveat);
    }
    let defects = report.recognized_mismatches + report.missing_files
        + report.size_conflicts + report.unsafe_or_unreadable_files;
    if defects > 0 {
        bail!("{defects} local media header or archive size failure(s); no files modified");
    }
    ensure!(report.checked_files > 0, "no supported media files sampled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_local_media_headers_without_decoding_full_media() {
        let jpeg = [0xff, 0xd8, 0xff, 0xe1];
        assert_eq!(recognize(&jpeg), Some(Signature::Jpeg));
        assert_eq!(recognize(b"\x89PNG\r\n\x1a\nabc"), Some(Signature::Png));
        assert_eq!(recognize(b"GIF89a"), Some(Signature::Gif));
        assert_eq!(recognize(b"II\x2a\x00test"), Some(Signature::Tiff));
        assert_eq!(recognize(b"RIFF1234WEBPVP8 "), Some(Signature::Webp));
        assert_eq!(recognize(b"\0\0\0\x18ftypheic\0\0\0\0mif1"), Some(Signature::Heif));
        assert_eq!(recognize(b"\0\0\0\x18ftypavif\0\0\0\0mif1"), Some(Signature::Avif));
        assert_eq!(recognize(b"\0\0\0\x18ftypqt  \0\0\0\0qt  "), Some(Signature::IsoVideo));
        assert_eq!(recognize(b"\0\0\0\x18ftypmif1\0\0\0\0mif1"), None,
            "generic HEIF brands alone must not falsely certify HEIC");
        assert_eq!(recognize(b"garbage"), None);
    }

    #[test]
    fn reports_invalid_and_missing_local_media_without_exposing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("correct.jpg"), b"\xff\xd8\xff\xe0").unwrap();
        fs::write(tmp.path().join("wrong.heic"), b"\xff\xd8\xff\xe0").unwrap();
        fs::write(tmp.path().join("unrecognized.mov"), b"????").unwrap();
        let fixture = serde_json::json!([
            {"Path":"correct.jpg","Size":4},
            {"Path":"wrong.heic","Size":4},
            {"Path":"unrecognized.mov","Size":4},
            {"Path":"missing.png","Size":8},
            {"Path":"not-a-media-file.xyz","Size":123}
        ]);
        let bytes = serde_json::to_vec(&fixture).unwrap();
        let report = inspect_local(tmp.path(), "photos", &bytes, 16).unwrap();
        assert_eq!(report.checked_files, 4);
        assert_eq!(report.signature_matches, 1);
        assert_eq!(report.recognized_mismatches, 1);
        assert_eq!(report.inconclusive_headers, 1);
        assert_eq!(report.missing_files, 1);
        assert_eq!(report.unsupported_formats_skipped, 1);
        let json = serde_json::to_string(&report).unwrap();
        for private in ["wrong.heic", "missing.png", "correct.jpg", "not-a-media-file"] {
            assert!(!json.contains(private));
        }
    }

    #[test]
    fn deterministic_sampling_never_claims_unchecked_media_was_scanned() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.jpg"), b"\xff\xd8\xff\xe0").unwrap();
        fs::write(tmp.path().join("b.jpg"), b"\xff\xd8\xff\xe0").unwrap();
        let fixture = br#"[
            {"Path":"b.jpg","Size":4},{"Path":"a.jpg","Size":4}
        ]"#;
        let report = inspect_local(tmp.path(), "photos", fixture, 1).unwrap();
        assert_eq!(report.signature_matches, 1);
        assert_eq!(report.checked_files, 1);
        assert_eq!(report.eligible_formats_in_manifest, 2);
        assert_eq!(report.eligible_not_checked_due_to_limit, 1);
    }
}
