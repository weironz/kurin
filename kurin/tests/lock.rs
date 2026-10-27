//! Cluster lock integration tests: cross-process contention and the
//! read-only command guarantee.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kurin::lock::ClusterLock;

fn state_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Child half of [`two_processes_contend_for_the_lock`]. It only does
/// anything when the parent sets `KURIN_LOCK_TEST_PATH`; the plain test run
/// (no env) is a no-op.
#[test]
fn lock_holder_child() {
    let Ok(path) = std::env::var("KURIN_LOCK_TEST_PATH") else {
        return;
    };
    let ready = PathBuf::from(std::env::var("KURIN_LOCK_TEST_READY").unwrap());
    let release = PathBuf::from(std::env::var("KURIN_LOCK_TEST_RELEASE").unwrap());
    let _guard = ClusterLock::acquire_at(&path, "tier1", "child-op").unwrap();
    fs::write(&ready, "ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !release.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn two_processes_contend_for_the_lock() {
    let dir = state_dir();
    let lock_path = dir.path().join("locks").join("tier1.lock");
    let ready = dir.path().join("ready");
    let release = dir.path().join("release");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_holder_child", "--nocapture"])
        .env("KURIN_LOCK_TEST_PATH", &lock_path)
        .env("KURIN_LOCK_TEST_READY", &ready)
        .env("KURIN_LOCK_TEST_RELEASE", &release)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lock holder");

    // Wait for the child to hold the lock.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "child never acquired the lock");

    let err = ClusterLock::acquire_at(&lock_path, "tier1", "parent-op").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("is busy"), "{msg}");
    assert!(
        msg.contains(&format!("pid {}", child.id())),
        "error must name the holder pid: {msg}"
    );
    assert!(
        msg.contains("child-op"),
        "error must name the holder op: {msg}"
    );
    assert!(
        msg.contains("lock_holder_child"),
        "error must include the holder command line: {msg}"
    );

    fs::write(&release, "go").unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "child failed: {status:?}");

    // The lock is released when the holder exits.
    let guard = ClusterLock::acquire_at(&lock_path, "tier1", "after").unwrap();
    assert_eq!(guard.info().op_id, "after");
}

#[test]
fn read_only_command_does_not_create_a_lock_file() {
    let dir = state_dir();
    let output = Command::new(env!("CARGO_BIN_EXE_kurin"))
        .args(["logs", "--cluster", "tier1"])
        .env("XDG_STATE_HOME", dir.path())
        .env("HOME", dir.path())
        .output()
        .expect("run kurin logs");
    // No logs exist, so the command fails — but it must not lock.
    assert!(!output.status.success());
    let lock = dir.path().join("kurin").join("locks").join("tier1.lock");
    assert!(
        !lock.exists(),
        "read-only `logs` must not create {}",
        lock.display()
    );
}
