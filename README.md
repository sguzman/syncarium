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

### Google Drive immutable-ID diagnostic

Run `syncarium --config ~/.config/syncarium/config.toml drive-collisions --remote gdrive: --json` to inspect Google Drive object IDs for duplicate sibling names, Google Docs export-name collisions and files obscured by duplicate-named folders. This is metadata-only, can consume API requests, and has a 180-second query timeout. It **does not** yet download these duplicate objects. It returns nonzero when collisions are detected.

## Planned Google Drive ID recovery

Use `drive-recovery-plan --source google-drive [--json]` to build a deterministic **read-only** list of identity-addressed recovery destinations for colliding Google Drive files and descendants of ambiguous folders. The planner maps immutable object IDs to SHA-256-based local names beneath `.rclone-duplicates/objects/`, avoiding exposure of private original names in output filenames. Size-unknown Google Docs exports are reported separately and **not** assumed to fit a byte budget. Neither planning nor the current collision audit downloads anything. The optional `google_drive_ids` adapter now implements native Rust ranged transfers for eligible known-size binary collision objects with provider MD5, subject to the global budget. It remains disabled by default and **has not passed live Google account QA**. rclone `backend copyid` is deliberately not used because it cannot meet strict cross-run chunk budgets for arbitrary large files.

## Experimental native Google Drive ID-range adapter

The optional `google_drive_ids` backend uses rclone **only for Drive object ID inventory** and Rust HTTPS for authenticated `files.get?alt=media` byte-range payload reads. It participates in the same scheduler, 8 MiB chunks, 128 MiB global allowance, checkpoints and no-overwrite publishing. The adapter requires a short-lived **local bearer access-token file** readable only by its owner. Expired tokens stop transfers; automatic OAuth renewal is not yet supported. Only collision candidates with known sizes and provider MD5 are eligible. Google-native Docs exports, files without known size/checksum, and already existing legacy recovery objects are **not** automatically migrated or declared archived. This adapter is disabled in the example, and live Google compatibility has not yet been verified.

**Native Google coverage accounting:** objects without valid size and MD5, including Google-native exports and shortcuts, are now explicitly counted as unresolved in the audit; runs report incomplete coverage rather than claiming success simply because eligible binaries downloaded.


## Authentication policy: manual intervention, no custom OAuth project

Syncarium deliberately **does not implement OAuth consent screens, login servers, browser automation, refresh-token acquisition, or a token-refresh subsystem**. Authentication setup and renewal are operator responsibilities. Provider adapters may reuse an *already configured* external backend (for example the existing rclone remotes), including whatever normal token management that backend itself supports, but Syncarium makes **no guarantee of uninterrupted authorization**.

For the native Rust Google ID adapter specifically, `token_file` contains a **short-lived access token** supplied manually. Once that token expires, the adapter reports a recoverable authentication failure and stops that source for the pass. It does not try to refresh the token or trigger sign-in. Short-lived manual access tokens are **not an unattended hourly authentication solution**. Do not enable this experimental adapter in a recurring timer unless there is a separately reviewed, supportable credential arrangement.

Other already-authorized sources can continue within the same bandwidth budget. Nothing in Syncarium may delete or overwrite local archive data because authentication expires. There is **no planned custom Google OAuth implementation**; improving the transfer engine, source coverage, and archive integrity takes priority.

- **Existing destination conflict protection:** completed paths must be regular files whose size matches the currently listed source item. Mismatched sizes, directories and symbolic links are preserved and flagged, not silently counted as archived or overwritten. A matching length is only a preliminary check, not a verified content hash.

- **Power-loss hardening:** durable directory flushes for checkpoint renames and completed hard-link publication, plus safe automatic repair of empty partials left by an interrupted initial checkpoint write. Nonempty or ambiguous orphan state is preserved for investigation.

- **Optional inventory sanity floors:** per-source `min_items` rejects implausibly short listings, useful when a provider temporarily omits thousands of photos. Thresholds are configured manually in private TOML after establishing a baseline.

- **Local SHA-256 receipts:** each newly published file has an immutable receipt recording exactly which local bytes were assembled. `syncarium verify [--source NAME] [--json]` rehashes those files without contacting cloud accounts; preexisting files without receipts are explicitly outside this verification.

- **Interrupted publication reconciliation:** a restarted executing pass can clean stale staging links from an already completed file only after confirming same-file inode identity, completed checkpoint, and matching receipt. Different-file conflicts are preserved.

- **Workspace export readiness:** `drive-workspace-plan [--json]` maps Google Docs/Sheets/Slides/Drawings to DOCX/XLSX/PPTX/SVG without exposing private names or downloading data. An experimental, disabled-by-default `google_workspace` backend now integrates these single-response exports into the same global round-robin scheduler. It reserves its full transfer ceiling before each request, checks the Google document modification metadata before and after downloading, and never overwrites existing archive data. The backend still requires controlled real-provider validation and is not suitable for unattended hourly operation with manually supplied short-lived tokens. Unsupported Google-native documents remain visible as coverage gaps.

**Experimental Workspace transfer source:** `kind = "google_workspace"` can share the normal round-robin with rclone/iCloud and binary Google ID sources. Its single-shot exports are capped by an upfront byte reservation and independently checked against Google source modification metadata. No custom OAuth renewal is implemented, and no live Google export QA has been performed. Keep it disabled outside an operator-controlled test.

- **Workspace version history:** edited Google Docs/Sheets/Slides/Drawings now map to distinct SHA-derived revision filenames. The source observation ledger still tracks each document by stable object ID rather than treating edits as vanished files.
- **Two-phase completion receipts:** each SHA-256 receipt is first prepared, then marked published after the final hard link is durable. After an interrupted publication, an existing same-sized file must match the recorded SHA-256 before the prepared receipt can be finalized.

## Strict incomplete-listing protection (opt-in)

For large libraries such as iCloud Photos, sources may set `min_items`, `min_total_bytes`, and `require_historical_coverage = true`. The first two enforce a minimum file count and **sum of provider-reported sizes** (known-size objects only); a listing below either floor is rejected before remote observation or downloads. Strict historical mode additionally previews the append-only ID/path ledger **before writing** and holds the entire source if *any* previously observed cloud identity disappears. The rejected listing is never merged into the historical ledger or used for transfers; healthy sources can still continue. These optional guards don't prove the provider ever listed every photo; absent objects that were never observed remain undetectable. A legitimate cloud deletion or rename also holds strict mode until the situation is reviewed. Defaults are off so existing source behavior stays unchanged.

## Why rclone is still involved

Syncarium is **Rust-first, not a from-scratch Rust replacement for rclone**. Ordinary cloud providers (especially iCloud) use the installed rclone executable for listing and ranged reads, while Rust owns global scheduling, archive integrity and source isolation. The optional native Google-ID/Workspace payload readers are experimental. See [Provider strategy](docs/PROVIDER_STRATEGY.md) for the maturity comparison with Apache OpenDAL and the acceptance criteria before any native-provider migration. The opt-in `probe-range` CLI can later test rclone's offset handling without publishing archive data; [operational instructions](docs/OPERATIONS.md) document its limits.
