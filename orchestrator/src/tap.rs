//! On-demand tap device allocation (research/15 v0 fix 2).
//!
//! Allocates tap devices via `ip link` commands. Requires
//! CAP_NET_ADMIN or root. Operators configure this via
//! `sudo microd` or `setcap cap_net_admin+ep` on the binary.

use std::io;
use std::process::Command;

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
