# Operations and troubleshooting

## Inspect without downloading

```fish
syncarium --config ~/.config/syncarium/config.toml doctor
syncarium --config ~/.config/syncarium/config.toml plan
syncarium --config ~/.config/syncarium/config.toml status
syncarium --config ~/.config/syncarium/config.toml status --json
syncarium --config ~/.config/syncarium/config.toml audit
syncarium --config ~/.config/syncarium/config.toml audit --json
systemctl --user status syncarium.timer
systemctl --user status syncarium.service
journalctl --user -u syncarium.service -n 100 --no-pager
```

`plan` contacts enabled source APIs for metadata. It **never downloads file payload**. `status` inspects local checkpoint manifests without contacting any cloud service. Its staged-byte totals are **not** an estimate of completed archive coverage. `audit` compares current provider-listed file paths and sizes with local files, reporting missing and staged items; it does not download media data. It cannot prove complete source listings or cryptographic content integrity.

## Start exactly one controlled run

Use a low trial budget (`budget_mib = 1` and `chunk_mib = 1`) with only one source marked `enabled = true`. Then run `syncarium --config ... run --execute` from Kitty. Inspect the checkpoint files and destination after the pass, but do not delete staging files to force progress.

## Enable/disable scheduled execution

```fish
systemctl --user enable --now syncarium.timer
systemctl --user disable --now syncarium.timer
systemctl --user stop syncarium.service
```

Stopping a running service during a chunk can leave an uncommitted tail. The checkpoint parser truncates only that uncommitted tail on the next live run. The systemd service is not a resident daemon; it launches one bounded pass then exits. The timer starts a fresh invocation every hour.

## Authentication expiration

If Apple asks for 2FA again, refresh the relevant rclone remote from an interactive terminal. Syncarium must never store Apple credentials in its public TOML or logs. Do not attempt unattended password entry. A failed source listing is reported and skipped for that pass, while other healthy sources can continue. The overall run returns a **nonzero** exit status if any source fails, and no local content is erased.

## A transfer refuses to run

Check, in order: mount still present (`findmnt --mountpoint /drive`), local destination writable, config entries unique and nonoverlapping, remote listed by `rclone listremotes`, and current source authorization. For HTTP, verify server supports Range and accurate Content-Range.

## Partial file held for review

A checkpoint can refuse to resume if source size/version changes, an unexpected truncated file is found, or a prior run crashed before it wrote metadata. Preserve the state files and investigate. Do **not** delete incomplete files simply to silence an error. Syncarium currently has no automated repair or state-import command.

## Bandwidth and conflicts

The configured byte budget applies to **all Syncarium sources combined**, but the old `google-drive-rclone.service` is an entirely separate process. If both are active, actual network traffic can exceed your intended cap. See [MIGRATION](MIGRATION.md) and do not enable the Syncarium timer until their relationship is resolved.

## Documentation / data hygiene

Never paste `rclone config show`, personal image listings, 2FA codes or local TOML containing credentials into GitHub. Persist downloaded content under `/drive`; code-only artifacts go to the repository. Logs can contain **source paths** and should be handled as potentially private.

## Partial success is not total success

A pass may report `SOURCE FAILED` or `ITEM HELD` and still commit completed chunks from other sources. This is intentional: a temporary Apple authentication error must not block Google Drive. The process exits nonzero when any provider or checkpoint fails. Investigate the reported source and renew authentication or resolve its partial state; do not delete the working archive. A `PASS DONE` message with a nonzero exit status is **not** proof of full backup coverage.

## Failed item handling

When a file transfer or publication fails, that item's checkpoint is preserved and the scheduler proceeds to the next file in that same source. After **three consecutive file failures**, the source is paused for the remainder of that pass to avoid repeatedly hammering an expired or failing backend. A successful chunk or already-present file resets the streak. Failures still cause a **nonzero exit**, preserving honest monitoring; the next timer pass can retry the held paths.

## Listings consume the run deadline

The configured `run_seconds` includes cloud enumeration. When one provider takes too long to list its files, Syncarium limits that listing to the remaining time and reports sources it could not process. This is deliberate: it never starts a late download beyond the user's allotted window. Enabling multiple sources requires considering their inventory latency as well as transfer speed.

## Detecting missing cloud items over time

