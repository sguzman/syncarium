# Local installation and initial setup (EndeavourOS / Hyprland)

This guide intentionally separates **repository preparation**, **cloud authentication**, **dry-run verification**, and **enabling recurring downloads**. Nothing should become autonomous until its source listing and a small transfer have been verified locally.

## 0. Checklist

You need: an EndeavourOS user session with `systemd --user`, Fish and Kitty, working Rust `cargo`/`rustc`, `git`, an 18 TB ext4 disk mounted at **/drive**, and a readable/writable `/drive`. Existing rclone is **optional**, but currently required for the three initial cloud sources. The initial remote names assumed by the sample are `gdrive:`, `icloud:` and `icloudphotos:`.

Run in your **normal terminal**, not via a noninteractive Jobflick execution that needs a sudo password:

```fish
rustc --version
cargo --version
rclone version
findmnt --mountpoint /drive
df -h /drive
rclone listremotes
```

If Rust is absent, install your EndeavourOS `rust` and `cargo` packages using your usual package-manager workflow (`yay -S --needed rust cargo`), or use the official rustup toolchain. Do not install both competing Rust toolchain managers without a reason.

Never share `rclone config show`, 2FA codes, API keys, OAuth refresh tokens, or signed URLs in an issue, commit, screenshot, or conversation.

## 1. Get the source, build locally

Choose an appropriate existing source checkout parent (example below, do not overwrite an existing checkout):

```fish
mkdir -p ~/Code/Sync
git clone https://github.com/sguzman/syncarium.git ~/Code/Sync/syncarium
cd ~/Code/Sync/syncarium
cargo test
cargo build --release
mkdir -p ~/.local/bin
install -m 755 target/release/syncarium ~/.local/bin/syncarium
```

All actual runtime control is in the compiled **Rust binary**, with no Python or Bash script.

## 2. Ensure the source remotes work first

Run the following **read-only listing commands** before enabling them in TOML:

```fish
rclone lsd gdrive:
rclone lsd icloud:
rclone lsd icloudphotos:
rclone lsd icloudphotos:PrimarySync
```

The iCloud remotes were configured separately: `icloud:` uses the Drive service and `icloudphotos:` uses the Photos service. Authentication stays in the local rclone configuration, generally `~/.config/rclone/rclone.conf`. These remotes are **not** reconfigured by Syncarium. Apple's trusted-device/2FA refresh may be required periodically. Authentication failures must stop downloads, not remove local files.

The iCloud Photos source path intentionally targets **All Photos**, not every album. Shared libraries, album memberships, edits, recently deleted items, and other Apple account categories are separate coverage work. Google Drive can contain multiple objects sharing a name; the current path-based adapter **does not recover duplicates**.

## 3. Prepare filesystem destinations

Before preparing directories, **confirm /drive is actually mounted**, rather than merely being an empty directory on the system disk:

```fish
findmnt --mountpoint /drive
mkdir -p /drive/gdrive /drive/icloud/drive /drive/icloud/photos/primary
test -w /drive
```

Do **not** empty or rename those directories. The directory `/drive/.syncarium` is reserved for local checkpoints. It must reside on the same filesystem as the completed files, since hard-link publication requires a single filesystem.

## 4. Create your private TOML

```fish
mkdir -p ~/.config/syncarium
cp ~/Code/Sync/syncarium/config.example.toml ~/.config/syncarium/config.toml
chmod 600 ~/.config/syncarium/config.toml
```

Edit `~/.config/syncarium/config.toml` to your requirements. By default, *all sources are disabled* (`enabled = false`). To preview a particular source, set **only that source's** `enabled = true`. The following settings apply globally to all active sources:

- `budget_mib = 128`: at most 128 MiB of requested source bytes per invocation, not per provider.
- `chunk_mib = 8`: append successful chunks to durable local checkpoints.
- `bandwidth_kib = 512`: one active transfer capped at 512 KiB/s.
- `run_seconds = 900`: stop scheduling additional chunks after about 15 minutes.
- `required_mount = "/drive"`: refuse running if the ext4 archive disk is not mounted read-write.

To add a new source, add another `[[sources]]` entry with a **unique** `name` and **non-overlapping** `destination`. Supported adapters are documented in [ADAPTERS](ADAPTERS.md).

## 5. Test without transfers

```fish
~/.local/bin/syncarium --config ~/.config/syncarium/config.toml doctor
~/.local/bin/syncarium --config ~/.config/syncarium/config.toml plan
```

`plan` lists remote metadata but does not download file content. It may still consume requests/bandwidth for metadata discovery and fail if Apple authentication expires.

Only after reviewing a **single test source** and keeping the budget tiny (for example `budget_mib = 1`, `chunk_mib = 1`) should you voluntarily test:

```fish
~/.local/bin/syncarium --config ~/.config/syncarium/config.toml run --execute
```

Inspect the resulting checkpoint and any newly completed file. Never infer that success for a single file proves complete archive coverage or reliable large-video ranged reads.

## 6. Install user systemd units, but DO NOT START THEM YET

The repository stores the canonical unit definitions. Link them only when the local binary and config exist:

```fish
mkdir -p ~/.config/systemd/user
ln -s ~/Code/Sync/syncarium/systemd/user/syncarium.service ~/.config/systemd/user/syncarium.service
ln -s ~/Code/Sync/syncarium/systemd/user/syncarium.timer ~/.config/systemd/user/syncarium.timer
systemctl --user daemon-reload
systemctl --user status syncarium.timer
```

If any target file/symlink already exists, inspect it instead of overwriting it. The command above will fail harmlessly on existing destinations.

**Do not run** `systemctl --user enable --now syncarium.timer` yet. First complete the migration checklist, including coordinating the existing Google Drive traffic. These units do not need root or a separate long-running daemon.

## 7. Final cutover (only after verification)

See [MIGRATION](MIGRATION.md). Stop old source-specific timers only when feature coverage is adequate and rollback is understood. Then, and only then:

```fish
systemctl --user enable --now syncarium.timer
systemctl --user list-timers syncarium.timer
```

If the service is stopped, or the machine is offline, the timer won't catch up all missed runs; `Persistent=false` prevents a surprise burst after login. You can start a single bounded run with `systemctl --user start syncarium.service`.

### What stays out of GitHub

Never commit `~/.config/rclone/rclone.conf`, your **local** `config.toml`, manifests with signed URLs, ``/drive` data, or `/drive/.syncarium`. Public documentation must not expose private filenames or album lists from your personal account.

## Optional native Google ID recovery: credential requirements

Do not enable this experimental adapter yet. For native ID-addressed Google blob transfers, Syncarium requires a local Google OAuth **access token** with Drive content read scope. Store only the short-lived token text in `~/.config/syncarium/google-access-token` and `chmod 600` the file. The Rust reader never logs it, but the token expires and does not refresh automatically. The program **does not import your rclone token, create OAuth clients, or force a new sign-in**. Obtaining a suitable token by an approved local method is a remaining setup dependency; do not paste a token into ChatGPT, GitHub, or a shell history command. Use the disabled TOML example from `config.example.toml` after live tests have been planned. This recovery destination must not overlap with any other enabled source destination.

The native Google ID recovery source must use a **separate, nonnested** destination, such as `/drive/gdrive-immutable-ids`. Do not place its destination under `/drive/gdrive`: Syncarium rejects overlapping source destinations even when one source is disabled. The adapter's own ID-keyed output tree lives inside this separate destination.
