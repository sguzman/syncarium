//! Local SHA-256 receipts for content created by Syncarium.
//!
//! Receipts identify the bytes that were staged and published; they do not
//! prove that the remote listed every item. Existing legacy archive files
//! are never assigned invented provenance.
use anyhow::{anyhow, bail, ensure, Context, Result};
use md5::Md5;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use crate::{check_mount, no_symlink_ancestors, safe_relative, sync_directory, Config};

const FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Receipt {
    format: u32,
    source: String,
    path: String,
    size: u64,
    source_version: String,
    sha256: String,
    /// A prepared receipt is not yet proof of completed hard-link publication.
    #[serde(default)]
    published: bool,
}

#[derive(Default, Serialize)]
struct SourceReport {
    name: String,
    receipts: u64,
    verified: u64,
    missing: u64,
    mismatched: u64,
    invalid_receipts: u64,
}

#[derive(Serialize)]
struct VerifyReport {
    sources: Vec<SourceReport>,
    warning: &'static str,
}

fn receipt_path(root: &Path, source: &str, path: &str) -> PathBuf {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    h.update([0]);
    h.update(path.as_bytes());
    let key = hex::encode(h.finalize());
    root.join(".syncarium").join("receipts").join(source)
        .join(&key[..2]).join(format!("{key}.json"))
}

/// Compute a SHA-256 receipt while validating any provided provider checksum.
/// This is one streaming pass, independent of the file size.
pub(crate) fn fingerprint_file(
    file: &Path,
    md5_expected: Option<&str>,
    sha_expected: Option<&str>,
) -> Result<String> {
    no_symlink_ancestors(file)?;
    ensure!(fs::symlink_metadata(file)?.file_type().is_file(),
        "cannot fingerprint a non-regular staged file");
    let mut input = File::open(file)?;
    let mut sha = Sha256::new();
    let mut md5 = Md5::new();
    let mut buffer = [0u8; 256 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        sha.update(&buffer[..n]);
        md5.update(&buffer[..n]);
    }
    let digest = hex::encode(sha.finalize());
    if let Some(expected) = md5_expected {
        ensure!(hex::encode(md5.finalize()) == expected,
            "provider MD5 mismatch; preserving staged bytes");
    }
    if let Some(expected) = sha_expected {
        ensure!(digest == expected,
            "provider SHA-256 mismatch; preserving staged bytes");
    }
    Ok(digest)
}

/// Record the completed staging fingerprint BEFORE its no-clobber hard link.
/// A receipt with no final target is counted as missing, not verified.
/// Existing receipts must match exactly; never replace recorded provenance.
pub(crate) fn record(
    root: &Path,
    source: &str,
    relative: &str,
    source_version: &str,
    size: u64,
    sha256: &str,
) -> Result<()> {
    safe_relative(relative)?;
    ensure!(sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid fingerprint");
    let new = Receipt {
        format: FORMAT, source: source.into(), path: relative.into(),
        size, source_version: source_version.into(), sha256: sha256.into(),
        published: false,
    };
    let path = receipt_path(root, source, relative);
    no_symlink_ancestors(&path)?;
    if path.exists() {
        let mut recorded: Receipt = serde_json::from_slice(&fs::read(&path)?)
            .context("reading existing immutable archive receipt")?;
        // Publication can transition only from prepared to published.
        // It must never alter original content identity or source revision.
        recorded.published = false;
        ensure!(recorded == new,
            "archive receipt identity or content changed; preserve local state for review");
        return Ok(());
    }
    let parent = path.parent().context("receipt parent missing")?;
    fs::create_dir_all(parent)?;
    no_symlink_ancestors(parent)?;
    let mut staged = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut staged, &new)?;
    staged.flush()?;
    staged.as_file().sync_all()?;
    // Never overwrite a concurrently created or preexisting immutable receipt.
    staged.persist_noclobber(&path)
        .map_err(|e| anyhow!("cannot create immutable receipt: {}", e.error))?;
    sync_directory(parent)?;
    Ok(())
}

/// The sole legal mutation of a completed receipt: prepared -> published.
/// A successful final hard link must already have been synced to disk.
pub(crate) fn mark_published(
    root: &Path,
    source: &str,
    relative: &str,
    source_version: &str,
    size: u64,
    sha256: &str,
) -> Result<()> {
    let path = receipt_path(root, source, relative);
    no_symlink_ancestors(&path)?;
    let mut existing: Receipt = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(existing.format == FORMAT && existing.source == source
        && existing.path == relative && existing.source_version == source_version
        && existing.size == size && existing.sha256 == sha256,
        "cannot finalize unrelated or mismatched archive receipt");
    if existing.published {
        return Ok(());
    }
    existing.published = true;
    let parent = path.parent().context("receipt directory missing")?;
    let mut staging = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut staging, &existing)?;
    staging.flush()?;
    staging.as_file().sync_all()?;
    staging.persist(&path)
        .map_err(|err| anyhow!("cannot finalize publication receipt: {}", err.error))?;
    sync_directory(parent)
}

