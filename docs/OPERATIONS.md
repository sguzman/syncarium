# Operations and troubleshooting

## Inspect without downloading

```fish
syncarium --config ~/.config/syncarium/config.toml doctor
syncarium --config ~/.config/syncarium/config.toml plan
syncarium --config ~/.config/syncarium/config.toml status
systemctl --user status syncarium.timer
systemctl --user status syncarium.service
journalctl --user -u syncarium.service -n 100 --no-pager
```

`plan` contacts enabled source APIs for metadata. It **never downloads file payload**.

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

If Apple asks for 2FA again, refresh the relevant rclone remote from an interactive terminal. Syncarium must never store Apple credentials in its public TOML or logs. Do not attempt unattended password entry. A failed source listing terminates a pass and **does not erase** local content.

## A transfer refuses to run

Check, in order: mount still present (`findmnt --mountpoint /drive`), local destination writable, config entries unique and nonoverlapping, remote listed by `rclone listremotes`, and current source authorization. For HTTP, verify server supports Range and accurate Content-Range.

## Partial file held for review

A checkpoint can refuse to resume if source size/version changes, an unexpected truncated file is found, or a prior run crashed before it wrote metadata. Preserve the state files and investigate. Do **not** delete incomplete files simply to silence an error. Syncarium currently has no automated repair or state-import command.

## Bandwidth and conflicts

The configured byte budget applies to **all Syncarium sources combined**, but the old `google-drive-rclone.service` is an entirely separate process. If both are active, actual network traffic can exceed your intended cap. See [MIGRATION](MIGRATION.md) and do not enable the Syncarium timer until their relationship is resolved.

## Documentation / data hygiene

Never paste `rclone config show`, personal image listings, 2FA codes or local TOML containing credentials into GitHub. Persist downloaded content under `/drive`; code-only artifacts go to the repository. Logs can contain **source paths** and should be handled as potentially private.
