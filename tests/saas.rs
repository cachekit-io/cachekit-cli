//! `ck run --backend saas` against a fake backend with injected latency and
//! faults.
//!
//! The cachekit-rs client refuses loopback and private addresses, so a local
//! HTTP stub cannot stand in for the service. Instead this test binary plays
//! two roles. Started with `CK_TEST_FAKE_SAAS` set, it is `ck`: the real
//! `cachekit_cli::main_with`, with the CacheKit client swapped for [`Fake`].
//! Started by `cargo test`, it drives the cases below, each running that `ck`
//! as separate processes in a sandbox. So every call is a real process with
//! its own stdin, stdout, signals and exit code, and only the network is fake.
//!
//! The fake keeps its entries in a directory, so calls from different
//! sandboxes ("hosts") share them, and appends every call it receives to a
//! log.

mod common;

use std::fs;
use std::io::Write;
use std::panic;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cachekit::backend::file::FileBackend;
use cachekit::backend::{Backend, HealthStatus};
use cachekit::{BackendError, BackendErrorKind};
use cachekit_cli::keys::KeyHasher;

use common::{code, entries, readme_sh_blocks, stderr, stdout, Sandbox, MASTER_KEY_HEX};

/// The fake's directory. Set, this binary is `ck`.
const FAKE: &str = "CK_TEST_FAKE_SAAS";
/// Milliseconds the fake waits before answering each call.
const LATENCY: &str = "CK_TEST_FAKE_LATENCY_MS";
/// `<op>=<fault>`, where op is get, set, delete or `*`, and fault is an HTTP
/// status, `timeout` (the client's own timeout error), `hang` (never
/// answers), or `hang-blocking` (never answers, holding a blocking thread, as
/// a stuck DNS lookup does).
const FAULT: &str = "CK_TEST_FAKE_FAULT";

const CASES: &[(&str, fn())] = &[
    ("cross_host_hit", cross_host_hit),
    (
        "a_failure_backs_off_other_hosts",
        a_failure_backs_off_other_hosts,
    ),
    ("outage_adds_at_most_1s", || {
        outage_adds_at_most_1s("*=hang")
    }),
    ("outage_on_a_stuck_lookup_adds_at_most_1s", || {
        outage_adds_at_most_1s("*=hang-blocking")
    }),
    (
        "slow_but_healthy_fills_and_hits",
        slow_but_healthy_fills_and_hits,
    ),
    (
        "a_401_on_the_value_read_runs_uncached",
        a_401_on_the_value_read_runs_uncached,
    ),
    (
        "a_client_timeout_is_an_outage_not_a_miss",
        a_client_timeout_is_an_outage_not_a_miss,
    ),
    (
        "a_failed_write_after_the_run_warns_once",
        a_failed_write_after_the_run_warns_once,
    ),
    ("a_corrupt_entry_is_a_miss", a_corrupt_entry_is_a_miss),
    (
        "saas_without_a_master_key_exits_125",
        saas_without_a_master_key_exits_125,
    ),
    ("unusable_api_keys_exit_125", unusable_api_keys_exit_125),
    (
        "an_api_key_alone_never_selects_saas",
        an_api_key_alone_never_selects_saas,
    ),
    ("readme_saas_examples_run", readme_saas_examples_run),
];

fn main() {
    if let Some(dir) = std::env::var_os(FAKE) {
        let dir = PathBuf::from(dir);
        let code = cachekit_cli::main_with(std::env::args_os().skip(1), &move |_api_key| {
            Ok(Box::new(Fake::open(&dir)))
        });
        std::process::exit(code);
    }

    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let mut failed = Vec::new();
    for (name, case) in CASES {
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        println!("test {name} ...");
        if panic::catch_unwind(case).is_err() {
            failed.push(*name);
        }
    }
    if !failed.is_empty() {
        println!("FAILED: {}", failed.join(", "));
        std::process::exit(101);
    }
    println!("test result: ok");
}

// ── The fake backend ─────────────────────────────────────────────────────────

struct Fake {
    entries: FileBackend,
    log: PathBuf,
    latency: Duration,
    faults: String,
}

impl Fake {
    fn open(dir: &Path) -> Self {
        let ms = std::env::var(LATENCY).map_or(0, |v| v.parse().unwrap());
        Self {
            entries: FileBackend::builder()
                .cache_dir(dir.join("entries"))
                .build()
                .unwrap(),
            log: dir.join("log"),
            latency: Duration::from_millis(ms),
            faults: std::env::var(FAULT).unwrap_or_default(),
        }
    }

