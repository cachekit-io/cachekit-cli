//! Every `sh` example in the README runs as written. Those that use
//! `--backend saas` run in tests/saas.rs, against a fake CacheKit.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use common::{code, readme_sh_blocks, stderr, Sandbox, BIN};

#[test]
fn readme_examples_run() {
    let examples = readme_sh_blocks();
    assert!(
        examples.len() >= 4,
        "found only {} examples",
        examples.len()
    );

    let s = Sandbox::new();
    // A stand-in for the 1Password CLI, which CI does not have.
    let op = s.bin().join("op");
    fs::write(&op, "#!/bin/sh\necho stand-in-secret\n").unwrap();
    fs::set_permissions(&op, fs::Permissions::from_mode(0o755)).unwrap();
    let ck_dir = Path::new(BIN).parent().unwrap();

    for example in examples {
        // tests/saas.rs runs these against its fake CacheKit.
        if example.contains("--backend saas") {
            continue;
        }
        let path = std::env::join_paths(
            [s.bin().as_path(), ck_dir]
                .into_iter()
                .map(Path::to_path_buf)
                .chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                )),
        )
        .unwrap();
        let out = Command::new("sh")
            .args(["-euc", &example])
            .env("HOME", s.home())
            .env_remove("CK_LOG")
            .env("PATH", path)
            // A pipe, as in CI, not /dev/null: the examples carry no redirect.
            .stdin(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!((code(&out), stderr(&out)), (0, String::new()), "{example}");
    }
}
