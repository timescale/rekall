// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! S3 side of publishing: a thin uploader bound to one bucket.
//!
//! Reuses the credential/region behaviour from `dataset/download.rs`
//! (`aws_config` default chain + region-redirect handling), but for writes.

use std::path::{Path, PathBuf};

use aws_sdk_s3::primitives::ByteStream;

use crate::error::{Error, Result};

/// Walk an SDK error's `source()` chain into one `: `-joined message — the
/// top-level Display is usually just "service error" / "dispatch failure".
fn describe<E: std::error::Error>(e: &E) -> String {
    let mut parts = vec![e.to_string()];
    let mut src: Option<&dyn std::error::Error> = e.source();
    while let Some(s) = src {
        parts.push(s.to_string());
        src = s.source();
    }
    parts.join(": ")
}

/// Best-effort content type from a file extension. `.json.gz` and friends
/// are archived by their final extension (gzip); browsers fetch the report
/// `.html`/`.svg` so those get real types.
fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "json" => "application/json",
        "gz" => "application/gzip",
        "html" | "htm" => "text/html",
        "svg" => "image/svg+xml",
        "txt" | "gnuplot" | "gp" | "log" => "text/plain",
        "csv" => "text/csv",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

/// Recursively collect all files under `dir` (no directories).
pub fn walk_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// An S3 uploader bound to a bucket, with on-demand region resolution.
pub struct Uploader {
    client: aws_sdk_s3::Client,
    base: aws_config::SdkConfig,
    pub bucket: String,
}

impl Uploader {
    pub async fn new(region: Option<&str>, bucket: &str) -> Result<Self> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(r) = region {
            loader = loader.region(aws_sdk_s3::config::Region::new(r.to_string()));
        }
        let base = loader.load().await;
        let client = aws_sdk_s3::Client::new(&base);
        Ok(Self {
            client,
            base,
            bucket: bucket.to_string(),
        })
    }

    fn rebind_region(&mut self, region: String) {
        let cfg = self
            .base
            .to_builder()
            .region(aws_sdk_s3::config::Region::new(region))
            .build();
        self.client = aws_sdk_s3::Client::new(&cfg);
    }

    /// Ensure the bucket exists: succeed if present (rebinding to its region
    /// on a redirect), create it on 404, error otherwise.
    pub async fn ensure_bucket(&mut self) -> Result<()> {
        match self.client.head_bucket().bucket(&self.bucket).send().await {
            Ok(_) => Ok(()),
            Err(e) => {
                // Pre-existing bucket in another region: the 301 carries the
                // real region — rebind and treat as existing.
                if let Some(region) = e
                    .raw_response()
                    .and_then(|r| r.headers().get("x-amz-bucket-region"))
                    .map(|h| h.to_string())
                {
                    self.rebind_region(region);
                    return Ok(());
                }
                if e.as_service_error().is_some_and(|se| se.is_not_found()) {
                    self.create_bucket().await
                } else {
                    Err(Error::Config(format!(
                        "S3 head-bucket '{}' failed: {}",
                        self.bucket,
                        describe(&e)
                    )))
                }
            }
        }
    }

    async fn create_bucket(&self) -> Result<()> {
        use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration};
        let region = self
            .base
            .region()
            .map(|r| r.as_ref().to_string())
            .unwrap_or_else(|| "us-east-1".to_string());

        let mut req = self.client.create_bucket().bucket(&self.bucket);
        // us-east-1 must NOT send a LocationConstraint; every other region must.
        if region != "us-east-1" {
            req = req.create_bucket_configuration(
                CreateBucketConfiguration::builder()
                    .location_constraint(BucketLocationConstraint::from(region.as_str()))
                    .build(),
            );
        }
        req.send().await.map_err(|e| {
            Error::Config(format!(
                "Could not create S3 bucket '{}': {}\n\
                 If the name is taken globally, set [publish].s3_bucket to a unique name.",
                self.bucket,
                describe(&e)
            ))
        })?;
        tracing::info!("created S3 bucket '{}' in {region}", self.bucket);
        Ok(())
    }

    /// Does an object exist at `key`? Used to detect a completed prior upload.
    pub async fn object_exists(&self, key: &str) -> Result<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|se| se.is_not_found()) => Ok(false),
            Err(e) => Err(Error::Config(format!(
                "S3 head-object s3://{}/{key} failed: {}",
                self.bucket,
                describe(&e)
            ))),
        }
    }

    pub async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let body = ByteStream::from_path(path)
            .await
            .map_err(|e| Error::Config(format!("read {} for upload: {e}", path.display())))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .content_type(content_type(path))
            .send()
            .await
            .map_err(|e| {
                Error::Config(format!(
                    "S3 put s3://{}/{key} failed: {}",
                    self.bucket,
                    describe(&e)
                ))
            })?;
        Ok(())
    }

    pub async fn put_bytes(&self, key: &str, bytes: Vec<u8>, content_type: &str) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| {
                Error::Config(format!(
                    "S3 put s3://{}/{key} failed: {}",
                    self.bucket,
                    describe(&e)
                ))
            })?;
        Ok(())
    }
}
