//! Git interoperability and hostile-metadata regressions for the remaining workflow surface.
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
            "user.name=test",
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
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
        ])
}
fn seed(root: &Path, state: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    safe(root, state)
        .args(["init", "-b", "main"])
        .arg(&repo)
        .run();
    write(repo.join("file"), "one\ntwo\nthree\n");
    safe(&repo, state).args(["add", "."]).run();
    safe(&repo, state).args(["commit", "-m", "base"]).run();
    repo
}
pub fn inspection_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state, "repo");
    write(repo.join("file"), "one\nTWO\nthree\nfour\n");
    safe(&repo, &state).args(["commit", "-am", "second"]).run();
    write(repo.join("other"), "other\n");
    safe(&repo, &state).args(["add", "other"]).run();
    safe(&repo, &state)
        .args(["commit", "-m", "unrelated"])
        .run();
    assert_eq!(
        safe(&repo, &state)
            .args(["show", "HEAD:file"])
            .output()
            .stdout,
        real(&repo).args(["show", "HEAD:file"]).output().stdout
    );
    assert_eq!(
        safe(&repo, &state)
            .args(["log", "--format=%H", "--", "file"])
            .run(),
        real(&repo).args(["log", "--format=%H", "--", "file"]).run()
    );
    let headers = |text: String| {
        text.lines()
            .filter(|l| {
                l.split(' ')
                    .next()
                    .is_some_and(|v| v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit()))
            })
            .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        headers(
            safe(&repo, &state)
                .args(["blame", "--line-porcelain", "file"])
                .run()
        ),
        headers(
            real(&repo)
                .args(["blame", "--line-porcelain", "file"])
                .run()
        )
    );
    write(repo.join("file"), "one\nLOCAL\nthree\nfour\n");
    assert_eq!(
        headers(
            safe(&repo, &state)
                .args(["blame", "--line-porcelain", "file"])
                .run()
        ),
        headers(
            real(&repo)
                .args(["blame", "--line-porcelain", "file"])
                .run()
        )
    );
    safe(&repo, &state).args(["restore", "file"]).run();
    safe(&repo, &state).args(["branch", "side", "HEAD^"]).run();
    let graph = safe(&repo, &state)
        .args(["log", "--all", "--graph", "--decorate", "--oneline"])
        .run();
    assert!(graph.contains("HEAD -> main") && graph.contains("side") && graph.contains('*'));
    assert_eq!(
        safe(&repo, &state).args(["diff", "side...HEAD"]).run(),
        real(&repo).args(["diff", "side...HEAD"]).run()
    );
    assert!(
        safe(&repo, &state)
            .args(["show", "--stat", "HEAD"])
            .run()
            .contains("other")
    );
    write(repo.join("file"), "staged\n");
    safe(&repo, &state).args(["add", "file"]).run();
    write(repo.join("file"), "unstaged\n");
    safe(&repo, &state).args(["mv", "file", "moved"]).run();
    assert_eq!(real(&repo).args(["show", ":moved"]).run(), "staged");
    assert_eq!(read(repo.join("moved")), "unstaged\n");
    safe(&repo, &state).args(["rm", "moved"]).fails();
    safe(&repo, &state)
        .args(["rm", "--cached", "-f", "moved"])
        .run();
    assert!(repo.join("moved").exists());
    write(repo.join(".gitignore"), "ignored\n");
    write(repo.join("ignored"), "keep\n");
    write(repo.join("untracked"), "remove\n");
    write(repo.join("only-staged"), "sole copy\n");
    safe(&repo, &state).args(["add", "only-staged"]).run();
    fs::remove_file(repo.join("only-staged")).unwrap();
    safe(&repo, &state)
        .args(["rm", "--cached", "only-staged"])
        .fails();
    assert_eq!(
        real(&repo).args(["show", ":only-staged"]).run(),
        "sole copy"
    );
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    write(outside.join("keep"), "outside\n");
    symlink(&outside, repo.join("escape")).unwrap();
    safe(&repo, &state).args(["clean", "-n"]).run();
    assert!(repo.join("untracked").exists());
    safe(&repo, &state).args(["clean", "-f"]).run();
    assert!(!repo.join("untracked").exists());
    assert!(repo.join("ignored").exists());
    assert_eq!(read(outside.join("keep")), "outside\n");
    let marker = root.join("EXECUTED");
    let probe = root.join("probe");
    write(&probe, format!("#!/bin/sh\ntouch {}\n", quote(&marker)));
    chmod(&probe, 0o700);
    for key in [
        "core.pager",
        "diff.external",
        "core.editor",
        "core.hooksPath",
    ] {
        real(&repo).arg("config").arg(key).arg(&probe).run();
    }
    for args in [
        vec!["show", "HEAD"],
        vec!["blame", "other"],
        vec!["log", "--graph", "--all"],
        vec!["clean", "-n"],
    ] {
        let trace = root.join("trace");
        safe(&repo, &state).args(args).traced(&trace).run();
        check_trace(&trace);
    }
    assert!(!marker.exists());
    passed(
        "show/blame/history match Git; moves retain unstaged edits; removal and clean preserve protected data without helpers",
    );
}
pub fn tags_remotes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state, "repo");
    safe(&repo, &state).args(["tag", "v1"]).run();
    safe(&repo, &state)
        .args(["tag", "-a", "v2", "-m", "release"])
        .run();
    assert_eq!(revision(&repo, "v1"), revision(&repo, "v2^{}"));
    real(&repo).args(["fsck", "--full"]).run();
    safe(&repo, &state).args(["tag", "v1"]).fails();
    safe(&repo, &state).args(["tag", "-d", "v1"]).run();
    let source = seed(root, &state, "source");
    let second = seed(root, &state, "second");
    for (name, path) in [("origin", &source), ("upstream", &second)] {
        safe(&repo, &state)
            .args(["remote", "add", name])
            .arg(path)
            .run();
    }
    real(&repo)
        .args(["config", "remote.origin.url", "/definitely/not/registered"])
        .run();
    assert_eq!(
        safe(&repo, &state)
            .args(["remote", "get-url", "origin"])
            .run(),
        source.to_str().unwrap()
    );
    safe(&repo, &state).args(["fetch", "--all"]).run();
    assert_eq!(revision(&repo, "origin/main"), revision(&source, "HEAD"));
    assert_eq!(revision(&repo, "upstream/main"), revision(&second, "HEAD"));
    safe(&repo, &state)
        .args(["remote", "rename", "upstream", "other"])
        .run();
    assert_eq!(revision(&repo, "other/main"), revision(&second, "HEAD"));
    safe(&repo, &state)
        .args(["remote", "remove", "other"])
        .run();
    assert_eq!(safe(&repo, &state).arg("remote").run(), "origin");
    passed(
        "local tag objects interoperate with Git; multiple registered remotes ignore hostile config URLs",
    );
}
pub fn sequences_bisect() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state, "repo");
    let base = revision(&repo, "HEAD");
    safe(&repo, &state).args(["switch", "-c", "topic"]).run();
    write(repo.join("topic"), "a\n");
    safe(&repo, &state).args(["add", "topic"]).run();
    safe(&repo, &state).args(["commit", "-m", "feature"]).run();
    write(repo.join("topic"), "b\n");
    safe(&repo, &state)
        .args(["commit", "-am", "fixup! feature"])
        .run();
    safe(&repo, &state)
        .args(["rebase", "--autosquash", &base])
        .run();
    assert_eq!(
        real(&repo)
            .args(["rev-list", "--count", &format!("{base}..HEAD")])
            .run(),
        "1"
    );
    assert_eq!(read(repo.join("topic")), "b\n");
    safe(&repo, &state).args(["switch", "main"]).run();
    write(repo.join("onto"), "onto\n");
    safe(&repo, &state).args(["add", "onto"]).run();
    safe(&repo, &state).args(["commit", "-m", "onto"]).run();
    let onto = revision(&repo, "HEAD");
    safe(&repo, &state).args(["switch", "topic"]).run();
    safe(&repo, &state)
        .args(["rebase", "--onto", &onto, &base])
        .run();
    assert_eq!(revision(&repo, "HEAD^"), onto);
    assert_eq!(read(repo.join("topic")), "b\n");
    let mut commits = Vec::new();
    for n in 0..8 {
        write(repo.join("number"), format!("{n}\n"));
        safe(&repo, &state).args(["add", "number"]).run();
        safe(&repo, &state)
            .args(["commit", "-m", &format!("number {n}")])
            .run();
        commits.push(revision(&repo, "HEAD"));
    }
    safe(&repo, &state)
        .args(["bisect", "start", &commits[7], &commits[0]])
        .run();
    let mut result = String::new();
    for _ in 0..8 {
        let n = read(repo.join("number")).trim().parse::<usize>().unwrap();
        result = safe(&repo, &state)
            .args(["bisect", if n >= 4 { "bad" } else { "good" }])
            .run();
        if result.contains("first bad commit") {
            break;
        }
    }
    assert!(result.contains(&format!("{} is the first bad commit", commits[4])));
    safe(&repo, &state)
        .args(["bisect", "run", "sh", "-c", "false"])
        .fails();
    safe(&repo, &state).args(["bisect", "reset"]).run();
    assert_eq!(revision(&repo, "HEAD"), commits[7]);
    passed(
        "autosquash and onto preserve patches; data-only bisect identifies the regression and restores the branch",
    );
}
pub fn linked_submodules() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state, "repo");
    let work = root.join("linked");
    safe(&repo, &state)
        .args(["worktree", "add", "-b", "other"])
        .arg(&work)
        .run();
    assert_eq!(read(work.join("file")), read(repo.join("file")));
    safe(&work, &state).arg("status").run();
    real(&work).args(["status", "--short"]).run();
    write(work.join("file"), "linked\n");
    safe(&work, &state)
        .args(["commit", "-am", "linked change"])
        .run();
    assert_eq!(revision(&repo, "other"), revision(&work, "HEAD"));
    assert_ne!(read(repo.join("file")), read(work.join("file")));
    safe(&repo, &state).args(["switch", "other"]).fails();
    safe(&work, &state).args(["switch", "main"]).fails();
    let link = read(work.join(".git"));
    write(
        work.join(".git"),
        format!("gitdir: {}\n", repo.join(".git").display()),
    );
    safe(&work, &state).arg("status").fails();
    write(work.join(".git"), link);
    safe(&repo, &state)
        .args(["worktree", "remove"])
        .arg(&work)
        .run();
    assert!(!work.exists());
    let existing_tip = revision(&repo, "other");
    safe(&repo, &state)
        .args(["worktree", "add"])
        .arg(root.join("other"))
        .run();
    assert_eq!(revision(&root.join("other"), "HEAD"), existing_tip);
    assert_eq!(revision(&repo, "other"), existing_tip);
    assert_eq!(
        safe(&repo, &state)
            .args(["worktree", "list", "--porcelain"])
            .run(),
        real(&repo).args(["worktree", "list", "--porcelain"]).run()
    );
    safe(&repo, &state)
        .args(["worktree", "remove"])
        .arg(root.join("other"))
        .run();
    let source = seed(root, &state, "module");
    safe(&repo, &state)
        .args(["submodule", "add"])
        .arg(&source)
        .arg("deps/module")
        .run();
    safe(&repo, &state).args(["commit", "-m", "module"]).run();
    let first = revision(&source, "HEAD");
    assert!(
        safe(&repo, &state)
            .args(["submodule", "status"])
            .run()
            .contains(&first)
    );
    write(source.join("file"), "new module\n");
    safe(&source, &state)
        .args(["commit", "-am", "module update"])
        .run();
    let second = revision(&source, "HEAD");
    real(&repo)
        .args([
            "update-index",
            "--cacheinfo",
            &format!("160000,{second},deps/module"),
        ])
        .run();
    write(
        repo.join(".gitmodules"),
        "[submodule \"deps/module\"]\npath = deps/module\nurl = ext::touch /tmp/should-never-run\nupdate = !touch /tmp/should-never-run\n",
    );
    safe(&repo, &state)
        .args(["submodule", "update", "--", "deps/module"])
        .run();
    assert_eq!(revision(&repo.join("deps/module"), "HEAD"), second);
    let untrusted = root.join("untrusted");
    safe(root, &state)
        .arg("clone")
        .arg(&repo)
        .arg(&untrusted)
        .run();
    safe(&untrusted, &state)
        .args(["submodule", "update", "--init"])
        .fails();
    safe(&untrusted, &state)
        .args(["submodule", "init", "--url"])
        .arg(&source)
        .args(["--", "deps/module"])
        .run();
    safe(&untrusted, &state).args(["submodule", "update"]).run();
    assert_eq!(revision(&untrusted.join("deps/module"), "HEAD"), first);
    passed(
        "linked worktrees share verified objects and refs through host registrations; submodule fetches ignore repo URLs and commands",
    );
}
pub fn shallow_network() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let state = root.join("state");
    let repo = seed(root, &state, "source");
    for n in 1..5 {
        write(repo.join("file"), format!("{n}\n"));
        safe(&repo, &state)
            .args(["commit", "-am", &format!("commit {n}")])
            .run();
    }
    let bare = root.join("source.git");
    real(root)
        .args(["clone", "--bare"])
        .arg(&repo)
        .arg(&bare)
        .run();
    let http = crate::support::http::Http::new(root);
    let urls = [
        repo.to_str().unwrap().to_owned(),
        format!("{}/source.git", http.url),
        fixture.url(&bare),
    ];
    for (n, url) in urls.iter().enumerate() {
        let work = root.join(format!("shallow-{n}"));
        fixture
            .safe(root)
            .args(["clone", "--depth", "2"])
            .arg(url)
            .arg(&work)
            .run();
        assert_eq!(
            real(&work)
                .args(["rev-parse", "--is-shallow-repository"])
                .run(),
            "true"
        );
        assert_eq!(
            fixture
                .safe(&work)
                .args(["rev-list", "--count", "HEAD"])
                .run(),
            "2"
        );
        real(&work).args(["fsck", "--full"]).run();
        fixture.safe(&work).args(["fetch", "--unshallow"]).run();
        assert_eq!(
            fixture
                .safe(&work)
                .args(["rev-list", "--count", "HEAD"])
                .run(),
            "5"
        );
    }
    passed("local, HTTP and SSH shallow clones bound history, pass Git fsck, and safely unshallow");
}

