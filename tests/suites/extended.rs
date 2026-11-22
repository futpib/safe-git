//! Regression coverage for the expanded developer workflow and its inert boundaries.
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
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
}
fn real(repo: &Path) -> TestCommand {
    git(repo)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .env("GIT_AUTHOR_DATE", "1234567890 +0000")
        .env("GIT_COMMITTER_DATE", "1234567890 +0000")
}
fn seed(root: &Path, state: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    safe(root, state)
        .args(["init", "-b", "main"])
        .arg(&repo)
        .run();
    write(repo.join("file"), "one\ntwo\nthree\n");
    safe(&repo, state).args(["add", "."]).run();
    safe(&repo, state).args(["commit", "-m", "base"]).run();
    repo
}
pub fn history() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state);
    let base = revision(&repo, "HEAD");
    safe(&repo, &state).args(["branch", "feature"]).run();
    assert!(safe(&repo, &state).arg("branch").run().contains("feature"));
    safe(&repo, &state).args(["switch", "feature"]).run();
    safe(&repo, &state).args(["switch", "-"]).run();
    assert_eq!(
        real(&repo).args(["symbolic-ref", "--short", "HEAD"]).run(),
        "main"
    );
    safe(&repo, &state)
        .args(["branch", "-m", "feature", "renamed"])
        .run();
    safe(&repo, &state).args(["branch", "-d", "renamed"]).run();
    fs::create_dir(repo.join("sub")).unwrap();
    write(repo.join("sub/a.txt"), "a\n");
    write(repo.join("sub/b.txt"), "b\n");
    write(repo.join("sub/c.rs"), "c\n");
    safe(&repo.join("sub"), &state).args(["add", "*.txt"]).run();
    assert_eq!(
        real(&repo).args(["diff", "--cached", "--name-only"]).run(),
        "sub/a.txt\nsub/b.txt"
    );
    safe(&repo.join("sub"), &state)
        .args(["restore", "--staged", "a.txt"])
        .run();
    safe(&repo, &state).args(["commit", "-m", "second"]).run();
    let second = revision(&repo, "HEAD");
    safe(&repo, &state)
        .args(["commit", "--amend", "-m", "replacement"])
        .run();
    assert_eq!(revision(&repo, "HEAD^"), base);
    assert_eq!(
        safe(&repo, &state).args(["rev-parse", "HEAD@{1}"]).run(),
        second
    );
    assert!(safe(&repo, &state).arg("reflog").run().contains("amend"));
    safe(&repo, &state)
        .args(["commit", "--amend", "--no-edit"])
        .run();
    safe(&repo, &state).args(["reset", "--soft", "HEAD^"]).run();
    assert!(
        !real(&repo)
            .args(["diff", "--cached", "--name-only"])
            .run()
            .is_empty()
    );
    safe(&repo, &state).args(["reset", "--mixed"]).run();
    assert!(
        real(&repo)
            .args(["diff", "--cached", "--name-only"])
            .run()
            .is_empty()
    );
    assert!(repo.join("sub/b.txt").exists());
    // Hunk selection retains an unselected second hunk, without invoking an editor.
    let original = (1..=30).map(|i| format!("line {i}\n")).collect::<String>();
    write(repo.join("file"), &original);
    safe(&repo, &state).args(["commit", "-am", "long"]).run();
    let edited = original
        .replace("line 2\n", "EDIT TWO\n")
        .replace("line 29\n", "EDIT TWENTY NINE\n");
    write(repo.join("file"), &edited);
    safe(&repo, &state)
        .args(["add", "-p", "file"])
        .input("y\nn\n")
        .run();
    let staged = real(&repo).args(["show", ":file"]).run();
    assert!(staged.contains("EDIT TWO"));
    assert!(!staged.contains("EDIT TWENTY NINE"));
    safe(&repo, &state)
        .args(["restore", "--staged", "-p", "file"])
        .input("y\n")
        .run();
    assert_eq!(
        real(&repo).args(["show", ":file"]).run(),
        original.trim_end()
    );
    safe(&repo, &state)
        .args(["restore", "-p", "file"])
        .input("n\ny\n")
        .run();
    assert!(read(repo.join("file")).contains("EDIT TWO"));
    assert!(!read(repo.join("file")).contains("EDIT TWENTY NINE"));
    // A repository-selected executable must remain data even for new commands.
    let marker = root.join("EXECUTED");
    let probe = root.join("probe");
    write(&probe, format!("#!/bin/sh\ntouch {}\n", quote(&marker)));
    chmod(&probe, 0o700);
    for key in [
        "core.editor",
        "sequence.editor",
        "core.hooksPath",
        "core.fsmonitor",
        "filter.probe.clean",
    ] {
        real(&repo).args(["config", key]).arg(&probe).run();
    }
    write(repo.join(".gitattributes"), "* filter=probe\n");
    safe(&repo, &state).args(["add", "file"]).run();
    safe(&repo, &state).args(["commit", "-m", "inert"]).run();
    assert!(!marker.exists());
    let outside = root.join("outside");
    write(&outside, "untouched\n");
    symlink(&outside, repo.join(".git/logs/refs/heads/evil")).unwrap();
    safe(&repo, &state).args(["branch", "evil"]).fails();
    assert_eq!(read(&outside), "untouched\n");
    assert!(
        real(&repo)
            .args(["rev-parse", "--verify", "evil"])
            .output()
            .code
            != Some(0)
    );
    passed(
        "init, branch management, reflog, amend/reset, relative/glob paths and selected hunks; hostile helpers and log links stay inert",
    );
}
pub fn stash_status() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state);
    write(repo.join(".gitignore"), "ignored\n");
    safe(&repo, &state).args(["add", "."]).run();
    safe(&repo, &state).args(["commit", "-m", "ignore"]).run();
    write(repo.join("new"), "untracked\n");
    write(repo.join("ignored"), "ignored\n");
    write(repo.join("file"), "tracked change\n");
    safe(&repo, &state)
        .args(["stash", "push", "-u", "-m", "first"])
        .run();
    assert!(!repo.join("new").exists());
    assert!(repo.join("ignored").exists());
    safe(&repo, &state)
        .args(["stash", "push", "-a", "-m", "second"])
        .run();
    assert!(!repo.join("ignored").exists());
    assert!(
        safe(&repo, &state)
            .args(["stash", "show", "-p", "stash@{1}"])
            .run()
            .contains("tracked change")
    );
    safe(&repo, &state)
        .args(["stash", "apply", "stash@{1}"])
        .run();
    assert_eq!(read(repo.join("new")), "untracked\n");
    safe(&repo, &state)
        .args(["stash", "drop", "stash@{1}"])
        .run();
    assert_eq!(
        safe(&repo, &state)
            .args(["stash", "list"])
            .run()
            .lines()
            .count(),
        1
    );
    safe(&repo, &state).args(["stash", "pop"]).run();
    assert_eq!(read(repo.join("ignored")), "ignored\n");
    safe(&repo, &state).args(["stash", "-u"]).run();
    real(&repo).args(["stash", "pop"]).run(); // Git understands our third parent.
    real(&repo).args(["stash", "push", "-u"]).run();
    safe(&repo, &state).args(["stash", "pop"]).run();
    safe(&repo, &state).args(["stash", "-u"]).run();
    safe(&repo, &state).args(["stash", "clear"]).run();
    assert!(safe(&repo, &state).args(["stash", "list"]).run().is_empty());
    fs::rename(repo.join("file"), repo.join("renamed")).unwrap();
    safe(&repo, &state).args(["add", "-A"]).run();
    for args in [["status", "--porcelain=v2"], ["status", "--porcelain=v1"]] {
        assert_eq!(
            safe(&repo, &state).args(args).run(),
            real(&repo).args(args).run()
        );
    }
    assert!(
        safe(&repo, &state)
            .args(["status", "--short"])
            .run()
            .contains("R  file -> renamed")
    );
    passed(
        "untracked/ignored stash, arbitrary selection/drop/show/clear, cross-client restore, rename detection and porcelain v2",
    );
}
pub fn sequences() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state);
    safe(&repo, &state).args(["switch", "-c", "topic"]).run();
    write(repo.join("file"), "topic\n");
    safe(&repo, &state).args(["commit", "-am", "topic"]).run();
    let topic = revision(&repo, "HEAD");
    safe(&repo, &state).args(["switch", "main"]).run();
    write(repo.join("file"), "main\n");
    safe(&repo, &state).args(["commit", "-am", "main"]).run();
    let main = revision(&repo, "HEAD");
    safe(&repo, &state).args(["merge", "topic"]).fails();
    assert!(
        safe(&repo, &state)
            .args(["status", "--short"])
            .run()
            .contains("UU file")
    );
    assert_eq!(
        real(&repo).args(["ls-files", "-u"]).run().lines().count(),
        3
    );
    for args in [["status", "--porcelain=v2"], ["status", "--porcelain=v1"]] {
        assert_eq!(
            safe(&repo, &state).args(args).run(),
            real(&repo).args(args).run()
        );
    }
    let saved_state = fs::read(repo.join(".git/safe-git-sequence")).unwrap();
    let mut corrupted: serde_json::Value = serde_json::from_slice(&saved_state).unwrap();
    corrupted["steps"][0]["action"] = serde_json::json!("exec");
    fs::write(
        repo.join(".git/safe-git-sequence"),
        serde_json::to_vec(&corrupted).unwrap(),
    )
    .unwrap();
    assert!(
        safe(&repo, &state)
            .args(["merge", "--continue"])
            .fails()
            .contains("never executed")
    );
    fs::write(repo.join(".git/safe-git-sequence"), &saved_state).unwrap();
    safe(&repo, &state)
        .args(["add", "-p", "file"])
        .input("y\n")
        .fails();
    safe(&repo, &state).args(["merge", "--abort"]).run();
    assert_eq!(revision(&repo, "HEAD"), main);
    assert_eq!(read(repo.join("file")), "main\n");
    safe(&repo, &state).args(["merge", "topic"]).fails();
    write(repo.join("file"), "resolved\n");
    safe(&repo, &state).args(["add", "file"]).run();
    safe(&repo, &state).args(["merge", "--continue"]).run();
    assert_eq!(revision(&repo, "HEAD^2"), topic);
    safe(&repo, &state)
        .args(["switch", "-c", "pick", &main])
        .run();
    safe(&repo, &state).args(["cherry-pick", &topic]).fails();
    write(repo.join("file"), "picked\n");
    safe(&repo, &state).args(["add", "file"]).run();
    safe(&repo, &state)
        .args(["cherry-pick", "--continue"])
        .run();
    let picked = revision(&repo, "HEAD");
    safe(&repo, &state).args(["revert", &picked]).run();
    assert_eq!(read(repo.join("file")), "main\n");
    safe(&repo, &state).args(["switch", "topic"]).run();
    safe(&repo, &state).args(["rebase", &main]).fails();
    safe(&repo, &state).args(["rebase", "--abort"]).run();
    assert_eq!(revision(&repo, "HEAD"), topic);
    safe(&repo, &state)
        .args(["switch", "-c", "linear", &main])
        .run();
    write(repo.join("new"), "one\n");
    safe(&repo, &state).args(["add", "new"]).run();
    safe(&repo, &state).args(["commit", "-m", "one"]).run();
    let one = revision(&repo, "HEAD");
    write(repo.join("new"), "two\n");
    safe(&repo, &state).args(["commit", "-am", "two"]).run();
    let two = revision(&repo, "HEAD");
    safe(&repo, &state)
        .args(["rebase", "-i", &main])
        .input(format!("reword {one} new message\nsquash {two}\ndone\n"))
        .run();
    assert_eq!(revision(&repo, "HEAD^"), main);
    assert!(
        real(&repo)
            .args(["log", "-1", "--format=%B"])
            .run()
            .contains("new message")
    );
    assert_eq!(read(repo.join("new")), "two\n");
    safe(&repo, &state)
        .args(["rebase", "-i", &main])
        .input("exec touch /tmp/unsafe\ndone\n")
        .fails();
    assert!(!repo.join(".git/safe-git-sequence").exists());
    let latest = revision(&repo, "HEAD");
    safe(&repo, &state)
        .args(["rebase", "-i", &main])
        .input(format!("edit {latest}\ndone\n"))
        .run();
    safe(&repo, &state)
        .args(["commit", "--amend", "-m", "edited"])
        .run();
    safe(&repo, &state).args(["rebase", "--continue"]).run();
    assert!(!repo.join(".git/safe-git-sequence").exists());
    passed(
        "merge/cherry-pick/revert conflicts, standard index stages, abort/continue, and inert interactive rebase plans",
    );
}
pub fn http_push() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state);
    let remote = root.join("remote.git");
    real(root).args(["init", "--bare"]).arg(&remote).run();
    real(&remote)
        .args(["config", "http.receivepack", "true"])
        .run();
    let server = crate::support::http::Http::new(root);
    let url = format!("{}/remote.git", server.url);
    safe(&repo, &state)
        .arg("--url")
        .arg(&url)
        .args(["push", "-u", "origin", "main"])
        .run();
    let first = revision(&repo, "HEAD");
    assert_eq!(revision(&remote, "main"), first);
    assert_eq!(
        safe(&repo, &state)
            .args(["status", "--porcelain=v2", "-b"])
            .run(),
        format!(
            "# branch.oid {first}\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0"
        )
    );
    write(repo.join("file"), "second\n");
    safe(&repo, &state).args(["commit", "-am", "second"]).run();
    let second = revision(&repo, "HEAD");
    safe(&repo, &state).arg("push").run();
    safe(&repo, &state).args(["reset", "--hard", &first]).run();
    safe(&repo, &state).arg("push").fails();
    safe(&repo, &state)
        .args(["push", "--force-with-lease"])
        .run();
    assert_eq!(revision(&remote, "main"), first);
    safe(&repo, &state)
        .arg("push")
        .arg(format!("--force-with-lease=main:{second}"))
        .fails();
    real(&repo).args(["tag", "v1"]).run();
    safe(&repo, &state).args(["push", "--tags"]).run();
    assert_eq!(revision(&remote, "v1"), first);
    safe(&repo, &state)
        .args(["push", "--delete", "origin", "refs/tags/v1"])
        .run();
    assert!(
        real(&remote)
            .args(["rev-parse", "--verify", "v1"])
            .output()
            .code
            != Some(0)
    );
    safe(&repo, &state).args(["branch", "other"]).run();
    safe(&repo, &state).args(["push", "origin", "other"]).run();
    safe(&repo, &state)
        .args(["push", "--delete", "origin", "other"])
        .run();
    // HTTPS uses the normal trust store. An explicit token never travels over cleartext HTTP.
    safe(&repo, &state)
        .env("SAFE_GIT_HTTP_TOKEN", "secret")
        .arg("push")
        .fails();
    let (tls, cert) = crate::support::http::Http::tls(root);
    let certfile = root.join("ca.pem");
    write(&certfile, cert);
    safe(&repo, &state)
        .arg("--url")
        .arg(format!("{}/remote.git", tls.url))
        .env("SSL_CERT_FILE", &certfile)
        .args(["push", "--force", "origin", "main"])
        .run();
    passed(
        "HTTP and HTTPS push, leases, tag and branch deletion, tracking counts and cleartext credential refusal",
    );
}

