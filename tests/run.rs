//! `ck run` failure semantics, each case driven through separate processes.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
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
    // The filler's command is running, so it holds the lock.
    wait_until(|| s.runs() == 2);

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
    let flags = ["--ttl", "1s", "--stale", "1h"];
    s.run(&flags);
    sleep(PAST_TTL);
    // The stored run is now servable as stale; --refresh must not serve it.
    s.set("out", "new\n");
    assert_eq!(
        stdout(&s.run(&["--ttl", "1s", "--stale", "1h", "--refresh"])),
        "new\n"
    );

    sleep(PAST_TTL);
    s.set("exit", "4");
    s.set("out", "failure output\n");
    let out = s.run(&["--ttl", "1s", "--stale", "1h", "--refresh"]);
    assert_eq!((code(&out), stdout(&out)), (4, "failure output\n".into()));
    assert_eq!(s.runs(), 3);
}

#[test]
fn a_success_clears_the_marker_even_when_not_stored() {
    let s = Sandbox::new();
    s.set("exit", "1");
    assert_eq!(code(&s.run(&[])), 1);
    // A success too large to store must still end the backoff.
    s.set("exit", "0");
    s.set("out", &"x".repeat(21 * 1024 * 1024));
    assert_eq!(code(&s.run(&["--refresh"])), 0);
    s.set("out", "small\n");
    let out = s.run(&[]);
    assert_eq!(
        (code(&out), stdout(&out)),
        (0, "small\n".into()),
        "{}",
        stderr(&out)
    );
    assert_eq!(s.runs(), 3);
}

#[test]
fn a_corrupt_entry_is_a_miss_and_never_served() {
    let s = Sandbox::new();
    s.run(&["--ttl", "1h", "--stale", "1h"]);
    for name in entries(&s.data_dir()) {
        let path = s.data_dir().join(name);
        let mut bytes = fs::read(&path).unwrap();
        // Past the 14-byte file header: flip a ciphertext byte.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, bytes).unwrap();
    }
    s.set("out", "rerun\n");
    let out = s.run(&["--ttl", "1h", "--stale", "1h"]);
    assert_eq!(
        (code(&out), stdout(&out)),
        (0, "rerun\n".into()),
        "{}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("cache is unavailable"),
        "{}",
        stderr(&out)
    );
    // A miss refills the entry; a backend error would have run uncached.
    assert_eq!(stdout(&s.run(&["--ttl", "1h", "--stale", "1h"])), "rerun\n");
    assert_eq!(s.runs(), 2);
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
    let ready = s.home().join("ready");
    let script = format!("touch '{}'; exec sleep 5", ready.display());
    let child = s
        .ck(&["run", "--ttl", "1h", "--", "sh", "-c", &script])
        .spawn()
        .unwrap();
    wait_until(|| ready.exists());
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
    wait_until(|| s.runs() == 1);
    // The shim has 0.6 s of sleep left from here.
    let started = Instant::now();
    // Only ck gets it, as if the child ignored the terminal's interrupt.
    signal(&child, rustix::process::Signal::INT);
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 128 + 2);
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "ck did not wait for the child"
    );
    assert_eq!(stdout(&out), "", "an interrupted run printed its output");

    s.unset("sleep");
    s.run(&["--ttl", "1h"]);
    assert_eq!(s.runs(), 2, "the interrupted run was cached");
}

#[test]
fn a_waiter_blocked_on_the_lock_exits_on_a_signal() {
    let s = Sandbox::new();
    s.set("sleep", "3");
    let holder = s.spawn(&["--ttl", "1h"]);
    // The holder's command is running, so it holds the lock.
    wait_until(|| s.runs() == 1);
    let waiter = s.spawn(&["--ttl", "1h"]);
    wait_handlers_installed(&waiter);
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

#[test]
fn a_process_group_interrupt_stores_nothing() {
    // ck and the command both get the signal, as from Ctrl-C at a terminal.
    // ck must never read the dead child as an ordinary failure: that would
    // serve stale with exit 0 and set a marker that suppresses later calls.
    let s = Sandbox::new();
    let flags = ["run", "--ttl", "1s", "--stale", "1h", "--", "origin"];
    assert_eq!(code(&s.ck(&flags).output().unwrap()), 0);
    sleep(PAST_TTL);
    s.set("sleep", "5");
    for round in 0..40 {
        let child = s.ck(&flags).spawn().unwrap();
        // The shim records its call before it sleeps.
        wait_until(|| s.runs() == round + 2);
        let group = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
        rustix::process::kill_process_group(group, rustix::process::Signal::INT).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(code(&out), 128 + 2, "round {round}: {}", stderr(&out));
    }
    assert_eq!(entries(&s.data_dir()).len(), 1, "a marker was stored");
}

#[test]
fn a_signal_while_writing_output_stops_ck() {
    let s = Sandbox::new();
    // More than a pipe buffer, into a stdout nobody reads: ck blocks writing.
    let child = s
        .ck(&["run", "--", "head", "-c", "1000000", "/dev/zero"])
        .spawn()
        .unwrap();
    wait_handlers_installed(&child);
    // Give it time to fill the pipe and block writing.
    sleep(Duration::from_millis(200));
    signal(&child, rustix::process::Signal::TERM);
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 128 + 15);
    assert!(entries(&s.data_dir()).is_empty(), "the run was cached");
}

