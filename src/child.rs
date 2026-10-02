//! Running the wrapped command: environment, signals and stdout capture.

use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::{emit, warn, Fatal};

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
}

pub enum Stdout {
    /// Held back: the caller decides whether to print it or replay a stored run.
    Captured(Vec<u8>),
    /// Already written to ck's stdout.
    Written,
}

enum State {
    /// No child yet. A signal now ends ck at once: nothing has been written.
    Waiting,
    Running(Pid),
    /// The child has exited. Its pid must not be signalled again.
    Exited,
}

/// Owns ck's handling of SIGTERM, SIGHUP, SIGINT and SIGQUIT from the point
/// ck may block (on the fill lock) or run the command.
///
/// SIGTERM and SIGHUP are forwarded to the child. SIGINT and SIGQUIT are not:
/// the child shares ck's foreground process group and has already had them
/// from the terminal, and some programs treat a second interrupt as a hard
/// abort. On any of them ck waits for the child, stores nothing, and exits
/// 128 + n.
pub struct Supervisor {
    state: Arc<Mutex<State>>,
    received: Arc<AtomicI32>,
}

impl Supervisor {
    pub fn install() -> Result<Self, Fatal> {
        let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT])
            .map_err(|e| Fatal(format!("cannot install signal handlers: {e}")))?;
        let state = Arc::new(Mutex::new(State::Waiting));
        let received = Arc::new(AtomicI32::new(0));
        let (thread_state, thread_received) = (Arc::clone(&state), Arc::clone(&received));
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                for sig in signals.forever() {
                    let _ = thread_received.compare_exchange(
                        0,
                        sig,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    let state = thread_state.lock().unwrap_or_else(PoisonError::into_inner);
                    match *state {
                        State::Waiting => std::process::exit(128 + sig),
                        State::Running(pid) => forward(pid, sig),
                        State::Exited => {}
                    }
                }
            })
            .map_err(|e| Fatal(format!("cannot start the signal thread: {e}")))?;
        Ok(Self { state, received })
    }

    /// The first terminating signal ck received, if any.
    pub fn received(&self) -> Option<i32> {
        Some(self.received.load(Ordering::SeqCst)).filter(|&s| s != 0)
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
        while let Err(rustix::io::Errno::INTR) = rustix::process::waitid(WaitId::Pid(pid), exited) {
        }
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = State::Exited;
        let code = match child.wait() {
            Ok(status) => status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            Err(e) => {
                warn(&format!("cannot collect the command's exit status: {e}"));
                126
            }
        };
        Ran::Exited { code, stdout }
    }
}

/// Read the child's stdout into memory, or, once it passes the cap, write
/// out what is held and stream the rest.
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
            emit(&buffer);
            emit(&chunk[..n]);
            stream(pipe, &mut chunk);
            return Stdout::Written;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

fn stream(mut pipe: impl Read, chunk: &mut [u8]) {
    loop {
        match pipe.read(chunk) {
            Ok(0) => return,
            Ok(n) => emit(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return warn(&format!("cannot read the command's output: {e}")),
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

fn pid_of(id: u32) -> Pid {
    i32::try_from(id)
        .ok()
        .and_then(Pid::from_raw)
        .unwrap_or_else(|| unreachable!("a spawned child has a positive pid"))
}
