# Source adapters

A source entry has a unique name, `enabled` switch, absolute destination, and a `kind` with provider-specific fields. All sources share the same global run budget. Adding a new source does **not** require a new systemd service.

## rclone bridge (current cloud compatibility)

```toml
[[sources]]
name = "icloud-drive"
enabled = true
kind = "rclone"
remote = "icloud:"
destination = "/drive/icloud/drive"
```

Credentials and 2FA remain in the *local* rclone configuration, not in GitHub. Backend operations used: `lsjson --recursive --files-only`, `cat --offset --count`. The bridge invokes the existing external **Go** rclone binary: it is not a pure-Rust client.

Before enabling an adapter, verify its remote `lsd` listing, a tiny ranged file download, end-of-file handling, large-file resumption, and whether the backend lists duplicate file paths. The current planner refuses ambiguous repeated relative paths, because silently losing the second file is worse than admitting lack of coverage.

## Native HTTPS manifest

```toml
[[sources]]
name = "partner-archive"
enabled = true
kind = "http_manifest"
manifest = "/home/YOUR_USER/.config/syncarium/partner-archive.json"
destination = "/drive/partner"
```

The local manifest is a **JSON array**:

```json
[
  {
    "path": "folder/sample.bin",
    "size": 1024000,
    "url": "https://example.com/files/sample.bin",
    "version": "etag-or-immutable-version-id-v1",
    "sha256": "64-character lowercase SHA-256 digest if known"
  }
]
```

`path` is always relative to the destination and cannot contain absolute paths, `..`, newlines or backslashes. `size` is the exact byte size. `url` must be HTTPS and support **206 Partial Content** and `Content-Range` for requested bytes. `version` must change if the data changes. Optional `sha256` is a 64-character hex digest; when present, the entire assembled file is SHA-256 verified **before** publication, and mismatches leave the partial intact for review. The value shown above is explanatory, not a valid hash; replace it with a real digest or omit it. This adapter never writes to the remote.

Put manifests with signed/private URLs in `~/.config/syncarium/` with `chmod 600`; never in the public repo. Test endpoints independently before production use. Tokens embedded in a URL should be treated as passwords.

## Proposed native adapter interface

- `list_items`: returns a provider object ID, relative destination path, exact size, provider version and optional checksums.
- `read_range`: reads a bounded byte range and provides response metadata suitable for version pinning.
- `capabilities`: declares whether ranges, checksums and immutable identifiers actually work.
- `credentials`: consume an already-authorized, operator-configured provider connection or owner-private short-lived token file. **No custom OAuth, login browser flow, or automatic token renewal in Syncarium.**

This interface is currently conceptual; the implementation uses one Rust enum to dispatch adapters. Future backend refactors must maintain current checkpoint safety semantics.

Candidates: OpenDAL-based Google Drive, WebDAV, SFTP, custom download APIs, and future iCloud-specific Rust libraries. Do not accept a backend merely because it can list filenames; correctness of unique identities, variants and range integrity matters.

## Google Drive collision audit

The optional `drive-collisions --remote gdrive:` command uses the read-only `rclone backend query gdrive: 'trashed = false'` API and analyzes stable object IDs, parent IDs and names in Rust. It reports aggregate counts, including duplicate folder descendants invisible to a path-based listing. No personal object names are printed. This is not duplicate **recovery**: the Rust engine will need an ID-addressed downloader and provenance sidecars before it can replace the legacy Google recovery job. Query coverage and configured Docs export formats can affect results.

## ID recovery plans

The `drive-recovery-plan` operation is independent of the ordinary path-based scheduler and is read-only. It maps colliding immutable Google file IDs to stable SHA-256-derived destinations, covering descendants of colliding folders. Run it with `--source google-drive` for the `google-drive` TOML entry; `--fixture <absolute JSON path>` is available for offline validation. No object data is fetched and no payload is published. Actual ID-based recovery needs a backend capable of budgeted, resumable range reads by immutable object ID. Do **not** replace the legacy duplicate-recovery service on the strength of the planner alone.

**HTTP redirects are deliberately disabled.** The final asset URL must directly return HTTP 206 with the exact expected `Content-Range: bytes START-END/TOTAL`. Syncarium verifies the total is the declared manifest size rather than just comparing the downloaded chunk length. If a server responds with a redirect or a different total size, the chunk is rejected and the current partial remains intact.

## Experimental native Google Drive ID-range transfers

