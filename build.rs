use std::{env, path::Path, process::Command};

// Runs only while compiling this source checkout, never in the installed binary.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn revision(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    // Do not accidentally report a parent repository's commit for a source archive.
    if !dot_git.exists() {
        return None;
    }
    if dot_git.is_file() {
        println!("cargo:rerun-if-changed={}", dot_git.display());
    }
    // Git resolves these paths for ordinary checkouts and linked worktrees.
    for name in ["HEAD", "refs", "packed-refs"] {
        let path = root.join(git(root, &["rev-parse", "--git-path", name])?);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let commit = git(root, &["rev-parse", "--verify", "HEAD"])?;
    ((commit.len() == 40 || commit.len() == 64)
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(commit)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let root = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    let commit = revision(Path::new(&root)).unwrap_or_else(|| "unknown".to_owned());
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    println!("cargo:rustc-env=SAFE_GIT_VERSION={version} ({commit})");
}