    /// Log the call, wait out the latency, then inject the fault, if any.
    async fn answer(&self, op: &str, key: &str) -> Result<(), BackendError> {
        let what = if key.ends_with(":neg") {
            "marker"
        } else {
            "value"
        };
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        log.write_all(format!("{op} {what}\n").as_bytes()).unwrap();
        tokio::time::sleep(self.latency).await;

        let fault = self.faults.split(',').find_map(|f| {
            let (on, fault) = f.split_once('=')?;
            (on == op || on == "*").then_some(fault)
        });
        match fault {
            None => Ok(()),
            Some("hang") => std::future::pending().await,
            Some("hang-blocking") => {
                let _ = tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_secs(30)))
                    .await;
                unreachable!("the deadline cuts this off")
            }
            Some("timeout") => Err(BackendError {
                kind: BackendErrorKind::Timeout,
                message: "operation timed out".into(),
                source: None,
            }),
            Some(status) => Err(BackendError::from_http_status(
                status.parse().unwrap(),
                b"injected",
            )),
        }
    }
}

#[async_trait]
impl Backend for Fake {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BackendError> {
        self.answer("get", key).await?;
        self.entries.get(key).await
    }

    async fn set(
        &self,
        key: &str,
        value: Vec<u8>,
        ttl: Option<Duration>,
    ) -> Result<(), BackendError> {
        self.answer("set", key).await?;
        self.entries.set(key, value, ttl).await
    }

    async fn delete(&self, key: &str) -> Result<bool, BackendError> {
        self.answer("delete", key).await?;
        self.entries.delete(key).await
    }

    async fn exists(&self, _key: &str) -> Result<bool, BackendError> {
        unreachable!("ck never calls exists")
    }

    async fn health(&self) -> Result<HealthStatus, BackendError> {
        unreachable!("ck never calls health")
    }
}

// ── Driving it ───────────────────────────────────────────────────────────────

/// The shared service: the fake's directory, seen by every host.
struct Service(tempfile::TempDir);

impl Service {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }

    /// `ck run --backend saas <flags> -- origin` on `host`, with both keys set.
    fn ck(&self, host: &Sandbox, flags: &[&str]) -> Command {
        let mut args = vec!["run", "--backend", "saas"];
        args.extend_from_slice(flags);
        args.extend(["--", "origin"]);
        let mut cmd = self.bare(host, &args);
        cmd.env("CACHEKIT_API_KEY", "ck_sdk_test")
            .env("CACHEKIT_MASTER_KEY", MASTER_KEY_HEX);
        cmd
    }

    /// This binary as `ck <args>` on `host`, with no CacheKit variables.
    fn bare(&self, host: &Sandbox, args: &[&str]) -> Command {
        let me = std::env::current_exe().unwrap();
        let mut cmd = host.command(&me, args);
        cmd.env(FAKE, self.0.path())
            .env_remove(LATENCY)
            .env_remove(FAULT);
        cmd
    }

    fn run(&self, host: &Sandbox, flags: &[&str]) -> Output {
        self.ck(host, flags).output().unwrap()
    }

    /// The calls the fake received, one `<op> <value|marker>` per line.
    fn calls(&self) -> Vec<String> {
        let log = fs::read_to_string(self.0.path().join("log")).unwrap_or_default();
        log.lines().map(str::to_owned).collect()
    }

    fn clear_calls(&self) {
        let _ = fs::remove_file(self.0.path().join("log"));
    }
}

fn lines(out: &Output) -> Vec<String> {
    stderr(out).lines().map(str::to_owned).collect()
}

/// How long the origin shim takes on its own, to subtract from ck's time.
fn origin_alone(host: &Sandbox) -> Duration {
    let started = Instant::now();
    let out = Command::new(host.bin().join("origin")).output().unwrap();
    assert!(out.status.code().is_some());
    host.unset("calls");
    started.elapsed()
}

// ── Cases ────────────────────────────────────────────────────────────────────

fn cross_host_hit() {
    let service = Service::new();
    let (laptop, runner) = (Sandbox::new(), Sandbox::new());

    let out = service
        .ck(&laptop, &["--ttl", "1h"])
        .current_dir(laptop.home())
        .env("HOSTNAME", "laptop")
        .output()
        .unwrap();
    assert_eq!(
        (code(&out), stdout(&out)),
        (0, "hello\n".into()),
        "{}",
        stderr(&out)
    );
    assert_eq!(laptop.runs(), 1);

    service.clear_calls();
    let out = service
        .ck(&runner, &["--ttl", "1h"])
        .current_dir(runner.home())
        .env("HOSTNAME", "ci-runner")
        .output()
        .unwrap();
    assert_eq!(
        (code(&out), stdout(&out), stderr(&out)),
        (0, "hello\n".into(), String::new())
    );
    assert_eq!(runner.runs(), 0, "the second host ran the command");
    assert_eq!(service.calls(), ["get value"]);
}

