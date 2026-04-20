//! Integration tests for orchestrator VM lifecycle.
//!
//! Tests call orchestrator methods directly (no IPC socket).
//! Gated on S3 availability via tcp probe.

use std::net::TcpStream;
use std::time::Duration;

use orchestrator::{DaemonConfig, Orchestrator, VmSpec, VmState};

/// Skip if the S3 endpoint isn't reachable.
fn require_s3() -> bool {
    TcpStream::connect_timeout(
        &"192.168.88.15:9090".parse().unwrap(),
        Duration::from_millis(500),
    ).is_ok()
}

fn test_config() -> DaemonConfig {
    let mut cfg = DaemonConfig::defaults();
    cfg.log_dir = std::env::temp_dir().join("microd-test-logs");
    cfg.ephemeral_root = std::env::temp_dir().join("microd-test-ephemeral");
    cfg
}

fn test_spec(name: &str) -> VmSpec {
    VmSpec {
        name: name.to_string(),
        mem_mb: 128,
        kernel: None,
        initrd: None,
        bucket: None,
        golden_bucket: None,
        ruleset: None,
        ephemeral_dir: None,
        net_ifname: None,
    }
}

#[test]
fn spawn_two_vms_distinct_resources() {
    if !require_s3() {
        eprintln!("SKIPPED: S3 not reachable");
        return;
    }

    let orch = Orchestrator::new(test_config()).unwrap();

    let info_a = orch.start_vm(test_spec("alpha")).unwrap();
    let info_b = orch.start_vm(test_spec("bravo")).unwrap();

    // Both should be Starting or Running.
    assert!(
        info_a.state == VmState::Starting || info_a.state == VmState::Running,
        "alpha state: {:?}", info_a.state,
    );
    assert!(
        info_b.state == VmState::Starting || info_b.state == VmState::Running,
        "bravo state: {:?}", info_b.state,
    );

    // Distinct name hashes.
    assert_ne!(info_a.name_hash, info_b.name_hash);

    // Both in list.
    let list = orch.list_vms();
    assert!(list.len() >= 2, "list should have at least 2 VMs");
    assert!(list.iter().any(|v| v.name == "alpha"));
    assert!(list.iter().any(|v| v.name == "bravo"));

    // Distinct buckets.
    let a_bucket = format!("vm-{}", info_a.name_hash);
    let b_bucket = format!("vm-{}", info_b.name_hash);
    assert_ne!(a_bucket, b_bucket);

    // Clean up — stop both. May fail if they crashed (no kernel),
    // that's OK for lifecycle test.
    let _ = orch.stop_vm("alpha");
    let _ = orch.stop_vm("bravo");
}

#[test]
fn stop_transitions_state() {
    if !require_s3() {
        eprintln!("SKIPPED: S3 not reachable");
        return;
    }

    let orch = Orchestrator::new(test_config()).unwrap();
    orch.start_vm(test_spec("charlie")).unwrap();

    // Give thread a moment to start.
    std::thread::sleep(Duration::from_millis(100));

    // Stop it.
    orch.stop_vm("charlie").unwrap();

    let info = orch.inspect_vm("charlie").unwrap();
    assert!(
        info.state == VmState::Stopped || info.state == VmState::Crashed,
        "charlie should be Stopped or Crashed after stop, got {:?}", info.state,
    );
}

#[test]
fn relaunch_reuses_bucket() {
    if !require_s3() {
        eprintln!("SKIPPED: S3 not reachable");
        return;
    }

    let orch = Orchestrator::new(test_config()).unwrap();

    // First launch.
    let info1 = orch.start_vm(test_spec("delta")).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let _ = orch.stop_vm("delta");

    // Second launch — same name.
    let info2 = orch.start_vm(test_spec("delta")).unwrap();

    // Same name_hash → same bucket.
    assert_eq!(info1.name_hash, info2.name_hash);

    let _ = orch.stop_vm("delta");
}

#[test]
fn already_running_rejected() {
    if !require_s3() {
        eprintln!("SKIPPED: S3 not reachable");
        return;
    }

    let orch = Orchestrator::new(test_config()).unwrap();
    orch.start_vm(test_spec("echo")).unwrap();

    // Second start with same name should fail.
    let result = orch.start_vm(test_spec("echo"));
    assert!(result.is_err(), "duplicate start should be rejected");

    let _ = orch.stop_vm("echo");
}
