//! Native, read-only Google Drive blob transfer by immutable file ID.
//! Auth is supplied as a local short-lived bearer token, never checked into Git.
use anyhow::{bail, ensure, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::Url;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn google_url(id: &str) -> Result<Url> {
    ensure!(
        !id.is_empty() && id.len() <= 512 && id.bytes().all(|byte|
            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'),
        "unexpected character in Google Drive immutable file ID"
    );
    // A trailing slash on the base path creates an empty path segment,
    // yielding /files//<id>. Build from the non-trailing path instead.
    let mut url = Url::parse("https://www.googleapis.com/drive/v3/files")?;
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("invalid API URL"))?
        .push(id);
    url.query_pairs_mut().append_pair("alt", "media");
    Ok(url)
}

/// A short-lived access token is an explicit local operator dependency.
/// Do not log its contents or accept permissive file permissions.
pub(crate) fn read_token(path: &Path) -> Result<String> {
    ensure!(path.is_absolute(), "Google access token file must be an absolute path");
    for ancestor in path.ancestors() {
        if ancestor.is_symlink() {
            bail!("Google token path contains a symlink");
        }
    }
    let metadata = fs::symlink_metadata(path).context("Google token file unavailable")?;
    ensure!(metadata.is_file() && !metadata.file_type().is_symlink(),
        "Google access token must be a regular non-symlink file");
    #[cfg(unix)]
    ensure!(metadata.permissions().mode() & 0o077 == 0,
        "Google access token permissions must deny group/other access (chmod 600)");
    ensure!(metadata.len() > 0 && metadata.len() <= 8192,
        "Google token file size is invalid");
    let value = fs::read_to_string(path).context("reading local Google OAuth access token")?;
    let token = value.trim();
    ensure!(
        !token.is_empty() && !token.chars().any(|c| c.is_whitespace() || c.is_control()),
        "invalid Google OAuth access token"
    );
    Ok(token.to_owned())
}

fn validate_range(content_range: &str, offset: u64, length: u64, total: u64)
    -> Result<()> {
    let end = offset.checked_add(length).and_then(|v| v.checked_sub(1))
        .context("invalid requested byte range")?;
    ensure!(
        content_range == format!("bytes {offset}-{end}/{total}"),
        "Google Drive response Content-Range does not match immutable object metadata"
    );
    Ok(())
}

/// Exact byte coordinates for a single immutable Google blob read.
#[derive(Clone, Copy)]
pub(crate) struct RangeSpec {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) total: u64,
}

/// Source-independent limits passed through the global transfer scheduler.
#[derive(Clone, Copy)]
pub(crate) struct TransferLimits {
    pub(crate) rate_kib: u64,
    pub(crate) deadline: Duration,
}

/// Request an exact byte range from Google's file-ID endpoint.
/// Blob files only. Google-native Docs require a separate export adapter.
pub(crate) fn read_range(
    token_file: &Path,
    object_id: &str,
    range: RangeSpec,
    limits: TransferLimits,
    staging: &Path,
) -> Result<()> {
    let token = read_token(token_file)?;
    transfer_range(&token, google_url(object_id)?, range, limits, staging)
}

