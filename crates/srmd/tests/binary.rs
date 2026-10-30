//! The tests that exercise `srmd` live with `srmctl`, which starts it. Cargo only
//! builds a package's binary for that package's own integration tests, so without
//! this file `cargo test --workspace` would run them against a stale daemon.

#[test]
fn the_daemon_reports_its_version() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_srmd"))
        .arg("--version")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.trim(), concat!("srmd ", env!("CARGO_PKG_VERSION")));
}