#[test]
fn a_closed_reader_ends_a_streamed_command() {
    use std::io::Read;
    let s = Sandbox::new();
    let mut child = s.ck(&["run", "--", "yes"]).spawn().unwrap();
    let mut head = [0u8; 10];
    child.stdout.take().unwrap().read_exact(&mut head).unwrap();
    // The read end is dropped here, as `| head -c 10` would.
    wait_exit(
        &mut child,
        Duration::from_secs(10),
        "ck kept draining a closed pipe",
    );
    // The command died of SIGPIPE because the reader left: not a failure.
    assert!(entries(&s.data_dir()).is_empty(), "a marker was stored");
}

#[test]
fn a_signal_ignored_by_the_caller_stays_ignored() {
    let s = Sandbox::new();
    s.set("sleep", "0.6");
    // Borrow the sandbox's environment (HOME, PATH, no CACHEKIT_*).
    let sandbox = s.ck(&[]);
    // nohup ignores SIGHUP, then execs ck in the same process.
    let ck = std::path::Path::new(common::BIN);
    let mut nohup = Command::new("nohup");
    nohup.arg(ck).args(["run", "--", "origin"]);
    for (k, v) in sandbox.get_envs() {
        match v {
            Some(v) => nohup.env(k, v),
            None => nohup.env_remove(k),
        };
    }
    let child = nohup
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The shim records its call after ck has installed its handlers.
    wait_until(|| s.runs() == 1);
    signal(&child, rustix::process::Signal::HUP);
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        (code(&out), stdout(&out)),
        (0, "hello\n".into()),
        "{}",
        stderr(&out)
    );
    assert_eq!(entries(&s.data_dir()).len(), 1, "the run was not cached");
}

/// `ck <args>` started with SIGCHLD ignored, as a parent can leave it.
/// Shells cannot do this (dash's `trap '' CHLD` keeps the default), so the
/// test sets the disposition itself between fork and exec.
#[allow(unsafe_code)]
fn with_sigchld_ignored(mut cmd: Command) -> std::process::Output {
    use std::os::unix::process::CommandExt;
    // SAFETY: only the async-signal-safe `signal` runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            Ok(())
        });
    }
    cmd.output().unwrap()
}

#[test]
fn an_inherited_ignored_sigchld_does_not_fake_a_failure() {
    let s = Sandbox::new();
    for call in 1..=2 {
        let out = with_sigchld_ignored(s.ck(&["run", "--", "origin"]));
        assert_eq!(
            (code(&out), stdout(&out)),
            (0, "hello\n".into()),
            "call {call}: {}",
            stderr(&out)
        );
    }
    assert_eq!(s.runs(), 1, "the first run was not cached");

    // The command itself still inherits SIGCHLD ignored.
    #[cfg(target_os = "linux")]
    {
        let out = with_sigchld_ignored(s.ck(&["run", "--", "grep", "SigIgn", "/proc/self/status"]));
        let mask = stdout(&out);
        let bits =
            u64::from_str_radix(mask.trim().trim_start_matches("SigIgn:").trim(), 16).unwrap();
        assert_ne!(
            bits & (1 << (libc::SIGCHLD - 1)),
            0,
            "SIGCHLD is not ignored in the command: {mask}"
        );
    }
}

#[test]
fn an_interrupt_the_command_traps_still_stores_nothing() {
    // terraform-style: the command traps SIGINT and exits 1 instead of dying
    // of it, so only ck's own handler can tell interrupted from failed.
    let s = Sandbox::new();
    let flag = s.home().join("slow");
    let ready = s.home().join("ready");
    let script = format!(
        "trap 'exit 1' INT; [ -f '{0}' ] && touch '{1}' && sleep 5; echo hi",
        flag.display(),
        ready.display()
    );
    let args = [
        "run", "--ttl", "1s", "--stale", "1h", "--", "sh", "-c", &script,
    ];
    assert_eq!(code(&s.ck(&args).output().unwrap()), 0);
    sleep(PAST_TTL);
    fs::write(&flag, "").unwrap();
    for round in 0..20 {
        let _ = fs::remove_file(&ready);
        let child = s.ck(&args).spawn().unwrap();
        // Signal only once the trap is set.
        wait_until(|| ready.exists());
        let group = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
        rustix::process::kill_process_group(group, rustix::process::Signal::INT).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(code(&out), 128 + 2, "round {round}: {}", stderr(&out));
    }
    assert_eq!(entries(&s.data_dir()).len(), 1, "a marker was stored");
}

