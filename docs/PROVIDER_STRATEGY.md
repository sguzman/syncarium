# Provider strategy: Rust-first, not an rclone rewrite

Last reviewed: 2026-10-19. No provider migration or local account changes are authorized by this design note.

## Boundary

Syncarium is a Rust scheduler, checkpoint/receipt engine, archive auditor and source-isolation layer. The baseline `rclone` adapter shells out to the **installed Go rclone executable**. We are not reimplementing its many cloud services. The adapter uses `rclone lsjson` for source discovery and `rclone cat --offset ... --count ...` for bounded reads. The operator's existing `rclone.conf` controls provider authentication; Syncarium must never silently rewrite credentials.

The optional native Google Drive ID and Workspace adapters use Rust HTTPS for payload operations, but still use `rclone backend query` for metadata enumeration. They are deliberately opt-in, require a manually supplied short-lived bearer token, and have NOT passed real Google account QA. No automatic Google OAuth, refresh-token acquisition, or custom login server is planned.

## Rust library comparison

- [Apache OpenDAL Rust core](https://opendal.apache.org/core/) is documented as stable and production-ready, with an async-first API and 50+ storage services. This describes the **framework**, not a blanket guarantee that every provider matches rclone's functionality.
- [OpenDAL Google Drive](https://opendal.apache.org/docs/rust/opendal/services/struct.Gdrive.html) supports listing and reading files. Its documented name-based resolution selects one match where Google Drive has duplicate-named objects; this is unacceptable for preserving every immutable Drive object in Syncarium's backup. The adapter requires an access token or conventional OAuth refresh-token/client credentials; adopting it would NOT eliminate OAuth work.
- The [OpenDAL service index](https://opendal.apache.org/docs/rust/opendal/services/) currently lists Google Drive but does not list iCloud Drive/Photos. Do not assume it replaces the rclone iCloud backend.
- [rclone's provider tiers](https://rclone.org/tiers/) classify Google Drive as Tier 1/Core, but iCloud Drive and Photos as Tier 4/Experimental with flaky tests. Keep extra completeness guards for iCloud; do not call it production-complete merely because it is supplied by rclone.
- [rclone iCloud documentation](https://rclone.org/iclouddrive/) describes an account trust-token/session flow (including required manual reauthorization) and a Photos service. Photo album listing uses disk caching and can take minutes for large collections. Cache reuse is not independent evidence of enumeration completeness.

## Policy

1. Keep rclone as the default source transport, especially for iCloud. Native provider integrations must be **opt-in**.
2. Prefer existing authorized local remotes. Credential setup/reconnection stays manual; never promise permanent Apple or Google authorization.
3. Never exchange better Rust-language purity for weaker archive identity, original-file coverage, immutable publication, resumability, or global budgets.
4. Strict guards (`min_items`, `min_total_bytes`, `require_historical_coverage`) may reject suspicious inventories and preserve previously downloaded data, but **cannot** prove cloud API enumeration is complete.
5. Accept a native Rust adapter only after independent test fixtures, exact duplicate-ID preservation, stable response metadata, bounded range reads, parity checks against rclone, and an explicitly approved small live-provider QA. No automatic cutover.

## Remaining iCloud risks

The standard iCloud Photos backend is considered experimental upstream. Cache freshness, page completeness, Live Photo companion enumeration, original-quality download behavior, and range/resume semantics must be measured with a **controlled live test**, not inferred from successful local mocks. The user controls if and when that test is authorized. Until then both legacy downloaders and the existing local services remain untouched.

## Metadata subprocess hardening

Provider commands have explicit deadlines and a maximum 256 MiB inventory output ceiling. Listing parser failures, output overflows, and missing critical fields are treated as **source failures** rather than an empty or successful archive inventory. This ceiling is a safety guard, not a substitute for the separate runtime budget or a proof that listings are complete.

## Ranged-read diagnostic gate

An explicit `probe-range --source SOURCE --execute` operation can test the established rclone bridge independently of archive publication. It performs three 64 KiB overlapping reads from a provider-listed file, and compares their overlaps to detect broken offsets. It writes only disposable temporary files. The command never runs as part of routine syncing. A passed probe is a necessary diagnostic signal, **not** proof of cloud library completeness or original-quality media recovery. The Rust reader also limits stdout bytes to a reserved chunk, independent of whether rclone obeys its `--count` option. Both features are covered by offline fake-rclone tests. Live iCloud behavior remains unverified.
