# safe-git

A small Rust client for this workflow in an **existing ordinary working tree**:

```sh
safe-git fetch
safe-git merge --ff-only origin/main
safe-git push
```

It implements the Git wire protocol, objects, pack/delta decoding, refs, index,
and checkout directly. It does not wrap Git or libgit2. Hooks, filters, command
aliases, credential/transport helpers, external diff/textconv, pagers, shell
execution, and local OpenSSH execution **are not implemented**. There is no
fallback to Git. SSH runs inside the process using `russh`.

Repository content and metadata are untrusted input. This is experimental,
unaudited security-sensitive software, not a claim that all unsafe behavior or
parser bugs have been eliminated.

## Install and use

Linux, Rust 1.99 or newer:

```sh
cargo install --locked --path .
```

Choose the endpoint outside the repository. For the supplied GitHub remote:

```sh
export SAFE_GIT_URL=git@github.com:futpib/folio.git
export SAFE_GIT_SSH_AGENT=true

cd /path/to/folio
safe-git fetch
safe-git merge --ff-only origin/main  # use the actual fetched branch
safe-git push
```

The SSH agent must already contain your key, and `~/.ssh/known_hosts` must contain
a verified host key. Alternatively, set `SAFE_GIT_IDENTITY_FILE` to an explicit
unencrypted private key, without setting `SAFE_GIT_SSH_AGENT`. Encrypted keys use
the agent; there are no password/passphrase prompts. `--identity-file`,
`--ssh-agent`, `--known-hosts FILE`, and `--url URL` are explicit flag alternatives.
Do not set the endpoint or authentication environment by sourcing repository code.

No-argument fetch selects the current branch and stores `origin/BRANCH`;
no-argument push sends the current branch under the same name. Repository
`remote.*`, `branch.*`, URL rewriting, and SSH configuration are never consulted.
This intentionally prevents untrusted repository configuration from choosing
where authenticated connections or pushes go. Select different endpoints or
branches explicitly when needed:

```sh
safe-git -C /path/to/repo --ssh-agent fetch git@github.com:owner/source.git main
safe-git -C /path/to/repo merge --ff-only origin/main
safe-git -C /path/to/repo --ssh-agent push git@github.com:owner/destination.git main:main
```

These push commands really push. Tests use disposable local servers; they do not
push to GitHub. `-C` must name the working-tree root or bare repository exactly;
there is no parent-directory repository discovery.

Both `user@host:path` and `ssh://user@host:port/path` work. SSH usernames must be
explicit. Host aliases, `~/.ssh/config`, ProxyCommand, certificates, password
authentication, and agent forwarding are absent. Plain and hashed known-host
entries work. Files with `@revoked` or `@cert-authority` markers are rejected
because the SSH library does not implement their semantics. There is no host-key
verification bypass. SSH URI paths cannot contain escapes or whitespace; scp
syntax accepts literal paths, quoted as a single argument to the remote service.

## Supported operations

| Command | Effect |
| --- | --- |
| `fetch [URL [BRANCH]] [--remote origin]` | Fetch one branch, validate the new reachable objects, and update one tracking ref. Existing worktree and index are untouched. Remote label is only a storage namespace. |
| `merge --ff-only origin/BRANCH` | Advance the current branch along any parent chain, updating files and index. Unrelated staged, unstaged, and untracked changes survive. Overlapping changes are refused. Equal tips or an incoming ancestor succeed without changes; divergence fails. |
| `push [URL [LOCAL:REMOTE]]` | Push one branch, requiring destination ancestry and server compare-and-swap. No force, deletion, wildcard, mirror, or arbitrary ref updates. A single branch argument names both ends. |
| `recover` | Recover an interrupted checkout using verified backups, without overwriting later user edits. |
| `init --bare [--initial-branch main] PATH` | Optional bootstrap of a fresh bare SHA-1 repository. Existing ordinary working trees are also supported. |

`--quiet`/`-q` suppresses successful summaries. `merge --no-stat` omits statistics.
No other Git subcommands are implemented. Branch arguments accept short names or
`refs/heads/...`; merge accepts short tracking names or `refs/remotes/...`.
Tags, detached HEAD, arbitrary revision expressions, and merge commits are outside
this subset. Tracking refs may follow an upstream rewind; local and destination
branches never move backwards.

Checkout writes raw blob bytes and executable modes. It never interprets
`.gitattributes` or `.gitmodules`, converts line endings, runs LFS, or contacts
submodules. Gitlinks update the index while existing submodule contents remain
untouched. Relative symlinks are supported when their lexical target stays in
the worktree and avoids `.git`; no filesystem operation follows them. Absolute
or escaping incoming symlinks, metadata paths, special files, file/directory
transitions, and submodule/file transitions are refused before checkout changes.

## Compatibility and output

Git's plain English summaries and stream placement are reproduced: fetch and
push report on stderr; fast-forward merge and diffstat report on stdout;
unchanged fetch is silent. Regression tests compare stdout and stderr separately
against Git for new/updated/rewound fetches, merge and push, no-ops, quiet mode,
and text/binary/mode-change statistics. Fatal runtime failures use stderr and
exit status 128. Usage errors use the CLI parser's own diagnostics.

Intentional output differences:

- Repository/global output configuration, attributes, diff drivers, textconv,
  colors, translations, and pagers are not evaluated. No remote progress stream
  is printed; protocol validation failures use escaped diagnostics.
