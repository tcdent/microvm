// S3 client — wraps rusty-s3 (signing) + ureq (HTTP).
//
// Sans-IO approach: rusty-s3 builds signed URLs, ureq makes the requests.
// No async runtime. Blocking HTTP fits our synchronous FUSE handler.
//
// Note: rusty-s3 sends encoding-type=url on ListObjectsV2, so XML keys
// are URL-encoded. We decode them in parse_list_response().

use std::sync::Arc;
use std::time::Duration;

use percent_encoding::percent_decode_str;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

/// Configuration for the S3 client.
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,

    /// Sign duration for presigned URLs
    pub sign_duration: Duration,

    // HTTP connection tuning
    /// Disable Nagle's algorithm for lower latency on small requests
    pub tcp_nodelay: bool,
    /// Maximum idle connections in the pool (across all hosts)
    pub max_idle_connections: usize,
    /// Maximum idle connections per host
    pub max_idle_connections_per_host: usize,
    /// How long to keep idle connections alive
    pub max_idle_age: Duration,
    /// Global timeout for any single S3 request
    pub timeout_global: Option<Duration>,
    /// Timeout for TCP connect phase
    pub timeout_connect: Option<Duration>,
}

impl Default for S3Config {
    fn default() -> Self {
        Self {
            endpoint: "http://192.168.88.15:9090".into(),
            bucket: "vm-dev".into(),
            region: "us-east-1".into(),
            access_key: "minioadmin".into(),
            secret_key: "minioadmin".into(),
            sign_duration: Duration::from_secs(3600),

            tcp_nodelay: true,
            max_idle_connections: 32,
            max_idle_connections_per_host: 16,
            max_idle_age: Duration::from_secs(30),
            timeout_global: Some(Duration::from_secs(30)),
            timeout_connect: Some(Duration::from_secs(5)),
        }
    }
}

/// S3 client for filesystem operations.
///
/// Each client has a **primary bucket** — the bucket every unqualified
/// method (`get`, `put`, `list`, …) talks to. That's the bucket the
/// per-VM `S3Fs` cares about, and keeping it implicit at those call
/// sites keeps the FUSE path legible.
///
/// Bucket-override variants (`get_from`, `put_to`, …) take an
/// explicit bucket name and are used where multi-bucket awareness
/// matters (golden loader). Credentials, endpoint, region, and the
/// `ureq::Agent` are global to the client — per-bucket credentials
/// aren't supported, by design (one fleet identity).
pub struct S3Client {
    bucket: Bucket,
    credentials: Credentials,
    agent: ureq::Agent,
    sign_duration: Duration,
    /// Endpoint URL (unparsed). Stashed so bucket-override methods
    /// can rebuild a `Bucket` on demand. Re-parsed per override call;
    /// override methods aren't on any hot path (golden loader runs
    /// once at orchestrator init), so avoiding a `url` crate dep
    /// outweighs saving microseconds.
    endpoint: String,
    /// Region string. Same rationale as `endpoint`.
    region: String,
}

impl S3Client {
    pub fn new(config: S3Config) -> Self {
        // Single-VM path: build our own Agent from this config's pool
        // settings. Equivalent to the orchestrator path's
        // `with_shared_agent(config, Arc::new(build_agent(&config)))`
        // but without forcing callers to thread Arcs they don't need.
        let agent = Self::build_agent(&config);
        let endpoint = config.endpoint.clone();
        let region = config.region.clone();
        let (bucket, credentials, sign_duration) = Self::split_config(config);
        Self { bucket, credentials, agent, sign_duration, endpoint, region }
    }

    /// Construct an `S3Client` that shares an externally-owned `ureq::Agent`
    /// (and therefore its TCP/TLS connection pool) with other clients.
    ///
    /// This is the orchestrator-era constructor: one `Arc<ureq::Agent>` per
    /// host, cloned into per-VM `S3Client`s and the golden read client.
    /// A 64-VM host stops maintaining ~512 HTTP connections (8/client ×
    /// 64 clients) and keeps ~32 shared ones.
    ///
    /// The per-VM `config` still controls bucket/credentials/sign duration
    /// (per-client state). The pool tuning fields on `S3Config`
    /// (`tcp_nodelay`, `max_idle_*`, `timeout_*`) are ignored here — the
    /// Agent the caller passed in already embodies those choices. If
    /// per-VM pool settings ever need to diverge, the right fix is to
    /// give each client its own Agent via `new()`, not to try to
    /// override a shared one.
    pub fn with_shared_agent(config: S3Config, agent: Arc<ureq::Agent>) -> Self {
        // Extract an Agent handle that shares the pool. `ureq::Agent`
        // is internally Arc-based for its pool, so `clone()` is a
        // reference-count bump, not a pool copy.
        let agent = (*agent).clone();
        let endpoint = config.endpoint.clone();
        let region = config.region.clone();
        let (bucket, credentials, sign_duration) = Self::split_config(config);
        Self { bucket, credentials, agent, sign_duration, endpoint, region }
    }