Once `run --execute` completes a valid source inventory listing, Syncarium keeps an append-only history of path fingerprints in `/drive/.syncarium/inventories`. A later listing missing any previously observed path reports `COVERAGE WARNING`, even if the cloud provider returns an apparently successful response. `audit --json` exposes `historical_absent` and `historical_ever_seen`. This is a cautious warning: intentional cloud deletion or rename can also cause it. Source deletions **never remove local files or past observation records**. The initial observation ledger does not guarantee all Apple Photos or duplicate Google Drive objects were ever visible.

## Google Drive identity audit

Use `syncarium --config ~/.config/syncarium/config.toml drive-collisions --remote gdrive: --json` to inspect duplicate filenames/folders from provider object metadata. It may use considerable API listing bandwidth but does not download file contents or change either archive. It fails with a nonzero status on duplicates, empty response or query error. This is a diagnostic; keep the legacy Google duplicate-recovery tool until Syncarium implements equivalent recovery.

## Read-only ID recovery planning

`syncarium --config ~/.config/syncarium/config.toml drive-recovery-plan --source google-drive --json` computes stable hashed destination paths for Google Drive collision candidates. This consumes metadata API requests but does **not** download files or create directories. For an offline test, add `--fixture /absolute/path/to/google-objects.json` with a local Google `backend query` fixture (treat it as private). Output does not include raw names or object IDs, but filenames and other source data may still be sensitive. Size-unknown items and unsupported shortcuts are reported explicitly. **Do not disable legacy Google recovery yet.**

## Native Google ID coverage gaps

The opt-in `google_drive_ids` adapter only downloads candidate objects with an exact source size **and** valid Google provider MD5. Unsupported shortcuts, Google Workspace exports, and files without verification metadata are counted as `unresolved_provider_items` by `audit --json`. When using `plan` or `run --execute`, unresolved items produce a `SOURCE INCOMPLETE` warning and a **nonzero** overall exit status, even if eligible files make progress. This prevents silently declaring a partially supported Google archive complete. Resolve these objects with a future dedicated exporter; do not delete the legacy downloader yet.

## Manual Google authentication / expired credentials

Authentication is explicitly an **operator task**, not an automation feature planned for Syncarium.

- Existing `gdrive:`: keep using the locally configured rclone remote; when its authorization fails, reauthorize it manually through rclone's supported interactive flow, then retry a bounded run.
- Experimental `google_drive_ids`: the private `token_file` contains a **short-lived Google access token**, which may expire before an hourly unattended run. If rejected, renew it manually through an authorized local method or simply leave this adapter disabled. Do not attempt to paste tokens into chat, commits, bug reports, or shell history.
- Do **not** create a new Google OAuth client, callback server, or token-refresh daemon just to use this project. Syncarium does not automatically obtain or renew credentials.
- A failed authentication must not reset or delete archives/checkpoints. Other healthy sources may still download, and the failing run returns a nonzero status for monitoring.

This manual boundary is intentional. There is no automatic Google OAuth renewal milestone on the roadmap.

## Local archive name or size conflict

When Syncarium reports `existing archived path held`, its destination already exists but is not a regular file with the expected byte count, or is a symlink. Syncarium intentionally leaves the existing path untouched and continues with other safe objects. Inspect the path and source inventory; **do not delete files or rename whole archive trees just to clear the warning**. The read-only audit identifies mismatched-size files. Matching sizes alone do not prove content hashes are identical.

## Crash recovery after a power interruption

A zero-byte partial file left behind before its checkpoint was first created is now recovered automatically on the **next live executing run**; `plan` remains read-only. If the orphan partial contains any bytes, or only a JSON manifest survives, Syncarium stops that item and preserves both state and data for investigation. Previously committed chunks are verified on resumption; excess uncommitted bytes are truncated only in execute mode.

Publishing now forces the completed destination directory entry to disk before the corresponding staged hard link and checkpoint are removed. Do not move or edit `/drive/.syncarium/partials` while a download is running. Interrupted state is supposed to survive a reboot so downloads can resume without starting over.

## Guard against unexpectedly small cloud inventories

After inspecting a known-good source listing, optionally set `min_items` on that source in local TOML (for example, a conservatively low bound for your iCloud Photos library). A later listing below the floor fails **before any state observation or transfers from that source**. The failure is reported in the ordinary source listing error path while other sources continue normally. Do not set a minimum larger than the number of eligible items the adapter actually returns, and do not mistake satisfying this threshold for a full media completeness audit. `plan` and `audit` also detect this condition without downloading files.

## Verify published local archive receipts (no cloud access)

