#!/usr/bin/env python3
"""Working-tree regression tests using Git as an independent format oracle."""
import hashlib
import json
import os
from pathlib import Path
import shlex
import signal
import shutil
import subprocess
import time
import integration as h


def suite(root, port, user):
    seed, source, dest = [root / n for n in ('seed', 'source.git', 'dest.git')]
    def git(repo, *args, **kw):
        return h.git(repo, '-c', 'core.fsmonitor=false', '-c', 'filter.probe.process=',
                     '-c', 'filter.probe.clean=', '-c', 'filter.probe.smudge=', *args, **kw)
    def ref(repo): return git(repo, 'rev-parse', 'HEAD')
    def url(repo): return f'ssh://{user}@127.0.0.1:{port}{repo}'
    def safe(repo, *args, ok=True):
        return h.run([h.BIN, '-C', repo, '--identity-file', root/'client_key',
                      '--known-hosts', root/'known_hosts', *args], ok=ok)
    git(root, 'init', '-q', '-b', 'main', seed)
    git(seed, 'config', 'user.name', 'safe-git test')
    git(seed, 'config', 'user.email', 'test@example.invalid')
    for name in ('payload', 'unstaged', 'staged', 'gone', 'executable'):
        (seed/name).write_text(name+' base\n')
    (seed/'.gitattributes').write_text('payload filter=probe\n')
    (seed/'link').symlink_to('payload')
    for i in range(300): (seed/f'bulk-{i:03}').write_text('old\n')
    git(seed, 'add', '.')
    git(seed, 'commit', '-qm', 'base')
    base = ref(seed)
    git(seed, 'update-index', '--add', '--cacheinfo', f'160000,{base},module')
    git(seed, 'commit', '-qm', 'gitlink')
    base = ref(seed)
    (seed/'payload').write_text('payload forward\n')
    (seed/'gone').unlink()
    (seed/'executable').chmod(0o755)
    (seed/'newdir').mkdir()
    (seed/'newdir/file').write_text('new file\n')
    (seed/'link').unlink(); (seed/'link').symlink_to('newdir/file')
    for i in range(300): (seed/f'bulk-{i:03}').write_text('new\n')
    git(seed, 'add', '.')
    git(seed, 'update-index', '--add', '--cacheinfo', f'160000,{base},module')
    git(seed, 'commit', '-qm', 'forward')
    forward = ref(seed)
    git(seed, 'branch', 'future', forward)
    git(seed, 'reset', '--hard', base)
    for repo in (source, dest): git(root, 'clone', '-q', '--bare', seed, repo)
    git(source, 'update-ref', 'refs/heads/main', forward)
    serial = 0
    def clone():
        nonlocal serial
        serial += 1
        work = root/f'work-{serial}'
        git(root, 'clone', '-q', '--no-hardlinks', seed, work)
        git(work, 'update-ref', 'refs/remotes/origin/main', forward)
        return work
    def unchanged(work, action):
        before_index = (work/'.git/index').read_bytes()
        before_ref = ref(work)
        output = action()
        assert ref(work) == before_ref
        assert (work/'.git/index').read_bytes() == before_index
        assert not (work/'.git/safe-git-checkout').exists()
        return output

    work = clone()
    (work/'unstaged').write_text('local unstaged edit\n')
    (work/'staged').write_text('local staged edit\n')
    git(work, 'add', 'staged')
    (work/'untracked').write_text('keep me\n')
    (work/'module').mkdir(exist_ok=True)
    (work/'module/dirty').write_text('submodule working files stay untouched\n')
    marker = root/'EXECUTED'
    probe = root/'probe'
    probe.write_text(f'#!/bin/sh\ntouch {shlex.quote(str(marker))}\nexit 91\n')
    probe.chmod(0o755)
    for name in ('post-merge', 'reference-transaction', 'pre-push', 'post-checkout'):
        target = work/'.git/hooks'/name
        target.write_text(probe.read_text()); target.chmod(0o755)
    with (work/'.git/config').open('a') as f:
        f.write(f'[core]\nsshCommand = {probe}\nfsmonitor = {probe}\n[filter "probe"]\nprocess = {probe}\nsmudge = {probe}\nclean = {probe}\n[credential]\nhelper = !{probe}\n')
    before = (work/'payload').read_bytes()
    safe(work, 'fetch', url(source), 'main')
    assert ref(work) == base and (work/'payload').read_bytes() == before
    if shutil.which('strace'):
        trace = root/'worktree-exec.trace'
        h.run(['strace', '-f', '-e', 'trace=execve,execveat', '-o', trace,
               h.BIN, '-C', work, 'merge', '--ff-only', 'origin/main'])
        executions = [line for line in trace.read_text().splitlines() if 'execve' in line]
        assert len(executions) == 1 and str(h.BIN) in executions[0], executions
    else:
        safe(work, 'merge', '--ff-only', 'origin/main')
    assert ref(work) == forward
    assert (work/'payload').read_text() == 'payload forward\n'
    assert (work/'newdir/file').read_text() == 'new file\n'
    assert not (work/'gone').exists() and (work/'executable').stat().st_mode & 0o111
    assert os.readlink(work/'link') == 'newdir/file'
    assert (work/'unstaged').read_text() == 'local unstaged edit\n'
    assert (work/'staged').read_text() == 'local staged edit\n'
    assert (work/'untracked').read_text() == 'keep me\n'
    assert (work/'module/dirty').read_text() == 'submodule working files stay untouched\n'
    assert git(work, 'diff', '--cached', '--name-only') == 'staged'
    assert set(git(work, 'diff', '--name-only').splitlines()) == {'unstaged'}
    safe(work, 'push', url(dest), 'main')
    assert ref(dest) == forward and not marker.exists()
    h.passed('working-tree SSH fetch/ff-merge/push updates files and index; unrelated local edits survive; execution settings inert')

    for staged in (False, True):
        work = clone(); (work/'payload').write_text('local conflicting edit\n')
        if staged: git(work, 'add', 'payload')
        unchanged(work, lambda: safe(work, 'merge', '--ff-only', 'origin/main', ok=False))
        assert (work/'payload').read_text() == 'local conflicting edit\n'
    work = clone(); (work/'newdir').mkdir(); (work/'newdir/file').write_text('untracked\n')
    unchanged(work, lambda: safe(work, 'merge', '--ff-only', 'origin/main', ok=False))
    assert (work/'newdir/file').read_text() == 'untracked\n'
    h.passed('overlapping staged/unstaged changes and untracked files rejected without mutation')

    work = clone(); outside=root/'outside'; outside.mkdir(); (work/'newdir').symlink_to(outside)
    unchanged(work, lambda: safe(work, 'merge', '--ff-only', 'origin/main', ok=False))
    assert list(outside.iterdir()) == []
    h.passed('symlink directory cannot redirect a checkout outside the worktree')

    for target in ('/tmp/outside', '../escape', '.git/config'):
        blob=git(seed, 'hash-object', '-w', '--stdin', data=target)
        tree=git(seed, 'mktree', data=f'120000 blob {blob}\tlink\n')
        bad=git(seed, 'commit-tree', tree, '-p', base, data='unsafe symlink\n')
        git(seed, 'update-ref', 'refs/heads/bad', bad)
        work=clone();git(work, 'update-ref', 'refs/remotes/origin/main', bad)
        unchanged(work, lambda: safe(work, 'merge', '--ff-only', 'origin/main', ok=False))
    tree=git(seed, 'rev-parse', f'{base}^{{tree}}')
    malicious=git(seed, 'mktree', data=f'040000 tree {tree}\t.git\n')
    bad=git(seed, 'commit-tree', malicious, '-p', base, data='metadata attack\n')
    git(seed, 'update-ref', 'refs/heads/bad', bad)
    work=clone();git(work, 'update-ref', 'refs/remotes/origin/main', bad)
    unchanged(work, lambda: safe(work, 'merge', '--ff-only', 'origin/main', ok=False))
    h.passed('incoming .git paths and escaping/metadata symlinks rejected before checkout')

    work=clone();git(work, 'update-index', '--index-version=4')
    safe(work, 'merge', '--ff-only', 'origin/main')
    assert ref(work)==forward and not git(work, 'status', '--porcelain')
    h.passed('Git index v4 is read and replaced by a valid v2 index')

    work=clone();original_index=(work/'.git/index').read_bytes()
    process=subprocess.Popen([str(h.BIN), '-C', str(work), 'merge', '--ff-only', 'origin/main'],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=h.ENV)
    txn=work/'.git/safe-git-checkout'
    stopped=False
    for _ in range(10000):
        if (txn/'old-0').exists():
            os.kill(process.pid,signal.SIGSTOP);stopped=True;break
        if process.poll() is not None: break
        time.sleep(0.0005)
    assert stopped, process.communicate()
    os.kill(process.pid,signal.SIGKILL);process.wait(timeout=10)
    assert ref(work)==base and txn.exists()
    safe(work, 'fetch', url(source), 'main', ok=False)
    # A later user edit must survive even if recovery has restored other files.
    journal = json.loads((txn/'journal').read_text())
    first = work/os.fsdecode(bytes(journal['changes'][0]['path']))
    first.write_text('new user edit after crash\n')
    safe(work, 'recover', ok=False)
    assert first.read_text() == 'new user edit after crash\n' and (txn/'old-0').exists()
    first.unlink()
    safe(work, 'recover')
    assert ref(work)==base and (work/'.git/index').read_bytes()==original_index
    assert (work/'bulk-000').read_text()=='old\n'
    assert not txn.exists() and not (work/'.git/index.lock').exists()
    assert not git(work,'status','--porcelain')
    h.passed('SIGKILL mid-checkout preserves backups; recover restores files, index, and locks')


if __name__ == '__main__':
    h.suite=suite
    h.main()
