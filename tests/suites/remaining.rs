//! Git interoperability for selective editing and bounded large-repository workflows.
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
    for name in ["file", "other"] {
        write(
            p.join(name),
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
        );
    }
    safe(&p, state).args(["add", "."]).run();
    safe(&p, state).args(["commit", "-m", "base"]).run();
    p
}
fn equal(a: &Path, b: &Path, state: &Path) {
    for args in [
        vec!["status", "--porcelain", "-uall"],
        vec!["status", "--porcelain=v2", "-uall"],
        vec!["diff"],
        vec!["diff", "--staged"],
    ] {
        let got = if args[0] == "diff" {
            real(a).args(&args).output()
        } else {
            safe(a, state).args(&args).output()
        };
        let want = real(b).args(&args).output();
        assert_eq!(got.code, Some(0), "{args:?}: {got:?}");
        assert_eq!(got.stdout, want.stdout, "{args:?}");
    }
}
pub fn editing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let state = root.join("state");
    let a = seed(root, &state, "a");
    let b = root.join("b");
    real(root).arg("clone").arg(&a).arg(&b).run();
    for repo in [&a, &b] {
        write(repo.join("new"), "new content\n");
    }
    safe(&a, &state).args(["add", "-N", "new"]).run();
    real(&b).args(["add", "-N", "new"]).run();
    equal(&a, &b, &state);
    assert!(
        real(&a)
            .args(["ls-files", "--debug", "new"])
            .run()
            .contains("flags: 20004000")
    );
    for args in [
        vec![
            "commit",
            "--allow-empty",
            "--author=Another <a@example.invalid>",
            "-m",
            "empty",
        ],
        vec!["add", "new"],
        vec!["commit", "-m", "added"],
    ] {
        safe(&a, &state).args(&args).run();
        real(&b).args(&args).run();
        assert_eq!(revision(&a, "HEAD"), revision(&b, "HEAD"));
        equal(&a, &b, &state);
    }
    for repo in [&a, &b] {
        write(repo.join("file"), "selected\n");
        write(repo.join("other"), "staged outside\n");
    }
    safe(&a, &state).args(["add", "other"]).run();
    real(&b).args(["add", "other"]).run();
    for args in [
        vec!["commit", "-m", "selected path", "--", "file"],
        vec!["reset", "--", "other"],
        vec!["checkout", "--", "other"],
    ] {
        safe(&a, &state).args(&args).run();
        real(&b).args(&args).run();
        equal(&a, &b, &state);
        assert_eq!(revision(&a, "HEAD"), revision(&b, "HEAD"));
    }
    assert!(
        safe(&a, &state)
            .args([
                "commit",
                "--allow-empty",
                "--author=bad\nidentity",
                "-m",
                "bad"
            ])
            .fails()
            .contains("author")
    );
    passed("intent-to-add, empty/author/path commits and legacy file restore match Git");
}
pub fn stashing() {
    for (n, args) in [
        vec!["stash", "push", "--keep-index"],
        vec!["stash", "push", "--staged"],
        vec!["stash", "push", "--", "file"],
        vec!["stash", "push", "-p"],
    ]
    .iter()
    .enumerate()
    {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let state = root.join("state");
        let a = seed(root, &state, "a");
        let b = root.join("b");
        real(root).arg("clone").arg(&a).arg(&b).run();
        for repo in [&a, &b] {
            write(
                repo.join("file"),
                "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            );
        }
        safe(&a, &state).args(["add", "file"]).run();
        real(&b).args(["add", "file"]).run();
        for repo in [&a, &b] {
            write(
                repo.join("file"),
                "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\n",
            );
            write(repo.join("other"), "unstaged outside\n");
        }
        safe(&a, &state).args(args).input("y\nn\nn\n").run();
        real(&b).args(args).input("y\nn\nn\n").run();
        equal(&a, &b, &state);
        assert_eq!(
            revision(&a, "refs/stash^{tree}"),
            revision(&b, "refs/stash^{tree}"),
            "stash case {n}"
        );
        assert_eq!(
            revision(&a, "refs/stash^2^{tree}"),
            revision(&b, "refs/stash^2^{tree}")
        );
        // Git can consume the created stash, including its separate index parent.
        real(&a).args(["stash", "show", "-p"]).run();
    }
    passed("selective, staged, keep-index and patch stashes preserve unrelated changes");
}
pub fn sparse() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let state = root.join("state");
    let a = seed(root, &state, "sparse");
    for (path, text) in [
        ("src/lib/file", "source\n"),
        ("src/sibling", "sibling\n"),
        ("docs/file", "docs\n"),
    ] {
        fs::create_dir_all(a.join(path).parent().unwrap()).unwrap();
        write(a.join(path), text);
    }
    safe(&a, &state).args(["add", "."]).run();
    safe(&a, &state).args(["commit", "-m", "dirs"]).run();
    let b = root.join("reference");
    real(root).arg("clone").arg(&a).arg(&b).run();
    for args in [vec!["sparse-checkout", "set", "src/lib"], vec!["add", "-A"]] {
        safe(&a, &state).args(&args).run();
        real(&b).args(&args).run();
        equal(&a, &b, &state);
        assert_eq!(
            real(&a).args(["ls-files", "-t"]).run(),
            real(&b).args(["ls-files", "-t"]).run()
        );
    }
    assert!(!a.join("docs/file").exists());
    assert!(a.join("src/sibling").exists());
    assert_eq!(
        safe(&a, &state).args(["sparse-checkout", "list"]).run(),
        "src/lib"
    );
    write(a.join("file"), "sparse stash\n");
    safe(&a, &state).args(["stash"]).run();
    assert!(!a.join("docs/file").exists());
    safe(&a, &state).args(["stash", "pop"]).run();
    assert_eq!(read(a.join("file")), "sparse stash\n");
    safe(&a, &state).args(["restore", "file"]).run();
    safe(&a, &state).args(["reset", "--mixed", "HEAD"]).run();
    equal(&a, &b, &state);
    for repo in [&a, &b] {
        write(repo.join("file"), "new root\n");
    }
    safe(&a, &state)
        .args(["commit", "-am", "sparse edit"])
        .run();
    real(&b).args(["commit", "-am", "sparse edit"]).run();
    assert_eq!(revision(&a, "HEAD"), revision(&b, "HEAD"));
    safe(&a, &state)
        .args(["switch", "-c", "sparse-branch", "HEAD~"])
        .run();
    assert!(!a.join("docs/file").exists());
    safe(&a, &state)
        .args(["sparse-checkout", "add", "docs"])
        .run();
    assert!(a.join("docs/file").exists());
    safe(&a, &state)
        .args(["sparse-checkout", "set", "--no-cone", "/docs/"])
        .run();
    assert!(!a.join("src/lib/file").exists());
    assert!(!a.join("file").exists());
    safe(&a, &state).args(["sparse-checkout", "disable"]).run();
    assert!(a.join("src/lib/file").exists());
    write(a.join("src/lib/file"), "local change\n");
    safe(&a, &state)
        .args(["sparse-checkout", "set", "docs"])
        .fails();
    assert_eq!(read(a.join("src/lib/file")), "local change\n");
    let outside = root.join("outside");
    write(&outside, "outside\n");
    fs::remove_file(a.join(".git/info/sparse-checkout")).unwrap();
    symlink(&outside, a.join(".git/info/sparse-checkout")).unwrap();
    safe(&a, &state)
        .args(["sparse-checkout", "set", "docs"])
        .fails();
    assert_eq!(read(&outside), "outside\n");
    passed(
        "cone/non-cone sparse checkout preserves skipped index entries and rejects dirty or escaping paths",
    );
}
pub fn network() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let state = root.join("state");
    let source = seed(root, &state, "source");
    for n in 1..5 {
        write(source.join("file"), format!("revision {n}\n"));
        safe(&source, &state)
            .args(["commit", "-am", &format!("revision {n}")])
            .run();
    }
    let bare = root.join("source.git");
    real(root)
        .args(["clone", "--bare"])
        .arg(&source)
        .arg(&bare)
        .run();
    real(&bare)
        .args(["config", "uploadpack.allowFilter", "true"])
        .run();
    real(&bare)
        .args(["config", "uploadpack.allowAnySHA1InWant", "true"])
        .run();
    let http = crate::support::http::Http::new(root);
    for (n, url) in [
        source.to_str().unwrap().to_owned(),
        format!("{}/source.git", http.url),
        fixture.url(&bare),
    ]
    .iter()
    .enumerate()
    {
        let work = root.join(format!("depth-{n}"));
        fixture
            .safe(root)
            .args(["clone", "--depth=2"])
            .arg(url)
            .arg(&work)
            .run();
        fixture.safe(&work).args(["fetch", "--deepen=2"]).run();
        assert_eq!(
            fixture
                .safe(&work)
                .args(["rev-list", "--count", "HEAD"])
                .run(),
            "4"
        );
        real(&work).args(["fsck", "--full"]).run();
        fixture.safe(&work).args(["fetch", "--deepen=2"]).run();
        assert_eq!(
            fixture
                .safe(&work)
                .args(["rev-list", "--count", "HEAD"])
                .run(),
            "5"
        );
        let partial = root.join(format!("partial-{n}"));
        fixture
            .safe(root)
            .args(["clone", "--filter=blob:none"])
            .arg(url)
            .arg(&partial)
            .run();
        assert_eq!(read(partial.join("file")), "revision 4\n");
        let missing = real(&partial)
            .args(["rev-list", "--objects", "--all", "--missing=print"])
            .run();
        assert!(missing.lines().any(|l| l.starts_with('?')));
        real(&partial).args(["fsck", "--full"]).run();
        real(&partial)
            .args([
                "config",
                "remote.origin.url",
                "ext::touch /tmp/safe-git-never",
            ])
            .run();
        real(&partial)
            .args(["config", "remote.origin.promisor", "false"])
            .run();
        let trace = root.join(format!("hydrate-{n}.trace"));
        assert_eq!(
            fixture
                .safe(&partial)
                .args(["show", "HEAD~:file"])
                .traced(&trace)
                .run(),
            "revision 3"
        );
        check_trace(&trace);
        if n == 0 {
            let deferred = root.join("deferred");
            fixture
                .safe(root)
                .args(["clone", "--filter=blob:none", "--no-checkout"])
                .arg(url)
                .arg(&deferred)
                .run();
            assert!(!deferred.join("file").exists());
            fixture
                .safe(&deferred)
                .args(["sparse-checkout", "set", "--no-cone", "/file"])
                .run();
            assert_eq!(read(deferred.join("file")), "revision 4\n");
            assert!(!deferred.join("other").exists());
            assert!(
                fixture
                    .safe(&deferred)
                    .args(["status", "--porcelain"])
                    .run()
                    .is_empty()
            );
            fixture
                .safe(&deferred)
                .args(["sparse-checkout", "disable"])
                .run();
            assert!(deferred.join("other").exists());
        }
        // Repo-authored promisor settings cannot authorize any network fetch.
        let unbound = root.join(format!("unbound-{n}"));
        fs::rename(&partial, &unbound).unwrap();
        assert!(
            fixture
                .safe(&unbound)
                .args(["show", "HEAD~2:file"])
                .fails()
                .contains("missing object")
        );
    }
    passed(
        "relative shallow fetch and blobless clones interoperate over local, HTTP and SSH without trusting repo endpoints",
    );
}
pub fn atomic_push() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let state = root.join("state");
    let source = seed(root, &state, "source-atomic");
    let http = crate::support::http::Http::new(root);
    for n in 0..2 {
        let bare = root.join(if n == 0 {
            "source.git"
        } else {
            "destination.git"
        });
        real(root)
            .args(["clone", "--bare"])
            .arg(&source)
            .arg(&bare)
            .run();
        real(&bare)
            .args(["config", "http.receivepack", "true"])
            .run();
        let url = if n == 0 {
            fixture.url(&bare)
        } else {
            format!("{}/destination.git", http.url)
        };
        let work = root.join(format!("push-{n}"));
        fixture.safe(root).arg("clone").arg(&url).arg(&work).run();
        let old = revision(&bare, "main");
        write(work.join("file"), "pushed\n");
        safe(&work, &state).args(["commit", "-am", "push"]).run();
        safe(&work, &state).args(["branch", "topic"]).run();
        write(
            bare.join("hooks/update"),
            "#!/bin/sh\n[ \"$1\" != refs/heads/topic ]\n",
        );
        chmod(bare.join("hooks/update"), 0o755);
        fixture
            .safe(&work)
            .args(["push", "--atomic", "--all", "origin"])
            .fails();
        assert_eq!(revision(&bare, "main"), old);
        assert!(
            real(&bare)
                .args(["show-ref", "--verify", "refs/heads/topic"])
                .output()
                .code
                != Some(0)
        );
        fs::remove_file(bare.join("hooks/update")).unwrap();
        real(&bare)
            .args(["config", "receive.advertiseAtomic", "false"])
            .run();
        assert!(
            fixture
                .safe(&work)
                .args(["push", "--atomic", "--all", "origin"])
                .fails()
                .contains("atomic")
        );
        assert_eq!(revision(&bare, "main"), old);
        real(&bare)
            .args(["config", "receive.advertiseAtomic", "true"])
            .run();
        fixture
            .safe(&work)
            .args(["push", "--atomic", "--all", "--dry-run", "origin"])
            .run();
        assert_eq!(revision(&bare, "main"), old);
        fixture
            .safe(&work)
            .args(["push", "--atomic", "--all", "-u", "origin"])
            .run();
        assert_eq!(revision(&bare, "main"), revision(&work, "main"));
        assert_eq!(revision(&bare, "topic"), revision(&work, "topic"));
    }
    passed(
        "SSH/HTTP atomic pushes update all branches together and fail closed on server rejection or missing capability",
    );
}
