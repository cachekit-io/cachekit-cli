//! `ck run` failure semantics, each case driven through separate processes.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{code, entries, stderr, stdout, Sandbox, MASTER_KEY_HEX};

/// Long enough for a 1 s TTL to lapse.
const PAST_TTL: Duration = Duration::from_millis(1_100);

#[test]
fn a_fresh_value_is_served_without_running() {
    let s = Sandbox::new();
    assert_eq!(stdout(&s.run(&["--ttl", "1h"])), "hello\n");
    s.set("out", "changed\n");
    let out = s.run(&["--ttl", "1h"]);
    assert_eq!((code(&out), stdout(&out)), (0, "hello\n".into()));
    assert_eq!(s.runs(), 1);
}

#[test]
fn outage_with_a_stale_value_serves_stale() {
    let s = Sandbox::new();
    let flags = ["--ttl", "1s", "--stale", "1h"];
    assert_eq!(code(&s.run(&flags)), 0);
    sleep(PAST_TTL);
    s.set("exit", "1");
    s.set("out", "error page\n");

    for call in 1..=10 {
        let out = s.run(&flags);
        assert_eq!(code(&out), 0, "call {call}: {}", stderr(&out));
        assert_eq!(stdout(&out), "hello\n", "call {call}");
        assert_eq!(
            stderr(&out).lines().count(),
            1,
            "call {call}: {}",
            stderr(&out)
        );
    }
    // One run filled the value; the outage cost at most two more.
    assert!(
        s.runs() - 1 <= 2,
        "the command ran {} times during the outage",
        s.runs() - 1
    );
}

#[test]
fn outage_with_no_value_runs_at_most_twice() {
    let s = Sandbox::new();
    s.set("exit", "3");
    for call in 1..=10 {
        let out = s.run(&[]);
        assert_eq!(code(&out), 3, "call {call}");
    }
    assert!(s.runs() <= 2, "the command ran {} times", s.runs());
    let out = s.run(&[]);
    assert!(out.stdout.is_empty(), "a suppressed call writes no stdout");
    assert!(stderr(&out).contains("next retry in"), "{}", stderr(&out));
}

#[test]
fn concurrent_cold_calls_run_once() {
    let s = Sandbox::new();
    s.set("sleep", "0.5");
    let children: Vec<_> = (0..10).map(|_| s.spawn(&["--ttl", "1h"])).collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            (code(&out), stdout(&out)),
            (0, "hello\n".into()),
            "{}",
            stderr(&out)
        );
    }
    assert_eq!(s.runs(), 1);
}

#[test]
fn a_contended_lock_with_a_stale_value_serves_stale_at_once() {
    let s = Sandbox::new();
    let flags = ["--ttl", "1s", "--stale", "1h"];
    s.run(&flags);
    sleep(PAST_TTL);
    s.set("sleep", "2");
    s.set("out", "new\n");
    let filler = s.spawn(&flags);
    sleep(Duration::from_millis(300));

    let started = Instant::now();
    let out = s.run(&flags);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the second call waited on the lock"
    );
    assert_eq!((code(&out), stdout(&out)), (0, "hello\n".into()));
    assert_eq!(stdout(&filler.wait_with_output().unwrap()), "new\n");
}

#[test]
fn a_late_marker_is_inert_while_the_value_is_fresh() {
    let s = Sandbox::new();
    assert_eq!(code(&s.run(&["--ttl", "1h"])), 0);
    // A failure that lands its marker after the success.
    s.set("exit", "1");
    assert_eq!(code(&s.run(&["--ttl", "1h", "--refresh"])), 1);

    let out = s.run(&["--ttl", "1h"]);
    assert_eq!(
        (code(&out), stdout(&out), stderr(&out)),
        (0, "hello\n".into(), String::new())
    );
    assert_eq!(s.runs(), 2);
}