#[test]
fn a_command_that_signals_itself_is_a_failure() {
    // ck received nothing, so a command dead of its own signal failed: the
    // stale value is served with exit 0 and a marker is set.
    // One forwarded signal and one that is not: the rule is the same for all.
    for sig in ["TERM", "INT"] {
        let s = Sandbox::new();
        let flags = ["--ttl", "1s", "--stale", "1h"];
        s.run(&flags);
        sleep(PAST_TTL);
        s.set("selfkill", sig);
        let out = s.run(&flags);
        assert_eq!(
            (code(&out), stdout(&out)),
            (0, "hello\n".into()),
            "{sig}: {}",
            stderr(&out)
        );
        assert_eq!(entries(&s.data_dir()).len(), 2, "{sig}: no marker was set");
    }
}

#[test]
fn sigterm_stops_ck_when_only_a_background_job_holds_stdout() {
    let s = Sandbox::new();
    let ready = s.home().join("ready");
    // The signal lands while the command is alive; it then dies of it,
    // leaving a background job holding its stdout.
    let script = format!("sleep 8 & touch '{}'; sleep 20", ready.display());
    let mut child = s
        .ck(&["run", "--", "sh", "-c", &script])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(|| ready.exists());
    signal(&child, rustix::process::Signal::TERM);
    // Time ck's own exit: the background sleep still holds the pipes.
    let status = wait_exit(
        &mut child,
        Duration::from_secs(3),
        "ck waited for the background job",
    );
    assert_eq!(status.code(), Some(128 + 15));
    assert!(
        entries(&s.data_dir()).is_empty(),
        "the interrupted run was stored"
    );
}

#[test]
fn a_fifo_file_key_exits_125_without_hanging() {
    let s = Sandbox::new();
    fs::create_dir_all(s.config_dir()).unwrap();
    let path = s.config_dir().join("file.key");
    // The mkfifo utility: rustix has no mkfifoat on Apple targets.
    assert!(Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut child = s.ck(&["run", "--", "origin"]).spawn().unwrap();
    wait_exit(
        &mut child,
        Duration::from_secs(5),
        "ck hung opening a FIFO key",
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!((code(&out), s.runs()), (125, 0), "{}", stderr(&out));
}

/// Poll `ready` until it holds, failing the test after 5 s.
fn wait_until(ready: impl Fn() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for readiness"
        );
        sleep(Duration::from_millis(5));
    }
}

/// Every thread but main blocks the handled signals, so their handlers run
/// on the main thread. Linux routes a group signal to the leader anyway, so
/// only the masks themselves can show a helper thread that does not block.
#[cfg(target_os = "linux")]
#[test]
fn helper_threads_block_the_handled_signals() {
    let s = Sandbox::new();
    s.set("sleep", "2");
    let mut child = s.spawn(&["--ttl", "1h"]);
    wait_until(|| s.runs() == 1);
    let pid = child.id();
    let mask_of = |tid: &str| {
        let status = fs::read_to_string(format!("/proc/{pid}/task/{tid}/status")).unwrap();
        let line = status.lines().find(|l| l.starts_with("SigBlk:")).unwrap();
        u64::from_str_radix(line["SigBlk:".len()..].trim(), 16).unwrap()
    };
    // SIGHUP 1, SIGINT 2, SIGQUIT 3, SIGTERM 15.
    let handled = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 14);
    let mut helpers = 0;
    for task in fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let tid = task.unwrap().file_name().into_string().unwrap();
        let mask = mask_of(&tid);
        if tid == pid.to_string() {
            assert_eq!(mask & handled, 0, "the main thread blocks a handled signal");
        } else {
            assert_eq!(
                mask & handled,
                handled,
                "thread {tid} does not block every handled signal"
            );
            helpers += 1;
        }
    }
    // The signal thread and tokio's blocking-pool thread.
    assert!(helpers >= 2, "found {helpers} helper threads");
    child.kill().unwrap();
    let _ = child.wait();
}

