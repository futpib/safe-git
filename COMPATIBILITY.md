# Git compatibility

Everyday editing includes init, clone/fetch/pull/push, branches, switch, status,
add/restore (including selected hunks), rm/mv/clean, commit/amend, reset, reflog,
and stash. Directory moves preserve untracked files and unstaged edits; clean
requires an explicit force request or dry run.

Inspection includes show, blame, path-filtered log, --all/--graph/--decorate,
three-dot diffs, and lightweight/annotated tag creation, listing, and deletion.
Merge, cherry-pick, revert, and rebase support conflicts and continue/abort.
Pull supports --rebase; rebase supports --onto and --autosquash. Interactive
rebase reads pick/reword/edit/squash/fixup/drop plans from stdin; it never opens
an editor or executes a plan command. Rebase flattens local merges. Bisect
supports start/good/bad/skip/log/reset, without executing test commands.

Clone --depth negotiates shallow history; fetch --unshallow restores it.
Fetch supports SSH, HTTP(S), native git://, and local objects. Push supports
SSH/HTTP(S), branches/tags, deletion, explicit force, and force-with-lease.
Caller-provided SAFE_GIT_HTTP_TOKEN is sent only over HTTPS. Redirects and
credential helpers are never followed.

Remote add/set-url/get-url/rename/remove/show/update and fetch --all use
host-owned registrations. Clone and push -u remember endpoints/upstreams there.
Existing repositories need an explicit URL or registration with remote add or
config remote.NAME.url. Repository URLs and rewrites never select endpoints.

Worktree add/list/remove/prune uses shared objects and refs, with host records
binding linked directories to their locations and filesystem identities.
Unregistered gitdir/commondir pointers are refused. Submodule add accepts a
caller URL; existing gitlinks need `submodule init --url URL -- PATH` before
`submodule update`. Each submodule endpoint is registered individually;
.gitmodules URLs and update commands are never followed. Status and deinit are
supported; deinit retains local repositories in .git/safe-git-submodules.

Paru/makepkg workflows remain covered. Hooks, filters, aliases, signers,
includes, external diffs, merge drivers, and executable sequencer instructions
remain unsupported. Filesystem access rejects escaping symlinks, external hard
links, unsafe paths, and unsupported formats. Parsing/traversal stays bounded;
unknown commands/options fail without invoking Git. Interrupted file/index
updates use recover.