#[test]
fn a_spawn_failure_sets_no_marker() {
    let s = Sandbox::new();
    let missing = ["run", "--", "ck-test-not-installed"];
    let out = s.ck(&missing).output().unwrap();
    assert_eq!(code(&out), 127);
    assert!(
        stderr(&out).contains("command not found"),
        "{}",
        stderr(&out)
    );

    s.install_shim("ck-test-not-installed");
    let out = s.ck(&missing).output().unwrap();
    assert_eq!(
        (code(&out), stdout(&out)),
        (0, "hello\n".into()),
        "{}",
        stderr(&out)
    );
    assert_eq!(s.runs(), 1);
}

#[test]
fn a_child_that_exits_127_sets_a_marker() {
    let s = Sandbox::new();
    s.set("exit", "127");
    assert_eq!(code(&s.run(&[])), 127);
    let out = s.run(&[]);
    assert_eq!(code(&out), 127);
    assert!(
        stderr(&out).contains("not running the command"),
        "{}",
        stderr(&out)
    );
    assert_eq!(s.runs(), 1);
}

#[test]
fn a_marker_more_than_15_minutes_ahead_is_inactive() {
    use cachekit_cli::entry::{now_ms, Marker};
    use cachekit_cli::keys::{marker_key, KeyHasher};
    use cachekit_cli::store::Store;

    let write_marker = |s: &Sandbox, ahead_ms: u64| {
        let master = hex::decode(MASTER_KEY_HEX).unwrap();
        let key = KeyHasher::new(&master)
            .unwrap()
            .value_key(&"".into(), &["origin".into()]);
        let store = Store::open_file(&s.data_dir(), &master).unwrap();
        let marker = Marker {
            exit: 9,
            failures: 1,
            retry_at_ms: now_ms() + ahead_ms,
        };
        store
            .set(
                &marker_key(&key),
                &marker.encode(),
                Duration::from_secs(3600),
            )
            .unwrap();
    };
    let ck = |s: &Sandbox| {
        s.ck(&["run", "--", "origin"])
            .env("CACHEKIT_MASTER_KEY", MASTER_KEY_HEX)
            .output()
            .unwrap()
    };

    let s = Sandbox::new();
    write_marker(&s, 16 * 60 * 1000);
    let out = ck(&s);
    assert_eq!((code(&out), s.runs()), (0, 1), "{}", stderr(&out));

    // The control: inside the window, the same marker suppresses.
    let s = Sandbox::new();
    write_marker(&s, 14 * 60 * 1000);
    let out = ck(&s);
    assert_eq!((code(&out), s.runs()), (9, 0), "{}", stderr(&out));
}

#[test]
fn lowering_stale_stops_an_old_entry_being_served() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1s", "--stale", "1h"]);
    sleep(PAST_TTL);
    s.set("exit", "1");
    s.set("out", "error\n");

    let out = s.run(&["--ttl", "1s", "--stale", "1h"]);
    assert_eq!((code(&out), stdout(&out)), (0, "hello\n".into()));

    let out = s.run(&["--ttl", "1s", "--stale", "0s"]);
    assert_ne!(code(&out), 0);
    assert!(
        !stdout(&out).contains("hello"),
        "an entry past the reader's --stale was served"
    );
}

#[test]
fn only_dev_null_stdin_is_cached() {
    let file = tempfile::NamedTempFile::new().unwrap();
    // Each stdin comes with whatever must stay open while ck runs.
    let stdin_for = |kind: &str| -> (Stdio, Option<std::os::fd::OwnedFd>) {
        match kind {
            "a terminal" => {
                use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
                let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
                grantpt(&master).unwrap();
                unlockpt(&master).unwrap();
                let name = ptsname(&master, Vec::new()).unwrap();
                let slave = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(name.to_str().unwrap())
                    .unwrap();
                (Stdio::from(slave), Some(master))
            }
            "a pipe" => (Stdio::piped(), None),
            "a socket" => {
                let (a, b) = UnixStream::pair().unwrap();
                (Stdio::from(std::os::fd::OwnedFd::from(a)), Some(b.into()))
            }
            _ => (Stdio::from(fs::File::open(file.path()).unwrap()), None),
        }
    };
    let cases = ["a terminal", "a pipe", "a socket", "a file"];

    for kind in cases {
        let s = Sandbox::new();
        for call in 1..=2 {
            let (stdin, _keep) = stdin_for(kind);
            let out = s
                .ck(&["run", "--", "origin"])
                .stdin(stdin)
                .output()
                .unwrap();
            assert_eq!(code(&out), 0, "{kind}");
            let err = stderr(&out);
            assert_eq!(err.lines().count(), 1, "{kind}: {err}");
            assert!(
                err.contains(kind) && err.contains("< /dev/null"),
                "{kind}: {err}"
            );
            assert_eq!(s.runs(), call, "{kind}: the call was cached");
        }
        assert!(
            !s.home().join(".cache").exists(),
            "{kind}: an uncached call touched the cache"
        );
    }

    let s = Sandbox::new();
    s.run(&[]);
    let out = s.run(&[]);
    assert_eq!((stderr(&out), s.runs()), (String::new(), 1));
}

