# Architecture and invariants

## Topology

```text
systemd user timer (hourly, disabled until manually enabled)
    → Syncarium (Rust, single process, exclusive lock)
        → TOML source registry
        → adapters: native HTTPS or optional rclone CLI
        → interleaved round-robin, one chunk per source at a time
        → /drive/.syncarium/partials/<source>/<hash-prefix>/
        → atomic, no-clobber hard link to each destination
```

An adapter must provide (1) a complete list of source items and version/size, and (2) an exact byte-range reader. It must never be granted a write API against the remote. Native HTTPS works for manifest-defined objects with functioning `Range` / `206` support. The compatibility adapter uses `rclone lsjson` and `rclone cat --offset --count`. Rust orchestrates all transfer and state logic.

## Explicit safety invariants

1. No upload, reverse synchronization, remote mutation or delete operation exists in the runtime.
2. Existing destination files are never overwritten. Once archived, a source disappearing does not remove its local copy.
3. Fail closed when the configured external archive mount disappears or becomes read-only. Fail closed on symlink ancestors and traversal paths.
4. A file is published **only when its advertised length has been downloaded**; publish uses a no-clobber hard link. Staging state and files live on the destination filesystem.
5. Exactly **one active transfer** inside a Syncarium pass. The process holds an exclusive local lock; the two old projects are external and cannot be locked by this process.
6. Every attempted range decrements the single global **payload budget**. API listing overhead, HTTP headers, retried chunks, and authentication requests are outside the exact cap.
7. Remote size or source-version changes cause the staged file to be held for inspection rather than silently concatenated. The version identifier is only as strong as the backend metadata.
8. Global scheduling alternates sources after each chunk, rotating the first eligible source on each new run. Full weighted fairness is not yet available.
9. Failure of one provider listing, source download, or partial-file checkpoint is isolated from other healthy sources. A failed item can be bypassed within the same source, with a three-consecutive-failure circuit breaker. The pass exits nonzero if any failure occurred, while retaining completed downloads from other sources.

## Checkpoint protocol

Each source-item path is mapped to a SHA-256-keyed pair of files: `.part` and `.json`. The JSON stores advertised total size, source version, committed prefix length, and the SHA-256 of the last completed chunk. A request downloads a range into a separate temporary file. After exact byte-length validation:

1. Append its bytes to `.part`.
2. Flush/sync the data.
3. Atomically update the checkpoint manifest.
4. On restart, discard only trailing bytes after the last committed checkpoint.
5. Verify the recorded last chunk. Resume the next offset.
6. At exactly total size, hard-link the partial to the final destination without replacing an existing file. Only then remove staging.

A failure can leave an orphaned partial if it occurs at an unlucky step. Fail closed and report the orphan; do not silently discard earlier progress. The current implementation verifies the **last saved chunk** during resumption. Native HTTP manifests may also supply an optional **SHA-256 of the entire file**, which is validated before publication. Rclone-backed objects without trusted provider hashes cannot yet be verified cryptographically end-to-end. Adding full independent hash verification and append-only provenance manifests is roadmap work.

## Current known deficits

- Google Drive duplicate-named objects, special Google Docs exports and dangling shortcuts need ID-aware adapters; ordinary rclone path listings cannot guarantee completeness.
- Apple Photos album hierarchy, Live Photo paired-media handling, shared library coverage, source listing completeness and range support remain to be verified.
- A source changing while the current 8 MiB ranged request is being served cannot always be detected without stable ETags or provider-specific immutable IDs.
- HTTP source URL integrity depends on accurate local manifest versions; only HTTPS URLs are accepted.
- This early implementation scans enabled source inventories each run. For very large cloud libraries it may burn time on listings. Persisted efficient inventories and per-source cursors are future work.
- POSIX symlink checks are best-effort against concurrent malicious filesystem mutation. Do not execute Syncarium on directories writable by untrusted local users.
- Runtime version, state schema and migration still need semantic versioning and compatibility controls.

## Why not simply use rclone sync?

`rclone sync` propagates remote deletions; archive safety explicitly forbids that. `rclone copy` is one-way, but traditional interrupted transfers do not automatically guarantee byte-accurate cross-invocation chunk checkpoints for all backends. Syncarium therefore wraps read-only ranged transfers with a source-independent checkpoint protocol.

## Long-term backend roadmap

Native Rust HTTP is implemented. Apache OpenDAL has a Rust Google Drive service, but authentication and duplicate-file IDs must be handled correctly before declaring feature equivalence. An older OpenDAL iCloud Drive backend is not proof of current iCloud Photos support. Use a separately-tested native adapter when its completeness and security match or exceed the bridge.

## Read-only coverage audit

