//! CLI flag → VmSpec conversion. Also handles short INI spec files
//! (research/15 §8: `microctl start --spec ./foo.ini`).

use std::path::PathBuf;

use orchestrator::ipc::proto::VmSpec;

/// Parse a minimal INI file into key=value pairs. Ignores [section]
/// headers and # comments. ~40 lines of hand-rolled parsing.
pub fn from_ini(path: &str) -> Result<VmSpec, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {}", path, e))?;

    let mut name = None;
    let mut mem_mb = 1024u32;
    let mut kernel = None;
    let mut initrd = None;
    let mut bucket = None;
    let mut golden_bucket = None;
    let mut ruleset = None;
    let mut ephemeral_dir = None;
    let mut net_ifname = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let (key, val) = line.split_once('=')
            .ok_or_else(|| format!("bad line (no =): {}", line))?;
        let key = key.trim();
        let val = val.trim();

        match key {
            "name" => name = Some(val.to_string()),
            "memory" | "mem_mb" => mem_mb = val.parse().map_err(|_| format!("bad memory: {}", val))?,
            "kernel" => kernel = Some(PathBuf::from(val)),
            "initrd" => initrd = Some(PathBuf::from(val)),
            "bucket" => bucket = Some(val.to_string()),
            "golden_bucket" => golden_bucket = Some(val.to_string()),
            "ruleset" => ruleset = Some(PathBuf::from(val)),
            "ephemeral_dir" => ephemeral_dir = Some(PathBuf::from(val)),
            "net_ifname" | "net" => net_ifname = Some(val.to_string()),
            _ => {} // ignore unknown keys
        }
    }

    let name = name.ok_or("missing 'name' in spec file")?;
    Ok(VmSpec { name, mem_mb, kernel, initrd, bucket, golden_bucket, ruleset, ephemeral_dir, net_ifname })
}
