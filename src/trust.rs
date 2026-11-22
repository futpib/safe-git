//! Caller-selected endpoints live in host state, never in repository-controlled config.
use crate::{object, repo::Repo};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::ffi::OsStrExt,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Binding {
    pub url: String,
    pub remote: String,
    pub branch: String,
    pub single: bool,
    pub mirror: bool,
    pub bare: bool,
}
fn path(repo: &Repo) -> Result<PathBuf> {
    let directory = std::env::var_os("SAFE_GIT_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::home_dir().map(|h| h.join(".local/state")))
                .map(|p| p.join("safe-git"))
        })
        .context("no host state directory; set SAFE_GIT_STATE_DIR")?;
    std::fs::create_dir_all(&directory)?;
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o022 == 0,
        "safe-git host state must be a private real directory"
    );
    let stat = repo.root.metadata()?;
    let location = std::fs::read_link(format!("/proc/self/fd/{}", repo.root.as_raw_fd()))?;
    let id = object::hash(&[
        format!("{}:{}:", stat.dev(), stat.ino()).as_bytes(),
        location.as_os_str().as_bytes(),
    ])?;
    Ok(directory.join(format!("{id}.json")))
}
pub fn get(repo: &Repo) -> Result<Option<Binding>> {
    let path = path(repo)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.permissions().mode() & 0o022 == 0,
        "unsafe host endpoint record"
    );
    let mut data = Vec::new();
    file.take(16385).read_to_end(&mut data)?;
    ensure!(data.len() <= 16384, "host endpoint record exceeds limit");
    let binding: Binding = serde_json::from_slice(&data)?;
    crate::repo::tracking(&binding.remote, &binding.branch)?;
    Ok(Some(binding))
}
pub fn save(repo: &Repo, binding: &Binding) -> Result<()> {
    let path = path(repo)?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    file.write_all(&serde_json::to_vec(binding)?)?;
    file.as_file().sync_all()?;
    file.persist(&path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub fn require(repo: &Repo) -> Result<Binding> {
    get(repo)?.context("provide a URL, --url, or SAFE_GIT_URL; repository remote configuration is not consulted (clone with safe-git or explicitly register the endpoint with safe-git config remote.origin.url URL)")
}
