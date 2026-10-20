#!/usr/bin/env bash
# One controlled real cloud->archive transfer pass using existing rclone auth.
# Does not install timers, alter credentials, or modify existing sync services.
set -euo pipefail
umask 077

if [[ $# -ne 1 ]]; then
    printf 'Usage: bash scripts/first-live-sync.sh {icloud-drive|icloud-photos|google-drive}\n' >&2
    exit 2
fi
source_name="$1"
case "$source_name" in
    google-drive)
        remote='gdrive:'
        destination='/drive/gdrive'
        legacy_service='google-drive-rclone.service'
        ;;
    icloud-drive)
        remote='icloud:'
        destination='/drive/icloud/drive'
        legacy_service='apple-sync-drive.service'
        ;;
    icloud-photos)
        remote='icloudphotos:PrimarySync/All Photos'
        destination='/drive/icloud/photos/primary'
        legacy_service='apple-sync-photos.service'
        ;;
    *)
        printf 'Unknown source: %s\n' "$source_name" >&2
        exit 2
        ;;
esac

for program in cargo rclone findmnt systemctl; do
    if ! command -v "$program" >/dev/null 2>&1; then
        printf 'BLOCKED: %s is missing; no cloud downloads attempted.\n' "$program" >&2
        exit 1
    fi
done

mount_info="$(findmnt -n --mountpoint /drive -o TARGET,FSTYPE,OPTIONS)" || {
    echo 'BLOCKED: /drive is not a mounted filesystem.' >&2
    exit 1
}
read -r mount_path mount_type mount_options <<<"$mount_info"
if [[ "$mount_path" != '/drive' || "$mount_type" != 'ext4' ||
      ",$mount_options," != *',rw,'* ]]; then
    echo 'BLOCKED: /drive must be a mounted read-write ext4 filesystem.' >&2
    exit 1
fi
if [[ -L /drive || -L "$destination" ]]; then
    echo 'BLOCKED: archive root or destination is a symlink.' >&2
    exit 1
fi
if [[ ! -d "$destination" || ! -w "$destination" ]]; then
    printf 'BLOCKED: archive destination absent or unwritable: %s\n' "$destination" >&2
    exit 1
fi
if systemctl --user is-active --quiet "$legacy_service"; then
    printf 'BLOCKED: existing %s is active. No service changed.\n' \
        "$legacy_service" >&2
    exit 1
fi
remote_name="$(printf '%s' "$remote" | cut -d: -f1):"
if ! rclone listremotes | grep -Fxq -- "$remote_name"; then
    printf 'BLOCKED: existing rclone remote %s is unavailable. No auth changed.\n' \
        "$remote_name" >&2
    exit 1
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT
config="$scratch/first-live-sync.toml"
cat >"$config" <<EOF
archive_root = "/drive"
required_mount = "/drive"

[scheduler]
budget_mib = 4
chunk_mib = 1
bandwidth_kib = 512
run_seconds = 300

[[sources]]
name = "$source_name"
kind = "rclone"
enabled = true
remote = "$remote"
destination = "$destination"
EOF

echo 'BUILD: compiling current Syncarium for one-source real transfer.'
cargo build --release --manifest-path "$repo_root/Cargo.toml"
binary="$repo_root/target/release/syncarium"
"$binary" --config "$config" doctor
printf 'REAL SYNC START: %s -> %s\n' "$remote" "$destination"
echo 'LIMIT: one pass, 4 MiB accepted cloud payload, 512 KiB/s, 5-minute scheduler. Provider internal traffic and metadata are extra.'
echo 'LEGACY: services/timers unchanged; this job does not install or enable a timer.'
"$binary" --config "$config" run --source "$source_name" --execute
echo 'REAL SYNC PASS COMPLETED. Any newly published files are under the existing archive destination.'
"$binary" --config "$config" status
