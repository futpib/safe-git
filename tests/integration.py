#!/usr/bin/env python3
"""End-to-end tests with OpenSSH and Git as an independent server/oracle.

Requires Python 3, git, ssh-keygen, sshd, and passwordless sudo for a temporary
loopback-only sshd (or run as root). All repository writes stay in a temp dir.
Usage: cargo build --locked && python3 tests/integration.py [binary]
"""
import fcntl
import os
from pathlib import Path
import pwd
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

BIN = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/safe-git").resolve()
ENV = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
ENV.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_TERMINAL_PROMPT="0")
CHECKS = 0


def run(args, *, env=None, ok=True, data=None):
    args = [str(a) for a in args]
    result = subprocess.run(args, input=data, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, env=env or ENV, timeout=40)
    if ok and result.returncode:
        raise AssertionError(f"{shlex.join(args)}\n{result.stdout}")
    if not ok and result.returncode == 0:
        raise AssertionError(f"unexpected success: {shlex.join(args)}\n{result.stdout}")
    return result.stdout.strip()


def git(repo, *args, **kwargs):
    return run(["git", "-c", "core.hooksPath=/dev/null", "-C", repo, *args], **kwargs)


def ref(repo, name="main"):
    return git(repo, "rev-parse", name)


def passed(name):
    global CHECKS
    CHECKS += 1
    print(f"PASS {name}", flush=True)