Run `syncarium --config ~/.config/syncarium/config.toml verify` to read and SHA-256 check all previously receipted Syncarium downloads, or add `--source icloud-photos` to check one configured source. Use `--json` for aggregate machine-readable counts. The command reads full local files (potentially **many gigabytes**), so run it deliberately when disk activity is acceptable, not after every hourly pass. It never downloads, uploads, deletes or repairs anything. A nonzero exit indicates missing, mismatched or invalid receipts. Legacy files that predate this receipt feature remain **unverified** until an explicit future provenance migration; they are not automatically certified based on matching names or sizes.

## Interrupted completion cleanup

After a reboot, a file may already be present at the final destination while its full staged partial and JSON checkpoint remain. The next **executing** Syncarium pass can safely reconcile this only when the staged partial and final archive path are actually hard links to the **same inode**, the saved checkpoint is complete and valid, and provider checksum checks pass. It logs `RECOVERED interrupted publication`. If a different file occupies the destination, Syncarium preserves both and reports the unresolved staging state instead of overwriting or deleting either. `run` without `--execute` never performs cleanup.

## Inspect Google Workspace export readiness (no downloads)

`syncarium --config ~/.config/syncarium/config.toml drive-workspace-plan --source google-drive --json` lists Google Workspace export candidates with deterministic hashed output filenames. Use `--fixture /absolute/private/google-objects.json` for offline checking without provider access; the local fixture can contain sensitive original filenames and object IDs and must never be committed. This command reads metadata, not exported bytes. It counts unsupported Google-native types and reports them with a nonzero status. The optional `google_workspace` backend now integrates bounded single-shot exports through the common global byte budget, but remains **disabled in the sample TOML and unvalidated against live Google exports**. It requires an owner-private, short-lived, manually supplied access token; **do not enable it under the hourly timer**. Oversized, unversioned and unknown-length exports remain unresolved. An interrupted export is retried whole on a later pass, not resumed by byte range. Keep the legacy exporter until equivalent coverage is validated.

## Experimental single-shot Workspace transfers

Leave `google_workspace` disabled until a deliberate, controlled live QA session. It requires the same manual, short-lived access-token handling as `google_drive_ids`; do not create an OAuth app or refresh daemon. Configure a separate `/drive/gdrive-workspace` destination and retain the normal `gdrive:` metadata remote. The scheduler rotates Workspace exports alongside other sources under one conservatively charged byte ceiling and time limit. The charge is the reserved maximum even for smaller responses. A failed or interrupted export is attempted again from the beginning on a future run; it is **not** resumable. Files larger than the remaining allowance or without stable modification metadata are not published. A stale existing output, mismatched receipt or changed source version is held for human review, never overwritten.

## Revisioned Workspace exports

Google Workspace documents edited after a previous export receive a **new ID-and-revision-hashed filename**, preserving earlier archived versions rather than overwriting them. The cloud observation inventory keys them by stable Google ID, not output filename, so an edit isn't automatically a disappearance warning. Exports lacking `modifiedTime` metadata are held as unresolved; the native transport rechecks the MIME and modification timestamp through Google's API before and after copying. If the document changes during the export, it is not published.

Completion receipts have a prepared/published phase. A power failure in between may leave a prepared SHA-256 receipt and an existing output file. Dry-run verification may rehash that single file to validate it without changing state; an executing pass can mark it published only when the bytes match. Unrelated same-sized files are never accepted, overwritten or deleted. Existing archives without a receipt remain unverified; use `syncarium verify` when you deliberately want complete local disk rehashing.

## Strict listing safety for iCloud Photos

After establishing a trusted baseline with metadata-only `plan`/`audit`, you may opt in per source with `require_historical_coverage = true`, and optionally a conservative `min_total_bytes` and `min_items`. Do **not** guess thresholds from the account's advertised totals; use the actual file entries/bytes reported by your configured adapter. The count and byte floors reject a listing **before** writing ledger state or downloading that source. Strict historical mode holds a source with `SOURCE HELD` if even one previously seen path or Workspace ID goes missing, and does not incorporate new identities from the suspicious listing. An executing run still permits independent healthy sources to proceed and returns nonzero to report incomplete coverage.

Intentional remote deletion, rename, or access-permission changes can also trigger strict mode. Existing archives are never deleted. When this occurs, inspect whether the provider listing is complete and whether the remote change was intentional; do not reset the append-only ledger or delete staging just to silence the hold. Strict mode cannot reconstruct items the provider never enumerated in the first place. It is **disabled by default**; ordinary source behavior remains warning-only for historical disappearances.
