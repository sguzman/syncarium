# Migration from google-drive-rclone and apple-sync

**Do not uninstall or delete either old project yet.** This document is a cutover checklist, not a command to immediately disable any existing service.

## Existing projects

- `sguzman/google-drive-rclone`: mature one-way downloader for `/drive/gdrive`, with Google-specific duplicate-ID recovery and Docs export behavior. The normal copy job may run indefinitely and uses 512 KiB/s.
- `sguzman/apple-sync`: separate iCloud Drive / Photos hourly units and resumable range implementation. At last review its timers were **disabled**, but verify locally rather than assuming.
- `sguzman/syncarium`: new Rust engine. Its global budget is separate from legacy services until the cutover.

## Safe cutover order

1. Keep the old Google Drive service running normally while Syncarium is developed. Do not modify, stop or delete the existing downloaded archive.
2. Confirm `/drive/gdrive`, `/drive/icloud/drive` and `/drive/icloud/photos/primary` paths and mount health. Existing directories are authoritative; never recopy or move them merely for migration.
3. Build Syncarium and test `doctor`, `plan`, plus a deliberately tiny live ranged download, using **one source only** in private TOML.
4. Resolve known coverage gaps: Google-native document exports, duplicate-name objects, Apple photo assets, Live Photos and incomplete API listings. If Syncarium still lacks these capabilities, **do not declare replacement complete**.
5. Check which old user timers/services exist and which are enabled before changing anything:

```fish
systemctl --user status google-drive-rclone.timer
systemctl --user status google-drive-rclone.service
systemctl --user status apple-sync-drive.timer
systemctl --user status apple-sync-photos.timer
systemctl --user list-timers --all
```

6. After feature parity and explicit approval, disable the old timers and **stop any active old transfer** as a controlled switchover. Warning: stopping the active Google Drive process may interrupt a file. Do not surprise-interrupt an active download.
7. Enable the Syncarium timer, monitor its first few runs, and audit expected cloud item counts and local files.
8. Keep old repositories and state as rollback references until successful reconciliation over time.

## Future switchover commands (NOT for immediate execution)

```fish
systemctl --user disable --now google-drive-rclone.timer
systemctl --user stop google-drive-rclone.service
systemctl --user disable --now apple-sync-drive.timer apple-sync-photos.timer
systemctl --user stop apple-sync-drive.service apple-sync-photos.service
systemctl --user enable --now syncarium.timer
```

These names are based on the old repositories, not a live inspection of the user's local system. A unit not installed locally may report `not found`. Do not run this group blindly.

## Rollback

Disable the Syncarium timer and stop its current service. Re-enable the original known-good Google Drive timer only when you have checked current state. The completed archive contents must not be deleted during rollback. Retain checkpoint directories; they are evidence of pending work and may be useful for future import support.
