use crate::support::*;
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};
fn safe(repo: &Path, state: &Path) -> TestCommand {
    TestCommand::new(BIN)
        .arg("-C")
        .arg(repo)
        .env("SAFE_GIT_STATE_DIR", state)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
}
fn real(repo: &Path) -> TestCommand {
    git(repo).args([
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
    ])
}
fn seed(root: &Path, state: &Path, name: &str, format: &str) -> PathBuf {
    let p = root.join(name);
    safe(root, state)
        .args(["init", "--object-format", format])
        .arg(&p)
        .run();
    write(p.join("file"), "base\n");
    safe(&p, state).args(["add", "."]).run();
    safe(&p, state).args(["commit", "-m", "base"]).run();
    p
}
pub fn topology() {
    for format in ["sha1", "sha256"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let state = root.join("state");
        let p = seed(root, &state, "repo", format);
        safe(&p, &state).args(["switch", "-c", "directory"]).run();
        fs::remove_file(p.join("file")).unwrap();
        fs::create_dir(p.join("file")).unwrap();
        write(p.join("file/child"), "child\n");
        safe(&p, &state).args(["add", "-A"]).run();
        safe(&p, &state).args(["commit", "-m", "directory"]).run();
        safe(&p, &state).args(["switch", "main"]).run();
        assert_eq!(read(p.join("file")), "base\n");
        safe(&p, &state).args(["switch", "directory"]).run();
        assert_eq!(read(p.join("file/child")), "child\n");
        write(p.join("file/untracked"), "keep\n");
        let head = revision(&p, "HEAD");
        safe(&p, &state).args(["switch", "main"]).fails();
        assert_eq!(revision(&p, "HEAD"), head);
        assert_eq!(read(p.join("file/untracked")), "keep\n");
        fs::remove_file(p.join("file/untracked")).unwrap();
        let outside = root.join("outside");
        write(&outside, "outside\n");
        symlink(&outside, p.join("file/untracked")).unwrap();
        safe(&p, &state).args(["switch", "main"]).fails();
        assert_eq!(read(&outside), "outside\n");
        fs::remove_file(p.join("file/untracked")).unwrap();
        safe(&p, &state).args(["switch", "main"]).run();
        real(&p).args(["fsck", "--full"]).run();
    }
    passed(
        "file/directory switches preserve tracked data and reject untracked and symlink obstructions",
    );
}
pub fn data_commands() {
    for format in ["sha1", "sha256"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let state = root.join("state");
        let p = seed(root, &state, "repo", format);
        let base = revision(&p, "HEAD");
        safe(&p, &state).args(["notes", "add", "-m", "note"]).run();
        assert_eq!(real(&p).args(["notes", "show"]).run(), "note");
        real(&p).args(["notes", "append", "-m", "second"]).run();
        assert_eq!(
            safe(&p, &state).args(["notes", "show"]).run(),
            "note\n\nsecond"
        );
        write(p.join("file"), "change\n");
        safe(&p, &state).args(["commit", "-am", "change"]).run();
        safe(&p, &state)
            .args(["notes", "copy", &base, "HEAD"])
            .run();
        assert_eq!(real(&p).args(["notes", "show"]).run(), "note\n\nsecond");
        let range = format!("{base}..HEAD");
        assert!(
            safe(&p, &state)
                .args(["range-diff", &range, &range])
                .run()
                .contains(" = ")
        );
        let bundle = root.join("safe.bundle");
        safe(&p, &state)
            .args(["bundle", "create"])
            .arg(&bundle)
            .arg("--all")
            .run();
        real(&p).args(["bundle", "verify"]).arg(&bundle).run();
        let target = root.join("target");
        safe(root, &state)
            .args(["init", "--bare", "--object-format", format])
            .arg(&target)
            .run();
        safe(&target, &state)
            .args(["bundle", "unbundle"])
            .arg(&bundle)
            .run();
        real(&target)
            .args(["cat-file", "-e", &revision(&p, "HEAD")])
            .run();
        let prior = revision(&p, "HEAD");
        write(p.join("file"), "later change\n");
        safe(&p, &state).args(["commit", "-am", "later"]).run();
        let later = revision(&p, "HEAD");
        let range = format!("{prior}..HEAD");
        let git_bundle = root.join("git.bundle");
        real(&p)
            .args(["bundle", "create"])
            .arg(&git_bundle)
            .arg(&range)
            .run();
        safe(&target, &state)
            .args(["bundle", "verify"])
            .arg(&git_bundle)
            .run();
        real(&target).args(["cat-file", "-e", &later]).fails();
        safe(&target, &state)
            .args(["bundle", "unbundle"])
            .arg(&git_bundle)
            .run();
        real(&target).args(["cat-file", "-e", &later]).run();
        // Verify does not publish refs or execute a repository-selected program.
        let marker = root.join("executed");
        let hook = p.join(".git/hooks/pre-commit");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display()));
        chmod(&hook, 0o755);
        safe(&p, &state)
            .args(["notes", "add", "-f", "-m", "safe"])
            .run();
        assert!(!marker.exists());
        let bundle_path = bundle.to_str().unwrap();
        for (i, args) in [
            vec!["notes", "list"],
            vec!["range-diff", &range, &range],
            vec!["bundle", "verify", bundle_path],
            vec!["rerere", "status"],
            vec!["gc"],
        ]
        .into_iter()
        .enumerate()
        {
            let trace = root.join(format!("extras-{i}.trace"));
            safe(&p, &state).args(args).traced(&trace).run();
            check_trace(&trace);
        }
        let bad = root.join("bad.bundle");
        let mut bytes = fs::read(&bundle).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&bad, bytes).unwrap();
        safe(&target, &state)
            .args(["bundle", "unbundle"])
            .arg(&bad)
            .fails();
    }
    passed(
        "Git interoperates with notes and SHA-1/SHA-256 full and incremental bundles; corrupt packs fail closed",
    );
}
pub fn signatures() {
    for format in ["sha1", "sha256"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let state = root.join("state");
        let p = seed(root, &state, "repo", format);
        let key = root.join("key");
        TestCommand::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .run();
        let allowed = root.join("allowed");
        write(
            &allowed,
            format!("test@example.invalid {}", read(key.with_extension("pub"))),
        );
        safe(&p, &state)
            .arg("--signing-key")
            .arg(&key)
            .args(["tag", "-s", "signed", "-m", "signed tag"])
            .run();
        real(&p)
            .args(["-c", "gpg.format=ssh", "-c"])
            .arg(format!("gpg.ssh.allowedSignersFile={}", allowed.display()))
            .args(["verify-tag", "signed"])
            .run();
        safe(&p, &state)
            .arg("--verification-key")
            .arg(key.with_extension("pub"))
            .args(["verify-tag", "signed"])
            .run();
        safe(&p, &state)
            .arg("--signing-key")
            .arg(&key)
            .args(["commit", "-S", "--allow-empty", "-m", "signed"])
            .run();
        safe(&p, &state)
            .arg("--verification-key")
            .arg(key.with_extension("pub"))
            .args(["verify-commit", "HEAD"])
            .run();
        real(&p)
            .args(["config", "gpg.ssh.allowedSignersFile"])
            .arg(&allowed)
            .run();
        safe(&p, &state).args(["verify-commit", "HEAD"]).fails();
        let other = root.join("other");
        TestCommand::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&other)
            .run();
        safe(&p, &state)
            .arg("--verification-key")
            .arg(other.with_extension("pub"))
            .args(["verify-commit", "HEAD"])
            .fails();
    }
    passed(
        "SSH signed tags and commits verify with explicit keys; repository-selected and wrong keys are rejected",
    );
}
pub fn worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", "sha1");
    let work = root.join("work");
    let moved = root.join("moved");
    safe(&p, &state)
        .args(["worktree", "add", "-b", "topic"])
        .arg(&work)
        .run();
    write(work.join("file"), "unstaged\n");
    safe(&p, &state)
        .args(["worktree", "lock", "--reason", "keep"])
        .arg(&work)
        .run();
    safe(&p, &state)
        .args(["worktree", "move"])
        .arg(&work)
        .arg(&moved)
        .fails();
    safe(&p, &state)
        .args(["worktree", "remove", "-f"])
        .arg(&work)
        .fails();
    safe(&p, &state)
        .args(["worktree", "unlock"])
        .arg(&work)
        .run();
    safe(&p, &state)
        .args(["worktree", "move"])
        .arg(&work)
        .arg(&moved)
        .run();
    assert_eq!(read(moved.join("file")), "unstaged\n");
    safe(&moved, &state).arg("status").run();
    let repaired = root.join("repaired");
    fs::rename(&moved, &repaired).unwrap();
    safe(&repaired, &state).arg("status").fails();
    write(repaired.join(".git"), "gitdir: /untrusted/location\n");
    safe(&p, &state)
        .args(["worktree", "repair"])
        .arg(&repaired)
        .run();
    safe(&repaired, &state).arg("status").run();
    let impostor = root.join("impostor");
    fs::create_dir(&impostor).unwrap();
    fs::copy(repaired.join(".git"), impostor.join(".git")).unwrap();
    safe(&p, &state)
        .args(["worktree", "repair"])
        .arg(&impostor)
        .fails();
    passed(
        "worktree locks and moves preserve edits; repair requires host-recorded filesystem identity",
    );
}
pub fn rerere_gc() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", "sha1");
    safe(&p, &state).args(["switch", "-c", "side"]).run();
    write(p.join("file"), "side\n");
    safe(&p, &state).args(["commit", "-am", "side"]).run();
    safe(&p, &state).args(["switch", "main"]).run();
    write(p.join("file"), "main\n");
    safe(&p, &state).args(["commit", "-am", "main"]).run();
    safe(&p, &state).args(["merge", "side"]).fails();
    safe(&p, &state).arg("rerere").run();
    assert!(
        safe(&p, &state)
            .args(["rerere", "status"])
            .run()
            .contains("file")
    );
    write(p.join("file"), "resolved\n");
    safe(&p, &state).arg("rerere").run();
    safe(&p, &state).args(["merge", "--abort"]).run();
    safe(&p, &state).args(["merge", "side"]).fails();
    safe(&p, &state).arg("rerere").run();
    assert_eq!(read(p.join("file")), "resolved\n");
    assert!(!real(&p).args(["ls-files", "-u"]).run().is_empty());
    safe(&p, &state).args(["add", "file"]).run();
    safe(&p, &state).args(["merge", "--continue"]).run();
    // Reflogs and the index are roots; only unreachable data is pruned.
    safe(&p, &state)
        .args(["commit", "--allow-empty", "-m", "orphan"])
        .run();
    let orphan = revision(&p, "HEAD");
    safe(&p, &state).args(["reset", "--hard", "HEAD~"]).run();
    safe(&p, &state).args(["gc", "--prune=now"]).run();
    real(&p).args(["cat-file", "-e", &orphan]).run();
    safe(&p, &state)
        .args(["reflog", "expire", "--expire=now", "--all"])
        .run();
    // ORIG_HEAD is also a root; remove it explicitly before asking to prune.
    if p.join(".git/ORIG_HEAD").exists() {
        fs::remove_file(p.join(".git/ORIG_HEAD")).unwrap();
    }
    write(p.join("staged"), "staged\n");
    safe(&p, &state).args(["add", "staged"]).run();
    let staged = real(&p).args(["rev-parse", ":staged"]).run();
    safe(&p, &state).args(["gc", "--prune=now"]).run();
    real(&p).args(["cat-file", "-e", &orphan]).fails();
    real(&p).args(["cat-file", "-e", &staged]).run();
    real(&p).args(["fsck", "--full"]).run();
    passed(
        "recorded resolutions are reused without staging; pruning preserves reflogs, index and resolution objects",
    );
}
pub fn merges() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", "sha1");
    let base = revision(&p, "HEAD");
    real(&p).args(["checkout", "--orphan", "unrelated"]).run();
    real(&p).args(["rm", "-rf", "."]).run();
    write(p.join("independent"), "new\n");
    real(&p).args(["add", "."]).run();
    real(&p).args(["commit", "-m", "root"]).run();
    safe(&p, &state).args(["switch", "main"]).run();
    safe(&p, &state).args(["merge", "unrelated"]).fails();
    safe(&p, &state)
        .args(["merge", "--allow-unrelated-histories", "unrelated"])
        .run();
    assert!(p.join("file").exists() && p.join("independent").exists());
    // Construct criss-cross history with two best common ancestors.
    real(&p).args(["reset", "--hard", &base]).run();
    let tree = revision(&p, "HEAD^{tree}");
    let a = real(&p)
        .args(["commit-tree", &tree, "-p", &base])
        .input("a\n")
        .run();
    let b = real(&p)
        .args(["commit-tree", &tree, "-p", &base])
        .input("b\n")
        .run();
    let ab = real(&p)
        .args(["commit-tree", &tree, "-p", &a, "-p", &b])
        .input("ab\n")
        .run();
    let ba = real(&p)
        .args(["commit-tree", &tree, "-p", &b, "-p", &a])
        .input("ba\n")
        .run();
    real(&p).args(["reset", "--hard", &ab]).run();
    safe(&p, &state).args(["merge", &ba]).run();
    assert_eq!(
        real(&p).args(["show", "-s", "--format=%P", "HEAD"]).run(),
        format!("{ab} {ba}")
    );
    real(&p).args(["reset", "--hard", &base]).run();
    // Conflicting binary bases must produce a conflict, not choose a side.
    write(p.join("file"), "left\0binary");
    real(&p).args(["add", "file"]).run();
    let left_tree = real(&p).arg("write-tree").run();
    let left = real(&p)
        .args(["commit-tree", &left_tree, "-p", &base])
        .input("left\n")
        .run();
    write(p.join("file"), "right\0binary");
    real(&p).args(["add", "file"]).run();
    let right_tree = real(&p).arg("write-tree").run();
    let right = real(&p)
        .args(["commit-tree", &right_tree, "-p", &base])
        .input("right\n")
        .run();
    let left_merge = real(&p)
        .args(["commit-tree", &left_tree, "-p", &left, "-p", &right])
        .input("left merge\n")
        .run();
    let right_merge = real(&p)
        .args(["commit-tree", &right_tree, "-p", &right, "-p", &left])
        .input("right merge\n")
        .run();
    real(&p).args(["reset", "--hard", &left_merge]).run();
    safe(&p, &state).args(["merge", &right_merge]).fails();
    assert!(!real(&p).args(["ls-files", "-u"]).run().is_empty());
    safe(&p, &state).args(["merge", "--abort"]).run();
    real(&p).args(["reset", "--hard", &base]).run();
    for (branch, file) in [("one", "one"), ("two", "two"), ("three", "three")] {
        real(&p).args(["switch", "-c", branch, &base]).run();
        write(p.join(file), file);
        real(&p).args(["add", file]).run();
        real(&p).args(["commit", "-m", branch]).run();
    }
    real(&p).args(["switch", "one"]).run();
    real(&p).args(["merge", "--no-edit", "two", "three"]).run();
    real(&p).args(["switch", "-c", "onto", &base]).run();
    write(p.join("onto"), "onto\n");
    real(&p).args(["add", "onto"]).run();
    real(&p).args(["commit", "-m", "onto"]).run();
    safe(&p, &state).args(["switch", "one"]).run();
    safe(&p, &state)
        .args(["rebase", "--rebase-merges", "--onto", "onto", &base])
        .run();
    assert_eq!(
        real(&p)
            .args(["show", "-s", "--format=%P", "HEAD"])
            .run()
            .split_whitespace()
            .count(),
        3
    );
    for file in ["one", "two", "three", "onto"] {
        assert!(p.join(file).exists());
    }
    real(&p).args(["fsck", "--full"]).run();
    passed(
        "explicit unrelated merges, recursive merge bases and octopus-preserving rebase complete in process",
    );
}