    /// Build a new `ureq::Agent` sized by this config's pool tuning.
    /// Public so the orchestrator can construct one Agent up front and
    /// then share it across all `S3Client`s via `with_shared_agent`.
    pub fn build_agent(config: &S3Config) -> ureq::Agent {
        ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .no_delay(config.tcp_nodelay)
                .max_idle_connections(config.max_idle_connections)
                .max_idle_connections_per_host(config.max_idle_connections_per_host)
                .max_idle_age(config.max_idle_age)
                .timeout_global(config.timeout_global)
                .timeout_connect(config.timeout_connect)
                .build()
        )
    }

    /// Rebuild a `Bucket` for an arbitrary bucket name, reusing this
    /// client's endpoint + region. Used by bucket-override methods;
    /// cheap enough (one URL parse + a few String allocations) to
    /// construct transiently per call since those calls aren't on any
    /// hot path. Caller is expected to have already verified the name
    /// is a valid S3 bucket identifier.
    fn bucket_for(&self, name: &str) -> Bucket {
        let endpoint = self.endpoint.parse().expect("invalid S3 endpoint URL");
        Bucket::new(endpoint, UrlStyle::Path, name.to_string(), self.region.clone())
            .expect("invalid bucket config")
    }

    /// Helper: split `S3Config` into the per-client state (bucket,
    /// credentials, sign duration). Shared between both constructors
    /// so we can't drift on which fields become client state vs which
    /// belong to the Agent.
    fn split_config(config: S3Config) -> (Bucket, Credentials, Duration) {
        let endpoint = config.endpoint.parse().expect("invalid S3 endpoint URL");
        let bucket = Bucket::new(endpoint, UrlStyle::Path, config.bucket, config.region)
            .expect("invalid bucket config");
        let credentials = Credentials::new(config.access_key, config.secret_key);
        (bucket, credentials, config.sign_duration)
    }

    pub fn get(&self, key: &str) -> Result<Vec<u8>, S3Error> {
        self.get_with_bucket(&self.bucket, key)
    }

    /// Bucket-override variant of [`get`]: GET `key` from the named
    /// bucket rather than this client's primary bucket. Used by the
    /// golden loader (one client, primary bucket = per-VM bucket,
    /// golden reads go through here). Credentials, endpoint, region,
    /// and the connection pool all come from this client — only the
    /// bucket name changes.
    pub fn get_from(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        self.get_with_bucket(&self.bucket_for(bucket), key)
    }

    /// Shared body of `get` and `get_from`. Takes a `&Bucket` so the
    /// two public entrypoints can differ only in how they source it.
    fn get_with_bucket(&self, bucket: &Bucket, key: &str) -> Result<Vec<u8>, S3Error> {
        let url = bucket
            .get_object(Some(&self.credentials), key)
            .sign(self.sign_duration);

        let response = self.agent.get(url.as_str()).call()
            .map_err(|e| S3Error::Http(format!("{e}")))?;

        let body = response.into_body().with_config().limit(u64::MAX).read_to_vec()
            .map_err(|e| S3Error::Http(format!("{e}")))?;
        Ok(body)
    }

    pub fn get_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        let url = self.bucket
            .get_object(Some(&self.credentials), key)
            .sign(self.sign_duration);

        let range = format!("bytes={}-{}", start, end);
        let response = self.agent.get(url.as_str())
            .header("Range", &range)
            .call()
            .map_err(|e| S3Error::Http(format!("{e}")))?;

        let body = response.into_body().with_config().limit(u64::MAX).read_to_vec()
            .map_err(|e| S3Error::Http(format!("{e}")))?;
        Ok(body)
    }

    pub fn put(&self, key: &str, data: &[u8]) -> Result<(), S3Error> {
        self.put_with_bucket(&self.bucket, key, data)
    }

    /// Bucket-override variant of [`put`]: PUT into the named bucket
    /// rather than this client's primary. Used by the golden-upload
    /// tool to write `golden/<hash>` and `golden/.manifest`.
    pub fn put_to(&self, bucket: &str, key: &str, data: &[u8]) -> Result<(), S3Error> {
        self.put_with_bucket(&self.bucket_for(bucket), key, data)
    }

    fn put_with_bucket(&self, bucket: &Bucket, key: &str, data: &[u8]) -> Result<(), S3Error> {
        let url = bucket
            .put_object(Some(&self.credentials), key)
            .sign(self.sign_duration);

        self.agent.put(url.as_str())
            .header("Content-Length", &data.len().to_string())
            .send(data)
            .map_err(|e| S3Error::Http(format!("{e}")))?;
        Ok(())
    }

    /// Existence check against a named bucket via HEAD. `Ok(true)`
    /// on 200, `Ok(false)` on 404, `Err` on any other HTTP / network
    /// error. Used by the golden-upload tool for
    /// HEAD-then-PUT-if-absent dedup on content-addressed blobs —
    /// avoids re-uploading blobs that already exist.
    pub fn exists_in(&self, bucket: &str, key: &str) -> Result<bool, S3Error> {
        let b = self.bucket_for(bucket);
        let url = b.head_object(Some(&self.credentials), key).sign(self.sign_duration);
        match self.agent.head(url.as_str()).call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(e) => Err(S3Error::Http(format!("{e}"))),
        }
    }

    /// Server-side copy — no data transferred through the client.
    pub fn copy(&self, src_key: &str, dst_key: &str) -> Result<(), S3Error> {
        let url = self.bucket
            .put_object(Some(&self.credentials), dst_key)
            .sign(self.sign_duration);

        // URL-encode the copy source — keys contain \x01 which is invalid in HTTP headers
        let encoded_key: String = src_key.bytes().map(|b| {
            if b.is_ascii_alphanumeric() || b == b'/' || b == b'-' || b == b'_' || b == b'.' {
                format!("{}", b as char)
            } else {
                format!("%{:02X}", b)
            }
        }).collect();
        let copy_source = format!("/{}/{}", self.bucket.name(), encoded_key);

        self.agent.put(url.as_str())
            .header("x-amz-copy-source", &copy_source)
            .send(&[] as &[u8])
            .map_err(|e| S3Error::Http(format!("{e}")))?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<(), S3Error> {
        let url = self.bucket
            .delete_object(Some(&self.credentials), key)
            .sign(self.sign_duration);

        self.agent.delete(url.as_str()).call()
            .map_err(|e| S3Error::Http(format!("{e}")))?;
        Ok(())
    }

    pub fn list(&self, prefix: &str, delimiter: Option<&str>) -> Result<ListResult, S3Error> {
        let mut all_objects = Vec::new();
        let mut all_prefixes = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut action = self.bucket
                .list_objects_v2(Some(&self.credentials));

            action.query_mut().insert("prefix", prefix);
            if let Some(d) = delimiter {
                action.query_mut().insert("delimiter", d);
            }
            action.query_mut().insert("max-keys", "1000");
            if let Some(ref token) = continuation_token {
                action.query_mut().insert("continuation-token", token);
            }

            let url = action.sign(self.sign_duration);

            let response = self.agent.get(url.as_str()).call()
                .map_err(|e| S3Error::Http(format!("{e}")))?;

            let body = response.into_body().read_to_string()
                .map_err(|e| S3Error::Http(format!("{e}")))?;

            let page = ListResult::from_xml(&body)?;
            all_objects.extend(page.objects);
            all_prefixes.extend(page.prefixes);

            if !page.is_truncated || page.next_continuation_token.is_none() {
                break;
            }
            continuation_token = page.next_continuation_token;
        }

        // We've drained the iterator, so by construction the aggregated
        // result is not truncated and has no continuation to resume from.
        Ok(ListResult {
            objects: all_objects,
            prefixes: all_prefixes,
            is_truncated: false,
            next_continuation_token: None,
        })
    }

    /// Lightweight LIST for lookup — single page, no pagination, small result set.
    /// Returns raw keys + sizes for cache population.
    pub fn list_probe(&self, prefix: &str, delimiter: Option<&str>, max_keys: u32) -> Result<ListResult, S3Error> {
        let mut action = self.bucket
            .list_objects_v2(Some(&self.credentials));

        let max_keys_str = max_keys.to_string();
        action.query_mut().insert("prefix", prefix);
        action.query_mut().insert("max-keys", &max_keys_str);
        if let Some(d) = delimiter {
            action.query_mut().insert("delimiter", d);
        }

        let url = action.sign(self.sign_duration);

        let response = self.agent.get(url.as_str()).call()
            .map_err(|e| S3Error::Http(format!("{e}")))?;

        let body = response.into_body().read_to_string()
            .map_err(|e| S3Error::Http(format!("{e}")))?;

        ListResult::from_xml(&body)
    }
}

