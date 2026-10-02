//! The hit-path budget: `ck run -- true` may add at most 5 ms over `true`,
//! against a cache directory holding about 10,000 entries.
//!
//! The populated directory matters: the file backend scans its whole
//! directory each time it is opened, and nothing evicts unread entries.
//! Ignored by default because it needs a release build to mean anything:
//! `cargo test --release --test bench -- --ignored --nocapture`.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::Sandbox;

const ENTRIES: usize = 10_000;
const ROUNDS: usize = 300;
const BUDGET: Duration = Duration::from_millis(5);

#[test]
#[ignore = "run in release mode by CI"]
fn hit_path_adds_at_most_5ms() {
    let s = Sandbox::new();
    let hit = || {
        s.ck(&["run", "--ttl", "1d", "--", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
    };
    assert!(hit().success());
    populate(&s);
    assert!(fs::read_dir(s.data_dir()).unwrap().count() > ENTRIES);

    let time = |cmd: &mut Command| {
        let started = Instant::now();
        assert!(cmd.status().unwrap().success());
        started.elapsed()
    };
    let mut ck = Vec::with_capacity(ROUNDS);
    let mut bare = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        ck.push(time(
            s.ck(&["run", "--ttl", "1d", "--", "true"])
                .stdout(Stdio::null()),
        ));
        bare.push(time(Command::new("true").stdin(Stdio::null())));
    }
    let (ck, bare) = (median(ck), median(bare));
    let added = ck.saturating_sub(bare);
    println!("hit path: ck {ck:?}, true {bare:?}, added {added:?} (budget {BUDGET:?})");
    assert!(
        added <= BUDGET,
        "ck added {added:?} on a hit, over the {BUDGET:?} budget"
    );
}

/// Entries in the file backend's on-disk format: a 14-byte header (magic,
/// version, flags, expiry) and a payload. Contents do not matter; the scan
/// cost is per directory entry.
fn populate(s: &Sandbox) {
    let mut header = [0u8; 14];
    header[..3].copy_from_slice(b"CK\x01");
    header[6..].copy_from_slice(&4_102_444_800u64.to_be_bytes());
    for i in 0..ENTRIES {
        let path = s.data_dir().join(format!("{i:032x}"));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        f.write_all(&header).unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    }
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}
