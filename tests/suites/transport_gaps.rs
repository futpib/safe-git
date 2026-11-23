use crate::support::*;
use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::Path,
    time::{Duration, Instant},
};

fn safe(root: &Path, repo: &Path) -> TestCommand {
    TestCommand::new(BIN)
        .arg("-C")
        .arg(repo)
        .env("SAFE_GIT_STATE_DIR", root.join("state"))
        .args([
            "-c",
            "user.name=Transport",
            "-c",
            "user.email=transport@example.invalid",
            "--timeout",
            "20",
        ])
}
fn seed(root: &Path) -> std::path::PathBuf {
    let repo = root.join("seed");
    git(root)
        .args(["init", "-q", "-b", "main"])
        .arg(&repo)
        .run();
    write(repo.join("file"), "first\n");
    git(&repo).args(["add", "."]).run();
    git(&repo)
        .args([
            "-c",
            "user.name=Transport",
            "-c",
            "user.email=transport@example.invalid",
            "commit",
            "-qm",
            "base",
        ])
        .run();
    repo
}
pub fn http() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let seed = seed(root);
    let remote = root.join("remote.git");
    git(root)
        .args(["clone", "-q", "--bare"])
        .arg(&seed)
        .arg(&remote)
        .run();
    git(&remote)
        .args(["config", "http.receivepack", "true"])
        .run();
    write(root.join(".http-authorization"), "Basic dXNlcjpwYXNz");
    let (tls, cert) = http::Http::tls(root);
    write(root.join("ca.pem"), cert);
    let url = format!("{}/remote.git", tls.url);
    let command = |repo: &Path| {
        safe(root, repo)
            .env("SSL_CERT_FILE", root.join("ca.pem"))
            .env("SAFE_GIT_HTTP_USERNAME", "user")
            .env("SAFE_GIT_HTTP_PASSWORD", "pass")
    };
    let work = root.join("work");
    command(root).arg("clone").arg(&url).arg(&work).run();
    write(work.join("file"), "https basic\n");
    command(&work).args(["commit", "-am", "https basic"]).run();
    command(&work).args(["push", "-u", "origin", "main"]).run();
    assert_eq!(revision(&work, "HEAD"), revision(&remote, "main"));
    command(&work).args(["pull", "--ff-only"]).run();
    command(&work).args(["branch", "tls-topic"]).run();
    command(&work)
        .args(["push", "--atomic", "--all", "origin"])
        .run();
    assert_eq!(revision(&work, "tls-topic"), revision(&remote, "tls-topic"));
    assert!(
        safe(root, root)
            .env("SSL_CERT_FILE", root.join("ca.pem"))
            .args(["ls-remote", &url])
            .fails()
            .contains("401")
    );
    let http = http::Http::new(root);
    assert!(
        command(root)
            .args(["ls-remote", &format!("{}/remote.git", http.url)])
            .fails()
            .contains("HTTPS")
    );
    assert!(
        command(root)
            .env("SAFE_GIT_HTTP_TOKEN", "conflicting")
            .args(["ls-remote", &url])
            .fails()
            .contains("either")
    );
    // Credentials are never available through repository-controlled Git config.
    git(&work)
        .args([
            "config",
            "http.extraHeader",
            "Authorization: Basic attacker",
        ])
        .run();
    command(&work).arg("fetch").run();
    passed(
        "HTTPS Basic auth clones, fetches and pushes; cleartext, ambiguous credentials and repository-selected headers are refused",
    );

    fs::remove_file(root.join(".http-authorization")).unwrap();
    write(root.join(".static-http"), "");
    git(&remote).arg("update-server-info").run();
    for (option, error) in [("--depth=1", "shallow"), ("--filter=blob:none", "filters")] {
        let destination = root.join(format!("unsupported-{error}"));
        assert!(
            safe(root, root)
                .args(["clone", option])
                .arg(format!("{}/remote.git", http.url))
                .arg(&destination)
                .fails()
                .contains(error)
        );
        assert!(!destination.exists());
    }
    write(
        remote.join("objects/info/http-alternates"),
        "http://127.0.0.1:1/secret/objects\n",
    );
    let clone = root.join("static-loose");
    safe(root, root)
        .arg("clone")
        .arg(format!("{}/remote.git", http.url))
        .arg(&clone)
        .run();
    assert_eq!(revision(&clone, "HEAD"), revision(&remote, "main"));
    git(&clone).args(["fsck", "--full"]).run();
    git(&remote).args(["repack", "-ad"]).run();
    git(&remote).arg("prune-packed").run();
    git(&remote).arg("update-server-info").run();
    let packed = root.join("static-packed");
    let trace = root.join("static.trace");
    safe(root, root)
        .env("SSL_CERT_FILE", root.join("ca.pem"))
        .arg("clone")
        .arg(&url)
        .arg(&packed)
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert_eq!(revision(&packed, "HEAD"), revision(&remote, "main"));
    git(&packed).args(["fsck", "--full"]).run();
    assert!(!read(root.join(".http-requests")).contains("alternates"));
    // A remote listing cannot choose traversal paths or another origin.
    write(remote.join("objects/info/packs"), "P ../../escape.pack\n");
    assert!(
        safe(root, root)
            .arg("clone")
            .arg(format!("{}/remote.git", http.url))
            .arg(root.join("bad"))
            .fails()
            .contains("pack")
    );
    assert!(!root.join("bad").exists());
    passed(
        "static HTTP(S) loose and packed objects match Git; alternates and malformed pack paths cannot redirect fetching",
    );
}
pub fn local_native() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let seed = seed(root);
    let remote = root.join("dest.git");
    safe(root, root).args(["init", "--bare"]).arg(&remote).run();
    fs::create_dir_all(remote.join("hooks")).unwrap();
    let marker = root.join("EXECUTED");
    write(
        remote.join("hooks/pre-receive"),
        format!("#!/bin/sh\ntouch {}\nexit 1\n", quote(&marker)),
    );
    chmod(remote.join("hooks/pre-receive"), 0o755);
    git(&remote)
        .args(["config", "core.hooksPath"])
        .arg(remote.join("hooks"))
        .run();
    let trace = root.join("local.trace");
    safe(root, &seed)
        .arg("push")
        .arg(&remote)
        .arg("main")
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert!(!marker.exists());
    assert_eq!(revision(&remote, "main"), revision(&seed, "HEAD"));
    git(&remote).args(["fsck", "--full"]).run();
    safe(root, &seed).args(["tag", "v1"]).run();
    let file_url = url::Url::from_file_path(&remote).unwrap().to_string();
    safe(root, &seed).args(["push", &file_url, "v1"]).run();
    assert_eq!(revision(&remote, "v1"), revision(&seed, "v1"));
    safe(root, &seed)
        .args(["push", "--delete", &file_url, "v1"])
        .run();
    assert!(
        git(&remote)
            .args(["show-ref", "--verify", "refs/tags/v1"])
            .output()
            .code
            != Some(0)
    );
    safe(root, &seed).args(["branch", "local-topic"]).run();
    assert!(
        safe(root, &seed)
            .args(["push", "--atomic", "--all", &file_url])
            .fails()
            .contains("atomic")
    );
    assert!(
        git(&remote)
            .args(["show-ref", "--verify", "refs/heads/local-topic"])
            .output()
            .code
            != Some(0)
    );
    safe(root, &seed).args(["push", "--all", &file_url]).run();
    assert_eq!(
        revision(&remote, "local-topic"),
        revision(&seed, "local-topic")
    );
    let checked = root.join("checked");
    git(root)
        .args(["clone", "-q"])
        .arg(&seed)
        .arg(&checked)
        .run();
    assert!(
        safe(root, &seed)
            .arg("push")
            .arg(&checked)
            .arg("main")
            .fails()
            .contains("checked-out")
    );
    safe(root, &seed)
        .arg("push")
        .arg(&checked)
        .arg("main:other")
        .run();
    assert_eq!(read(checked.join("file")), "first\n");
    write(
        checked.join(".git/refs/heads/alias"),
        "ref: refs/heads/main\n",
    );
    write(checked.join(".git/HEAD"), "ref: refs/heads/alias\n");
    assert!(
        safe(root, &seed)
            .arg("push")
            .arg(&checked)
            .arg("main")
            .fails()
            .contains("checked-out")
    );
    let linked = root.join("linked");
    git(&remote)
        .args(["worktree", "add", "-q", "-b", "occupied"])
        .arg(&linked)
        .arg("main")
        .run();
    let before = revision(&remote, "occupied");
    assert!(
        safe(root, &seed)
            .arg("push")
            .arg(&remote)
            .arg("main:occupied")
            .fails()
            .contains("linked worktree")
    );
    assert_eq!(revision(&remote, "occupied"), before);
    write(
        remote.join("worktrees/linked/gitdir"),
        "/forbidden/outside\n",
    );
    safe(root, &seed)
        .arg("push")
        .arg(&remote)
        .arg("main:spare")
        .run();
    assert_eq!(read(linked.join("file")), "first\n");
    passed(
        "local and file URL push verifies objects, preserves checked-out branches and never runs destination hooks",
    );

    // Git daemon enables receive-pack explicitly; the client never supplies a command.
    fs::remove_file(remote.join("hooks/pre-receive")).unwrap();
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
    let url = format!("git://127.0.0.1:{port}/dest.git");
    write(seed.join("file"), "native push\n");
    safe(root, &seed).args(["commit", "-am", "native"]).run();
    safe(root, &seed).args(["push", "-u", &url, "main"]).run();
    assert_eq!(revision(&remote, "main"), revision(&seed, "HEAD"));
    safe(root, &seed).args(["push", &url, "main:other"]).run();
    safe(root, &seed)
        .args(["push", "--delete", &url, "other"])
        .run();
    safe(root, &seed).args(["pull", "--ff-only"]).run();
    let before = revision(&remote, "main");
    write(seed.join("file"), "native atomic push\n");
    safe(root, &seed).args(["commit", "-am", "atomic"]).run();
    safe(root, &seed).args(["branch", "native-topic"]).run();
    write(
        remote.join("hooks/update"),
        "#!/bin/sh\n[ \"$1\" != refs/heads/native-topic ]\n",
    );
    chmod(remote.join("hooks/update"), 0o755);
    safe(root, &seed)
        .args(["push", "--atomic", "--all", &url])
        .fails();
    assert_eq!(revision(&remote, "main"), before);
    assert!(
        git(&remote)
            .args(["show-ref", "--verify", "refs/heads/native-topic"])
            .output()
            .code
            != Some(0)
    );
    fs::remove_file(remote.join("hooks/update")).unwrap();
    git(&remote)
        .args(["config", "receive.advertiseAtomic", "false"])
        .run();
    assert!(
        safe(root, &seed)
            .args(["push", "--atomic", "--all", &url])
            .fails()
            .contains("atomic")
    );
    assert_eq!(revision(&remote, "main"), before);
    git(&remote)
        .args(["config", "receive.advertiseAtomic", "true"])
        .run();
    safe(root, &seed)
        .args(["push", "--atomic", "--all", &url])
        .run();
    assert_eq!(revision(&remote, "main"), revision(&seed, "main"));
    assert_eq!(
        revision(&remote, "native-topic"),
        revision(&seed, "native-topic")
    );
    git(&remote).args(["fsck", "--full"]).run();
    passed("native git:// push interoperates with Git daemon for updates, upstreams and deletion");
}
pub fn ssh() {
    {
        let expired = Fixture::expired_certificate();
        assert!(
            expired
                .safe(expired.root())
                .args([
                    "ls-remote",
                    &expired.url(&expired.root().join("source.git"))
                ])
                .fails()
                .contains("certificate")
        );
    }
    {
        use std::sync::atomic::Ordering;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let server = crate::support::password::Password::new(root);
        let trace = root.join("password.trace");
        safe(root, root)
            .arg("--known-hosts")
            .arg(&server.known)
            .env("SAFE_GIT_SSH_PASSWORD", "caller-only-password")
            .args(["ls-remote", &server.url])
            .traced(&trace)
            .run();
        check_trace(&trace);
        assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
        let error = safe(root, root)
            .arg("--known-hosts")
            .arg(&server.known)
            .env("SAFE_GIT_SSH_PASSWORD", "wrong-private-value")
            .args(["ls-remote", &server.url])
            .fails();
        assert!(!error.contains("wrong-private-value"));
        let count = server.attempts.load(Ordering::SeqCst);
        write(&server.known, "");
        safe(root, root)
            .arg("--known-hosts")
            .arg(&server.known)
            .env("SAFE_GIT_SSH_PASSWORD", "caller-only-password")
            .args(["ls-remote", &server.url])
            .fails();
        assert_eq!(
            server.attempts.load(Ordering::SeqCst),
            count,
            "untrusted host received a password"
        );
        passed(
            "caller-selected SSH password authentication requires a verified host and does not log credentials",
        );
    }
    let target = Fixture::certificate();
    let root = target.root();
    let seed = seed(root);
    let remote = root.join("source.git");
    git(root)
        .args(["clone", "-q", "--bare"])
        .arg(&seed)
        .arg(&remote)
        .run();
    target
        .safe(root)
        .args(["ls-remote", &target.url(&remote)])
        .run();
    let config = root.join("ssh_config");
    let user = TestCommand::new("id").arg("-un").run();
    write(
        &config,
        format!(
            "Host target\n HostName 127.0.0.1\n User {user}\n Port {}\n IdentityFile {}\n UserKnownHostsFile {}\n",
            target.port,
            root.join("client_key").display(),
            root.join("known_hosts").display()
        ),
    );
    let alias = format!("target:{}", remote.display());
    let trace = root.join("ssh.trace");
    safe(root, root)
        .env("SAFE_GIT_SSH_CONFIG", &config)
        .args(["ls-remote", &alias])
        .traced(&trace)
        .run();
    check_trace(&trace);
    // CA trust cannot override principal checks or revocation.
    let wrong = root.join("wrong_known");
    write(
        &wrong,
        format!(
            "@cert-authority [localhost]:{} {}",
            target.port,
            read(root.join("ca.pub"))
        ),
    );
    assert!(
        safe(root, root)
            .arg("--identity-file")
            .arg(root.join("client_key"))
            .arg("--known-hosts")
            .arg(&wrong)
            .args([
                "ls-remote",
                &target.url(&remote).replace("127.0.0.1", "localhost")
            ])
            .fails()
            .contains("hostname mismatch")
    );
    append(
        root.join("known_hosts"),
        &format!(
            "@revoked [127.0.0.1]:{} {}",
            target.port,
            read(root.join("ca.pub"))
        ),
    );
    assert!(
        target
            .safe(root)
            .args(["ls-remote", &target.url(&remote)])
            .fails()
            .contains("revoked")
    );
    let known = read(root.join("known_hosts"))
        .lines()
        .filter(|l| !l.starts_with("@revoked"))
        .map(|l| format!("{l}\n"))
        .collect::<String>();
    write(root.join("known_hosts"), known);
    let jump = Fixture::forwarding();
    let jump_root = jump.root();
    append(
        root.join("known_hosts"),
        &read(jump_root.join("known_hosts")),
    );
    append(
        &config,
        &format!(
            "Host jump\n HostName 127.0.0.1\n User {user}\n Port {}\n IdentityFile {}\n UserKnownHostsFile {}\nHost target\n ProxyJump jump\n",
            jump.port,
            jump_root.join("client_key").display(),
            root.join("known_hosts").display()
        ),
    );
    safe(root, root)
        .env("SAFE_GIT_SSH_CONFIG", &config)
        .args(["ls-remote", &alias])
        .run();
    append(&config, "Host jump\n ProxyJump target\n");
    assert!(
        safe(root, root)
            .env("SAFE_GIT_SSH_CONFIG", &config)
            .args(["ls-remote", &alias])
            .fails()
            .contains("cyclic")
    );
    passed(
        "SSH host aliases, per-host keys, verified CA certificates and in-process ProxyJump; principal mismatches, revocation and cycles rejected",
    );
}
