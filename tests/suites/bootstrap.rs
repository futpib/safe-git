//! Working-tree clone and one-lock fast-forward pull, using real Git/OpenSSH.
use crate::support::*;
use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::symlink},
};

pub fn suite() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let seed = root.join("seed");
    let source = root.join("source.git");
    let dest = root.join("dest.git");
    let work = root.join("work");
    let url = fixture.url(&source);
    let marker = root.join("EXECUTED");
    let probe = root.join("probe");
    write(
        &probe,
        format!("#!/bin/sh\ntouch {}\nexit 99\n", quote(&marker)),
    );
    chmod(&probe, 0o700);
    git(root)
        .args(["init", "-q", "-b", "master"])
        .arg(&seed)
        .run();
    git(&seed).args(["config", "user.name", "test"]).run();
    git(&seed)
        .args(["config", "user.email", "test@example.invalid"])
        .run();
    write(seed.join("payload"), "base\n");
    write(seed.join("unchanged"), "keep\n");
    write(seed.join(".gitattributes"), "payload filter=probe\n");
    write(
        seed.join(".gitmodules"),
        "[submodule \"evil\"]\npath = evil\nurl = ext::probe\n",
    );
    fs::copy(&probe, seed.join("script")).unwrap();
    symlink("payload", seed.join("link")).unwrap();
    git(&seed).args(["add", "."]).run();
    git(&seed).args(["commit", "-qm", "base"]).run();
    let base = revision(&seed, "HEAD");
    git(&seed).args(["branch", "other"]).run();
    git(&seed).args(["tag", "tag"]).run();
    write(seed.join("payload"), "forward\n");
    git(&seed).args(["commit", "-qam", "forward"]).run();
    let forward = revision(&seed, "HEAD");
    git(root)
        .args(["clone", "-q", "--bare"])
        .arg(&seed)
        .arg(&source)
        .run();
    git(&source)
        .args(["update-ref", "refs/heads/master", &base])
        .run();
    let hostile = |cmd: TestCommand| {
        cmd.env("GIT_CONFIG_GLOBAL", &probe)
            .env("GIT_SSH_COMMAND", &probe)
            .env("GIT_TEMPLATE_DIR", root.join("templates"))
            .env("GIT_DIR", root.join("wrong"))
    };
    let clone = |path: &std::path::Path| {
        hostile(fixture.safe(root))
            .args(["clone", "--single-branch", "--", &url])
            .arg(path)
    };
    let safe = |args: &[&str]| hostile(fixture.safe(&work)).args(args);
    let trace = root.join("clone.trace");
    clone(&work).traced(&trace).run();
    check_trace(&trace);
    assert_eq!(revision(&work, "HEAD"), base);
    assert_eq!(
        git(&work).args(["symbolic-ref", "HEAD"]).run(),
        "refs/heads/master"
    );
    assert_eq!(revision(&work, "origin/master"), base);
    assert_eq!(read(work.join("payload")), "base\n");
    assert_eq!(
        fs::read_link(work.join("link")).unwrap(),
        std::path::Path::new("payload")
    );
    assert_eq!(git(&work).args(["config", "remote.origin.url"]).run(), url);
    assert!(git(&work).args(["status", "--porcelain"]).run().is_empty());
    git(&work).args(["fsck", "--full"]).run();
    git(&work)
        .args(["show-ref", "--verify", "refs/remotes/origin/other"])
        .fails();
    git(&work)
        .args(["show-ref", "--verify", "refs/tags/tag"])
        .fails();
    assert!(!marker.exists());
    passed(
        "single-branch working-tree clone chooses advertised master; Git accepts index and objects; no child exec",
    );

    let selected = root.join("selected");
    hostile(fixture.safe(root))
        .args(["clone", "--branch", "other", &url])
        .arg(&selected)
        .run();
    assert_eq!(
        git(&selected).args(["symbolic-ref", "HEAD"]).run(),
        "refs/heads/other"
    );
    let empty = root.join("empty-destination");
    fs::create_dir(&empty).unwrap();
    clone(&empty).run();
    assert_eq!(revision(&empty, "HEAD"), base);
    // zond supplies /proc/PID/fd/FD/output inside its own private staging dir.
    let directory = fs::File::open(root).unwrap();
    let proc_path = std::path::PathBuf::from(format!(
        "/proc/{}/fd/{}/published",
        std::process::id(),
        directory.as_raw_fd()
    ));
    clone(&proc_path).run();
    assert_eq!(revision(&root.join("published"), "HEAD"), base);
    passed("explicit branch, existing empty destination, and zond proc-fd destination work");

    let ssh = format!(
        "ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o {} -o StrictHostKeyChecking=yes",
        quote(root.join("client_key")),
        quote(format!("UserKnownHostsFile={}/known_hosts", root.display()))
    );
    let compare = root.join("compare");
    let got = clone(&compare).output();
    fs::remove_dir_all(&compare).unwrap();
    let expected = git(root)
        .args(["clone", "--single-branch", "--no-progress", &url])
        .arg(&compare)
        .env("GIT_SSH_COMMAND", &ssh)
        .output();
    assert_eq!(got, expected);
    let quiet = root.join("quiet");
    let got = fixture
        .safe(root)
        .args(["clone", "-q", &url])
        .arg(&quiet)
        .output();
    assert_eq!(
        got,
        Captured {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new()
        }
    );
    passed("clone banner and streams match Git; quiet clone is silent");

    let blocked = root.join("blocked");
    write(&blocked, "preserve");
    clone(&blocked).fails();
    assert_eq!(read(&blocked), "preserve");
    let linked = root.join("linked");
    symlink(&work, &linked).unwrap();
    clone(&linked).fails();
    clone(&work).fails();
    let failed = root.join("failed");
    fixture
        .safe(root)
        .args(["clone", "-b", "missing", &url])
        .arg(&failed)
        .fails();
    assert!(!failed.exists());
    fs::create_dir(&failed).unwrap();
    fixture
        .safe(root)
        .args(["clone", "-b", "missing", &url])
        .arg(&failed)
        .fails();
    assert_eq!(fs::read_dir(&failed).unwrap().count(), 0);
    passed(
        "clone refuses files, symlinks, and populated directories; errors preserve empty destinations",
    );

    let config = work.join(".git/config");
    append(
        &config,
        &format!(
            "[core]\nsshCommand = {}\nhooksPath = {}\nfsmonitor = {}\n[filter \"probe\"]\nprocess = {}\n[branch \"master\"]\nmerge = refs/heads/other\n[url \"ext::probe\"]\ninsteadOf = ssh://\n",
            probe.display(),
            root.join("hooks").display(),
            probe.display(),
            probe.display()
        ),
    );
    fs::create_dir(root.join("hooks")).unwrap();
    for name in ["post-merge", "reference-transaction", "post-checkout"] {
        fs::copy(&probe, root.join("hooks").join(name)).unwrap();
    }
    write(work.join("unchanged"), "local edit\n");
    write(work.join("untracked"), "local file\n");
    git(&source)
        .args(["update-ref", "refs/heads/master", &forward])
        .run();
    safe(&["pull", "--ff-only", "--no-recurse-submodules", "--", &url])
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert_eq!(revision(&work, "HEAD"), forward);
    assert_eq!(read(work.join("payload")), "forward\n");
    assert_eq!(read(work.join("unchanged")), "local edit\n");
    assert_eq!(read(work.join("untracked")), "local file\n");
    assert!(!marker.exists());
    passed(
        "pull ignores hostile local config and hooks, uses current branch, preserves unrelated edits, and executes no children",
    );

    for _ in 0..2 {
        let got = fixture
            .safe(&empty)
            .args(["pull", "--ff-only", &url, "master"])
            .output();
        let expected = git(&compare)
            .args([
                "pull",
                "--ff-only",
                "--no-recurse-submodules",
                &url,
                "master",
            ])
            .env("GIT_SSH_COMMAND", &ssh)
            .output();
        assert_eq!(got, expected);
    }
    passed("fast-forward and unchanged pull stdout/stderr match Git");

    let got = safe(&["pull", "--ff-only", "-q", &url]).output();
    assert_eq!(
        got,
        Captured {
            code: Some(0),
            stdout: String::new(),
            stderr: String::new()
        }
    );
    safe(&["pull", &url]).fails();
    safe(&["pull", "--ff-only", "--rebase", &url]).fails();
    safe(&["pull", "--ff-only", "--recurse-submodules", &url]).fails();
    let lock = fs::File::open(work.join(".git/.safe-git.lock")).unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
    assert!(
        safe(&["pull", "--ff-only", &url])
            .fails()
            .contains("locked")
    );
    drop(lock);
    passed(
        "pull requires ff-only, refuses rebase/submodule execution and concurrent lock; quiet no-op is silent",
    );

    let tree = revision(&seed, "HEAD^{tree}");
    let side = git(&seed)
        .args(["commit-tree", &tree, "-p", &base])
        .input("side\n")
        .run();
    git(&source)
        .args(["fetch", "-q"])
        .arg(&seed)
        .arg(&side)
        .run();
    git(&source)
        .args(["update-ref", "refs/heads/master", &side])
        .run();
    let index = fs::read(work.join(".git/index")).unwrap();
    assert!(
        safe(&["pull", "--ff-only", &url])
            .fails()
            .contains("Not possible to fast-forward")
    );
    assert_eq!(revision(&work, "HEAD"), forward);
    assert_eq!(fs::read(work.join(".git/index")).unwrap(), index);
    git(&source)
        .args(["update-ref", "refs/heads/master", &base])
        .run();
    safe(&["pull", "--ff-only", &url]).run();
    assert_eq!(revision(&work, "HEAD"), forward);
    passed(
        "divergent pull preserves checkout and HEAD; upstream rewind never rewinds local branch",
    );

    let stalled = root.join("stalled");
    clone(&stalled).run();
    // A tracking ref ahead of HEAD must not be merged after a failed fetch.
    git(&stalled)
        .args(["fetch", "-q"])
        .arg(&seed)
        .arg(&forward)
        .run();
    git(&stalled)
        .args(["update-ref", "refs/remotes/origin/master", &forward])
        .run();
    write(root.join("malformed"), "frame");
    fixture
        .safe(&stalled)
        .args(["pull", "--ff-only", &url])
        .fails();
    assert_eq!(revision(&stalled, "HEAD"), base);
    let bad_clone = root.join("bad-clone");
    clone(&bad_clone).fails();
    assert!(!bad_clone.exists());
    fs::remove_file(root.join("malformed")).unwrap();
    passed("failed fetch cannot merge stale tracking data; malformed discovery publishes no clone");

    write(seed.join("payload"), "third\n");
    git(&seed).args(["commit", "-qam", "third"]).run();
    let third = revision(&seed, "HEAD");
    git(&source)
        .args(["fetch", "-q"])
        .arg(&seed)
        .arg("master:master")
        .run();
    write(stalled.join("payload"), "overlapping local edit\n");
    fixture
        .safe(&stalled)
        .args(["pull", "--ff-only", &url])
        .fails();
    assert_eq!(read(stalled.join("payload")), "overlapping local edit\n");
    assert_eq!(revision(&stalled, "HEAD"), base);
    // Explicitly selected source branch still updates the current local branch.
    fixture
        .safe(&selected)
        .args(["pull", "--ff-only", "--no-stat", &url, "master"])
        .run();
    assert_eq!(revision(&selected, "HEAD"), third);
    assert_eq!(
        git(&selected).args(["symbolic-ref", "HEAD"]).run(),
        "refs/heads/other"
    );
    passed("pull preserves overlapping edits on refusal and supports an explicit fetched branch");

    fs::remove_file(seed.join("link")).unwrap();
    symlink("/etc/passwd", seed.join("link")).unwrap();
    git(&seed).args(["commit", "-qam", "unsafe tree"]).run();
    git(&source)
        .args(["fetch", "-q"])
        .arg(&seed)
        .arg("master:master")
        .run();
    let unsafe_clone = root.join("unsafe");
    clone(&unsafe_clone).fails();
    assert!(!unsafe_clone.exists());
    assert!(!fs::read_dir(root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".safe-git-clone-")
    }));
    assert!(!marker.exists());
    passed("unsafe checkout paths abort clone and remove private staging without publishing");

    git(root)
        .args(["init", "-q", "--bare", "-b", "master"])
        .arg(&dest)
        .run();
    let empty_clone = root.join("empty-clone");
    let got = fixture
        .safe(root)
        .args(["clone", &fixture.url(&dest)])
        .arg(&empty_clone)
        .output();
    assert_eq!(got.code, Some(0), "{got:?}");
    assert!(got.stderr.contains("empty repository"));
    assert_eq!(
        git(&empty_clone).args(["symbolic-ref", "HEAD"]).run(),
        "refs/heads/master"
    );
    assert!(
        git(&empty_clone)
            .args(["status", "--porcelain"])
            .run()
            .is_empty()
    );
    passed("empty remote creates an ordinary unborn master working tree");
}
