//! On-demand tap device allocation (research/15 v0 fix 2).
//!
//! Allocates tap devices via `ip link` commands. Requires
//! CAP_NET_ADMIN or root. Operators configure this via
//! `sudo microd` or `setcap cap_net_admin+ep` on the binary.

use std::io;
use std::process::Command;

/// Clean up stale tap devices from a previous microd run.
///
/// Scans for interfaces matching our naming convention (`tap-`
/// followed by 8 hex chars) and deletes them. Runs once at daemon
/// startup before the IPC server accepts VM spawns. Each removal
/// is logged; failures are non-fatal (log and continue).
pub fn cleanup_stale() {
    let output = match Command::new("ip").args(["-o", "link", "show"]).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("tap::cleanup_stale: ip link show failed: {}", e);
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // ip -o link show format: "N: name: <FLAGS> ..."
        // Extract the interface name (second field, strip trailing colon).
        let name = match line.split_whitespace().nth(1) {
            Some(n) => n.trim_end_matches(':'),
            None => continue,
        };

        if is_our_tap(name) {
            eprintln!("tap::cleanup_stale: removing {}", name);
            release(name);
        }
    }
}

/// Check if an interface name matches our convention: tap-<8 hex chars>.
fn is_our_tap(name: &str) -> bool {
    if let Some(suffix) = name.strip_prefix("tap-") {
        suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_hexdigit())
    } else {
        false
    }
}

/// Allocate a tap device named `tap-<hash_prefix>`.
/// Creates the device and brings it up.
pub fn allocate(name_hash: &str) -> io::Result<String> {
    let tap_name = format!("tap-{}", &name_hash[..8.min(name_hash.len())]);

    // Create the tap device.
    let output = Command::new("ip")
        .args(["link", "add", &tap_name, "type", "tap"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // "RTNETLINK answers: File exists" means it's already there — reuse.
        if !stderr.contains("File exists") {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("ip link add {} failed: {}", tap_name, stderr.trim()),
            ));
        }
    }

    // Bring it up.
    let output = Command::new("ip")
        .args(["link", "set", "dev", &tap_name, "up"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("ip link set {} up failed: {}", tap_name, stderr.trim()),
        ));
    }

    eprintln!("tap::allocate: {}", tap_name);
    Ok(tap_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_our_tap_matches_convention() {
        assert!(is_our_tap("tap-644a9bc5"));
        assert!(is_our_tap("tap-0ad42b94"));
        assert!(is_our_tap("tap-abcdef01"));
    }

    #[test]
    fn is_our_tap_rejects_others() {
        assert!(!is_our_tap("tap0"));          // not our format
        assert!(!is_our_tap("tapdev1"));       // different tool
        assert!(!is_our_tap("tap-toolong9"));  // 9 chars, not 8
        assert!(!is_our_tap("tap-short"));     // not hex
        assert!(!is_our_tap("eth0"));          // unrelated
        assert!(!is_our_tap("tap-GHIJKLMN"));  // not hex
    }
}

/// Release a tap device. Best-effort — log and swallow on failure.
pub fn release(tap_name: &str) {
    match Command::new("ip")
        .args(["link", "del", tap_name])
        .output()
    {
        Ok(output) if output.status.success() => {
            eprintln!("tap::release: {}", tap_name);
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("tap::release: {} failed (non-critical): {}", tap_name, stderr.trim());
        }
        Err(e) => {
            eprintln!("tap::release: {} failed (non-critical): {}", tap_name, e);
        }
    }
}