The `audit` subcommand inventories every enabled source and compares path and file **size** without transferring payload. It reports present-size-matched, local conflicts, staged, missing, and inaccessible/held items separately. Counts reflect only what the source adapter lists, which can be incomplete (especially Apple Photos). Matching size is **not** proof of matching bytes or successful original-quality export. Results can be obtained as structured JSON for future UIs; the audit never deletes or repairs content.

## Time budget and source enumeration

The overall `run_seconds` deadline starts **before** provider listing, not after it. Each rclone inventory query is capped by the lesser of 180 seconds or the remaining pass time. An unlisted source is postponed and reported as an issue if the pass is out of time. This avoids a long list of slow cloud sources silently extending a supposedly bounded hourly pass. The process still has normal filesystem and process-shutdown overhead; the systemd service is an additional upper bound.

## Append-only source observation ledger

Successful **executing** passes keep source-path SHA-256 fingerprints under `/drive/.syncarium/inventories/<source>.json`. They are derived from the source name and remote relative path; filenames and credentials are not stored in the ledger. Newly observed paths extend the set. Previously observed but currently missing paths remain recorded, trigger a coverage warning and make the overall pass return nonzero. No source or local files are deleted. Dry-run `plan` and `audit` only read this ledger. A first listing is a **provisional baseline**, not proof of full cloud coverage; upstream APIs may omit unseen photos or duplicate Google Drive objects. Path renames look like a disappearance plus an addition until resolved manually. Do not treat an absent source item as authorization to delete its local archive.

## Google Drive path collisions

The `drive-collisions` command compares immutable Drive object IDs for siblings that map to the same output filename, considering common Google-native Docs export extensions. It also walks descendants of same-named folders. A path-based rclone listing alone cannot prove these objects are present. The command is explicit and read-only; no automatic ID-based recovery is claimed.

## ID recovery planning (not transfers)

The read-only `drive-recovery-plan` CLI selects ambiguous Google file objects by their immutable IDs rather than any path-based rclone resolution. It detects sibling file-output collisions, duplicate folders, file-versus-folder collisions, and all descendant files reachable from ambiguous folders. A SHA-256 fingerprint of each Google file ID generates a deterministic destination under `.rclone-duplicates/objects/`. Original names are excluded from filesystem path elements. Unknown file sizes and unsupported Google types are explicitly counted. This plan is **not** a complete recovery implementation: `rclone backend copyid` fetches a whole object, potentially violating the global byte limit for large files. We must build and validate a resumable, ID-addressed reader before enabling execution.

## HTTPS range exactness

Native HTTP adapter requests explicitly bounded byte ranges and requires a `206 Partial Content` response whose `Content-Range` exactly matches **both the requested start/end and the original source's declared total size**. HTTP redirects are disabled to prevent unexpected endpoint changes or loss of source identity between chunks. Range mismatches are rejected before staging bytes are committed, preserving existing checkpoints. HTTPS manifests must point directly to range-capable final asset URLs.

## Opt-in native Google blob ID downloads

The experimental `google_drive_ids` adapter calls Google's file content GET endpoint by immutable ID with `alt=media` and a bounded `Range` header, using Rust HTTPS. Google object IDs and provider MD5/size/version metadata come from a read-only rclone `backend query` listing; access tokens come only from an owner-private local token file. Only known-size, provider-MD5-backed binary collision candidates are transferred. Google Docs exports and absent checksums are excluded, not silently claimed complete. Publication checks the **entire local file MD5** against the provider hash before atomic no-clobber linking. Network transfers share the scheduler's single global byte and time limits and its cross-run checkpoints. The token is short-lived and not renewed yet; Google's real endpoint and cross-version consistency still require end-to-end live QA.

### Provider completeness signaling

For the native Google immutable-ID source, listing returns both eligible byte-range items and an explicit unresolved count. The scheduler makes progress on eligible items while reporting a nonzero incomplete-run status when unsupported Google Docs, unknown sizes or missing provider checksums make full ID recovery impossible. The read-only audit exposes the count independently. This is not equivalent to end-to-end completeness: inaccessible or never-listed items remain outside the provider inventory.

## Authentication boundary (deliberately outside Syncarium)

The scheduler must not manage Google OAuth registration or user-consent workflows. It receives read-only authorization through previously configured external remotes or an explicitly supplied, owner-private local access-token file. **No custom OAuth login, token exchange, refresh-token persistence, automatic renewal, or unattended reauthentication is planned.** If an external backend can refresh its *own* token, that behavior belongs to the backend, and Syncarium must still handle its failure.

The native Google ID adapter's short-lived bearer token is a **manual-session credential**, not viable persistent unattended authentication by itself. Keep the adapter disabled under the recurring timer until separate local authorization handling and live transfer behavior are validated. Authentication errors must preserve staged bytes, be actionable, and never block other healthy sources from making progress.