def suite(root, port, user):
    seed, source, dest, relay = [root / name for name in ("seed", "source.git", "dest.git", "relay.git")]
    key, known = root / "client_key", root / "known_hosts"
    marker = root / "EXECUTED"
    probe = root / "probe"
    probe.write_text(f"#!/bin/sh\ntouch {shlex.quote(str(marker))}\nexit 99\n")
    probe.chmod(0o700)
    included = root / "included.config"
    included.write_text(f"[core]\nsshCommand = {probe}\n")

    def url(path):
        return f"ssh://{user}@127.0.0.1:{port}{path}"

    hostile_env = ENV | {"GIT_SSH_COMMAND": str(probe), "GIT_DIR": str(root / "wrong"),
                         "GIT_CONFIG_GLOBAL": str(probe), "GIT_CONFIG_COUNT": "1",
                         "GIT_CONFIG_KEY_0": "core.sshCommand", "GIT_CONFIG_VALUE_0": str(probe),
                         "GIT_EXEC_PATH": str(root), "GIT_CONFIG_SYSTEM": str(probe),
                         "GIT_OBJECT_DIRECTORY": str(root / "wrong"), "GIT_REPLACE_REF_BASE": "refs/heads/"}
    common = [BIN, "-C", relay, "--known-hosts", known, "--identity-file", key, "--timeout", "20"]

    def safe(*args, ok=True, prefix=(), extra=()):
        return run([*prefix, *common, *extra, *args], env=hostile_env, ok=ok)

    git(root, "init", "-q", "--object-format=sha1", "-b", "main", seed)
    git(seed, "config", "user.name", "safe-git test")
    git(seed, "config", "user.email", "test@example.invalid")
    (seed / "payload").write_text("base\n")
    (seed / ".gitattributes").write_text("payload filter=probe\n")
    (seed / ".gitmodules").write_text('[submodule "evil"]\npath = evil\nurl = ext::probe\n')
    (seed / "script").write_text(probe.read_text())
    (seed / "script").chmod(0o700)
    (seed / "symlink").symlink_to("/etc/passwd")
    # Similar files force the server's normal pack generator to use deltas.
    for n in range(20):
        (seed / f"similar{n}").write_text("0123456789abcdef\n" * 1000 + str(n))
    git(seed, "add", ".")
    git(seed, "commit", "-qm", "base")
    base = ref(seed)
    (seed / "payload").write_text("forward\n")
    git(seed, "add", ".")
    git(seed, "commit", "-qm", "forward")
    forward = ref(seed)
    tree = ref(seed, "HEAD^{tree}")
    side = git(seed, "commit-tree", tree, "-p", base, data="divergent\n")
    joined = git(seed, "commit-tree", tree, "-p", side, "-p", forward, data="second parent fast forward\n")
    for name, oid in (("forward", forward), ("side", side), ("joined", joined)):
        git(seed, "update-ref", f"refs/heads/{name}", oid)
    git(seed, "update-ref", "refs/heads/main", base)
    for path in (source, dest):
        git(root, "clone", "-q", "--bare", seed, path)
    git(dest, "update-ref", "-d", "refs/heads/forward")
    safe("init", "--bare", relay)
    safe("fetch", url(source), "main")
    assert not (relay / "refs/heads/main").exists()
    safe("merge", "--ff-only", "origin/main")
    assert ref(relay) == base
    passed("init, fetch, and unborn fast-forward")

    hooks = relay / "hooks"
    hooks.mkdir()
    for name in ("pre-push", "post-merge", "reference-transaction", "post-checkout"):
        shutil.copy(probe, hooks / name)
    with (relay / "config").open("a") as f:
        f.write(f'''[core]
    hooksPath = {hooks}
    sshCommand = {probe}
    fsmonitor = {probe}
[remote "origin"]
    url = ext::{probe}
    pushurl = ext::{probe}
    mirror = true
    fetch = +refs/*:refs/*
[url "ext::{probe}"]
    insteadOf = ssh://
[credential]
    helper = !{probe}
[filter "probe"]
    process = {probe}
    smudge = {probe}
[alias]
    fetch = !{probe}
[include]
    path = {included}
''')
    (relay / "got.conf").write_text("malicious irrelevant configuration\n")
    git(source, "update-ref", "refs/heads/main", forward)
    trace = root / "exec.trace"
    tracing = ["strace", "-f", "-e", "trace=execve,execveat", "-o", trace] if shutil.which("strace") else []
    safe("fetch", url(source), "main", prefix=tracing)
    assert ref(relay) == base and ref(relay, "refs/remotes/origin/main") == forward
    if tracing:
        executions = [line for line in trace.read_text().splitlines() if "execve" in line]
        assert len(executions) == 1 and str(BIN) in executions[0], executions
    for operation in [("merge", "--ff-only", "origin/main"), ("push", url(dest))]:
        safe(*operation, prefix=tracing)
        if tracing:
            executions = [line for line in trace.read_text().splitlines() if "execve" in line]
            assert len(executions) == 1 and str(BIN) in executions[0], executions
    assert ref(relay) == ref(dest) == forward and not marker.exists()
    assert git(dest, "show-ref", "--verify", "refs/heads/forward", ok=False)
    passed("fetch/merge/push; hostile config, hooks, environment, tracked scripts inert; no child exec")

    safe("merge", "--ff-only", "origin/main")
    safe("push", url(dest))
    git(source, "update-ref", "refs/heads/main", base)
    safe("fetch", url(source), "main")
    safe("merge", "--ff-only", "origin/main")
    assert ref(relay) == forward
    passed("equal tips and upstream rewind are successful merge no-ops")
    git(source, "update-ref", "refs/heads/main", side)
    safe("fetch", url(source), "main")
    assert "Not possible to fast-forward" in safe("merge", "--ff-only", "origin/main", ok=False)
    assert ref(relay) == forward
    git(dest, "update-ref", "refs/heads/main", side)
    assert "non-fast-forward" in safe("push", url(dest), ok=False)
    assert ref(dest) == side
    passed("divergent merge and push rejected without moving branches")

    git(source, "update-ref", "refs/heads/main", joined)
    safe("fetch", url(source), "main")
    safe("merge", "--ff-only", "origin/main")
    assert ref(relay) == joined
    safe("push", url(dest), "main:other")
    assert ref(dest, "other") == joined
    passed("fast-forward via second merge parent; explicit destination branch creation")
    # CAS race: the receive-pack advertisement precedes an upload-pack read.
    # The test SSH service moves destination after capturing receive-pack's
    # advertisement, before releasing it to the client.
    git(dest, "update-ref", "refs/heads/main", forward)
    (root / "race").write_text(side)
    assert "rejected" in safe("push", url(dest), ok=False)
    assert ref(dest) == side
    passed("destination race rejected by expected-old ref update")

    git(dest, "update-ref", "refs/heads/main", forward)
    reject = dest / "hooks/pre-receive"
    reject.write_text("#!/bin/sh\nexit 1\n")
    reject.chmod(0o700)
    assert "rejected" in safe("push", url(dest), ok=False)
    assert ref(dest) == forward
    reject.unlink()
    passed("server hook rejection is reported as failure")

    # Exercise both Git pack delta encodings independently of upload-pack.
    for flag in ("--delta-base-offset", "--no-delta-base-offset"):
        packed = root / ("ofs.git" if flag == "--delta-base-offset" else "ref.git")
        git(root, "clone", "-q", "--bare", "--no-hardlinks", relay, packed)
        # Remove loose duplicates to force safe-git to decode the generated pack.
        git(packed, "pack-objects", "--all", flag, str(packed / "objects/pack/pack"))
        for directory in (packed / "objects").iterdir():
            if len(directory.name) == 2:
                shutil.rmtree(directory)
        git(packed, "pack-refs", "--all")
        # clone does not copy tracking refs; install a packed tracking reference.
        git(packed, "update-ref", "refs/remotes/origin/main", joined)
        git(packed, "pack-refs", "--all")
        run([BIN, "-C", packed, "merge", "--ff-only", "origin/main"])
        git(packed, "fsck", "--full")
    passed("existing packed refs and OFS/REF delta packs interoperate with Git")
    git(relay, "fsck", "--full")
    safe("push", url(dest))
    assert ref(dest) == joined
    passed("Git fsck accepts fetched storage and pushed objects")

    before = ref(relay, "refs/remotes/origin/main")
    for mode in ("frame", "checksum", "missing"):
        (root / "malformed").write_text(mode)
        safe("fetch", url(source), "main", ok=False)
        assert ref(relay, "refs/remotes/origin/main") == before
    (root / "malformed").unlink()
    passed("malformed framing, corrupt pack, and missing graph rejected before ref mutation")

    corrupt = relay / "objects" / joined[:2] / joined[2:]
    saved = corrupt.read_bytes()
    corrupt.write_bytes(b"not a zlib object")
    safe("merge", "--ff-only", "origin/main", ok=False)
    safe("push", url(dest), ok=False)
    assert ref(relay) == joined and ref(dest) == joined
    corrupt.write_bytes(saved)
    passed("corrupted local objects are rejected even for up-to-date operations")

    for metadata in ("shallow", "commondir", "objects/info/alternates", "info/grafts"):
        path = relay / metadata
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("untrusted\n")
        safe("merge", "--ff-only", "origin/main", ok=False)
        path.unlink()
    passed("shallow, linked-repository, alternate object store, and graft metadata rejected")

    for path in (relay / "objects", relay / "refs/remotes/origin", relay / "config"):
        backup = path.with_name(path.name + ".original")
        path.rename(backup)
        path.symlink_to(backup)
        safe("merge", "--ff-only", "origin/main", ok=False)
        path.unlink()
        backup.rename(path)
    with (relay / ".safe-git.lock").open("r+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert "locked" in safe("merge", "--ff-only", "origin/main", ok=False)
    passed("repository symlinks and concurrent safe-git lock rejected")

    good_known = known.read_text()
    known.write_text("")
    safe("fetch", url(source), "main", ok=False)
    wrong = (root / "client_key.pub").read_text().split()
    known.write_text(f"[127.0.0.1]:{port} {wrong[0]} {wrong[1]}\n")
    safe("fetch", url(source), "main", ok=False)
    known.write_text(good_known + "@revoked " + good_known)
    safe("fetch", url(source), "main", ok=False)
    known.write_text(good_known)
    passed("unknown, changed, and marker-bearing host-key files fail closed")

    run(["ssh-keygen", "-H", "-f", known])
    safe("fetch", url(source), "main")
    passed("hashed known_hosts entries accepted")
    run([BIN, "-C", relay, "--identity-file", root / "host_key", "--known-hosts", known,
         "fetch", url(source), "main"], ok=False)

    home = root / "home"
    ssh = home / ".ssh"
    ssh.mkdir(parents=True)
    shutil.copy(known, ssh / "known_hosts")
    auto_env = {k: v for k, v in hostile_env.items()
                if not k.startswith("SAFE_GIT_") and k != "SSH_AUTH_SOCK"}
    auto_env["HOME"] = str(home)
    automatic = [BIN, "-C", relay, "--timeout", "20"]

    def auto_fetch(*options, env=None, ok=True):
        return run([*automatic, *options, "fetch", url(source), "main"],
                   env=env or auto_env, ok=ok)

    assert "SSH authentication failed" in auto_fetch(ok=False)
    authorized = root / "authorized_keys"
    for algorithm in ("ed25519", "ecdsa", "rsa"):
        default = ssh / f"id_{algorithm}"
        run(["ssh-keygen", "-q", "-t", algorithm, "-N", "", "-f", default])
        with authorized.open("a") as f:
            f.write("restrict " + default.with_suffix(".pub").read_text())
        auto_fetch()
        run([*automatic, "push", url(dest)], env=auto_env)
        default.unlink()
    passed("default Ed25519, ECDSA, RSA keys and known_hosts authenticate fetch and push")

    default = ssh / "id_ed25519"
    shutil.copy(key, default)
    stale_env = auto_env | {"SSH_AUTH_SOCK": str(root / "missing-agent.sock")}
    auto_fetch(env=stale_env)
    assert "connect SSH_AUTH_SOCK" in auto_fetch("--ssh-agent", env=stale_env, ok=False)
    auto_fetch("--identity-file", root / "host_key", ok=False)
    auto_fetch(env=auto_env | {"SAFE_GIT_IDENTITY_FILE": str(root / "host_key")}, ok=False)
    passed("unavailable automatic agent falls back; explicit key or agent never falls back")

    fallback = ssh / "id_rsa"
    shutil.copy(key, fallback)
    run(["ssh-keygen", "-p", "-P", "", "-N", "test-passphrase", "-f", default])
    auto_fetch()
    default.write_text("not a key\n")
    auto_fetch()
    fallback.unlink()
    assert "SSH authentication failed" in auto_fetch(ok=False)
    shutil.copy(key, default)
    passed("unusable default keys are skipped and absent usable credentials fail")

    (ssh / "known_hosts").write_text("")
    auto_fetch(ok=False)
    shutil.copy(known, ssh / "known_hosts")
    run([*tracing, *automatic, "fetch", url(source), "main"], env=auto_env)
    if tracing:
        executions = [line for line in trace.read_text().splitlines() if "execve" in line]
        assert len(executions) == 1 and str(BIN) in executions[0], executions
    passed("default host-key verification remains required; automatic auth runs no children")

    agent_socket = root / "agent.sock"
    agent = subprocess.Popen(["ssh-agent", "-D", "-a", str(agent_socket)],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            if agent_socket.exists():
                break
            time.sleep(0.01)
        agent_env = auto_env | {"SSH_AUTH_SOCK": str(agent_socket)}
        auto_fetch(env=agent_env)
        auto_fetch("--ssh-agent", env=agent_env, ok=False)
        run(["ssh-add", root / "host_key"], env=agent_env)
        auto_fetch(env=agent_env)
        auto_fetch("--ssh-agent", env=agent_env, ok=False)
        passed("empty or unauthorized automatic agent falls back to default keys")
        run(["ssh-add", "-D"], env=agent_env)
        run(["ssh-add", key], env=agent_env)
        auto_fetch("--identity-file", root / "host_key", env=agent_env, ok=False)
        default.unlink()
        auto_fetch(env=agent_env)
        run([*automatic, "push", url(dest)], env=agent_env)
        passed("agent is discovered without authentication flags; explicit identity overrides it")
        run([BIN, "-C", relay, "--ssh-agent", "--known-hosts", known,
             "fetch", url(source), "main"], env=agent_env)
    finally:
        agent.terminate()
        agent.wait(timeout=10)
    passed("explicit agent authentication works; unauthorized private key rejected")

    for args in [("merge", "origin/main"), ("push", url(dest), "+main"),
                 ("push", url(dest), ":main"), ("fetch", "origin", "main"),
                 ("fetch", url(source), "../main"), ("fetch", "ext::probe", "main")]:
        safe(*args, ok=False)
    assert not marker.exists()
    passed("missing ff-only, force/deletion refspecs, remote aliases, traversal, helpers rejected")


def main():
    assert BIN.is_file(), f"build the binary first: {BIN}"
    with tempfile.TemporaryDirectory(prefix="safe-git-test-") as directory:
        root = Path(directory)
        user = pwd.getpwuid(os.getuid()).pw_name
        for key in ("client_key", "host_key"):
            run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", root / key])
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        host = (root / "host_key.pub").read_text().split()
        (root / "known_hosts").write_text(f"[127.0.0.1]:{port} {host[0]} {host[1]}\n")
        (root / "authorized_keys").write_text("restrict " + (root / "client_key.pub").read_text())
        service = root / "service.py"
        service.write_text('''import hashlib, os, shlex, subprocess, sys
from pathlib import Path
root = Path(__file__).parent
args = shlex.split(os.environ.get("SSH_ORIGINAL_COMMAND", ""))
if len(args) != 2 or args[0] not in ("git-upload-pack", "git-receive-pack") or args[1] not in [str(root / n) for n in ("source.git", "dest.git")]:
    sys.exit(1)
if (root / "malformed").exists():
    mode = (root / "malformed").read_text()
    if mode == "frame":
        sys.stdout.buffer.write(b"ffff")
        sys.exit(0)
    def pkt(data):
        return f"{len(data)+4:04x}".encode() + data
    sys.stdout.buffer.write(pkt(b"ffffffffffffffffffffffffffffffffffffffff refs/heads/main\\0ofs-delta\\n") + b"0000")
    sys.stdout.buffer.flush()
    while True:
        size = int(sys.stdin.buffer.read(4), 16)
        if size and sys.stdin.buffer.read(size - 4) == b"done\\n":
            break
    pack = b"PACK\\x00\\x00\\x00\\x02\\x00\\x00\\x00\\x00"
    pack += hashlib.sha1(pack).digest() if mode == "missing" else bytes(20)
    sys.stdout.buffer.write(pkt(b"NAK\\n") + pack)
    sys.exit(0)
command = ["/usr/bin/git", args[0][4:], args[1]]
if args[0] == "git-receive-pack" and args[1] == str(root / "dest.git") and (root / "race").exists():
    counter = root / "race-count"
    seen = int(counter.read_text()) if counter.exists() else 0
    counter.write_text(str(seen + 1))
    if seen == 0:
        os.execv(command[0], command)
    child = subprocess.Popen(command, stdin=sys.stdin.buffer, stdout=subprocess.PIPE)
    while True:
        header = child.stdout.read(4)
        size = int(header, 16)
        sys.stdout.buffer.write(header)
        if not size:
            break
        sys.stdout.buffer.write(child.stdout.read(size - 4))
    race = (root / "race").read_text()
    (root / "race").unlink()
    subprocess.run(["/usr/bin/git", "-C", args[1], "update-ref", "refs/heads/main", race], check=True)
    sys.stdout.buffer.flush()
    while chunk := child.stdout.read(65536):
        sys.stdout.buffer.write(chunk)
    sys.stdout.buffer.flush()
    sys.exit(child.wait())
os.execv(command[0], command)
''')
        config = root / "sshd_config"
        config.write_text(f'''Port {port}
ListenAddress 127.0.0.1
HostKey {root}/host_key
PidFile {root}/sshd.pid
AuthorizedKeysFile {root}/authorized_keys
StrictModes no
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
PermitRootLogin yes
AllowUsers {user}
DisableForwarding yes
PermitTTY no
PermitUserRC no
ForceCommand /usr/bin/python3 {service}
LogLevel ERROR
''')
        # Authentication refusal tests must not rate-limit later connections.
        # Older OpenSSH releases predate this option and do not need it.
        if subprocess.run(["/usr/bin/sshd", "-T", "-f", str(config), "-o", "PerSourcePenalties=no"],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0:
            with config.open("a") as f:
                f.write("PerSourcePenalties no\n")
        sudo = [] if os.getuid() == 0 else ["sudo", "-n"]
        with (root / "sshd.log").open("w+") as log:
            server = subprocess.Popen([*sudo, "/usr/bin/sshd", "-D", "-e", "-f", str(config)],
                                      stdout=log, stderr=log, start_new_session=True)
            try:
                for _ in range(100):
                    if server.poll() is not None:
                        log.seek(0)
                        raise RuntimeError(log.read())
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        time.sleep(0.05)
                else:
                    raise RuntimeError("test sshd did not start")
                suite(root, port, user)
            except Exception:
                log.seek(0)
                print(log.read(), file=sys.stderr)
                raise
            finally:
                subprocess.run([*sudo, "kill", "-TERM", "--", f"-{server.pid}"], check=False,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                server.wait(timeout=10)
    print(f"{CHECKS} integration checks passed")


if __name__ == "__main__":
    main()
