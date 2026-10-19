# Syncarium

**One Rust executable. Many read-only data sources. One global bandwidth budget. One local archive.**

Syncarium consolidates download-only archival from Google Drive, iCloud Drive, iCloud Photos, and future external services. It is intentionally **not** a two-way synchronization engine: remote deletes never delete local files, and existing local files are never overwritten.

The core scheduler, transfer logic, checkpointing, HTTP implementation, command-line interface, and safety checks are written in **Rust**. A native HTTPS ranged-download adapter is provided. An **optional** rclone adapter bridges to existing cloud authorizations while future native Rust integrations are developed; that bridge executes rclone (a Go binary). **Do not describe the current system as 100% Rust end-to-end.**

## Status

**v0.1 prototype: NOT READY FOR UNATTENDED PRODUCTION USE.** A source-aware scheduler and durable byte-range downloader are implemented, with initial tests. End-to-end behavior with the rclone iCloud Photos backend and duplicate Google Drive objects has not been validated; data-completeness audits are still outstanding. No timers are enabled or started by repository changes.

## Current architecture

- One global **128 MiB payload allowance per run**; source listings and protocol overhead may use additional traffic.
- One transfer at a time, **512 KiB/s** cap; **8 MiB chunks**, **15-minute** pass deadline, and systemd once per hour.
- Weighted scheduling is **not yet implemented**; current code rotates starting sources each run and interleaves one chunk per source.
- **Fault isolation:** an expired/failed source listing or an invalid individual checkpoint does not suppress healthy sources. Individual failed transfers are held while other files proceed; a three-failure circuit breaker pauses repeatedly failing backends for the current pass. The pass still reports failure (nonzero exit) if any source or file was held.
- Preserve completed chunk data under **/drive/.syncarium/partials**, with a JSON checkpoint and last-chunk SHA-256 check.
- **Local progress inspection:** `syncarium status` and `syncarium status --json` report staged bytes and partial file counts without contacting cloud accounts.
- **Append-only observation ledger:** each successful executing source listing records SHA-256 fingerprints of every observed remote path. Later listings flag previously seen items that disappear, without deleting local files or erasing the history. This detects some API listing regressions, not unseen content.
- **Read-only coverage audit:** `syncarium audit [--json]` compares provider listings with local files, separating missing, partial, present-size-matched, and conflicting files. It does not certify content integrity or provider listing completeness.
- Atomic publication of a completed file via a **no-clobber hard link**. Interrupted chunks do not replace completed chunks. Local destination files are immutable.
- Source metadata/version changes halt resumption rather than silently stitching together two versions.
- No passwords, tokens, local data, or file manifests containing signed URLs in this public repository.
- Source adapters currently: `rclone` (optional external executable) and `http_manifest` (native Rust/HTTPS range reads).

### Local destinations

| Source | Remote / adapter | Destination |
| --- | --- | --- |
| Google Drive | `gdrive:` | `/drive/gdrive` |
| iCloud Drive | `icloud:` | `/drive/icloud/drive` |
| iCloud Photos primary library | `icloudphotos:PrimarySync/All Photos` | `/drive/icloud/photos/primary` |

These are **existing** local destinations. Syncarium must not move or delete their content.

## Documentation (read before running)

1. **[Local setup](docs/LOCAL_SETUP.md)** — exact commands and steps required on EndeavourOS + Hyprland; build, rclone remotes, filesystem, TOML, and systemd.
2. **[Operator handbook](docs/OPERATIONS.md)** — start/stop, failure recovery, reauthentication, throttling, logs, and incident response.
3. **[Migration plan](docs/MIGRATION.md)** — carefully replacing the former Google Drive and Apple downloader services; no immediate deletion.
4. **[Architecture](docs/ARCHITECTURE.md)** — transfer invariants, checkpoint protocol, fairness, integrity limits, and design.
5. **[Adapters](docs/ADAPTERS.md)** — adding an independent source, native HTTP, rclone compatibility, and missing capabilities.

Sample configuration: [config.example.toml](config.example.toml). Checked-in systemd units: [systemd/user](systemd/user).

## Local CLI

```fish
syncarium --config ~/.config/syncarium/config.toml doctor
syncarium --config ~/.config/syncarium/config.toml plan
syncarium --config ~/.config/syncarium/config.toml run
# Only once approved for a live test:
syncarium --config ~/.config/syncarium/config.toml run --execute
```

Both `plan` and `run` without `--execute` are read-only. A systemd service **would execute downloads**, so do not enable it until local configuration has been reviewed.

## Important limitations

The current rclone backend uses **paths**, not provider-native immutable object IDs. Duplicate-named Google Drive objects cannot be safely represented; the current planner **rejects duplicate output paths** instead of claiming coverage. Google Workspace document exports and Apple-specific metadata are not independently inventoried or validated. Apple Photos listings and ranged-read behavior need live QA. Very large listings may consume time; not every backend guarantees HTTP byte ranges. HTTP manifests require stable absolute URLs and an accurate source version string.

New features should improve coverage without weakening download-only, no-delete, no-overwrite, or mount safety.

License: MIT.