#[test]
fn the_child_sees_no_cachekit_variables() {
    let s = Sandbox::new();
    s.set("env", "");
    let out = s
        .ck(&["run", "--", "origin"])
        .env("CACHEKIT_MASTER_KEY", MASTER_KEY_HEX)
        .env("CACHEKIT_API_KEY", "ck_sdk_test")
        .env("CACHEKIT_ANYTHING", "x")
        .env("NOT_CACHEKIT_SURVIVES", "y")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0);
    let env = s.read("env.out");
    assert!(!env.lines().any(|l| l.starts_with("CACHEKIT_")), "{env}");
    assert!(env.contains("NOT_CACHEKIT_SURVIVES=y"));
}

#[test]
fn concurrent_first_runs_agree_on_one_file_key() {
    let s = Sandbox::new();
    s.set("sleep", "0.3");
    let children: Vec<_> = (0..10).map(|_| s.spawn(&["--ttl", "1h"])).collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert_eq!(code(&out), 0, "{}", stderr(&out));
    }
    // One key means one cache key, one fill lock and so one run.
    assert_eq!(s.runs(), 1);
    assert_eq!(entries(&s.config_dir()), ["file.key"]);
    let meta = fs::metadata(s.config_dir().join("file.key")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        fs::metadata(s.config_dir()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    s.run(&["--ttl", "1h"]);
    assert_eq!(s.runs(), 1);
}

#[test]
fn an_invalid_file_key_exits_125_and_is_left_untouched() {
    let valid = "ab".repeat(32);
    let cases = [
        ("not hex", "zz".repeat(32), 0o600),
        ("too short", "ab".repeat(31), 0o600),
        ("a trailing newline", format!("{valid}\n"), 0o600),
        ("mode 0644", valid.clone(), 0o644),
    ];
    for (why, content, mode) in cases {
        let s = Sandbox::new();
        fs::create_dir_all(s.config_dir()).unwrap();
        let path = s.config_dir().join("file.key");
        fs::write(&path, &content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();

        let out = s.run(&[]);
        assert_eq!(code(&out), 125, "{why}");
        assert!(
            stderr(&out).contains("Delete it"),
            "{why}: {}",
            stderr(&out)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), content, "{why}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            mode,
            "{why}"
        );
        assert_eq!(s.runs(), 0, "{why}");
    }
}

#[test]
fn an_unusable_cache_directory_exits_125() {
    let s = Sandbox::new();
    fs::create_dir_all(s.data_dir()).unwrap();
    fs::set_permissions(s.data_dir(), fs::Permissions::from_mode(0o777)).unwrap();
    let out = s.run(&[]);
    assert_eq!((code(&out), s.runs()), (125, 0));
    assert!(stderr(&out).contains("chmod 700"), "{}", stderr(&out));
}

#[test]
fn bad_flags_exit_125_before_anything_runs() {
    let s = Sandbox::new();
    for flags in [
        &["--ttl", "0s"][..],
        &["--ttl", "1.5h"],
        &["--stale", "1w"],
        &["--ttl", "20d", "--stale", "10d1s"],
        &["--ttl", "20d", "--stale", "864001s"],
        &["--backend", "saas"],
        &["--nope"],
    ] {
        let out = s.run(flags);
        assert_eq!(code(&out), 125, "{flags:?}");
    }
    assert_eq!(s.runs(), 0);
    assert!(!s.home().join(".cache").exists());
    assert_eq!(code(&s.run(&["--ttl", "20d", "--stale", "10d"])), 0);
}

#[test]
fn refresh_runs_and_never_serves_stale() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h", "--stale", "1h"]);
    s.set("out", "new\n");
    assert_eq!(stdout(&s.run(&["--ttl", "1h", "--refresh"])), "new\n");
    assert_eq!(stdout(&s.run(&["--ttl", "1h"])), "new\n");

    s.set("exit", "4");
    s.set("out", "failure output\n");
    let out = s.run(&["--ttl", "1h", "--stale", "1h", "--refresh"]);
    assert_eq!((code(&out), stdout(&out)), (4, "failure output\n".into()));
    assert_eq!(s.runs(), 3);
}

