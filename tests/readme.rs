//! Every `sh` example in the README runs as written.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use common::{code, stderr, Sandbox, BIN};

#[test]
fn readme_examples_run() {
    let readme = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    let examples = sh_blocks(&readme);
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
        assert!(
            example.contains("< /dev/null"),
            "an example without the redirect:\n{example}"
        );
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
            .env("PATH", path)
            .output()
            .unwrap();
        assert_eq!(code(&out), 0, "{example}\n{}", stderr(&out));
    }
}

fn sh_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        match (&mut current, line.trim_end()) {
            (None, "```sh") => current = Some(String::new()),
            (Some(block), "```") => {
                blocks.push(std::mem::take(block));
                current = None;
            }
            (Some(block), text) => {
                block.push_str(text);
                block.push('\n');
            }
            (None, _) => {}
        }
    }
    blocks
}
