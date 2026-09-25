use std::process::Command;

#[test]
fn binary_runs_and_prints_its_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_cachekit-cli"))
        .output()
        .expect("failed to spawn the binary");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("stdout is not UTF-8");
    assert_eq!(
        stdout.trim(),
        format!("cachekit-cli {}", env!("CARGO_PKG_VERSION"))
    );
}
