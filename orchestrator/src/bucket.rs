//! Per-VM bucket lifecycle (research/15 §5).
//!
//! Orchestrator owns create-on-start; buckets are PRESERVED on stop
//! so re-launching the same name resumes the same state. A separate
//! destroy command (not in v0) does the actual deletion.
//!
//! S3AdminClient uses ureq with basic-auth style presigned URLs
//! against RustFS/MinIO. Minimum viable — real S3 needs sigv4 lift
//! later. Sufficient for dev + single-host deployment.

use std::sync::Arc;

use crate::Error;

/// Lightweight S3 admin client for bucket create/delete/exists.
/// Shares a ureq::Agent for connection pooling.
pub struct S3AdminClient {
    agent: ureq::Agent,
    endpoint: String,
    access_key: String,
    secret_key: String,
}

impl S3AdminClient {
    pub fn new(endpoint: &str, access_key: &str, secret_key: &str) -> Self {
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder().build(),
        );
        Self {
            agent,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
        }
    }

    /// Check if a bucket exists (HEAD bucket).
    pub fn bucket_exists(&self, name: &str) -> Result<bool, Error> {
        let url = format!("{}/{}", self.endpoint, name);
        match self.agent.head(&url)
            .header("Authorization", &self.auth_header())
            .call()
        {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(e) => Err(Error::BucketCreate(format!(
                "HEAD bucket {}: {}", name, e
            ))),
        }
    }

    /// Create a bucket (PUT bucket).
    pub fn create_bucket(&self, name: &str) -> Result<(), Error> {
        let url = format!("{}/{}", self.endpoint, name);
        match self.agent.put(&url)
            .header("Authorization", &self.auth_header())
            .send_empty()
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(409)) => Ok(()), // already exists
            Err(e) => Err(Error::BucketCreate(format!(
                "PUT bucket {}: {}", name, e
            ))),
        }
    }

    /// Basic auth header for RustFS/MinIO dev endpoints.
    /// NOT sigv4 — won't work against real AWS S3.
    fn auth_header(&self) -> String {
        use std::io::Write;
        let mut buf = Vec::new();
        write!(buf, "{}:{}", self.access_key, self.secret_key).unwrap();
        let encoded = base64_encode(&buf);
        format!("Basic {}", encoded)
    }
}

/// Minimal base64 encode (no external crate needed for this).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        result.push(CHARS[(b0 >> 2)] as char);
        result.push(CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((b1 & 0xF) << 2) | (b2 >> 6)] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(b2 & 0x3F)] as char);
        } else {
            result.push('=');
        }
    }
    result
}

/// Ensure the bucket exists for this VM.
///
/// - If explicit_bucket is Some, return it as-is (user is responsible
///   for bucket lifecycle).
/// - Otherwise compute `vm-<name_hash>`, check existence via HEAD,
///   create via PUT if missing.
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

/// Destroy a bucket. Not wired in v0 — kept here so the API shape is
/// visible. Wiring comes after v0 is stable.
pub fn destroy(_name_hash: &str) -> Result<(), Error> {
    todo!("post-v0")
}
