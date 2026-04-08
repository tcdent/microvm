use std::sync::OnceLock;
use s3_rootfs::{S3Client, S3Config};

/// Default test endpoint (rustfs-test container).
fn test_endpoint() -> String {
    std::env::var("S3_TEST_ENDPOINT").unwrap_or_else(|_| "http://localhost:19090".into())
}

fn test_bucket() -> String {
    std::env::var("S3_TEST_BUCKET").unwrap_or_else(|_| "test-s3fs".into())
}

pub fn test_config() -> S3Config {
    S3Config {
        endpoint: test_endpoint(),
        bucket: test_bucket(),
        ..S3Config::default()
    }
}

/// Returns true if the S3 test backend is reachable.
/// Caches the result for the process lifetime.
pub fn s3_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let client = S3Client::new(test_config());
        // Try listing with an empty prefix — if the bucket doesn't exist
        // RustFS/MinIO returns an error, but the connection itself works.
        // We also accept "bucket not found" as "S3 is available" and
        // let the fixture handle bucket creation.
        // Try a LIST — any HTTP response (even 404 NoSuchBucket) means S3 is reachable.
        // Only connection errors mean the backend is down.
        match client.list("", None) {
            Ok(_) => true,
            Err(e) => {
                let msg = format!("{e}");
                // HTTP errors (status codes) mean S3 is reachable
                if msg.contains("http status") || msg.contains("NoSuchBucket") {
                    true
                } else {
                    eprintln!("S3 test backend not available at {}: {e}", test_endpoint());
                    false
                }
            }
        }
    })
}

/// Call at the top of each test to skip if S3 is unavailable.
macro_rules! require_s3 {
    () => {
        if !$crate::harness::s3::s3_available() {
            eprintln!("SKIPPED: S3 test backend not available");
            return;
        }
    };
}
pub(crate) use require_s3;
