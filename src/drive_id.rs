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
    let mut url = Url::parse("https://www.googleapis.com/drive/v3/files/")?;
    url.path_segments_mut().map_err(|_| anyhow::anyhow!("invalid API URL"))?
        .push(id);
    url.query_pairs_mut().append_pair("alt", "media");
    Ok(url)
}

/// A short-lived access token is an explicit local operator dependency.
/// Do not log its contents or accept permissive file permissions.
fn read_token(path: &Path) -> Result<String> {
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

/// Request an exact byte range from Google's file-ID endpoint.
/// Blob files only. Google-native Docs require a separate export adapter.
pub(crate) fn read_range(
    token_file: &Path,
    object_id: &str,
    offset: u64,
    length: u64,
    total: u64,
    rate_kib: u64,
    deadline: Duration,
    staging: &Path,
) -> Result<()> {
    ensure!(length > 0 && rate_kib > 0 && deadline > Duration::ZERO,
        "invalid transfer budget");
    let end = offset.checked_add(length).and_then(|v| v.checked_sub(1))
        .context("invalid byte range")?;
    ensure!(end < total, "byte range exceeds object metadata size");
    let token = read_token(token_file)?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(deadline)
        .build()?;
    let mut response = client.get(google_url(object_id)?)
        .bearer_auth(&token)
        .header(RANGE, format!("bytes={offset}-{end}"))
        .send().context("Google Drive range request failed")?;
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
        let count = response.read(&mut buffer[..take])?;
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

}
