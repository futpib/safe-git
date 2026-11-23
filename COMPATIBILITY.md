# Git compatibility

SHA-1 and SHA-256 repositories are supported; `init --object-format=sha256`
selects SHA-256, and clone preserves the advertised format. Mixed formats are rejected.

Everyday editing includes init, clone/fetch/pull/push, branches, switch, status,
add/restore (including interactive file selection and split/edited hunks), rm/mv/clean, commit/amend, reset, reflog,
and stash. Directory moves preserve untracked files and unstaged edits; clean
requires an explicit force request or dry run.
Status accepts paths. Commits accept message files/stdin and --fixup; global
config reads inert values and writes user.name/user.email without needing a repo.
Staging supports `add -N`; commits accept paths, explicit `--author`, and
`--allow-empty`. File restores also accept `reset -- PATH` and `checkout -- PATH`.
Stash supports selected paths, `--patch`, `--staged`, and `--keep-index`.

Inspection includes show, blame, path-filtered log, --all/--graph/--decorate,
--author/--grep/--since filters, log patches/--follow, tracked-file grep,
word/whitespace/rename diffs, diff --check/--name-status, three-dot
diffs, and lightweight/annotated tag creation, listing, and deletion.
Merge, cherry-pick, revert, and rebase support conflicts and continue/abort.
Merges combine multiple merge bases recursively; unrelated roots require
`merge --allow-unrelated-histories`. Checkout handles file/directory transitions
without discarding untracked contents.
Pull supports --rebase; rebase supports --root, --autostash, --onto, --autosquash,
and --rebase-merges, including conflict-free octopus merges. Interactive rebase
displays its proposed plan and reads
pick/reword/edit/squash/fixup/drop/break and label/reset/merge actions from stdin.
Enter `done` to submit; `done` alone keeps the proposed plan. Reword messages go
on the action line; `merge -c` takes `# MESSAGE`. No editor or exec action runs.
Plain rebase flattens merges. Bisect
supports start/good/bad/skip/log/reset, without executing test commands.

Apply handles unified text and Git binary patches, --check, --index, --cached,
and --reverse, including copy and inert gitlink changes. Format-patch exports
mailboxes or files; am decodes MIME headers/bodies and supports
--continue/--skip/--abort. Attachment names are never used as paths.

Clone --depth negotiates shallow history; fetch --unshallow restores it.
Fetch --depth/--deepen adjusts the boundary. Partial clones support
`--filter=blob:none` and `blob:limit=N`, fetching missing blobs only from a
host-registered promisor. Sparse checkout supports cone and non-cone patterns
with ordinary, split, and compressed sparse indexes. Shared indexes are hash
verified and opened without following links; edits write ordinary indexes.
Fetch supports SSH, smart or static HTTP(S), native git://, and local objects.
Push supports SSH/HTTP(S), git://, local paths/file://, branches/tags, deletion,
explicit force, and force-with-lease. Local pushes never run destination hooks
and refuse checked-out branches. Static HTTP verifies loose/packed objects;
alternates cannot redirect fetching, and shallow/filter negotiation needs smart Git.
Empty static repositories must publish their config to identify the object format.
Push --all selects every local branch; --atomic requires server support and
updates all requested refs together or none. Local pushes do not support --atomic.
Push --dry-run checks the destination without sending updates or saving upstreams.
HTTPS authentication accepts caller-provided SAFE_GIT_HTTP_TOKEN (Bearer) or
SAFE_GIT_HTTP_USERNAME plus SAFE_GIT_HTTP_PASSWORD (Basic, including PATs).
Credentials are never sent over plain HTTP. Redirects, URL credentials, and
credential helpers are never followed.

SSH uses keys, an agent, or caller-provided SAFE_GIT_SSH_PASSWORD.
Host aliases, HostName/User/Port, IdentityFile/IdentitiesOnly, UserKnownHostsFile,
and ProxyJump come only from ~/.ssh/config or caller-selected SAFE_GIT_SSH_CONFIG.
Jump hosts use their own key settings or SAFE_GIT_SSH_JUMP_PASSWORD. Every hop
requires a verified key or host certificate; known_hosts CA/revocation markers
and hashed names are supported. Include/Match blocks are not evaluated, and
ProxyCommand/LocalCommand/KnownHostsCommand cannot run. No password prompts or
repository-selected SSH commands execute. FTP/FTPS and rsync are unsupported.

Remote add/set-url/get-url/rename/remove/show/update and fetch --all use
host-owned registrations. Clone and push -u remember endpoints/upstreams there.
Existing repositories need an explicit URL or registration with remote add or
config remote.NAME.url. Repository URLs and rewrites never select endpoints.

Worktree add/list/remove/prune/move/lock/unlock/repair uses shared objects and
refs, with host records binding directories to their locations and identities.
Repair accepts moved directories only when their device/inode matches a host
registration. Unregistered gitdir/commondir pointers are refused. Submodule add
accepts a caller URL; existing gitlinks need `submodule init --url URL -- PATH` before
`submodule update`. Recursive updates require each submodule endpoint to be registered individually;
.gitmodules URLs and update commands are never followed. Status and deinit are
supported; deinit retains local repositories in .git/safe-git-submodules.

`fsck` verifies local object hashes and graph references without fetching. `gc`
packs verified objects; `gc --prune=now` explicitly removes unreachable objects,
protecting refs, reflogs, indexes, linked worktrees, and recorded resolutions.
`reflog expire --expire=now --all` explicitly expires reflogs. Pruning refuses
active operations, unregistered worktrees, and kept/promisor packs. `commit -S` signs
in-process with an OpenSSH key selected by --signing-key or SAFE_GIT_SIGNING_KEY.
Signed tags use `tag -s` with the same key selection. `verify-commit` and
`verify-tag` require an OpenSSH public key selected by `--verification-key` or
`SAFE_GIT_VERIFICATION_KEY`; repository settings cannot select keys or trust.

Bundle create/verify/list-heads/unbundle supports SHA-1/SHA-256, prerequisites,
and thin packs resolved from verified local objects. Notes support list/show,
add/append/edit/copy/remove with explicit messages. Range-diff pairs patches and
shows changes without external diffs. Explicit `rerere` records and reuses
resolutions; status/remaining/diff/forget/clear manage its separate bounded cache.
Reused resolutions remain unstaged for review.

Paru/makepkg workflows remain covered. Hooks, filters, aliases, external signers,
includes, external diffs, merge drivers, and executable sequencer instructions
remain unsupported. Filesystem access rejects escaping symlinks, external hard
links, unsafe paths, and unsupported formats. Parsing/traversal stays bounded;
unknown commands/options fail without invoking Git. Interrupted file/index
updates use recover.