pub struct ListResult {
    pub objects: Vec<ListObject>,
    pub prefixes: Vec<String>,
    pub is_truncated: bool,
    /// Continuation token for the next page when `is_truncated` is true.
    /// Paginating callers pass this back as `continuation-token`; terminal
    /// callers ignore it.
    pub next_continuation_token: Option<String>,
}

pub struct ListObject {
    pub key: String,
    pub size: u64,
}

impl ListResult {
    /// Parse an S3 ListObjectsV2 XML response. Single deserialization site
    /// for everything we pull out of the response — add new fields here, not
    /// via extra extract_tag calls at the call site.
    pub fn from_xml(xml: &str) -> Result<Self, S3Error> {
        let mut objects = Vec::new();
        let mut prefixes = Vec::new();

        let mut i = 0;

        while i < xml.len() {
            if let Some(pos) = xml[i..].find("<Contents>") {
                let start = i + pos;
                if let Some(end_pos) = xml[start..].find("</Contents>") {
                    let block = &xml[start..start + end_pos + 11];
                    // URL-decode key — rusty-s3 requests encoding-type=url,
                    // so the server returns URL-encoded keys in the XML
                    let key_raw = extract_tag(block, "Key").unwrap_or_default();
                    let key = percent_decode_str(&key_raw).decode_utf8_lossy().into_owned();
                    let size = extract_tag(block, "Size")
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    objects.push(ListObject { key, size });
                    i = start + end_pos + 11;
                    continue;
                }
            }
            if let Some(pos) = xml[i..].find("<CommonPrefixes>") {
                let start = i + pos;
                if let Some(end_pos) = xml[start..].find("</CommonPrefixes>") {
                    let block = &xml[start..start + end_pos + 17];
                    if let Some(prefix_raw) = extract_tag(block, "Prefix") {
                        let prefix = percent_decode_str(&prefix_raw).decode_utf8_lossy().into_owned();
                        prefixes.push(prefix);
                    }
                    i = start + end_pos + 17;
                    continue;
                }
            }
            // Advance to the next UTF-8 char boundary. Plain `i += 1`
            // panics on multi-byte characters (e.g. Chinese path names
            // in node's test fixtures).
            i += xml[i..].chars().next().map_or(1, |c| c.len_utf8());
        }

        let is_truncated = extract_tag(xml, "IsTruncated")
            .map(|v| v == "true")
            .unwrap_or(false);
        let next_continuation_token = extract_tag(xml, "NextContinuationToken");

        Ok(ListResult { objects, prefixes, is_truncated, next_continuation_token })
    }
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

#[derive(Debug)]
pub enum S3Error {
    Http(String),
    NotFound,
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            S3Error::Http(e) => write!(f, "S3 HTTP error: {e}"),
            S3Error::NotFound => write!(f, "S3 object not found"),
        }
    }
}

