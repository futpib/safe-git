#!/usr/bin/env python3
"""Compare stdout and stderr separately against real Git in disposable repos."""
import os
import shlex
import subprocess
import integration as h


def suite(root, port, user):
    seed, source, dest, work, oracle = [root / n for n in
                                       ('seed', 'source.git', 'dest.git', 'work', 'oracle')]
    def git(repo, *args): return h.git(repo, *args)
    def url(repo): return f'ssh://{user}@127.0.0.1:{port}{repo}'
    ssh = shlex.join(['ssh', '-F', '/dev/null', '-i', str(root/'client_key'),
                     '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes',
                     '-o', f'UserKnownHostsFile={root}/known_hosts',
                     '-o', 'StrictHostKeyChecking=yes'])
    env = h.ENV | {'LC_ALL': 'C', 'GIT_SSH_COMMAND': ssh,
                   'SAFE_GIT_IDENTITY_FILE': str(root/'client_key'),
                   'SAFE_GIT_URL': url(source)}
    def capture(args, **extra):
        r = subprocess.run(list(map(str, args)), env=env | extra,
                           capture_output=True, text=True, timeout=40)
        return r.returncode, r.stdout, r.stderr
    def safe(*args, **extra):
        return capture([h.BIN, '-C', work, '--known-hosts', root/'known_hosts', *args], **extra)
    def real(*args):
        return capture(['git', '-c', 'core.hooksPath=/dev/null', '-C', oracle, *args])
    def equal(label, got, expected):
        assert got == expected, (label, got, expected)
        assert got[0] == 0, (label, got)
        h.passed(label)

    git(root, 'init', '-q', '-b', 'main', seed)
    git(seed, 'config', 'user.name', 'test')
    git(seed, 'config', 'user.email', 'test@example.invalid')
    (seed/'a').write_text('one\ntwo\n')
    (seed/'gone').write_text('gone\n')
    (seed/'mode').write_text('mode\n')
    (seed/'binary').write_bytes(b'a\x00b')
    git(seed, 'add', '.')
    git(seed, 'commit', '-qm', 'base')
    base = git(seed, 'rev-parse', 'HEAD')
    (seed/'a').write_text('three\ntwo\nfour\n')
    (seed/'gone').unlink()
    (seed/'mode').chmod(0o755)
    (seed/'new').write_text('new\n')
    (seed/'binary').write_bytes(b'a\x00bc')
    (seed/'café').write_text('non-ASCII path\n')
    git(seed, 'add', '.')
    git(seed, 'commit', '-qm', 'forward')
    forward = git(seed, 'rev-parse', 'HEAD')
    git(root, 'clone', '-q', '--bare', seed, source)
    git(seed, 'reset', '--hard', base)
    git(root, 'clone', '-q', '--bare', seed, dest)
    for repo in (work, oracle):
        git(root, 'clone', '-q', '--no-hardlinks', seed, repo)
        git(repo, 'remote', 'set-url', 'origin', url(source))
        git(repo, 'update-ref', '-d', 'refs/remotes/origin/main')

    equal('new-branch fetch output and streams; caller URL/key environment', safe('fetch'), real('fetch', 'origin'))
    equal('unchanged fetch is silent', safe('fetch'), real('fetch', 'origin'))
    equal('fast-forward output including text/binary/mode/path statistics',
          safe('merge', '--ff-only', 'origin/main'), real('merge', '--ff-only', 'origin/main'))
    equal('up-to-date merge stdout', safe('merge', '--ff-only', 'origin/main'), real('merge', '--ff-only', 'origin/main'))
    got = safe('push', SAFE_GIT_URL=url(dest))
    git(dest, 'update-ref', 'refs/heads/main', base)
    equal('fast-forward push stderr', got, real('push', url(dest), 'main'))
    equal('up-to-date push stderr', safe('push', SAFE_GIT_URL=url(dest)), real('push', url(dest), 'main'))
    got = safe('push', url(dest), 'main:other')
    git(dest, 'update-ref', '-d', 'refs/heads/other')
    equal('new-branch push stderr', got, real('push', url(dest), 'main:other'))
    git(source, 'update-ref', 'refs/heads/main', base)
    equal('forced tracking update stderr', safe('fetch'), real('fetch', 'origin'))
    equal('incoming ancestor merge stdout', safe('merge', '--ff-only', 'origin/main'), real('merge', '--ff-only', 'origin/main'))
    git(source, 'update-ref', 'refs/heads/main', forward)
    equal('fast-forward fetch stderr', safe('fetch'), real('fetch', 'origin'))
    for repo in (work, oracle): git(repo, 'reset', '--hard', base)
    equal('no-stat merge output', safe('merge', '--ff-only', '--no-stat', 'origin/main'), real('merge', '--ff-only', '--no-stat', 'origin/main'))
    equal('quiet merge output', safe('merge', '--ff-only', '-q', 'origin/main'), real('merge', '--ff-only', '-q', 'origin/main'))
    equal('quiet push output', safe('push', '-q', SAFE_GIT_URL=url(dest)), real('push', '-q', url(dest), 'main'))
    failed = safe('fetch', 'https://example.invalid/repo', 'main')
    assert failed[0] == 128 and failed[1] == '' and failed[2].startswith('fatal: ')
    h.passed('safety refusal is fatal on stderr with no stdout')

    # Optional presentation must neither perform unbounded quadratic work nor
    # turn a completed, valid checkout into an apparent failure.
    git(seed, 'reset', '--hard', forward)
    (seed/'big').write_text('old\n' * 3000)
    git(seed, 'add', 'big'); git(seed, 'commit', '-qm', 'large base')
    large_base = git(seed, 'rev-parse', 'HEAD')
    git(work, 'fetch', str(seed), 'main')
    git(work, 'reset', '--hard', large_base)
    (seed/'big').write_text('new\n' * 3000)
    git(seed, 'commit', '-qam', 'large forward')
    large_forward = git(seed, 'rev-parse', 'HEAD')
    git(seed, 'push', str(source), 'main')
    assert safe('fetch')[0] == 0
    result = safe('merge', '--ff-only', 'origin/main')
    assert result == (0, f'Updating {large_base[:7]}..{large_forward[:7]}\nFast-forward\n',
                      'warning: safe-git: diffstat omitted (diffstat comparison limit exceeded)\n'), result
    assert git(work, 'rev-parse', 'HEAD') == large_forward
    assert (work/'big').read_text() == 'new\n' * 3000
    assert git(work, 'status', '--porcelain') == ''
    h.passed('diffstat resource limit gives a warning while checkout succeeds')


if __name__ == '__main__':
    h.suite = suite
    h.main()