/// Run `script` under ck in its own process group, wait for its ready file,
/// send `first` to the group (as Ctrl-C does) or to ck alone, then `then` to
/// ck alone, as a supervisor would. Returns ck's exit code and how long it
/// took after the second signal.
fn two_signals(
    script: &str,
    first: rustix::process::Signal,
    to_group: bool,
    then: rustix::process::Signal,
) -> (Option<i32>, Duration) {
    let s = Sandbox::new();
    let ready = s.home().join("ready");
    let script = script.replace("READY", &ready.display().to_string());
    let mut child = s
        .ck(&["run", "--", "sh", "-c", &script])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(|| ready.exists());
    let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    if to_group {
        rustix::process::kill_process_group(pid, first).unwrap();
    } else {
        rustix::process::kill_process(pid, first).unwrap();
    }
    sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the first signal alone ended ck"
    );
    let started = Instant::now();
    rustix::process::kill_process(pid, then).unwrap();
    let status = wait_exit(
        &mut child,
        Duration::from_secs(5),
        "the second signal was not forwarded",
    );
    assert!(
        entries(&s.data_dir()).is_empty(),
        "the interrupted run was stored"
    );
    (status.code(), started.elapsed())
}

#[test]
fn a_term_after_a_trapped_interrupt_is_still_forwarded() {
    use rustix::process::Signal;
    // The command ignores the terminal's interrupt; the supervisor's TERM
    // must still reach it. The exit code names the first signal.
    let (code, took) = two_signals(
        "trap '' INT; touch 'READY'; sleep 10 & wait",
        Signal::INT,
        true,
        Signal::TERM,
    );
    assert_eq!(code, Some(128 + 2));
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[test]
fn a_second_term_reaches_a_command_that_trapped_the_first() {
    use rustix::process::Signal;
    let (code, took) = two_signals(
        "trap 'trap - TERM' TERM; touch 'READY'; sleep 10 & wait; wait",
        Signal::TERM,
        false,
        Signal::TERM,
    );
    assert_eq!(code, Some(128 + 15));
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[test]
fn a_signal_after_the_command_exited_stops_ck() {
    let s = Sandbox::new();
    let ready = s.home().join("ready");
    // The command exits at once; only its background job holds stdout.
    let script = format!("sleep 8 & echo hi; touch '{}'", ready.display());
    let mut child = s
        .ck(&["run", "--", "sh", "-c", &script])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(|| ready.exists());
    // Let the shell finish exiting after the touch.
    sleep(Duration::from_millis(200));
    signal(&child, rustix::process::Signal::TERM);
    let status = wait_exit(
        &mut child,
        Duration::from_secs(3),
        "ck waited for the background job",
    );
    assert_eq!(status.code(), Some(128 + 15));
    assert!(
        entries(&s.data_dir()).is_empty(),
        "the interrupted run was stored"
    );
}

/// Wait for `child` to exit, failing the test with `msg` after `limit`.
/// Before failing it kills the child's whole process group, so ck, its
/// command and any background job all go: `Sandbox::ck` makes ck a group
/// leader.
fn wait_exit(
    child: &mut std::process::Child,
    limit: Duration,
    msg: &str,
) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() >= limit {
            // Never leave a failing test's processes running.
            if let Some(group) = rustix::process::Pid::from_raw(child.id() as i32) {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("{msg}");
        }
        sleep(Duration::from_millis(20));
    }
}

/// Wait until ck has installed its SIGTERM handler, so a signal sent now is
/// ck's to handle. Linux shows it in `SigCgt`; elsewhere, wait a moment.
fn wait_handlers_installed(child: &std::process::Child) {
    #[cfg(target_os = "linux")]
    wait_until(|| {
        let status = fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap_or_default();
        status
            .lines()
            .find_map(|l| l.strip_prefix("SigCgt:"))
            .and_then(|m| u64::from_str_radix(m.trim(), 16).ok())
            .is_some_and(|m| m & (1 << (libc::SIGTERM - 1)) != 0)
    });
    #[cfg(not(target_os = "linux"))]
    {
        let _ = child;
        sleep(Duration::from_millis(300));
    }
}

/// Repeated signals to a command that ignores them add no threads to ck.
#[cfg(target_os = "linux")]
#[test]
fn repeated_signals_start_one_waiter() {
    let s = Sandbox::new();
    let ready = s.home().join("ready");
    let script = format!("trap '' TERM; touch '{}'; sleep 3", ready.display());
    let mut child = s
        .ck(&["run", "--", "sh", "-c", &script])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(|| ready.exists());
    let threads = || {
        fs::read_dir(format!("/proc/{}/task", child.id()))
            .unwrap()
            .count()
    };
    let baseline = threads();
    signal(&child, rustix::process::Signal::TERM);
    // The first signal starts exactly one waiter.
    wait_until(|| threads() == baseline + 1);
    let after_one = threads();
    for _ in 0..50 {
        signal(&child, rustix::process::Signal::TERM);
        sleep(Duration::from_millis(5));
    }
    sleep(Duration::from_millis(100));
    assert_eq!(
        threads(),
        after_one,
        "repeated signals started more threads"
    );
    let status = wait_exit(&mut child, Duration::from_secs(10), "ck did not exit");
    assert_eq!(status.code(), Some(128 + 15));
}
