# cachekit-cli

`ck`, the command-line client for [CacheKit](https://cachekit.io).

## Install

There is no install script. Install with Cargo, or download a release binary
and verify where it came from.

### With Cargo

```text
cargo install --locked cachekit-cli
```

This builds `ck` from the crate on crates.io and puts it in `~/.cargo/bin`.
Each crates.io release is published by this repository's release workflow.

### Release binaries

Each [release](https://github.com/cachekit-io/cachekit-cli/releases) carries
static Linux binaries for `x86_64` and `aarch64`, and a macOS binary for Apple
silicon. On other platforms, install with Cargo. Download the archive for your
platform, then verify it with the [GitHub CLI](https://cli.github.com/) before
you unpack it:

```text
gh attestation verify ck-v0.1.0-x86_64-unknown-linux-musl.tar.gz --repo cachekit-io/cachekit-cli \
  --signer-workflow cachekit-io/cachekit-cli/.github/workflows/release.yml \
  --deny-self-hosted-runners
```

The check passes only if the file was built by this repository's release
workflow on a GitHub-hosted runner. `--owner cachekit-io` alone is not enough:
it accepts a file built by any workflow in any `cachekit-io` repository. The
`ck` binary inside the archive is attested too, so the same command verifies it
after unpacking.

### From a checkout

Build with `cargo build --release --locked` and put `target/release/ck` on
your `PATH`.

## `ck run`

`ck run` wraps a command and caches its stdout, so a slow or rate-limited call
is not repeated. A typical use is a secret read in a shell startup file, where
every new shell would otherwise call the secret manager again:

```sh
API_TOKEN="$(ck run --ttl 12h --stale 7d -- op read op://Private/api/token)"
export API_TOKEN
```

The first call runs `op` and stores its output. Calls in the next 12 hours
print the stored output without running `op`. After that, ck runs `op` again;
if `op` fails, ck serves the stored output for up to 7 more days. That is
also the revocation bound: a token rotated at the source can still be served
for up to 12 hours plus 7 days, so set `--stale` to how long you can tolerate
serving a revoked secret (see [How long a cached secret lives](#how-long-a-cached-secret-lives)).
Assigning before exporting keeps ck's exit code visible to the shell.

```text
ck run [--backend file|saas] [--ttl D] [--stale D] [--scope S] [--refresh] -- <command> [args...]
```

| Flag | Default | Meaning |
|---|---|---|
| `--ttl D` | `60s` | Serve the stored output as fresh for `D`. At least `1s`. |
| `--stale D` | `0s` | After that, serve it for `D` more when the command fails. |
| `--scope S` | empty | A string folded into the cache key (see below). |
| `--refresh` | off | Run the command now and store the result, ignoring what is stored. |
| `--backend B` | `file` | `file` keeps entries in `~/.cache/ck`. `saas` keeps them in CacheKit, shared across hosts (see [Sharing a cache across hosts](#sharing-a-cache-across-hosts)). |

A duration `D` is a whole number followed by one unit: `s`, `m`, `h` or `d`,
such as `90s`, `15m`, `12h` or `7d`. `--ttl` plus `--stale` may not exceed
`30d`. ck's own errors, such as a bad flag, exit 125 before anything runs.

### Only exit 0 is cached

ck stores a run only when the command exits 0. A run that fails is never
stored and never served. Commands that use a non-zero code for a normal
result, like `grep` (1 for no match) or `diff` (1 for a difference), are
therefore not cacheable.

When a run fails, ck backs off: it records the failure and does not run the
command again for 30 seconds, doubling after each consecutive failure up to
15 minutes. During the backoff a call serves the stored output if `--stale`
allows it (exit 0, with one line on stderr giving its age), and otherwise
prints nothing and exits with the failed run's code. `--refresh` runs the
command whatever the backoff says:

```sh
ck run --refresh -- date -u
```

Concurrent calls for the same command on one machine run it once: the first
takes a lock and the rest wait for its result, or serve stale output if they
have it. Output larger than 20 MiB is passed through and not cached; with
`--backend saas` the limit is 1 MiB, so that every host can read an entry back
within its 1-second request deadline. Either way ck prints one warning when
output passes the limit. With `--backend saas` the backoff is shared by every
host, but the lock is not, so hosts that miss at the same moment each run the
command.

### The command does not get your stdin

ck runs the command with an empty stdin (`/dev/null`), whatever ck's own stdin
is, and never reads its own. The cache key does not cover stdin, so this is
what lets a call cache the same way from a terminal, a pipe, CI or an agent's
shell. Pass input in arguments, files or `--scope` instead. Programs that
prompt by opening the terminal themselves, as `op`, `ssh` and `sudo` do, still
reach you.

### What the key does not cover

The key is the command and its arguments, plus `--scope`. It does not cover
the environment, the working directory, the contents of files the command
reads, or the network. Wrapping a command in ck is your assertion that none of
those change the output. When one does, put it in `--scope`:

```sh
ck run --ttl 1h --scope "$PWD" -- ls
```

### Wrap the command itself

Run the command directly rather than through `sh -c`:

```sh
ck run --ttl 5m -- uname -a
```

When ck cannot start the command at all (it is not installed, or not
executable), it exits 127 or 126 and records no failure, so the next call
tries again at once. Through `sh -c`, a missing command is just the shell
exiting 127, which ck must treat as an ordinary failure and back off.

### How long a cached secret lives

Stored output can be served for up to `--ttl` plus `--stale` after it was
produced. ck gets no signal when a secret is rotated at its source, so if you
cache a secret, set `--stale` to how long you can tolerate serving a revoked
one. The default `--stale 0s` keeps the bound at the TTL.

To stop serving old output:

- lower `--stale` or `--ttl`, which takes effect on the next call;
- delete `~/.cache/ck` to remove every entry;
- delete `~/.config/ck/file.key`, or change `CACHEKIT_MASTER_KEY` if you set
  one, to make every stored entry unreadable. This is the purge for
  `--backend saas`: once a host has the new key, it can no longer read any
  entry stored under the old one. A host still on the old key keeps reading
  them, and CacheKit keeps the old entries until they expire, at most `--ttl`
  plus `--stale` after they were written.

### Encryption and the file key

Entries are encrypted with AES-256-GCM, and their file names are keyed hashes,
so neither the output nor the command line appears on disk. The key comes from
`CACHEKIT_MASTER_KEY` (exactly 64 hex characters, from `openssl rand -hex 32`) if
it is set. Otherwise ck
creates a random key in `~/.config/ck/file.key` on first use, mode 0600. If
that file is damaged, ck exits 125 and tells you to delete it; it never
overwrites it. A stored entry that fails to decrypt, because it was corrupted
or tampered with, is never served: ck treats it as a miss and prints one
warning.

ck removes every `CACHEKIT_*` variable from the command's environment, so a
log or environment dump from the command does not carry your keys.

### Sharing a cache across hosts

`--backend saas` keeps entries in [CacheKit](https://cachekit.io), so a laptop
and a CI runner that run the same command share one result. ck uses it only
when you pass `--backend saas`; setting `CACHEKIT_API_KEY` alone never selects
it. It needs two variables:

- `CACHEKIT_API_KEY`: an SDK API key (`ck_sdk_...`) or a `ck_live_` key from
  the CacheKit dashboard. ck stores its entries in the `ck-run` namespace,
  which a `ck_api_` key cannot write, so ck refuses one with exit 125. A key
  limited to named namespaces must be granted `ck-run`.
- `CACHEKIT_MASTER_KEY`: 64 hex characters from `openssl rand -hex 32`. It
  encrypts every entry before it leaves the machine and keys every entry's
  name, so CacheKit stores only ciphertext under names it cannot reverse.
  Without it, ck exits 125 before running anything. It never falls back to
  the file key.

Give every host that shares the cache the same two values:

```sh
ck run --backend saas --ttl 12h --stale 1d -- date -u +%F
```

**The master key is the trust boundary.** Every host that holds it can store
output that every other holder will then serve as the command's result.
Encryption keeps the output from CacheKit, but it does not tell one holder's
output from another's, and `--scope` does not separate holders either. So:

- use one master key per group of machines that already trust each other;
- never give it to a runner that executes untrusted code, such as a build of
  a pull request from a fork;
- never `eval` or execute shared output unless every holder of the key may
  already run code on the machine that consumes it.

**A CacheKit fault never stops your command.** When CacheKit cannot answer, or
answers with an error, ck runs the command uncached and prints one warning
naming the cause. When another call on the same machine is already running
the command and this one has stale output it may serve, ck serves that
output instead, with the cause on its one stderr line. That covers an outage, a timeout, and a rejected key
(revoked, rotated or missing the `ck-run` grant), whose warning also names the
fix. Each request to CacheKit gets 1 second: a call adds at most 1 second when
CacheKit is down, and a first call that stores its output adds at most 3. On a
link where requests routinely take longer than that, every call warns and
nothing is cached; use the file backend there. The cost of carrying on is the
quota caching was saving you: while the fault lasts, every call reaches the
command's origin. An outage ends by itself, but a bad key lasts until someone
fixes it, so against a daily quota it can use up the rest of the day. Watch
for the warning.

**Keep the clocks in sync.** Each call judges an entry's age by its own clock
against the time the writing host recorded. A writer whose clock runs behind
makes entries look older, so they refresh early. A writer whose clock runs
ahead makes them look fresh for longer, by up to the difference, until
CacheKit expires them at `--ttl` plus `--stale` after they were written.

**What CacheKit receives.** Each request carries the API key, the entry's
keyed name, and for a write the encrypted entry and its lifetime. A command's
failure record is stored beside its output, under the same name plus `:neg`.
So CacheKit can see how many entries you have, their sizes and lifetimes, when
each is read and written, and which commands have a recorded failure, but
neither the commands nor their output. The CacheKit client also sends three
headers on every request, which ck cannot turn off: `X-CacheKit-Session-ID`, a
random identifier for each ck process; `X-CacheKit-Session-Start`, when that
process began; and `X-CacheKit-L1-Status: disabled`. Requests go through the
proxy in `HTTPS_PROXY` when it is set. ck always talks to
`https://api.cachekit.io`, and ignores `CACHEKIT_API_URL`, so a project's
environment cannot send your key elsewhere.

### Signals

When ck itself receives SIGTERM, SIGHUP, SIGINT or SIGQUIT, the run is
interrupted. ck forwards SIGTERM and SIGHUP to the command; SIGINT and SIGQUIT
already reach it from the terminal. ck then waits for the command to exit, but
not for a background job it left holding the output. Later SIGTERM and SIGHUP
signals are still forwarded, so a command that handles the first one
gracefully can still be stopped.

An interrupted run discards the output ck was holding back, stores nothing,
and exits 128 plus the number of a signal it received; when several arrive
close together, which one is unspecified. Output already written, by an
uncached call or past 20 MiB, stays written, and a cache entry being written
is never left half-written. A signal while ck is printing stored output can
cut that output off; ck still exits 128 plus the signal number, so a caller
that captures output with `$(…)` must check the exit code. On an interrupted
run streamed past 20 MiB, ck ends as soon as the command does, so the last
part of the output it was still relaying can be cut off. A SIGINT or SIGQUIT
that lands while ck is starting the command may not reach it; ck still exits
128 plus the signal number when the command exits.

A command that dies of a signal ck did not receive, such as one that kills
itself, has simply failed: ck backs off and serves stored output as for any
other failure. A signal ck inherits as ignored, as under `nohup`, stays
ignored: ck neither forwards it nor stops for it, and the command inherits it
ignored too.

### Platforms

Linux and macOS. The cache directory must be on a local filesystem: on NFS or
SMB, concurrent first calls may each run the command, though entries are never
torn.

## License

MIT. See [LICENSE](LICENSE).
