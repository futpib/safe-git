//! Ordinary developer commands and hostile worktrees, compared with real Git.
use crate::support::*;
use std::{fs, os::unix::fs::symlink, path::Path};

fn safe(repo: &Path, state: &Path) -> TestCommand {
    TestCommand::new(BIN)
        .arg("-C")
        .arg(repo)
        .env("SAFE_GIT_STATE_DIR", state)
        .env("GIT_AUTHOR_DATE", "1234567890 +0000")
        .env("GIT_COMMITTER_DATE", "1234567890 +0000")
        .args([
            "-c",
            "user.name=workflow",
            "-c",
            "user.email=workflow@example.invalid",
        ])
}
fn real(repo: &Path) -> TestCommand {
    git(repo)
        .env("GIT_AUTHOR_DATE", "1234567890 +0000")
        .env("GIT_COMMITTER_DATE", "1234567890 +0000")
        .args([
            "-c",
            "user.name=workflow",
            "-c",
            "user.email=workflow@example.invalid",
        ])
}
fn equivalent(work: &Path, expected: &Path, state: &Path) {
    for args in [
        vec!["status", "--porcelain", "-uall"],
        vec!["status", "--porcelain", "-z"],
        vec!["diff", "--staged", "--no-ext-diff"],
        vec!["diff", "--no-ext-diff"],
    ] {
        let got = safe(work, state).args(&args).output();
        let want = real(expected).args(&args).output();
        assert_eq!(got.code, Some(0), "{args:?}: {got:?}");
        assert_eq!(got.stdout, want.stdout, "{args:?}");
    }
    assert_eq!(
        real(work).args(["write-tree"]).run(),
        real(expected).args(["write-tree"]).run()
    );
}
pub fn suite() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let state = root.join("state");
    let seed = root.join("seed");
    let work = root.join("work");
    let expected = root.join("expected");
    real(root)
        .args(["init", "-q", "-b", "main"])
        .arg(&seed)
        .run();
    fs::create_dir(seed.join("dir")).unwrap();
    write(seed.join("tracked"), "one\ntwo\nthree\n");
    write(seed.join("deleted"), "delete me\n");
    write(seed.join("dir/file"), "nested\n");
    write(
        seed.join(".gitignore"),
        "target/\n*.tmp\n!keep.tmp\nlogs/**\n",
    );
    symlink("tracked", seed.join("link")).unwrap();
    real(&seed).args(["add", "."]).run();
    real(&seed).args(["commit", "-qm", "base"]).run();
    safe(root, &state).arg("clone").arg(&seed).arg(&work).run();
    real(root)
        .args(["clone", "-q"])
        .arg(&seed)
        .arg(&expected)
        .run();
    assert!(
        safe(&work, &state)
            .arg("status")
            .run()
            .contains("working tree clean")
    );
    safe(&work, &state).args(["switch", "-c", "feature"]).run();
    real(&expected).args(["switch", "-c", "feature"]).run();
    for repo in [&work, &expected] {
        write(repo.join("tracked"), "one\nSTAGED\nthree\n");
        write(repo.join("new file"), "new\n");
        write(repo.join("dir/new"), "nested new\n");
        fs::create_dir(repo.join("target")).unwrap();
        write(repo.join("target/artifact"), "ignored\n");
        write(repo.join("other.tmp"), "ignored\n");
        write(repo.join("keep.tmp"), "included\n");
    }
    equivalent(&work, &expected, &state);
    for args in [
        vec!["add", "tracked", "new file"],
        vec!["restore", "--staged", "new file"],
        vec!["add", "dir"],
        vec!["add", "-A"],
    ] {
        safe(&work, &state).args(&args).run();
        real(&expected).args(&args).run();
        equivalent(&work, &expected, &state);
    }
    for repo in [&work, &expected] {
        write(repo.join("tracked"), "one\nSTAGED\nUNSTAGED\n");
        fs::remove_file(repo.join("deleted")).unwrap();
        chmod(repo.join("dir/file"), 0o755);
    }
    equivalent(&work, &expected, &state);
    for args in [
        vec!["restore", "dir/file"],
        vec!["add", "-u", "deleted"],
        vec!["restore", "--staged", "deleted"],
    ] {
        safe(&work, &state).args(&args).run();
        real(&expected).args(&args).run();
        equivalent(&work, &expected, &state);
    }
    safe(&work, &state)
        .args(["commit", "-m", "selected changes"])
        .run();
    real(&expected)
        .args(["commit", "-m", "selected changes"])
        .run();
    assert_eq!(revision(&work, "HEAD"), revision(&expected, "HEAD"));
    equivalent(&work, &expected, &state);
    for args in [
        vec!["log", "--oneline", "-10"],
        vec!["log", "--oneline", "-n", "1"],
    ] {
        assert_eq!(
            safe(&work, &state).args(&args).output().stdout,
            real(&expected).args(&args).output().stdout
        );
    }
    passed(
        "status, selective/recursive staging, unstaging, restore, switch and abbreviated log counts match Git",
    );

    for repo in [&work, &expected] {
        write(repo.join("tracked"), "one\nSTASH INDEX\nthree\n");
        write(repo.join("added"), "stash new\n");
    }
    safe(&work, &state).args(["add", "tracked", "added"]).run();
    real(&expected).args(["add", "tracked", "added"]).run();
    for repo in [&work, &expected] {
        write(repo.join("tracked"), "one\nSTASH INDEX\nSTASH WORK\n");
    }
    let before_index = real(&work).args(["write-tree"]).run();
    safe(&work, &state)
        .args(["stash", "push", "-m", "saved work"])
        .run();
    real(&expected)
        .args(["stash", "push", "-m", "saved work"])
        .run();
    equivalent(&work, &expected, &state);
    assert_eq!(
        revision(&work, "refs/stash^{tree}"),
        revision(&expected, "refs/stash^{tree}")
    );
    assert_eq!(revision(&work, "refs/stash^2^{tree}"), before_index);
    assert_eq!(
        safe(&work, &state).args(["stash", "list"]).output().stdout,
        real(&expected).args(["stash", "list"]).output().stdout
    );
    safe(&work, &state).args(["stash", "pop", "--index"]).run();
    real(&expected).args(["stash", "pop", "--index"]).run();
    equivalent(&work, &expected, &state);
    // Cross-client compatibility in both directions, including the default pop behavior.
    safe(&work, &state).arg("stash").run();
    real(&expected).arg("stash").run();
    real(&work).args(["stash", "pop"]).run();
    safe(&expected, &state).args(["stash", "pop"]).run();
    equivalent(&work, &expected, &state);
    // Stacks are LIFO, and conflicts leave both the worktree and stash intact.
    safe(&work, &state)
        .args(["stash", "push", "-m", "first"])
        .run();
    write(work.join("tracked"), "second\n");
    safe(&work, &state)
        .args(["stash", "push", "-m", "second"])
        .run();
    safe(&work, &state).args(["stash", "pop"]).run();
    assert_eq!(read(work.join("tracked")), "second\n");
    let saved = revision(&work, "refs/stash");
    assert!(
        safe(&work, &state)
            .args(["stash", "pop"])
            .fails()
            .contains("conflict")
    );
    assert_eq!(read(work.join("tracked")), "second\n");
    assert_eq!(revision(&work, "refs/stash"), saved);
    safe(&work, &state).args(["restore", "tracked"]).run();
    safe(&work, &state).args(["stash", "pop"]).run();
    passed(
        "stash preserves staged/unstaged data, interoperates with Git, stacks entries and retains conflicts",
    );

    let unborn = root.join("unborn");
    real(root)
        .args(["init", "-q", "-b", "main"])
        .arg(&unborn)
        .run();
    real(&unborn).arg("fetch").arg(&seed).arg("main").run();
    real(&unborn)
        .args(["branch", "existing", "FETCH_HEAD"])
        .run();
    safe(&unborn, &state)
        .args(["switch", "-C", "existing"])
        .fails();
    assert_eq!(
        real(&unborn).args(["symbolic-ref", "HEAD"]).run(),
        "refs/heads/main"
    );
    safe(&unborn, &state)
        .args(["switch", "-c", "initial"])
        .run();
    write(unborn.join("first"), "first\n");
    safe(&unborn, &state).args(["add", "first"]).run();
    assert_eq!(
        safe(&unborn, &state).args(["status", "--porcelain"]).run(),
        "A  first"
    );
    safe(&unborn, &state)
        .args(["restore", "--staged", "first"])
        .run();
    assert_eq!(
        safe(&unborn, &state).args(["status", "--porcelain"]).run(),
        "?? first"
    );
    safe(&unborn, &state).args(["add", "."]).run();
    safe(&unborn, &state).args(["commit", "-m", "first"]).run();
    passed("initial staging, unstaging and first commit work without an existing HEAD");

    // Hostile config, hooks and attributes are never execution instructions.
    let marker = root.join("EXECUTED");
    let probe = root.join("probe");
    write(
        &probe,
        format!("#!/bin/sh\ntouch {}\nexit 99\n", quote(&marker)),
    );
    chmod(&probe, 0o700);
    for key in [
        "core.hooksPath",
        "core.fsmonitor",
        "core.sshCommand",
        "core.pager",
        "credential.helper",
        "diff.external",
        "filter.probe.process",
        "filter.probe.clean",
        "filter.probe.smudge",
        "merge.probe.driver",
    ] {
        git(&unborn).args(["config", key]).arg(&probe).run();
    }
    for hook in [
        "pre-commit",
        "post-commit",
        "post-checkout",
        "reference-transaction",
    ] {
        fs::copy(&probe, unborn.join(".git/hooks").join(hook)).unwrap();
    }
    write(
        unborn.join(".gitattributes"),
        "* filter=probe diff=probe merge=probe\n",
    );
    write(unborn.join("first"), "inert\n");
    for (i, args) in [
        vec!["add", "."],
        vec!["status"],
        vec!["commit", "-m", "inert"],
        vec!["switch", "-c", "inert"],
    ]
    .iter()
    .enumerate()
    {
        let trace = root.join(format!("workflow-{i}.trace"));
        safe(&unborn, &state).args(args).traced(&trace).run();
        check_trace(&trace);
    }
    write(unborn.join("first"), "saved inert\n");
    for (i, args) in [
        vec!["stash"],
        vec!["stash", "pop"],
        vec!["restore", "first"],
    ]
    .iter()
    .enumerate()
    {
        let trace = root.join(format!("stash-{i}.trace"));
        safe(&unborn, &state).args(args).traced(&trace).run();
        check_trace(&trace);
    }
    assert!(!marker.exists());
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    write(outside.join("secret"), "untouched\n");
    symlink(&outside, unborn.join("escape")).unwrap();
    let index_before = fs::read(unborn.join(".git/index")).unwrap();
    safe(&unborn, &state).args(["add", "escape/secret"]).fails();
    assert_eq!(fs::read(unborn.join(".git/index")).unwrap(), index_before);
    assert_eq!(read(outside.join("secret")), "untouched\n");
    fs::hard_link(outside.join("secret"), unborn.join("hardlink")).unwrap();
    write(unborn.join("first"), "must not stage on failure\n");
    safe(&unborn, &state)
        .args(["add", "first", "hardlink"])
        .fails();
    assert_eq!(fs::read(unborn.join(".git/index")).unwrap(), index_before);
    safe(&unborn, &state)
        .args(["add", "../outside/secret"])
        .fails();
    safe(&unborn, &state).args(["add", ".git/config"]).fails();
    // A lock symlink is refused rather than followed or replaced.
    symlink(outside.join("secret"), unborn.join(".git/index.lock")).unwrap();
    safe(&unborn, &state).args(["add", "first"]).fails();
    assert_eq!(read(outside.join("secret")), "untouched\n");
    passed(
        "new workflow commands run no child processes and reject escaping paths, hard links and hostile index locks",
    );
}

