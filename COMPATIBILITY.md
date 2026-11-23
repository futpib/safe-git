# Git compatibility

Everyday editing includes init, clone/fetch/pull/push, branches, switch, status,
add/restore (including selected hunks), rm/mv/clean, commit/amend, reset, reflog,
and stash. Directory moves preserve untracked files and unstaged edits; clean
requires an explicit force request or dry run.
Status accepts paths. Commits accept message files/stdin and --fixup; global
config reads inert values and writes user.name/user.email without needing a repo.
Staging supports `add -N`; commits accept paths, explicit `--author`, and
`--allow-empty`. File restores also accept `reset -- PATH` and `checkout -- PATH`.
Stash supports selected paths, `--patch`, `--staged`, and `--keep-index`.

Inspection includes show, blame, path-filtered log, --all/--graph/--decorate,
--author/--grep filters, tracked-file grep, diff --check/--name-status, three-dot
diffs, and lightweight/annotated tag creation, listing, and deletion.
Merge, cherry-pick, revert, and rebase support conflicts and continue/abort.
Pull supports --rebase; rebase supports --onto, --autosquash, and --rebase-merges
for two-parent merges. Interactive rebase displays its proposed plan and reads
pick/reword/edit/squash/fixup/drop/break and label/reset/merge actions from stdin.
Enter `done` to submit; `done` alone keeps the proposed plan. Reword messages go
on the action line; `merge -c` takes `# MESSAGE`. No editor or exec action runs.
Plain rebase flattens merges. Bisect
supports start/good/bad/skip/log/reset, without executing test commands.

Apply handles unified text and Git binary patches, --check, --index, --cached,
and --reverse. Format-patch exports mailboxes or files; am imports UTF-8 inline
mail and supports --continue/--skip/--abort. Encoded MIME bodies/headers, copy
patches, and gitlink patches are rejected. Patch paths use checkout's safeguards.

Clone --depth negotiates shallow history; fetch --unshallow restores it.
Fetch --depth/--deepen adjusts the boundary. Partial clones support
`--filter=blob:none` and `blob:limit=N`, fetching missing blobs only from a
host-registered promisor. Sparse checkout supports cone and non-cone patterns
with ordinary indexes; compressed sparse indexes remain unsupported.
Fetch supports SSH, HTTP(S), native git://, and local objects. Push supports
SSH/HTTP(S), branches/tags, deletion, explicit force, and force-with-lease.
Push --all selects every local branch; --atomic requires server support and
updates all requested refs together or none.
Push --dry-run checks the destination without sending updates or saving upstreams.
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
