//! Per-workload ruleset engine (research/14 §1).
//!
//! TOML-driven path→behavior config, loaded at mount time. Globs
//! compile to a `GlobSet` (Aho-Corasick automaton from the `globset`
//! crate) so resolution is O(path length), not O(rules). The result
//! is resolved once per `open()`/`create()` and cached on the file
//! handle as [`HandleRules`] — every subsequent read/write/release
//! is a struct field access, not a trie walk.
//!
//! # Default behavior
//!
//! Paths not matching any rule get conservative defaults:
//! `Durable` / `Auto` read-ahead / no coalesce. This is opt-in
//! behavior tuning, not opt-out.
//!
//! # Path matching
//!
//! Glob patterns match against the S3 key's path component, which
//! has no leading slash (e.g. `root/.npm/_cacache/...`, not
//! `/root/.npm/_cacache/...`). A rule like `glob = "/etc/*"` will
//! silently never match — use `glob = "etc/*"` or `glob = "**/etc/*"`.
//!
//! # TOML format
//!
//! ```toml
//! [[rule]]
//! glob = "**/.npm/_cacache/**"
//! durability = "ephemeral"
//! readahead = "off"
//!
//! [[rule]]
//! glob = "**/node_modules/**"
//! durability = "local"
//! coalesce_ms = 250
//! ```

use std::path::Path;
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};

/// How writes for this path reach durable storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Never touches S3. Data lives in the inflight map until
    /// evicted by RAM pressure or ruleset timeout. Guest reads
    /// still see bytes via the inflight consult path.
    Ephemeral,
    /// Placeholder: currently identical to `Durable` with a larger
    /// coalesce window. Will drain to a local NVMe journal once
    /// host-crash durability is justified (research/14 §2a).
    Local,
    /// Current behavior: drain to S3 via worker pool with
    /// exponential backoff. fsync() blocks until PUT completes.
    Durable,
}

/// Read-ahead strategy for this path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAhead {
    /// No read-ahead. Each read fetches exactly the requested
    /// range. Appropriate for random-access files (SQLite, etc.).
    Off,
    /// Dynamic detection: grow window on sequential reads, shrink
    /// on random access. Default.
    Auto,
    /// Start with a large window and grow faster. Appropriate for
    /// archives, binaries, streaming reads.
    Aggressive,
}

/// Per-handle behavior, resolved once at `open()`/`create()` time.
///
/// Stored on [`HandleEntry`](crate::s3fs::HandleEntry) and consulted
/// by read/write/release without further glob matching.
#[derive(Debug, Clone, Copy)]
pub struct HandleRules {
    pub durability: Durability,
    pub readahead: ReadAhead,
    pub coalesce: Option<Duration>,
}

impl Default for HandleRules {
    fn default() -> Self {
        Self {
            durability: Durability::Durable,
            readahead: ReadAhead::Auto,
            coalesce: None,
        }
    }
}

/// Compiled ruleset. Immutable after construction — safe to share
/// via `Arc` across threads without locking.
///
/// Manual `Debug` because `GlobSet` doesn't implement `Debug`.
pub struct Ruleset {
    matcher: GlobSet,
    /// Parallel to matcher's glob indices: matcher.matches(path)
    /// returns indices into this vec. Last match wins (TOML order).
    rules: Vec<HandleRules>,
}

impl std::fmt::Debug for Ruleset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ruleset")
            .field("rules_count", &self.rules.len())
            .finish()
    }
}

impl Ruleset {
    /// Empty ruleset — every path gets [`HandleRules::default()`].
    pub fn empty() -> Self {
        Self {
            matcher: GlobSet::empty(),
            rules: Vec::new(),
        }
    }

    /// Load and compile a ruleset from a TOML file.
    pub fn from_toml(path: &Path) -> Result<Self, RulesetError> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| RulesetError::Io(path.to_path_buf(), e))?;
        Self::parse(&contents)
    }

    /// Parse and compile a ruleset from a TOML string.
    pub fn parse(toml_str: &str) -> Result<Self, RulesetError> {
        let doc: TomlRuleset = toml::from_str(toml_str)
            .map_err(RulesetError::Parse)?;

        let mut builder = GlobSetBuilder::new();
        let mut rules = Vec::with_capacity(doc.rule.len());

        for (i, entry) in doc.rule.iter().enumerate() {
            let glob = Glob::new(&entry.glob)
                .map_err(|e| RulesetError::Glob(i, entry.glob.clone(), e))?;
            builder.add(glob);

            let durability = match entry.durability.as_deref() {
                Some("ephemeral") => Durability::Ephemeral,
                Some("local") => Durability::Local,
                Some("durable") | None => Durability::Durable,
                Some(other) => return Err(RulesetError::BadValue(
                    i, "durability", other.to_string(),
                )),
            };

            let readahead = match entry.readahead.as_deref() {
                Some("off") => ReadAhead::Off,
                Some("auto") | None => ReadAhead::Auto,
                Some("aggressive") => ReadAhead::Aggressive,
                Some(other) => return Err(RulesetError::BadValue(
                    i, "readahead", other.to_string(),
                )),
            };

            let coalesce = entry.coalesce_ms
                .map(|ms| Duration::from_millis(ms));

            rules.push(HandleRules { durability, readahead, coalesce });
        }

        let matcher = builder.build()
            .map_err(RulesetError::Build)?;

        Ok(Self { matcher, rules })
    }

    /// Resolve the rules for a path. Last matching rule wins
    /// (TOML declaration order). Returns default rules if no
    /// rule matches.
    pub fn resolve(&self, path: &str) -> HandleRules {
        let matches = self.matcher.matches(path);
        match matches.last() {
            Some(&idx) => self.rules[idx],
            None => HandleRules::default(),
        }
    }

    /// Number of compiled rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// True if no rules are loaded.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// TOML deserialization shape.
