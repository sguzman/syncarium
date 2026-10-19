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

mod inventory;
mod drive_collisions;
mod drive_recovery;
mod drive_id;
mod drive_workspace;
mod provider_probe;
mod receipts;

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
    /// Validate provider byte-offset and count behavior without archiving data.
    /// Downloads 192 KiB only with explicit --execute.
    ProbeRange {
        /// Exact configured rclone source name to diagnose.
        #[arg(long)]
        source: String,
        /// Explicit opt-in to contact the source and read sample bytes.
        #[arg(long)]
        execute: bool,
    },
    /// Inventory Google Workspace exports and identify unsupported native types.
    /// This is metadata-only; it never downloads export content.
    DriveWorkspacePlan {
        /// Configured Google Drive source name.
        #[arg(long, default_value = "google-drive")]
        source: String,
        /// Read a private local provider inventory JSON fixture instead.
        #[arg(long)]
        fixture: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Plan immutable-ID recovery locations without downloading payload.
    DriveRecoveryPlan {
        /// Configured Google Drive source name.
        #[arg(long, default_value = "google-drive")]
        source: String,
        /// Optional local JSON fixture (for offline tests).
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// Show safe ID-derived destination paths in JSON.
        #[arg(long)]
        json: bool,
    },
    /// Read-only Google Drive immutable-ID duplicate name inspection.
    DriveCollisions {
        #[arg(long, default_value = "gdrive:")]
        remote: String,
        #[arg(long)]
        json: bool,
    },
    /// Audit source-to-local coverage (metadata listing; no file payload downloads).
    Audit {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Rehash files published with local Syncarium SHA-256 receipts (no cloud access).
    Verify {
        #[arg(long)]
        source: Option<String>,
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
    /// Optional sanity floor on current *eligible* provider-listed file count.
    #[serde(default)]
    min_items: Option<usize>,
    /// Optional lower bound on summed known byte sizes from a provider listing.
    #[serde(default)]
    min_total_bytes: Option<u64>,
    /// Hold this source when any previously observed remote identity vanishes.
    #[serde(default)]
    require_historical_coverage: bool,
    #[serde(flatten)]
    backend: Backend,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Backend {
    Rclone { remote: String },
    HttpManifest { manifest: PathBuf },
    /// Immutable Google IDs, native Rust ranged REST reads; disabled by default.
    GoogleDriveIds { remote: String, token_file: PathBuf },
    /// Explicitly opt-in single-response exports of Google-native documents.
    GoogleWorkspace { remote: String, token_file: PathBuf },
}
#[derive(Clone, Debug)]
struct Item {
    path: String,
    size: u64,
    version: String,
    url: Option<String>,
    sha256: Option<String>,
    object_id: Option<String>,
    md5: Option<String>,
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
    #[serde(default)]
    expected_md5: Option<String>,
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
    workspace_export_candidates: u64,
    unresolved_provider_items: u64,
    historical_absent: u64,
    historical_ever_seen: u64,
    newly_observed: u64,
    error: Option<String>,
}

#[derive(Serialize)]
struct AuditReport {
    archive_root: String,
    sources: Vec<AuditSource>,
    warning: &'static str,
}
struct Listing {
    files: Vec<Item>,
    exports: Vec<drive_workspace::ExportCandidate>,
    /// Provider-visible objects not eligible for bounded, verified transfer.
    unresolved: usize,
}

struct Pending {
    source: Source,
    files: Vec<Item>,
    index: usize,
    exports: Vec<drive_workspace::ExportCandidate>,
    export_index: usize,
    consecutive_failures: u8,
    circuit_open: bool,
}

const MAX_CONSECUTIVE_ITEM_FAILURES: u8 = 3;

fn hold_failed_item(queue: &mut Pending) {
    if queue.index < queue.files.len() {
        queue.index = queue.index.saturating_add(1);
    } else {
        queue.export_index = queue.export_index.saturating_add(1);
    }
    queue.consecutive_failures = queue.consecutive_failures.saturating_add(1);
    if queue.consecutive_failures >= MAX_CONSECUTIVE_ITEM_FAILURES {
        queue.circuit_open = true;
    }
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
        Action::Verify { source, json } => receipts::verify(&cfg, source.as_deref(), json)?,
        Action::Audit { json } => audit(&cfg, json)?,
        Action::ProbeRange { source, execute } =>
            provider_probe::probe(&cfg, &source, execute)?,
        Action::DriveCollisions { remote, json } => {
            check_mount(&cfg)?;
            drive_collisions(&remote, json)?;
        },
        Action::DriveRecoveryPlan { source, fixture, json } => {
            drive_recovery_plan(&cfg, &source, fixture.as_deref(), json)?;
        },
        Action::DriveWorkspacePlan { source, fixture, json } => {
            drive_workspace_plan(&cfg, &source, fixture.as_deref(), json)?;
        },
        Action::Plan => transfer_pass(&cfg, false)?,
        Action::Run { execute } => transfer_pass(&cfg, execute)?,
    }
    Ok(())
}

/// Metadata-only Workspace export readiness inspection. Neither rclone copyid
/// nor Google's export API is called from this command.
fn drive_workspace_plan(
    cfg: &Config,
    source_name: &str,
    fixture: Option<&Path>,
    json: bool,
) -> Result<()> {
    check_mount(cfg)?;
    let source = cfg.sources.iter()
        .find(|src| src.name == source_name)
        .with_context(|| format!("Google Drive source '{source_name}' not configured"))?;
    let remote = match &source.backend {
        Backend::Rclone { remote }
        | Backend::GoogleDriveIds { remote, .. }
        | Backend::GoogleWorkspace { remote, .. } => remote.as_str(),
        Backend::HttpManifest { .. } => bail!("Workspace planning requires Google Drive source"),
    };
    ensure!(source.destination.is_dir(), "archive destination must exist");
    no_symlink_ancestors(&source.destination)?;
    let raw = if let Some(path) = fixture {
        ensure!(path.is_absolute(), "fixture path must be absolute");
        no_symlink_ancestors(path)?;
        fs::read(path).context("reading local private Workspace fixture")?
    } else {
        ensure!(remote.contains(':') && !remote.starts_with('-')
            && !remote.chars().any(char::is_control), "invalid Google remote");
        let mut command = Command::new("rclone");
        command.args(["backend", "query", remote, "trashed = false"]);
        command_output_with_timeout(&mut command, Duration::from_secs(180))
            .context("read-only Google Workspace inventory query failed")?
    };
    ensure!(raw.len() <= 256 * 1024 * 1024, "Workspace inventory exceeds 256 MiB cap");
    let plan = drive_workspace::plan(&raw)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!("Read-only Workspace export readiness for {source_name}");
        println!("{} supported export candidates (size unknown until export)",
            plan.supported.len());
        println!("{} Google-native objects need different handling",
            plan.unsupported_google_types);
        println!("{} non-Workspace/folder/shortcut objects ignored by this planner",
            plan.ignored_non_workspace);
        println!("No export downloads and no archive modifications.");
    }
    if plan.unsupported_google_types > 0 {
        bail!("Workspace export coverage incomplete: {} unsupported Google-native objects",
            plan.unsupported_google_types);
    }
    Ok(())
}

/// Pure planning: target paths depend solely on immutable Google object IDs.
/// Never uses rclone copyid or writes a file to the archive.
fn drive_recovery_plan(
    cfg: &Config,
    source_name: &str,
    fixture: Option<&Path>,
    json: bool,
) -> Result<()> {
    check_mount(cfg)?;
    let source = cfg.sources.iter()
        .find(|src| src.name == source_name)
        .with_context(|| format!("Google Drive source '{source_name}' not configured"))?;
    let remote = match &source.backend {
        Backend::Rclone { remote } => remote.as_str(),
        Backend::GoogleDriveIds { remote, .. }
        | Backend::GoogleWorkspace { remote, .. } => remote.as_str(),
        Backend::HttpManifest { .. } => bail!("Drive recovery requires an rclone source"),
    };
    ensure!(
        source.destination.is_dir(),
        "archive directory does not exist: {}",
        source.destination.display()
    );
    no_symlink_ancestors(&source.destination)?;
    let bytes = if let Some(path) = fixture {
        no_symlink_ancestors(path)?;
        fs::read(path).context("reading local-only recovery plan fixture")?
    } else {
        ensure!(
            remote.contains(':') && !remote.starts_with('-')
                && !remote.chars().any(char::is_control),
            "invalid rclone remote"
        );
        let mut command = Command::new("rclone");
        command.args(["backend", "query", remote, "trashed = false"]);
        command_output_with_timeout(&mut command, Duration::from_secs(180))
            .context("querying Drive IDs for a read-only recovery plan")?
    };
    ensure!(
        bytes.len() <= 256 * 1024 * 1024,
        "Drive object inventory exceeds the 256 MiB in-memory planner limit"
    );
    let plan = drive_recovery::plan_recovery_json(&bytes)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!("Read-only ID-based recovery plan for {}", source.name);
        println!("{} recoverable candidates", plan.items.len());
        println!("{} bytes of known-size objects", plan.known_size_bytes);
        println!("{} objects require size/format verification", plan.unknown_size);
        println!("{} unsupported Drive-native/shortcut objects", plan.unsupported);
        println!("Candidate destinations: {}",
            source.destination.join(".rclone-duplicates/objects").display());
        println!("No cloud files or local archive files were modified.");
    }
    Ok(())
}

