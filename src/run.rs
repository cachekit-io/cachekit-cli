//! `ck run`: the control flow.
//!
//! A fresh value always wins, and the marker is read only when no fresh value
//! exists, so a failure that lands its marker after another process's success
//! is inert while the value is fresh.

use std::fs;
use std::ops::ControlFlow;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{FlockOperation, Mode, OFlags};

use crate::child::{Ran, Stdout, Supervisor};
use crate::cli::{BackendKind, RunArgs};
use crate::entry::{human, now_ms, Envelope, Freshness, Marker};
use crate::keys::{self, KeyHasher};
use crate::saas::{self, Connect};
use crate::store::{ReadError, Store};
use crate::{debug, emit, warn, Fatal};

pub fn run(args: &RunArgs, connect: &Connect) -> Result<i32, Fatal> {
    let started = Instant::now();
    let debug = crate::debug_enabled();
    // A saas call without its credentials exits 125 before anything runs.
    let credentials = match args.backend {
        BackendKind::File => None,
        BackendKind::Saas => Some(saas::credentials()?),
    };

    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Fatal("HOME is not set".into()))?;
    let cache_dir = home.join(".cache/ck");
    let (store, master_key) = match credentials {
        None => {
            let master_key = keys::master_key(&home.join(".config/ck"))?;
            (
                Store::open_file(&cache_dir.join("data"), &master_key)?,
                master_key,
            )
        }
        Some(c) => (
            Store::open_saas(connect(&c.api_key)?, &c.master_key)?,
            c.master_key,
        ),
    };
    let key = KeyHasher::new(&master_key)?.value_key(&args.scope, &args.command);

    // The fill lock is host-local on every backend.
    let call = Call {
        args,
        marker_key: keys::marker_key(&key),
        lock_path: cache_dir.join("locks").join(keys::key_hash(&key)),
        key,
        store,
        debug,
        started,
    };
    if args.refresh {
        return call.refresh();
    }
    call.cached()
}

struct Call<'a> {
    args: &'a RunArgs,
    store: Store,
    key: String,
    marker_key: String,
    lock_path: PathBuf,
    /// `CK_LOG=debug`: each outcome reports itself through [`Call::done`].
    debug: bool,
    started: Instant,
}

/// A stored run that may be replayed because the command failed.
struct Stale {
    envelope: Envelope,
    age_ms: u64,
}

