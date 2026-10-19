//! Google Workspace export planning and strictly bounded whole-file reader.
//!
//! Workspace exports are generally NOT resumable byte-range blobs. The
//! planner is read-only. The whole-file export adapter is opt-in and must
//! verify source revision metadata before AND after the export.
use anyhow::{ensure, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// Per Google Drive API, files.export output is capped at 10 MB.
pub(crate) const GOOGLE_EXPORT_MAX_BYTES: u64 = 10_000_000;
const NATIVE_PREFIX: &str = "application/vnd.google-apps.";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceObject {
    id: String,
    name: String,
    mime_type: String,
    #[serde(default)]
    modified_time: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ExportCandidate {
    /// Never serialized or written into public repo/logs.
    #[serde(skip_serializing)]
    pub(crate) object_id: String,
    #[serde(skip_serializing)]
    pub(crate) source_version: Option<String>,
    /// This indicates metadata presence, not an immutable snapshot.
    pub(crate) source_version_known: bool,
    pub(crate) object_fingerprint: String,
    pub(crate) relative_destination: String,
    pub(crate) source_mime: String,
    pub(crate) export_mime: &'static str,
    /// No exported size/MD5 is known until the actual export.
    pub(crate) size_known_before_export: bool,
}

impl ExportCandidate {
    pub(crate) fn version_key(&self) -> &str {
        self.source_version.as_deref().unwrap_or("")
    }
}

#[derive(Serialize)]
pub(crate) struct ExportPlan {
    pub(crate) supported: Vec<ExportCandidate>,
    pub(crate) unsupported_google_types: usize,
    pub(crate) ignored_non_workspace: usize,
    warning: &'static str,
}

fn format_for_type(mime: &str) -> Option<(&'static str, &'static str)> {
    match mime {
        "application/vnd.google-apps.document" =>
            Some(("application/vnd.openxmlformats-officedocument.wordprocessingml.document", ".docx")),
        "application/vnd.google-apps.spreadsheet" =>
            Some(("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", ".xlsx")),
        "application/vnd.google-apps.presentation" =>
            Some(("application/vnd.openxmlformats-officedocument.presentationml.presentation", ".pptx")),
        "application/vnd.google-apps.drawing" =>
            Some(("image/svg+xml", ".svg")),
        _ => None,
    }
}

pub(crate) fn plan(raw: &[u8]) -> Result<ExportPlan> {
    let objects: Vec<WorkspaceObject> = serde_json::from_slice(raw)
        .context("invalid Google immutable-ID inventory JSON")?;
    ensure!(!objects.is_empty(), "empty Google object inventory is not a complete backup");
    let mut seen = HashSet::new();
    let mut supported = Vec::new();
    let mut unsupported = 0usize;
    let mut ignored = 0usize;
    for obj in objects {
        ensure!(!obj.id.is_empty() && !obj.name.is_empty() && !obj.mime_type.is_empty(),
            "Google object lacks required identity information");
        ensure!(seen.insert(obj.id.clone()),
            "duplicate Google object ID; refusing incomplete workspace plan");
        if let Some((mime, extension)) = format_for_type(&obj.mime_type) {
            let hash = hex::encode(Sha256::digest(obj.id.as_bytes()));
            // Version-qualified publication retains prior Google revisions;
            // the inventory still tracks the stable object ID separately.
            let mut revision = Sha256::new();
            revision.update(obj.id.as_bytes());
            revision.update([0]);
            revision.update(obj.modified_time.as_deref().unwrap_or("").as_bytes());
            let revision_hash = hex::encode(revision.finalize());
            supported.push(ExportCandidate {
                object_id: obj.id,
                source_version_known: obj.modified_time.is_some(),
                source_version: obj.modified_time,
                object_fingerprint: hash.clone(),
                relative_destination: format!(
                    "workspace-exports/{}/id-{hash}-rev-{}{extension}",
                    &hash[..2], &revision_hash[..16]
                ),
                source_mime: obj.mime_type,
                export_mime: mime,
                size_known_before_export: false,
            });
        } else if obj.mime_type.starts_with(NATIVE_PREFIX)
            && obj.mime_type != "application/vnd.google-apps.folder"
            && obj.mime_type != "application/vnd.google-apps.shortcut"
        {
            unsupported += 1;
        } else {
            ignored += 1;
        }
    }
    supported.sort_by(|a, b| a.object_fingerprint.cmp(&b.object_fingerprint));
    Ok(ExportPlan {
        supported, unsupported_google_types: unsupported, ignored_non_workspace: ignored,
        warning: "Read-only plan. Export sizes are unknown until response headers arrive. Google files.export is limited to 10 MB and does not guarantee resumable byte ranges. No files were downloaded or certified.",
    })
}

fn export_url(id: &str, export_mime: &str) -> Result<Url> {
    ensure!(!id.is_empty() && id.len() <= 512
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'),
        "invalid immutable Google file ID");
    let mut url = Url::parse("https://www.googleapis.com/drive/v3/files")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Google API path invalid"))?
        .push(id).push("export");
    url.query_pairs_mut().append_pair("mimeType", export_mime);
    Ok(url)
}

#[derive(Clone, Copy)]
pub(crate) struct ExportLimits {
    pub(crate) max_bytes: u64,
    pub(crate) rate_kib: u64,
    pub(crate) deadline: Duration,
}

/// A manually supplied short-lived token is required; never renewed here.
/// Re-check source MIME and modification metadata around the export.
pub(crate) fn export_small(
    token_file: &Path,
    item: &ExportCandidate,
    staging: &Path,
    limits: ExportLimits,
) -> Result<u64> {
    ensure!(!item.version_key().is_empty(),
        "cannot export an item without source modification metadata");
    let token = crate::drive_id::read_token(token_file)?;
    let started = Instant::now();
    assert_current_revision(&token, item, limits.deadline.min(Duration::from_secs(10)))?;
    let time_left = limits.deadline.saturating_sub(started.elapsed());
    ensure!(time_left > Duration::from_secs(2),
        "no time remaining for bounded Workspace export");
    let length = stream_export(
        &token, export_url(&item.object_id, item.export_mime)?,
        item.export_mime, staging,
        ExportLimits { deadline: time_left.saturating_sub(Duration::from_secs(1)), ..limits },
    )?;
    // An export from a mutable Google Doc is acceptable only when its source
    // metadata still agrees after the response has been completely staged.
    let time_left = limits.deadline.saturating_sub(started.elapsed());
    assert_current_revision(&token, item, time_left.min(Duration::from_secs(10)))?;
    Ok(length)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentVersion {
    modified_time: Option<String>,
    mime_type: Option<String>,
}

fn matches_expected_revision(
    raw: &[u8],
    expected_mime: &str,
    expected_modified: &str,
) -> Result<()> {
    let current: CurrentVersion = serde_json::from_slice(raw)?;
    ensure!(current.mime_type.as_deref() == Some(expected_mime),
        "Google Workspace source MIME type changed during export");
    ensure!(current.modified_time.as_deref() == Some(expected_modified),
        "Google Workspace document changed during export");
    Ok(())
}

/// Small, fixed-field metadata GET. Only the strict request's success/failure
/// escapes: never print HTTP error objects that may contain private Google IDs.
fn assert_current_revision(
    token: &str,
    item: &ExportCandidate,
    deadline: Duration,
) -> Result<()> {
    ensure!(deadline > Duration::ZERO, "Workspace metadata check timed out");
    let mut url = Url::parse("https://www.googleapis.com/drive/v3/files")?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid Drive metadata path"))?
        .push(&item.object_id);
    url.query_pairs_mut().append_pair("fields", "modifiedTime,mimeType");
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(deadline)
        .build()?;
    let response = client.get(url).bearer_auth(token)
        .send().map_err(|_| anyhow::anyhow!("Google Workspace metadata check failed"))?;
    ensure!(response.status().as_u16() == 200,
        "Google Workspace source revision unavailable (HTTP {})",
        response.status().as_u16());
    // Limit even malformed metadata responses to a few KiB.
    let mut limited = response.take(16 * 1024 + 1);
    let mut body = Vec::new();
    limited.read_to_end(&mut body)
        .map_err(|_| anyhow::anyhow!("Google Workspace metadata could not be read"))?;
    ensure!(body.len() <= 16 * 1024,
        "unexpectedly large Google Workspace metadata response");
    matches_expected_revision(&body, &item.source_mime, item.version_key())
}

/// Guard whole-file exports against overrunning a source-independent budget.
/// No retry/range resume is attempted for these non-ranged byte streams.
fn stream_export(
    token: &str,
    url: Url,
    expected_mime: &str,
    staging: &Path,
    limits: ExportLimits,
) -> Result<u64> {
    ensure!(limits.max_bytes > 0 && limits.rate_kib > 0
        && limits.deadline > Duration::ZERO, "invalid export limits");
    let cap = limits.max_bytes.min(GOOGLE_EXPORT_MAX_BYTES);
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(limits.deadline)
        .build()?;
    let mut response = client.get(url).bearer_auth(token)
        .header(ACCEPT_ENCODING, "identity")
        .send().map_err(|_| anyhow::anyhow!("Google Workspace export request failed"))?;
    ensure!(response.status().as_u16() == 200,
        "Google Workspace export did not return HTTP 200; authentication or format may be invalid");
    // Without Content-Length there is no defensible strict payload budget.
    let length: u64 = response.headers().get(CONTENT_LENGTH)
        .context("Workspace export has no Content-Length; refusing unknown-size body")?
        .to_str()?.parse().context("invalid Workspace export Content-Length")?;
    ensure!(length > 0 && length <= cap,
        "Workspace export exceeds single-pass budget or exceeds 10 MB maximum");
    let media_type = response.headers().get(CONTENT_TYPE)
        .context("Workspace export omitted Content-Type")?
        .to_str()?.split(';').next().unwrap_or("").trim();
    ensure!(media_type == expected_mime,
        "Workspace export format differs from requested MIME type");
    crate::no_symlink_ancestors(staging)?;
    let mut output = OpenOptions::new().write(true).truncate(true).open(staging)?;
    let mut left = length;
    let started = Instant::now();
    let mut buffer = [0u8; 16 * 1024];
    while left > 0 {
        ensure!(started.elapsed() < limits.deadline, "Workspace export deadline exceeded");
        let count = (left as usize).min(buffer.len());
        let n = response.read(&mut buffer[..count])
            .map_err(|_| anyhow::anyhow!("Workspace export response read failed"))?;
        ensure!(n > 0, "Workspace export ended before declared Content-Length");
        output.write_all(&buffer[..n])?;
        left -= n as u64;
        let received = length - left;
        let required = Duration::from_secs_f64(
            received as f64 / (limits.rate_kib.saturating_mul(1024)) as f64
        );
        if required > started.elapsed() {
            thread::sleep(required - started.elapsed());
        }
    }
    output.sync_all()?;
    ensure!(fs::metadata(staging)?.len() == length, "Workspace export length mismatch");
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn export_plan_is_id_stable_and_omits_private_names() {
        let raw = serde_json::json!([
            {"id":"secret-doc-123","name":"private-personal","mimeType":"application/vnd.google-apps.document","modifiedTime":"2026-10-01T00:00:00Z"},
            {"id":"secret-sheet-123","name":"taxes","mimeType":"application/vnd.google-apps.spreadsheet"},
            {"id":"secret-slide-123","name":"slides","mimeType":"application/vnd.google-apps.presentation"},
            {"id":"secret-draw-123","name":"art","mimeType":"application/vnd.google-apps.drawing"},
            {"id":"unhandled-form","name":"form","mimeType":"application/vnd.google-apps.form"},
            {"id":"normal","name":"normal","mimeType":"application/octet-stream"},
            {"id":"folder","name":"folder","mimeType":"application/vnd.google-apps.folder"}
        ]);
        let plan = plan(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(plan.supported.len(), 4);
        assert_eq!(plan.unsupported_google_types, 1);
        assert_eq!(plan.ignored_non_workspace, 2);
        let printed = serde_json::to_string(&plan).unwrap();
        assert!(!printed.contains("private-personal"));
        assert!(!printed.contains("secret-doc-123"));
        assert!(!printed.contains("2026-10-01"));
        let plan2 = super::plan(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(serde_json::to_string(&plan).unwrap(), serde_json::to_string(&plan2).unwrap());
        assert!(plan.supported.iter().all(|item|
            item.relative_destination.starts_with("workspace-exports/")));
        assert!(export_url("valid-ID", plan.supported[0].export_mime).is_ok());
        assert!(export_url("../traversal", plan.supported[0].export_mime).is_err());
    }

    #[test]
    fn rejected_incomplete_and_duplicate_inventories() {
        assert!(plan(b"[]").is_err());
        let raw = br#"[
            {"id":"same","name":"a","mimeType":"application/vnd.google-apps.document"},
            {"id":"same","name":"b","mimeType":"application/vnd.google-apps.spreadsheet"}
        ]"#;
        assert!(plan(raw).is_err());
    }

    fn serve(status: &str, media_type: &str, declared: Option<usize>, body: &[u8])
        -> (Result<u64>, Vec<u8>, String)
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_owned();
        let mime = media_type.to_owned();
        let content = body.to_vec();
        let (sender, receiver) = mpsc::sync_channel::<String>(1);
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|x| x == b"\r\n\r\n") {
                let n = socket.read(&mut buf).unwrap();
                if n == 0 { break; }
                request.extend_from_slice(&buf[..n]);
                assert!(request.len() < 16 * 1024);
            }
            sender.send(String::from_utf8_lossy(&request).to_ascii_lowercase()).unwrap();
            let mut reply = format!("HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nConnection: close\r\n");
            if let Some(len) = declared {
                reply.push_str(&format!("Content-Length: {len}\r\n"));
            } else {
                reply.push_str("Transfer-Encoding: chunked\r\n");
            }
            reply.push_str("\r\n");
            socket.write_all(reply.as_bytes()).unwrap();
            if declared.is_some() {
                socket.write_all(&content).unwrap();
            } else {
                socket.write_all(format!("{:X}\r\n", content.len()).as_bytes()).unwrap();
                socket.write_all(&content).unwrap();
                socket.write_all(b"\r\n0\r\n\r\n").unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("stage");
        fs::write(&target, b"").unwrap();
        let mime = format_for_type("application/vnd.google-apps.document").unwrap().0;
        let result = stream_export(
            "offline-fake-token",
            Url::parse(&format!("http://{addr}/export")).unwrap(),
            mime, &target,
            ExportLimits { max_bytes: 10, rate_kib: 10240, deadline: Duration::from_secs(5) },
        );
        let request = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        server.join().unwrap();
        (result, fs::read(&target).unwrap(), request)
    }

    #[test]
    fn only_known_length_correct_mime_and_within_budget_can_stage() {
        let mime = format_for_type("application/vnd.google-apps.document").unwrap().0;
        let (ok, staged, request) = serve("200 OK", mime, Some(6), b"sample");
        assert_eq!(ok.unwrap(), 6);
        assert_eq!(staged, b"sample");
        assert!(request.contains("authorization: bearer offline-fake-token"));

        let (too_large, staged, _) = serve("200 OK", mime, Some(11), b"12345678901");
        assert!(too_large.is_err());
        assert!(staged.is_empty());

        let (unknown, staged, _) = serve("200 OK", mime, None, b"hello");
        assert!(unknown.is_err());
        assert!(staged.is_empty());

        let (bad_type, staged, _) = serve("200 OK", "text/html", Some(6), b"sample");
        assert!(bad_type.is_err());
        assert!(staged.is_empty());

        let (redirect, staged, _) = serve("302 Found", mime, Some(6), b"sample");
        assert!(redirect.is_err());
        assert!(staged.is_empty());
    }
    #[test]
    fn metadata_checks_reject_mutable_or_incomplete_workspace_objects() {
        let ok = br#"{"modifiedTime":"2026-10-19T00:00:00.000Z","mimeType":"application/vnd.google-apps.document"}"#;
        assert!(matches_expected_revision(
            ok, "application/vnd.google-apps.document", "2026-10-19T00:00:00.000Z"
        ).is_ok());
        assert!(matches_expected_revision(
            ok, "application/vnd.google-apps.spreadsheet", "2026-10-19T00:00:00.000Z"
        ).is_err());
        assert!(matches_expected_revision(
            ok, "application/vnd.google-apps.document", "2026-10-18T00:00:00.000Z"
        ).is_err());
        assert!(matches_expected_revision(
            br#"{"mimeType":"application/vnd.google-apps.document"}"#,
            "application/vnd.google-apps.document", "2026-10-19T00:00:00.000Z"
        ).is_err());
    }

    #[test]
    fn new_document_revision_has_distinct_archive_path_without_exposing_time() {
        fn single(time: &str) -> ExportPlan {
            let content = serde_json::json!([{
                "id":"private-google-object-id",
                "name":"family-budget",
                "mimeType":"application/vnd.google-apps.spreadsheet",
                "modifiedTime":time
            }]);
            plan(&serde_json::to_vec(&content).unwrap()).unwrap()
        }
        let old = single("2026-10-01T00:00:00Z");
        let edited = single("2026-10-19T00:00:00Z");
        assert_eq!(old.supported[0].object_fingerprint,
                   edited.supported[0].object_fingerprint);
        assert_ne!(old.supported[0].relative_destination,
                   edited.supported[0].relative_destination);
        for item in [&old.supported[0], &edited.supported[0]] {
            assert!(!item.relative_destination.contains("family-budget"));
            assert!(!item.relative_destination.contains("2026"));
            assert!(item.relative_destination.ends_with(".xlsx"));
        }
    }

}
