use std::process::Command;

#[test]
fn binary_runs() {
    let out = Command::new(env!("CARGO_BIN_EXE_cachekit-cli"))
        .output()
        .expect("failed to spawn the binary");
    assert!(
        out.status.success(),
        "exit {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}
