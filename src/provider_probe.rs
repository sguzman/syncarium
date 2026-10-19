//! Explicit, small read-only probe for rclone-backed cloud ranges.
//!
//! iCloud's rclone backend is experimental. Correctly sized ranged reads
//! are not evidence that offset handling or cloud enumeration is correct.
//! Three overlapping windows detect common ignored-offset errors. The
//! command never creates checkpoints, receipts or archive publications.
use anyhow::{bail, ensure, Context, Result};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

use crate::{check_mount, download_range, read_items, Backend, Config, Item, Source};

const WINDOW: u64 = 65_536;
const STEP: u64 = WINDOW / 2;
const MIN_ITEM_SIZE: u64 = WINDOW + STEP * 2;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

fn is_rclone(source: &Source) -> bool {
    matches!(source.backend, Backend::Rclone { .. })
}

fn select_item<'a>(items: &'a [Item]) -> Result<&'a Item> {
    items.iter().find(|item| item.size >= MIN_ITEM_SIZE)
        .context("no provider-listed file large enough for overlapping range probe (128 KiB required)")
}

/// Three bounded requests: [0,64K), [32K,96K), [64K,128K).
/// Overlap equality is necessary for correct range behavior, though it
/// cannot prove offset handling for unusually repetitive file content.
fn inspect_windows(a: &[u8], b: &[u8], c: &[u8]) -> Result<()> {
    let window = WINDOW as usize;
    let step = STEP as usize;
    ensure!(a.len() == window && b.len() == window && c.len() == window,
        "provider returned incorrectly sized range-probe windows");
    ensure!(a[step..] == b[..step] && b[step..] == c[..step],
        "provider ignored or misapplied range offsets; archive left untouched");
    if a == b && b == c {
        bail!("range probe inconclusive: sampled file has repeating windows");
    }
    Ok(())
}

pub(crate) fn probe(cfg: &Config, source_name: &str, execute: bool) -> Result<()> {
    let source = cfg.sources.iter().find(|s| s.name == source_name)
        .with_context(|| format!("unknown configured source: {source_name}"))?;
    ensure!(is_rclone(source), "range probe currently supports only rclone-backed sources");
    if !execute {
        println!("PREVIEW ONLY: probe-range --source {source_name} --execute would list this source and read three 64 KiB overlapping byte windows (192 KiB total).");
        println!("No cloud connection, download, checkpoint or archive change attempted.");
        return Ok(());
    }
    check_mount(cfg)?;
    ensure!(source.destination.is_dir(), "source destination missing");
    let listing = read_items(source, Duration::from_secs(180))
        .context("could not obtain a usable cloud listing for range probe")?;
    ensure!(listing.unresolved == 0, "source listing has unresolved objects");
    let item = select_item(&listing.files)?;
    let scratch = tempdir().context("creating disposable local probe workspace")?;
    let windows: [(&str, u64); 3] = [
        ("first", 0), ("middle", STEP), ("last", STEP * 2)
    ];
    let mut data = Vec::new();
    for (name, offset) in windows {
        let path = scratch.path().join(name);
        fs::write(&path, [])?;
        download_range(source, item, offset, WINDOW,
            cfg.scheduler.bandwidth_kib, PROBE_TIMEOUT, &path)
            .context("bounded provider range probe failed")?;
        data.push(fs::read(&path)?);
    }
    inspect_windows(&data[0], &data[1], &data[2])?;
    println!("PASS: {source_name} returned three correctly sized, consistent overlapping byte windows.");
    println!("192 KiB payload inspected; no archive files, checkpoints or receipts written.");
    println!("This does not prove full iCloud Photos enumeration, original-quality export, or long-file resume correctness.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_ranges_detect_ignored_offsets() {
        let bytes: Vec<u8> = (0..MIN_ITEM_SIZE as usize)
            .map(|i| (i % 251) as u8).collect();
        let step = STEP as usize;
        let end = WINDOW as usize;
        let first = &bytes[0..end];
        let middle = &bytes[step..step + end];
        let last = &bytes[step * 2..step * 2 + end];
        inspect_windows(first, middle, last).unwrap();
        assert!(inspect_windows(first, first, first).is_err(),
            "ignored offsets must not be mistaken for success");
        assert!(inspect_windows(first, middle, first).is_err(),
            "wrong offset in the third request must fail");
    }

    #[test]
    fn uniform_content_is_not_a_conclusive_range_probe() {
        let all_zero = vec![0; WINDOW as usize];
        let err = inspect_windows(&all_zero, &all_zero, &all_zero).unwrap_err();
        assert!(err.to_string().contains("inconclusive"));
    }
}
