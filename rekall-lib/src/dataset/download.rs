use std::path::Path;

use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};

enum Scheme {
    S3 { bucket: String, key: String },
    Http,
}

fn parse_uri(uri: &str) -> Result<Scheme> {
    if let Some(rest) = uri.strip_prefix("s3://") {
        let (bucket, key) = rest.split_once('/').ok_or_else(|| {
            Error::Config(format!("Invalid S3 URI '{uri}': expected s3://bucket/key"))
        })?;
        Ok(Scheme::S3 {
            bucket: bucket.to_string(),
            key: key.to_string(),
        })
    } else if uri.starts_with("http://") || uri.starts_with("https://") {
        Ok(Scheme::Http)
    } else {
        Err(Error::Config(format!(
            "Unsupported URI scheme in '{uri}'. Expected s3://, http://, or https://"
        )))
    }
}

fn make_progress_bar(total: Option<u64>, filename: &str) -> ProgressBar {
    let pb = if let Some(total) = total {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{msg}\n{wide_bar:.cyan/dim} {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
                )
                .unwrap()
                .progress_chars("━╸─"),
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{msg} {spinner} {bytes} ({bytes_per_sec})")
                .unwrap(),
        );
        pb
    };
    pb.set_message(format!("Downloading {filename}"));
    pb
}

/// Ensure the dataset file exists locally, downloading from `uri` if needed.
///
/// Returns `Ok(true)` if a download occurred, `Ok(false)` if the file
/// already existed.
pub async fn ensure_available(uri: &str, dest: &Path) -> Result<bool> {
    if dest.exists() {
        tracing::info!("Dataset exists at {}", dest.display());
        return Ok(false);
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Config(format!(
                "Cannot create data directory '{}': {e}",
                parent.display()
            ))
        })?;
    }

    tracing::info!("Downloading dataset from {uri}");
    tracing::info!("  → {}", dest.display());

    let scheme = parse_uri(uri)?;
    let tmp_dest = dest.with_extension("hdf5.part");

    let result = match scheme {
        Scheme::S3 { bucket, key } => download_s3(&bucket, &key, &tmp_dest).await,
        Scheme::Http => download_http(uri, &tmp_dest).await,
    };

    match result {
        Ok(()) => {
            std::fs::rename(&tmp_dest, dest).map_err(|e| {
                Error::Config(format!(
                    "Cannot rename '{}' → '{}': {e}",
                    tmp_dest.display(),
                    dest.display()
                ))
            })?;
            tracing::info!("Download complete: {}", dest.display());
            Ok(true)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_dest);
            Err(e)
        }
    }
}

async fn download_s3(bucket: &str, key: &str, dest: &Path) -> Result<()> {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);

    let filename = key.rsplit('/').next().unwrap_or(key);

    // HEAD request to get content length for progress bar
    let head = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| Error::Config(format!("S3 HEAD s3://{bucket}/{key} failed: {e}")))?;
    let total = head.content_length().map(|l| l as u64);

    let pb = make_progress_bar(total, filename);

    let resp = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|e| Error::Config(format!("S3 GET s3://{bucket}/{key} failed: {e}")))?;

    let mut body = resp.body.into_async_read();
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| Error::Config(format!("Cannot create '{}': {e}", dest.display())))?;

    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut body, &mut buf)
            .await
            .map_err(|e| Error::Config(format!("S3 download read error: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .await
            .map_err(|e| Error::Config(format!("Write error: {e}")))?;
        pb.inc(n as u64);
    }

    file.flush()
        .await
        .map_err(|e| Error::Config(format!("Flush error: {e}")))?;

    pb.finish_with_message(format!("Downloaded {filename}"));
    Ok(())
}

async fn download_http(uri: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .get(uri)
        .send()
        .await
        .map_err(|e| Error::Config(format!("HTTP GET {uri} failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(Error::Config(format!(
            "HTTP GET {uri} returned {}",
            resp.status()
        )));
    }

    let total = resp.content_length();
    let filename = uri.rsplit('/').next().unwrap_or("dataset");
    let pb = make_progress_bar(total, filename);

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| Error::Config(format!("Cannot create '{}': {e}", dest.display())))?;

    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Config(format!("HTTP download error: {e}")))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| Error::Config(format!("Write error: {e}")))?;
        pb.inc(chunk.len() as u64);
    }

    file.flush()
        .await
        .map_err(|e| Error::Config(format!("Flush error: {e}")))?;

    pb.finish_with_message(format!("Downloaded {filename}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uri_s3() {
        let scheme = parse_uri("s3://my-bucket/path/to/file.hdf5").unwrap();
        assert!(matches!(
            scheme,
            Scheme::S3 {
                ref bucket,
                ref key
            } if bucket == "my-bucket" && key == "path/to/file.hdf5"
        ));
    }

    #[test]
    fn parse_uri_https() {
        assert!(matches!(
            parse_uri("https://example.com/data.hdf5").unwrap(),
            Scheme::Http
        ));
    }

    #[test]
    fn parse_uri_http() {
        assert!(matches!(
            parse_uri("http://example.com/data.hdf5").unwrap(),
            Scheme::Http
        ));
    }

    #[test]
    fn parse_uri_unknown() {
        assert!(parse_uri("ftp://example.com/data.hdf5").is_err());
    }

    #[test]
    fn parse_uri_s3_no_key() {
        assert!(parse_uri("s3://bucket-only").is_err());
    }

    #[tokio::test]
    async fn ensure_available_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.hdf5");
        std::fs::write(&path, b"data").unwrap();

        let downloaded = ensure_available("s3://bucket/key", &path).await.unwrap();
        assert!(!downloaded);
    }
}
