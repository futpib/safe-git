//! Working-tree safety and recovery, using Git as a format oracle.
use crate::support::*;
use std::{
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{PermissionsExt, symlink},
    },
    path::Path,
    thread,
    time::{Duration, Instant},
};

fn unchanged(work: &Path, action: impl FnOnce()) {
    let before_index = fs::read(work.join(".git/index")).unwrap();
    let before_ref = head(work);
    action();
    assert_eq!(head(work), before_ref);
    assert_eq!(fs::read(work.join(".git/index")).unwrap(), before_index);
    assert!(!work.join(".git/safe-git-checkout").exists());
}

pub fn suite() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let seed = root.join("seed");
    let source = root.join("source.git");
    let dest = root.join("dest.git");
    git(root)
        .args(["init", "-q", "--object-format=sha1", "-b", "main"])
        .arg(&seed)
        .run();
    git(&seed)
        .args(["config", "user.name", "safe-git test"])
        .run();
    git(&seed)
        .args(["config", "user.email", "test@example.invalid"])
        .run();
    for name in ["payload", "unstaged", "staged", "gone", "executable"] {
        write(seed.join(name), format!("{name} base\n"));
    }
    write(seed.join(".gitattributes"), "payload filter=probe\n");
    symlink("payload", seed.join("link")).unwrap();
    for i in 0..300 {
        write(seed.join(format!("bulk-{i:03}")), "old\n");
    }
    git(&seed).args(["add", "."]).run();
    git(&seed).args(["commit", "-qm", "base"]).run();
    let base = head(&seed);
    git(&seed)
        .args(["update-index", "--add", "--cacheinfo"])
        .arg(format!("160000,{base},module"))
        .run();
    git(&seed).args(["commit", "-qm", "gitlink"]).run();
    let base = head(&seed);
    write(seed.join("payload"), "payload forward\n");
    fs::remove_file(seed.join("gone")).unwrap();
    chmod(seed.join("executable"), 0o755);
    fs::create_dir(seed.join("newdir")).unwrap();
    write(seed.join("newdir/file"), "new file\n");
    fs::remove_file(seed.join("link")).unwrap();
    symlink("newdir/file", seed.join("link")).unwrap();
    for i in 0..300 {
        write(seed.join(format!("bulk-{i:03}")), "new\n");
    }
    git(&seed).args(["add", "."]).run();
    git(&seed)
        .args(["update-index", "--add", "--cacheinfo"])
        .arg(format!("160000,{base},module"))
        .run();
    git(&seed).args(["commit", "-qm", "forward"]).run();
    let forward = head(&seed);
    git(&seed).args(["branch", "future", &forward]).run();
    git(&seed).args(["reset", "--hard", &base]).run();
    for repo in [&source, &dest] {
        git(root)
            .args(["clone", "-q", "--bare"])
            .arg(&seed)
            .arg(repo)
            .run();
    }
    git(&source)
        .args(["update-ref", "refs/heads/main", &forward])
        .run();
    let mut serial = 0;
    let mut clone = || {
        serial += 1;
        let work = root.join(format!("work-{serial}"));
        git(root)
            .args(["clone", "-q", "--no-hardlinks"])
            .arg(&seed)
            .arg(&work)
            .run();
        git(&work)
            .args(["update-ref", "refs/remotes/origin/main", &forward])
            .run();
        work
    };
    let merge = ["merge", "--ff-only", "origin/main"];
    let work = clone();
    write(work.join("unstaged"), "local unstaged edit\n");
    write(work.join("staged"), "local staged edit\n");
    git(&work).args(["add", "staged"]).run();
    write(work.join("untracked"), "keep me\n");
    fs::create_dir_all(work.join("module")).unwrap();
    write(
        work.join("module/dirty"),
        "submodule working files stay untouched\n",
    );
    let marker = root.join("EXECUTED");
    let probe = root.join("probe");
    write(
        &probe,
        format!("#!/bin/sh\ntouch {}\nexit 91\n", quote(&marker)),
    );
    chmod(&probe, 0o755);
    for name in [
        "post-merge",
        "reference-transaction",
        "pre-push",
        "post-checkout",
    ] {
        fs::copy(&probe, work.join(".git/hooks").join(name)).unwrap();
    }
    let probe = probe.display();
    append(
        work.join(".git/config"),
        &format!(
            "[core]\nsshCommand = {probe}\nfsmonitor = {probe}\n[filter \"probe\"]\nprocess = {probe}\nsmudge = {probe}\nclean = {probe}\n[credential]\nhelper = !{probe}\n"
        ),
    );
    let before = fs::read(work.join("payload")).unwrap();
    fixture
        .safe(&work)
        .arg("fetch")
        .arg(fixture.url(&source))
        .arg("main")
        .run();
    assert_eq!(head(&work), base);
    assert_eq!(fs::read(work.join("payload")).unwrap(), before);
    let trace = root.join("worktree-exec.trace");
    fixture.safe(&work).args(merge).traced(&trace).run();
    check_trace(&trace);
    assert_eq!(head(&work), forward);
    assert_eq!(read(work.join("payload")), "payload forward\n");
    assert_eq!(read(work.join("newdir/file")), "new file\n");
    assert!(!work.join("gone").exists());
    assert_ne!(
        fs::metadata(work.join("executable"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        fs::read_link(work.join("link")).unwrap(),
        Path::new("newdir/file")
    );
    assert_eq!(read(work.join("unstaged")), "local unstaged edit\n");
    assert_eq!(read(work.join("staged")), "local staged edit\n");
    assert_eq!(read(work.join("untracked")), "keep me\n");
    assert_eq!(
        read(work.join("module/dirty")),
        "submodule working files stay untouched\n"
    );
    assert_eq!(
        git(&work).args(["diff", "--cached", "--name-only"]).run(),
        "staged"
    );
    assert_eq!(git(&work).args(["diff", "--name-only"]).run(), "unstaged");
    fixture
        .safe(&work)
        .arg("push")
        .arg(fixture.url(&dest))
        .arg("main")
        .run();
    assert_eq!(head(&dest), forward);
    assert!(!marker.exists());
    passed(
        "working-tree SSH fetch/ff-merge/push updates files and index; unrelated local edits survive; execution settings inert",
    );

    for staged in [false, true] {
        let work = clone();
        write(work.join("payload"), "local conflicting edit\n");
        if staged {
            git(&work).args(["add", "payload"]).run();
        }
        unchanged(&work, || {
            fixture.safe(&work).args(merge).fails();
        });
        assert_eq!(read(work.join("payload")), "local conflicting edit\n");
    }
    let work = clone();
    fs::create_dir(work.join("newdir")).unwrap();
    write(work.join("newdir/file"), "untracked\n");
    unchanged(&work, || {
        fixture.safe(&work).args(merge).fails();
    });
    assert_eq!(read(work.join("newdir/file")), "untracked\n");
    passed("overlapping staged/unstaged changes and untracked files rejected without mutation");

    let work = clone();
    let outside = root.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, work.join("newdir")).unwrap();
    unchanged(&work, || {
        fixture.safe(&work).args(merge).fails();
    });
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    passed("symlink directory cannot redirect a checkout outside the worktree");

    for target in ["/tmp/outside", "../escape", ".git/config"] {
        let blob = git(&seed)
            .args(["hash-object", "-w", "--stdin"])
            .input(target)
            .run();
        let tree = git(&seed)
            .arg("mktree")
            .input(format!("120000 blob {blob}\tlink\n"))
            .run();
        let bad = git(&seed)
            .args(["commit-tree", &tree, "-p", &base])
            .input("unsafe symlink\n")
            .run();
        git(&seed)
            .args(["update-ref", "refs/heads/bad", &bad])
            .run();
        let work = clone();
        git(&work)
            .args(["update-ref", "refs/remotes/origin/main", &bad])
            .run();
        unchanged(&work, || {
            fixture.safe(&work).args(merge).fails();
        });
    }
    let tree = revision(&seed, &format!("{base}^{{tree}}"));
    let malicious = git(&seed)
        .arg("mktree")
        .input(format!("040000 tree {tree}\t.git\n"))
        .run();
    let bad = git(&seed)
        .args(["commit-tree", &malicious, "-p", &base])
        .input("metadata attack\n")
        .run();
    git(&seed)
        .args(["update-ref", "refs/heads/bad", &bad])
        .run();
    let work = clone();
    git(&work)
        .args(["update-ref", "refs/remotes/origin/main", &bad])
        .run();
    unchanged(&work, || {
        fixture.safe(&work).args(merge).fails();
    });
    passed("incoming .git paths and escaping/metadata symlinks rejected before checkout");

    let work = clone();
    git(&work).args(["update-index", "--index-version=4"]).run();
    fixture.safe(&work).args(merge).run();
    assert_eq!(head(&work), forward);
    assert!(git(&work).args(["status", "--porcelain"]).run().is_empty());
    assert_eq!(
        &fs::read(work.join(".git/index")).unwrap()[4..8],
        &2u32.to_be_bytes()
    );
    passed("Git index v4 is read and replaced by a valid v2 index");

    let work = clone();
    let original_index = fs::read(work.join(".git/index")).unwrap();
    let mut process = fixture.safe(&work).args(merge).spawn();
    let txn = work.join(".git/safe-git-checkout");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if txn.join("old-0").exists() {
            process.signal(rustix::process::Signal::STOP);
            break;
        }
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "checkout exited before it could be interrupted"
        );
        assert!(
            Instant::now() < deadline,
            "checkout never created its first backup"
        );
        thread::sleep(Duration::from_micros(500));
    }
    process.signal(rustix::process::Signal::KILL);
    process.child.wait().unwrap();
    assert_eq!(head(&work), base);
    assert!(txn.exists());
    fixture
        .safe(&work)
        .arg("fetch")
        .arg(fixture.url(&source))
        .arg("main")
        .fails();
    // Later edits must survive even if recovery restores other files first.
    let journal: serde_json::Value = serde_json::from_str(&read(txn.join("journal"))).unwrap();
    let bytes: Vec<u8> = serde_json::from_value(journal["changes"][0]["path"].clone()).unwrap();
    let first = work.join(std::ffi::OsString::from_vec(bytes));
    write(&first, "new user edit after crash\n");
    fixture.safe(&work).arg("recover").fails();
    assert_eq!(read(&first), "new user edit after crash\n");
    assert!(txn.join("old-0").exists());
    fs::remove_file(first).unwrap();
    fixture.safe(&work).arg("recover").run();
    assert_eq!(head(&work), base);
    assert_eq!(fs::read(work.join(".git/index")).unwrap(), original_index);
    assert_eq!(read(work.join("bulk-000")), "old\n");
    assert!(!txn.exists() && !work.join(".git/index.lock").exists());
    assert!(git(&work).args(["status", "--porcelain"]).run().is_empty());
    passed("SIGKILL mid-checkout preserves backups; recover restores files, index, and locks");
}
