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
    "version": "etag-or-immutable-version-id-v1"
  }
]
```

`path` is always relative to the destination and cannot contain absolute paths, `..`, newlines or backslashes. `size` is the exact byte size. `url` must be HTTPS and support **206 Partial Content** and `Content-Range` for requested bytes. `version` must change if the data changes. This adapter never writes to the remote.

Put manifests with signed/private URLs in `~/.config/syncarium/` with `chmod 600`; never in the public repo. Test endpoints independently before production use. Tokens embedded in a URL should be treated as passwords.

## Proposed native adapter interface

- `list_items`: returns a provider object ID, relative destination path, exact size, provider version and optional checksums.
- `read_range`: reads a bounded byte range and provides response metadata suitable for version pinning.
- `capabilities`: declares whether ranges, checksums and immutable identifiers actually work.
- `authenticate`: provider-specific flow, using OS-protected credential storage, never a public TOML file.

This interface is currently conceptual; the implementation uses one Rust enum to dispatch adapters. Future backend refactors must maintain current checkpoint safety semantics.

Candidates: OpenDAL-based Google Drive, WebDAV, SFTP, custom download APIs, and future iCloud-specific Rust libraries. Do not accept a backend merely because it can list filenames; correctness of unique identities, variants and range integrity matters.
