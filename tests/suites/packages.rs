//! Actual paru/aur-fetch and makepkg command sequences, plus hostile metadata.
use crate::support::*;
use std::{fs, os::unix::fs::symlink, path::Path};
fn safe(repo: &Path, state: &Path) -> TestCommand {
    TestCommand::new(BIN)
        .arg("-C")
        .arg(repo)
        .env("SAFE_GIT_STATE_DIR", state)
}
fn equal(repo: &Path, state: &Path, args: &[&str]) {
    let got = safe(repo, state).args(args).output();
    let expected = git(repo).args(args).output();
    assert_eq!(got.code, expected.code, "{args:?}: {got:?}");
    assert_eq!(got.stdout, expected.stdout, "{args:?}");
}
pub fn suite() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let state = root.join("state");
    let seed = root.join("seed");
    let work = root.join("work");
    git(root)
        .args(["init", "-q", "-b", "master"])
        .arg(&seed)
        .run();
    git(&seed).args(["config", "user.name", "test"]).run();
    git(&seed)
        .args(["config", "user.email", "test@example.invalid"])
        .run();
    write(
        seed.join("PKGBUILD"),
        "pkgname=fixture\npkgver=1\npkgrel=1\n",
    );
    write(seed.join(".SRCINFO"), "pkgbase = fixture\n\tpkgver = 1\n");
    write(
        seed.join(".gitattributes"),
        "* filter=probe diff=probe\nPKGBUILD export-subst export-ignore\n",
    );
    write(
        seed.join(".gitmodules"),
        "[submodule \"evil\"]\npath=evil\nurl=ext::probe\n",
    );
    fs::create_dir(seed.join("dir")).unwrap();
    write(seed.join("dir/file"), "content\n");
    symlink("dir/file", seed.join("link")).unwrap();
    write(seed.join("a".repeat(150)), "long name\n");
    git(&seed).args(["add", "."]).run();
    git(&seed).args(["commit", "-qm", "base"]).run();
    let base = revision(&seed, "HEAD");
    git(&seed).args(["branch", "other"]).run();
    git(&seed).args(["tag", "-am", "release", "v1"]).run();
    safe(root, &state)
        .args(["clone", "--no-progress", "--"])
        .arg(&seed)
        .arg(&work)
        .run();
    assert_eq!(revision(&work, "origin/other"), base);
    for args in [
        vec!["rev-parse", "HEAD", "HEAD@{u}"],
        vec!["rev-parse", "--verify", "HEAD"],
        vec!["symbolic-ref", "-q", "HEAD"],
        vec!["tag", "-l", "--format=%(tag)", "v1"],
        vec!["describe", "--tags", "--long"],
        vec!["rev-list", "--count", "HEAD"],
        vec!["rev-parse", "--short=7", "HEAD"],
        vec!["rev-parse", "--git-path", "HEAD"],
    ] {
        equal(&work, &state, &args);
    }
    equal(
        &work,
        &state,
        &["ls-remote", "--", seed.to_str().unwrap(), "HEAD"],
    );
    safe(&work, &state)
        .args(["update-ref", "AUR_SEEN", "HEAD"])
        .run();
    equal(&work, &state, &["rev-parse", "--verify", "AUR_SEEN"]);
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
        "credential.helper",
        "diff.external",
        "filter.probe.process",
        "filter.probe.clean",
        "filter.probe.smudge",
        "diff.probe.command",
        "merge.probe.driver",
    ] {
        git(&work).args(["config", key]).arg(&probe).run();
    }
    git(&work)
        .args([
            "config",
            "remote.origin.url",
            "ext::attacker-selected-command",
        ])
        .run();
    write(
        seed.join("PKGBUILD"),
        "pkgname=fixture\npkgver=2\npkgrel=1\n",
    );
    write(seed.join(".SRCINFO"), "pkgbase = fixture\n\tpkgver = 2\n");
    git(&seed).args(["commit", "-qam", "upstream"]).run();
    let upstream = revision(&seed, "HEAD");
    let trace = root.join("fetch.trace");
    safe(&work, &state)
        .args(["fetch", "-v"])
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert_eq!(revision(&work, "origin/master"), upstream);
    equal(&work, &state, &["rev-parse", "AUR_SEEN", "HEAD@{u}"]);
    equal(
        &work,
        &state,
        &["merge-base", "--is-ancestor", "HEAD@{u}", "AUR_SEEN"],
    );
    // Avoid Git's intentionally hostile diff/execution settings in the comparison clone.
    let expected = root.join("expected");
    git(root)
        .args(["clone", "-q"])
        .arg(&seed)
        .arg(&expected)
        .run();
    git(&expected).args(["reset", "--hard", &base]).run();
    equal(&work, &state, &["log", "..HEAD@{u}", "--color=never"]);
    let initial = [
        "diff",
        "--stat",
        "--patch",
        "--color=never",
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904..HEAD@{u}",
        "--",
        ":!.SRCINFO",
    ];
    assert_eq!(
        safe(&work, &state).args(initial).output().stdout,
        git(&expected).args(initial).output().stdout
    );
    safe(&work, &state)
        .args(["reset", "--hard", "AUR_SEEN"])
        .run();
    let trace = root.join("merge.trace");
    safe(&work, &state)
        .args([
            "-c",
            "user.email=aur",
            "-c",
            "user.name=aur",
            "merge",
            "--no-edit",
            "--no-ff",
            "--no-commit",
        ])
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert_eq!(revision(&work, "HEAD"), base);
    assert_eq!(read(work.join("PKGBUILD")), read(seed.join("PKGBUILD")));
    git(&expected)
        .args([
            "-c",
            "user.name=aur",
            "-c",
            "user.email=aur",
            "merge",
            "--no-edit",
            "--no-ff",
            "--no-commit",
        ])
        .run();
    let diffargs = [
        "diff",
        "--stat",
        "--patch",
        "--cached",
        "--color=never",
        "--",
        ":!.SRCINFO",
    ];
    let got = safe(&work, &state).args(diffargs).output();
    let want = git(&expected).args(diffargs).output();
    assert_eq!(got.code, Some(0), "{got:?}");
    assert_eq!(got.stdout, want.stdout);
    safe(&work, &state)
        .args(["reset", "--hard", "-q", "HEAD"])
        .run();
    assert_eq!(
        read(work.join("PKGBUILD")),
        "pkgname=fixture\npkgver=1\npkgrel=1\n"
    );
    let trace = root.join("rebase.trace");
    safe(&work, &state)
        .args(["rebase", "--stat"])
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert_eq!(revision(&work, "HEAD"), upstream);
    write(
        work.join("PKGBUILD"),
        "pkgname=fixture\npkgver=2\npkgrel=2\n",
    );
    assert_eq!(
        safe(&work, &state)
            .args(["diff", "--exit-code"])
            .output()
            .code,
        Some(1)
    );
    let trace = root.join("commit.trace");
    safe(&work, &state)
        .args([
            "-c",
            "user.name=aur",
            "-c",
            "user.email=aur",
            "commit",
            "-am",
            "AUR",
        ])
        .traced(&trace)
        .run();
    check_trace(&trace);
    let local = revision(&work, "HEAD");
    assert_ne!(local, upstream);
    assert_eq!(revision(&work, "HEAD^"), upstream);
    write(seed.join("dir/file"), "upstream again\n");
    git(&seed).args(["commit", "-qam", "next"]).run();
    let next = revision(&seed, "HEAD");
    safe(&work, &state).args(["fetch", "-v"]).run();
    safe(&work, &state)
        .args([
            "-c",
            "user.name=aur",
            "-c",
            "user.email=aur",
            "rebase",
            "--stat",
        ])
        .run();
    assert_eq!(revision(&work, "HEAD^"), next);
    assert!(read(work.join("PKGBUILD")).contains("pkgrel=2"));
    // A conflicting rebase records stages and can restore the original commit and files.
    let preserved = revision(&work, "HEAD");
    write(
        seed.join("PKGBUILD"),
        "pkgname=fixture\npkgver=2\npkgrel=3\n",
    );
    git(&seed).args(["commit", "-qam", "conflict"]).run();
    safe(&work, &state).arg("fetch").run();
    safe(&work, &state)
        .args([
            "-c",
            "user.name=aur",
            "-c",
            "user.email=aur",
            "rebase",
            "--stat",
        ])
        .fails();
    safe(&work, &state).args(["rebase", "--abort"]).run();
    assert_eq!(revision(&work, "HEAD"), preserved);
    assert!(read(work.join("PKGBUILD")).contains("pkgrel=2"));
    assert!(!marker.exists());
    passed(
        "paru review, saved changes, clean rebase and conflict preservation; hostile config cannot execute or redirect fetch",
    );
    // makepkg: mirror, prune, local shared clone, checkout tag/commit, exact tar checksums.
    let mirror = root.join("mirror.git");
    safe(root, &state)
        .args(["clone", "--origin=origin", "--mirror"])
        .arg(&seed)
        .arg(&mirror)
        .run();
    safe(&mirror, &state)
        .args([
            "-c",
            "safe.bareRepository=all",
            "config",
            "--get",
            "remote.origin.url",
        ])
        .run();
    safe(&mirror, &state)
        .args(["-c", "safe.bareRepository=all", "fetch", "--all", "-p"])
        .run();
    equal(&mirror, &state, &["rev-parse", "--absolute-git-dir"]);
    let extracted = root.join("extracted");
    safe(root, &state)
        .args(["clone", "--origin=origin", "-s"])
        .arg(&mirror)
        .arg(&extracted)
        .run();
    assert!(!extracted.join(".git/objects/info/alternates").exists());
    safe(&extracted, &state)
        .args([
            "checkout",
            "--force",
            "--no-track",
            "-B",
            "makepkg",
            "v1",
            "--",
        ])
        .run();
    assert_eq!(revision(&extracted, "HEAD"), base);
    safe(&extracted, &state).arg("fetch").run();
    safe(&extracted, &state)
        .args([
            "checkout",
            "--force",
            "--no-track",
            "-B",
            "makepkg",
            "origin/master",
            "--",
        ])
        .run();
    assert_eq!(revision(&extracted, "HEAD"), revision(&seed, "HEAD"));
    fs::create_dir_all(mirror.join("info")).unwrap();
    write(
        mirror.join("info/attributes"),
        "* -export-subst -export-ignore\n",
    );
    for name in ["master", "v1"] {
        let args = ["-c", "core.abbrev=no", "archive", "--format", "tar", name];
        let got = safe(&mirror, &state).args(args).output();
        let want = git(&mirror).args(args).output();
        assert_eq!(got.code, Some(0), "{got:?}");
        assert_eq!(
            got.stdout.as_bytes(),
            want.stdout.as_bytes(),
            "archive {name}"
        );
    }
    git(&seed).args(["branch", "-D", "other"]).run();
    safe(&mirror, &state).args(["fetch", "--all", "-p"]).run();
    assert_eq!(
        safe(&mirror, &state)
            .args(["rev-parse", "--verify", "refs/heads/other"])
            .output()
            .code,
        Some(128)
    );
    // Detached HEAD reset and switching back must not rewrite the old branch.
    let branch = revision(&extracted, "makepkg");
    safe(&extracted, &state)
        .args(["checkout", "--force", &base])
        .run();
    assert_eq!(
        safe(&extracted, &state)
            .args(["symbolic-ref", "-q", "HEAD"])
            .output()
            .code,
        Some(1)
    );
    safe(&extracted, &state)
        .args(["reset", "--hard", "-q", "HEAD"])
        .run();
    safe(&extracted, &state).args(["checkout", "makepkg"]).run();
    assert_eq!(revision(&extracted, "makepkg"), branch);
    assert!(!marker.exists());
    git(&extracted).args(["fsck", "--full"]).run();
    passed(
        "makepkg mirror/prune, materialized shared clone, force checkout, detached HEAD and byte-identical Git tar archives",
    );
    // A staged-only commit preserves unstaged user edits, and a fixed identity/date produces Git's exact object.
    let staged_only = root.join("staged-only");
    git(root)
        .args(["clone", "-q"])
        .arg(&seed)
        .arg(&staged_only)
        .run();
    let comparison = root.join("comparison");
    git(root)
        .args(["clone", "-q"])
        .arg(&seed)
        .arg(&comparison)
        .run();
    for repo in [&staged_only, &comparison] {
        write(repo.join("PKGBUILD"), "staged\n");
        git(repo).args(["add", "PKGBUILD"]).run();
        write(repo.join("PKGBUILD"), "unstaged\n");
    }
    let commit_args = [
        "-c",
        "user.name=aur",
        "-c",
        "user.email=aur",
        "commit",
        "-m",
        "AUR",
    ];
    let got = safe(&staged_only, &state)
        .args(commit_args)
        .env("GIT_AUTHOR_DATE", "1234567890 +0000")
        .env("GIT_COMMITTER_DATE", "1234567890 +0000")
        .output();
    let want = git(&comparison)
        .args(commit_args)
        .env("GIT_AUTHOR_DATE", "1234567890 +0000")
        .env("GIT_COMMITTER_DATE", "1234567890 +0000")
        .output();
    assert_eq!(got.code, Some(0), "{got:?}");
    assert_eq!(
        revision(&staged_only, "HEAD"),
        revision(&comparison, "HEAD")
    );
    assert_eq!(got.stdout, want.stdout);
    assert_eq!(read(staged_only.join("PKGBUILD")), "unstaged\n");
    equal(
        &staged_only,
        &state,
        &["log", "-1", "--format=%H%n%h%n%s%n%ct%n%at%n%%"],
    );
    equal(
        &staged_only,
        &state,
        &["log", "-1", "--format=%cd", "--date=format:%Y%m%d"],
    );
    // Hard links cannot import host files through commit/diff/reset's working-tree reads.
    let secret = root.join("secret");
    write(&secret, "host data\n");
    fs::remove_file(staged_only.join("PKGBUILD")).unwrap();
    fs::hard_link(&secret, staged_only.join("PKGBUILD")).unwrap();
    for args in [
        vec!["diff"],
        vec!["reset", "--hard", "HEAD"],
        vec![
            "-c",
            "user.name=aur",
            "-c",
            "user.email=aur",
            "commit",
            "-am",
            "refused",
        ],
    ] {
        assert!(
            safe(&staged_only, &state)
                .args(args)
                .fails()
                .contains("hard links")
        );
    }
    assert_eq!(read(&secret), "host data\n");
    passed(
        "staged-only commits preserve unstaged edits, match Git object IDs/output, and reject external hard links",
    );
    // Repository files cannot select a new source; pre-existing Git caches require explicit registration.
    assert!(
        safe(&expected, &state)
            .arg("fetch")
            .fails()
            .contains("repository remote configuration is not consulted")
    );
    safe(&expected, &state)
        .args(["config", "remote.origin.url"])
        .arg(&seed)
        .run();
    safe(&expected, &state).arg("fetch").run();
    // No filesystem redirection even for explicit destructive commands.
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    write(outside.join("file"), "untouched\n");
    fs::remove_dir_all(extracted.join("dir")).unwrap();
    symlink(&outside, extracted.join("dir")).unwrap();
    safe(&extracted, &state)
        .args(["reset", "--hard", "HEAD"])
        .fails();
    assert_eq!(read(outside.join("file")), "untouched\n");
    assert!(!marker.exists());
    passed("explicit cache registration and destructive-command symlink confinement");
}
