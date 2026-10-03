//! `ck`, the CacheKit command-line client.
//!
//! This library exists to serve the `ck` binary and its tests. It is not a
//! stable API.

use std::ffi::OsString;
use std::io::{ErrorKind, Write};

mod child;
pub mod cli;
pub mod entry;
pub mod keys;
mod run;
pub mod saas;
pub mod store;

/// ck's own error before the command runs: printed, then exit 125.
#[derive(Debug)]
pub struct Fatal(pub String);

/// Exit code for ck's own errors, after the `env` and `timeout` convention.
pub const EXIT_CK_ERROR: i32 = 125;

pub fn main(args: impl IntoIterator<Item = OsString>) -> i32 {
    main_with(args, &saas::connect)
}

/// [`main`] with the SaaS client replaced, so tests can run ck's saas path
/// against a fake backend. A local HTTP stub cannot stand in for the service:
/// the cachekit-rs client refuses loopback and private addresses.
#[doc(hidden)]
pub fn main_with(args: impl IntoIterator<Item = OsString>, connect: &saas::Connect) -> i32 {
    let invocation = match cli::parse(args) {
        Ok(i) => i,
        Err(e) => {
            warn(&e);
            let _ = writeln!(std::io::stderr(), "{}", cli::USAGE);
            return EXIT_CK_ERROR;
        }
    };
    match invocation {
        cli::Invocation::Help => {
            emit(format!("{}\n", cli::USAGE).as_bytes());
            0
        }
        cli::Invocation::Version => {
            emit(format!("ck {}\n", env!("CARGO_PKG_VERSION")).as_bytes());
            0
        }
        cli::Invocation::Run(args) => run::run(&args, connect).unwrap_or_else(|Fatal(e)| {
            warn(&e);
            EXIT_CK_ERROR
        }),
    }
}

/// One line on stderr, prefixed so it is never mistaken for the command's.
/// A closed stderr is not worth dying over, so a failed write is dropped.
pub(crate) fn warn(message: &str) {
    let _ = writeln!(std::io::stderr(), "ck: {message}");
}

/// Write to stdout, and say whether it worked. A reader that has gone away
/// (EPIPE) is normal in a pipeline and stays silent; any other failure is
/// reported.
pub(crate) fn emit(bytes: &[u8]) -> bool {
    let mut out = std::io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => true,
        Err(e) => {
            if e.kind() != ErrorKind::BrokenPipe {
                warn(&format!("cannot write stdout: {e}"));
            }
            false
        }
    }
}