pub fn network() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let seed = root.join("seed");
    let source = root.join("source.git");
    let trap = root.join("trap.git");
    let work = root.join("work");
    real(root)
        .args(["init", "-q", "-b", "main"])
        .arg(&seed)
        .run();
    write(seed.join("file"), "base\n");
    real(&seed).args(["add", "."]).run();
    real(&seed).args(["commit", "-qm", "base"]).run();
    for repo in [&source, &trap] {
        real(root)
            .args(["clone", "--bare", "-q"])
            .arg(&seed)
            .arg(repo)
            .run();
    }
    fixture
        .safe(root)
        .arg("clone")
        .arg(fixture.url(&source))
        .arg(&work)
        .run();
    fixture.safe(&work).args(["switch", "-c", "feature"]).run();
    write(work.join("file"), "feature\n");
    fixture.safe(&work).args(["add", "file"]).run();
    fixture
        .safe(&work)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "feature",
        ])
        .run();
    let tip = revision(&work, "HEAD");
    // Config tries to redirect both endpoint and destination branch.
    git(&work)
        .args(["config", "remote.origin.url"])
        .arg(fixture.url(&trap))
        .run();
    git(&work)
        .args(["config", "remote.origin.pushurl"])
        .arg(fixture.url(&trap))
        .run();
    git(&work)
        .args(["config", "branch.feature.remote", "evil"])
        .run();
    git(&work)
        .args(["config", "branch.feature.merge", "refs/heads/main"])
        .run();
    fixture
        .safe(&work)
        .args(["push", "-u", "origin", "feature:review"])
        .run();
    assert_eq!(revision(&source, "review"), tip);
    assert_eq!(
        fixture.safe(&work).args(["rev-parse", "HEAD@{u}"]).run(),
        tip
    );
    assert_eq!(revision(&trap, "main"), revision(&seed, "HEAD"));
    git(&trap).args(["rev-parse", "--verify", "review"]).fails();
    write(work.join("file"), "second\n");
    fixture
        .safe(&work)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-am",
            "second",
        ])
        .run();
    fixture.safe(&work).arg("push").run();
    assert_eq!(revision(&source, "review"), revision(&work, "HEAD"));
    // A plain pull uses the trusted mapping even when repo config lies.
    real(&seed).arg("fetch").arg(&source).arg("review").run();
    real(&seed)
        .args(["checkout", "-B", "review", "FETCH_HEAD"])
        .run();
    write(seed.join("file"), "remote update\n");
    real(&seed).args(["commit", "-am", "remote"]).run();
    real(&seed).arg("push").arg(&source).arg("review").run();
    fixture.safe(&work).args(["pull", "--ff-only"]).run();
    assert_eq!(read(work.join("file")), "remote update\n");
    // A Git-created checkout does not gain trust from its config alone.
    let unregistered = root.join("unregistered");
    real(root)
        .args(["clone", "-q"])
        .arg(&seed)
        .arg(&unregistered)
        .run();
    assert!(
        fixture
            .safe(&unregistered)
            .arg("push")
            .fails()
            .contains("repository remote configuration is not consulted")
    );
    fixture
        .safe(&unregistered)
        .args(["push", "-u", "origin", "review"])
        .fails();
    // Caller-provided endpoint environment can register an existing checkout,
    // without deriving anything from that checkout's remote configuration.
    fixture
        .safe(&unregistered)
        .env("SAFE_GIT_URL", fixture.url(&source))
        .args(["push", "-u", "origin", "review:existing"])
        .run();
    assert_eq!(
        revision(&source, "existing"),
        revision(&unregistered, "HEAD")
    );
    passed(
        "push -u and subsequent push/pull use host-bound destinations despite hostile remote and branch config",
    );
}