/// Workspace outputs must have a receipt from this source revision. A local
/// file with no receipt or an old revision is a conflict, not a success.
/// Rehashing is intentionally left to the explicit disk-wide verify command.
pub(crate) fn check_export_target(
    root: &Path,
    source: &str,
    relative: &str,
    source_version: &str,
    target: &Path,
    reconcile: bool,
) -> Result<()> {
    ensure!(!source_version.is_empty(), "Workspace source revision not recorded");
    safe_relative(relative)?;
    no_symlink_ancestors(target)?;
    let metadata = fs::symlink_metadata(target)?;
    ensure!(metadata.file_type().is_file(), "Workspace output is not a regular file");
    let receipt_file = receipt_path(root, source, relative);
    no_symlink_ancestors(&receipt_file)?;
    let record: Receipt = serde_json::from_slice(&fs::read(receipt_file)
        .context("Workspace output has no matching Syncarium receipt")?)?;
    ensure!(record.format == FORMAT && record.source == source
        && record.path == relative && record.source_version == source_version,
        "Workspace revision changed or receipt does not match");
    ensure!(record.size == metadata.len(),
        "Workspace output size differs from receipt");
    if !record.published {
        // This can occur after a crash between creating a receipt and
        // linking the destination. A same-sized unrelated file must NEVER
        // be mistaken for a successfully archived Google export.
        let hash = fingerprint_file(target, None, None)?;
        ensure!(hash == record.sha256,
            "unfinalized Workspace receipt does not match archived bytes");
        if reconcile {
            mark_published(root, source, relative, source_version, record.size, &hash)?;
        }
    }
    Ok(())
}

