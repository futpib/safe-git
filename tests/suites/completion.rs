//! End-to-end Git equivalence and hostile input checks for the final workflow gaps.
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
        .env("GIT_AUTHOR_DATE", "1234567890 +0000")
        .env("GIT_COMMITTER_DATE", "1234567890 +0000")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
}
fn real(repo: &Path) -> TestCommand {
    git(repo)
        .env("GIT_AUTHOR_DATE", "1234567890 +0000")
        .env("GIT_COMMITTER_DATE", "1234567890 +0000")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
}
fn seed(root: &Path, state: &Path, name: &str) -> PathBuf {
    let p = root.join(name);
    safe(root, state).arg("init").arg(&p).run();
    write(p.join("file"), "one\ntwo\nthree\n");
    safe(&p, state).args(["add", "file"]).run();
    safe(&p, state).args(["commit", "-m", "base"]).run();
    p
}
fn equal(repo: &Path, state: &Path, args: &[&str]) {
    let got = safe(repo, state).args(args).output();
    let want = real(repo).args(args).output();
    assert_eq!(got.code, want.code, "{args:?}: {got:?} / {want:?}");
    assert_eq!(got.stdout, want.stdout, "{args:?}");
}
pub fn inspection() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state, "repo");
    fs::create_dir(repo.join("dir")).unwrap();
    write(repo.join("dir/new"), "two\n");
    write(repo.join("other"), "unrelated\n");
    write(repo.join("file"), "one\nTWO  \nthree\n");
    equal(&repo, &state, &["status", "--porcelain", "--", "file"]);
    equal(&repo, &state, &["status", "--porcelain", "--", "dir/new"]);
    equal(&repo, &state, &["diff", "--check"]);
    equal(&repo, &state, &["diff", "--name-status"]);
    equal(&repo, &state, &["grep", "-n", "-i", "two"]);
    equal(&repo, &state, &["grep", "--cached", "two"]);
    equal(&repo, &state, &["grep", "absent"]);
    write(
        repo.join("message"),
        "Second commit\n\nDetails with needle\n",
    );
    safe(&repo, &state)
        .args(["commit", "-a", "-F", "message"])
        .run();
    for args in [
        vec!["log", "--oneline", "--author=Test"],
        vec!["log", "--oneline", "--grep=^Details"],
        vec!["log", "--oneline", "--grep=absent"],
        vec!["log", "--oneline", "-i", "--grep=second"],
    ] {
        equal(&repo, &state, &args);
    }
    write(repo.join("file"), "fixed\n");
    safe(&repo, &state).args(["add", "file"]).run();
    safe(&repo, &state).args(["commit", "--fixup=HEAD"]).run();
    assert_eq!(
        real(&repo).args(["log", "-1", "--format=%s"]).run(),
        "fixup! Second commit"
    );
    let config = root.join("global");
    safe(root, &state)
        .env("GIT_CONFIG_GLOBAL", &config)
        .args(["config", "--global", "user.name", "Global name"])
        .run();
    assert_eq!(
        safe(root, &state)
            .env("GIT_CONFIG_GLOBAL", &config)
            .args(["config", "--global", "--get", "user.name"])
            .run(),
        "Global name"
    );
    let outside = root.join("outside");
    write(&outside, "secret\n");
    symlink(&outside, repo.join("message-link")).unwrap();
    assert!(
        safe(&repo, &state)
            .args(["commit", "-F", "message-link"])
            .output()
            .code
            != Some(0)
    );
    let marker = root.join("executed");
    let cmd = format!("touch {}", marker.display());
    real(&repo).args(["config", "core.pager", &cmd]).run();
    real(&repo).args(["config", "diff.external", &cmd]).run();
    let trace = root.join("trace");
    safe(&repo, &state)
        .args(["grep", "fixed"])
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert!(!marker.exists());
    passed(
        "path status, diff checks, search, commit files/fixups and host-only global identity settings",
    );
}
pub fn patches() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state, "repo");
    let base = revision(&repo, "HEAD");
    write(repo.join("file"), "one\nTWO\nthree\n");
    write(repo.join("new file"), "new\n");
    safe(&repo, &state).args(["add", "."]).run();
    safe(&repo, &state)
        .args(["commit", "-m", "change\n\nbody"])
        .run();
    let expected = revision(&repo, "HEAD^{tree}");
    let patch = real(&repo)
        .args(["diff", "--binary", "HEAD^", "HEAD"])
        .output()
        .stdout;
    safe(&repo, &state).args(["reset", "--hard", &base]).run();
    let before = read(repo.join("file"));
    safe(&repo, &state)
        .args(["apply", "--check"])
        .input(&patch)
        .run();
    assert_eq!(read(repo.join("file")), before);
    assert!(!repo.join("new file").exists());
    safe(&repo, &state)
        .args(["apply", "--index"])
        .input(&patch)
        .run();
    assert_eq!(real(&repo).args(["write-tree"]).run(), expected);
    safe(&repo, &state)
        .args(["apply", "--index", "-R"])
        .input(&patch)
        .run();
    assert_eq!(
        revision(&repo, "HEAD^{tree}"),
        real(&repo).args(["write-tree"]).run()
    );
    safe(&repo, &state)
        .args(["apply", "--cached"])
        .input(&patch)
        .run();
    assert_eq!(read(repo.join("file")), before);
    assert_eq!(real(&repo).args(["write-tree"]).run(), expected);
    safe(&repo, &state).args(["reset", "--hard", &base]).run();
    let hostile = patch
        .replace("a/file", "a/../outside")
        .replace("b/file", "b/../outside");
    assert!(
        safe(&repo, &state)
            .arg("apply")
            .input(hostile)
            .output()
            .code
            != Some(0)
    );
    assert_eq!(read(repo.join("file")), before);
    let outside = root.join("outside");
    write(&outside, "outside\n");
    symlink(&outside, repo.join("file-link")).unwrap();
    let bad = patch
        .replace("a/file", "a/file-link")
        .replace("b/file", "b/file-link");
    assert!(safe(&repo, &state).arg("apply").input(bad).output().code != Some(0));
    assert_eq!(read(&outside), "outside\n");
    fs::remove_file(repo.join("file-link")).unwrap();
    // A later failing hunk must not install an earlier successful change.
    let corrupt = patch.replace("-two", "-not-the-source");
    assert!(
        safe(&repo, &state)
            .arg("apply")
            .input(corrupt)
            .output()
            .code
            != Some(0)
    );
    assert_eq!(read(repo.join("file")), before);
    assert!(!repo.join("new file").exists());
    safe(&repo, &state)
        .args(["apply", "--index"])
        .input(&patch)
        .run();
    safe(&repo, &state)
        .args(["commit", "-m", "mail subject\n\nmail body"])
        .run();
    let commit = revision(&repo, "HEAD");
    let git_mail = real(&repo)
        .args(["format-patch", "--stdout", "-1"])
        .output()
        .stdout;
    let safe_mail = safe(&repo, &state)
        .args(["format-patch", "--stdout", "-1"])
        .output();
    assert_eq!(safe_mail.code, Some(0), "{safe_mail:?}");
    safe(&repo, &state).args(["reset", "--hard", &base]).run();
    safe(&repo, &state).arg("am").input(&git_mail).run();
    assert_eq!(revision(&repo, "HEAD"), commit);
    safe(&repo, &state).args(["reset", "--hard", &base]).run();
    real(&repo).arg("am").input(&safe_mail.stdout).run();
    assert_eq!(revision(&repo, "HEAD"), commit);
    safe(&repo, &state).args(["reset", "--hard", &base]).run();
    write(repo.join("file"), "conflict\n");
    safe(&repo, &state)
        .args(["commit", "-am", "conflict"])
        .run();
    let original = revision(&repo, "HEAD");
    assert!(safe(&repo, &state).arg("am").input(&git_mail).output().code != Some(0));
    safe(&repo, &state).args(["am", "--abort"]).run();
    assert_eq!(revision(&repo, "HEAD"), original);
    assert_eq!(read(repo.join("file")), "conflict\n");
    // Continue a failed mailbox only after the caller explicitly resolves and stages files.
    assert!(safe(&repo, &state).arg("am").input(&git_mail).output().code != Some(0));
    write(repo.join("file"), "one\nTWO\nthree\n");
    write(repo.join("new file"), "new\n");
    safe(&repo, &state).args(["add", "file", "new file"]).run();
    let before_continue = revision(&repo, "HEAD");
    write(repo.join(".git/index.lock"), "other operation");
    assert!(safe(&repo, &state).args(["am", "--continue"]).output().code != Some(0));
    assert_eq!(revision(&repo, "HEAD"), before_continue);
    fs::remove_file(repo.join(".git/index.lock")).unwrap();
    safe(&repo, &state).args(["am", "--continue"]).run();
    assert_eq!(revision(&repo, "HEAD^{tree}"), expected);
    // Git literal/delta binary patches round-trip in both directions.
    let binary_base = revision(&repo, "HEAD");
    write(
        repo.join("binary"),
        (0..8192).map(|n| (n % 251) as u8).collect::<Vec<_>>(),
    );
    safe(&repo, &state).args(["add", "binary"]).run();
    safe(&repo, &state)
        .args(["commit", "-m", "binary base"])
        .run();
    let binary_tip = revision(&repo, "HEAD");
    let mut bytes = fs::read(repo.join("binary")).unwrap();
    bytes[200..210].copy_from_slice(b"abcdefghij");
    write(repo.join("binary"), &bytes);
    safe(&repo, &state)
        .args(["commit", "-am", "binary change"])
        .run();
    let binary_tree = revision(&repo, "HEAD^{tree}");
    let binary_patch = real(&repo)
        .args(["diff", "--binary", &binary_tip, "HEAD"])
        .output()
        .stdout;
    let binary_mail = safe(&repo, &state)
        .args(["format-patch", "--stdout", &binary_base])
        .output();
    assert_eq!(binary_mail.code, Some(0), "{binary_mail:?}");
    safe(&repo, &state)
        .args(["reset", "--hard", &binary_tip])
        .run();
    safe(&repo, &state)
        .args(["apply", "--index"])
        .input(&binary_patch)
        .run();
    assert_eq!(real(&repo).args(["write-tree"]).run(), binary_tree);
    safe(&repo, &state)
        .args(["apply", "--index", "-R"])
        .input(&binary_patch)
        .run();
    assert_eq!(
        real(&repo).args(["write-tree"]).run(),
        revision(&repo, "HEAD^{tree}")
    );
    safe(&repo, &state)
        .args(["reset", "--hard", &binary_base])
        .run();
    real(&repo).arg("am").input(&binary_mail.stdout).run();
    assert_eq!(revision(&repo, "HEAD^{tree}"), binary_tree);
    safe(&repo, &state)
        .args(["reset", "--hard", &binary_base])
        .run();
    safe(&repo, &state)
        .arg("am")
        .input(&binary_mail.stdout)
        .run();
    assert_eq!(revision(&repo, "HEAD^{tree}"), binary_tree);
    passed(
        "atomic apply/check/reverse/cached, hostile paths, Git mailbox interoperability and am abort",
    );
}
pub fn rebase() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state, "repo");
    let base = revision(&repo, "HEAD");
    safe(&repo, &state).args(["switch", "-c", "topic"]).run();
    write(repo.join("one"), "one\n");
    safe(&repo, &state).args(["add", "one"]).run();
    safe(&repo, &state).args(["commit", "-m", "one"]).run();
    let one = revision(&repo, "HEAD");
    write(repo.join("two"), "two\n");
    safe(&repo, &state).args(["add", "two"]).run();
    safe(&repo, &state).args(["commit", "-m", "two"]).run();
    let two = revision(&repo, "HEAD");
    let plan = format!("reword {two} renamed two\npick {one}\ndone\n");
    safe(&repo, &state)
        .args(["rebase", "-i", &base])
        .input(plan)
        .run();
    assert_eq!(
        real(&repo).args(["log", "-2", "--format=%s"]).run(),
        "one\nrenamed two"
    );
    let original = revision(&repo, "HEAD");
    let marker = root.join("executed");
    let bad = format!("exec touch {}\ndone\n", marker.display());
    assert!(
        safe(&repo, &state)
            .args(["rebase", "-i", &base])
            .input(bad)
            .output()
            .code
            != Some(0)
    );
    assert_eq!(revision(&repo, "HEAD"), original);
    assert!(!marker.exists());
    let plan = format!("edit {original}\ndone\n");
    safe(&repo, &state)
        .args(["rebase", "-i", &base])
        .input(plan)
        .run();
    safe(&repo, &state)
        .args(["commit", "--amend", "-m", "edited"])
        .run();
    safe(&repo, &state).args(["rebase", "--continue"]).run();
    assert_eq!(
        real(&repo).args(["log", "-1", "--format=%s"]).run(),
        "edited"
    );
    safe(&repo, &state).args(["switch", "main"]).run();
    safe(&repo, &state).args(["switch", "-c", "left"]).run();
    write(repo.join("left"), "left\n");
    safe(&repo, &state).args(["add", "left"]).run();
    safe(&repo, &state).args(["commit", "-m", "left"]).run();
    let left = revision(&repo, "HEAD");
    safe(&repo, &state)
        .args(["switch", "-c", "right", &base])
        .run();
    write(repo.join("right"), "right\n");
    safe(&repo, &state).args(["add", "right"]).run();
    safe(&repo, &state).args(["commit", "-m", "right"]).run();
    let right = revision(&repo, "HEAD");
    safe(&repo, &state).args(["switch", "left"]).run();
    safe(&repo, &state)
        .args(["merge", "--no-ff", "right"])
        .run();
    let merged = revision(&repo, "HEAD");
    safe(&repo, &state).args(["switch", "main"]).run();
    write(repo.join("upstream"), "upstream\n");
    safe(&repo, &state).args(["add", "upstream"]).run();
    safe(&repo, &state).args(["commit", "-m", "upstream"]).run();
    let upstream = revision(&repo, "HEAD");
    safe(&repo, &state).args(["switch", "left"]).run();
    safe(&repo, &state)
        .args(["rebase", "--rebase-merges", "main"])
        .run();
    assert_eq!(
        real(&repo)
            .args(["rev-list", "--parents", "-1", "HEAD"])
            .run()
            .split_whitespace()
            .count(),
        3
    );
    assert_eq!(revision(&repo, "HEAD^1^"), upstream);
    assert_eq!(revision(&repo, "HEAD^2^"), upstream);
    assert!(
        repo.join("left").exists() && repo.join("right").exists() && repo.join("upstream").exists()
    );
    safe(&repo, &state).args(["reset", "--hard", &merged]).run();
    let plan = format!(
        "reset onto\npick {right}\nlabel side\nreset onto\nreword {left} changed left\nmerge -C {merged} side\ndone\n"
    );
    safe(&repo, &state)
        .args(["rebase", "-i", "--rebase-merges", "main"])
        .input(plan)
        .run();
    assert_eq!(
        real(&repo)
            .args(["log", "-1", "--format=%s", "HEAD^1"])
            .run(),
        "changed left"
    );
    assert_eq!(revision(&repo, "HEAD^2^"), upstream);
    let edits = seed(root, &state, "edits");
    let start = revision(&edits, "HEAD");
    let mut ids = Vec::new();
    for name in ["first", "second", "third"] {
        write(edits.join(name), name);
        safe(&edits, &state).args(["add", name]).run();
        safe(&edits, &state).args(["commit", "-m", name]).run();
        ids.push(revision(&edits, "HEAD"));
    }
    let plan = format!(
        "pick {}\nsquash {}\ndrop {}\ndone\n",
        ids[0], ids[1], ids[2]
    );
    safe(&edits, &state)
        .args(["rebase", "-i", &start])
        .input(plan)
        .run();
    assert_eq!(
        real(&edits)
            .args(["rev-list", "--count", &format!("{start}..HEAD")])
            .run(),
        "1"
    );
    assert_eq!(
        real(&edits).args(["log", "-1", "--format=%B"]).run(),
        "first\n\nsecond"
    );
    assert!(!edits.join("third").exists());
    safe(&edits, &state)
        .args(["reset", "--hard", &ids[2]])
        .run();
    let plan = format!(
        "pick {}\nfixup {}\nbreak\ndrop {}\ndone\n",
        ids[0], ids[1], ids[2]
    );
    safe(&edits, &state)
        .args(["rebase", "-i", &start])
        .input(plan)
        .run();
    safe(&edits, &state).args(["rebase", "--continue"]).run();
    assert_eq!(
        real(&edits).args(["log", "-1", "--format=%B"]).run(),
        "first"
    );
    // Autosquash also works when the plan retains branch structure.
    write(edits.join("first"), "amended");
    safe(&edits, &state).args(["add", "first"]).run();
    safe(&edits, &state).args(["commit", "--fixup=HEAD"]).run();
    safe(&edits, &state)
        .args(["rebase", "-i", "--rebase-merges", "--autosquash", &start])
        .input("done\n")
        .run();
    assert_eq!(read(edits.join("first")), "amended");
    assert_eq!(
        real(&edits)
            .args(["rev-list", "--count", &format!("{start}..HEAD")])
            .run(),
        "1"
    );
    let conflict = seed(root, &state, "conflict");
    let fork = revision(&conflict, "HEAD");
    write(conflict.join("file"), "ours\n");
    safe(&conflict, &state)
        .args(["commit", "-am", "ours"])
        .run();
    let ours = revision(&conflict, "HEAD");
    safe(&conflict, &state)
        .args(["switch", "-c", "up", &fork])
        .run();
    write(conflict.join("file"), "theirs\n");
    safe(&conflict, &state)
        .args(["commit", "-am", "theirs"])
        .run();
    safe(&conflict, &state).args(["switch", "main"]).run();
    assert!(
        safe(&conflict, &state)
            .args(["rebase", "-i", "up"])
            .input("done\n")
            .output()
            .code
            != Some(0)
    );
    safe(&conflict, &state).args(["rebase", "--abort"]).run();
    assert_eq!(revision(&conflict, "HEAD"), ours);
    assert!(
        safe(&conflict, &state)
            .args(["rebase", "-i", "up"])
            .input("done\n")
            .output()
            .code
            != Some(0)
    );
    write(conflict.join("file"), "resolved\n");
    safe(&conflict, &state).args(["add", "file"]).run();
    safe(&conflict, &state).args(["rebase", "--continue"]).run();
    assert_eq!(read(conflict.join("file")), "resolved\n");
    passed(
        "interactive reword/reorder/edit/continue, exec rejection and merge-preserving label/reset/merge plans",
    );
}
pub fn push() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let state = root.join("state");
    let seed = seed(root, &state, "seed-dry");
    let bare = root.join("source.git");
    real(root)
        .args(["clone", "--bare"])
        .arg(&seed)
        .arg(&bare)
        .run();
    let url = fixture.url(&bare);
    let work = root.join("work-dry");
    fixture.safe(root).arg("clone").arg(&url).arg(&work).run();
    write(work.join("new"), "new\n");
    safe(&work, &state).args(["add", "new"]).run();
    safe(&work, &state).args(["commit", "-m", "new"]).run();
    let before = revision(&bare, "HEAD");
    let marker = root.join("receive-hook-ran");
    write(
        bare.join("hooks/pre-receive"),
        format!("#!/bin/sh\ntouch {}\n", marker.display()),
    );
    chmod(bare.join("hooks/pre-receive"), 0o755);
    let tracking = revision(&work, "origin/main");
    fixture
        .safe(&work)
        .args(["push", "--dry-run", "-u", "origin", "main:newbranch"])
        .run();
    assert!(
        real(&bare)
            .args(["show-ref", "--verify", "refs/heads/newbranch"])
            .output()
            .code
            != Some(0)
    );
    assert_eq!(revision(&bare, "HEAD"), before);
    assert_eq!(revision(&work, "origin/main"), tracking);
    fixture
        .safe(&work)
        .args(["push", "--dry-run", "origin", "main"])
        .run();
    assert_eq!(revision(&bare, "HEAD"), before);
    fixture
        .safe(&work)
        .args(["push", "--dry-run", "--delete", "origin", "main"])
        .run();
    assert_eq!(revision(&bare, "HEAD"), before);
    assert!(!marker.exists());
    assert_eq!(
        fixture.safe(&work).args(["rev-parse", "HEAD@{u}"]).run(),
        tracking
    );
    passed(
        "push dry-run validates transport and intent without updating remote refs or local tracking",
    );
}
