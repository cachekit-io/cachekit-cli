# cachekit-cli

`ck`, the command-line client for [CacheKit](https://cachekit.io).

Pre-release: nothing is published yet. To try it, build from a checkout with
`cargo build --release` and put `target/release/ck` on your `PATH`.

## `ck run`

`ck run` wraps a command and caches its stdout, so a slow or rate-limited call
is not repeated. A typical use is a secret read in a shell startup file, where
every new shell would otherwise call the secret manager again:

```sh
API_TOKEN="$(ck run --ttl 12h --stale 7d -- op read op://Private/api/token < /dev/null)"
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
ck run [--backend file] [--ttl D] [--stale D] [--scope S] [--refresh] -- <command> [args...]
```

| Flag | Default | Meaning |
|---|---|---|
| `--ttl D` | `60s` | Serve the stored output as fresh for `D`. At least `1s`. |
| `--stale D` | `0s` | After that, serve it for `D` more when the command fails. |
| `--scope S` | empty | A string folded into the cache key (see below). |
| `--refresh` | off | Run the command now and store the result, ignoring what is stored. |
| `--backend file` | `file` | Entries live in `~/.cache/ck`. It is the only backend in this release. |

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
ck run --refresh -- date -u < /dev/null
```

Concurrent calls for the same command on one machine run it once: the first
takes a lock and the rest wait for its result, or serve stale output if they
have it. Output larger than 20 MiB is passed through and not cached.

### stdin must be `/dev/null`

The cache key covers the command line and `--scope`, not what arrives on
stdin. So ck caches a call only when stdin is `/dev/null` (or closed). With a
terminal, a pipe, a file or a socket on stdin, ck runs the command uncached and
prints one warning. That is why every example here ends in `< /dev/null`.
Programs that prompt by opening the terminal themselves, as `op`, `ssh` and
`sudo` do, still reach you with stdin redirected.

### What the key does not cover

The key is the command and its arguments, plus `--scope`. It does not cover
the environment, the working directory, the contents of files the command
reads, or the network. Redirecting stdin to `/dev/null` is your assertion that
none of those change the output. When one does, put it in `--scope`:

```sh
ck run --ttl 1h --scope "$PWD" -- ls < /dev/null
```

### Wrap the command itself

Run the command directly rather than through `sh -c`:

```sh
ck run --ttl 5m -- uname -a < /dev/null
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
  one, to make every stored entry unreadable.

### Encryption and the file key

Entries are encrypted with AES-256-GCM, and their file names are keyed hashes,
so neither the output nor the command line appears on disk. The key comes from
`CACHEKIT_MASTER_KEY` (hex, an even number of characters and at least 64) if
it is set. Otherwise ck
creates a random key in `~/.config/ck/file.key` on first use, mode 0600. If
that file is damaged, ck exits 125 and tells you to delete it; it never
overwrites it.

ck removes every `CACHEKIT_*` variable from the command's environment, so a
log or environment dump from the command does not carry your keys.

### Signals

ck forwards SIGTERM and SIGHUP to the command. On those and on SIGINT or
SIGQUIT it waits for the command, stores nothing and exits 128 plus the
signal number.

### Platforms

Linux and macOS. The cache directory must be on a local filesystem: on NFS or
SMB, concurrent first calls may each run the command, though entries are never
torn.

## License

MIT. See [LICENSE](LICENSE).