pub fn edge_safety() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let state = root.join("state");
    let repo = seed(root, &state, "repo");
    fs::create_dir(repo.join("dir")).unwrap();
    write(repo.join("dir/tracked"), "tracked\n");
    safe(&repo, &state).args(["add", "dir"]).run();
    safe(&repo, &state).args(["commit", "-m", "dir"]).run();
    write(repo.join("dir/local"), "untracked\n");
    safe(&repo, &state).args(["mv", "dir", "renamed"]).run();
    assert!(!repo.join("dir").exists());
    assert_eq!(read(repo.join("renamed/local")), "untracked\n");
    assert!(
        !real(&repo)
            .args(["ls-files"])
            .run()
            .contains("renamed/local")
    );
    fs::create_dir_all(repo.join("empty/nested")).unwrap();
    safe(&repo, &state).args(["clean", "-fd"]).run();
    assert!(!repo.join("empty").exists());
    assert!(!repo.join("renamed/local").exists());
    safe(&repo, &state).args(["commit", "-m", "rename"]).run();
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    write(outside.join("keep"), "preserve\n");
    symlink(&outside, repo.join(".git/worktrees")).unwrap();
    safe(&repo, &state)
        .args(["worktree", "add", "-b", "bad"])
        .arg(root.join("bad"))
        .fails();
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    fs::remove_file(repo.join(".git/worktrees")).unwrap();
    let linked = root.join("linked");
    safe(&repo, &state)
        .args(["worktree", "add", "-b", "other"])
        .arg(&linked)
        .run();
    for args in [
        vec!["branch", "-D", "other"],
        vec!["branch", "-f", "other", "HEAD"],
        vec!["checkout", "other"],
        vec!["branch", "-m", "other", "changed"],
    ] {
        safe(&repo, &state).args(args).fails();
    }
    let metadata = PathBuf::from(
        read(linked.join(".git"))
            .trim()
            .strip_prefix("gitdir: ")
            .unwrap(),
    );
    let original_head = read(metadata.join("HEAD"));
    fs::remove_file(metadata.join("HEAD")).unwrap();
    TestCommand::new("mkfifo").arg(metadata.join("HEAD")).run();
    safe(&repo, &state)
        .args(["branch", "-f", "other", "HEAD"])
        .fails();
    fs::remove_file(metadata.join("HEAD")).unwrap();
    write(metadata.join("HEAD"), original_head);
    safe(&repo, &state)
        .args(["worktree", "remove"])
        .arg(&linked)
        .run();
    let unborn = root.join("unborn");
    safe(root, &state).arg("init").arg(&unborn).run();
    write(unborn.join("new"), "new\n");
    safe(&unborn, &state).args(["add", "new"]).run();
    safe(&unborn, &state).args(["rm", "-f", "new"]).run();
    assert!(!unborn.join("new").exists());
    assert!(
        real(&unborn)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .code
            != Some(0)
    );
    let remote = seed(root, &state, "remote");
    let work = root.join("pull");
    safe(root, &state)
        .arg("clone")
        .arg(&remote)
        .arg(&work)
        .run();
    write(work.join("local"), "local\n");
    safe(&work, &state).args(["add", "local"]).run();
    safe(&work, &state).args(["commit", "-m", "local"]).run();
    write(remote.join("remote"), "remote\n");
    safe(&remote, &state).args(["add", "remote"]).run();
    safe(&remote, &state).args(["commit", "-m", "remote"]).run();
    safe(&work, &state).args(["pull", "--rebase"]).run();
    assert_eq!(revision(&work, "HEAD^"), revision(&remote, "HEAD"));
    assert_eq!(read(work.join("local")), "local\n");
    safe(&repo, &state)
        .args(["submodule", "add"])
        .arg(&remote)
        .arg("module")
        .run();
    safe(&repo, &state).args(["commit", "-m", "module"]).run();
    let module = repo.join("module");
    write(module.join("file"), "unpublished\n");
    safe(&module, &state)
        .args(["commit", "-am", "private"])
        .run();
    let private = revision(&module, "HEAD");
    safe(&repo, &state)
        .args(["submodule", "deinit", "--", "module"])
        .run();
    let backup = fs::read_dir(repo.join(".git/safe-git-submodules"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(revision(&backup, "HEAD"), private);
    assert_eq!(read(outside.join("keep")), "preserve\n");
    passed(
        "directory operations preserve edits; worktree metadata cannot escape; occupied branches, unborn HEAD, rebase pull and submodule backups remain safe",
    );
}
