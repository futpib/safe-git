# safe-git

A Git CLI that **does not trust the repository it works on**. Its purpose is to
fetch, fast-forward, and push without executing anything an attacker puts in the
repository.

Git and SSH are implemented in-process in Rust. There is no shell, subprocess,
or fallback to Git. Hooks, filters, command aliases, credential helpers, and
external diff tools are never run. Repository content and metadata are treated
as untrusted data, never instructions to execute.

You choose the remote explicitly; repository configuration cannot redirect
authenticated connections or pushes.

Install from this source checkout on Linux with Rust:

```sh
cargo install --locked --path .
```

Use an existing working tree, choosing the URL yourself:

```sh
export SAFE_GIT_URL=git@github.com:owner/repo.git
cd /path/to/repo
safe-git fetch
safe-git merge --ff-only origin/main  # replace main with your current branch
safe-git push
```

SSH uses your agent or default keys in `~/.ssh`; a verified host key must already
be in `~/.ssh/known_hosts`. Your host environment and credentials are trusted.
Run `safe-git --help` for options, bare-repository initialization, and checkout
recovery. Only this small subset of Git operations is supported.

Experimental and unaudited. Preventing repository-directed execution is the
design, not a guarantee against implementation or dependency bugs. There is no
OS sandbox, and pushes can still trigger hooks or CI on the remote server.
