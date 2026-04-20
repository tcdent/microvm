//! Daemon config (research/15 §8).
//!
//! Merge order: defaults → config file → CLI flags.
//! Config file: INI format with [daemon], [s3], [defaults] sections.
//! Lookup: --config FLAG, then /etc/microd/config.ini, then
//! ~/.microvm/daemon.ini. Missing file = use defaults silently.

use std::path::PathBuf;

#[derive(Clone)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub log_dir: PathBuf,
    pub s3_endpoint: String,
    pub s3_admin_access_key: String,
    pub s3_admin_secret_key: String,
    pub default_kernel: PathBuf,
    pub default_initrd: PathBuf,
    pub default_golden_bucket: Option<String>,
    pub default_ruleset: Option<PathBuf>,
    pub ephemeral_root: PathBuf,
}

impl DaemonConfig {
    /// Parse CLI args + config file. Merge order: defaults → file → CLI.
    /// Returns Err with a clear message if the config file exists but
    /// can't be parsed.
    pub fn from_args() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().collect();
        let mut cfg = Self::defaults();

        // Find config file: --config FLAG, or well-known paths.
        let config_path = flag_val(&args, "--config")
            .map(PathBuf::from)
            .or_else(|| {
                let paths = [
                    PathBuf::from("/etc/microd/config.ini"),
                    dirs_home().join(".microvm/daemon.ini"),
                ];
                paths.into_iter().find(|p| p.exists())
            });

        if let Some(ref path) = config_path {
            if path.exists() {
                let content = std::fs::read_to_string(path)
                    .map_err(|e| format!("read config {}: {}", path.display(), e))?;
                cfg.apply_ini(&content)?;
            }
        }

        // CLI overrides.
        if let Some(v) = flag_val(&args, "--socket") {
            cfg.socket_path = PathBuf::from(v);
        }
        if let Some(v) = flag_val(&args, "--log-dir") {
            cfg.log_dir = PathBuf::from(v);
        }
        if let Some(v) = flag_val(&args, "--s3-endpoint") {
            cfg.s3_endpoint = v;
        }

        Ok(cfg)
    }

    /// Kept for tests and backward compat with callers that don't
    /// need file/CLI parsing.
    pub fn from_args_or_default() -> Self {
        Self::from_args().unwrap_or_else(|e| {
            eprintln!("config error: {e}");
            std::process::exit(1);
        })
    }

    pub fn defaults() -> Self {
        Self {
            socket_path: PathBuf::from("/tmp/microd.sock"),
            log_dir: PathBuf::from("/tmp/microd-logs"),
            s3_endpoint: "http://192.168.88.15:9090".to_string(),
            s3_admin_access_key: "minioadmin".to_string(),
            s3_admin_secret_key: "minioadmin".to_string(),
            default_kernel: PathBuf::from("dist/vmlinux"),
            default_initrd: PathBuf::from("dist/initramfs.cpio.gz"),
            default_golden_bucket: None,
            default_ruleset: None,
            ephemeral_root: PathBuf::from("/tmp/microd-ephemeral"),
        }
    }

    /// Apply key=value pairs from INI content. Supports [daemon],
    /// [s3], [defaults] sections.
    fn apply_ini(&mut self, content: &str) -> Result<(), String> {
        let mut section = String::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len()-1].to_string();
                continue;
            }
            let (key, val) = line.split_once('=')
                .ok_or_else(|| format!("bad config line: {line}"))?;
            let key = key.trim();
            let val = val.trim();

            match (section.as_str(), key) {
                ("daemon", "socket") => self.socket_path = PathBuf::from(val),
                ("daemon", "log_dir") => self.log_dir = PathBuf::from(val),
                ("s3", "endpoint") => self.s3_endpoint = val.to_string(),
                ("s3", "admin_access_key") => self.s3_admin_access_key = val.to_string(),
                ("s3", "admin_secret_key") => self.s3_admin_secret_key = val.to_string(),
                ("defaults", "kernel") => self.default_kernel = PathBuf::from(val),
                ("defaults", "initrd") => self.default_initrd = PathBuf::from(val),
                ("defaults", "golden_bucket") => self.default_golden_bucket = Some(val.to_string()),
                ("defaults", "ruleset") => self.default_ruleset = Some(PathBuf::from(val)),
                ("defaults", "ephemeral_root") => self.ephemeral_root = PathBuf::from(val),
                _ => {} // ignore unknown keys/sections
            }
        }
        Ok(())
    }
}

fn flag_val(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = DaemonConfig::defaults();
        assert!(!cfg.s3_endpoint.is_empty());
        assert!(!cfg.socket_path.as_os_str().is_empty());
    }

    #[test]
    fn parse_ini() {
        let ini = r#"
[daemon]
socket = /run/microd.sock
log_dir = /var/log/microd

[s3]
endpoint = http://10.0.0.1:9000
admin_access_key = mykey
admin_secret_key = mysecret

[defaults]
kernel = /opt/vmlinux
golden_bucket = golden-prod
ephemeral_root = /data/ephemeral
"#;
        let mut cfg = DaemonConfig::defaults();
        cfg.apply_ini(ini).unwrap();

        assert_eq!(cfg.socket_path, PathBuf::from("/run/microd.sock"));
        assert_eq!(cfg.log_dir, PathBuf::from("/var/log/microd"));
        assert_eq!(cfg.s3_endpoint, "http://10.0.0.1:9000");
        assert_eq!(cfg.s3_admin_access_key, "mykey");
        assert_eq!(cfg.default_kernel, PathBuf::from("/opt/vmlinux"));
        assert_eq!(cfg.default_golden_bucket, Some("golden-prod".to_string()));
        assert_eq!(cfg.ephemeral_root, PathBuf::from("/data/ephemeral"));
    }
}