/// Explicit, read-only disk integrity inspection. Does not read cloud APIs,
/// copy archive objects, or turn old unreceipted files into verified ones.
pub(crate) fn verify(cfg: &Config, selected: Option<&str>, json: bool) -> Result<()> {
    check_mount(cfg)?;
    if let Some(name) = selected {
        ensure!(cfg.sources.iter().any(|s| s.name == name),
            "unknown configured source: {name}");
    }
    let root = cfg.archive_root.join(".syncarium").join("receipts");
    no_symlink_ancestors(&root)?;
    let mut report = VerifyReport {
        sources: Vec::new(),
        warning: "Only files completed by Syncarium with local SHA-256 receipts are checked. Preexisting archives without receipts are NOT verified; no cloud completeness claim.",
    };
    let mut failures = 0u64;
    for source in &cfg.sources {
        if selected.is_some_and(|name| name != source.name) {
            continue;
        }
        let mut row = SourceReport { name: source.name.clone(), ..SourceReport::default() };
        let folder = root.join(&source.name);
        no_symlink_ancestors(&folder)?;
        if folder.exists() {
            for bucket in fs::read_dir(&folder)? {
                let bucket = bucket?;
                ensure!(bucket.file_type()?.is_dir() && !bucket.file_type()?.is_symlink(),
                    "unsafe receipt directory");
                for entry in fs::read_dir(bucket.path())? {
                    let entry = entry?;
                    if entry.path().extension().is_none_or(|ext| ext != "json") {
                        continue; // ignore a temporary file from a killed writer
                    }
                    row.receipts += 1;
                    if !entry.file_type()?.is_file() || entry.path().is_symlink() {
                        row.invalid_receipts += 1;
                        continue;
                    }
                    let receipt: Receipt = match serde_json::from_slice(&fs::read(entry.path())?) {
                        Ok(value) => value,
                        Err(_) => {
                            row.invalid_receipts += 1;
                            continue;
                        }
                    };
                    if receipt.format != FORMAT || receipt.source != source.name
                        || safe_relative(&receipt.path).is_err()
                        || receipt.sha256.len() != 64
                        || !receipt.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                        || entry.path() != receipt_path(&cfg.archive_root, &source.name, &receipt.path)
                    {
                        row.invalid_receipts += 1;
                        continue;
                    }
                    let target = source.destination.join(&receipt.path);
                    if no_symlink_ancestors(&target).is_err() {
                        row.mismatched += 1;
                        continue;
                    }
                    let metadata = match fs::symlink_metadata(&target) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            row.missing += 1;
                            continue;
                        }
                        Err(_) => {
                            row.mismatched += 1;
                            continue;
                        }
                    };
                    if !metadata.file_type().is_file() || metadata.len() != receipt.size {
                        row.mismatched += 1;
                        continue;
                    }
                    match fingerprint_file(&target, None, None) {
                        Ok(checksum) if checksum == receipt.sha256 => row.verified += 1,
                        _ => row.mismatched += 1,
                    }
                }
            }
        }
        failures += row.missing + row.mismatched + row.invalid_receipts;
        report.sources.push(row);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Read-only SHA-256 verification of Syncarium receipts");
        for row in &report.sources {
            println!("{}: {} receipts, {} verified, {} missing, {} mismatched, {} invalid receipts",
                row.name, row.receipts, row.verified,
                row.missing, row.mismatched, row.invalid_receipts);
        }
        println!("WARNING: {}", report.warning);
    }
    if failures > 0 {
        bail!("{failures} local archive receipt verification failure(s); no files modified");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_workspace_receipt_never_certifies_an_unrelated_same_size_file() {
        let tmp = tempfile::tempdir().unwrap();
        let relative = "workspace-exports/aa/locked.docx";
        let source = "workspace";
        let target = tmp.path().join("archive.docx");
        let correct = hex::encode(Sha256::digest(b"first"));
        record(tmp.path(), source, relative, "revision-1", 5, &correct).unwrap();
        fs::write(&target, b"other").unwrap();
        assert!(check_export_target(tmp.path(), source, relative,
            "revision-1", &target, true).is_err());
        // Failed reconciliation may neither rewrite the wrong file nor
        // certify the prepared receipt.
        assert_eq!(fs::read(&target).unwrap(), b"other");
        let stored: Receipt = serde_json::from_slice(
            &fs::read(receipt_path(tmp.path(), source, relative)).unwrap()).unwrap();
        assert!(!stored.published);

        fs::write(&target, b"first").unwrap();
        check_export_target(tmp.path(), source, relative, "revision-1", &target, false)
            .unwrap();
        let stored: Receipt = serde_json::from_slice(
            &fs::read(receipt_path(tmp.path(), source, relative)).unwrap()).unwrap();
        assert!(!stored.published, "read-only inspection must not alter receipt");
        check_export_target(tmp.path(), source, relative, "revision-1", &target, true)
            .unwrap();
        let stored: Receipt = serde_json::from_slice(
            &fs::read(receipt_path(tmp.path(), source, relative)).unwrap()).unwrap();
        assert!(stored.published);
        record(tmp.path(), source, relative, "revision-1", 5, &correct).unwrap();
        assert!(record(tmp.path(), source, relative, "revision-2", 5, &correct).is_err());
    }

    #[test]
    fn immutable_receipt_can_be_reused_but_never_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = hex::encode(Sha256::digest(b"sample"));
        record(tmp.path(), "photos", "year/photo.heic", "v1", 6, &sha).unwrap();
        record(tmp.path(), "photos", "year/photo.heic", "v1", 6, &sha).unwrap();
        assert!(record(tmp.path(), "photos", "year/photo.heic", "v2", 6, &sha).is_err());
        let saved = fs::read(receipt_path(tmp.path(), "photos", "year/photo.heic")).unwrap();
        let receipt: Receipt = serde_json::from_slice(&saved).unwrap();
        assert_eq!(receipt.source_version, "v1");
    }

    #[test]
    fn a_receipt_does_not_certify_a_missing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let source_dest = tmp.path().join("photos");
        fs::create_dir_all(&source_dest).unwrap();
        let sha = hex::encode(Sha256::digest(b"sample"));
        record(tmp.path(), "photos", "year/photo.heic", "v1", 6, &sha).unwrap();
        let cfg: Config = toml::from_str(&format!(
            "archive_root = {:?}\n[scheduler]\nbudget_mib = 1\nchunk_mib = 1\nbandwidth_kib = 1024\nrun_seconds = 120\n[[sources]]\nname = \"photos\"\nenabled = true\nkind = \"rclone\"\nremote = \"unused:\"\ndestination = {:?}",
            tmp.path().to_str().unwrap(), source_dest.to_str().unwrap()
        )).unwrap();
        assert!(verify(&cfg, None, false).is_err(), "missing target is not verified");
    }

    #[test]
    fn a_local_fingerprint_detects_changed_content_of_same_length() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("blob");
        fs::write(&file, b"sample").unwrap();
        let hash = fingerprint_file(&file, None, None).unwrap();
        assert_eq!(hash, hex::encode(Sha256::digest(b"sample")));
        assert!(fingerprint_file(&file, None, Some(&hash)).is_ok());
        fs::write(&file, b"tamper").unwrap();
        assert!(fingerprint_file(&file, None, Some(&hash)).is_err());
    }
}
