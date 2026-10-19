//! Syncarium: a bounded, append-only downloader for heterogeneous sources.
//! All transfers are opt-in; see docs/LOCAL_SETUP.md before enabling systemd.
use anyhow::{anyhow, bail, ensure, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, RANGE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

#[derive(Parser)]
#[command(about = "One-way, bandwidth-budgeted archival across independent sources")]
struct Cli {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Validate configuration and basic local prerequisites; no remote downloads.
    Doctor,
    /// Enumerate configured sources and show pending counts; no downloads.
    Plan,
    /// Default is preview; --execute is required to download.
    Run { #[arg(long)] execute: bool },
    /// Print local state paths; no cloud calls.
    Status,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    archive_root: PathBuf,
    required_mount: Option<PathBuf>,
    scheduler: Scheduler,
    sources: Vec<Source>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scheduler {
    budget_mib: u64,
    chunk_mib: u64,
    bandwidth_kib: u64,
    run_seconds: u64,
}
#[derive(Debug, Clone, Deserialize)]
struct Source {
    name: String,
    enabled: bool,
    destination: PathBuf,
    #[serde(flatten)]
    backend: Backend,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Backend {
    Rclone { remote: String },
    HttpManifest { manifest: PathBuf },
}
#[derive(Clone, Debug)]
struct Item {
    path: String,
    size: u64,
    version: String,
    url: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Checkpoint {
    source: String,
    path: String,
    size: u64,
    version: String,
    committed: u64,
    last_offset: u64,
    last_size: u64,
    last_sha256: String,
}
#[derive(Deserialize)]
struct HttpEntry {
    path: String,
    size: u64,
    url: String,
    version: String,
}
#[derive(Serialize, Deserialize, Default)]
struct Rotation {
    next: usize,
}
struct Pending {
    source: Source,
    files: Vec<Item>,
    index: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let content = fs::read_to_string(&cli.config)
        .with_context(|| format!("reading {}", cli.config.display()))?;
    let cfg: Config = toml::from_str(&content).context("parsing TOML")?;
    validate(&cfg)?;
    match cli.command {
        Action::Doctor => {
            check_mount(&cfg)?;
            println!("Configuration valid: {} source(s); {} enabled", cfg.sources.len(),
                cfg.sources.iter().filter(|s| s.enabled).count());
            println!("Archive root: {}", cfg.archive_root.display());
            println!("No cloud connections or downloads attempted.");
        }
        Action::Status => {
            println!("Archive: {}", cfg.archive_root.display());
            println!("Checkpoint directory: {}", cfg.archive_root.join(".syncarium").display());
            println!("Read-only status; no transfers attempted.");
        }
        Action::Plan => transfer_pass(&cfg, false)?,
        Action::Run { execute } => transfer_pass(&cfg, execute)?,
    }
    Ok(())
}

fn validate(cfg: &Config) -> Result<()> {
    ensure!(cfg.archive_root.is_absolute(), "archive_root must be absolute");
    clean_path(&cfg.archive_root)?;
    if let Some(mount) = &cfg.required_mount {
        ensure!(mount.is_absolute(), "required_mount must be absolute");
        clean_path(mount)?;
        ensure!(cfg.archive_root.starts_with(mount), "archive must live within required_mount");
    }
    ensure!((1..=4096).contains(&cfg.scheduler.budget_mib), "budget_mib must be 1..4096");
    ensure!((1..=64).contains(&cfg.scheduler.chunk_mib), "chunk_mib must be 1..64");
    ensure!((1..=10240).contains(&cfg.scheduler.bandwidth_kib), "bandwidth_kib must be 1..10240");
    ensure!((30..=3600).contains(&cfg.scheduler.run_seconds), "run_seconds must be 30..3600");
    ensure!(!cfg.sources.is_empty(), "add at least one source");
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for s in &cfg.sources {
        ensure!(!s.name.is_empty() && s.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "source names require ASCII letters/digits/hyphens/underscores");
        ensure!(names.insert(s.name.clone()), "duplicate source name {}", s.name);
        ensure!(s.destination.is_absolute(), "destination for {} must be absolute", s.name);
        clean_path(&s.destination)?;
        ensure!(s.destination.starts_with(&cfg.archive_root) && s.destination != cfg.archive_root,
            "{} destination must be a child of archive_root", s.name);
        ensure!(paths.insert(s.destination.clone()), "destinations must be distinct");
        match &s.backend {
            Backend::Rclone { remote } =>
                ensure!(!remote.is_empty() && remote.contains(':') && !remote.contains('\n'),
                    "{} needs an rclone remote:path", s.name),
            Backend::HttpManifest { manifest } =>
                ensure!(manifest.is_absolute(), "{} manifest path must be absolute", s.name),
        }
    }
    // Detect nested paths too: otherwise independent sources could collide.
    for a in &cfg.sources {
        for b in &cfg.sources {
            if a.name != b.name {
                ensure!(!a.destination.starts_with(&b.destination),
                    "nested source destinations are unsafe: {} and {}", a.name, b.name);
            }
        }
    }
    Ok(())
}

fn clean_path(path: &Path) -> Result<()> {
    ensure!(!path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "path must not contain .. or .: {}", path.display());
    Ok(())
}
fn safe_relative(path: &str) -> Result<PathBuf> {
    ensure!(!path.is_empty() && !path.contains('\\') && !path.chars().any(char::is_control),
        "unsafe relative path");
    let parsed = Path::new(path);
    ensure!(!path.split('/').any(|part| part.is_empty() || part == "." || part == ".."),
        "empty or traversing component");
    ensure!(!parsed.is_absolute() && parsed.components().all(|c| matches!(c, Component::Normal(_))),
        "unsafe path from source");
    Ok(parsed.to_path_buf())
}
fn no_symlink_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if let Ok(metadata) = fs::symlink_metadata(ancestor) {
            ensure!(!metadata.file_type().is_symlink(), "symlink forbidden: {}", ancestor.display());
        }
    }
    Ok(())
}
fn check_mount(cfg: &Config) -> Result<()> {
    ensure!(cfg.archive_root.is_dir(), "archive_root missing: {}", cfg.archive_root.display());
    no_symlink_ancestors(&cfg.archive_root)?;
    if let Some(mount) = &cfg.required_mount {
        let result = Command::new("findmnt")
            .args(["--mountpoint", &mount.to_string_lossy(), "-n", "-o", "TARGET,FSTYPE,OPTIONS"])
            .output().context("findmnt missing")?;
        ensure!(result.status.success(), "{} is not mounted", mount.display());
        let line = String::from_utf8(result.stdout)?;
        let values: Vec<_> = line.split_whitespace().collect();
        ensure!(values.len() >= 3 && values[0] == mount.to_string_lossy()
            && values[1] == "ext4" && values[2].split(',').any(|x| x == "rw"),
            "{} must be mounted as writable ext4", mount.display());
    }
    Ok(())
}
fn state_file(root: &Path, source: &str, item: &Item) -> (PathBuf, PathBuf) {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    h.update([0]);
    h.update(item.path.as_bytes());
    let key = hex::encode(h.finalize());
    let dir = root.join(".syncarium").join("partials").join(source).join(&key[..2]);
    (dir.join(format!("{key}.part")), dir.join(format!("{key}.json")))
}
fn atomic_json<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    let parent = path.parent().context("state without parent")?;
    fs::create_dir_all(parent)?;
    no_symlink_ancestors(parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut tmp, data)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| anyhow!("persisting checkpoint: {}", e.error))?;
    Ok(())
}
fn checkpoint(root: &Path, source: &str, item: &Item, create: bool)
    -> Result<(Checkpoint, PathBuf, PathBuf)> {
    let (part, meta) = state_file(root, source, item);
    no_symlink_ancestors(&part)?;
    no_symlink_ancestors(&meta)?;
    let part_exists = part.exists();
    let meta_exists = meta.exists();
    ensure!(part_exists == meta_exists,
        "orphan partial/manifest for {}: manual review required", item.path);
    if !part_exists {
        let cp = Checkpoint { source: source.into(), path: item.path.clone(),
            size: item.size, version: item.version.clone(), committed: 0,
            last_offset: 0, last_size: 0, last_sha256: String::new() };
        if create {
            fs::create_dir_all(part.parent().context("no parent")?)?;
            OpenOptions::new().write(true).create_new(true).open(&part)?;
            atomic_json(&meta, &cp)?;
        }
        return Ok((cp, part, meta));
    }
    let cp: Checkpoint = serde_json::from_slice(&fs::read(&meta)?)?;
    ensure!(cp.source == source && cp.path == item.path
        && cp.size == item.size && cp.version == item.version,
        "source changed during staged download: {}; preserved for review", item.path);
    ensure!(cp.committed <= item.size, "invalid stored byte offset");
    let actual = fs::metadata(&part)?.len();
    ensure!(actual >= cp.committed, "partial shrunk unexpectedly");
    if create && actual > cp.committed {
        let file = OpenOptions::new().write(true).open(&part)?;
        file.set_len(cp.committed)?;
        file.sync_all()?;
    }
    if cp.last_size > 0 {
        ensure!(cp.last_offset.checked_add(cp.last_size) == Some(cp.committed),
            "invalid last-chunk checkpoint");
        let mut file = File::open(&part)?;
        file.seek(SeekFrom::Start(cp.last_offset))?;
        let mut bytes = vec![0u8; cp.last_size as usize];
        file.read_exact(&mut bytes)?;
        ensure!(hex::encode(Sha256::digest(bytes)) == cp.last_sha256,
            "last downloaded chunk failed SHA-256 verification");
    }
    Ok((cp, part, meta))
}
fn read_items(source: &Source) -> Result<Vec<Item>> {
    let mut items = match &source.backend {
        Backend::Rclone { remote } => {
            let mut command = Command::new("rclone");
            command.args(["lsjson", remote, "--recursive", "--files-only",
                "--checkers", "1", "--tpslimit", "2"]);
            let output = command_output_with_timeout(&mut command, Duration::from_secs(180))?;
            let values: Vec<serde_json::Value> = serde_json::from_slice(&output)?;
            let mut items = Vec::new();
            for value in values {
                let (Some(path), Some(size)) = (
                    value.get("Path").and_then(|x| x.as_str()),
                    value.get("Size").and_then(|x| x.as_u64())) else { continue };
                // Identity is metadata, not a claim of end-to-end cryptographic verification.
                let version = serde_json::to_string(&serde_json::json!({
                    "modtime":value.get("ModTime"), "id":value.get("ID"),
                    "hashes":value.get("Hashes")
                }))?;
                items.push(Item { path: path.into(), size, version, url: None });
            }
            items
        }
        Backend::HttpManifest { manifest } => {
            no_symlink_ancestors(manifest)?;
            let entries: Vec<HttpEntry> = serde_json::from_slice(&fs::read(manifest)?)?;
            entries.into_iter().map(|e| Item {
                path:e.path, size:e.size, version:e.version, url: Some(e.url)
            }).collect()
        }
    };
    let mut seen = HashSet::new();
    for item in &items {
        safe_relative(&item.path)?;
        ensure!(seen.insert(item.path.clone()),
            "{} contains duplicate filename {}; requires identity-aware recovery",
            source.name, item.path);
        if let Some(url) = &item.url {
            ensure!(url.starts_with("https://"), "HTTP manifest requires HTTPS URLs");
        }
    }
    items.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(items)
}
fn command_output_with_timeout(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    let stdout = NamedTempFile::new()?;
    command.stdout(Stdio::from(stdout.reopen()?)).stderr(Stdio::null());
    let mut child = command.spawn().context("starting external provider backend")?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "provider command failed (status {status})");
            return Ok(fs::read(stdout.path())?);
        }
        if start.elapsed() >= timeout {
            child.kill().ok();
            child.wait().ok();
            bail!("provider command exceeded {}s timeout", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(100));
    }
}
fn download_range(source: &Source, item: &Item, offset: u64, count: u64,
    rate_kib: u64, deadline: Duration, staging: &Path) -> Result<()> {
    match &source.backend {
        Backend::Rclone { remote } => {
            let path = format!("{}/{}", remote.trim_end_matches('/'), item.path);
            let mut cmd = Command::new("rclone");
            cmd.args(["cat", &path, "--offset", &offset.to_string(),
                "--count", &count.to_string(), "--bwlimit", &format!("{rate_kib}K"),
                "--retries", "1", "--low-level-retries", "1"]);
            cmd.stdout(Stdio::from(OpenOptions::new().write(true).truncate(true).open(staging)?))
                .stderr(Stdio::null());
            let mut child = cmd.spawn().context("starting rclone cat")?;
            let start = Instant::now();
            loop {
                if let Some(status) = child.try_wait()? {
                    ensure!(status.success(), "rclone ranged download failed");
                    break;
                }
                if start.elapsed() >= deadline {
                    child.kill().ok();
                    child.wait().ok();
                    bail!("rclone transfer timed out");
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        Backend::HttpManifest { .. } => {
            let url = item.url.as_deref().context("missing URL in HTTP manifest")?;
            let end = offset.checked_add(count).and_then(|n| n.checked_sub(1))
                .context("invalid byte range")?;
            let client = Client::builder().timeout(deadline).build()?;
            let mut response = client.get(url)
                .header(RANGE, format!("bytes={offset}-{end}")).send()?;
            ensure!(response.status().as_u16() == 206,
                "server did not honor HTTP Range (206 required)");
            let claimed = response.headers().get(CONTENT_RANGE)
                .context("missing Content-Range")?.to_str()?;
            ensure!(claimed.starts_with(&format!("bytes {offset}-{end}/")),
                "server returned incorrect Content-Range");
            let mut file = OpenOptions::new().write(true).truncate(true).open(staging)?;
            let mut received = 0u64;
            let start = Instant::now();
            let max_rate = rate_kib.saturating_mul(1024);
            let mut buffer = [0u8; 16 * 1024];
            while received < count {
                let take = (count - received).min(buffer.len() as u64) as usize;
                let n = response.read(&mut buffer[..take])?;
                ensure!(n != 0, "HTTP response ended before requested range completed");
                file.write_all(&buffer[..n])?;
                received += n as u64;
                let should_elapsed = Duration::from_secs_f64(received as f64 / max_rate as f64);
                if should_elapsed > start.elapsed() {
                    thread::sleep(should_elapsed - start.elapsed());
                }
            }
            file.sync_all()?;
        }
    }
    ensure!(fs::metadata(staging)?.len() == count,
        "downloaded chunk length does not match requested range");
    Ok(())
}
fn store_chunk(part: &Path, meta: &Path, cp: &mut Checkpoint, chunk: &Path) -> Result<()> {
    let content = fs::read(chunk)?;
    ensure!(fs::metadata(part)?.len() == cp.committed, "partial size mismatch");
    let mut writer = OpenOptions::new().append(true).open(part)?;
    writer.write_all(&content)?;
    writer.sync_all()?;
    let start = cp.committed;
    cp.committed += content.len() as u64;
    cp.last_offset = start;
    cp.last_size = content.len() as u64;
    cp.last_sha256 = hex::encode(Sha256::digest(&content));
    atomic_json(meta, cp)
}
fn publish(part: &Path, meta: &Path, destination: &Path, relative: &str, expected: u64) -> Result<()> {
    let target = destination.join(safe_relative(relative)?);
    no_symlink_ancestors(&target)?;
    ensure!(fs::metadata(part)?.len() == expected, "cannot publish incomplete file");
    fs::create_dir_all(target.parent().context("missing parent")?)?;
    no_symlink_ancestors(target.parent().context("missing parent")?)?;
    match fs::hard_link(part, &target) {
        Ok(()) => {
            fs::remove_file(part)?;
            fs::remove_file(meta)?;
            println!("COMPLETE {}", relative);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            println!("PRESERVE existing local file {}", relative);
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn transfer_pass(cfg: &Config, execute: bool) -> Result<()> {
    check_mount(cfg)?;
    let state_root = cfg.archive_root.join(".syncarium");
    let _lock = if execute {
        fs::create_dir_all(&state_root)?;
        no_symlink_ancestors(&state_root)?;
        let handle = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(state_root.join("run.lock"))?;
        handle.try_lock_exclusive().context("another Syncarium run is active")?;
        Some(handle)
    } else { None };
    let start = Instant::now();
    let mut queues = Vec::new();
    for source in cfg.sources.iter().filter(|s| s.enabled) {
        ensure!(source.destination.is_dir(), "{} destination missing: {}",
            source.name, source.destination.display());
        no_symlink_ancestors(&source.destination)?;
        let items = read_items(source).with_context(|| format!("listing {}", source.name))?;
        let mut files = Vec::new();
        for item in items {
            let target = source.destination.join(safe_relative(&item.path)?);
            no_symlink_ancestors(&target)?;
            if target.exists() { continue; }
            let (cp, _, _) = checkpoint(&cfg.archive_root, &source.name, &item, false)?;
            files.push((cp.committed == 0, item));
        }
        files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
        println!("{}: {} pending item(s)", source.name, files.len());
        queues.push(Pending { source: source.clone(),
            files: files.into_iter().map(|(_, x)| x).collect(), index: 0 });
    }
    if !execute {
        println!("PREVIEW ONLY: no files downloaded. Use run --execute when ready.");
        return Ok(());
    }
    let rotation_path = state_root.join("rotation.json");
    let mut rotation: Rotation = if rotation_path.exists() {
        serde_json::from_slice(&fs::read(&rotation_path)?)?
    } else { Rotation::default() };
    let mut budget = cfg.scheduler.budget_mib * 1024 * 1024;
    let chunk_limit = cfg.scheduler.chunk_mib * 1024 * 1024;
    let time_limit = Duration::from_secs(cfg.scheduler.run_seconds);
    let count = queues.len();
    if count == 0 {
        println!("No sources enabled. Nothing downloaded.");
        return Ok(());
    }
    let first = rotation.next % count;
    // Advance start source even when a later transfer fails.
    rotation.next = (first + 1) % count;
    atomic_json(&rotation_path, &rotation)?;
    while budget > 0 && start.elapsed() < time_limit {
        let mut any = false;
        for i in 0..count {
            if budget == 0 { break; }
            let pending = &mut queues[(first + i) % count];
            if pending.index >= pending.files.len() { continue; }
            let item = &pending.files[pending.index];
            let target = pending.source.destination.join(safe_relative(&item.path)?);
            if target.exists() {
                pending.index += 1;
                any = true;
                continue;
            }
            let remaining = time_limit.saturating_sub(start.elapsed());
            if remaining < Duration::from_secs(8) { break; }
            let (mut cp, part, meta) = checkpoint(&cfg.archive_root,
                &pending.source.name, item, true)?;
            if cp.committed == item.size {
                publish(&part, &meta, &pending.source.destination, &item.path, item.size)?;
                pending.index += 1;
                any = true;
                continue;
            }
            let seconds = remaining.as_secs().saturating_sub(4);
            let time_budget = cfg.scheduler.bandwidth_kib * 1024 * seconds;
            let n = budget.min(chunk_limit).min(item.size - cp.committed).min(time_budget);
            if n == 0 { break; }
            // Account for attempted bytes before invoking any external or network client.
            budget -= n;
            let folder = part.parent().context("missing staged directory")?;
            let chunk = NamedTempFile::new_in(folder)?;
            println!("CHUNK {} {} @{} +{}", pending.source.name, item.path, cp.committed, n);
            download_range(&pending.source, item, cp.committed, n, cfg.scheduler.bandwidth_kib,
                remaining.saturating_sub(Duration::from_secs(2)), chunk.path())?;
            store_chunk(&part, &meta, &mut cp, chunk.path())?;
            if cp.committed == item.size {
                publish(&part, &meta, &pending.source.destination, &item.path, item.size)?;
                pending.index += 1;
            }
            any = true;
        }
        if !any { break; }
    }
    println!("PASS DONE: {} bytes of payload budget unused.", budget);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsafe_remote_paths() {
        for s in ["../a", "a/../b", "/etc/passwd", "a\\b", "a//b", "."] {
            assert!(safe_relative(s).is_err(), "should reject {s}");
        }
        assert!(safe_relative("Photos/2026/image.HEIC").is_ok());
    }
    #[test]
    fn new_checkpoints_do_not_limit_file_size() {
        let tmp = tempfile::tempdir().unwrap();
        let item = Item { path: "large.mp4".into(), size: 10_000_000_000,
            version: "v1".into(), url: None };
        let (cp, part, _) = checkpoint(tmp.path(), "photos", &item, false).unwrap();
        assert_eq!(cp.committed, 0);
        assert!(!part.exists());
    }
    #[test]
    fn atomic_state_detects_source_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let item = Item { path: "x".into(), size: 100, version: "v1".into(), url: None };
        checkpoint(tmp.path(), "photos", &item, true).unwrap();
        let other = Item { version: "v2".into(), ..item };
        assert!(checkpoint(tmp.path(), "photos", &other, false).is_err());
    }
}
