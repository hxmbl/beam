//! Helpers for Beam's end-to-end tests in `tests/`: `loopback.rs` drives the
//! real binaries over plain TCP, `sshd_e2e.rs` through a real sshd.

use std::path::PathBuf;
use std::process::Command;

/// Directory holding the beam binaries, built on demand.
pub fn bin_dir() -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "-q",
            "-p",
            "beam-client",
            "-p",
            "beam-server-handler",
        ])
        .status()
        .expect("run cargo build");
    assert!(status.success(), "building beam binaries failed");
    // target/<profile>/deps/<test>-<hash> -> target/<profile>
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    assert!(
        dir.join("beam-client").exists(),
        "no beam-client in {dir:?}"
    );
    assert!(dir.join("beam-server-handler").exists());
    dir
}
