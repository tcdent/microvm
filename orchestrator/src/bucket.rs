//! Per-VM bucket lifecycle (research/15 §5).
//!
//! Orchestrator owns create-on-start; buckets are PRESERVED on stop
//! so re-launching the same name resumes the same state. A separate
//! destroy command (not in v0) does the actual deletion.
//!
//! S3AdminClient uses rusty-s3 for sigv4-signed presigned URLs,
//! same signing approach as s3-rootfs::S3Client.

use std::time::Duration;

use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

use crate::Error;

const SIGN_DURATION: Duration = Duration::from_secs(300);

/// Lightweight S3 admin client for bucket create/delete/exists.
pub struct S3AdminClient {
    agent: ureq::Agent,
    credentials: Credentials,
    endpoint: String,
}

impl S3AdminClient {
    pub fn new(endpoint: &str, access_key: &str, secret_key: &str) -> Self {
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder().build(),
        );
        let credentials = Credentials::new(access_key, secret_key);
        Self {
            agent,
            credentials,
            endpoint: endpoint.trim_end_matches('/').to_string(),
        }
    }

    /// Check if a bucket exists (HEAD bucket via presigned URL).
    pub fn bucket_exists(&self, name: &str) -> Result<bool, Error> {
        let bucket = self.make_bucket(name);
        // Use a simple GET on the bucket root with max-keys=0.
        // rusty-s3 doesn't have a dedicated HeadBucket action, but
        // ListObjectsV2 with max-keys=0 works as an existence check.
        let mut action = bucket.list_objects_v2(Some(&self.credentials));
        action.query_mut().insert("max-keys", "0");
        let url = action.sign(SIGN_DURATION);

        match self.agent.get(url.as_str()).call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(e) => Err(Error::BucketCreate(format!(
                "bucket exists check {}: {}", name, e
            ))),
        }
    }

    /// Create a bucket (PUT bucket via presigned URL).
    pub fn create_bucket(&self, name: &str) -> Result<(), Error> {
        let bucket = self.make_bucket(name);
        let action = bucket.create_bucket(&self.credentials);
        let url = action.sign(SIGN_DURATION);

        match self.agent.put(url.as_str()).send_empty() {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(409)) => Ok(()), // already exists
            Err(e) => Err(Error::BucketCreate(format!(
                "create bucket {}: {}", name, e
            ))),
        }
    }

    fn make_bucket(&self, name: &str) -> Bucket {
        let base_url = self.endpoint.parse().expect("valid S3 endpoint URL");
        Bucket::new(base_url, UrlStyle::Path, name.to_string(), "us-east-1")
            .expect("valid bucket name")
    }
}

/// Ensure the bucket exists for this VM.
///
/// - If explicit_bucket is Some, return it as-is (user is responsible
///   for bucket lifecycle).
/// - Otherwise compute `vm-<name_hash>`, check existence,
///   create if missing.
pub fn ensure(
    name_hash: &str,
    explicit_bucket: Option<&str>,
    admin: &S3AdminClient,
) -> Result<String, Error> {
    if let Some(bucket) = explicit_bucket {
        return Ok(bucket.to_string());
    }

    let bucket_name = format!("vm-{}", name_hash);

    if admin.bucket_exists(&bucket_name)? {
        eprintln!("bucket::ensure: {} exists (reuse)", bucket_name);
        return Ok(bucket_name);
    }

    eprintln!("bucket::ensure: creating {}", bucket_name);
    admin.create_bucket(&bucket_name)?;
    Ok(bucket_name)
}

/// Destroy a bucket. Not wired in v0.
pub fn destroy(_name_hash: &str) -> Result<(), Error> {
    todo!("post-v0")
}
