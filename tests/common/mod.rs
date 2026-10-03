//! A sandbox per test: its own HOME, and a PATH shim that counts its calls.
//!
//! Every test drives the real `ck` binary as a separate process.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

pub const BIN: &str = env!("CARGO_BIN_EXE_ck");

/// A fixed master key, for tests that need to write entries themselves.
pub const MASTER_KEY_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

pub struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let sandbox = Self {
            root: tempfile::tempdir().expect("tempdir"),
        };
        fs::create_dir_all(sandbox.home()).unwrap();
        fs::create_dir_all(sandbox.bin()).unwrap();
        sandbox.install_shim("origin");
        sandbox
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    pub fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    fn state(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// A command on PATH that records each call, then prints the contents of
    /// `out` (default `hello`) and exits with the code in `exit` (default 0).
    /// `sleep`, `env` and `stdin` files make it sleep first, dump its
    /// environment, or copy what it reads on stdin, then `EOF`, to `stdin.out`;
    /// `selfkill` (holding a signal name) makes it send that signal to itself.
    pub fn install_shim(&self, name: &str) {
        let r = self.root.path().display();
        let script = format!(
            "#!/bin/sh\n\
             echo run >> '{r}/calls'\n\
             [ -f '{r}/sleep' ] && sleep \"$(cat '{r}/sleep')\"\n\
             [ -f '{r}/selfkill' ] && kill -\"$(cat '{r}/selfkill')\" $$\n\
             [ -f '{r}/env' ] && env > '{r}/env.out'\n\
             [ -f '{r}/stdin' ] && {{ cat; echo EOF; }} > '{r}/stdin.out'\n\
             if [ -f '{r}/out' ]; then cat '{r}/out'; else echo hello; fi\n\
             exit \"$(cat '{r}/exit' 2>/dev/null || echo 0)\"\n"
        );
        let path = self.bin().join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn set(&self, name: &str, value: &str) {
        fs::write(self.state(name), value).unwrap();
    }

    pub fn unset(&self, name: &str) {
        let _ = fs::remove_file(self.state(name));
    }

    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.state(name)).unwrap_or_default()
    }

    /// How many times the shim has run.
    pub fn runs(&self) -> usize {
        self.read("calls").lines().count()
    }

    /// `ck <args>` in this sandbox, in its own process group, with stdin on
    /// /dev/null and no CacheKit variables inherited from the machine running
    /// the tests.
    pub fn ck(&self, args: &[&str]) -> Command {
        self.command(Path::new(BIN), args)
    }

    /// As [`Sandbox::ck`], with another program standing in for `ck`.
    pub fn command(&self, program: &Path, args: &[&str]) -> Command {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(program);
        // Its own process group, so a failing test can kill ck together with
        // its command and any background job (see `wait_exit`).
        cmd.process_group(0);
        cmd.args(args);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("CACHEKIT_") {
                cmd.env_remove(name);
            }
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin()];
        paths.extend(std::env::split_paths(&path));
        cmd.env("HOME", self.home())
            .env("PATH", std::env::join_paths(paths).unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// `ck run <flags> -- origin`, run to completion.
    pub fn run(&self, flags: &[&str]) -> Output {
        self.ck(&run_args(flags)).output().unwrap()
    }

    pub fn spawn(&self, flags: &[&str]) -> Child {
        self.ck(&run_args(flags)).spawn().unwrap()
    }

    pub fn config_dir(&self) -> PathBuf {
        self.home().join(".config/ck")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.home().join(".cache/ck/data")
    }
}

fn run_args<'a>(flags: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["run"];
    args.extend_from_slice(flags);
    args.extend(["--", "origin"]);
    args
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

pub fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The bodies of the README's `sh` code blocks.
pub fn readme_sh_blocks() -> Vec<String> {
    let readme = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    sh_blocks(&readme)
}

fn sh_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        match (&mut current, line.trim_end()) {
            (None, "```sh") => current = Some(String::new()),
            (Some(block), "```") => {
                blocks.push(std::mem::take(block));
                current = None;
            }
            (Some(block), text) => {
                block.push_str(text);
                block.push('\n');
            }
            (None, _) => {}
        }
    }
    blocks
}
