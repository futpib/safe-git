//! Incremental pushes must not read blobs that the receiver already owns.
use crate::support::*;
use std::{fs, path::Path};

fn commit(repo: &Path, large: &str, parent: Option<&str>, message: &str) -> String {
    let small = git(repo)
        .args(["hash-object", "-w", "--stdin"])
        .input(message)
        .run();
    let tree = git(repo)
        .args(["mktree", "--missing"])
        .input(format!(
            "100644 blob {small}\tfile\n100644 blob {large}\tlarge\n"
        ))
        .run();
    let mut command = git(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit-tree",
            &tree,
        ])
        .input(message);
    if let Some(parent) = parent {
        command = command.args(["-p", parent]);
    }
    command.run()
}

pub fn suite() {
    let fixture = Fixture::new();
    let root = fixture.root();
    let sender = root.join("sender.git");
    let dest = root.join("dest.git");
    let empty = root.join("source.git");
    for repo in [&sender, &empty] {
        git(root)
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(repo)
            .run();
    }
    // A sparse file keeps fixture generation cheap, but Git stores a real blob
    // above safe-git's object limit in a valid, highly compressed pack.
    let large_file = root.join("large");
    fs::File::create(&large_file)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    let large = git(&sender)
        .args(["hash-object", "-w"])
        .arg(&large_file)
        .run();
    let base = commit(&sender, &large, None, "base");
    git(&sender)
        .args(["update-ref", "refs/heads/main", &base])
        .run();
    let pack = git(&sender)
        .arg("pack-objects")
        .arg(sender.join("objects/pack/pack"))
        .input(format!("{large}\n"))
        .run();
    fs::remove_file(sender.join(format!("objects/{}/{}", &large[..2], &large[2..]))).unwrap();
    git(root)
        .args(["clone", "-q", "--bare", "--no-hardlinks"])
        .arg(&sender)
        .arg(&dest)
        .run();
    // Advertised refs unknown to the sender must not force a fetch just to
    // discover which additional objects could be omitted.
    let unavailable = commit(&dest, &large, Some(&base), "receiver only");
    git(&dest)
        .args(["update-ref", "refs/heads/unavailable", &unavailable])
        .run();
    let next = commit(&sender, &large, Some(&base), "next");
    git(&sender)
        .args(["update-ref", "refs/heads/main", &next])
        .run();
    let push = |url: &str| fixture.safe(&sender).args(["push", url, "main:main"]);
    assert!(
        push(&fixture.url(&empty))
            .fails()
            .contains("pack object exceeds limit")
    );
    assert!(git(&empty).arg("for-each-ref").run().is_empty());

    // Keeping the oversized blob in the new tree catches implementations that
    // stop at the old commit but still load all blobs in the new snapshot.
    push(&fixture.url(&dest)).run();
    assert_eq!(head(&dest), next);

    // Objects already held by the receiver can also be absent in a partial
    // sender. No promisor or local fallback may be needed to push a small edit.
    for extension in ["pack", "idx"] {
        fs::remove_file(sender.join(format!("objects/pack/pack-{pack}.{extension}"))).unwrap();
    }
    git(&sender).args(["cat-file", "-e", &large]).fails();
    let latest = commit(&sender, &large, Some(&next), "latest");
    for branch in ["main", "topic"] {
        git(&sender)
            .args(["update-ref", &format!("refs/heads/{branch}"), &latest])
            .run();
    }
    fixture
        .safe(&sender)
        .args(["push", "--atomic", &fixture.url(&dest), "main", "topic"])
        .run();
    assert_eq!(head(&dest), latest);
    assert_eq!(revision(&dest, "topic"), latest);
    // A newly created branch can share the receiver's other advertised refs.
    fixture
        .safe(&sender)
        .args(["push", &fixture.url(&dest), "main:other"])
        .run();
    assert_eq!(revision(&dest, "other"), latest);
    git(&sender)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "tag",
            "-a",
            "-m",
            "release",
            "v1",
        ])
        .run();
    fixture
        .safe(&sender)
        .args(["push", &fixture.url(&dest), "v1"])
        .run();
    assert_eq!(revision(&dest, "v1"), revision(&sender, "v1"));
    assert!(
        push(&fixture.url(&empty))
            .fails()
            .contains("missing object")
    );
    assert!(git(&empty).arg("for-each-ref").run().is_empty());

    // Exercise the same preparation over smart HTTP with a divergent update.
    git(&dest)
        .args(["config", "http.receivepack", "true"])
        .run();
    let http = http::Http::new(root);
    let divergent = commit(&sender, &large, Some(&base), "divergent");
    git(&sender)
        .args(["update-ref", "refs/heads/main", &divergent])
        .run();
    fixture
        .safe(&sender)
        .args([
            "push",
            &format!("--force-with-lease=main:{latest}"),
            &format!("{}/dest.git", http.url),
            "main:main",
        ])
        .run();
    assert_eq!(head(&dest), divergent);
    git(&dest).args(["fsck", "--full"]).run();
    git(&sender).args(["cat-file", "-e", &large]).fails();
    git(&sender).args(["cat-file", "-e", &unavailable]).fails();
    passed(
        "incremental SSH/HTTP pushes reuse receiver-owned oversized or missing blobs; new objects still obey limits",
    );
}
