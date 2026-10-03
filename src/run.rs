//! `ck run`: the control flow.
//!
//! A fresh value always wins, and the marker is read only when no fresh value
//! exists, so a failure that lands its marker after another process's success
//! is inert while the value is fresh.

use std::fs;
use std::io::IsTerminal;
use std::ops::ControlFlow;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{FlockOperation, Mode, OFlags};

use crate::child::{Ran, Stdout, Supervisor};
use crate::cli::RunArgs;
use crate::entry::{human, now_ms, Envelope, Freshness, Marker};
use crate::keys::{self, KeyHasher};
use crate::store::{ReadError, Store};
use crate::{emit, warn, Fatal};

pub fn run(args: &RunArgs) -> Result<i32, Fatal> {
    if let Some(kind) = uncacheable_stdin() {
        warn(&format!(
            "stdin is {kind}, so this call is not cached. End the command with `< /dev/null` to cache it"
        ));
        return Ok(run_uncached(&Supervisor::install()?, &args.command));
    }

    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Fatal("HOME is not set".into()))?;
    let master_key = keys::master_key(&home.join(".config/ck"))?;
    let cache_dir = home.join(".cache/ck");
    let store = Store::open_file(&cache_dir.join("data"), &master_key)?;
    let key = KeyHasher::new(&master_key)?.value_key(&args.scope, &args.command);

    let call = Call {
        args,
        marker_key: keys::marker_key(&key),
        lock_path: cache_dir.join("locks").join(keys::key_hash(&key)),
        key,
        store,
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
            ControlFlow::Break(fresh) => {
                emit(&fresh);
                return Ok(0);
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
                let marker = self.store.marker(&self.marker_key).ok().flatten();
                if let Some(code) = supervisor.interrupted() {
                    return Ok(code);
                }
                return Ok(self.serve_stale(&stale, marker.as_ref(), None));
            }
            Err(e) => {
                warn(&format!(
                    "cannot take the fill lock {}: {e}; running uncached",
                    self.lock_path.display()
                ));
                return Ok(run_uncached(&supervisor, &self.args.command));
            }
        };

        // Holding the lock: whoever held it before may have filled the value.
        let (value, marker) = match (
            self.store.value(&self.key),
            self.store.marker(&self.marker_key),
        ) {
            (Ok(v), Ok(m)) => (v, m),
            (Err(e), _) | (_, Err(e)) => return self.read_failed(e, &supervisor),
        };
        let stale = match self.fresh_or_stale(value) {
            ControlFlow::Break(fresh) => {
                if let Some(code) = supervisor.interrupted() {
                    return Ok(code);
                }
                emit(&fresh);
                return Ok(0);
            }
            ControlFlow::Continue(stale) => stale,
        };
        let now = now_ms();
        if let Some(m) = marker.filter(|m| m.is_active(now)) {
            if let Some(code) = supervisor.interrupted() {
                return Ok(code);
            }
            if let Some(stale) = stale {
                return Ok(self.serve_stale(&stale, Some(&m), None));
            }
            warn(&format!(
                "not running the command: its last run exited {}; next retry in {}. Use --refresh to run it now",
                m.exit,
                human(m.retry_at_ms - now)
            ));
            return Ok(m.exit);
        }

        let code = self.record(&supervisor, stale, marker);
        drop(lock);
        Ok(code)
    }

    /// `--refresh`: run now, whatever the value, the marker and the lock say.
    /// Never serves stale. The marker is read only to continue its count.
    fn refresh(&self) -> Result<i32, Fatal> {
        let supervisor = Supervisor::install()?;
        let marker = self.store.marker(&self.marker_key).ok().flatten();
        Ok(self.record(&supervisor, None, marker))
    }

    /// Run the command and record the outcome: the value on success, the
    /// bumped marker on failure. `marker` is the current one, if any.
    fn record(&self, supervisor: &Supervisor, stale: Option<Stale>, marker: Option<Marker>) -> i32 {
        let (code, stdout) = match supervisor.run(&self.args.command, true) {
            Ran::SpawnFailed { code } => {
                // The command never started, so the origin was not reached:
                // no marker is set or bumped. `Supervisor::run` has already
                // turned a received signal into `Interrupted`.
                return match stale {
                    Some(stale) => self.serve_stale(&stale, None, None),
                    None => code,
                };
            }
            Ran::Interrupted { signal } => return 128 + signal,
            Ran::StatusUnknown { stdout } => {
                return match (stale, stdout) {
                    (Some(stale), Stdout::Captured(_)) => self.serve_stale(&stale, None, None),
                    (_, stdout) => {
                        stdout.emit_captured();
                        126
                    }
                };
            }
            Ran::Exited { code, stdout } => (code, stdout),
        };

        let now = now_ms();
        if code == 0 {
            if let Stdout::Captured(out) = &stdout {
                // Print before the write-back, so a pipeline sees output at once.
                emit(out);
                let ttl = Duration::from_secs(self.args.ttl_secs + self.args.stale_secs);
                if let Err(e) = self.store.set(&self.key, &Envelope::encode(now, out), ttl) {
                    warn(&format!(
                        "the command succeeded but its output was not cached: {e}"
                    ));
                }
            }
            // A success ends the backoff, whether or not its output was stored.
            if let Err(e) = self.store.delete(&self.marker_key) {
                warn(&format!("cannot clear the recorded failure: {e}"));
            }
            return 0;
        }

        if matches!(stdout, Stdout::OutputLost) {
            // ck's stdout failed mid-stream and the command most likely died
            // of SIGPIPE: no evidence about the origin, so no marker.
            return code;
        }
        let next = Marker::bumped(marker.as_ref(), code, now);
        if let Err(e) = self
            .store
            .set(&self.marker_key, &next.encode(), next.backend_ttl(now))
        {
            warn(&format!("cannot record the failure: {e}"));
        }
        match (stdout, stale) {
            (Stdout::Captured(_), Some(stale)) => self.serve_stale(&stale, Some(&next), Some(code)),
            (stdout, _) => {
                stdout.emit_captured();
                code
            }
        }
    }

    /// Break with a fresh value to print, or continue with the stale run (if
    /// `--stale` still allows one) for the caller to fall back on.
    fn fresh_or_stale(&self, value: Option<Envelope>) -> ControlFlow<Vec<u8>, Option<Stale>> {
        let Some(envelope) = value else {
            return ControlFlow::Continue(None);
        };
        match envelope.freshness(now_ms(), self.args.ttl_secs, self.args.stale_secs) {
            Freshness::Fresh => ControlFlow::Break(envelope.stdout),
            Freshness::Stale { age_ms } => ControlFlow::Continue(Some(Stale { envelope, age_ms })),
            Freshness::Expired => ControlFlow::Continue(None),
        }
    }

    /// Replay a stored exit-0 run with exit 0 and one stderr line.
    fn serve_stale(&self, stale: &Stale, marker: Option<&Marker>, failed_now: Option<i32>) -> i32 {
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
        warn(&line);
        emit(&stale.envelope.stdout);
        0
    }

    /// Any backend error before the run disables caching for this call, never
    /// the command. No further backend calls are made.
    fn read_failed(&self, e: ReadError, supervisor: &Supervisor) -> Result<i32, Fatal> {
        match e {
            ReadError::Local(fatal) => Err(fatal),
            ReadError::Backend(e) => {
                warn(&format!("the cache is unavailable ({e}); running uncached"));
                Ok(run_uncached(supervisor, &self.args.command))
            }
        }
    }
}

