// SPDX-License-Identifier: GPL-3.0-only
//! HTTP downloader with Range-resume, SHA256 verification, and `indicatif`
//! progress UI. Supports a mirror override and first-run downloads.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

/// Running total of payload bytes written to disk by every download in this
/// process.
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Payload bytes written to disk by all downloads so far in this process.
///
/// Exists so a caller can estimate the *live* transfer rate — sample this
/// twice a couple of seconds apart and divide by the interval — and from
/// that derive a "done in about N minutes" figure for a desktop
/// notification, without threading a progress callback through every model
/// fetch in the workspace. Monotonic; resumed downloads count only the
/// bytes this process actually transferred.
#[must_use]
pub fn bytes_written() -> u64 {
    BYTES_WRITTEN.load(Ordering::Relaxed)
}

/// Download `url` to `dest`, resuming if a partial file exists. Verifies the
/// final file against `expected_sha256` (hex lowercase, 64 chars). A hash
/// starting with `"0000..."` is treated as "unpinned" and only logged.
pub async fn download(url: &str, dest: &Path, expected_sha256: &str) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    // Stream into a sibling `.part` file and only rename into `dest` once the
    // content is fully written and SHA-verified. This guarantees `dest` never
    // exists as a truncated/corrupt file after an interrupted download or a
    // killed process — callers gate on a plain `dest.exists()`, so a partial
    // file left at `dest` would otherwise be trusted forever and fail to load.
    let part = part_path(dest);
    let pinned = !expected_sha256.chars().all(|c| c == '0');

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match try_download(url, &part).await {
            Ok(()) => {
                let actual = sha256_file(&part).await?;
                if !pinned {
                    info!("downloaded {dest:?}: sha256={actual} (unpinned)");
                    break;
                } else if actual.eq_ignore_ascii_case(expected_sha256) {
                    info!("downloaded {dest:?}: sha256 verified");
                    break;
                } else if attempt < 3 {
                    warn!(
                        "sha256 mismatch for {dest:?} (attempt {attempt}): expected \
                         {expected_sha256}, got {actual}; re-downloading"
                    );
                    // Complete but corrupt: resuming can't repair it, so start over.
                    tokio::fs::remove_file(&part).await.ok();
                    tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
                } else {
                    tokio::fs::remove_file(&part).await.ok();
                    return Err(anyhow!(
                        "sha256 mismatch for {dest:?}: expected {expected_sha256}, got {actual}"
                    ));
                }
            }
            Err(e) if attempt < 3 => {
                warn!("download attempt {attempt} failed: {e}; retrying");
                // Keep the partial file so the next attempt resumes via Range.
                tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }

    // Atomically publish the verified file. Same directory ⇒ same filesystem,
    // so the rename is atomic on both Unix and Windows.
    tokio::fs::rename(&part, dest)
        .await
        .with_context(|| format!("failed to move downloaded file into place: {dest:?}"))?;
    Ok(())
}

/// Sibling temp path for an in-progress download: `<dest>.part` in the same
/// directory, so the final rename stays on one filesystem and is atomic.
fn part_path(dest: &Path) -> std::path::PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

async fn try_download(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("fono/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let existing = tokio::fs::metadata(dest).await.ok().map(|m| m.len()).unwrap_or(0);
    let mut builder = client.get(url);
    if existing > 0 {
        builder = builder.header("Range", format!("bytes={existing}-"));
    }
    let resp = builder.send().await.context("GET failed")?;
    let status = resp.status();
    if !status.is_success() && status.as_u16() != 206 {
        return Err(anyhow!("HTTP {status} for {url}"));
    }
    // Append only when the server actually resumed. One that ignores `Range`
    // answers 200 with the whole file, and appending that after the bytes
    // already on disk would duplicate the start of the file.
    let resumed = existing > 0 && status.as_u16() == 206;
    if existing > 0 && !resumed {
        info!("server ignored the resume request for {url}; downloading from the start");
    }
    let existing = if resumed { existing } else { 0 };

    let total = resp.content_length().map(|c| c + existing).unwrap_or(0);
    let pb = ProgressBar::new(total.max(1));
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    if existing > 0 {
        pb.set_position(existing);
    }

    let mut file = OpenOptions::new().create(true).append(resumed).write(true).open(dest).await?;
    if !resumed {
        file.set_len(0).await.ok();
    }

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("stream chunk")?;
        file.write_all(&bytes).await?;
        BYTES_WRITTEN.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        pb.inc(bytes.len() as u64);
    }
    file.flush().await?;
    pb.finish_and_clear();
    Ok(())
}

/// Compute the lowercase-hex SHA-256 of an on-disk file. Exposed so callers
/// can verify an already-cached file and skip a redundant download.
pub async fn sha256_file(path: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut f = File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    /// Serve `body` with `200 OK` to every request, ignoring any `Range`
    /// header, the way some CDN edges and caching proxies do.
    async fn serve_ignoring_range(body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut req = [0u8; 4096];
                let _ = sock.read(&mut req).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });
        format!("http://{addr}/model.bin")
    }

    /// A partial file from an interrupted download must not end up as a
    /// prefix of the finished one when the server sends the whole file again.
    #[tokio::test]
    async fn full_reply_to_a_resume_replaces_the_partial_file() {
        const BODY: &[u8] = b"the whole model file";
        let url = serve_ignoring_range(BODY).await;
        let dir = std::env::temp_dir().join(format!("fono-download-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dest = dir.join("model.bin");
        tokio::fs::write(part_path(&dest), &BODY[..9]).await.unwrap();

        download(&url, &dest, &"0".repeat(64)).await.unwrap();

        let got = tokio::fs::read(&dest).await.unwrap();
        tokio::fs::remove_dir_all(&dir).await.ok();
        assert_eq!(got, BODY);
    }
}