pub fn topology_recovery() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", "sha1");
    for i in 0..200 {
        write(p.join(format!("node-{i:03}")), "old\n");
    }
    real(&p).args(["add", "."]).run();
    real(&p).args(["commit", "-m", "files"]).run();
    real(&p).args(["switch", "-c", "directories"]).run();
    for i in 0..200 {
        let path = p.join(format!("node-{i:03}"));
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        write(path.join("child"), "new\n");
    }
    real(&p).args(["add", "-A"]).run();
    real(&p).args(["commit", "-m", "directories"]).run();
    for (from, to, early) in [
        ("main", "directories", true),
        ("main", "directories", false),
        ("directories", "main", true),
        ("directories", "main", false),
    ] {
        real(&p).args(["switch", from]).run();
        if from == "directories" {
            for i in 0..200 {
                chmod(p.join(format!("node-{i:03}")), 0o700);
            }
        }
        let original = revision(&p, "HEAD");
        let index = fs::read(p.join(".git/index")).unwrap();
        let mut child = safe(&p, &state).args(["switch", to]).spawn();
        let txn = p.join(".git/safe-git-checkout");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let ready = if early {
                txn.join(if to == "main" { "old-1" } else { "old-0" })
                    .exists()
            } else if to == "directories" {
                p.join("node-000/child").exists()
            } else {
                p.join("node-000").is_file()
            };
            if ready {
                child.signal(rustix::process::Signal::STOP);
                break;
            }
            assert!(
                child.child.try_wait().unwrap().is_none(),
                "checkout completed before interruption"
            );
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_micros(100));
        }
        child.signal(rustix::process::Signal::KILL);
        child.child.wait().unwrap();
        safe(&p, &state).arg("recover").run();
        assert_eq!(revision(&p, "HEAD"), original);
        assert_eq!(fs::read(p.join(".git/index")).unwrap(), index);
        assert!(real(&p).args(["status", "--porcelain"]).run().is_empty());
        assert!(!txn.exists());
        if from == "directories" {
            use std::os::unix::fs::PermissionsExt;
            for i in 0..200 {
                assert_eq!(
                    fs::metadata(p.join(format!("node-{i:03}")))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
            }
        }
    }
    passed(
        "file/directory transitions recover after interruption during backup and installation in both directions",
    );
}