/// Explicit identity inspection: contacts the Google metadata API, not file bytes.
fn drive_collisions(remote: &str, json: bool) -> Result<()> {
    ensure!(
        remote.contains(':') && !remote.starts_with('-') && !remote.chars().any(char::is_control),
        "remote must be a Google Drive rclone remote"
    );
    let mut command = Command::new("rclone");
    command.args(["backend", "query", remote, "trashed = false"]);
    let raw = command_output_with_timeout(&mut command, Duration::from_secs(180))
        .context("Google Drive object ID query failed")?;
    ensure!(raw.len() <= 256 * 1024 * 1024, "Google object listing too large to audit");
    let counts = drive_collisions::summarize_json(&raw)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&counts)?);
    } else {
        println!("Google Drive object identity audit: {remote}");
        println!("Objects: {}", counts.objects);
        println!("Duplicate file groups: {} ({} objects)", counts.file_groups, counts.file_objects);
        println!("Duplicate folder groups: {} ({} objects)", counts.folder_groups, counts.folder_objects);
        println!("Hidden descendant files under duplicate folders: {}", counts.hidden_descendant_files);
        println!("File versus folder output-name collisions: {}", counts.cross_type_groups);
        println!("Read-only analysis; nothing was downloaded or modified.");
    }
    if counts.has_collisions() {
        bail!("Google Drive contains ID-level name collisions requiring recovery");
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
        match read_items(source, Duration::from_secs(180)) {
            Err(error) => {
                row.error = Some(format!("{error:#}"));
                errors += 1;
            }
            Ok(listing) => {
                if listing.unresolved > 0 {
                    row.unresolved_provider_items = listing.unresolved as u64;
                    errors += 1;
                }
                row.workspace_export_candidates = listing.exports.len() as u64;
                let items = listing.files;
                let exports = listing.exports;
                match inventory::observe(
                    &cfg.archive_root,
                    &source.name,
                    items.iter().map(|item| item.path.as_str())
                        .chain(exports.iter().map(|item| item.object_fingerprint.as_str())),
                    false,
                ) {
                    Ok(observation) => {
                        row.historical_absent = observation.absent;
                        row.historical_ever_seen = observation.ever_seen;
                        row.newly_observed = observation.new;
                        if observation.absent > 0 {
                            errors += 1;
                        }
                    }
                    Err(error) => {
                        row.error = Some(format!("local inventory ledger: {error:#}"));
                        errors += 1;
                        report.sources.push(row);
                        continue;
                    }
                }
                for candidate in &exports {
                    let target = source.destination.join(safe_relative(&candidate.relative_destination)?);
                    no_symlink_ancestors(&target)?;
                    if target.exists() {
                        if receipts::check_export_target(
                            &cfg.archive_root, &source.name,
                            &candidate.relative_destination, candidate.version_key(),
                            &target, false
                        ).is_err() {
                            row.held_files += 1;
                        }
                    } else {
                        row.missing_files += 1;
                    }
                }
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
                    "{}: {} eligible listed, {} Workspace exports, {} size-matched local, {} staged, {} missing, {} mismatched, {} held, {} unresolved, {} historically observed but absent",
                    row.name, row.listed_files, row.workspace_export_candidates,
                    row.present_size_matched, row.staged_files, row.missing_files,
                    row.present_size_mismatch, row.held_files,
                    row.unresolved_provider_items, row.historical_absent
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
        if let Some(minimum) = s.min_items {
            ensure!((1..=10_000_000).contains(&minimum),
                "source {} min_items must be 1..10000000", s.name);
        }
        if let Some(minimum) = s.min_total_bytes {
            ensure!(minimum > 0, "source {} min_total_bytes must be positive", s.name);
            ensure!(!matches!(s.backend, Backend::GoogleWorkspace { .. }),
                "{} cannot use min_total_bytes: Workspace sizes are unknown before export", s.name);
        }
        ensure!(s.destination.is_absolute(), "destination for {} must be absolute", s.name);
        clean_path(&s.destination)?;
        ensure!(s.destination.starts_with(&cfg.archive_root) && s.destination != cfg.archive_root,
            "{} destination must be a child of archive_root", s.name);
        ensure!(paths.insert(s.destination.clone()), "destinations must be distinct");
        match &s.backend {
            Backend::Rclone { remote } =>
                ensure!(!remote.is_empty() && remote.contains(':') && !remote.contains('\n'),
                    "{} needs an rclone remote:path", s.name),
            Backend::GoogleDriveIds { remote, token_file }
            | Backend::GoogleWorkspace { remote, token_file } => {
                ensure!(!remote.is_empty() && remote.contains(':') && !remote.contains('\n'),
                    "{} needs a Google rclone remote", s.name);
                ensure!(token_file.is_absolute(), "{} needs absolute token_file", s.name);
                clean_path(token_file)?;
            }
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
/// Flush directory entries when publishing state or archive hard links.
fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)
        .with_context(|| format!("opening directory for sync: {}", directory.display()))?
        .sync_all()
        .with_context(|| format!("syncing directory: {}", directory.display()))
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
    // The JSON must survive a sudden power loss after the rename.
    sync_directory(parent)?;
    Ok(())
}
fn checkpoint(root: &Path, source: &str, item: &Item, create: bool)
    -> Result<(Checkpoint, PathBuf, PathBuf)> {
    let (part, meta) = state_file(root, source, item);
    no_symlink_ancestors(&part)?;
    no_symlink_ancestors(&meta)?;
    let part_exists = part.exists();
    let meta_exists = meta.exists();
    let fresh = Checkpoint {
        source: source.into(), path: item.path.clone(), size: item.size,
        version: item.version.clone(), committed: 0,
        last_offset: 0, last_size: 0, last_sha256: String::new(),
        expected_sha256: item.sha256.clone(), expected_md5: item.md5.clone()
    };
    if part_exists && !meta_exists {
        // The process could have crashed between create_new(.part) and the
        // first manifest write. Only an EMPTY regular file is safe to adopt.
        // A nonempty orphan might contain irreplaceable bytes: never reset it.
        let metadata = fs::symlink_metadata(&part)?;
        ensure!(metadata.file_type().is_file() && metadata.len() == 0,
            "nonempty or unsafe orphan partial for {}: preserved for review", item.path);
        if create {
            atomic_json(&meta, &fresh)?;
        }
        return Ok((fresh, part, meta));
    }
    ensure!(!meta_exists || part_exists,
        "orphan checkpoint manifest for {}: preserved for review", item.path);
    if !part_exists {
        if create {
            fs::create_dir_all(part.parent().context("no parent")?)?;
            let file = OpenOptions::new().write(true).create_new(true).open(&part)?;
            file.sync_all()?;
            sync_directory(part.parent().context("no parent")?)?;
            atomic_json(&meta, &fresh)?;
        }
        return Ok((fresh, part, meta));
    }
    let cp: Checkpoint = serde_json::from_slice(&fs::read(&meta)?)?;
    ensure!(cp.source == source && cp.path == item.path
        && cp.size == item.size && cp.version == item.version
        && cp.expected_sha256 == item.sha256 && cp.expected_md5 == item.md5,
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
/// Partition immutable Google object ID candidates into verified ranged-transfer
/// inputs and unresolved entries that require a separate export pathway.
fn eligible_google_objects(plan: drive_recovery::RecoveryPlan) -> (Vec<Item>, usize) {
    let total = plan.items.len();
    let unsupported = plan.unsupported;
    let eligible: Vec<Item> = plan
        .items
        .into_iter()
        .filter_map(|item| {
            // Only files with provider-confirmed size AND MD5 may use the
            // native range adapter. Do not infer unknown Google Docs sizes.
            match (item.source_size, item.md5) {
                (Some(size), Some(md5)) => Some(Item {
                    path: item.relative_destination,
                    size,
                    version: item.version,
                    url: None,
                    sha256: None,
                    object_id: Some(item.object_id),
                    md5: Some(md5),
                }),
                _ => None,
            }
        })
        .collect();
    let unresolved = total - eligible.len() + unsupported;
    (eligible, unresolved)
}

fn enforce_listing_floor(source: &Source, count: usize, total_bytes: u64) -> Result<()> {
    if let Some(minimum) = source.min_items {
        ensure!(
            count >= minimum,
            "{} provider returned {} eligible items below configured min_items={minimum}; refusing incomplete inventory",
            source.name, count
        );
    }
    if let Some(minimum) = source.min_total_bytes {
        ensure!(
            total_bytes >= minimum,
            "{} provider listed {total_bytes} bytes below configured min_total_bytes={minimum}; refusing incomplete inventory",
            source.name
        );
    }
    Ok(())
}

fn read_items(source: &Source, listing_timeout: Duration) -> Result<Listing> {
    let (mut items, exports, unresolved) = match &source.backend {
        Backend::Rclone { remote } => {
            let mut command = Command::new("rclone");
            command.args(["lsjson", remote, "--recursive", "--files-only",
                "--checkers", "1", "--tpslimit", "2"]);
            let output = command_output_with_timeout(&mut command, listing_timeout)?;
            let values: Vec<serde_json::Value> = serde_json::from_slice(&output)?;
            let mut items = Vec::new();
            for value in values {
                let (Some(path), Some(size)) = (
                    value.get("Path").and_then(|x| x.as_str()),
                    value.get("Size").and_then(|x| x.as_u64())
                ) else {
                    bail!(
                        "{} returned a file entry without a valid path and nonnegative size;                         refusing incomplete inventory",
                        source.name
                    );
                };
                // Identity is metadata, not a claim of end-to-end cryptographic verification.
                let version = serde_json::to_string(&serde_json::json!({
                    "modtime":value.get("ModTime"), "id":value.get("ID"),
                    "hashes":value.get("Hashes")
                }))?;
                items.push(Item { path: path.into(), size, version, url: None,
                    sha256: None, object_id: None, md5: None });
            }
            (items, Vec::new(), 0)
        }
        Backend::GoogleDriveIds { remote, .. } => {
            // Metadata-only enumeration through the already authorized rclone
            // remote. Payloads are then read natively over Google Drive REST.
            let mut cmd = Command::new("rclone");
            cmd.args(["backend", "query", remote, "trashed = false"]);
            let raw = command_output_with_timeout(&mut cmd, listing_timeout)?;
            ensure!(raw.len() <= 256 * 1024 * 1024,
                "Google Drive ID listing exceeds 256 MiB limit");
            let plan = drive_recovery::plan_recovery_json(&raw)?;
            let (items, unresolved) = eligible_google_objects(plan);
            (items, Vec::new(), unresolved)
        }
        Backend::GoogleWorkspace { remote, .. } => {
            let mut command = Command::new("rclone");
            command.args(["backend", "query", remote, "trashed = false"]);
            let raw = command_output_with_timeout(&mut command, listing_timeout)?;
            ensure!(raw.len() <= 256 * 1024 * 1024,
                "Google Workspace ID inventory exceeds 256 MiB limit");
            let plan = drive_workspace::plan(&raw)?;
            // Without a source revision we cannot safely identify an export.
            let (exports, unversioned): (Vec<_>, Vec<_>) =
                plan.supported.into_iter()
                    .partition(|item| !item.version_key().is_empty());
            (Vec::new(), exports, plan.unsupported_google_types + unversioned.len())
        }
        Backend::HttpManifest { manifest } => {
            no_symlink_ancestors(manifest)?;
            let entries: Vec<HttpEntry> = serde_json::from_slice(&fs::read(manifest)?)?;
            (entries.into_iter().map(|e| Item {
                path:e.path, size:e.size, version:e.version, url: Some(e.url),
                sha256:e.sha256.map(|hash| hash.to_ascii_lowercase()),
                object_id: None, md5: None
            }).collect(), Vec::new(), 0)
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
        if let Some(md5) = &item.md5 {
            ensure!(md5.len() == 32 && md5.bytes().all(|ch| ch.is_ascii_hexdigit()),
                "invalid MD5 for {}", item.path);
        }
    }
    for candidate in &exports {
        safe_relative(&candidate.relative_destination)?;
        ensure!(seen.insert(candidate.relative_destination.clone()),
            "{} contains duplicate Workspace export destination", source.name);
    }
    // Known sizes only. Workspace exports have no size at enumeration time.
    let total_bytes = items.iter().try_fold(0u64, |sum, item| {
        sum.checked_add(item.size)
            .context("provider inventory total size exceeds u64")
    })?;
    enforce_listing_floor(source, items.len() + exports.len(), total_bytes)?;
    items.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Listing { files: items, exports, unresolved })
}
// A malformed/cloud-side runaway listing must not fill the system's temp
// filesystem. In particular, iCloud Photos can enumerate very large albums.
// This ceiling bounds BOTH temporary disk consumption and JSON allocation.
const MAX_PROVIDER_LISTING_BYTES: u64 = 256 * 1024 * 1024;

fn command_output_with_timeout(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    command_output_with_limit(command, timeout, MAX_PROVIDER_LISTING_BYTES)
}

fn command_output_with_limit(
    command: &mut Command,
    timeout: Duration,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    ensure!(max_bytes > 0, "provider listing byte limit must be positive");
    ensure!(timeout > Duration::ZERO, "provider command requires nonzero deadline");
    let stdout = NamedTempFile::new()?;
    command.stdout(Stdio::from(stdout.reopen()?)).stderr(Stdio::null());
    let mut child = command.spawn().context("starting external provider backend")?;
    let start = Instant::now();
    loop {
        // Inspect the actual output file on every poll, including AFTER the
        // subprocess terminates. Never load oversized JSON into Rust memory.
        if stdout.as_file().metadata()?.len() > max_bytes {
            child.kill().ok();
            child.wait().ok();
            bail!("provider inventory exceeded {max_bytes} byte output limit; refusing incomplete listing");
        }
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "provider command failed (status {status})");
            // A final check covers races between metadata sampling and exit.
            ensure!(stdout.as_file().metadata()?.len() <= max_bytes,
                "provider inventory exceeded {max_bytes} byte output limit");
            return fs::read(stdout.path()).context("reading bounded provider listing");
        }
        if start.elapsed() >= timeout {
            child.kill().ok();
            child.wait().ok();
            bail!("provider command exceeded {}s timeout", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(100));
    }
}
/// Bound bytes inside Rust rather than trusting rclone cat --count.
/// No stdout byte beyond the caller's reservation is written to disk.
fn bounded_rclone_chunk(
    command: &mut Command,
    count: u64,
    deadline: Duration,
    staging: &Path,
) -> Result<()> {
    ensure!(count > 0 && count <= 64 * 1024 * 1024,
        "invalid reserved provider chunk size");
    ensure!(deadline > Duration::ZERO, "provider deadline must be positive");
    no_symlink_ancestors(staging)?;
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().context("starting bounded provider transfer")?;
    let mut stdout = child.stdout.take().context("rclone stdout pipe missing")?;
    let path = staging.to_path_buf();
    // A dedicated reader permits a strict deadline on a stalled subprocess.
    let reader = thread::spawn(move || -> Result<u64> {
        let mut file = OpenOptions::new().write(true).truncate(true).open(&path)?;
        let mut written = 0u64;
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let n = stdout.read(&mut buffer)
                .context("provider transfer stream read failed")?;
            if n == 0 {
                break;
            }
            let new_size = written.checked_add(n as u64)
                .context("provider transfer exceeded integer byte range")?;
            ensure!(new_size <= count,
                "rclone returned more bytes than the reserved chunk; transfer rejected");
            file.write_all(&buffer[..n])?;
            written = new_size;
        }
        file.sync_all()?;
        Ok(written)
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if start.elapsed() >= deadline {
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    // After process exit or forced kill the pipe closes. Failed temporary
    // chunks cannot be published by their callers.
    let copied = reader.join().map_err(|_| anyhow!("provider reader thread panicked"))??;
    ensure!(status.is_some(), "rclone transfer timed out");
    ensure!(status.is_some_and(|status| status.success()), "rclone ranged download failed");
    ensure!(copied == count,
        "rclone returned {copied} bytes rather than reserved {count}");
    Ok(())
}

fn download_range(source: &Source, item: &Item, offset: u64, count: u64,
    rate_kib: u64, deadline: Duration, staging: &Path) -> Result<()> {
    match &source.backend {
        Backend::GoogleDriveIds { token_file, .. } => {
            drive_id::read_range(
                token_file, item.object_id.as_deref().context("missing Google immutable ID")?,
                drive_id::RangeSpec { offset, length: count, total: item.size },
                drive_id::TransferLimits { rate_kib, deadline },
                staging
            )?;
        }
        Backend::GoogleWorkspace { .. } => {
            bail!("Workspace exports require bounded single-response transport")
        }
        Backend::Rclone { remote } => {
            let path = format!("{}/{}", remote.trim_end_matches('/'), item.path);
            let mut cmd = Command::new("rclone");
            cmd.args(["cat", &path, "--offset", &offset.to_string(),
                "--count", &count.to_string(), "--bwlimit", &format!("{rate_kib}K"),
                "--retries", "1", "--low-level-retries", "1"]);
            bounded_rclone_chunk(&mut cmd, count, deadline, staging)?;
        }
        Backend::HttpManifest { .. } => {
            let url = item.url.as_deref().context("missing URL in HTTP manifest")?;
            let end = offset.checked_add(count).and_then(|n| n.checked_sub(1))
                .context("invalid byte range")?;
            let client = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(deadline)
                .build()?;
            let mut response = client.get(url)
                .header(RANGE, format!("bytes={offset}-{end}")).send()?;
            ensure!(response.status().as_u16() == 206,
                "server did not honor HTTP Range (206 required)");
            let claimed = response.headers().get(CONTENT_RANGE)
                .context("missing Content-Range")?.to_str()?;
            ensure!(claimed == format!("bytes {offset}-{end}/{}", item.size),
                "server returned incorrect or inconsistent Content-Range");
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
/// A matching path is not automatically a valid archived copy. Fail closed
/// on directories, symbolic links, and obvious size mismatches, without ever
/// modifying whatever is already present at that path.
fn destination_already_archived(target: &Path, item: &Item) -> Result<bool> {
    no_symlink_ancestors(target)?;
    match fs::symlink_metadata(target) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "local archive conflict: existing path is not a regular file: {}",
                target.display()
            );
            ensure!(
                metadata.len() == item.size,
                "local archive conflict: {} has {} bytes, but current source advertises {} bytes; no overwrite attempted",
                target.display(), metadata.len(), item.size
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspecting existing archive path"),
    }
}

fn publish(
    archive_root: &Path,
    source: &str,
    part: &Path,
    meta: Option<&Path>,
    destination: &Path,
    item: &Item,
) -> Result<()> {
    let target = destination.join(safe_relative(&item.path)?);
    no_symlink_ancestors(&target)?;
    ensure!(fs::metadata(part)?.len() == item.size, "cannot publish incomplete file");

    // A single streaming pass produces the independent local SHA-256 receipt
    // and validates any provider MD5 or SHA-256 before creating a final link.
    let sha256 = receipts::fingerprint_file(
        part, item.md5.as_deref(), item.sha256.as_deref(),
    )?;
    fs::create_dir_all(target.parent().context("missing parent")?)?;
    no_symlink_ancestors(target.parent().context("missing parent")?)?;

    // Prepare a durable receipt before publication. If a crash happens now,
    // verification sees a missing target instead of inventing successful
    // publication. Repeated identical preparation is idempotent.
    receipts::record(archive_root, source, &item.path, &item.version, item.size, &sha256)?;

    match fs::hard_link(part, &target) {
        Ok(()) => {
            sync_directory(target.parent().context("missing target parent")?)?;
            receipts::mark_published(
                archive_root, source, &item.path, &item.version, item.size, &sha256
            )?;
            fs::remove_file(part)?;
            if let Some(meta) = meta {
                fs::remove_file(meta)?;
            }
            sync_directory(part.parent().context("missing staging parent")?)?;
            println!("COMPLETE {}", item.path);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("archive destination appeared during immutable publication; preserving existing file");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Complete an interrupted successful hard-link publication, without
/// deleting an unrelated local file of the same name or size. Ordinary legacy
/// archive files without staging state are left entirely untouched.
fn reconcile_published(
    root: &Path,
    source: &str,
    item: &Item,
    target: &Path,
    execute: bool,
) -> Result<()> {
    if !execute {
        return Ok(());
    }
    let (part, meta) = state_file(root, source, item);
    no_symlink_ancestors(&part)?;
    no_symlink_ancestors(&meta)?;
    let part_exists = part.exists();
    let meta_exists = meta.exists();
    if !part_exists && !meta_exists {
        return Ok(());
    }
    ensure!(part_exists && meta_exists,
        "inconsistent old staging state for published path; preserving for review");
    let (checkpoint, _, _) = checkpoint(root, source, item, false)?;
    ensure!(checkpoint.committed == item.size,
        "published path conflicts with incomplete staged item; preserving state");
    let staged = fs::symlink_metadata(&part)?;
    let final_file = fs::symlink_metadata(target)?;
    ensure!(staged.file_type().is_file() && final_file.file_type().is_file(),
        "published path or staging data is not an ordinary file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(staged.dev() == final_file.dev() && staged.ino() == final_file.ino(),
            "published path and partial are different files; refusing cleanup");
    }
    #[cfg(not(unix))]
    bail!("hard-link identity reconciliation is only supported on Unix");
    let sha256 = receipts::fingerprint_file(
        &part, item.md5.as_deref(), item.sha256.as_deref(),
    )?;
    receipts::record(root, source, &item.path, &item.version, item.size, &sha256)?;
    receipts::mark_published(
        root, source, &item.path, &item.version, item.size, &sha256
    )?;
    fs::remove_file(&part)?;
    fs::remove_file(&meta)?;
    sync_directory(part.parent().context("missing staging directory")?)?;
    println!("RECOVERED interrupted publication for {}", item.path);
    Ok(())
}

/// Advance at most one chunk of a source. A failure stays confined to this
/// source for this pass, while other healthy providers get their turn.
/// Execute at most one non-resumable Workspace export in the same global
/// round-robin. Reserve the entire possible byte ceiling BEFORE contacting
/// Google; unused bytes are deliberately NOT refunded.
fn workspace_export_reservation(budget: u64, rate_kib: u64, remaining: Duration) -> u64 {
    budget.min(drive_workspace::GOOGLE_EXPORT_MAX_BYTES)
        .min(rate_kib.saturating_mul(1024)
            .saturating_mul(remaining.as_secs().saturating_sub(4)))
}

fn advance_workspace_source(
    cfg: &Config,
    pending: &mut Pending,
    budget: &mut u64,
    start: Instant,
    time_limit: Duration,
) -> Result<bool> {
    advance_workspace_with_transfer(
        cfg, pending, budget, start, time_limit, drive_workspace::export_small
    )
}

/// Testable adapter boundary; production passes the native Google exporter.
fn advance_workspace_with_transfer<F>(
    cfg: &Config,
    pending: &mut Pending,
    budget: &mut u64,
    start: Instant,
    time_limit: Duration,
    transfer: F,
) -> Result<bool>
where
    F: FnOnce(&Path, &drive_workspace::ExportCandidate,
        &Path, drive_workspace::ExportLimits) -> Result<u64>,
{
    if pending.export_index >= pending.exports.len() {
        return Ok(false);
    }
    let remaining = time_limit.saturating_sub(start.elapsed());
    if remaining < Duration::from_secs(8) {
        return Ok(false);
    }
    let token_file = match &pending.source.backend {
        Backend::GoogleWorkspace { token_file, .. } => token_file,
        _ => bail!("Workspace export queue/backend mismatch"),
    };
    let item = &pending.exports[pending.export_index];
    let rate = cfg.scheduler.bandwidth_kib;
    let cap = workspace_export_reservation(*budget, rate, remaining);
    if cap == 0 {
        return Ok(false);
    }
    let target = pending.source.destination.join(safe_relative(&item.relative_destination)?);
    ensure!(!target.exists(), "Workspace destination appeared before export");

    *budget -= cap;
    let stage_root = cfg.archive_root.join(".syncarium").join("export-staging");
    fs::create_dir_all(&stage_root)?;
    no_symlink_ancestors(&stage_root)?;
    let staging = NamedTempFile::new_in(&stage_root)?;
    let len = transfer(
        token_file, item, staging.path(),
        drive_workspace::ExportLimits {
            max_bytes: cap, rate_kib: rate,
            deadline: remaining.saturating_sub(Duration::from_secs(2)),
        }
    )?;
    ensure!(len > 0 && len <= cap, "Workspace export exceeded reserved budget");

    let completed = Item {
        path: item.relative_destination.clone(),
        size: len,
        version: item.version_key().to_owned(),
        url: None,
        sha256: None,
        object_id: None,
        md5: None,
    };
    publish(
        &cfg.archive_root, &pending.source.name, staging.path(), None,
        &pending.source.destination, &completed
    )?;
    pending.export_index += 1;
    Ok(true)
}

fn advance_source(
    cfg: &Config,
    pending: &mut Pending,
    budget: &mut u64,
    chunk_limit: u64,
    start: Instant,
    time_limit: Duration,
) -> Result<bool> {
    if pending.circuit_open {
        return Ok(false);
    }
    if pending.index >= pending.files.len() {
        return advance_workspace_source(cfg, pending, budget, start, time_limit);
    }
    let item = &pending.files[pending.index];
    let relative = safe_relative(&item.path)?;
    let target = pending.source.destination.join(relative);
    if destination_already_archived(&target, item)? {
        reconcile_published(&cfg.archive_root, &pending.source.name, item, &target, true)?;
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
        publish(
            &cfg.archive_root, &pending.source.name,
            &part, Some(&meta), &pending.source.destination, item,
        )?;
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
        publish(
            &cfg.archive_root, &pending.source.name,
            &part, Some(&meta), &pending.source.destination, item,
        )?;
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

        let remaining = Duration::from_secs(cfg.scheduler.run_seconds)
            .saturating_sub(started.elapsed());
        if remaining < Duration::from_secs(2) {
            let message = format!(
                "{}: pass deadline reached before remote listing; postponed",
                source.name
            );
            eprintln!("SOURCE POSTPONED: {message}");
            issues.push(message);
            continue;
        }
        let listing = match read_items(source, remaining.min(Duration::from_secs(180))) {
            Ok(listing) => listing,
            Err(error) => {
                let message = format!("{}: listing failed: {error:#}", source.name);
                eprintln!("SOURCE FAILED: {message}");
                issues.push(message);
                continue;
            }
        };
        if listing.unresolved > 0 {
            let message = format!(
                "{}: {} Google Drive collision objects not eligible for verified ranged recovery; needs separate handling",
                source.name, listing.unresolved
            );
            eprintln!("SOURCE INCOMPLETE: {message}");
            issues.push(message);
        }
        let items = listing.files;
        let exports = listing.exports;
        // Preview the complete inventory BEFORE persisting any observations.
        // Strict sources (notably iCloud Photos) must never accept a
        // truncated listing or stage its partial snapshot as the new baseline.
        let identity_paths = || items.iter().map(|item| item.path.as_str())
            .chain(exports.iter().map(|item| item.object_fingerprint.as_str()));
        let inspected = match inventory::observe(
            &cfg.archive_root, &source.name, identity_paths(), false
        ) {
            Ok(result) => result,
            Err(error) => {
                let message = format!("{}: inventory ledger failed: {error:#}", source.name);
                eprintln!("SOURCE FAILED: {message}");
                issues.push(message);
                continue;
            }
        };
        if source.require_historical_coverage && inspected.absent > 0 {
            let message = format!(
                "{}: {} previously observed cloud identities missing; strict source held without transfer or inventory mutation",
                source.name, inspected.absent
            );
            eprintln!("SOURCE HELD: {message}");
            issues.push(message);
            continue;
        }
        let observed = if execute {
            match inventory::observe(
                &cfg.archive_root, &source.name, identity_paths(), true
            ) {
                Ok(result) => result,
                Err(error) => {
                    let message = format!("{}: inventory persistence failed: {error:#}", source.name);
                    eprintln!("SOURCE FAILED: {message}");
                    issues.push(message);
                    continue;
                }
            }
        } else {
            inspected
        };
        if observed.absent > 0 {
            let message = format!(
                "{}: {} previously observed remote paths absent from current listing;                 historical inventory retained",
                source.name, observed.absent
            );
            eprintln!("COVERAGE WARNING: {message}");
            issues.push(message);
        }
        println!(
            "{}: {} observed historically, {} newly observed",
            source.name, observed.ever_seen, observed.new
        );
        let mut files = Vec::new();
        for item in items {
            let target = source.destination.join(safe_relative(&item.path)?);
            match destination_already_archived(&target, &item) {
                Ok(true) => {
                    if let Err(error) = reconcile_published(
                        &cfg.archive_root, &source.name, &item, &target, execute
                    ) {
                        let message = format!(
                            "{}: published path has unresolved staged state for {}: {error:#}",
                            source.name, item.path
                        );
                        eprintln!("ITEM HELD: {message}");
                        issues.push(message);
                    }
                    continue;
                },
                Ok(false) => {}
                Err(error) => {
                    let message = format!(
                        "{}: existing archived path held for {}: {error:#}",
                        source.name, item.path
                    );
                    eprintln!("ITEM HELD: {message}");
                    issues.push(message);
                    continue;
                }
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
        let mut export_queue = Vec::new();
        for candidate in exports {
            let target = source.destination.join(safe_relative(&candidate.relative_destination)?);
            if target.exists() {
                if let Err(error) = receipts::check_export_target(
                    &cfg.archive_root, &source.name,
                    &candidate.relative_destination, candidate.version_key(), &target,
                    execute
                ) {
                    let message = format!(
                        "{}: existing Workspace export held for {}: {error:#}",
                        source.name, candidate.relative_destination
                    );
                    eprintln!("ITEM HELD: {message}");
                    issues.push(message);
                }
            } else {
                export_queue.push(candidate);
            }
        }
        println!("{}: {} pending binary item(s), {} pending Workspace export(s)",
            source.name, files.len(), export_queue.len());
        queues.push(Pending {
            source: source.clone(),
            files: files.into_iter().map(|(_, item)| item).collect(),
            index: 0,
            exports: export_queue,
            export_index: 0,
            consecutive_failures: 0,
            circuit_open: false,
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
                Ok(progress) => {
                    if progress {
                        pending.consecutive_failures = 0;
                    }
                    any |= progress;
                }
                Err(error) => {
                    let item = pending.files.get(pending.index)
                        .map(|item| item.path.as_str())
                        .or_else(|| pending.exports.get(pending.export_index)
                            .map(|item| item.relative_destination.as_str()))
                        .unwrap_or("<unknown>");
                    let message = format!(
                        "{}: download held for {}: {error:#}",
                        pending.source.name, item
                    );
                    eprintln!("ITEM HELD: {message}");
                    issues.push(message);
                    // An individual broken file must not starve the rest of
                    // its provider's archive. A brief failure streak opens
                    // the provider's circuit to avoid flooding a broken API.
                    hold_failed_item(pending);
                    if pending.circuit_open {
                        eprintln!(
                            "SOURCE PAUSED: {} reached {} consecutive item failures",
                            pending.source.name, MAX_CONSECUTIVE_ITEM_FAILURES
                        );
                    }
                    any = true;
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
            version: "v1".into(), url: None, sha256: None, object_id: None, md5: None };
        let (cp, part, _) = checkpoint(tmp.path(), "photos", &item, false).unwrap();
        assert_eq!(cp.committed, 0);
        assert!(!part.exists());
    }
    #[test]
    fn atomic_state_detects_source_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let item = Item { path: "x".into(), size: 100, version: "v1".into(), url: None, sha256: None, object_id: None, md5: None };
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
            object_id: None,
            md5: None,
        };
        assert!(publish(tmp.path(), "test", &part, Some(&meta), &destination, &item).is_err());
        assert!(!destination.join("file.bin").exists());
        assert!(part.exists(), "bad checksum must preserve the partial");
        item.sha256 = Some(hex::encode(Sha256::digest(b"correct-data")));
        publish(tmp.path(), "test", &part, Some(&meta), &destination, &item).unwrap();
        assert_eq!(std::fs::read(destination.join("file.bin")).unwrap(), b"correct-data");
        assert!(!part.exists());
    }

    #[test]
    fn item_failures_advance_queue_and_open_circuit_after_three() {
        let source = Source {
            name: "test".into(),
            enabled: true,
            destination: PathBuf::from("/tmp/test"),
            min_items: None,
            min_total_bytes: None,
            require_historical_coverage: false,
            backend: Backend::Rclone { remote: "none:".into() },
        };
        let mut pending = Pending {
            source,
            files: (0..5).map(|i| Item {
                path: format!("item-{i}.bin"),
                size: 1,
                version: "v1".into(),
                url: None,
                sha256: None,
                object_id: None,
                md5: None,
            }).collect(),
            index: 0,
            exports: Vec::new(),
            export_index: 0,
            consecutive_failures: 0,
            circuit_open: false,
        };
        hold_failed_item(&mut pending);
        assert_eq!(pending.index, 1);
        assert!(!pending.circuit_open);
        hold_failed_item(&mut pending);
        assert_eq!(pending.index, 2);
        assert!(!pending.circuit_open);
        hold_failed_item(&mut pending);
        assert_eq!(pending.index, 3);
        assert!(pending.circuit_open);
    }

    #[test]
    fn native_google_listing_reports_unknown_size_hash_and_shortcut_gaps() {
        let raw = serde_json::to_vec(&serde_json::json!([
            {"id":"good","name":"same.bin","mimeType":"application/octet-stream","parents":["root"],"size":"12","md5Checksum":"900150983cd24fb0d6963f7d28e17f72"},
            {"id":"no-hash","name":"same.bin","mimeType":"application/octet-stream","parents":["root"],"size":"12"},
            {"id":"google-doc","name":"same","mimeType":"application/vnd.google-apps.document","parents":["root"]},
            {"id":"shortcut","name":"same.docx","mimeType":"application/vnd.google-apps.shortcut","parents":["root"]}
        ])).unwrap();
        let plan = drive_recovery::plan_recovery_json(&raw).unwrap();
        let (files, unresolved) = eligible_google_objects(plan);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].object_id.as_deref(), Some("good"));
        assert_eq!(unresolved, 3, "all ineligible objects must remain visible");
    }


    #[test]
    fn provider_md5_blocks_bad_publication_and_accepts_verified_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let destination = tmp.path().join("archive");
        std::fs::create_dir(&destination).unwrap();
        let part = tmp.path().join("native.part");
        let meta = tmp.path().join("native.json");
        std::fs::write(&part, b"correct-data").unwrap();
        std::fs::write(&meta, b"{}").unwrap();
        let mut item = Item {
            path: ".rclone-duplicates/objects/xx/safe.bin".into(),
            size: 12,
            version: "immutable-id+version".into(),
            url: None,
            sha256: None,
            object_id: Some("test-google-id".into()),
            md5: Some("00000000000000000000000000000000".into()),
        };
        assert!(publish(tmp.path(), "test", &part, Some(&meta), &destination, &item).is_err());
        assert!(!destination.join(&item.path).exists());
        assert!(part.exists(), "checksum mismatch must preserve staged bytes");
        item.md5 = Some(hex::encode(md5::Md5::digest(b"correct-data")));
        publish(tmp.path(), "test", &part, Some(&meta), &destination, &item).unwrap();
        assert_eq!(std::fs::read(destination.join(&item.path)).unwrap(), b"correct-data");
        assert!(!part.exists());
    }

    #[test]
    fn preexisting_archive_conflicts_fail_closed_and_leave_local_data_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("same-file.bin");
        let item = Item {
            path: "same-file.bin".into(),
            size: 4,
            version: "v1".into(),
            url: None,
            sha256: None,
            object_id: None,
            md5: None,
        };
        assert!(!destination_already_archived(&target, &item).unwrap());
        std::fs::write(&target, b"1234").unwrap();
        assert!(destination_already_archived(&target, &item).unwrap());
        std::fs::write(&target, b"wrong-length").unwrap();
        assert!(destination_already_archived(&target, &item).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"wrong-length");
        std::fs::remove_file(&target).unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(destination_already_archived(&target, &item).is_err());
        assert!(target.is_dir());
    }

    #[test]
    fn empty_orphan_partial_recovers_only_when_executing() {
        let temp = tempfile::tempdir().unwrap();
        let item = Item {
            path: "photo.heic".into(), size: 1_000_000, version: "v1".into(),
            url: None, sha256: None, object_id: None, md5: None
        };
        let (part, meta) = state_file(temp.path(), "photos", &item);
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, b"").unwrap();
        let (preview, _, _) = checkpoint(temp.path(), "photos", &item, false).unwrap();
        assert_eq!(preview.committed, 0);
        assert!(!meta.exists(), "read-only planning must never repair state");
        let (recovered, _, _) = checkpoint(temp.path(), "photos", &item, true).unwrap();
        assert_eq!(recovered.committed, 0);
        assert!(meta.exists(), "execution recovers the safe empty orphan");
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 0);
    }

    #[test]
    fn nonempty_orphan_and_manifest_only_state_are_never_discarded() {
        let temp = tempfile::tempdir().unwrap();
        let item = Item {
            path: "video.mov".into(), size: 10_000_000, version: "v1".into(),
            url: None, sha256: None, object_id: None, md5: None
        };
        let (part, meta) = state_file(temp.path(), "videos", &item);
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, b"precious uncommitted data").unwrap();
        assert!(checkpoint(temp.path(), "videos", &item, false).is_err());
        assert!(checkpoint(temp.path(), "videos", &item, true).is_err());
        assert!(!meta.exists());
        assert_eq!(std::fs::read(&part).unwrap(), b"precious uncommitted data");
        std::fs::remove_file(&part).unwrap();
        std::fs::write(&meta, b"{}").unwrap();
        assert!(checkpoint(temp.path(), "videos", &item, true).is_err());
        assert_eq!(std::fs::read(&meta).unwrap(), b"{}");
    }

    #[test]
    fn interrupted_tail_is_truncated_only_on_execute_and_last_chunk_is_verified() {
        let temp = tempfile::tempdir().unwrap();
        let item = Item {
            path: "resume.bin".into(), size: 100, version: "v1".into(),
            url: None, sha256: None, object_id: None, md5: None
        };
        let (mut cp, part, meta) = checkpoint(temp.path(), "fixture", &item, true).unwrap();
        let chunk = temp.path().join("chunk");
        std::fs::write(&chunk, b"committed").unwrap();
        store_chunk(&part, &meta, &mut cp, &chunk).unwrap();
        std::fs::OpenOptions::new().append(true).open(&part).unwrap()
            .write_all(b"interrupted").unwrap();
        checkpoint(temp.path(), "fixture", &item, false).unwrap();
        assert_eq!(std::fs::metadata(&part).unwrap().len(), 20);
        let (restored, _, _) = checkpoint(temp.path(), "fixture", &item, true).unwrap();
        assert_eq!(restored.committed, 9);
        assert_eq!(std::fs::read(&part).unwrap(), b"committed");
        std::fs::write(&part, b"tampered!").unwrap();
        assert!(checkpoint(temp.path(), "fixture", &item, true).is_err());
        assert_eq!(std::fs::read(&part).unwrap(), b"tampered!");
    }

    #[test]
    fn configured_inventory_floor_rejects_short_and_empty_listings() {
        let source = Source {
            name: "photos".into(),
            enabled: true,
            destination: PathBuf::from("/tmp/photos"),
            min_items: Some(500),
            min_total_bytes: None,
            require_historical_coverage: false,
            backend: Backend::Rclone { remote: "photos:".into() },
        };
        assert!(enforce_listing_floor(&source, 0, 0).is_err());
        assert!(enforce_listing_floor(&source, 499, 0).is_err());
        assert!(enforce_listing_floor(&source, 500, 0).is_ok());
        assert!(enforce_listing_floor(&source, 700, 0).is_ok());
        // A deliberately unconfigured floor permits genuinely empty sources.
        let relaxed = Source { min_items: None, ..source };
        assert!(enforce_listing_floor(&relaxed, 0, 0).is_ok());
    }

    #[test]
    fn completed_hard_link_interrupted_before_cleanup_is_reconciled() {
        let tmp = tempfile::tempdir().unwrap();
        let destination = tmp.path().join("archive");
        std::fs::create_dir(&destination).unwrap();
        let item = Item {
            path: "result.bin".into(), size: 8, version: "v1".into(),
            url: None, sha256: None, object_id: None, md5: None,
        };
        let (mut cp, part, meta) =
            checkpoint(tmp.path(), "fixture", &item, true).unwrap();
        let chunk = tmp.path().join("chunk");
        std::fs::write(&chunk, b"complete").unwrap();
        store_chunk(&part, &meta, &mut cp, &chunk).unwrap();
        let sha = receipts::fingerprint_file(&part, None, None).unwrap();
        receipts::record(tmp.path(), "fixture", &item.path, &item.version, item.size, &sha).unwrap();
        let target = destination.join(&item.path);
        std::fs::hard_link(&part, &target).unwrap();

        reconcile_published(tmp.path(), "fixture", &item, &target, false).unwrap();
        assert!(part.exists(), "read-only operations must not clean up staging");

        reconcile_published(tmp.path(), "fixture", &item, &target, true).unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"complete");
        assert!(!part.exists());
        assert!(!meta.exists());
    }

    #[test]
    fn different_inode_at_existing_target_never_destroys_staging_state() {
        let tmp = tempfile::tempdir().unwrap();
        let destination = tmp.path().join("archive");
        std::fs::create_dir(&destination).unwrap();
        let item = Item {
            path: "same-size.bin".into(), size: 8, version: "v1".into(),
            url: None, sha256: None, object_id: None, md5: None,
        };
        let (mut cp, part, meta) =
            checkpoint(tmp.path(), "fixture", &item, true).unwrap();
        let chunk = tmp.path().join("chunk");
        std::fs::write(&chunk, b"original").unwrap();
        store_chunk(&part, &meta, &mut cp, &chunk).unwrap();
        let target = destination.join(&item.path);
        std::fs::write(&target, b"otherone").unwrap();
        assert!(reconcile_published(tmp.path(), "fixture", &item, &target, true).is_err());
        assert_eq!(std::fs::read(&part).unwrap(), b"original");
        assert_eq!(std::fs::read(&target).unwrap(), b"otherone");
        assert!(meta.exists());
    }

    #[test]
    fn workspace_export_consumes_reserved_global_budget_and_publishes_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        let destination = root.join("workspace");
        std::fs::create_dir_all(&destination).unwrap();
        let cfg: Config = toml::from_str(&format!(
            "archive_root = {:?}\n[scheduler]\nbudget_mib = 12\nchunk_mib = 1\nbandwidth_kib = 10240\nrun_seconds = 120\n[[sources]]\nname = \"workspace\"\nkind = \"google_workspace\"\nenabled = true\nremote = \"unused:\"\ntoken_file = {:?}\ndestination = {:?}",
            root.to_str().unwrap(),
            tmp.path().join("unused-private-token").to_str().unwrap(),
            destination.to_str().unwrap()
        )).unwrap();
        let source = cfg.sources[0].clone();
        let candidate = drive_workspace::plan(&serde_json::to_vec(&serde_json::json!([
            {"id":"example-workspace-id","name":"test",
             "mimeType":"application/vnd.google-apps.document",
             "modifiedTime":"2026-10-19T12:00:00Z"}
        ])).unwrap()).unwrap().supported.remove(0);
        let relative = candidate.relative_destination.clone();
        let mut pending = Pending {
            source, files: Vec::new(), index: 0,
            exports: vec![candidate], export_index: 0,
            consecutive_failures: 0, circuit_open: false,
        };
        let mut budget = 12 * 1024 * 1024;
        let initial = budget;
        let progressed = advance_workspace_with_transfer(
            &cfg, &mut pending, &mut budget,
            Instant::now(), Duration::from_secs(120),
            |_, _, stage, limits| {
                assert_eq!(limits.max_bytes, 10_000_000);
                std::fs::write(stage, b"sample").unwrap();
                Ok(6)
            }
        ).unwrap();
        assert!(progressed);
        assert_eq!(pending.export_index, 1);
        assert_eq!(initial - budget, 10_000_000,
            "unused bytes stay reserved instead of being falsely refunded");
        let target = destination.join(relative);
        assert_eq!(std::fs::read(&target).unwrap(), b"sample");
        receipts::check_export_target(
            &root, "workspace",
            pending.exports[0].relative_destination.as_str(),
            pending.exports[0].version_key(), &target, false
        ).unwrap();
    }

    #[test]
    fn workspace_reservation_is_hard_capped_by_time_and_global_bytes() {
        assert_eq!(workspace_export_reservation(100_000_000, 10240,
            Duration::from_secs(60)), 10_000_000);
        assert_eq!(workspace_export_reservation(900, 10240,
            Duration::from_secs(60)), 900);
        assert_eq!(workspace_export_reservation(100_000_000, 1,
            Duration::from_secs(6)), 2048);
        assert_eq!(workspace_export_reservation(1_000_000, 10240,
            Duration::from_secs(3)), 0);
    }

    #[test]
    fn failed_workspace_export_does_not_publish_bytes_or_refund_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("archive");
        let dest = root.join("workspace");
        std::fs::create_dir_all(&dest).unwrap();
        let cfg: Config = toml::from_str(&format!(
            "archive_root = {:?}\n[scheduler]\nbudget_mib = 12\nchunk_mib = 1\nbandwidth_kib = 10240\nrun_seconds = 120\n[[sources]]\nname = \"workspace\"\nkind = \"google_workspace\"\nenabled = true\nremote = \"unused:\"\ntoken_file = {:?}\ndestination = {:?}",
            root.to_str().unwrap(),
            tmp.path().join("unused-private-token").to_str().unwrap(),
            dest.to_str().unwrap()
        )).unwrap();
        let candidate = drive_workspace::plan(&serde_json::to_vec(&serde_json::json!([
            {"id":"failure-id","name":"test",
             "mimeType":"application/vnd.google-apps.spreadsheet",
             "modifiedTime":"2026-10-19T12:00:00Z"}
        ])).unwrap()).unwrap().supported.remove(0);
        let target = dest.join(&candidate.relative_destination);
        let mut pending = Pending {
            source: cfg.sources[0].clone(), files: Vec::new(), index: 0,
            exports: vec![candidate], export_index: 0,
            consecutive_failures: 0, circuit_open: false,
        };
        let mut budget = 12 * 1024 * 1024;
        let result = advance_workspace_with_transfer(
            &cfg, &mut pending, &mut budget, Instant::now(),
            Duration::from_secs(120),
            |_, _, staging, _| {
                std::fs::write(staging, b"partial").unwrap();
                anyhow::bail!("simulated network interruption")
            }
        );
        assert!(result.is_err());
        assert_eq!(budget, 12 * 1024 * 1024 - 10_000_000);
        assert_eq!(pending.export_index, 0);
        assert!(!target.exists());
    }

    #[test]
    fn bytes_floor_rejects_tiny_aggregate_even_at_expected_item_count() {
        let source = Source {
            name: "icloud-photos".into(),
            enabled: true,
            destination: PathBuf::from("/tmp/icloud-photos"),
            min_items: Some(2),
            min_total_bytes: Some(10_000),
            require_historical_coverage: true,
            backend: Backend::Rclone { remote: "photos:".into() },
        };
        assert!(enforce_listing_floor(&source, 2, 9_999).is_err());
        assert!(enforce_listing_floor(&source, 2, 10_000).is_ok());
        assert!(enforce_listing_floor(&source, 1, 100_000).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn provider_stdout_is_bounded_before_json_can_be_loaded() {
        let mut overflowing = Command::new("sh");
        overflowing.args(["-c", "head -c 8192 /dev/zero"]);
        let err = command_output_with_limit(
            &mut overflowing, Duration::from_secs(5), 1024
        ).unwrap_err();
        assert!(format!("{err:#}").contains("output limit"));

        let mut small = Command::new("sh");
        small.args(["-c", "printf 'valid-small-listing'"]);
        let output = command_output_with_limit(
            &mut small, Duration::from_secs(5), 1024
        ).unwrap();
        assert_eq!(output, b"valid-small-listing");
    }

    #[cfg(unix)]
    #[test]
    fn timed_out_provider_command_is_terminated() {
        let mut delayed = Command::new("sh");
        delayed.args(["-c", "sleep 2"]);
        let err = command_output_with_limit(
            &mut delayed, Duration::from_millis(50), 1024
        ).unwrap_err();
        assert!(format!("{err:#}").contains("timeout"));
    }

    #[cfg(unix)]
    #[test]
    fn rclone_chunk_rejects_overlong_and_short_success_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("chunk");
        std::fs::write(&stage, b"").unwrap();
        let mut good = Command::new("sh");
        good.args(["-c", "printf '12345678'"]);
        bounded_rclone_chunk(&mut good, 8, Duration::from_secs(3), &stage).unwrap();
        assert_eq!(std::fs::read(&stage).unwrap(), b"12345678");

        let mut overflow = Command::new("sh");
        overflow.args(["-c", "head -c 8192 /dev/zero"]);
        let err = bounded_rclone_chunk(
            &mut overflow, 8, Duration::from_secs(3), &stage).unwrap_err();
        assert!(format!("{err:#}").contains("more bytes"));
        assert!(std::fs::metadata(&stage).unwrap().len() <= 8,
            "a bad provider must not write beyond the byte reservation");

        let mut short = Command::new("sh");
        short.args(["-c", "printf 'abc'"]);
        let err = bounded_rclone_chunk(
            &mut short, 8, Duration::from_secs(3), &stage).unwrap_err();
        assert!(format!("{err:#}").contains("rather than reserved"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_provider_transfer_times_out_without_publishing() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("chunk");
        std::fs::write(&stage, b"").unwrap();
        let mut stalled = Command::new("sh");
        stalled.args(["-c", "exec sleep 2"]);
        let err = bounded_rclone_chunk(
            &mut stalled, 32, Duration::from_millis(70), &stage).unwrap_err();
        assert!(format!("{err:#}").contains("timed out"));
    }

}
