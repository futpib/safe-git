//! Ordinary Git compatibility plus hostile metadata checks for the remaining gaps.
use crate::support::*;
use base64::Engine;
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
    write(p.join("file"), "one\ntwo\nthree\nfour\nfive\n");
    write(p.join("other"), "other\n");
    safe(&p, state).args(["add", "."]).run();
    safe(&p, state).args(["commit", "-m", "base"]).run();
    p
}
pub fn review() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo");
    write(p.join("file"), "one  \n  two\nthree\nfour\nfive\n");
    assert!(
        safe(&p, &state)
            .args(["diff", "-w", "--exit-code"])
            .run()
            .is_empty()
    );
    write(p.join("file"), "one\nchanged\nthree\nfour\nfive\n");
    let out = safe(&p, &state).args(["diff", "--word-diff"]).run();
    assert!(out.contains("[-two-]{+changed+}"), "{out}");
    safe(&p, &state).args(["restore", "file"]).run();
    fs::rename(p.join("file"), p.join("renamed")).unwrap();
    safe(&p, &state).args(["add", "-A"]).run();
    assert_eq!(
        safe(&p, &state)
            .args(["diff", "--staged", "-M", "--name-status"])
            .run(),
        real(&p)
            .args(["diff", "--staged", "-M", "--name-status"])
            .run()
    );
    safe(&p, &state).args(["commit", "-m", "rename"]).run();
    assert_eq!(
        safe(&p, &state)
            .args(["log", "--follow", "--format=%s", "--", "renamed"])
            .run(),
        real(&p)
            .args(["log", "--follow", "--format=%s", "--", "renamed"])
            .run()
    );
    assert!(
        safe(&p, &state)
            .args(["log", "--since=2030-01-01"])
            .run()
            .is_empty()
    );
    assert!(
        safe(&p, &state)
            .args(["log", "-p", "-1"])
            .run()
            .contains("diff --git")
    );
    passed("whitespace/word/rename diffs and patch/follow/date history operate in process");
}
pub fn interactive() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo");
    write(p.join("file"), "ONE\ntwo\nthree\nfour\nFIVE\n");
    safe(&p, &state)
        .args(["add", "-p", "file"])
        .input("s\ny\nn\n")
        .run();
    assert_eq!(
        real(&p).args(["show", ":file"]).run(),
        "ONE\ntwo\nthree\nfour\nfive"
    );
    safe(&p, &state).args(["restore", "--staged", "file"]).run();
    safe(&p, &state)
        .args(["add", "-p", "file"])
        .input("e\n-one\n+CUSTOM\n two\n three\n four\n five\n.\n")
        .run();
    assert_eq!(
        real(&p).args(["show", ":file"]).run(),
        "CUSTOM\ntwo\nthree\nfour\nfive"
    );
    safe(&p, &state).args(["restore", "--staged", "file"]).run();
    safe(&p, &state)
        .args(["add", "-i"])
        .input("u\n1\nq\n")
        .run();
    assert_eq!(
        real(&p).args(["show", ":file"]).run(),
        "ONE\ntwo\nthree\nfour\nFIVE"
    );
    passed("interactive file selection and split/edited hunks preserve unselected edits");
}
pub fn rebase() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo");
    safe(&p, &state).args(["switch", "-c", "topic"]).run();
    write(p.join("added"), "topic\n");
    safe(&p, &state).args(["add", "added"]).run();
    safe(&p, &state).args(["commit", "-m", "topic"]).run();
    safe(&p, &state).args(["switch", "main"]).run();
    write(p.join("upstream"), "up\n");
    safe(&p, &state).args(["add", "upstream"]).run();
    safe(&p, &state).args(["commit", "-m", "upstream"]).run();
    safe(&p, &state).args(["switch", "topic"]).run();
    write(p.join("other"), "dirty\n");
    safe(&p, &state)
        .args(["rebase", "--autostash", "main"])
        .run();
    assert_eq!(read(p.join("other")), "dirty\n");
    assert!(safe(&p, &state).args(["stash", "list"]).run().is_empty());
    safe(&p, &state).args(["restore", "other"]).run();
    let tree = real(&p).args(["rev-parse", "HEAD^{tree}"]).run();
    let count = real(&p).args(["rev-list", "--count", "HEAD"]).run();
    safe(&p, &state)
        .args(["rebase", "-i", "--root"])
        .input("done\n")
        .run();
    assert_eq!(real(&p).args(["rev-parse", "HEAD^{tree}"]).run(), tree);
    assert_eq!(real(&p).args(["rev-list", "--count", "HEAD"]).run(), count);
    real(&p).args(["fsck", "--full"]).run();
    passed(
        "autostash restores edits and root rebase preserves the real root without synthetic parents",
    );
}
pub fn maintenance_signing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo");
    let before = revision(&p, "HEAD");
    safe(&p, &state).arg("fsck").run();
    safe(&p, &state).arg("gc").run();
    assert_eq!(revision(&p, "HEAD"), before);
    real(&p).args(["fsck", "--full"]).run();
    assert!(
        real(&p)
            .args(["count-objects", "-v"])
            .run()
            .starts_with("count: 0")
    );
    let key = root.join("signing");
    TestCommand::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .run();
    let allowed = root.join("allowed");
    write(
        &allowed,
        format!("test@example.invalid {}", read(key.with_extension("pub"))),
    );
    real(&p)
        .args(["config", "gpg.program", "/does/not/exist"])
        .run();
    real(&p)
        .args(["config", "user.signingKey", "/does/not/exist"])
        .run();
    write(p.join("other"), "signed\n");
    safe(&p, &state).args(["add", "other"]).run();
    safe(&p, &state)
        .arg("--signing-key")
        .arg(&key)
        .args(["commit", "-S", "-m", "signed"])
        .run();
    real(&p)
        .args(["-c", "gpg.format=ssh", "-c"])
        .arg(format!("gpg.ssh.allowedSignersFile={}", allowed.display()))
        .args(["verify-commit", "HEAD"])
        .run();
    let blob = real(&p).args(["rev-parse", "HEAD:other"]).run();
    write(
        p.join(".git/objects").join(&blob[..2]).join(&blob[2..]),
        "corrupted",
    );
    safe(&p, &state).arg("fsck").fails();
    passed(
        "conservative packing preserves objects, fsck detects corruption, and Git verifies caller-selected SSH signatures",
    );
}
pub fn indexes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo");
    real(&p).args(["update-index", "--split-index"]).run();
    write(p.join("file"), "changed\n");
    real(&p).args(["add", "file"]).run();
    assert_eq!(
        safe(&p, &state).args(["diff", "--staged"]).run(),
        real(&p).args(["diff", "--staged"]).run()
    );
    safe(&p, &state).args(["commit", "-m", "split"]).run();
    real(&p).args(["fsck", "--full"]).run();
    for name in ["visible/a", "hidden/b"] {
        fs::create_dir_all(p.join(name).parent().unwrap()).unwrap();
        write(p.join(name), "data\n");
    }
    safe(&p, &state).args(["add", "."]).run();
    safe(&p, &state).args(["commit", "-m", "dirs"]).run();
    real(&p).args(["update-index", "--no-split-index"]).run();
    real(&p).args(["update-index", "--refresh"]).run();
    real(&p)
        .args(["sparse-checkout", "init", "--cone", "--sparse-index"])
        .run();
    real(&p).args(["sparse-checkout", "set", "visible"]).run();
    assert!(
        !p.join("hidden/b").exists(),
        "hidden file exists immediately after Git sparse setup: {}",
        read(p.join(".git/config"))
    );
    assert!(
        safe(&p, &state)
            .args(["status", "--porcelain"])
            .run()
            .is_empty()
    );
    write(p.join("visible/a"), "edited\n");
    safe(&p, &state).args(["add", "visible"]).run();
    safe(&p, &state).args(["commit", "-m", "sparse"]).run();
    assert_eq!(real(&p).args(["show", "HEAD:hidden/b"]).run(), "data");
    assert!(!p.join("hidden/b").exists());
    real(&p).args(["sparse-checkout", "disable"]).run();
    real(&p).args(["update-index", "--split-index"]).run();
    let shared = fs::read_dir(p.join(".git"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("sharedindex.")
        })
        .unwrap();
    let external = root.join("external-index");
    fs::copy(&shared, &external).unwrap();
    fs::remove_file(&shared).unwrap();
    symlink(&external, &shared).unwrap();
    safe(&p, &state).arg("status").fails();
    passed("split and sparse indexes expand safely; shared index symlinks are refused");
}
pub fn mail_patches() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo");
    let base = revision(&p, "HEAD");
    write(p.join("other"), "encoded\n");
    real(&p).args(["commit", "-am", "encoded subject"]).run();
    let mail = real(&p).args(["format-patch", "-1", "--stdout"]).run();
    let (headers, body) = mail.split_once("\n\n").unwrap();
    let headers = headers.replace(
        "Subject: [PATCH] encoded subject",
        "Subject: =?UTF-8?B?W1BBVENIXSBlbmNvZGVkIHN1YmplY3Q=?=",
    );
    let encoded = format!(
        "{headers}\nMIME-Version: 1.0\nContent-Type: text/plain; charset=UTF-8\nContent-Transfer-Encoding: base64\n\n{}\n",
        base64::engine::general_purpose::STANDARD.encode(body)
    );
    real(&p).args(["reset", "--hard", &base]).run();
    safe(&p, &state).arg("am").input(encoded).run();
    assert_eq!(read(p.join("other")), "encoded\n");
    let copy =
        "diff --git a/file b/copied\nsimilarity index 100%\ncopy from file\ncopy to copied\n";
    safe(&p, &state)
        .args(["apply", "--index"])
        .input(copy)
        .run();
    assert_eq!(read(p.join("file")), read(p.join("copied")));
    safe(&p, &state)
        .args(["apply", "--index", "-R"])
        .input(copy)
        .run();
    assert!(!p.join("copied").exists());
    let patch = format!(
        "diff --git a/module b/module\nnew file mode 160000\nindex 0000000..{}\n--- /dev/null\n+++ b/module\n@@ -0,0 +1 @@\n+Subproject commit {base}\n",
        &base[..7]
    );
    safe(&p, &state)
        .args(["apply", "--cached"])
        .input(patch)
        .run();
    assert!(
        real(&p)
            .args(["ls-files", "--stage", "module"])
            .run()
            .starts_with("160000")
    );
    passed("encoded mail, reversible copies and inert gitlink patches interoperate with Git");
}
pub fn submodules() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let nested = seed(root, &state, "nested");
    let child = seed(root, &state, "child");
    let parent = seed(root, &state, "parent");
    safe(&child, &state)
        .args(["submodule", "add"])
        .arg(&nested)
        .arg("nested")
        .run();
    safe(&child, &state)
        .args(["commit", "-m", "nested module"])
        .run();
    safe(&parent, &state)
        .args(["submodule", "add"])
        .arg(&child)
        .arg("child")
        .run();
    safe(&parent, &state).args(["commit", "-m", "module"]).run();
    assert!(
        safe(&parent, &state)
            .args(["submodule", "update", "--init", "--recursive"])
            .fails()
            .contains("untrusted")
    );
    safe(&parent.join("child"), &state)
        .args(["submodule", "init", "--url"])
        .arg(&nested)
        .args(["--", "nested"])
        .run();
    write(
        parent.join("child/.gitmodules"),
        "[submodule \"nested\"]\npath = nested\nurl = ssh://evil.invalid/repo\nupdate = !touch forbidden\n",
    );
    // Discard only the fixture's poisoned working file so checkout stays clean; the hostile
    // endpoint is also stored in config to prove registrations, not config, select it.
    safe(&parent.join("child"), &state)
        .args(["restore", ".gitmodules"])
        .run();
    real(&parent.join("child"))
        .args(["config", "submodule.nested.url", "ssh://evil.invalid/repo"])
        .run();
    safe(&parent, &state)
        .args(["submodule", "update", "--init", "--recursive"])
        .run();
    assert!(parent.join("child/nested/file").is_file());
    passed("recursive updates require a trusted endpoint for every nested submodule");
}
