# Explicit Apple-style photo asset/resource evidence

## Purpose and limits

Ordinary rclone iCloud Photos `lsjson` inventories contain relative media paths, sizes, and sometimes provider metadata, but they do **not** expose the PhotoKit resource relationship necessary to prove that a `.HEIC` and `.MOV` file belong to the same Live Photo. Same-folder or same-stem filename matching is only a heuristic. Apple PhotoKit's `PHAssetResourceType.pairedVideo` identifies the **original** Live Photo video; `fullSizePairedVideo` represents a current edited version and is not interchangeable with the original. The original still resource has type `photo` (the current edited still can be `fullSizePhoto`).

Apple reference documentation:
- [pairedVideo](https://developer.apple.com/documentation/photos/phassetresourcetype/pairedvideo)
- [fullSizePairedVideo](https://developer.apple.com/documentation/photos/phassetresourcetype/fullsizepairedvideo)
- [PHAssetResource](https://developer.apple.com/documentation/photos/phassetresource)
- [rclone iCloud Photos backend](https://rclone.org/iclouddrive/)

Syncarium's `photos-asset-evidence` command performs **strictly offline** cross-referencing of a private, explicitly supplied asset/resource map with a saved rclone listing. It does not itself acquire PhotoKit objects or infer relationships from file names. This format is **not** raw rclone output, not an Apple-issued signature, and not a native Apple JSON export. A real mapping must come from independently captured asset/resource metadata with stable identifiers and corresponding output paths; that capture path has **not** been implemented or validated. No Mac, Apple login, OAuth app, local daemon, or hardware setup is required merely to run this diagnostic on Linux, but without genuine external mapping data it cannot make new claims about your library.

## Private normalized input

Both required files must already exist locally. Input one is a private JSON evidence file using schema version 1:

```json
{
  "schema_version": 1,
  "scope": "sample",
  "assets": [
    {
      "asset_id": "EXAMPLE-not-an-actual-Apple-ID",
      "kind": "live_photo",
      "resources": [
        {
          "resource_type": "photo",
          "path": "album/sample.heic",
          "size": 1024
        },
        {
          "resource_type": "paired_video",
          "path": "album/sample.mov",
          "size": 2048
        }
      ]
    }
  ]
}
```

The second file is a separately saved `rclone lsjson --recursive --files-only` JSON array, scoped to exactly the same relative root as the resource paths. Asset kinds: `photo`, `video`, `live_photo`, `other`. Resource names are snake_case PhotoKit-like types such as `photo`, `paired_video`, `full_size_photo`, `full_size_paired_video`, `alternate_photo`, and `adjustment_data`. Resource `path` and `size` may be null only when **unknown**; unknown relationships are counted as unresolved, not accepted as successfully archived. Unknown future resource role names are counted as unsupported evidence, not silently classified as original videos.

The `scope` is either `sample` (only explicitly mapped source assets are examined; unrelated rclone files are counted but don't by themselves fail the command) or `claimed_complete` (a **user-supplied claim** that every file in this saved rclone inventory has a corresponding asset resource mapping). Any unmapped listing entries in claimed-complete mode cause failure. **Even successful claimed-complete mode cannot establish that the provider listed every asset that exists in Apple's library**: the claim and the listing have not been authenticated to Apple's source database. Do not manufacture an evidence manifest from coincidental file stems.

## Running entirely offline

```text
syncarium --config ~/.config/syncarium/config.toml photos-asset-evidence \
  --source icloud-photos \
  --evidence /absolute/private/asset-resources.json \
  --listing /absolute/private/icloud-lsjson.json \
  --json
```

The command reads only its two local JSON inputs, even when the archive mount is absent. It never contacts rclone, Apple, Google, network services, or the local archive. It never changes cloud credentials, source services, archive content, partial checkpoints, published receipts or inventory ledgers. Its output contains **aggregated counts only** and never prints source IDs, filenames, full paths, or per-asset relationships. The input data **does** contain private identifiers: keep it out of Git and public issue reports.

The summary differentiates original role coverage from actual presence in the saved listing, counts resources missing from the listing, size conflicts, unassigned resource paths, unknown types, repeated original roles, and unmapped files. Different file names can be paired only when an explicit asset ID maps them; matching file stems by themselves are not accepted. Original `paired_video` is required to count an original Live Photo video even if `full_size_paired_video` exists. Bad/duplicate/traversing paths or duplicate asset IDs are rejected. Missing required evidence causes nonzero exit; a good sample can exit successfully but does not prove complete Apple coverage or file content integrity.

## Future native evidence collection

An independent source of asset IDs and resource relationships would make this command significantly more useful. A future Linux-only option could inspect embedded content identifiers in local media, but content identifiers and sidecar metadata are not yet implemented, and their reliability would need independent testing. Do not require the user to purchase a Mac or create a new Apple authentication flow to use Syncarium. Keep existing archival mechanisms running until validated parity is established.
