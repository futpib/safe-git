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
    #[serde(default)]
    pub filter: Option<String>,
    pub url: String,
    pub remote: String,
    pub branch: String,
    pub single: bool,
    pub mirror: bool,
    pub bare: bool,
    #[serde(default)]
    pub upstreams: std::collections::BTreeMap<String, String>,
}
pub(crate) fn directory() -> Result<PathBuf> {
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
    Ok(directory)
}
fn path(repo: &Repo) -> Result<PathBuf> {
    let directory = directory()?;
    let root = repo.common.as_ref().unwrap_or(&repo.root);
    let stat = root.metadata()?;
    let location = std::fs::read_link(format!("/proc/self/fd/{}", root.as_raw_fd()))?;
    let id = object::hash(&[
        format!("{}:{}:", stat.dev(), stat.ino()).as_bytes(),
        location.as_os_str().as_bytes(),
    ])?;
    Ok(directory.join(format!("{id}.json")))
}
#[derive(Default, Serialize, Deserialize)]
pub struct Registry {
    pub default: String,
    pub remotes: std::collections::BTreeMap<String, Binding>,
}
pub fn registry(repo: &Repo) -> Result<Registry> {
    let path = path(repo)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.permissions().mode() & 0o022 == 0,
        "unsafe host endpoint record"
    );
    let mut data = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() <= 1024 * 1024,
        "host endpoint record exceeds limit"
    );
    let registry: Registry = if let Ok(registry) = serde_json::from_slice(&data) {
        registry
    } else {
        let binding: Binding = serde_json::from_slice(&data)?;
        Registry {
            default: binding.remote.clone(),
            remotes: [(binding.remote.clone(), binding)].into(),
        }
    };
    ensure!(registry.remotes.len() <= 256, "too many registered remotes");
    for (name, binding) in &registry.remotes {
        ensure!(name == &binding.remote, "invalid remote record");
        crate::repo::tracking(&binding.remote, &binding.branch)?;
        for (local, remote) in &binding.upstreams {
            crate::repo::branch(local)?;
            crate::repo::branch(remote)?;
        }
    }
    Ok(registry)
}
pub fn get(repo: &Repo) -> Result<Option<Binding>> {
    let registry = registry(repo)?;
    let head = repo.head()?;
    if let Some(branch) = head.strip_prefix("refs/heads/")
        && let Some(binding) = registry
            .remotes
            .values()
            .find(|b| b.upstreams.contains_key(branch))
    {
        return Ok(Some(binding.clone()));
    }
    Ok(registry.remotes.get(&registry.default).cloned())
}
pub fn named(repo: &Repo, name: &str) -> Result<Option<Binding>> {
    Ok(registry(repo)?.remotes.remove(name))
}
pub fn selected(repo: &Repo, value: Option<&str>) -> Result<Option<Binding>> {
    match value {
        Some(v) => named(repo, v),
        None => get(repo),
    }
}
pub fn save_registry(repo: &Repo, registry: &Registry) -> Result<()> {
    let path = path(repo)?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    let bytes = serde_json::to_vec(registry)?;
    ensure!(
        bytes.len() <= 1024 * 1024 && registry.remotes.len() <= 256,
        "host endpoint record exceeds limit"
    );
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(&path)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub fn save(repo: &Repo, binding: &Binding) -> Result<()> {
    let mut registry = registry(repo)?;
    if registry.default.is_empty() {
        registry.default = binding.remote.clone();
    }
    for other in registry.remotes.values_mut() {
        other
            .upstreams
            .retain(|local, _| !binding.upstreams.contains_key(local));
    }
    registry
        .remotes
        .insert(binding.remote.clone(), binding.clone());
    save_registry(repo, &registry)
}
/// Resolve only caller input and the external trust record. Repository config,
/// pushurl, URL rewrites and branch settings never select a network destination.
pub fn endpoint(repo: &Repo, positional: Option<&str>, explicit: Option<&str>) -> Result<String> {
    match positional {
        Some(value) => {
            match named(repo, value)? {
                Some(binding) if value == binding.remote => {
                    return Ok(explicit.unwrap_or(&binding.url).to_owned());
                }
                None if !value.contains([':', '/']) && explicit.is_some() => {
                    crate::repo::tracking(value, "main")?;
                    return Ok(explicit.unwrap().to_owned());
                }
                _ => {}
            }
            Ok(value.to_owned())
        }
        None => match explicit {
            Some(value) => Ok(value.to_owned()),
            None => Ok(require(repo)?.url),
        },
    }
}
pub fn require(repo: &Repo) -> Result<Binding> {
    get(repo)?.context("provide a URL, --url, or SAFE_GIT_URL; repository remote configuration is not consulted (clone with safe-git or explicitly register the endpoint with safe-git config remote.origin.url URL)")
}
