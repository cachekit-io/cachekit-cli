//! Running the wrapped command: environment, signals and stdout capture.

use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::{emit, warn, Fatal};

/// The signals ck takes over while it may block or run the command.
const HANDLED: [i32; 4] = [SIGTERM, SIGHUP, SIGINT, SIGQUIT];

/// Output past this is streamed, not cached. The SaaS rejects values over
/// 25 MB; the margin covers the envelope and encryption overhead.
pub const OUTPUT_CAP: usize = 20 * 1024 * 1024;

pub enum Ran {
    /// ck could not start the command, so the origin was never reached.
    SpawnFailed {
        code: i32,
    },
    Exited {
        code: i32,
        stdout: Stdout,
    },
    /// The command ran, but its exit status could not be collected. That is
    /// no evidence the origin failed, so it is treated like a spawn failure:
    /// no marker.
    StatusUnknown {
        stdout: Stdout,
    },
    /// ck received one of [`HANDLED`]. The run writes nothing: no stdout,
    /// no entry and no marker. The caller exits 128 + n.
    Interrupted {
        signal: i32,
    },
}

pub enum Stdout {
    /// Held back: the caller decides whether to print it or replay a stored run.
    Captured(Vec<u8>),
    /// Already written to ck's stdout.
    Written,
    /// Being streamed when ck's own stdout failed: the reader went away, or
    /// the write hit ENOSPC or EIO. The pipe was dropped, so the command most
    /// likely died of SIGPIPE, which says nothing about the origin.
    OutputLost,
}

impl Stdout {
    /// Print output that was held back; anything else is already out.
    pub fn emit_captured(&self) {
        if let Self::Captured(out) = self {
            emit(out);
        }
    }
}

enum State {
    /// No child is running: none yet, or it has exited. A signal now ends ck
    /// at once. Nothing is half-written, because entries are written to a
    /// temp file and renamed into place.
    Idle,
    Running(Pid),
}

/// Owns ck's handling of SIGTERM, SIGHUP, SIGINT and SIGQUIT from the point
/// ck may block (on the fill lock) or run the command.
///
/// SIGTERM and SIGHUP are forwarded to the child. SIGINT and SIGQUIT are not:
/// the child shares ck's foreground process group and has already had them
/// from the terminal, and some programs treat a second interrupt as a hard
/// abort. On any of them ck waits for the child to exit (not for whatever
/// still holds its stdout), writes nothing, and exits 128 + n.
///
/// A signal ck inherited as ignored (`nohup`, a background job) is left
/// ignored, so the command inherits it ignored too and ck never forwards it:
/// wrapping a command must not change what it would do bare.
pub struct Supervisor {
    state: Arc<Mutex<State>>,
    /// Set inside the signal handler itself, so it is visible before the
    /// main thread can reap a child killed by the same process-group signal.
    received: Arc<AtomicUsize>,
    /// SIGCHLD was inherited as ignored. ck handles it itself, or the kernel
    /// would reap the child before ck could read its exit status, and gives
    /// the child the ignored disposition back.
    sigchld_ignored: bool,
}

