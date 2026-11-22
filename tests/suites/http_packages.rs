use crate::support::*;
use std::{fs, os::unix::fs::symlink};
pub fn suite() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let state = root.join("state");
    let seed = root.join("seed");
    let source = root.join("fixture");
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
        "pkgname=fixture\npkgver=1\npkgrel=1\narch=(any)\n",
    );
    write(
        seed.join(".SRCINFO"),
        "pkgbase = fixture\n\tpkgver = 1\n\tpkgrel = 1\n\tarch = any\npkgname = fixture\n",
    );
    git(&seed).args(["add", "."]).run();
    git(&seed).args(["commit", "-qm", "base"]).run();
    git(&seed).args(["tag", "-am", "release", "v1"]).run();
    git(&seed).args(["branch", "other"]).run();
    git(root)
        .args(["clone", "-q", "--bare"])
        .arg(&seed)
        .arg(&source)
        .run();
    let http = http::Http::new(root);
    let url = format!("{}/fixture", http.url);
    let safe = |repo: &std::path::Path| {
        TestCommand::new(BIN)
            .arg("-C")
            .arg(repo)
            .env("SAFE_GIT_STATE_DIR", &state)
    };
    let work = root.join("work");
    let trace = root.join("http.trace");
    safe(root)
        .args(["clone", "--no-progress", &url])
        .arg(&work)
        .traced(&trace)
        .run();
    check_trace(&trace);
    assert_eq!(revision(&work, "HEAD"), revision(&seed, "HEAD"));
    assert_eq!(revision(&work, "origin/other"), revision(&seed, "other"));
    assert_eq!(revision(&work, "v1"), revision(&seed, "v1"));
    let got = safe(root).args(["ls-remote", "--", &url, "HEAD"]).output();
    let want = git(root).args(["ls-remote", "--", &url, "HEAD"]).output();
    assert_eq!(got, want);
    write(
        seed.join("PKGBUILD"),
        "pkgname=fixture\npkgver=2\npkgrel=1\narch=(any)\n",
    );
    git(&seed).args(["commit", "-qam", "update"]).run();
    git(&seed).args(["push"]).arg(&source).arg("master").run();
    safe(&work).args(["fetch", "-v"]).run();
    safe(&work).args(["rebase", "--stat"]).run();
    assert_eq!(revision(&work, "HEAD"), revision(&seed, "HEAD"));
    assert!(
        safe(root)
            .args(["ls-remote", "--", &format!("{}/redirect", http.url), "HEAD"])
            .fails()
            .contains("redirects")
    );
    passed(
        "smart HTTP discovery, branch/tag transfer, fetch and redirect refusal without child execution",
    );
    let (tls, cert) = http::Http::tls(root);
    let tls_url = format!("{}/fixture", tls.url);
    let trusted = root.join("ca.pem");
    write(&trusted, cert);
    let error = safe(root)
        .args(["ls-remote", "--", &tls_url, "HEAD"])
        .fails();
    assert!(
        error.contains("certificate") || error.contains("UnknownIssuer"),
        "{error}"
    );
    let secure = root.join("secure");
    safe(root)
        .env("SSL_CERT_FILE", &trusted)
        .args(["clone", "--mirror", &tls_url])
        .arg(&secure)
        .run();
    assert_eq!(revision(&secure, "master"), revision(&seed, "HEAD"));
    passed(
        "HTTPS verifies certificates and uses the trusted host CA file for in-process transfers",
    );
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let _daemon = TestCommand::new("git")
        .args(["daemon", "--listen=127.0.0.1", "--export-all"])
        .arg(format!("--port={port}"))
        .arg(format!("--base-path={}", root.display()))
        .arg(root)
        .spawn();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let native = root.join("native");
    safe(root)
        .args([
            "clone",
            "--mirror",
            &format!("git://127.0.0.1:{port}/fixture"),
        ])
        .arg(&native)
        .run();
    safe(&native).args(["fetch", "--all", "-p"]).run();
    assert_eq!(revision(&native, "master"), revision(&seed, "HEAD"));
    passed("native git:// discovery and fetch interoperate with Git daemon");
    if available("paru") {
        let paru_dir = root.join("paru");
        fs::create_dir(&paru_dir).unwrap();
        let cache = root.join("cache");
        fs::create_dir(&cache).unwrap();
        let config = root.join("paru.conf");
        write(&config, "[options]\n[bin]\n");
        TestCommand::new("paru")
            .current_dir(&paru_dir)
            .env("SAFE_GIT_STATE_DIR", &state)
            .env("XDG_CACHE_HOME", &cache)
            .args(["-G", "--aur", "--noconfirm", "--config"])
            .arg(&config)
            .args([
                "--aururl",
                &http.url,
                "--aurrpcurl",
                &format!("{}/rpc", http.url),
                "--git",
                BIN,
                "fixture",
            ])
            .run();
        assert_eq!(
            revision(&paru_dir.join("fixture"), "HEAD"),
            revision(&seed, "HEAD")
        );
        passed("installed paru -G clones through safe-git against loopback AUR service");
    }
    if std::path::Path::new("/usr/share/makepkg/source/git.sh").exists() {
        let bindir = root.join("bin");
        fs::create_dir(&bindir).unwrap();
        symlink(BIN, bindir.join("git")).unwrap();
        let build = root.join("makepkg");
        fs::create_dir(&build).unwrap();
        let tar = git(&seed)
            .args(["archive", "--format", "tar", "v1"])
            .output()
            .stdout;
        let tarfile = root.join("expected.tar");
        write(&tarfile, tar);
        let digest = TestCommand::new("sha256sum").arg(&tarfile).run();
        let digest = digest.split_whitespace().next().unwrap();
        write(
            build.join("PKGBUILD"),
            format!(
                "pkgname=safe-git-fixture\npkgver=1\npkgrel=1\narch=(any)\nsource=('git+{url}#tag=v1')\nsha256sums=('{digest}')\nprepare() {{ cd fixture; git rev-list --count HEAD > ../count; git describe --tags --long > ../description; }}\npkgver() {{ cd fixture; printf 'r%s.%s' \"$(git rev-list --count HEAD)\" \"$(git rev-parse --short=7 HEAD)\"; }}\npackage() {{ :; }}\n"
            ),
        );
        let path = format!("{}:{}", bindir.display(), std::env::var("PATH").unwrap());
        for _ in 0..2 {
            TestCommand::new("makepkg")
                .current_dir(&build)
                .env("PATH", &path)
                .env("SAFE_GIT_STATE_DIR", &state)
                .args(["--nobuild", "--nodeps", "--noconfirm"])
                .run();
            assert_eq!(read(build.join("src/count")).trim(), "1");
            assert_eq!(
                read(build.join("src/description")).trim(),
                "v1-0-g".to_owned() + &revision(&seed, "v1^{}")[..7]
            );
        }
        passed(
            "installed makepkg downloads, verifies Git archive checksum, extracts and runs common pkgver twice using safe-git",
        );
    }
}
