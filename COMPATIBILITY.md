# Package-manager compatibility

Supported: paru's AUR clone/fetch, review diff, seen marker, saved-edit commit
and clean rebase; makepkg's Git mirror, prune, local clone, tag/branch/commit
checkout and tar checksum workflow. `rev-list --count`, `rev-parse --short`,
`describe`, and basic `log` support common `pkgver()` functions.

Transfers use SSH, smart HTTP(S), native `git://`, or verified local objects.
HTTP uses the host's trusted CA certificates. There are no HTTP credential
helpers, authentication prompts, redirects, or subprocess fallbacks.

`clone` records the endpoint outside the repository under
`${XDG_STATE_HOME:-~/.local/state}/safe-git` (or `SAFE_GIT_STATE_DIR`). Records bind
the endpoint to the metadata directory's location and filesystem identity.
Changing `.git/config` cannot change that endpoint. A Git-created or relocated
cache needs explicit registration before a URL-less fetch:

```sh
safe-git -C /path/to/cache config remote.origin.url https://aur.archlinux.org/package.git
```

Paru's `--git /path/to/safe-git` selects its own client. Makepkg invokes `git`
through `PATH`; testing/replacement must also provide a `git` symlink to safe-git
in a caller-controlled directory on that path. This repository does not install
that symlink or modify either application's configuration.

Deliberate differences:

- `clone -s` copies verified objects; it never creates or follows alternates.
- Attributes, hooks, includes, helpers, aliases, pagers, signers, merge drivers,
  submodule commands and sequencer scripts are not implemented.
- Merge/rebase conflicts and local merge-commit rebases fail before changing the
  checkout. Review merges require `--no-ff --no-commit`; ordinary merges require
  `--ff-only`. Push still requires an explicit SSH endpoint and is fast-forward only.
- Checkout/reset reject unsafe paths, escaping symlinks, external hard links,
  file/directory transitions and unsupported index/metadata formats. Commands
  operate at the repository root and retain bounded parsing/traversal budgets.
- Text output is plain and escapes repository-supplied terminal controls.
  Tar output preserves Git's bytes for makepkg, with attributes ignored.

This is the package-manager command subset, not every command an arbitrary
PKGBUILD can request. Unknown commands and options fail instead of invoking Git.