/// Internal transport primitive, shared by the production Google URL and
/// a local HTTP test server. Never logs bearer tokens or writes final files.
fn transfer_range(
    token: &str,
    url: Url,
    range: RangeSpec,
    limits: TransferLimits,
    staging: &Path,
) -> Result<()> {
    let RangeSpec { offset, length, total } = range;
    let TransferLimits { rate_kib, deadline } = limits;
    ensure!(length > 0 && rate_kib > 0 && deadline > Duration::ZERO,
        "invalid transfer budget");
    let end = offset.checked_add(length).and_then(|v| v.checked_sub(1))
        .context("invalid byte range")?;
    ensure!(end < total, "byte range exceeds object metadata size");
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(deadline)
        .build()?;
    let mut response = client.get(url)
        .bearer_auth(token)
        .header(RANGE, format!("bytes={offset}-{end}"))
        // A reqwest error can contain the full request URL, including a
        // private Google object ID. Never print its Debug/Display form.
        .send().map_err(|_| anyhow::anyhow!("Google Drive range request failed"))?;
    if response.status().as_u16() != 206 {
        bail!(
            "Google Drive ID range rejected (status {}); token may be expired or server may not support ranges",
            response.status().as_u16()
        );
    }
    let header = response.headers().get(CONTENT_RANGE)
        .context("missing Google Drive Content-Range")?.to_str()?;
    validate_range(header, offset, length, total)?;

    let mut output = OpenOptions::new().write(true).truncate(true).open(staging)?;
    let started = Instant::now();
    let bytes_per_sec = rate_kib.saturating_mul(1024);
    let mut saved = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    while saved < length {
        ensure!(started.elapsed() < deadline, "Google Drive transfer exceeded deadline");
        let take = (length - saved).min(buffer.len() as u64) as usize;
        let count = response.read(&mut buffer[..take])
            .map_err(|_| anyhow::anyhow!("Google Drive response read failed"))?;
        ensure!(count > 0, "Google Drive range ended early");
        output.write_all(&buffer[..count])?;
        saved += count as u64;
        let required = Duration::from_secs_f64(saved as f64 / bytes_per_sec as f64);
        if required > started.elapsed() {
            thread::sleep(required - started.elapsed());
        }
    }
    output.flush()?;
    output.sync_all()?;
    ensure!(File::open(staging)?.metadata()?.len() == length,
        "Google Drive downloaded byte count mismatch");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_endpoint_has_immutable_id_and_no_extra_path() {
        let url = google_url("test_FILE-123").unwrap();
        assert_eq!(url.as_str(), "https://www.googleapis.com/drive/v3/files/test_FILE-123?alt=media");
        assert!(google_url("../evil").is_err());
        assert!(google_url("a?alt=media").is_err());
        assert!(google_url("some id").is_err());
    }

    #[test]
    fn range_must_match_start_end_and_total() {
        assert!(validate_range("bytes 100-199/500", 100, 100, 500).is_ok());
        assert!(validate_range("bytes 100-199/900", 100, 100, 500).is_err());
        assert!(validate_range("bytes 0-99/500", 100, 100, 500).is_err());
        assert!(validate_range("bytes 100-200/500", 100, 100, 500).is_err());
        assert!(validate_range("bytes 0-99/*", 0, 100, 500).is_err());
    }

    #[test]
    fn secret_token_file_requires_private_permissions() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("token");
        fs::write(&path, "test-secret\n").unwrap();
        #[cfg(unix)]
        {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_token(&path).is_err());
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(read_token(&path).unwrap(), "test-secret");
        fs::write(&path, "embedded spaces").unwrap();
        assert!(read_token(&path).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn reject_token_file_through_symlink_parent() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let actual = temp.path().join("private");
        fs::create_dir(&actual).unwrap();
        let secret = actual.join("token");
        fs::write(&secret, b"token-content").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let redirect = temp.path().join("link");
        symlink(&actual, &redirect).unwrap();
        assert!(read_token(&redirect.join("token")).is_err());
        assert_eq!(read_token(&secret).unwrap(), "token-content");
    }

    /// Exercise real streaming and header checks using an isolated local HTTP
    /// responder. These tests never contact Google or read actual user tokens.
    #[test]
    fn native_http_byte_ranges_are_authenticated_and_exact() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        fn scenario(status: &str, content_range: &str, body: &'static [u8])
            -> (String, Vec<u8>, String)
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let status_line = status.to_owned();
            let range_header = content_range.to_owned();
            let (tx, rx) = mpsc::sync_channel::<String>(1);
            let server = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut bytes = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut bytes).unwrap();
                    if n == 0 { break; }
                    request.extend_from_slice(&bytes[..n]);
                    assert!(request.len() < 16 * 1024);
                }
                tx.send(String::from_utf8_lossy(&request).to_ascii_lowercase()).unwrap();
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Range: {range_header}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(response.as_bytes()).unwrap();
                socket.write_all(body).unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let staging = dir.path().join("chunk");
            File::create(&staging).unwrap();
            let success = transfer_range(
                "fake-test-token",
                Url::parse(&format!("http://{address}/file")).unwrap(),
                RangeSpec { offset: 2, length: 3, total: 10 },
                TransferLimits { rate_kib: 10240, deadline: Duration::from_secs(5) },
                &staging
            );
            let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            server.join().unwrap();
            (format!("{success:?}"), fs::read(&staging).unwrap(), request)
        }

        let (ok, data, req) = scenario("206 Partial Content", "bytes 2-4/10", b"cde");
        assert!(ok.starts_with("Ok("), "{ok}");
        assert_eq!(data, b"cde");
        assert!(req.contains("range: bytes=2-4"));
        assert!(req.contains("authorization: bearer fake-test-token"));

        let (bad, data, _) = scenario("206 Partial Content", "bytes 2-4/11", b"cde");
        assert!(bad.starts_with("Err("));
        assert!(data.is_empty(), "incorrect total must never be staged");

        let (redirect, data, _) = scenario("302 Found", "bytes 2-4/10", b"cde");
        assert!(redirect.starts_with("Err("));
        assert!(data.is_empty(), "redirects must not be followed or committed");
    }

}