pub fn gc_protection() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", "sha256");
    let linked = root.join("linked");
    safe(&p, &state)
        .args(["worktree", "add", "--detach"])
        .arg(&linked)
        .run();
    safe(&linked, &state)
        .args(["commit", "--allow-empty", "-m", "detached"])
        .run();
    let detached = revision(&linked, "HEAD");
    write(linked.join("staged"), "staged in linked worktree\n");
    safe(&linked, &state).args(["add", "staged"]).run();
    let staged = real(&linked).args(["rev-parse", ":staged"]).run();
    safe(&p, &state).args(["gc", "--prune=now"]).run();
    real(&p).args(["cat-file", "-e", &detached]).run();
    real(&p).args(["cat-file", "-e", &staged]).run();
    let foreign = root.join("foreign");
    real(&p)
        .args(["worktree", "add", "--detach"])
        .arg(&foreign)
        .run();
    safe(&p, &state).args(["gc", "--prune=now"]).fails();
    real(&p).args(["worktree", "remove"]).arg(&foreign).run();
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    write(outside.join("sentinel"), "keep\n");
    symlink(&outside, p.join(".git/logs/hostile")).unwrap();
    safe(&p, &state).args(["gc", "--prune=now"]).fails();
    assert_eq!(read(outside.join("sentinel")), "keep\n");
    passed(
        "pruning protects detached linked HEADs and indexes and rejects unregistered or symlinked metadata",
    );
}