fn run_uncached(supervisor: &Supervisor, argv: &[std::ffi::OsString]) -> i32 {
    match supervisor.run(argv, false) {
        Ran::SpawnFailed { code } | Ran::Exited { code, .. } => code,
        Ran::StatusUnknown { .. } => 126,
        Ran::Interrupted { signal } => 128 + signal,
    }
}

/// The key covers argv and `--scope` only, so only a call whose caller has
/// shut stdin off is cached: `/dev/null`, or a closed fd 0 (which Rust's
/// runtime reopens on `/dev/null` before `main`). A TTY carries whatever the
/// user types, so it does not qualify. Returns what stdin is otherwise.
fn uncacheable_stdin() -> Option<&'static str> {
    let stdin = std::io::stdin();
    let meta = match stdin
        .as_fd()
        .try_clone_to_owned()
        .map(fs::File::from)
        .and_then(|f| f.metadata())
    {
        Ok(meta) => meta,
        Err(e) if e.raw_os_error() == Some(rustix::io::Errno::BADF.raw_os_error()) => return None,
        Err(_) => return Some("not inspectable"),
    };
    let kind = meta.file_type();
    if kind.is_char_device() {
        let Ok(null) = fs::metadata("/dev/null") else {
            // Without /dev/null to compare against, nothing proves stdin
            // is empty.
            return Some("not inspectable");
        };
        if meta.rdev() == null.rdev() {
            return None;
        }
        return Some(if stdin.is_terminal() {
            "a terminal"
        } else {
            "a character device"
        });
    }
    Some(if kind.is_fifo() {
        "a pipe"
    } else if kind.is_socket() {
        "a socket"
    } else if kind.is_file() {
        "a file"
    } else {
        "not /dev/null"
    })
}

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
