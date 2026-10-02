//! `ck`, the CacheKit command-line client.
//!
//! This library exists to serve the `ck` binary and its tests. It is not a
//! stable API.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{ErrorKind, Write};

mod child;
pub mod cli;
pub mod entry;
pub mod keys;
mod run;
pub mod store;

/// ck's own error before the command runs: printed, then exit 125.
#[derive(Debug)]
pub struct Fatal(pub String);

/// Exit code for ck's own errors, after the `env` and `timeout` convention.
pub const EXIT_CK_ERROR: i32 = 125;

pub fn main(args: impl IntoIterator<Item = OsString>) -> i32 {
    let invocation = match cli::parse(args) {
        Ok(i) => i,
        Err(e) => {
            warn(&e);
            eprintln!("{}", cli::USAGE);
            return EXIT_CK_ERROR;
        }
    };
    match invocation {
        cli::Invocation::Help => {
            println!("{}", cli::USAGE);
            0
        }
        cli::Invocation::Version => {
            println!("ck {}", env!("CARGO_PKG_VERSION"));
            0
        }
        cli::Invocation::Run(args) => run::run(&args).unwrap_or_else(|Fatal(e)| {
            warn(&e);
            EXIT_CK_ERROR
        }),
    }
}

/// One line on stderr, prefixed so it is never mistaken for the command's.
pub(crate) fn warn(message: &str) {
    eprintln!("ck: {message}");
}

/// Write to stdout. A reader that has gone away (EPIPE) is normal in a
/// pipeline and stays silent; any other failure is reported.
pub(crate) fn emit(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    if let Err(e) = out.write_all(bytes).and_then(|()| out.flush()) {
        if e.kind() != ErrorKind::BrokenPipe {
            warn(&format!("cannot write stdout: {e}"));
        }
    }
}
