//! Single-branch clone into private staging, then atomic directory publication.
use crate::{
    checkout,
    object::{ReadObjects, reachable},
    protocol,
    repo::{Repo, Store},
    transport::{Auth, Remote},
};
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, CWD, RenameFlags};
use std::{
    fs::{File, Metadata},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

struct Destination {
    path: PathBuf,
    existing: Option<Metadata>,
    staging: tempfile::TempDir,
}

impl Destination {
    fn new(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .context("clone requires a destination directory name")?;
        // The caller selects the parent directory, including zond's /proc/fd
        // staging parent. Repository content never supplies this path.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = parent
            .canonicalize()
            .context("open clone destination parent")?;
        let path = parent.join(name);
        let existing = match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(
                    meta.is_dir(),
                    "clone destination must be a real directory, not a file or symlink"
                );
                ensure!(
                    std::fs::read_dir(&path)?.next().is_none(),
                    "clone destination is not empty"
                );
                Some(meta)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let staging = tempfile::Builder::new()
            .prefix(".safe-git-clone-")
            .tempdir_in(parent)?;
        Ok(Self {
            path,
            existing,
            staging,
        })
    }
    fn publish(self) -> Result<()> {
        if let Some(original) = self.existing {
            let current = std::fs::symlink_metadata(&self.path)?;
            ensure!(
                current.is_dir()
                    && current.dev() == original.dev()
                    && current.ino() == original.ino(),
                "clone destination changed"
            );
            // rename can replace an empty directory, but never a populated
            // directory, regular file, or symlink with our directory.
            fs::renameat(CWD, self.staging.path(), CWD, &self.path)?;
        } else {
            fs::renameat_with(
                CWD,
                self.staging.path(),
                CWD,
                &self.path,
                RenameFlags::NOREPLACE,
            )
            .context("publish clone without replacing an existing destination")?;
        }
        // The staging path was moved. Disarm its destructor before returning.
        let _ = self.staging.keep();
        File::open(self.path.parent().context("clone parent")?)?.sync_all()?;
        Ok(())
    }
}

fn config_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

pub async fn run(
    destination: &Path,
    display: &Path,
    url: &str,
    auth: &Auth,
    branch: Option<&str>,
    quiet: bool,
) -> Result<()> {
    let remote = Remote::parse(url)?;
    if let Some(branch) = branch {
        crate::repo::branch(branch)?;
    }
    let destination = Destination::new(destination)?;
    if !quiet {
        eprintln!(
            "Cloning into '{}'...",
            display.display().to_string().escape_debug()
        );
    }
    let advertisement = protocol::discover(&remote, auth).await?;
    let (branch, expected) = advertisement.clone_branch(branch)?;
    ensure!(
        branch != "HEAD" && !branch.starts_with("HEAD/"),
        "branch conflicts with origin/HEAD"
    );
    let repo = Repo::init_worktree(destination.staging.path(), &branch)?;
    if let Some(expected) = expected {
        let (tip, objects) = protocol::fetch(&remote, auth, &branch, Some(expected), &[]).await?;
        let mut store = Store::new(&repo);
        store.add(objects)?;
        store.get(tip)?.commit()?;
        for id in reachable(&mut store, tip)? {
            repo.write_object(store.get(id)?.as_ref())?;
        }
        repo.update_ref(&crate::repo::tracking("origin", &branch)?, None, tip)?;
        checkout::merge(&repo, &mut store, &repo.head()?, None, tip)?;
        repo.atomic(
            "refs/remotes/origin/HEAD",
            format!("ref: refs/remotes/origin/{branch}\n").as_bytes(),
        )?;
    } else {
        eprintln!("warning: You appear to have cloned an empty repository.");
    }
    // Store only caller-selected endpoint and validated branch data for Git
    // interoperability. safe-git never reads remote/upstream configuration.
    repo.atomic("config", format!(
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n[remote \"origin\"]\n\turl = {}\n\tfetch = {}\n[branch {}]\n\tremote = origin\n\tmerge = {}\n",
        config_value(url),
        config_value(&format!("+refs/heads/{branch}:refs/remotes/origin/{branch}")),
        config_value(&branch),
        config_value(&format!("refs/heads/{branch}")),
    ).as_bytes())?;
    drop(repo);
    destination.publish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_never_clobbers_files_or_populated_directories() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("clone");
        let stage = Destination::new(&path).unwrap();
        std::fs::write(&path, "preserve").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve");
        assert!(Destination::new(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let stage = Destination::new(&path).unwrap();
        std::fs::write(path.join("keep"), "preserve").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("keep")).unwrap(),
            "preserve"
        );
        assert!(Destination::new(&path).is_err());
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Destination::new(&link).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }
}