#[derive(serde::Deserialize)]
struct TomlRuleset {
    #[serde(default)]
    rule: Vec<TomlRule>,
}

#[derive(serde::Deserialize)]
struct TomlRule {
    glob: String,
    durability: Option<String>,
    readahead: Option<String>,
    coalesce_ms: Option<u64>,
}

/// Errors from ruleset loading / compilation.
#[derive(Debug)]
pub enum RulesetError {
    Io(std::path::PathBuf, std::io::Error),
    Parse(toml::de::Error),
    Glob(usize, String, globset::Error),
    Build(globset::Error),
    BadValue(usize, &'static str, String),
}

impl std::fmt::Display for RulesetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(p, e) => write!(f, "ruleset file {}: {}", p.display(), e),
            Self::Parse(e) => write!(f, "ruleset TOML parse: {}", e),
            Self::Glob(i, pat, e) => write!(f, "rule[{}] glob {:?}: {}", i, pat, e),
            Self::Build(e) => write!(f, "globset build: {}", e),
            Self::BadValue(i, field, val) => {
                write!(f, "rule[{}] bad {}: {:?}", i, field, val)
            }
        }
    }
}

impl std::error::Error for RulesetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_ruleset_returns_defaults() {
        let rs = Ruleset::empty();
        let rules = rs.resolve("usr/bin/bash");
        assert_eq!(rules.durability, Durability::Durable);
        assert_eq!(rules.readahead, ReadAhead::Auto);
        assert!(rules.coalesce.is_none());
        assert!(rs.is_empty());
    }

    #[test]
    fn parse_basic_rules() {
        let toml = r#"
[[rule]]
glob = "**/.npm/_cacache/**"
durability = "ephemeral"
readahead = "off"

[[rule]]
glob = "**/node_modules/**"
durability = "local"
coalesce_ms = 250
"#;
        let rs = Ruleset::parse(toml).unwrap();
        assert_eq!(rs.len(), 2);

        let r1 = rs.resolve("root/.npm/_cacache/content-v2/sha512/abc");
        assert_eq!(r1.durability, Durability::Ephemeral);
        assert_eq!(r1.readahead, ReadAhead::Off);
        assert!(r1.coalesce.is_none());

        let r2 = rs.resolve("root/project/node_modules/express/index.js");
        assert_eq!(r2.durability, Durability::Local);
        assert_eq!(r2.coalesce, Some(Duration::from_millis(250)));
    }

    #[test]
    fn last_match_wins() {
        let toml = r#"
[[rule]]
glob = "**/node_modules/**"
durability = "local"

[[rule]]
glob = "**/node_modules/.cache/**"
durability = "ephemeral"
"#;
        let rs = Ruleset::parse(toml).unwrap();

        // .cache/** matches BOTH rules; last wins → ephemeral
        let r = rs.resolve("root/project/node_modules/.cache/babel/x.js");
        assert_eq!(r.durability, Durability::Ephemeral);

        // Regular node_modules file matches only first → local
        let r = rs.resolve("root/project/node_modules/express/index.js");
        assert_eq!(r.durability, Durability::Local);
    }

    #[test]
    fn no_match_returns_default() {
        let toml = r#"
[[rule]]
glob = "**/.git/objects/**"
durability = "durable"
readahead = "aggressive"
"#;
        let rs = Ruleset::parse(toml).unwrap();

        let r = rs.resolve("root/src/main.rs");
        assert_eq!(r.durability, Durability::Durable);
        assert_eq!(r.readahead, ReadAhead::Auto);
    }

    #[test]
    fn bad_durability_value_errors() {
        let toml = r#"
[[rule]]
glob = "**"
durability = "yolo"
"#;
        let err = Ruleset::parse(toml).unwrap_err();
        assert!(err.to_string().contains("bad durability"));
    }

    #[test]
    fn bad_glob_errors() {
        let toml = r#"
[[rule]]
glob = "[invalid"
"#;
        let err = Ruleset::parse(toml).unwrap_err();
        assert!(err.to_string().contains("glob"));
    }

    #[test]
    fn empty_toml_is_valid() {
        let rs = Ruleset::parse("").unwrap();
        assert!(rs.is_empty());
    }

    #[test]
    fn all_readahead_modes() {
        let toml = r#"
[[rule]]
glob = "**/random/**"
readahead = "off"

[[rule]]
glob = "**/stream/**"
readahead = "aggressive"

[[rule]]
glob = "**/normal/**"
readahead = "auto"
"#;
        let rs = Ruleset::parse(toml).unwrap();
        assert_eq!(rs.resolve("x/random/y").readahead, ReadAhead::Off);
        assert_eq!(rs.resolve("x/stream/y").readahead, ReadAhead::Aggressive);
        assert_eq!(rs.resolve("x/normal/y").readahead, ReadAhead::Auto);
    }
}
