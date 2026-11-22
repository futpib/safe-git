# Git compatibility

The everyday workflow includes working-tree/bare `init`, clone/fetch/pull/push,
branch listing/creation/rename/deletion, switch (including `-` and tracking),
status, add, restore, diff, log, commit/amend, reset, reflog, and stash.
Add/restore accept relative paths, wildcard pathspecs, and built-in `-p` hunk
selection. Status includes renames, ahead/behind counts, and porcelain v1/v2.
Stash supports untracked/ignored files, older entries, show/apply/pop/drop/clear.

Merge, cherry-pick, revert, and rebase support conflicts: edit the files, `add`
them, then use the operation's `--continue`, `--skip` (where applicable), or
`--abort`. Conflicts use Git index stages. Interactive rebase reads a plan from
stdin: `pick`, `reword`, `edit`, `squash`, `fixup`, or `drop` followed by a commit
and optional message; end with `done`. No editor or `exec` instruction runs.
Rebase flattens local merges. Interrupted checkout updates use `recover`.

Fetch supports SSH, HTTP(S), native `git://`, and local objects. Push supports
SSH and HTTP(S), branch/tag refspecs, `--tags`, deletion, explicit `--force`, and
`--force-with-lease[=REF:OID]`. Without an explicit force request, existing
branches must fast-forward and existing tags cannot be replaced. An optional
caller-provided `SAFE_GIT_HTTP_TOKEN` is sent as a bearer token only over HTTPS.
Redirects and credential helpers are never followed.

Clone registers the endpoint outside the repository in trusted host state.
`push -u origin BRANCH` records its upstream there; subsequent push/pull use
that mapping. Existing Git-created repositories require an explicit URL or
registration with `safe-git config remote.origin.url URL`. Repository remote
URLs, upstream settings, and URL rewrites cannot redirect network operations.

Paru's `--git /path/to/safe-git` selects this client; makepkg selects `git`
through `PATH`. Their clone/fetch/review/rebase, mirror/prune/checkout/archive,
and common `pkgver()` commands are covered by integration tests.

Hooks, filters, aliases, signers, includes, external diffs, merge drivers,
submodule commands, and executable sequencer instructions remain unsupported.
Ignore files are bounded inert patterns. Unsafe paths, escaping symlinks,
external hard links, unsupported repository formats, and file/directory
transitions are rejected. Unknown options fail rather than invoking Git.
