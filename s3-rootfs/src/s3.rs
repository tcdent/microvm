// S3 client — wraps rusty-s3 (signing) + ureq (HTTP).
//
// Sans-IO approach: rusty-s3 builds signed URLs, ureq makes the requests.
// No async runtime. Blocking HTTP fits our synchronous FUSE handler.
//
// Note: rusty-s3 sends encoding-type=url on ListObjectsV2, so XML keys
// are URL-encoded. We decode them in parse_list_response().

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
pub struct S3Client {
    bucket: Bucket,
    credentials: Credentials,
    agent: ureq::Agent,
    sign_duration: Duration,
}

impl S3Client {
    pub fn new(config: S3Config) -> Self {
        let endpoint = config.endpoint.parse().expect("invalid S3 endpoint URL");
        let bucket_name = config.bucket.clone();
        let region = config.region.clone();
        let bucket = Bucket::new(endpoint, UrlStyle::Path, bucket_name, region)
            .expect("invalid bucket config");
        let credentials = Credentials::new(config.access_key, config.secret_key);
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .no_delay(config.tcp_nodelay)
                .max_idle_connections(config.max_idle_connections)
                .max_idle_connections_per_host(config.max_idle_connections_per_host)
                .max_idle_age(config.max_idle_age)
                .timeout_global(config.timeout_global)
                .timeout_connect(config.timeout_connect)
                .build()
        );

        Self {
            bucket,
            credentials,
            agent,
            sign_duration: config.sign_duration,
        }
    }

    pub fn get(&self, key: &str) -> Result<Vec<u8>, S3Error> {
        let url = self.bucket
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
        let url = self.bucket
            .put_object(Some(&self.credentials), key)
            .sign(self.sign_duration);

        self.agent.put(url.as_str())
            .header("Content-Length", &data.len().to_string())
            .send(data)
            .map_err(|e| S3Error::Http(format!("{e}")))?;
        Ok(())
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
            i += 1;
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