impl Supervisor {
    pub fn install() -> Result<Self, Fatal> {
        let handled: Vec<i32> = HANDLED
            .into_iter()
            .filter(|&s| !inherited_ignored(s))
            .collect();
        let fail = |e: std::io::Error| Fatal(format!("cannot install signal handlers: {e}"));
        let received = Arc::new(AtomicUsize::new(0));
        for &sig in &handled {
            let value = usize::try_from(sig)
                .unwrap_or_else(|_| unreachable!("signal numbers are positive"));
            signal_hook::flag::register_usize(sig, Arc::clone(&received), value).map_err(fail)?;
        }
        let sigchld_ignored = inherited_ignored(SIGCHLD);
        if sigchld_ignored {
            signal_hook::flag::register(SIGCHLD, Arc::new(AtomicBool::new(false))).map_err(fail)?;
        }
        let mut signals = Signals::new(&handled).map_err(fail)?;
        let state = Arc::new(Mutex::new(State::Idle));
        let thread_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                block_handled_signals();
                // The first handled signal decides how ck ends; it never
                // returns to the loop.
                if let Some(sig) = signals.forever().next() {
                    let running = {
                        let state = thread_state.lock().unwrap_or_else(PoisonError::into_inner);
                        match *state {
                            State::Idle => std::process::exit(128 + sig),
                            State::Running(pid) => {
                                // Under the lock, so the pid cannot be reaped
                                // and reused before the signal lands.
                                forward(pid, sig);
                                pid
                            }
                        }
                    };
                    // Wait for the command itself, not for EOF on its stdout:
                    // a background job it started may hold that open for
                    // ever. ECHILD means main has already reaped it.
                    let exited = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
                    while let Err(rustix::io::Errno::INTR) =
                        rustix::process::waitid(WaitId::Pid(running), exited)
                    {}
                    std::process::exit(128 + sig);
                }
            })
            .map_err(|e| Fatal(format!("cannot start the signal thread: {e}")))?;
        Ok(Self {
            state,
            received,
            sigchld_ignored,
        })
    }

    /// The latest handled signal ck received, if any.
    fn received(&self) -> Option<i32> {
        i32::try_from(self.received.load(Ordering::SeqCst))
            .ok()
            .filter(|&s| s != 0)
    }

    /// [`Self::received`], judged once the child has exited.
    ///
    /// On Linux a process-group signal is queued to every member before any
    /// of them can exit, so ck's flag is already set. Other kernels (XNU)
    /// signal the members one by one, newest first, and promise no order
    /// against the child's exit. So when the child died of a handled signal
    /// and ck has not seen one, wait briefly: a group signal reaches ck
    /// within microseconds, and a signal the command sent itself never does.
    fn received_after(&self, status: Option<&std::process::ExitStatus>) -> Option<i32> {
        const GRACE: Duration = Duration::from_millis(50);
        if let Some(signal) = self.received() {
            return Some(signal);
        }
        if !status
            .and_then(|s| s.signal())
            .is_some_and(|s| HANDLED.contains(&s))
        {
            return None;
        }
        let deadline = Instant::now() + GRACE;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
            if let Some(signal) = self.received() {
                return Some(signal);
            }
        }
        None
    }

    /// Run `argv` with every `CACHEKIT_*` variable removed from its
    /// environment. stdin and stderr pass straight through. stdout is
    /// captured up to [`OUTPUT_CAP`] when `capture` is set, else inherited.
    pub fn run(&self, argv: &[OsString], capture: bool) -> Ran {
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        // Hygiene against accidents, not a boundary: a build log or `env`
        // dump from the command should not carry ck's keys.
        for (name, _) in std::env::vars_os() {
            if name.as_bytes().starts_with(b"CACHEKIT_") {
                command.env_remove(name);
            }
        }
        if capture {
            command.stdout(Stdio::piped());
        }
        if self.sigchld_ignored {
            ignore_sigchld_after_fork(&mut command);
        }

        let mut child = {
            // Held across the spawn so a signal cannot fall between "no
            // child yet" and "child running".
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            match command.spawn() {
                Ok(child) => {
                    *state = State::Running(pid_of(child.id()));
                    child
                }
                Err(e) => {
                    let name = argv[0].to_string_lossy();
                    return Ran::SpawnFailed {
                        code: if e.kind() == ErrorKind::NotFound {
                            warn(&format!("{name}: command not found"));
                            127
                        } else {
                            warn(&format!("{name}: cannot run it: {e}"));
                            126
                        },
                    };
                }
            }
        };

        let stdout = match child.stdout.take() {
            Some(pipe) => collect(pipe),
            None => Stdout::Written,
        };

        // Wait without reaping, then mark the child exited, then reap. A
        // signal forwarded in between reaches a zombie, never a recycled pid.
        let pid = pid_of(child.id());
        let exited = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        let waited = loop {
            match rustix::process::waitid(WaitId::Pid(pid), exited) {
                Err(rustix::io::Errno::INTR) => {}
                other => break other,
            }
        };
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = State::Idle;
        let status = waited
            .map_err(std::io::Error::from)
            .and_then(|_| child.wait());
        // Only a signal ck itself received is an interruption. A command that
        // dies of a signal on its own (`kill -INT $$`, `pkill op`) failed,
        // and may be answered with stale output.
        if let Some(signal) = self.received_after(status.as_ref().ok()) {
            drop(stdout);
            return Ran::Interrupted { signal };
        }
        let status = match status {
            Ok(status) => status,
            Err(e) => {
                warn(&format!("cannot collect the command's exit status: {e}"));
                return Ran::StatusUnknown { stdout };
            }
        };
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        Ran::Exited { code, stdout }
    }
}

