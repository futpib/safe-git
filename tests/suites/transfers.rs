//! Transfer, protocol, metadata and authentication regressions.
use crate::support::*;
use std::{
    fs,
    os::unix::fs::symlink,
    thread,
    time::{Duration, Instant},
};

pub fn suite() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let seed = root.join("seed");
    let source = root.join("source.git");
    let dest = root.join("dest.git");
    let relay = root.join("relay.git");
    let key = root.join("client_key");
    let known = root.join("known_hosts");
    let marker = root.join("EXECUTED");
    let probe = root.join("probe");
    write(
        &probe,
        format!("#!/bin/sh\ntouch {}\nexit 99\n", quote(&marker)),
    );
    chmod(&probe, 0o700);
    let included = root.join("included.config");
    write(
        &included,
        format!("[core]\nsshCommand = {}\n", probe.display()),
    );
    let hostile = |cmd: TestCommand| {
        cmd.env("GIT_SSH_COMMAND", &probe)
            .env("GIT_DIR", root.join("wrong"))
            .env("GIT_CONFIG_GLOBAL", &probe)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.sshCommand")
            .env("GIT_CONFIG_VALUE_0", &probe)
            .env("GIT_EXEC_PATH", root)
            .env("GIT_CONFIG_SYSTEM", &probe)
            .env("GIT_OBJECT_DIRECTORY", root.join("wrong"))
            .env("GIT_REPLACE_REF_BASE", "refs/heads/")
    };
    let safe = |args: &[&str]| hostile(fixture.safe(&relay).args(args));
    let source_url = fixture.url(&source);
    let dest_url = fixture.url(&dest);
    let fetch = ["fetch", &source_url, "main"];
    let merge = ["merge", "--ff-only", "origin/main"];
    let push = ["push", &dest_url];

    git(root)
        .args(["init", "-q", "--object-format=sha1", "-b", "main"])
        .arg(&seed)
        .run();
    git(&seed)
        .args(["config", "user.name", "safe-git test"])
        .run();
    git(&seed)
        .args(["config", "user.email", "test@example.invalid"])
        .run();
    write(seed.join("payload"), "base\n");
    write(seed.join(".gitattributes"), "payload filter=probe\n");
    write(
        seed.join(".gitmodules"),
        "[submodule \"evil\"]\npath = evil\nurl = ext::probe\n",
    );
    fs::copy(&probe, seed.join("script")).unwrap();
    symlink("/etc/passwd", seed.join("symlink")).unwrap();
    // Similar blobs force the normal Git pack generator to use deltas.
    for n in 0..20 {
        write(
            seed.join(format!("similar{n}")),
            format!("{}{n}", "0123456789abcdef\n".repeat(1000)),
        );
    }
    git(&seed).args(["add", "."]).run();
    git(&seed).args(["commit", "-qm", "base"]).run();
    let base = head(&seed);
    write(seed.join("payload"), "forward\n");
    git(&seed).args(["add", "."]).run();
    git(&seed).args(["commit", "-qm", "forward"]).run();
    let forward = head(&seed);
    let tree = revision(&seed, "HEAD^{tree}");
    let side = git(&seed)
        .args(["commit-tree", &tree, "-p", &base])
        .input("divergent\n")
        .run();
    let joined = git(&seed)
        .args(["commit-tree", &tree, "-p", &side, "-p", &forward])
        .input("second parent fast forward\n")
        .run();
    for (name, oid) in [("forward", &forward), ("side", &side), ("joined", &joined)] {
        git(&seed)
            .arg("update-ref")
            .arg(format!("refs/heads/{name}"))
            .arg(oid)
            .run();
    }
    git(&seed)
        .args(["update-ref", "refs/heads/main", &base])
        .run();
    for path in [&source, &dest] {
        git(root)
            .args(["clone", "-q", "--bare"])
            .arg(&seed)
            .arg(path)
            .run();
    }
    git(&dest)
        .args(["update-ref", "-d", "refs/heads/forward"])
        .run();
    git(&dest)
        .args(["update-ref", "-d", "refs/heads/side"])
        .run();
    // Fork hosts advertise shared storage as repeatable non-ref .have hints.
    write(
        dest.join("objects/info/alternates"),
        format!("{}\n", seed.join(".git/objects").display()),
    );
    let advertisement = git(root)
        .args(["receive-pack", "--advertise-refs"])
        .arg(&dest)
        .run();
    assert!(
        advertisement.matches(" .have").count() >= 2,
        "{advertisement}"
    );
    safe(&["init", "--bare"]).arg(&relay).run();
    safe(&fetch).run();
    assert!(!relay.join("refs/heads/main").exists());
    safe(&merge).run();
    assert_eq!(head(&relay), base);
    passed("init, fetch, and unborn fast-forward");

    let hooks = relay.join("hooks");
    fs::create_dir(&hooks).unwrap();
    for name in [
        "pre-push",
        "post-merge",
        "reference-transaction",
        "post-checkout",
    ] {
        fs::copy(&probe, hooks.join(name)).unwrap();
    }
    append(
        relay.join("config"),
        &format!(
            r#"[core]
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
"#,
            hooks = hooks.display(),
            probe = probe.display(),
            included = included.display()
        ),
    );
    write(
        relay.join("got.conf"),
        "malicious irrelevant configuration\n",
    );
    git(&source)
        .args(["update-ref", "refs/heads/main", &forward])
        .run();
    let trace = root.join("exec.trace");
    safe(&fetch).traced(&trace).run();
    assert_eq!(head(&relay), base);
    assert_eq!(revision(&relay, "refs/remotes/origin/main"), forward);
    check_trace(&trace);
    for args in [&merge[..], &push[..]] {
        safe(args).traced(&trace).run();
        check_trace(&trace);
    }
    assert_eq!(head(&relay), forward);
    assert_eq!(head(&dest), forward);
    assert!(!marker.exists());
    assert!(
        !git(&dest)
            .args(["show-ref", "--verify", "refs/heads/forward"])
            .fails()
            .is_empty()
    );
    passed(
        "fetch/merge/push with shared-object hints; hostile config and hooks inert; no child exec",
    );

    safe(&merge).run();
    safe(&push).run();
    git(&source)
        .args(["update-ref", "refs/heads/main", &base])
        .run();
    safe(&fetch).run();
    safe(&merge).run();
    assert_eq!(head(&relay), forward);
    passed("equal tips and upstream rewind are successful merge no-ops");
    git(&source)
        .args(["update-ref", "refs/heads/main", &side])
        .run();
    safe(&fetch).run();
    assert!(
        safe(&merge)
            .fails()
            .contains("Not possible to fast-forward")
    );
    assert_eq!(head(&relay), forward);
    git(&dest)
        .args(["update-ref", "refs/heads/main", &side])
        .run();
    assert!(safe(&push).fails().contains("non-fast-forward"));
    assert_eq!(head(&dest), side);
    passed("divergent merge and push rejected without moving branches");

    git(&source)
        .args(["update-ref", "refs/heads/main", &joined])
        .run();
    safe(&fetch).run();
    safe(&merge).run();
    assert_eq!(head(&relay), joined);
    safe(&["push", &dest_url, "main:other"]).run();
    assert_eq!(revision(&dest, "other"), joined);
    passed("fast-forward via second merge parent; explicit destination branch creation");
    // The service moves the destination after capturing receive-pack's
    // advertisement, before releasing it to the client.
    git(&dest)
        .args(["update-ref", "refs/heads/main", &forward])
        .run();
    write(root.join("race"), &side);
    assert!(safe(&push).fails().contains("rejected"));
    assert_eq!(head(&dest), side);
    passed("destination race rejected by expected-old ref update");

    git(&dest)
        .args(["update-ref", "refs/heads/main", &forward])
        .run();
    let reject = dest.join("hooks/pre-receive");
    write(&reject, "#!/bin/sh\nexit 1\n");
    chmod(&reject, 0o700);
    assert!(safe(&push).fails().contains("rejected"));
    assert_eq!(head(&dest), forward);
    fs::remove_file(&reject).unwrap();
    passed("server hook rejection is reported as failure");

    let trailer = root.join("push-trailer");
    // Keep Git's oracle separate from the deliberately hostile relay config,
    // which ordinary Git would execute (including URL rewriting).
    let oracle = root.join("push-oracle.git");
    git(root)
        .args(["clone", "-q", "--bare"])
        .arg(&seed)
        .arg(&oracle)
        .run();
    git(&oracle)
        .args(["update-ref", "refs/heads/main", &joined])
        .run();
    let ssh_command = format!(
        "ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o {} -o StrictHostKeyChecking=yes",
        quote(&key),
        quote(format!("UserKnownHostsFile={}", known.display()))
    );
    for tail in [&b"0000"[..], &b"trailing server diagnostics\n"[..]] {
        write(&trailer, tail);
        for real_git in [false, true] {
            git(&dest)
                .args(["update-ref", "refs/heads/main", &forward])
                .run();
            if real_git {
                git(&oracle)
                    .args(["push", &dest_url, "main:main"])
                    .env("GIT_SSH_COMMAND", &ssh_command)
                    .run();
            } else {
                safe(&push).run();
            }
            assert_eq!(head(&dest), joined);
        }
    }
    write(&trailer, b"0017ok refs/heads/main\n0000");
    git(&dest)
        .args(["update-ref", "refs/heads/main", &forward])
        .run();
    write(&reject, "#!/bin/sh\nexit 1\n");
    chmod(&reject, 0o700);
    assert!(safe(&push).fails().contains("rejected"));
    assert_eq!(head(&dest), forward);
    fs::remove_file(reject).unwrap();
    fs::remove_file(trailer).unwrap();
    passed("completed push report ignores trailers like Git; trailers cannot override rejection");

    for (flag, name) in [
        ("--delta-base-offset", "ofs.git"),
        ("--no-delta-base-offset", "ref.git"),
    ] {
        let packed = root.join(name);
        git(root)
            .args(["clone", "-q", "--bare", "--no-hardlinks"])
            .arg(&relay)
            .arg(&packed)
            .run();
        git(&packed)
            .args(["pack-objects", "--all", flag])
            .arg(packed.join("objects/pack/pack"))
            .run();
        // Remove loose duplicates so safe-git must decode the generated pack.
        for entry in fs::read_dir(packed.join("objects")).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().len() == 2 {
                fs::remove_dir_all(entry.path()).unwrap();
            }
        }
        git(&packed).args(["pack-refs", "--all"]).run();
        git(&packed)
            .args(["update-ref", "refs/remotes/origin/main", &joined])
            .run();
        git(&packed).args(["pack-refs", "--all"]).run();
        fixture.safe(&packed).args(merge).run();
        git(&packed).args(["fsck", "--full"]).run();
    }
    passed("existing packed refs and OFS/REF delta packs interoperate with Git");
    git(&relay).args(["fsck", "--full"]).run();
    safe(&push).run();
    assert_eq!(head(&dest), joined);
    passed("Git fsck accepts fetched storage and pushed objects");

    let before = revision(&relay, "refs/remotes/origin/main");
    for mode in ["frame", "checksum", "missing"] {
        write(root.join("malformed"), mode);
        safe(&fetch).fails();
        assert_eq!(revision(&relay, "refs/remotes/origin/main"), before);
    }
    fs::remove_file(root.join("malformed")).unwrap();
    passed("malformed framing, corrupt pack, and missing graph rejected before ref mutation");

    let corrupt = relay.join("objects").join(&joined[..2]).join(&joined[2..]);
    let saved = fs::read(&corrupt).unwrap();
    write(&corrupt, b"not a zlib object");
    safe(&merge).fails();
    safe(&push).fails();
    assert_eq!(head(&relay), joined);
    assert_eq!(head(&dest), joined);
    write(&corrupt, saved);
    passed("corrupted local objects are rejected even for up-to-date operations");

    for metadata in [
        "shallow",
        "commondir",
        "objects/info/alternates",
        "info/grafts",
    ] {
        let path = relay.join(metadata);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write(&path, "untrusted\n");
        safe(&merge).fails();
        fs::remove_file(path).unwrap();
    }
    passed("shallow, linked-repository, alternate object store, and graft metadata rejected");
    for name in ["objects", "refs/remotes/origin", "config"] {
        let path = relay.join(name);
        let backup = path.with_file_name(format!(
            "{}.original",
            path.file_name().unwrap().to_str().unwrap()
        ));
        fs::rename(&path, &backup).unwrap();
        symlink(&backup, &path).unwrap();
        safe(&merge).fails();
        fs::remove_file(&path).unwrap();
        fs::rename(backup, path).unwrap();
    }
    {
        let lock = fs::File::open(relay.join(".safe-git.lock")).unwrap();
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        assert!(safe(&merge).fails().contains("locked"));
    }
    passed("repository symlinks and concurrent safe-git lock rejected");

    let good_known = read(&known);
    write(&known, "");
    safe(&fetch).fails();
    let wrong = read(root.join("client_key.pub"));
    let wrong: Vec<_> = wrong.split_whitespace().collect();
    write(
        &known,
        format!("[127.0.0.1]:{} {} {}\n", fixture.port, wrong[0], wrong[1]),
    );
    safe(&fetch).fails();
    write(&known, format!("{good_known}@revoked {good_known}"));
    safe(&fetch).fails();
    write(&known, &good_known);
    passed("unknown, changed, and marker-bearing host-key files fail closed");
    TestCommand::new("ssh-keygen")
        .args(["-H", "-f"])
        .arg(&known)
        .run();
    safe(&fetch).run();
    passed("hashed known_hosts entries accepted");
    TestCommand::new(BIN)
        .arg("-C")
        .arg(&relay)
        .arg("--identity-file")
        .arg(root.join("host_key"))
        .arg("--known-hosts")
        .arg(&known)
        .args(fetch)
        .fails();

    let home = root.join("home");
    let ssh = home.join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::copy(&known, ssh.join("known_hosts")).unwrap();
    let automatic = || {
        hostile(
            TestCommand::new(BIN)
                .arg("-C")
                .arg(&relay)
                .args(["--timeout", "20"]),
        )
        .env("HOME", &home)
    };
    let auto_fetch = |options: &[&str]| automatic().args(options).args(fetch);
    assert!(
        auto_fetch(&[])
            .fails()
            .contains("SSH authentication failed")
    );
    for algorithm in ["ed25519", "ecdsa", "rsa"] {
        let default = ssh.join(format!("id_{algorithm}"));
        TestCommand::new("ssh-keygen")
            .args(["-q", "-t", algorithm, "-N", "", "-f"])
            .arg(&default)
            .run();
        append(
            root.join("authorized_keys"),
            &format!("restrict {}", read(default.with_extension("pub"))),
        );
        auto_fetch(&[]).run();
        automatic().args(push).run();
        fs::remove_file(default).unwrap();
    }
    passed("default Ed25519, ECDSA, RSA keys and known_hosts authenticate fetch and push");
    let default = ssh.join("id_ed25519");
    fs::copy(&key, &default).unwrap();
    let stale = root.join("missing-agent.sock");
    auto_fetch(&[]).env("SSH_AUTH_SOCK", &stale).run();
    assert!(
        auto_fetch(&["--ssh-agent"])
            .env("SSH_AUTH_SOCK", &stale)
            .fails()
            .contains("connect SSH_AUTH_SOCK")
    );
    automatic()
        .arg("--identity-file")
        .arg(root.join("host_key"))
        .args(fetch)
        .fails();
    auto_fetch(&[])
        .env("SAFE_GIT_IDENTITY_FILE", root.join("host_key"))
        .fails();
    passed("unavailable automatic agent falls back; explicit key or agent never falls back");

    let fallback = ssh.join("id_rsa");
    fs::copy(&key, &fallback).unwrap();
    TestCommand::new("ssh-keygen")
        .args(["-p", "-P", "", "-N", "test-passphrase", "-f"])
        .arg(&default)
        .run();
    auto_fetch(&[]).run();
    write(&default, "not a key\n");
    auto_fetch(&[]).run();
    fs::remove_file(fallback).unwrap();
    assert!(
        auto_fetch(&[])
            .fails()
            .contains("SSH authentication failed")
    );
    fs::copy(&key, &default).unwrap();
    passed("unusable default keys are skipped and absent usable credentials fail");
    write(ssh.join("known_hosts"), "");
    auto_fetch(&[]).fails();
    fs::copy(&known, ssh.join("known_hosts")).unwrap();
    auto_fetch(&[]).traced(&trace).run();
    check_trace(&trace);
    passed("default host-key verification remains required; automatic auth runs no children");

    {
        let socket = root.join("agent.sock");
        let mut agent = TestCommand::new("ssh-agent")
            .args(["-D", "-a"])
            .arg(&socket)
            .spawn();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(
                agent.child.try_wait().unwrap().is_none(),
                "ssh-agent exited before creating its socket"
            );
            assert!(Instant::now() < deadline, "ssh-agent did not start");
            thread::sleep(Duration::from_millis(10));
        }
        let agent_fetch = |options: &[&str]| auto_fetch(options).env("SSH_AUTH_SOCK", &socket);
        agent_fetch(&[]).run();
        agent_fetch(&["--ssh-agent"]).fails();
        TestCommand::new("ssh-add")
            .arg(root.join("host_key"))
            .env("SSH_AUTH_SOCK", &socket)
            .run();
        agent_fetch(&[]).run();
        agent_fetch(&["--ssh-agent"]).fails();
        passed("empty or unauthorized automatic agent falls back to default keys");
        TestCommand::new("ssh-add")
            .arg("-D")
            .env("SSH_AUTH_SOCK", &socket)
            .run();
        TestCommand::new("ssh-add")
            .arg(&key)
            .env("SSH_AUTH_SOCK", &socket)
            .run();
        automatic()
            .arg("--identity-file")
            .arg(root.join("host_key"))
            .args(fetch)
            .env("SSH_AUTH_SOCK", &socket)
            .fails();
        fs::remove_file(&default).unwrap();
        agent_fetch(&[]).run();
        automatic().args(push).env("SSH_AUTH_SOCK", &socket).run();
        passed("agent is discovered without authentication flags; explicit identity overrides it");
        TestCommand::new(BIN)
            .arg("-C")
            .arg(&relay)
            .arg("--ssh-agent")
            .arg("--known-hosts")
            .arg(&known)
            .args(fetch)
            .env("SSH_AUTH_SOCK", &socket)
            .run();
    }
    passed("explicit agent authentication works; unauthorized private key rejected");

    for args in [
        vec!["merge", "--unknown-option", "origin/main"],
        vec!["push", "--unknown-option", &dest_url, "+main"],
        vec!["push", &dest_url, ":../main"],
        vec!["fetch", "origin", "main"],
        vec!["fetch", &source_url, "../main"],
        vec!["fetch", "ext::probe", "main"],
    ] {
        safe(&args).fails();
    }
    assert!(!marker.exists());
    passed(
        "unknown options, unsafe refspecs, unregistered remote aliases, traversal and helpers rejected",
    );
}
