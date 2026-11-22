//! Compare status, stdout and stderr separately with real Git.
use crate::support::*;
use std::fs;

fn equal(label: &str, got: Captured, expected: Captured) {
    assert_eq!(got, expected, "{label}");
    assert_eq!(got.code, Some(0), "{label}: {got:?}");
    passed(label);
}

pub fn suite() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let seed = root.join("seed");
    let source = root.join("source.git");
    let dest = root.join("dest.git");
    let work = root.join("work");
    let oracle = root.join("oracle");
    let ssh = format!(
        "ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o {} -o StrictHostKeyChecking=yes",
        quote(root.join("client_key")),
        quote(format!("UserKnownHostsFile={}/known_hosts", root.display()))
    );
    let environment = |cmd: TestCommand| {
        cmd.env("GIT_SSH_COMMAND", &ssh)
            .env("SAFE_GIT_IDENTITY_FILE", root.join("client_key"))
            .env("SAFE_GIT_URL", fixture.url(&source))
    };
    let safe = |args: &[&str]| {
        environment(
            TestCommand::new(BIN)
                .arg("-C")
                .arg(&work)
                .arg("--known-hosts")
                .arg(root.join("known_hosts"))
                .args(args),
        )
    };
    let real = |args: &[&str]| environment(git(&oracle).args(args)).output();
    let dest_url = fixture.url(&dest);

    git(root)
        .args(["init", "-q", "--object-format=sha1", "-b", "main"])
        .arg(&seed)
        .run();
    git(&seed).args(["config", "user.name", "test"]).run();
    git(&seed)
        .args(["config", "user.email", "test@example.invalid"])
        .run();
    write(seed.join("a"), "one\ntwo\n");
    write(seed.join("gone"), "gone\n");
    write(seed.join("mode"), "mode\n");
    write(seed.join("binary"), b"a\0b");
    git(&seed).args(["add", "."]).run();
    git(&seed).args(["commit", "-qm", "base"]).run();
    let base = head(&seed);
    write(seed.join("a"), "three\ntwo\nfour\n");
    fs::remove_file(seed.join("gone")).unwrap();
    chmod(seed.join("mode"), 0o755);
    write(seed.join("new"), "new\n");
    write(seed.join("binary"), b"a\0bc");
    write(seed.join("café"), "non-ASCII path\n");
    git(&seed).args(["add", "."]).run();
    git(&seed).args(["commit", "-qm", "forward"]).run();
    let forward = head(&seed);
    git(root)
        .args(["clone", "-q", "--bare"])
        .arg(&seed)
        .arg(&source)
        .run();
    git(&seed).args(["reset", "--hard", &base]).run();
    git(root)
        .args(["clone", "-q", "--bare"])
        .arg(&seed)
        .arg(&dest)
        .run();
    for repo in [&work, &oracle] {
        git(root)
            .args(["clone", "-q", "--no-hardlinks"])
            .arg(&seed)
            .arg(repo)
            .run();
        git(repo)
            .args(["remote", "set-url", "origin"])
            .arg(fixture.url(&source))
            .run();
        git(repo)
            .args(["update-ref", "-d", "refs/remotes/origin/main"])
            .run();
    }

    equal(
        "new-branch fetch output and streams; caller URL/key environment",
        safe(&["fetch"]).output(),
        real(&["fetch", "origin"]),
    );
    equal(
        "unchanged fetch is silent",
        safe(&["fetch"]).output(),
        real(&["fetch", "origin"]),
    );
    let merge = ["merge", "--ff-only", "origin/main"];
    equal(
        "fast-forward output including text/binary/mode/path statistics",
        safe(&merge).output(),
        real(&merge),
    );
    equal(
        "up-to-date merge stdout",
        safe(&merge).output(),
        real(&merge),
    );
    let got = safe(&["push"]).env("SAFE_GIT_URL", &dest_url).output();
    git(&dest)
        .args(["update-ref", "refs/heads/main", &base])
        .run();
    equal(
        "fast-forward push stderr",
        got,
        real(&["push", &dest_url, "main"]),
    );
    equal(
        "up-to-date push stderr",
        safe(&["push"]).env("SAFE_GIT_URL", &dest_url).output(),
        real(&["push", &dest_url, "main"]),
    );
    let got = safe(&["push", &dest_url, "main:other"]).output();
    git(&dest)
        .args(["update-ref", "-d", "refs/heads/other"])
        .run();
    equal(
        "new-branch push stderr",
        got,
        real(&["push", &dest_url, "main:other"]),
    );
    git(&source)
        .args(["update-ref", "refs/heads/main", &base])
        .run();
    equal(
        "forced tracking update stderr",
        safe(&["fetch"]).output(),
        real(&["fetch", "origin"]),
    );
    equal(
        "incoming ancestor merge stdout",
        safe(&merge).output(),
        real(&merge),
    );
    git(&source)
        .args(["update-ref", "refs/heads/main", &forward])
        .run();
    equal(
        "fast-forward fetch stderr",
        safe(&["fetch"]).output(),
        real(&["fetch", "origin"]),
    );
    for repo in [&work, &oracle] {
        git(repo).args(["reset", "--hard", &base]).run();
    }
    let no_stat = ["merge", "--ff-only", "--no-stat", "origin/main"];
    equal(
        "no-stat merge output",
        safe(&no_stat).output(),
        real(&no_stat),
    );
    let quiet_merge = ["merge", "--ff-only", "-q", "origin/main"];
    equal(
        "quiet merge output",
        safe(&quiet_merge).output(),
        real(&quiet_merge),
    );
    equal(
        "quiet push output",
        safe(&["push", "-q"])
            .env("SAFE_GIT_URL", &dest_url)
            .output(),
        real(&["push", "-q", &dest_url, "main"]),
    );
    let failed = safe(&["fetch", "https://example.invalid/repo", "main"]).output();
    assert_eq!(failed.code, Some(128));
    assert!(
        failed.stdout.is_empty() && failed.stderr.starts_with("fatal: "),
        "{failed:?}"
    );
    passed("safety refusal is fatal on stderr with no stdout");

    // Optional presentation must remain bounded and must not turn a completed
    // checkout into an apparent failure.
    git(&seed).args(["reset", "--hard", &forward]).run();
    write(seed.join("big"), "old\n".repeat(3000));
    git(&seed).args(["add", "big"]).run();
    git(&seed).args(["commit", "-qm", "large base"]).run();
    let large_base = head(&seed);
    git(&work).arg("fetch").arg(&seed).arg("main").run();
    git(&work).args(["reset", "--hard", &large_base]).run();
    write(seed.join("big"), "new\n".repeat(3000));
    git(&seed).args(["commit", "-qam", "large forward"]).run();
    let large_forward = head(&seed);
    git(&seed).arg("push").arg(&source).arg("main").run();
    safe(&["fetch"]).run();
    assert_eq!(
        safe(&merge).output(),
        Captured {
            code: Some(0),
            stdout: format!(
                "Updating {}..{}\nFast-forward\n",
                &large_base[..7],
                &large_forward[..7]
            ),
            stderr: "warning: safe-git: diffstat omitted (diffstat comparison limit exceeded)\n"
                .to_owned(),
        }
    );
    assert_eq!(head(&work), large_forward);
    assert_eq!(read(work.join("big")), "new\n".repeat(3000));
    assert!(git(&work).args(["status", "--porcelain"]).run().is_empty());
    passed("diffstat resource limit gives a warning while checkout succeeds");
}
