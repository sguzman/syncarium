//! Append-only remote-observation ledger.
//!
//! Keeps only SHA-256 path fingerprints, not filenames or credentials.
//! Never interprets an absent item as permission to delete a local file.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Ledger {
    format: u32,
    hashes: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Observation {
    pub(crate) ever_seen: u64,
    pub(crate) absent: u64,
    pub(crate) new: u64,
}

fn ledger_path(root: &Path, source: &str) -> PathBuf {
    root.join(".syncarium").join("inventories").join(format!("{source}.json"))
}

fn fingerprint(source: &str, path: &str) -> String {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    h.update([0]);
    h.update(path.as_bytes());
    hex::encode(h.finalize())
}

/// Compare a provider listing with every previously observed path.
/// Execute mode merges new paths into persistent state; dry runs only read.
pub(crate) fn observe<'a>(
    root: &Path,
    source: &str,
    paths: impl Iterator<Item = &'a str>,
    persist: bool,
) -> Result<Observation> {
    ensure!(
        !source.is_empty()
            && source
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid inventory source name"
    );
    let path = ledger_path(root, source);
    for parent in path.ancestors() {
        if parent.is_symlink() {
            bail!("inventory ledger path must not contain a symlink");
        }
    }
    let mut known = if path.exists() {
        let old: Ledger = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("loading inventory ledger for {source}"))?;
        ensure!(old.format == FORMAT, "unsupported inventory ledger version");
        let mut seen = BTreeSet::new();
        for hash in old.hashes {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid inventory fingerprint for {source}"
            );
            ensure!(seen.insert(hash), "duplicate fingerprints in inventory ledger");
        }
        seen
    } else {
        BTreeSet::new()
    };
    let current: BTreeSet<_> = paths.map(|item| fingerprint(source, item)).collect();
    let absent = known.difference(&current).count() as u64;
    let new = current.difference(&known).count() as u64;
    if persist && new > 0 {
        known.extend(current.iter().cloned());
        let folder = path.parent().context("missing inventory directory")?;
        fs::create_dir_all(folder)?;
        ensure!(!folder.is_symlink(), "inventory directory is a symlink");
        let mut staging = NamedTempFile::new_in(folder)?;
        serde_json::to_writer(
            &mut staging,
            &Ledger { format: FORMAT, hashes: known.iter().cloned().collect() },
        )?;
        staging.flush()?;
        staging.as_file().sync_all()?;
        staging
            .persist(&path)
            .map_err(|err| err.error)
            .context("atomically persisting inventory ledger")?;
    }
    Ok(Observation {
        ever_seen: (known.len() as u64).max(
            (current.len() as u64).saturating_add(absent)
        ),
        absent,
        new,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn append_only_ledger_detects_disappearance_and_reappearance() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let first = observe(root, "photos", ["a.heic", "b.mov"].into_iter(), false).unwrap();
        assert_eq!((first.ever_seen, first.new, first.absent), (2, 2, 0));
        assert!(!ledger_path(root, "photos").exists(), "read-only scan must not write");
        let first = observe(root, "photos", ["a.heic", "b.mov"].into_iter(), true).unwrap();
        assert_eq!((first.ever_seen, first.new, first.absent), (2, 2, 0));
        let current = observe(root, "photos", ["a.heic"].into_iter(), true).unwrap();
        assert_eq!((current.ever_seen, current.new, current.absent), (2, 0, 1));
        let ledger: Ledger = serde_json::from_slice(&fs::read(ledger_path(root, "photos")).unwrap()).unwrap();
        assert_eq!(ledger.hashes.len(), 2, "an absent source path must remain in history");
        let current = observe(root, "photos", ["a.heic", "b.mov", "c.jpg"].into_iter(), true).unwrap();
        assert_eq!((current.ever_seen, current.new, current.absent), (3, 1, 0));
    }
    #[test]
    fn empty_listing_does_not_erase_history() {
        let temp = tempfile::tempdir().unwrap();
        observe(temp.path(), "drive", ["x"].into_iter(), true).unwrap();
        let scan = observe(temp.path(), "drive", std::iter::empty(), true).unwrap();
        assert_eq!((scan.ever_seen, scan.absent, scan.new), (1, 1, 0));
        let raw = fs::read_to_string(ledger_path(temp.path(), "drive")).unwrap();
        assert!(!raw.contains("\"x\""), "ledger must not expose raw private filenames");
    }
    #[test]
    fn malformed_state_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let target = ledger_path(temp.path(), "drive");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, b"{\"format\":2,\"hashes\":[]}").unwrap();
        assert!(observe(temp.path(), "drive", ["x"].into_iter(), false).is_err());
    }
}