/// Read the child's stdout into memory, or, once it passes the cap, write
/// out what is held and stream the rest. Once ck's own stdout fails (the
/// reader has gone), the pipe is dropped, so the command gets SIGPIPE just as
/// it would without ck.
fn collect(mut pipe: impl Read) -> Stdout {
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = match pipe.read(&mut chunk) {
            Ok(0) => return Stdout::Captured(buffer),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                warn(&format!("cannot read the command's output: {e}"));
                emit(&buffer);
                return Stdout::Written;
            }
        };
        if buffer.len() + n > OUTPUT_CAP {
            warn("the command's output passed 20 MiB, so this run is not cached");
            if emit(&buffer) && emit(&chunk[..n]) && stream(pipe, &mut chunk) {
                return Stdout::Written;
            }
            return Stdout::OutputLost;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

/// Copy the rest of the pipe to stdout. Returns `false` if ck's own stdout
/// failed first.
fn stream(mut pipe: impl Read, chunk: &mut [u8]) -> bool {
    loop {
        let n = match pipe.read(chunk) {
            Ok(0) => return true,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                warn(&format!("cannot read the command's output: {e}"));
                return true;
            }
        };
        if !emit(&chunk[..n]) {
            return false;
        }
    }
}

fn forward(pid: Pid, sig: i32) {
    let signal = match sig {
        SIGTERM => Signal::TERM,
        SIGHUP => Signal::HUP,
        _ => return,
    };
    let _ = rustix::process::kill_process(pid, signal);
}

/// Block [`HANDLED`] on the calling thread. ck calls this on every thread it
/// starts (the signal thread and tokio's blocking pool), so their handlers
/// always run on the main thread: a signal delivered to ck is recorded before
/// the main thread's next `read` or `waitid` returns. Whether it is
/// delivered before the child exits depends on the kernel; see
/// [`Supervisor::received_after`].
#[allow(unsafe_code)]
pub(crate) fn block_handled_signals() {
    // SAFETY: `sigemptyset` and `sigaddset` initialise a local set, and
    // `pthread_sigmask` changes only the calling thread's mask. Its one
    // failure, EINVAL, needs an invalid `how`, and SIG_BLOCK is valid.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in HANDLED {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Whether `sig` is ignored, as inherited from ck's parent.
///
/// No dependency offers a safe query of a signal's disposition, so this is
/// one of ck's three `unsafe` blocks.
#[allow(unsafe_code)]
fn inherited_ignored(sig: i32) -> bool {
    // SAFETY: an all-zero `sigaction` is a valid value of the C struct, and
    // with a null new action `sigaction` only writes the current disposition
    // into `old`; it changes nothing.
    let (rc, old) = unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        let rc = libc::sigaction(sig, std::ptr::null(), &mut old);
        (rc, old)
    };
    rc == 0 && old.sa_sigaction == libc::SIG_IGN
}

/// Set SIGCHLD back to ignored in the child, between fork and exec, so the
/// command inherits the disposition it would have had without ck.
#[allow(unsafe_code)]
fn ignore_sigchld_after_fork(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the forked child before exec and calls
    // only `signal`, which is async-signal-safe, allocating nothing and
    // taking no lock.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGCHLD, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn pid_of(id: u32) -> Pid {
    i32::try_from(id)
        .ok()
        .and_then(Pid::from_raw)
        .unwrap_or_else(|| unreachable!("a spawned child has a positive pid"))
}
