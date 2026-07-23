//! CLI-surface tests that spawn the built `rustyweb` binary (Cargo exposes its
//! path as `CARGO_BIN_EXE_rustyweb`).

use std::process::Command;

/// `index` requires `--collection`: every crawl belongs to a collection, so a
/// bare `index <wacz>` must fail with guidance (exit 2), not invent a singleton.
#[test]
fn index_requires_a_collection() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rustyweb"))
        .args(["index", "some.wacz"])
        .arg("--home")
        .arg(tmp.path())
        .output()
        .unwrap();

    assert!(!out.status.success(), "bare index should fail");
    assert_eq!(out.status.code(), Some(2), "exit code 2 for a usage error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--collection"),
        "stderr should tell the user to pass --collection: {stderr}"
    );
}