Use `kind = "google_drive_ids"` with `remote = "gdrive:"` for metadata listing and `token_file = "/absolute/private/path"` for a short-lived Google OAuth bearer access token. This backend uses **native Rust** `files.get?alt=media` with a strict 206 + exact Content-Range check, then writes through the same durable, globally budgeted chunks as other sources. Completed binary objects are MD5-verified against Google's `md5Checksum` metadata before no-clobber publication. Only objects selected as duplicate-name candidates by the ID recovery planner, with a valid size and MD5, are downloaded. Google Workspace exports, shortcuts and items without byte sizes or MD5 remain unsupported. **Never commit or paste token files.** The token file must be a regular non-symlink, owner-private file (mode 0600); expired tokens require manual regeneration for now. This is not automatic Google OAuth integration. Keep this adapter disabled until controlled live testing.

For the native Google ID adapter, use a distinct destination such as `/drive/gdrive-immutable-ids`. Its output contains `.rclone-duplicates/objects/` beneath that root. Using `/drive/gdrive/.id-recovery` is invalid alongside the ordinary `/drive/gdrive` source because the configuration rejects nested destinations to prevent namespace collisions.

### Ineligible Google objects are never silently ignored

The native ID adapter counts every planned collision candidate it cannot download under its conservative contract: unsupported types (including shortcuts), unknown-size exports, and binary files missing a valid MD5. `audit --json` reports `unresolved_provider_items`; `run` continues eligible bytes inside its budget but returns nonzero for incomplete source coverage. No attempt is made to fabricate file sizes or infer an absent hash.

Google object IDs are deliberately omitted from serialized recovery plans and local destination names. The native Rust HTTP adapter also strips underlying HTTP error details that could include immutable Drive IDs or signed request information from logs. File integrity is validated against Google-provided MD5 before publication, but this is not a cryptographic proof against malicious provider responses.

## Credential policy

Google OAuth registration, consent screens, refresh tokens, callback servers, and automatic renewal are **out of scope by design**. For ordinary Google Drive use the existing authorized `gdrive:` rclone connection; rclone may manage its own token state. If authorization ceases to work, manual intervention is expected. For the experimental native Google ID adapter, `token_file` holds a **short-lived, manually supplied token**. This adapter must stay disabled in unattended schedules while manually renewed access tokens are the only available authorization mechanism. Credentials must never appear in a checked-in TOML file.

## Optional inventory count floor

Any `[[sources]]` entry can specify `min_items = 10000` (choose a threshold appropriate for **your own verified inventory**). If the current adapter reports fewer eligible file entries, Syncarium rejects the entire listing and declines to persist it or download from that source during the run. This is especially useful for large photo libraries vulnerable to unexpectedly truncated API listings. Leave the field absent for genuinely empty or newly created sources. An item floor does **not** prove completeness; it only detects some unusually small provider responses. For `google_drive_ids`, counts apply to eligible ID-selected binaries, not total Google account objects.

## Google Workspace native export readiness (metadata-only)

Google Workspace documents do **not** behave like ordinary byte-range files. The `drive-workspace-plan --source google-drive [--json]` command enumerates Google object IDs through an existing locally authorized rclone Drive remote (or an offline `--fixture /absolute/path`), maps the legacy export formats **Google Docs → DOCX, Sheets → XLSX, Slides → PPTX, Drawings → SVG**, and records ID-derived collision-free output paths. Original names and cloud IDs are omitted from JSON output and from those local paths. Unsupported Google-native types (such as Forms) are counted and cause a nonzero exit status. This operation **does not download** any exports.

The opt-in `google_workspace` source uses a strictly bounded Rust HTTP helper for small exports using Google's `files.export` endpoint. It refuses missing or excessive `Content-Length`, unexpected `Content-Type`, redirects and non-200 responses **before writing payload**. The Google API documents a maximum of 10 MB per `files.export` response. Export content has no provider source-file MD5 or guaranteed range resume, and its exact size is normally unknown before the HTTP headers. The helper is integrated as a **disabled-by-default single-shot export queue** in the shared scheduler. It reserves its full allowable response size before starting a request, never refunds that reservation, and checks source modification metadata immediately before and after transfer. Files with missing source versions are held as unresolved. It is **not** a resumable ranged adapter: oversized/unknown-length exports are rejected, and interruptions require restarting that export from byte zero. No large Workspace exports are marked completed. Google also provides an asynchronous `files.download` API, but its result may indicate `partialDownloadAllowed=false`; it requires separate resumability testing. No new Google OAuth implementation is planned. <https://developers.google.com/workspace/drive/api/reference/rest/v3/files/export>

### Opt-in bounded Google Workspace source

