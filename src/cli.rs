//! Argument parsing for `ck run`.
//!
//! Hand-rolled rather than clap: one subcommand with five flags does not
//! justify the start-up cost and dependency tree of a parser framework on a
//! path that has to stay inside a 5 ms budget.

use std::ffi::OsString;

/// `ttl + stale` may not exceed this on any backend. It is the SaaS storage
/// cap, applied everywhere so a command that works on file also works on saas.
pub const MAX_RETENTION_SECS: u64 = 30 * 86_400;

pub const USAGE: &str = "\
usage: ck run [--backend file] [--ttl D] [--stale D] [--scope S] [--refresh] -- <command> [args...]

Runs <command> and caches its stdout when it exits 0. Only a call whose stdin
is /dev/null is cached, so end every cached call with `< /dev/null`.

  --backend file  where entries live (default: file, in ~/.cache/ck)
  --ttl D         serve the stored output as fresh for D (default: 60s)
  --stale D       after that, serve it for D more if the command fails (default: 0s)
  --scope S       an extra string folded into the cache key
  --refresh       run the command now, ignoring any stored output

D is a whole number followed by s, m, h or d, such as 90s, 15m, 12h or 7d.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    File,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RunArgs {
    pub backend: BackendKind,
    pub ttl_secs: u64,
    pub stale_secs: u64,
    pub scope: OsString,
    pub refresh: bool,
    pub command: Vec<OsString>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Run(RunArgs),
    Help,
    Version,
}

/// Parse everything after the program name. An `Err` is a usage error: the
/// caller prints it and exits 125 without running anything.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, String> {
    let mut args = args.into_iter();
    let Some(sub) = args.next() else {
        return Err("missing subcommand".into());
    };
    match sub.to_str() {
        Some("run") => parse_run(args),
        Some("-h" | "--help" | "help") => Ok(Invocation::Help),
        Some("-V" | "--version") => Ok(Invocation::Version),
        _ => Err(format!("unknown subcommand {:?}", sub)),
    }
}

fn parse_run(mut args: impl Iterator<Item = OsString>) -> Result<Invocation, String> {
    let mut backend = BackendKind::File;
    let mut ttl_secs = 60;
    let mut stale_secs = 0;
    let mut scope = OsString::new();
    let mut refresh = false;

    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        // Flags end at `--` or at the first argument that is not a flag, so
        // the wrapped command's own flags are never read as ck's.
        let Some(text) = arg.to_str().filter(|s| s.starts_with('-')) else {
            command.push(arg);
            break;
        };
        if text == "--" {
            break;
        }
        let (name, inline) = match text.split_once('=') {
            Some((n, v)) => (n, Some(OsString::from(v))),
            None => (text, None),
        };
        let mut value = || -> Result<OsString, String> {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match name {
            "--backend" => {
                backend = match value()?.to_str() {
                    Some("file") => BackendKind::File,
                    Some("saas") => {
                        return Err("--backend saas is not available in this release".into())
                    }
                    _ => return Err("--backend must be file".into()),
                }
            }
            "--ttl" => ttl_secs = duration(&value()?, name)?,
            "--stale" => stale_secs = duration(&value()?, name)?,
            "--scope" => scope = value()?,
            "--refresh" if inline.is_none() => refresh = true,
            "--refresh" => return Err("--refresh takes no value".into()),
            "-h" | "--help" => return Ok(Invocation::Help),
            _ => return Err(format!("unknown flag {name}")),
        }
    }
    command.extend(args);

    if command.is_empty() {
        return Err("missing the command to run".into());
    }
    if ttl_secs < 1 {
        return Err("--ttl must be at least 1s".into());
    }
    if ttl_secs.saturating_add(stale_secs) > MAX_RETENTION_SECS {
        return Err("--ttl plus --stale may not exceed 30d".into());
    }
    Ok(Invocation::Run(RunArgs {
        backend,
        ttl_secs,
        stale_secs,
        scope,
        refresh,
        command,
    }))
}

/// A non-negative integer followed by exactly one unit: `s`, `m`, `h` or `d`.
fn duration(raw: &OsString, flag: &str) -> Result<u64, String> {
    let bad = || format!("{flag}: {raw:?} is not a duration such as 90s, 15m, 12h or 7d");
    let text = raw.to_str().ok_or_else(bad)?;
    let unit_len = text.chars().last().map_or(0, char::len_utf8);
    let (digits, unit) = text.split_at(text.len() - unit_len);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(bad()),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<RunArgs, String> {
        let all = std::iter::once("run").chain(args.iter().copied());
        match parse(all.map(OsString::from))? {
            Invocation::Run(r) => Ok(r),
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let r = run(&["--", "op", "read", "x"]).unwrap();
        assert_eq!((r.ttl_secs, r.stale_secs, r.refresh), (60, 0, false));
        assert_eq!(r.command, ["op", "read", "x"]);
        assert!(r.scope.is_empty());
    }

    #[test]
    fn flags_end_at_the_first_non_flag() {
        let r = run(&["--ttl=12h", "--stale", "7d", "op", "--stale", "1s"]).unwrap();
        assert_eq!((r.ttl_secs, r.stale_secs), (43_200, 604_800));
        assert_eq!(r.command, ["op", "--stale", "1s"]);
    }

    #[test]
    fn duration_grammar() {
        for ok in ["0s", "90s", "15m", "12h", "7d"] {
            assert!(duration(&ok.into(), "--stale").is_ok(), "{ok}");
        }
        for bad in [
            "",
            "s",
            "1",
            "1.5h",
            "-1s",
            "+1s",
            "1h30m",
            "1S",
            " 1s",
            "1w",
            "99999999999999999999d",
            "5é",
            "é",
        ] {
            assert!(duration(&bad.into(), "--stale").is_err(), "{bad}");
        }
    }

    #[test]
    fn limits() {
        assert!(run(&["--ttl", "0s", "--", "x"]).is_err());
        assert!(run(&["--ttl", "29d", "--stale", "1d", "--", "x"]).is_ok());
        assert!(run(&["--ttl", "29d", "--stale", "1d", "--stale", "86401s", "--", "x"]).is_err());
        assert!(run(&["--"]).is_err());
        assert!(run(&["--bogus", "--", "x"]).is_err());
        assert!(run(&["--backend", "saas", "--", "x"]).is_err());
        assert!(run(&["--refresh=yes", "--", "x"]).is_err());
    }
}
