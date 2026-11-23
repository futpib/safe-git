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
fn seed(root: &Path, state: &Path, name: &str, format: &str) -> PathBuf {
    let p = root.join(name);
    safe(root, state)
        .args(["init", "--object-format", format])
        .arg(&p)
        .run();
    write(p.join("file"), "one\ntwo\nthree\nfour\nfive\n");
    write(p.join("other"), "other\n");
    safe(&p, state).args(["add", "."]).run();
    safe(&p, state).args(["commit", "-m", "base"]).run();
    p
}
pub fn review() {
    review_format("sha1");
}
fn review_format(format: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", format);
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
    interactive_format("sha1");
}
fn interactive_format(format: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", format);
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
    rebase_format("sha1");
}
fn rebase_format(format: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", format);
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
    maintenance_signing_format("sha1");
}
fn maintenance_signing_format(format: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", format);
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
    indexes_format("sha1");
}
fn indexes_format(format: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", format);
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
    mail_patches_format("sha1");
}
fn mail_patches_format(format: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", format);
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
    submodules_format("sha1");
}
fn submodules_format(format: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let nested = seed(root, &state, "nested", format);
    let child = seed(root, &state, "child", format);
    let parent = seed(root, &state, "parent", format);
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

pub fn sha256_workflows() {
    review_format("sha256");
    interactive_format("sha256");
    rebase_format("sha256");
    maintenance_signing_format("sha256");
    indexes_format("sha256");
    mail_patches_format("sha256");
    submodules_format("sha256");
}
pub fn sha256_storage_transport() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let p = seed(root, &state, "repo", "sha256");
    let tip = revision(&p, "HEAD");
    assert_eq!(tip.len(), 64);
    assert_eq!(
        safe(&p, &state)
            .args(["rev-parse", "--show-object-format"])
            .run(),
        "sha256"
    );
    assert_eq!(safe(&p, &state).args(["rev-parse", "HEAD"]).run(), tip);
    assert_eq!(safe(&p, &state).args(["rev-parse", &tip[..12]]).run(), tip);
    // Identical commit metadata and content must hash exactly as Git does.
    real(&p).args(["commit", "--amend", "--no-edit"]).run();
    assert_eq!(revision(&p, "HEAD"), tip);
    write(p.join("other"), "changed\n");
    safe(&p, &state).args(["commit", "-am", "second"]).run();
    assert_eq!(safe(&p, &state).args(["rev-parse", "HEAD@{1}"]).run(), tip);
    write(p.join("other"), "dirty\n");
    safe(&p, &state).args(["stash", "push"]).run();
    safe(&p, &state).args(["stash", "pop"]).run();
    assert_eq!(read(p.join("other")), "dirty\n");
    safe(&p, &state).args(["restore", "other"]).run();
    write(p.join("intent"), "intent\n");
    safe(&p, &state).args(["add", "-N", "intent"]).run();
    assert_eq!(
        safe(&p, &state).args(["status", "--porcelain=v2"]).run(),
        real(&p).args(["status", "--porcelain=v2"]).run()
    );
    safe(&p, &state).args(["reset", "--hard", "HEAD"]).run();
    let remote = root.join("remote.git");
    safe(root, &state)
        .args(["init", "--bare", "--object-format=sha256"])
        .arg(&remote)
        .run();
    let empty = root.join("empty");
    safe(root, &state)
        .arg("clone")
        .arg(&remote)
        .arg(&empty)
        .run();
    assert_eq!(
        real(&empty)
            .args(["rev-parse", "--show-object-format"])
            .run(),
        "sha256"
    );
    safe(&p, &state).arg("push").arg(&remote).arg("main").run();
    assert_eq!(revision(&remote, "main"), revision(&p, "HEAD"));
    real(&remote)
        .args(["config", "http.receivepack", "true"])
        .run();
    let http = http::Http::new(root);
    let url = format!("{}/remote.git", http.url);
    let empty_remote = root.join("empty.git");
    safe(root, &state)
        .args(["init", "--bare", "--object-format=sha256"])
        .arg(&empty_remote)
        .run();
    let empty_smart = root.join("empty-smart");
    safe(root, &state)
        .arg("clone")
        .arg(format!("{}/empty.git", http.url))
        .arg(&empty_smart)
        .run();
    assert_eq!(
        real(&empty_smart)
            .args(["rev-parse", "--show-object-format"])
            .run(),
        "sha256"
    );
    let clone = root.join("clone");
    safe(root, &state).arg("clone").arg(&url).arg(&clone).run();
    assert_eq!(revision(&clone, "HEAD"), revision(&p, "HEAD"));
    write(clone.join("binary"), b"\0new binary\xff\n");
    safe(&clone, &state).args(["add", "binary"]).run();
    safe(&clone, &state).args(["commit", "-m", "binary"]).run();
    let patch = safe(&clone, &state)
        .args(["format-patch", "--stdout", "-1"])
        .run();
    safe(&p, &state).arg("am").input(patch).run();
    assert_eq!(revision(&clone, "HEAD^{tree}"), revision(&p, "HEAD^{tree}"));
    safe(&clone, &state).args(["push", "origin", "main"]).run();
    assert_eq!(revision(&clone, "HEAD"), revision(&remote, "main"));
    safe(&clone, &state).arg("fetch").run();
    assert_eq!(
        safe(&clone, &state).args(["rev-parse", "FETCH_HEAD"]).run(),
        revision(&clone, "HEAD")
    );
    safe(&clone, &state).args(["branch", "topic"]).run();
    safe(&clone, &state)
        .args(["push", "--atomic", "--all", "origin"])
        .run();
    safe(&clone, &state)
        .args(["push", "--delete", "origin", "topic"])
        .run();
    let shallow = root.join("shallow");
    safe(root, &state)
        .args(["clone", "--depth=1"])
        .arg(&url)
        .arg(&shallow)
        .run();
    safe(&shallow, &state).args(["fetch", "--unshallow"]).run();
    real(&shallow).args(["fsck", "--full"]).run();
    // Read Git's delta packs, then have Git verify packs written by safe-git.
    real(&clone).args(["gc", "--aggressive"]).run();
    safe(&clone, &state).arg("fsck").run();
    safe(&clone, &state).arg("gc").run();
    real(&clone).args(["fsck", "--full"]).run();
    real(&remote).args(["repack", "-ad"]).run();
    real(&remote).arg("prune-packed").run();
    real(&remote).arg("update-server-info").run();
    write(root.join(".static-http"), "");
    real(&empty_remote).arg("update-server-info").run();
    let empty_static = root.join("empty-static");
    safe(root, &state)
        .arg("clone")
        .arg(format!("{}/empty.git", http.url))
        .arg(&empty_static)
        .run();
    assert_eq!(
        real(&empty_static)
            .args(["rev-parse", "--show-object-format"])
            .run(),
        "sha256"
    );
    let static_clone = root.join("static");
    safe(root, &state)
        .arg("clone")
        .arg(&url)
        .arg(&static_clone)
        .run();
    real(&static_clone).args(["fsck", "--full"]).run();
    assert_eq!(revision(&static_clone, "HEAD"), revision(&remote, "HEAD"));
    let wrong = root.join("wrong.git");
    safe(root, &state)
        .args(["init", "--bare"])
        .arg(&wrong)
        .run();
    assert!(
        safe(&p, &state)
            .arg("push")
            .arg(&wrong)
            .arg("main")
            .fails()
            .contains("format")
    );
    assert!(real(&wrong).args(["for-each-ref"]).run().is_empty());
    write(
        p.join(".git/refs/heads/invalid"),
        "1111111111111111111111111111111111111111\n",
    );
    assert!(safe(&p, &state).arg("fsck").fails().contains("format"));
    passed(
        "SHA-256 matches Git for hashes, reflogs, stashes, packs, binary mail, shallow fetch and smart/static/local transfer; mixed formats fail closed",
    );
}