fn a_failure_backs_off_other_hosts() {
    let service = Service::new();
    let (a, b) = (Sandbox::new(), Sandbox::new());
    a.set("exit", "4");
    assert_eq!(code(&service.run(&a, &[])), 4);

    // b's origin would succeed, but the shared marker backs it off.
    let out = service.run(&b, &[]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("next retry in"), "{}", stderr(&out));
    assert_eq!(b.runs(), 0);
}

fn outage_adds_at_most_1s(fault: &str) {
    let service = Service::new();
    let host = Sandbox::new();
    host.set("exit", "3");
    let baseline = origin_alone(&host);

    let started = Instant::now();
    let out = service.ck(&host, &[]).env(FAULT, fault).output().unwrap();
    let added = started.elapsed().saturating_sub(baseline);

    assert_eq!((code(&out), stdout(&out)), (3, "hello\n".into()));
    let warnings = lines(&out);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("no answer within 1s"), "{warnings:?}");
    assert_eq!(host.runs(), 1);
    assert!(added < Duration::from_millis(1_300), "ck added {added:?}");
    // The first timeout ends the backend's part of the call.
    assert_eq!(service.calls(), ["get value"]);
}

fn slow_but_healthy_fills_and_hits() {
    let service = Service::new();
    let host = Sandbox::new();
    let baseline = origin_alone(&host);

    let started = Instant::now();
    let out = service
        .ck(&host, &["--ttl", "1h"])
        .env(LATENCY, "900")
        .output()
        .unwrap();
    let added = started.elapsed().saturating_sub(baseline);
    assert_eq!(
        (code(&out), stdout(&out), stderr(&out)),
        (0, "hello\n".into(), String::new())
    );
    // Five calls in three round trips: the post-lock reads go out together,
    // and so do the post-run write and clear.
    let mut calls = service.calls();
    calls.sort();
    assert_eq!(
        calls,
        [
            "delete marker",
            "get marker",
            "get value",
            "get value",
            "set value"
        ]
    );
    assert!(
        added < Duration::from_millis(3_400),
        "the cold fill added {added:?}"
    );

    let out = service
        .ck(&host, &["--ttl", "1h"])
        .env(LATENCY, "900")
        .output()
        .unwrap();
    assert_eq!(
        (code(&out), stdout(&out), stderr(&out)),
        (0, "hello\n".into(), String::new())
    );
    assert_eq!(host.runs(), 1, "the second call did not hit");
}

fn a_401_on_the_value_read_runs_uncached() {
    for exit in ["0", "5"] {
        let service = Service::new();
        let host = Sandbox::new();
        host.set("exit", exit);
        let out = service
            .ck(&host, &[])
            .env(FAULT, "get=401")
            .output()
            .unwrap();

        assert_eq!(code(&out).to_string(), exit);
        assert_eq!(stdout(&out), "hello\n");
        let warnings = lines(&out);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        for fix in [
            "HTTP 401",
            "ck_sdk_",
            "ck_live_",
            "ck-run",
            "running uncached",
        ] {
            assert!(warnings[0].contains(fix), "no {fix:?} in {warnings:?}");
        }
        assert_eq!(host.runs(), 1);
        assert_eq!(service.calls(), ["get value"]);
    }
}

fn a_client_timeout_is_an_outage_not_a_miss() {
    // On the file backend a Timeout is a miss. On saas it is an outage: the
    // call warns and stores nothing.
    let service = Service::new();
    let host = Sandbox::new();
    let out = service
        .ck(&host, &[])
        .env(FAULT, "get=timeout")
        .output()
        .unwrap();
    assert_eq!((code(&out), stdout(&out)), (0, "hello\n".into()));
    let warnings = lines(&out);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("running uncached"), "{warnings:?}");
    assert_eq!(service.calls(), ["get value"]);
}

fn a_failed_write_after_the_run_warns_once() {
    let service = Service::new();
    let host = Sandbox::new();
    let out = service
        .ck(&host, &["--ttl", "1h"])
        .env(FAULT, "set=500,delete=500")
        .output()
        .unwrap();
    assert_eq!((code(&out), stdout(&out)), (0, "hello\n".into()));
    let warnings = lines(&out);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("not cached"), "{warnings:?}");

    // Nothing was stored, so the next call runs the command again.
    service.run(&host, &["--ttl", "1h"]);
    assert_eq!(host.runs(), 2);
}