impl std::error::Error for S3Error {}

/// Extension trait to convert `Result<T, S3Error>` into `io::Result<T>`
/// with logged context. All S3 failures collapse to `EIO` because the
/// FUSE caller (kernel) can't act on a finer classification, but the
/// log line preserves the underlying cause so debugging doesn't require
/// git-archaeology. Centralizing the conversion here keeps the 25+ call
/// sites in `s3fs.rs` readable: `.io_err("release PUT")?` in place of a
/// multi-line `.map_err(|e| { log!(...); io::Error::from_raw_os_error(EIO) })`.
pub trait S3ResultExt<T> {
    fn io_err(self, context: &str) -> std::io::Result<T>;
}

impl<T> S3ResultExt<T> for Result<T, S3Error> {
    fn io_err(self, context: &str) -> std::io::Result<T> {
        self.map_err(|e| {
            crate::log!("S3Fs: {}: {}", context, e);
            std::io::Error::from_raw_os_error(libc::EIO)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> S3Config {
        // Minimal config — endpoint is parseable but nothing is
        // actually dialed; these tests only verify construction
        // shape, not end-to-end S3 behavior.
        S3Config {
            endpoint: "http://127.0.0.1:9090".to_string(),
            bucket: "test-bucket".to_string(),
            region: "us-east-1".to_string(),
            access_key: "ak".to_string(),
            secret_key: "sk".to_string(),
            ..S3Config::default()
        }
    }

    #[test]
    fn new_constructs_own_agent() {
        // Just verify `new()` returns a client without panicking on
        // the ureq::Agent build step. No network.
        let _client = S3Client::new(cfg());
    }

    #[test]
    fn with_shared_agent_shares_pool() {
        // Build one Agent, construct two clients from it, verify
        // both constructions succeed. The `Arc::strong_count` jump
        // from 1 to 3 (one in `shared`, two clones-of-inner stashed
        // inside each S3Client) confirms the passed-in Arc is
        // unwrapped-and-cloned correctly, not re-Arc'd.
        let shared = Arc::new(S3Client::build_agent(&cfg()));
        assert_eq!(Arc::strong_count(&shared), 1);

        let _c1 = S3Client::with_shared_agent(cfg(), Arc::clone(&shared));
        let _c2 = S3Client::with_shared_agent(cfg(), Arc::clone(&shared));
        // The passed-in Arc clones went into with_shared_agent and
        // were dropped there (after extracting an Agent clone); the
        // strong count returns to 1.
        assert_eq!(Arc::strong_count(&shared), 1);
    }
}