The separate, disabled-by-default `[[sources]]` entry uses `kind = "google_workspace"`, `remote = "gdrive:"`, and an owner-private **manually supplied** `token_file`. Keep its destination disjoint from `/drive/gdrive`, for example `/drive/gdrive-workspace`. Metadata enumeration continues through the pre-existing rclone Google remote. For payload, the native Rust adapter uses the short-lived token only for `files.get` modification-time/MIME checks and `files.export` content. No OAuth login or token renewal has been added.

Unlike binary Drive files, Workspace export size is unknown until its HTTP response headers. The scheduler reserves the **entire** possible export allowance (at most 10,000,000 bytes; also bounded by remaining global pass budget and deadline) before the request, deducts that reservation even if the export uses fewer bytes, and rejects missing `Content-Length`, an over-cap length, redirects and MIME mismatches before any payload write. Failures are isolated per source and the already reserved allowance is not refunded. Exports without modification-time metadata are explicitly unresolved. Successful bytes are SHA-256 recorded, hard-linked to ID-derived names without overwrite, and then eligible for the offline `verify` command. Source changes detected before or after transfer abort the export. **A Google API result that omits the required headers is not an invitation to weaken the budget contract.**

This is a single-shot downloader, not resumable Workspace export streaming. A crash may leave an untracked temporary staging file under `.syncarium/export-staging`; avoid deleting any such data automatically before inspection. The normal Google Docs `files.export` 10 MB limit remains in force. No real-account QA has been performed; leave disabled until an explicitly approved manual test.

### Revision-aware Workspace archive behavior

Workspace filenames are derived from both immutable object identity and the modification-time revision, so a newer version never silently overwrites the previous export. The cloud observation ledger uses the stable object ID, not a version-specific output path. Google metadata is rechecked both before and after `files.export`; a mismatch aborts publication. Receipt publication uses a prepared → published state, so an interrupted link operation cannot certify an unrelated same-sized file. A prepared receipt with an existing target requires a true SHA-256 comparison before it can be finalized. Read-only modes may check it but never repair metadata. This still does **not** make Workspace byte streams resumable; a failed export restarts from zero on a later pass.

## Per-source coverage fail-closed guards

`min_items` guards the number of currently eligible listing entries. `min_total_bytes` guards the **sum of the declared source byte sizes** for ordinary rclone, native Google binary-ID, or HTTPS manifest sources. The latter cannot be configured for `google_workspace`, whose export sizes are unknown until the HTTP response. Both thresholds are **optional and explicitly configured** from a conservatively measured baseline, not inferred from a possibly incomplete first scan. Any inventory below either configured floor fails before archive metadata or file payload changes.

`require_historical_coverage = true` is a separate opt-in strict mode. The source compares its current file paths or Workspace stable object IDs to every identity already recorded in the append-only ledger. If one or more identities are absent, the source is held for this pass, the newly seen identities are **not merged**, and no file from that source is downloaded. Other healthy sources remain eligible. This also triggers when objects were intentionally deleted, moved, or renamed remotely; it is intentionally cautious and is not evidence that local files should ever be deleted. The existing ledger retains previously observed source identities, but **cannot detect an item never observed in any earlier successful enumeration**.

## Why rclone remains the default provider bridge

See [Provider strategy](PROVIDER_STRATEGY.md). Apache OpenDAL is a production-grade Rust storage framework with Google Drive support, but it does not automatically solve Google duplicate-name identity preservation, manual OAuth requirements, or iCloud Photos backend reliability. Syncarium stays Rust-first without rebuilding entire cloud provider implementations. External rclone listing processes are deadline-limited and their JSON inventories are size-limited before being treated as valid source observations.

## Conservative validation of rclone ranged reads

Syncarium does not trust `rclone cat --count` to enforce its transfer reservation. The Rust adapter now reads rclone stdout through a bounded pipe and writes no more than the exact requested chunk into its temporary staging file. If the provider writes excessive bytes, stops early, errors or stalls, the chunk is rejected. A rejected temporary chunk is never committed to a checkpoint or published. This protects local disk and the scheduler's accepted payload, but **cannot guarantee how many bytes rclone or an experimental iCloud backend fetches internally from the remote**, including hidden provider caches and metadata requests.

The explicit `probe-range` command is a separate diagnostic for rclone-backed sources. Without `--execute`, it only prints a preview and does not contact a provider. With `--execute`, it enumerates the configured source, selects one file at least 128 KiB long, and reads exactly three 64 KiB windows at offsets 0, 32 KiB and 64 KiB (192 KiB accepted stdout total). Overlap comparisons expose many ignored-offset or incorrect byte-range implementations. The probe reports uniform/repeating samples as inconclusive rather than claiming success. It creates **no archive files, receipts, or checkpoint state**. This is not proof of complete Photos enumeration, preservation of originals, Live Photo coverage, or long-file resume compatibility.