fn a_corrupt_entry_is_a_miss() {
    let service = Service::new();
    let host = Sandbox::new();
    let key = KeyHasher::new(&hex::decode(MASTER_KEY_HEX).unwrap())
        .unwrap()
        .value_key(&"".into(), &["origin".into()]);
    let entries = FileBackend::builder()
        .cache_dir(service.0.path().join("entries"))
        .build()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime
        .block_on(entries.set(&key, vec![0; 64], None))
        .unwrap();

    let out = service.run(&host, &["--ttl", "1h"]);
    assert_eq!((code(&out), stdout(&out)), (0, "hello\n".into()));
    assert!(
        stderr(&out).contains("failed to decrypt"),
        "{}",
        stderr(&out)
    );
    assert_eq!(host.runs(), 1);
    // The run replaced the corrupt entry.
    let out = service.run(&host, &["--ttl", "1h"]);
    assert_eq!((code(&out), stderr(&out)), (0, String::new()));
    assert_eq!(host.runs(), 1);
}

fn saas_without_a_master_key_exits_125() {
    let service = Service::new();
    let host = Sandbox::new();
    // A file key exists, and saas still refuses to fall back to it.
    assert_eq!(code(&host.run(&[])), 0);
    assert!(host.config_dir().join("file.key").exists());

    let out = service
        .ck(&host, &[])
        .env_remove("CACHEKIT_MASTER_KEY")
        .output()
        .unwrap();
    assert_eq!(code(&out), 125, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("CACHEKIT_MASTER_KEY"),
        "{}",
        stderr(&out)
    );
    assert_eq!(host.runs(), 1, "the command ran");
    assert!(service.calls().is_empty());
}

fn unusable_api_keys_exit_125() {
    let service = Service::new();
    let host = Sandbox::new();
    for (key, says) in [
        (Some("ck_api_abc"), "ck_api_"),
        (Some(""), "CACHEKIT_API_KEY"),
        (None, "CACHEKIT_API_KEY"),
    ] {
        let mut cmd = service.ck(&host, &[]);
        match key {
            Some(k) => cmd.env("CACHEKIT_API_KEY", k),
            None => cmd.env_remove("CACHEKIT_API_KEY"),
        };
        // Stdin that would otherwise run the command uncached.
        let out = cmd.stdin(std::process::Stdio::piped()).output().unwrap();
        assert_eq!(code(&out), 125, "{key:?}: {}", stderr(&out));
        assert!(stderr(&out).contains(says), "{key:?}: {}", stderr(&out));
    }
    assert_eq!(host.runs(), 0);
    assert!(service.calls().is_empty());
}

fn an_api_key_alone_never_selects_saas() {
    let service = Service::new();
    let host = Sandbox::new();
    let out = service
        .bare(&host, &["run", "--", "origin"])
        .env("CACHEKIT_API_KEY", "ck_sdk_test")
        .env("CACHEKIT_MASTER_KEY", MASTER_KEY_HEX)
        .output()
        .unwrap();
    assert_eq!((code(&out), stdout(&out)), (0, "hello\n".into()));
    assert!(service.calls().is_empty(), "{:?}", service.calls());
    assert!(
        !entries(&host.data_dir()).is_empty(),
        "the file backend stored nothing"
    );
}

fn readme_saas_examples_run() {
    let examples: Vec<_> = readme_sh_blocks()
        .into_iter()
        .filter(|e| e.contains("--backend saas"))
        .collect();
    assert!(!examples.is_empty(), "the README has no saas example");

    let service = Service::new();
    let host = Sandbox::new();
    // `ck` on PATH is this binary, which is ck with the fake CacheKit.
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), host.bin().join("ck")).unwrap();
    for example in examples {
        let sh = || {
            host.command(Path::new("sh"), &["-euc", &example])
                .env(FAKE, service.0.path())
                .env("CACHEKIT_API_KEY", "ck_sdk_test")
                .env("CACHEKIT_MASTER_KEY", MASTER_KEY_HEX)
                .output()
                .unwrap()
        };
        let first = sh();
        assert_eq!(
            (code(&first), stderr(&first)),
            (0, String::new()),
            "{example}"
        );
        service.clear_calls();
        let second = sh();
        assert_eq!(
            (code(&second), stderr(&second)),
            (0, String::new()),
            "{example}"
        );
        assert_eq!(stdout(&second), stdout(&first), "{example}");
        assert_eq!(
            service.calls(),
            ["get value"],
            "the second run missed: {example}"
        );
    }
}
