// S3 client — wraps rusty-s3 (signing) + ureq (HTTP).
//
// Sans-IO approach: rusty-s3 builds signed URLs, ureq makes the requests.
// No async runtime. Blocking HTTP fits our synchronous FUSE handler.

use std::time::Duration;

use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

/// Configuration for the S3 client.
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
            endpoint: "http://localhost:9090".into(),
            bucket: "rootfs".into(),
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

pub struct ObjectMeta {
    pub content_length: u64,
    pub exists: bool,
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

    pub fn head(&self, key: &str) -> Result<ObjectMeta, S3Error> {
        let url = self.bucket
            .head_object(Some(&self.credentials), key)
            .sign(self.sign_duration);

        match self.agent.head(url.as_str()).call() {
            Ok(response) => {
                let len = response.headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                Ok(ObjectMeta { content_length: len, exists: true })
            }
            Err(ureq::Error::StatusCode(404)) => {
                Ok(ObjectMeta { content_length: 0, exists: false })
            }
            Err(e) => Err(S3Error::Http(format!("{e}"))),
        }
    }

    pub fn get(&self, key: &str) -> Result<Vec<u8>, S3Error> {
        let url = self.bucket
            .get_object(Some(&self.credentials), key)
            .sign(self.sign_duration);

        let response = self.agent.get(url.as_str()).call()
            .map_err(|e| S3Error::Http(format!("{e}")))?;

        let body = response.into_body().read_to_vec()
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

        let body = response.into_body().read_to_vec()
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

    pub fn delete(&self, key: &str) -> Result<(), S3Error> {
        let url = self.bucket
            .delete_object(Some(&self.credentials), key)
            .sign(self.sign_duration);

        self.agent.delete(url.as_str()).call()
            .map_err(|e| S3Error::Http(format!("{e}")))?;
        Ok(())
    }

    pub fn list(&self, prefix: &str, delimiter: Option<&str>) -> Result<ListResult, S3Error> {
        let mut action = self.bucket
            .list_objects_v2(Some(&self.credentials));

        action.query_mut().insert("prefix", prefix);
        if let Some(d) = delimiter {
            action.query_mut().insert("delimiter", d);
        }
        action.query_mut().insert("max-keys", "1000");

        let url = action.sign(self.sign_duration);

        let response = self.agent.get(url.as_str()).call()
            .map_err(|e| S3Error::Http(format!("{e}")))?;

        let body = response.into_body().read_to_string()
            .map_err(|e| S3Error::Http(format!("{e}")))?;

        parse_list_response(&body)
    }
}

pub struct ListResult {
    pub objects: Vec<ListObject>,
    pub prefixes: Vec<String>,
}

pub struct ListObject {
    pub key: String,
    pub size: u64,
}

fn parse_list_response(xml: &str) -> Result<ListResult, S3Error> {
    let mut objects = Vec::new();
    let mut prefixes = Vec::new();

    let mut i = 0;

    while i < xml.len() {
        if let Some(pos) = xml[i..].find("<Contents>") {
            let start = i + pos;
            if let Some(end_pos) = xml[start..].find("</Contents>") {
                let block = &xml[start..start + end_pos + 11];
                let key = extract_tag(block, "Key").unwrap_or_default();
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
                if let Some(prefix) = extract_tag(block, "Prefix") {
                    prefixes.push(prefix);
                }
                i = start + end_pos + 17;
                continue;
            }
        }
        i += 1;
    }

    Ok(ListResult { objects, prefixes })
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
