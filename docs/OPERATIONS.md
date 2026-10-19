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