pub fn ssh_push() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let state = root.join("state");
    let repo = seed(root, &state);
    let remote = root.join("source.git");
    real(root).args(["init", "--bare"]).arg(&remote).run();
    let url = fixture.url(&remote);
    fixture.safe(&repo).args(["push", "-u", &url, "main"]).run();
    let first = revision(&repo, "HEAD");
    write(repo.join("file"), "second\n");
    safe(&repo, &state).args(["commit", "-am", "second"]).run();
    fixture.safe(&repo).arg("push").run();
    fixture.safe(&repo).args(["reset", "--hard", &first]).run();
    fixture.safe(&repo).args(["push", "--force"]).run();
    assert_eq!(revision(&remote, "main"), first);
    real(&repo).args(["tag", "-a", "-m", "release", "v1"]).run();
    fixture.safe(&repo).args(["push", "--tags"]).run();
    assert_eq!(revision(&remote, "v1"), revision(&repo, "v1"));
    fixture
        .safe(&repo)
        .args(["push", "origin", "HEAD:temporary"])
        .run();
    fixture
        .safe(&repo)
        .args(["push", "--delete", "origin", "temporary"])
        .run();
    fixture
        .safe(&repo)
        .args(["push", "--delete", "origin", "refs/tags/v1"])
        .run();
    real(&remote).args(["fsck", "--full"]).run();
    // A fetched remote branch can be selected automatically, but only its host registration supplies the endpoint.
    real(&remote).args(["branch", "remote-topic", "main"]).run();
    fixture.safe(&repo).arg("fetch").run();
    fixture.safe(&repo).args(["switch", "remote-topic"]).run();
    assert_eq!(
        real(&repo).args(["symbolic-ref", "--short", "HEAD"]).run(),
        "remote-topic"
    );
    fixture.safe(&repo).args(["switch", "main"]).run();
    fixture
        .safe(&repo)
        .args(["switch", "-c", "tracked", "--track", "origin/main"])
        .run();
    assert_eq!(
        fixture.safe(&repo).args(["rev-parse", "HEAD@{u}"]).run(),
        first
    );
    passed(
        "SSH force/delete, annotated-tag pushes, explicit HEAD refspecs and trusted automatic tracking",
    );
}