#[test]
fn output_past_20_mib_is_streamed_and_not_cached() {
    let s = Sandbox::new();
    let big = 21 * 1024 * 1024;
    let args = ["run", "--", "head", "-c", &big.to_string(), "/dev/zero"];
    for _ in 0..2 {
        let out = s.ck(&args).output().unwrap();
        assert_eq!((code(&out), out.stdout.len()), (0, big));
        assert!(stderr(&out).contains("20 MiB"), "{}", stderr(&out));
    }
}

fn signal(child: &std::process::Child, sig: rustix::process::Signal) {
    let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    rustix::process::kill_process(pid, sig).unwrap();
}

#[test]
fn sigterm_is_forwarded_and_nothing_is_stored() {
    let s = Sandbox::new();
    // `sleep` itself, not the shim: a shell killed mid-`sleep` leaves the
    // sleep holding stdout open, with or without ck.
    let child = s
        .ck(&["run", "--ttl", "1h", "--", "sleep", "5"])
        .spawn()
        .unwrap();
    sleep(Duration::from_millis(300));
    let started = Instant::now();
    signal(&child, rustix::process::Signal::TERM);
    let out = child.wait_with_output().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the child was not terminated"
    );
    assert_eq!(code(&out), 128 + 15);
    assert!(
        entries(&s.data_dir()).is_empty(),
        "the interrupted run was cached"
    );
}

#[test]
fn sigint_waits_for_the_child_and_stores_nothing() {
    let s = Sandbox::new();
    s.set("sleep", "0.6");
    let child = s.spawn(&["--ttl", "1h"]);
    sleep(Duration::from_millis(200));
    // Only ck gets it, as if the child ignored the terminal's interrupt.
    signal(&child, rustix::process::Signal::INT);
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 128 + 2);
    assert_eq!(stdout(&out), "hello\n", "ck did not wait for the child");

    s.unset("sleep");
    s.run(&["--ttl", "1h"]);
    assert_eq!(s.runs(), 2, "the interrupted run was cached");
}

#[test]
fn a_waiter_blocked_on_the_lock_exits_on_a_signal() {
    let s = Sandbox::new();
    s.set("sleep", "3");
    let holder = s.spawn(&["--ttl", "1h"]);
    sleep(Duration::from_millis(300));
    let waiter = s.spawn(&["--ttl", "1h"]);
    sleep(Duration::from_millis(300));
    let started = Instant::now();
    signal(&waiter, rustix::process::Signal::TERM);
    let out = waiter.wait_with_output().unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(code(&out), 128 + 15);
    assert_eq!(code(&holder.wait_with_output().unwrap()), 0);
}

#[test]
fn scope_and_argv_are_in_the_key() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h"]);
    s.run(&["--ttl", "1h", "--scope", "prod"]);
    s.run(&["--ttl", "1h", "--scope", "prod"]);
    s.ck(&["run", "--ttl", "1h", "--", "origin", "x"])
        .output()
        .unwrap();
    assert_eq!(s.runs(), 3);
}

#[test]
fn version_and_usage() {
    let s = Sandbox::new();
    let out = s.ck(&["--version"]).output().unwrap();
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).starts_with("ck "));
    assert_eq!(code(&s.ck(&[]).output().unwrap()), 125);
}
