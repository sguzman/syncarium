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
    /// Audit source-to-local coverage (metadata listing; no file payload downloads).
    Audit {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Inspect durable staged progress locally (no cloud calls).
    Status {
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
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
    sha256: Option<String>,
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
    #[serde(default)]
    expected_sha256: Option<String>,
}
#[derive(Deserialize)]
struct HttpEntry {
    path: String,
    size: u64,
    url: String,
    version: String,
    #[serde(default)]
    sha256: Option<String>,
}
#[derive(Serialize, Deserialize, Default)]
struct Rotation {
    next: usize,
}

#[derive(Serialize)]
struct SourceProgress {
    name: String,
    enabled: bool,
    partial_files: u64,
    committed_bytes: u64,
    outstanding_bytes_in_partials: u64,
}

#[derive(Serialize)]
struct StatusReport {
    archive_root: String,
    partial_files: u64,
    committed_bytes: u64,
    sources: Vec<SourceProgress>,
}

#[derive(Serialize, Default)]
struct AuditSource {
    name: String,
    // Source listing is only as complete as the provider API.
    listed_files: u64,
    listed_bytes: u64,
    present_size_matched: u64,
    present_size_mismatch: u64,
    missing_files: u64,
    missing_bytes: u64,
    staged_files: u64,
    staged_saved_bytes: u64,
    staged_remaining_bytes: u64,
    held_files: u64,
    error: Option<String>,
}

#[derive(Serialize)]
struct AuditReport {
    archive_root: String,
    sources: Vec<AuditSource>,
    warning: &'static str,
}
struct Pending {
    source: Source,
    files: Vec<Item>,
    index: usize,
    failed: bool,
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
        Action::Status { json } => status(&cfg, json)?,
        Action::Audit { json } => audit(&cfg, json)?,
        Action::Plan => transfer_pass(&cfg, false)?,
        Action::Run { execute } => transfer_pass(&cfg, execute)?,
    }
    Ok(())
}