### Resume-consistency probe for path-based sources

Every subsequent chunk now requests the last `min(previous_chunk_bytes, 64 KiB)` saved bytes **as a prefix of the same ranged payload request** that brings in new data. Rust compares the returned prefix against the saved partial and commits only the new suffix. The complete request, including the overlapping prefix, is deducted from the normal global run budget. This catches many providers that ignore or misapply range offsets **within a single pass**, in addition to cross-run source drift. Completed checkpoint files awaiting publication are verified against a separate remote-tail request. Checks apply across rclone, Google binary-ID, and HTTPS range adapters; Workspace exports use their separate nonresumable protocol. Identical/repetitive remote bytes can make an offset error undetectable; the check is not a cryptographic proof of every remote byte or a replacement for the standalone `probe-range` diagnostic.

## iCloud-style photo inventory classification and limits

The opt-in `photos-inventory` diagnostic shares the regular rclone `lsjson` parser and rejects malformed path/size entries and duplicate output paths rather than silently interpreting partial data as a full archive. It uses filename extensions only to categorize still images, motion files, metadata sidecars, and other items. Same-folder and case-insensitive filename-stem HEIC/JPEG + MOV/MP4 matches are **candidate pairings**, not asset relationships returned by Apple. Ambiguous groups are counted separately. The report does not expose raw paths, original names, or IDs; unrecognized extensions are grouped under `other_or_unrecognized` rather than disclosed. Missing provider ID or modification metadata are reported as gaps without assuming that every rclone backend must provide them. Use this diagnostic to discover provider representation, not to certify missing Live Photo companions or original-quality exports. Unlike generic `plan`, it requires explicit `--execute` for live metadata enumeration.

## Read-only snapshot parity checks

`photos-compare` uses the same strict rclone `lsjson` parser for two local saved inventory inputs. It refuses invalid/unsafe/duplicate paths and missing nonnegative sizes. The comparator reports only aggregates, with no raw filenames, folder names or provider IDs serialized. It distinguishes source metadata snapshots from a local archive listing with `--reference-kind archive`; local IDs and timestamps cannot meaningfully be compared to Apple-side asset IDs and mtimes. Differing paths and sizes produce a nonzero outcome, as do comparable provider ID/mtime changes when both fields exist. Empty snapshots are inconclusive, not proof of synchronization. The command never calls `rclone` or the archive scheduler, and never manufactures cryptographic verification from metadata equality.

## Bounded local photo signature checks

`photos-headers` is entirely separate from provider range reads. It accepts a private local `rclone lsjson` manifest, checks selected manifest-relative file paths only inside the configured source destination, and reads at most 128 local header bytes per file upon explicit `--execute`. It refuses symlinked paths, incompatible recognized signatures and advertised-size conflicts, and counts unknown signatures separately. Manifest inputs are parsed with the common strict duplicate/path/size checks; invalid manifests fail closed. The default sample is 128 supported-format files, maximum 10,000. Reports contain aggregate counts only, never original media names. This provides format-plausibility diagnostics, **not** original-asset authentication or companion completeness.

## Independent checksum reference limitations

`photos-checksums` accepts a local private JSON list of expected relative paths, byte sizes and SHA-256 digests. It hashes existing archive media **only after explicit opt-in** and uses a separate file/byte budget to avoid scanning an entire 18 TB archive by accident. The data source for the expected digests is not inferred: an iCloud `lsjson` response that lacks trustworthy hashes **cannot** be turned into proof of the Apple originals. If expected SHA-256 is absent, the entry is counted unverified and causes nonzero completion. Duplicate/unsafe paths and malformed digests fail before inspection. The JSON report contains only counts; it does not leak original filenames or reference hashes. A matching hash says the local bytes agree with the supplied reference, not that the provider exported all assets or retained Apple-specific edit/companion metadata.

## Explicit Live Photo resource evidence

The optional `photos-asset-evidence` command accepts two **saved local JSON** inputs: a normalized private source mapping that explicitly groups asset identifiers and resource roles, and a provider `lsjson` list. It compares exact paths and optional expected byte sizes, not guesses based on same-stem HEIC/MOV names. Only an original `photo` resource and `paired_video` role can satisfy original Live Photo role coverage; edited `full_size_paired_video` is distinct. Unresolved mapping entries fail instead of certifying recovery. There is no native Apple metadata acquisition in Syncarium yet; the command must not imply that rclone supplies these identifiers. See [ASSET_EVIDENCE](ASSET_EVIDENCE.md).