pub fn sha256_ssh_native() {
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let root = fixture.root();
    let state = root.join("state");
    let p = seed(root, &state, "seed", "sha256");
    let source = root.join("source.git");
    let dest = root.join("dest.git");
    for bare in [&source, &dest] {
        safe(root, &state)
            .args(["init", "--bare", "--object-format=sha256"])
            .arg(bare)
            .run();
    }
    safe(&p, &state).arg("push").arg(&source).arg("main").run();
    let clone = root.join("ssh");
    let trace = root.join("ssh.trace");
    fixture
        .safe(root)
        .arg("clone")
        .arg(fixture.url(&source))
        .arg(&clone)
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert_eq!(revision(&clone, "HEAD"), revision(&p, "HEAD"));
    fixture
        .safe(&clone)
        .arg("push")
        .arg(fixture.url(&dest))
        .arg("main")
        .run();
    fixture.safe(&clone).arg("fetch").run();
    assert_eq!(revision(&dest, "main"), revision(&p, "HEAD"));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let _server = TestCommand::new("git")
        .args([
            "daemon",
            "--listen=127.0.0.1",
            "--export-all",
            "--enable=receive-pack",
        ])
        .arg(format!("--port={port}"))
        .arg(format!("--base-path={}", root.display()))
        .arg(root)
        .spawn();
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let native = root.join("native");
    let url = format!("git://127.0.0.1:{port}/source.git");
    safe(root, &state).arg("clone").arg(&url).arg(&native).run();
    write(native.join("file"), "native sha256\n");
    safe(&native, &state)
        .args(["commit", "-am", "native"])
        .run();
    safe(&native, &state).args(["push", "origin", "main"]).run();
    safe(&native, &state).arg("fetch").run();
    assert_eq!(revision(&native, "HEAD"), revision(&source, "main"));
    real(&source).args(["fsck", "--full"]).run();
    passed("SHA-256 SSH and native Git clone/fetch/push interoperate without local helpers");
}