impl Call<'_> {
    fn cached(&self) -> Result<i32, Fatal> {
        let value = match self.store.value(&self.key) {
            Ok(v) => v,
            Err(e) => return self.read_failed(e, &Supervisor::install()?),
        };
        let stale = match self.fresh_or_stale(value) {
            ControlFlow::Break((fresh, age_ms)) => {
                emit(&fresh);
                return Ok(self.done("served fresh", Some(age_ms), 0));
            }
            ControlFlow::Continue(stale) => stale,
        };

        let supervisor = Supervisor::install()?;
        // From here a signal is ck's to answer. Each return below that serves
        // stored output or is suppressed first checks for one already
        // received, so it is never ignored or answered alongside output.
        // Another process holding the lock is filling. Serve what we have
        // rather than wait; with nothing servable, wait for its result.
        let lock = match fill_lock(&self.lock_path, stale.is_none()) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                let stale = stale.unwrap_or_else(|| {
                    unreachable!("only a call with stale output skips the wait")
                });
                // The marker only adds to the stderr line, so a failed read
                // still serves stale: the lock holder is running the command.
                // The cause joins that one line.
                let (marker, unavailable) = match self.store.marker(&self.marker_key) {
                    Ok(m) => (m, None),
                    Err(ReadError::Local(fatal)) => return Err(fatal),
                    Err(ReadError::Backend(e)) => (None, Some(e)),
                };
                if let Some(code) = supervisor.interrupted() {
                    return Ok(code);
                }
                return Ok(self.serve_stale(&stale, marker.as_ref(), None, unavailable));
            }
            Err(e) => {
                warn(&format!(
                    "cannot take the fill lock {}: {e}; running uncached",
                    self.lock_path.display()
                ));
                return Ok(self.run_uncached(&supervisor));
            }
        };

        // Holding the lock: whoever held it before may have filled the value.
        // A fresh value wins even if the marker read failed.
        let (value, marker) = self.store.value_and_marker(&self.key, &self.marker_key);
        let value = match value {
            Ok(v) => v,
            Err(e) => return self.read_failed(e, &supervisor),
        };
        let stale = match self.fresh_or_stale(value) {
            ControlFlow::Break((fresh, age_ms)) => {
                if let Some(code) = supervisor.interrupted() {
                    return Ok(code);
                }
                emit(&fresh);
                return Ok(self.done("served fresh", Some(age_ms), 0));
            }
            ControlFlow::Continue(stale) => stale,
        };
        let marker = match marker {
            Ok(m) => m,
            Err(e) => return self.read_failed(e, &supervisor),
        };
        let now = now_ms();
        if let Some(m) = marker.filter(|m| m.is_active(now)) {
            if let Some(code) = supervisor.interrupted() {
                return Ok(code);
            }
            if let Some(stale) = stale {
                return Ok(self.serve_stale(&stale, Some(&m), None, None));
            }
            warn(&format!(
                "not running the command: its last run exited {}; next retry in {}. Use --refresh to run it now",
                m.exit,
                human(m.retry_at_ms - now)
            ));
            return Ok(self.done("suppressed", None, m.exit));
        }

        let code = self.record(&supervisor, stale, marker);
        drop(lock);
        Ok(code)
    }

    /// `--refresh`: run now, whatever the value, the marker and the lock say.
    /// Never serves stale. The marker is read only to continue its count.
    fn refresh(&self) -> Result<i32, Fatal> {
        let supervisor = Supervisor::install()?;
        let marker = match self.store.marker(&self.marker_key) {
            Ok(m) => m,
            Err(e) => return self.read_failed(e, &supervisor),
        };
        Ok(self.record(&supervisor, None, marker))
    }

    /// Run the command and record the outcome: the value on success, the
    /// bumped marker on failure. `marker` is the current one, if any.
    fn record(&self, supervisor: &Supervisor, stale: Option<Stale>, marker: Option<Marker>) -> i32 {
        let age_ms = stale.as_ref().map(|s| s.age_ms);
        let ran = if self.args.refresh {
            "refreshed"
        } else {
            "ran"
        };
        let not_stored = format!("{ran}, not stored");
        let (code, stdout) = match supervisor.run(&self.args.command, Some(self.store.max_output()))
        {
            Ran::SpawnFailed { code } => {
                // The command never started, so the origin was not reached:
                // no marker is set or bumped. `Supervisor::run` has already
                // turned a received signal into `Interrupted`.
                return match stale {
                    Some(stale) => self.serve_stale(&stale, None, None, None),
                    None => self.done(NOT_STARTED, None, code),
                };
            }
            Ran::Interrupted { signal } => return 128 + signal,
            Ran::StatusUnknown { stdout } => {
                return match (stale, stdout) {
                    (Some(stale), Stdout::Captured(_)) => {
                        self.serve_stale(&stale, None, None, None)
                    }
                    (_, stdout) => {
                        stdout.emit_captured();
                        self.done(&not_stored, age_ms, 126)
                    }
                };
            }
            Ran::Exited { code, stdout } => (code, stdout),
        };

        let now = now_ms();
        if code == 0 {
            let envelope = match &stdout {
                Stdout::Captured(out) => {
                    // Print before the write-back, so a pipeline sees output at once.
                    emit(out);
                    Some(Envelope::encode(now, out))
                }
                _ => None,
            };
            let ttl = Duration::from_secs(self.args.ttl_secs + self.args.stale_secs);
            // A success ends the backoff, whether or not its output is stored.
            let (stored, cleared) = self.store.record_success(
                &self.key,
                envelope.as_deref().map(|e| (e, ttl)),
                &self.marker_key,
            );
            let outcome = match (&envelope, &stored) {
                (Some(_), Ok(())) => format!("{ran} and stored"),
                _ => not_stored,
            };
            // One warning: a failed write and a failed clear usually share a cause.
            match (stored, cleared) {
                (Err(e), Err(_)) => warn(&format!(
                    "the command succeeded but its output was not cached: {e}; \
                     any recorded failure still stands, so the next call may not run it"
                )),
                (Err(e), Ok(())) => warn(&format!(
                    "the command succeeded but its output was not cached: {e}"
                )),
                (Ok(()), Err(e)) => warn(&format!("cannot clear the recorded failure: {e}")),
                (Ok(()), Ok(())) => {}
            }
            return self.done(&outcome, age_ms, 0);
        }

        if matches!(stdout, Stdout::OutputLost) {
            // ck's stdout failed mid-stream and the command most likely died
            // of SIGPIPE: no evidence about the origin, so no marker.
            return self.done(&not_stored, age_ms, code);
        }
        let next = Marker::bumped(marker.as_ref(), code, now);
        if let Err(e) = self
            .store
            .set(&self.marker_key, &next.encode(), next.backend_ttl(now))
        {
            warn(&format!("cannot record the failure: {e}"));
        }
        match (stdout, stale) {
            (Stdout::Captured(_), Some(stale)) => {
                self.serve_stale(&stale, Some(&next), Some(code), None)
            }
            (stdout, _) => {
                stdout.emit_captured();
                self.done(&not_stored, age_ms, code)
            }
        }
    }

    /// Break with a fresh value to print and its age, or continue with the
    /// stale run (if `--stale` still allows one) for the caller to fall back on.
    fn fresh_or_stale(
        &self,
        value: Option<Envelope>,
    ) -> ControlFlow<(Vec<u8>, u64), Option<Stale>> {
        let Some(envelope) = value else {
            return ControlFlow::Continue(None);
        };
        let now = now_ms();
        match envelope.freshness(now, self.args.ttl_secs, self.args.stale_secs) {
            Freshness::Fresh => {
                let age_ms = now.saturating_sub(envelope.stored_at_ms);
                ControlFlow::Break((envelope.stdout, age_ms))
            }
            Freshness::Stale { age_ms } => ControlFlow::Continue(Some(Stale { envelope, age_ms })),
            Freshness::Expired => ControlFlow::Continue(None),
        }
    }

    /// Replay a stored exit-0 run with exit 0 and one stderr line, which ends
    /// with `unavailable`, the backend error, when there was one.
    fn serve_stale(
        &self,
        stale: &Stale,
        marker: Option<&Marker>,
        failed_now: Option<i32>,
        unavailable: Option<String>,
    ) -> i32 {
        let now = now_ms();
        let mut line = match failed_now {
            Some(code) => format!(
                "the command exited {code}; serving its output from {} ago",
                human(stale.age_ms)
            ),
            None => format!("serving output from {} ago", human(stale.age_ms)),
        };
        if let Some(m) = marker.filter(|m| m.is_active(now)) {
            if failed_now.is_none() {
                line.push_str(&format!("; the last run exited {}", m.exit));
            }
            line.push_str(&format!("; next retry in {}", human(m.retry_at_ms - now)));
        }
        if let Some(e) = unavailable {
            line.push_str(&format!("; the cache is unavailable ({e})"));
        }
        warn(&line);
        emit(&stale.envelope.stdout);
        self.done("served stale", Some(stale.age_ms), 0)
    }

    /// Any backend error before the run disables caching for this call, never
    /// the command. No further backend calls are made.
    fn read_failed(&self, e: ReadError, supervisor: &Supervisor) -> Result<i32, Fatal> {
        match e {
            ReadError::Local(fatal) => Err(fatal),
            ReadError::Backend(e) => {
                warn(&format!("the cache is unavailable ({e}); running uncached"));
                Ok(self.run_uncached(supervisor))
            }
        }
    }

    fn run_uncached(&self, supervisor: &Supervisor) -> i32 {
        match supervisor.run(&self.args.command, None) {
            Ran::SpawnFailed { code } => self.done(NOT_STARTED, None, code),
            Ran::Exited { code, .. } => self.done("ran uncached", None, code),
            Ran::StatusUnknown { .. } => self.done("ran uncached", None, 126),
            Ran::Interrupted { signal } => 128 + signal,
        }
    }

    /// Return `code`, first saying on stderr what the call did when
    /// `CK_LOG=debug`. Every outcome passes through here exactly once; an
    /// interrupted run or a ck error does not.
    ///
    /// The line names only the program: its arguments, its output and both
    /// keys can carry secrets. The entry id is the start of the keyed hash
    /// that names the fill lock, so a guessed command line cannot be checked
    /// against it without the master key.
    fn done(&self, outcome: &str, age_ms: Option<u64>, code: i32) -> i32 {
        if !self.debug {
            return code;
        }
        let backend = match self.args.backend {
            BackendKind::File => "file",
            BackendKind::Saas => "saas",
        };
        // Debug-quoted, so control characters cannot drive the terminal.
        let program = self.args.command[0].to_string_lossy();
        let entry = &keys::key_hash(&self.key)[..ENTRY_ID_LEN];
        let age = age_ms.map_or(String::new(), |ms| format!(" age={ms}ms"));
        debug(&format!(
            "{outcome}: backend={backend} program={program:?} entry={entry}{age} exit={code} elapsed={}ms",
            self.started.elapsed().as_millis()
        ));
        code
    }
}

/// Hex characters of the key hash that identify an entry in a debug line.
const ENTRY_ID_LEN: usize = 12;

const NOT_STARTED: &str = "could not start the command";

/// Take the fill lock, an exclusive `flock` on the key's lockfile. When it is
/// contended, wait for it only if `wait`; otherwise return `None`. flock is
/// released when its holder exits or dies, so no lock goes stale.
///
/// Fill locks live outside the backend's directory and are never unlinked:
/// unlinking a lockfile races a waiter that has already opened it.
fn fill_lock(path: &Path, wait: bool) -> std::io::Result<Option<OwnedFd>> {
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    // CLOEXEC keeps the lock out of the child, so a command that leaves a
    // daemon behind does not hold it after ck exits.
    let flags = OFlags::CREATE | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let lock = rustix::fs::open(path, flags, Mode::from_raw_mode(0o600))?;
    match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => return Ok(Some(lock)),
        Err(rustix::io::Errno::WOULDBLOCK) if !wait => return Ok(None),
        Err(rustix::io::Errno::WOULDBLOCK) => {}
        Err(e) => return Err(e.into()),
    }
    loop {
        match rustix::fs::flock(&lock, FlockOperation::LockExclusive) {
            Ok(()) => return Ok(Some(lock)),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}
