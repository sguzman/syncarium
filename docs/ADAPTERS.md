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