pub fn ignores() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let state = root.join("state");
    let repo = root.join("repo");
    real(root)
        .args(["init", "-q", "-b", "main"])
        .arg(&repo)
        .run();
    write(
        repo.join(".gitignore"),
        "/rooted\n*.tmp\n!keep.tmp\nignored/\n**/deep.secret\ncache/**/discard\n[ab][0-9].dat\n\\#literal\nescaped\\ name\n",
    );
    write(repo.join(".git/info/exclude"), "excluded\n");
    for dir in ["nested", "ignored", "cache", "cache/a", "cache/a/b"] {
        fs::create_dir(repo.join(dir)).unwrap();
    }
    write(
        repo.join("nested/.gitignore"),
        "*.local\n!keep.local\n/rooted\n",
    );
    let ignored = [
        "rooted",
        "foo.tmp",
        "nested/foo.tmp",
        "ignored/file",
        "deep.secret",
        "nested/deep.secret",
        "cache/discard",
        "cache/a/b/discard",
        "a1.dat",
        "#literal",
        "escaped name",
        "excluded",
        "nested/no.local",
        "nested/rooted",
    ];
    let included = [
        "keep.tmp",
        "nested/keep.tmp",
        "nested/keep.local",
        "c1.dat",
        "cache/a/keep",
        "plain",
    ];
    for path in ignored.iter().chain(&included) {
        write(repo.join(path), "data\n");
    }
    for args in [
        vec!["status", "--porcelain", "-uall"],
        vec!["status", "--porcelain", "-z"],
    ] {
        assert_eq!(
            safe(&repo, &state).args(&args).output().stdout,
            real(&repo).args(&args).output().stdout
        );
    }
    safe(&repo, &state).args(["add", "."]).run();
    let staged = real(&repo).args(["ls-files"]).run();
    for path in ignored {
        assert!(
            !staged.lines().any(|p| p == path),
            "ignored path was staged: {path}"
        );
    }
    for path in included {
        assert!(
            staged.lines().any(|p| p == path),
            "included path was omitted: {path}"
        );
    }
    let index = fs::read(repo.join(".git/index")).unwrap();
    safe(&repo, &state).args(["add", "ignored/file"]).fails();
    assert_eq!(fs::read(repo.join(".git/index")).unwrap(), index);
    safe(&repo, &state)
        .args(["add", "-f", "ignored/file"])
        .run();
    assert!(
        real(&repo)
            .args(["ls-files", "ignored/file"])
            .run()
            .contains("ignored/file")
    );
    safe(&repo, &state)
        .args(["commit", "-m", "ignored rules"])
        .run();
    // Tracked files continue to stage even if a later ignore rule hides them.
    append(repo.join(".gitignore"), "plain\n");
    write(repo.join("plain"), "tracked despite ignore\n");
    safe(&repo, &state).args(["add", "plain"]).run();
    assert_eq!(
        real(&repo).args(["show", ":plain"]).run(),
        "tracked despite ignore"
    );
    // Never follow a symlinked ignore file or descend into an embedded repo.
    fs::remove_file(repo.join("nested/.gitignore")).unwrap();
    symlink(root.join("missing"), repo.join("nested/.gitignore")).unwrap();
    safe(&repo, &state).args(["status", "--porcelain"]).run();
    real(root)
        .args(["init", "-q"])
        .arg(repo.join("embedded"))
        .run();
    write(repo.join("embedded/private"), "nested data\n");
    let index = fs::read(repo.join(".git/index")).unwrap();
    safe(&repo, &state).args(["add", "embedded"]).fails();
    assert_eq!(fs::read(repo.join(".git/index")).unwrap(), index);
    // Unknown matcher syntax fails before altering the index.
    append(repo.join(".gitignore"), "[[:alpha:]]\n");
    safe(&repo, &state).args(["add", "plain"]).fails();
    assert_eq!(fs::read(repo.join(".git/index")).unwrap(), index);
    passed(
        "nested ignores, negation, anchored wildcards, forced staging and hostile ignore paths are bounded and inert",
    );
}