/// Audit current remote listings against local files without downloading payloads.
/// Never call this a full integrity check: provider inventories can be incomplete.
fn audit(cfg: &Config, json: bool) -> Result<()> {
    check_mount(cfg)?;
    let mut report = AuditReport {
        archive_root: cfg.archive_root.display().to_string(),
        sources: Vec::new(),
        warning: "Counts reflect only items returned by the provider API; present files are compared by size, not content hash. This is not proof of a complete archive.",
    };
    let mut errors = 0u64;
    for source in cfg.sources.iter().filter(|src| src.enabled) {
        ensure!(
            source.destination.is_dir(),
            "{} archive destination missing: {}",
            source.name,
            source.destination.display()
        );
        no_symlink_ancestors(&source.destination)?;
        let mut row = AuditSource {
            name: source.name.clone(),
            ..AuditSource::default()
        };
        match read_items(source) {
            Err(error) => {
                row.error = Some(format!("{error:#}"));
                errors += 1;
            }
            Ok(items) => {
                for item in items {
                    row.listed_files += 1;
                    row.listed_bytes = row.listed_bytes.saturating_add(item.size);
                    let target = source.destination.join(safe_relative(&item.path)?);
                    no_symlink_ancestors(&target)?;
                    match fs::symlink_metadata(&target) {
                        Ok(metadata) => {
                            if metadata.is_file() && metadata.len() == item.size {
                                row.present_size_matched += 1;
                            } else {
                                // The archive is append-only; never repair or replace
                                // a conflicting local path without explicit intervention.
                                row.present_size_mismatch += 1;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            match checkpoint(&cfg.archive_root, &source.name, &item, false) {
                                Ok((cp, part, _)) if part.exists() => {
                                    row.staged_files += 1;
                                    row.staged_saved_bytes = row
                                        .staged_saved_bytes.saturating_add(cp.committed);
                                    row.staged_remaining_bytes = row
                                        .staged_remaining_bytes.saturating_add(item.size - cp.committed);
                                }
                                Ok(_) => {
                                    row.missing_files += 1;
                                    row.missing_bytes = row.missing_bytes.saturating_add(item.size);
                                }
                                Err(_) => {
                                    row.held_files += 1;
                                }
                            }
                        }
                        Err(error) => return Err(error).context("inspecting local archive"),
                    }
                }
                if row.present_size_mismatch > 0 || row.held_files > 0 {
                    errors += 1;
                }
            }
        }
        report.sources.push(row);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Syncarium archive coverage audit");
        for row in &report.sources {
            if let Some(error) = &row.error {
                println!("{}: listing FAILED: {}", row.name, error);
            } else {
                println!(
                    "{}: {} listed, {} size-matched local, {} staged, {} missing, {} mismatched, {} held",
                    row.name, row.listed_files, row.present_size_matched,
                    row.staged_files, row.missing_files,
                    row.present_size_mismatch, row.held_files
                );
                println!(
                    "  Pending: {} MiB wholly missing, {} MiB remaining in partials",
                    row.missing_bytes / 1_048_576,
                    row.staged_remaining_bytes / 1_048_576
                );
            }
        }
        println!("WARNING: {}", report.warning);
    }
    if errors > 0 {
        bail!("{errors} source(s) have audit errors/conflicts; see report above");
    }
    Ok(())
}

/// Read only local staging metadata. Never query a provider or modify a partial.
fn status(cfg: &Config, json: bool) -> Result<()> {
    check_mount(cfg)?;
    let state = cfg.archive_root.join(".syncarium").join("partials");
    no_symlink_ancestors(&state)?;
    let mut report = StatusReport {
        archive_root: cfg.archive_root.display().to_string(),
        partial_files: 0,
        committed_bytes: 0,
        sources: Vec::new(),
    };

    for source in &cfg.sources {
        let mut summary = SourceProgress {
            name: source.name.clone(),
            enabled: source.enabled,
            partial_files: 0,
            committed_bytes: 0,
            outstanding_bytes_in_partials: 0,
        };
        let root = state.join(&source.name);
        if root.exists() {
            no_symlink_ancestors(&root)?;
            for bucket in fs::read_dir(&root)? {
                let bucket = bucket?;
                let kind = bucket.file_type()?;
                ensure!(
                    kind.is_dir() && !kind.is_symlink(),
                    "unsafe checkpoint directory: {}",
                    bucket.path().display()
                );
                for entry in fs::read_dir(bucket.path())? {
                    let entry = entry?;
                    if entry.path().extension().is_none_or(|ext| ext != "json") {
                        continue;
                    }
                    ensure!(
                        entry.file_type()?.is_file(),
                        "unsafe checkpoint entry: {}",
                        entry.path().display()
                    );
                    let cp: Checkpoint = serde_json::from_slice(&fs::read(entry.path())?)
                        .with_context(|| format!("reading {}", entry.path().display()))?;
                    ensure!(
                        cp.source == source.name && cp.committed <= cp.size,
                        "invalid checkpoint: {}",
                        entry.path().display()
                    );
                    // Every manifest must have a matching .part file.
                    let part = entry.path().with_extension("part");
                    ensure!(
                        part.is_file() && !part.is_symlink(),
                        "checkpoint missing its partial: {}",
                        part.display()
                    );
                    ensure!(
                        fs::metadata(&part)?.len() >= cp.committed,
                        "checkpoint's partial is shorter than committed bytes: {}",
                        part.display()
                    );
                    summary.partial_files = summary.partial_files.checked_add(1)
                        .context("partial count overflow")?;
                    summary.committed_bytes = summary.committed_bytes
                        .checked_add(cp.committed).context("byte count overflow")?;
                    summary.outstanding_bytes_in_partials = summary
                        .outstanding_bytes_in_partials
                        .checked_add(cp.size - cp.committed)
                        .context("remaining byte count overflow")?;
                }
            }
        }
        report.partial_files = report.partial_files
            .checked_add(summary.partial_files).context("total count overflow")?;
        report.committed_bytes = report.committed_bytes
            .checked_add(summary.committed_bytes).context("total bytes overflow")?;
        report.sources.push(summary);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Archive: {}", report.archive_root);
        println!("Staged partial files: {}", report.partial_files);
        println!("Staged committed bytes: {}", report.committed_bytes);
        for source in &report.sources {
            println!(
                "{}: {} partial(s), {} bytes saved, {} bytes remaining in partials{}",
                source.name,
                source.partial_files,
                source.committed_bytes,
                source.outstanding_bytes_in_partials,
                if source.enabled { "" } else { " [disabled]" }
            );
        }
        println!("This is staged progress only, not a completed-archive inventory.");
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
            last_offset: 0, last_size: 0, last_sha256: String::new(),
            expected_sha256: item.sha256.clone() };
        if create {
            fs::create_dir_all(part.parent().context("no parent")?)?;
            OpenOptions::new().write(true).create_new(true).open(&part)?;
            atomic_json(&meta, &cp)?;
        }
        return Ok((cp, part, meta));
    }
    let cp: Checkpoint = serde_json::from_slice(&fs::read(&meta)?)?;
    ensure!(cp.source == source && cp.path == item.path
        && cp.size == item.size && cp.version == item.version
        && cp.expected_sha256 == item.sha256,
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
                items.push(Item { path: path.into(), size, version, url: None, sha256: None });
            }
            items
        }
        Backend::HttpManifest { manifest } => {
            no_symlink_ancestors(manifest)?;
            let entries: Vec<HttpEntry> = serde_json::from_slice(&fs::read(manifest)?)?;
            entries.into_iter().map(|e| Item {
                path:e.path, size:e.size, version:e.version, url: Some(e.url),
                sha256:e.sha256.map(|hash| hash.to_ascii_lowercase())
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
        if let Some(hash) = &item.sha256 {
            ensure!(hash.len() == 64 && hash.bytes().all(|ch| ch.is_ascii_hexdigit()),
                "invalid SHA-256 fingerprint for {}", item.path);
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
fn publish(part: &Path, meta: &Path, destination: &Path, item: &Item) -> Result<()> {
    let target = destination.join(safe_relative(&item.path)?);
    no_symlink_ancestors(&target)?;
    ensure!(
        fs::metadata(part)?.len() == item.size,
        "cannot publish incomplete file"
    );
    if let Some(expected) = &item.sha256 {
        let mut reader = File::open(part)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 256 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        ensure!(
            hex::encode(hasher.finalize()) == *expected,
            "whole-file SHA-256 mismatch for {}; preserving partial for review",
            item.path
        );
    }
    fs::create_dir_all(target.parent().context("missing parent")?)?;
    no_symlink_ancestors(target.parent().context("missing parent")?)?;
    match fs::hard_link(part, &target) {
        Ok(()) => {
            fs::remove_file(part)?;
            fs::remove_file(meta)?;
            println!("COMPLETE {}", item.path);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            println!("PRESERVE existing local file {}", item.path);
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
/// Advance at most one chunk of a source. A failure stays confined to this
/// source for this pass, while other healthy providers get their turn.
fn advance_source(
    cfg: &Config,
    pending: &mut Pending,
    budget: &mut u64,
    chunk_limit: u64,
    start: Instant,
    time_limit: Duration,
) -> Result<bool> {
    if pending.index >= pending.files.len() || pending.failed {
        return Ok(false);
    }
    let item = &pending.files[pending.index];
    let relative = safe_relative(&item.path)?;
    let target = pending.source.destination.join(relative);
    no_symlink_ancestors(&target)?;
    if target.exists() {
        pending.index += 1;
        return Ok(true);
    }
    let remaining = time_limit.saturating_sub(start.elapsed());
    if remaining < Duration::from_secs(8) {
        return Ok(false);
    }
    let (mut cp, part, meta) =
        checkpoint(&cfg.archive_root, &pending.source.name, item, true)?;
    if cp.committed == item.size {
        publish(&part, &meta, &pending.source.destination, item)?;
        pending.index += 1;
        return Ok(true);
    }
    let seconds = remaining.as_secs().saturating_sub(4);
    let time_budget = cfg.scheduler.bandwidth_kib * 1024 * seconds;
    let n = (*budget)
        .min(chunk_limit)
        .min(item.size - cp.committed)
        .min(time_budget);
    if n == 0 {
        return Ok(false);
    }
    // Account for attempted bytes before starting any network operation.
    *budget -= n;
    let folder = part.parent().context("missing staged directory")?;
    let chunk = NamedTempFile::new_in(folder)?;
    println!(
        "CHUNK {} {} @{} +{}",
        pending.source.name, item.path, cp.committed, n
    );
    download_range(
        &pending.source,
        item,
        cp.committed,
        n,
        cfg.scheduler.bandwidth_kib,
        remaining.saturating_sub(Duration::from_secs(2)),
        chunk.path(),
    )?;
    store_chunk(&part, &meta, &mut cp, chunk.path())?;
    if cp.committed == item.size {
        publish(&part, &meta, &pending.source.destination, item)?;
        pending.index += 1;
    }
    Ok(true)
}

fn transfer_pass(cfg: &Config, execute: bool) -> Result<()> {
    check_mount(cfg)?;
    let state_root = cfg.archive_root.join(".syncarium");
    let _lock = if execute {
        fs::create_dir_all(&state_root)?;
        no_symlink_ancestors(&state_root)?;
        let handle = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_root.join("run.lock"))?;
        handle
            .try_lock_exclusive()
            .context("another Syncarium run is active")?;
        Some(handle)
    } else {
        None
    };

    let started = Instant::now();
    let mut issues: Vec<String> = Vec::new();
    let mut queues = Vec::new();

    for source in cfg.sources.iter().filter(|s| s.enabled) {
        // Mount and local destination failures are global safety failures,
        // unlike a provider listing or an individual partial.
        ensure!(
            source.destination.is_dir(),
            "{} destination missing: {}",
            source.name,
            source.destination.display()
        );
        no_symlink_ancestors(&source.destination)?;

        let items = match read_items(source) {
            Ok(items) => items,
            Err(error) => {
                let message = format!("{}: listing failed: {error:#}", source.name);
                eprintln!("SOURCE FAILED: {message}");
                issues.push(message);
                continue;
            }
        };
        let mut files = Vec::new();
        for item in items {
            let target = source.destination.join(safe_relative(&item.path)?);
            no_symlink_ancestors(&target)?;
            if target.exists() {
                continue;
            }
            match checkpoint(&cfg.archive_root, &source.name, &item, false) {
                Ok((cp, _, _)) => files.push((cp.committed == 0, item)),
                Err(error) => {
                    let message = format!(
                        "{}: checkpoint held for {}: {error:#}",
                        source.name, item.path
                    );
                    eprintln!("ITEM HELD: {message}");
                    issues.push(message);
                }
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
        println!("{}: {} pending item(s)", source.name, files.len());
        queues.push(Pending {
            source: source.clone(),
            files: files.into_iter().map(|(_, item)| item).collect(),
            index: 0,
            failed: false,
        });
    }

    if !execute {
        println!("PREVIEW ONLY: no files downloaded. Use run --execute when ready.");
        if !issues.is_empty() {
            bail!("{} source/item issue(s); other sources were still inspected", issues.len());
        }
        return Ok(());
    }

    let rotation_path = state_root.join("rotation.json");
    let mut rotation: Rotation = if rotation_path.exists() {
        serde_json::from_slice(&fs::read(&rotation_path)?)?
    } else {
        Rotation::default()
    };
    let mut budget = cfg.scheduler.budget_mib * 1024 * 1024;
    let chunk_limit = cfg.scheduler.chunk_mib * 1024 * 1024;
    let time_limit = Duration::from_secs(cfg.scheduler.run_seconds);
    let count = queues.len();
    if count == 0 {
        if !issues.is_empty() {
            bail!("All source listings failed ({} issue(s))", issues.len());
        }
        println!("No sources enabled. Nothing downloaded.");
        return Ok(());
    }

    let first = rotation.next % count;
    rotation.next = (first + 1) % count;
    atomic_json(&rotation_path, &rotation)?;

    while budget > 0 && started.elapsed() < time_limit {
        let mut any = false;
        for i in 0..count {
            if budget == 0 || time_limit.saturating_sub(started.elapsed()) < Duration::from_secs(8) {
                break;
            }
            let pending = &mut queues[(first + i) % count];
            match advance_source(cfg, pending, &mut budget, chunk_limit, started, time_limit) {
                Ok(progress) => any |= progress,
                Err(error) => {
                    let item = pending
                        .files
                        .get(pending.index)
                        .map_or("<unknown>", |item| item.path.as_str());
                    let message = format!(
                        "{}: download held for {}: {error:#}",
                        pending.source.name, item
                    );
                    eprintln!("SOURCE FAILED: {message}");
                    pending.failed = true; // Do not retry the same failure this pass.
                    issues.push(message);
                    any = true; // Continue with other sources.
                }
            }
        }
        if !any {
            break;
        }
    }
    println!("PASS DONE: {budget} bytes of payload budget unused.");
    if !issues.is_empty() {
        bail!(
            "{} source/item issue(s); completed work from healthy sources has been preserved",
            issues.len()
        );
    }
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
            version: "v1".into(), url: None, sha256: None };
        let (cp, part, _) = checkpoint(tmp.path(), "photos", &item, false).unwrap();
        assert_eq!(cp.committed, 0);
        assert!(!part.exists());
    }
    #[test]
    fn atomic_state_detects_source_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let item = Item { path: "x".into(), size: 100, version: "v1".into(), url: None, sha256: None };
        checkpoint(tmp.path(), "photos", &item, true).unwrap();
        let other = Item { version: "v2".into(), ..item };
        assert!(checkpoint(tmp.path(), "photos", &other, false).is_err());
    }
    #[test]
    fn valid_sha256_is_checked_before_immutable_publication() {
        let tmp = tempfile::tempdir().unwrap();
        let destination = tmp.path().join("archive");
        std::fs::create_dir(&destination).unwrap();
        let part = tmp.path().join("file.part");
        let meta = tmp.path().join("file.json");
        std::fs::write(&part, b"correct-data").unwrap();
        std::fs::write(&meta, b"{}").unwrap();
        let mut item = Item {
            path: "file.bin".into(),
            size: 12,
            version: "v1".into(),
            url: None,
            sha256: Some(hex::encode(Sha256::digest(b"different-data"))),
        };
        assert!(publish(&part, &meta, &destination, &item).is_err());
        assert!(!destination.join("file.bin").exists());
        assert!(part.exists(), "bad checksum must preserve the partial");
        item.sha256 = Some(hex::encode(Sha256::digest(b"correct-data")));
        publish(&part, &meta, &destination, &item).unwrap();
        assert_eq!(std::fs::read(destination.join("file.bin")).unwrap(), b"correct-data");
        assert!(!part.exists());
    }

}