- Diffstat uses raw blobs, a fixed 80-column layout, seven-character commit IDs,
  and a bounded exact line edit count. Rename/copy detection and Git's heuristic
  diff optimizations are absent, so some statistics differ. Git's custom
  abbreviations, wide-terminal formatting, and custom display options are ignored.
- Diffstat is omitted with a warning if required historical blobs are absent or
  it exceeds 16 MiB of blob input, 200,000 lines per pair, 8 million comparisons,
  or 10,000 changed paths. The successful checkout remains successful. Use
  `--no-stat` to skip this optional work explicitly.
- Safety refusals, unsupported features, and recovery have specific diagnostics.
  Errors escape control characters to protect the terminal. There is no
  byte-for-byte promise for every Git failure or optional progress/advice message.

Git index versions 2, 3, and 4 are read; updates write v2 with fresh stat-cache
entries. Split/sparse indexes, assume-unchanged and extended entry flags,
unmerged indexes, shallow history, alternates, grafts, replacements, linked
worktrees, gitdir files, and repository extensions are refused. Other config
sections and includes are inert; only repository format, bare state, and unsafe
worktree redirection are checked. `GIT_*` environment variables are ignored.

Local pack indexes permit lazy, hash-checked object decoding. A partial clone
can work if the commit history and objects required by the specific operation
are present. Missing historical blobs unrelated to that operation need not be
loaded. There is no automatic promisor-object fetching. Fetch negotiates existing
commit tips and accepts non-thin packs; push sends reachable content for commits
after the destination boundary in a non-delta pack, potentially resending
unchanged trees and blobs. A new destination may need all history and exceed the
limits.

Disposable snapshots of `snakes-telegram` (branch `master`) and `EKA2L1` (branch
`wasm-port`, including its partial object store) passed the full SSH fetch,
working-tree fast-forward, and push sequence with a synthetic new commit. Their
original checkouts were not changed. The nested
`snakes-telegram/vendor/eka2l1` checkout uses a gitdir file and shallow metadata;
using it directly as `-C` is unsupported. Updating its parent gitlink is supported.

## Filesystem updates and recovery

Repository-internal accesses walk open directory descriptors with `O_NOFOLLOW`.
New files and atomic renames avoid overwriting existing hardlinked objects,
refs, index files, or worktree files in place. Object hashes use collision-detecting
SHA-1. Ref updates take Git's `<ref>.lock` and compare the previous ID. A
nonblocking `flock` on `.safe-git.lock` serializes safe-git processes; the OS
releases it on exit, though the lock file remains.

Worktree updates preflight changed paths, acquire the index and branch locks,
and stage new content plus the old index under `.git/safe-git-checkout`. Original
files are renamed into backups. The new index is committed before the branch
ref, which is committed last. Failed application attempts recovery automatically;
a process crash leaves the journal for `safe-git recover`. Recovery checks the
recorded branch, index hashes, backup contents, and lock identities. Later user
edits block conflicting restoration and retain the backups. A crash before a
complete journal is durable, interrupted recovery itself, or damaged metadata
may require manual recovery. Preserve the transaction directory in those cases.

This is recoverable multi-file updating, not an atomic filesystem snapshot. The
three commands are separate operations. Fetch failures can leave validated
unreachable objects. A push error after sending data may follow a successful
server update; a lost acknowledgement cannot prove the destination is unchanged.

## Security limits

Fixed bounds include 64 MiB per object, 200,000 objects per graph/pack, 256 MiB per
network pack and combined local compressed packs, 512 MiB decoded incoming data
and changed checkout content, 128 MiB local object cache, and delta depth 64.
Other metadata and advertisements are bounded. Buffers and graph structures can
coexist; these are not a hard process-memory limit. Compression uses the Rust
`miniz_oxide` backend. `--timeout` defaults to 120 seconds (maximum 3,600); SSH
inactivity is limited to 30 seconds. Synchronous parsing is bounded by input
sizes and is not preempted by the asynchronous deadline.

There is no OS sandbox. Same-user adversarial processes, concurrent directory
replacement, hostile mounts/filesystems, compromised credentials, dependency
vulnerabilities, and full resource-exhaustion resistance are not covered. Do not
concurrently mutate the checkout using ordinary Git or another tool ignoring the
safe-git lock. Working-tree entries and metadata must be on the same filesystem.
The crate forbids its own `unsafe` code; dependencies remain part of the trusted
code, and a compromised process retains its user's permissions.

Remote upload/receive services still run on the server. Push can trigger server
hooks or CI. Files transferred here can affect ordinary Git, editors, build tools,
or other programs if you subsequently choose to run those programs against them.

## Validation

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 tests/integration.py
python3 tests/worktree.py
python3 tests/output.py
```

The suites require Git, Python, OpenSSH tools, and passwordless sudo for a
loopback-only temporary sshd (or root). Temporary repositories and generated keys
are removed afterward. With `strace`, they verify no child executable is launched
by fetch, working-tree/bare merge, or push.

Coverage includes real Git interoperability, packed refs and both delta forms,
all-parent ancestry, destination races/server rejection, hostile execution
configuration, malformed packets/objects/indexes, SSH host/identity checks,
symlink escapes, preservation of local edits, index v4, and SIGKILL recovery that
protects later user edits. Tests do not replace fuzzing and independent review.

Format references: [pack protocol](https://git-scm.com/docs/pack-protocol),
[pack format](https://git-scm.com/docs/pack-format),
[index format](https://git-scm.com/docs/gitformat-index), and
[diff output](https://git-scm.com/docs/git-diff).
